use super::keep::KeepMode;
use super::loop_mode::LoopMode;
use super::mode::{Mode, ModeContext, ModeKind, ModeResponse};
use super::unni::UnniMode;
use crate::agent::agent_pool::AgentPool;
use crate::agent::context_assembler::ContextAssembler;
use crate::agent::thinking::{InstanceOutcome, ThinkingFactory, ThinkingInstance};
use crate::agent::thought::ThinkingOutput as PersistentThinkingOutput;
use crate::common::AgentError;
use crate::data::duckdb::Registry;
use crate::data::thought_store::ThoughtStore;
use crate::data::ModelRow;
use crate::logic::model::registry::ProviderRegistry;
use crate::logic::model::stream::StreamChunk;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub struct ModeManager {
    current: ModeKind,
    unni: UnniMode,
    keep: KeepMode,
    loop_mode: LoopMode,
    ctx: ModeContext,

    thinking_factory: ThinkingFactory,

    agent_pool: Arc<AgentPool>,

    pending_instances: Vec<ThinkingInstance>,

    provider_registry: ProviderRegistry,
    default_model: ModelRow,

    thought_store: Arc<ThoughtStore>,

    stream_tx: mpsc::Sender<(String, StreamChunk)>,

    pool_tx: mpsc::Sender<InstanceOutcome>,

    active: HashMap<String, (Arc<Notify>, JoinHandle<()>)>,

    context_assembler: ContextAssembler,

    capability_registry: Registry,

    reload_rx: Option<mpsc::Receiver<crate::logic::capability::executor::ReloadEvent>>,

    duckdb: Option<std::sync::Arc<std::sync::Mutex<duckdb::Connection>>>,
}

impl ModeManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_deps(
        ctx: ModeContext,
        provider_registry: ProviderRegistry,
        default_model: ModelRow,
        thought_store: Arc<ThoughtStore>,
        assembler: ContextAssembler,
        registry: Registry,
        pool: std::sync::Arc<AgentPool>,
        duckdb: Option<std::sync::Arc<std::sync::Mutex<duckdb::Connection>>>,
    ) -> Self {
        let (stream_tx, _) = mpsc::channel(128);
        let (pool_tx, _) = mpsc::channel(128);

        let (_reload_tx, reload_rx) =
            mpsc::channel::<crate::logic::capability::executor::ReloadEvent>(32);

        let thinking_factory = ThinkingFactory::new();

        Self {
            current: ModeKind::Unni,
            unni: UnniMode::new(),
            keep: KeepMode::new(),
            loop_mode: LoopMode::new(),
            ctx,
            thinking_factory,
            agent_pool: pool,
            pending_instances: Vec::new(),
            provider_registry,
            default_model,
            thought_store,
            stream_tx,
            pool_tx,
            active: HashMap::new(),
            context_assembler: assembler,
            capability_registry: registry,
            reload_rx: Some(reload_rx),
            duckdb,
        }
    }

    pub fn current_kind(&self) -> ModeKind {
        self.current
    }

    pub fn current_name(&self) -> &'static str {
        self.current.name()
    }

    pub fn current_mode(&self) -> &dyn Mode {
        match self.current {
            ModeKind::Unni => &self.unni,
            ModeKind::Keep => &self.keep,
            ModeKind::Loop => &self.loop_mode,
        }
    }

    pub fn current_mode_mut(&mut self) -> &mut dyn Mode {
        match self.current {
            ModeKind::Unni => &mut self.unni,
            ModeKind::Keep => &mut self.keep,
            ModeKind::Loop => &mut self.loop_mode,
        }
    }

    pub fn ctx(&self) -> &ModeContext {
        &self.ctx
    }

    pub fn pending_instances(&self) -> &[ThinkingInstance] {
        &self.pending_instances
    }

    pub fn thought_store(&self) -> &ThoughtStore {
        &self.thought_store
    }

    pub async fn cycle_mode(&mut self) -> Result<(), AgentError> {
        let next = self.current.next();
        self.switch_mode(next).await
    }

    pub async fn cycle_mode_back(&mut self) -> Result<(), AgentError> {
        let prev = self.current.prev();
        self.switch_mode(prev).await
    }

    pub async fn handle_input(&mut self, input: &str) -> Result<ModeResponse, AgentError> {
        if let Some(ref mut rx) = self.reload_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    crate::logic::capability::executor::ReloadEvent::CapabilityTable(_)
                    | crate::logic::capability::executor::ReloadEvent::Agent => {
                        if let Some(db) = &self.duckdb {
                            let conn = db.lock().unwrap();
                            match crate::data::duckdb::load_all_into_memory(&conn) {
                                Ok(new_registry) => {
                                    tracing::info!(
                                        "registry reloaded: {} base capabilities, {} composite capabilities",
                                        new_registry.base_capabilities.len(),
                                        new_registry.composite_capabilities.len()
                                    );
                                    self.capability_registry = new_registry;
                                }
                                Err(e) => {
                                    tracing::warn!("registry reload: load failed: {e}");
                                }
                            }
                        }
                    }
                }
            }
        }

        self.ctx.mode_name = self.current;
        self.ctx.request_permission = match self.current {
            ModeKind::Keep => true,
            ModeKind::Unni | ModeKind::Loop => false,
        };

        let mode_hint = self.current.name().to_lowercase();
        let output = self.agent_run(&mode_hint, input).await?;

        let awaiting = self.current_mode().gate_awaiting(&output);

        let mut resp = ModeResponse::text(output.say.unwrap_or_default());
        resp.think = output.think.clone();
        if awaiting {
            resp = resp.with_awaiting_confirmation();
        }
        Ok(resp)
    }

    async fn agent_run(
        &mut self,
        mode_hint: &str,
        input: &str,
    ) -> Result<crate::agent::output::AgentOutput, AgentError> {
        self.thinking_factory
            .run_with_dm_in_period(
                mode_hint,
                input,
                &self.default_model,
                &self.provider_registry,
                &self.context_assembler,
                &self.agent_pool,
                &self.thought_store,
            )
            .await
    }

    pub async fn switch_mode(&mut self, kind: ModeKind) -> Result<(), AgentError> {
        if self.current == kind {
            return Ok(());
        }
        let Self {
            current,
            unni,
            keep,
            loop_mode,
            ctx,
            ..
        } = self;

        let exit_mode: &mut dyn Mode = match current {
            ModeKind::Unni => unni,
            ModeKind::Keep => keep,
            ModeKind::Loop => loop_mode,
        };
        exit_mode.exit(ctx).await?;

        *current = kind;

        ctx.mode_name = kind;
        ctx.mode_hint = kind.name().to_lowercase();
        let enter_mode: &mut dyn Mode = match current {
            ModeKind::Unni => unni,
            ModeKind::Keep => keep,
            ModeKind::Loop => loop_mode,
        };
        enter_mode.enter(ctx).await?;

        Ok(())
    }

    pub fn take_channels(
        &mut self,
    ) -> (
        mpsc::Receiver<(String, StreamChunk)>,
        mpsc::Receiver<InstanceOutcome>,
    ) {
        let (stream_tx, stream_rx) = mpsc::channel(128);
        let (pool_tx, pool_rx) = mpsc::channel(128);
        self.stream_tx = stream_tx;
        self.pool_tx = pool_tx;
        (stream_rx, pool_rx)
    }

    pub async fn spawn(&mut self, input: String) -> Result<String, AgentError> {
        self.spawn_with_override(input, None).await
    }

    pub async fn spawn_with_override(
        &mut self,
        input: String,
        override_input: Option<crate::agent::thought::ThinkingInput>,
    ) -> Result<String, AgentError> {
        self.ctx.mode_name = self.current;
        self.ctx.request_permission = match self.current {
            ModeKind::Keep => true,
            ModeKind::Unni | ModeKind::Loop => false,
        };
        let mode_name = self.current.name();
        let mode_hint = self.current.name().to_lowercase();
        let mut instance =
            self.thinking_factory
                .create_from_mode(mode_name, mode_hint, input.clone())?;
        if let Some(ov) = override_input {
            instance = instance.with_input_override(ov);
        }
        let id = instance.id.clone();
        let thought_context = instance.thought_context();
        self.thought_store.persist_input(&thought_context)?;

        let provider_kind = self.default_model.api_type.to_lowercase();
        let provider = match self.provider_registry.pick_by_kind(&provider_kind).cloned() {
            Some(provider) => provider,
            None => {
                let error = AgentError::Llm(format!(
                    "spawn: 无 provider for kind '{}' (api_type={})",
                    provider_kind, self.default_model.api_type
                ));
                let mut failed_context = thought_context;
                failed_context.set_output(PersistentThinkingOutput::failed(error.to_string()));
                self.thought_store.persist_output(&failed_context)?;
                return Err(error);
            }
        };

        let cancel = Arc::new(Notify::new());

        let handle = instance
            .spawn_streaming(
                self.stream_tx.clone(),
                self.pool_tx.clone(),
                cancel.clone(),
                self.default_model.clone(),
                provider,
                &self.context_assembler,
                Arc::clone(&self.agent_pool),
                Arc::clone(&self.thought_store),
                thought_context,
            )
            .await;

        self.active.insert(id.clone(), (cancel, handle));
        self.pending_instances.push(instance);

        Ok(id)
    }

    pub fn cancel_all_active(&mut self) {
        for (_, (cancel, _handle)) in self.active.iter() {
            cancel.notify_one();
        }
    }

    pub fn cancel_latest_active(&mut self) {
        for inst in self.pending_instances.iter().rev() {
            if let Some((cancel, _handle)) = self.active.get(&inst.id) {
                cancel.notify_one();
                return;
            }
        }
    }

    pub fn loop_note_noop(&mut self) {
        if self.current == ModeKind::Loop {
            self.loop_mode.note_noop();
        }
    }

    pub fn loop_reset_idle(&mut self) {
        if self.current == ModeKind::Loop {
            self.loop_mode.reset_idle();
        }
    }

    pub fn loop_should_stop_idle(&self) -> bool {
        self.current == ModeKind::Loop && self.loop_mode.should_stop_idle()
    }

    pub fn active_is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn bookkeep(&mut self, outcome: InstanceOutcome, _user_message: &str) {
        if let Err(error) = &outcome.result {
            tracing::warn!(
                "bookkeep: instance outcome error (thought_id={}): {error}",
                outcome.id
            );
        }

        self.active.remove(&outcome.id);

        self.pending_instances.retain(|inst| inst.id != outcome.id);

        let outcome_id = outcome.id.clone();
        let pool = std::sync::Arc::clone(&self.agent_pool);
        tokio::spawn(async move {
            pool.remove_core_agent(&outcome_id).await;
            pool.snapshot_detailed().await;
        });
    }

    pub fn remove_active(&mut self, id: &str) {
        self.active.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::thought::{ThinkingInput, ThinkingTerminalState};
    use crate::logic::model::provider::{LlmProvider, LlmRequest, LlmResponse};
    use crate::logic::model::registry::ProviderRegistry;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[allow(dead_code)]
    const INVALID_RAW_OUTPUT: &str =
        "NOVA_RAW_OUTPUT_SENTINEL_7f0d4c8e9a2b6f31_do_not_copy_to_metadata";

    #[allow(dead_code)]
    struct SuccessfulProvider;

    #[async_trait::async_trait]
    impl LlmProvider for SuccessfulProvider {
        fn id(&self) -> &'static str {
            "openai"
        }

        fn name(&self) -> &'static str {
            "successful test provider"
        }

        async fn call(
            &self,
            _request: &LlmRequest,
        ) -> std::result::Result<LlmResponse, AgentError> {
            Ok(LlmResponse {
                content: r#"{"think":"durable work","say":"durable reply"}"#.to_string(),
                usage: None,
            })
        }

        async fn call_stream(
            &self,
            _request: &LlmRequest,
            on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> std::result::Result<LlmResponse, AgentError> {
            on_chunk(StreamChunk::Delta(
                r#"{"think":"durable stream work","#.to_string(),
            ));
            on_chunk(StreamChunk::Delta(
                r#""say":"durable stream reply"}"#.to_string(),
            ));
            on_chunk(StreamChunk::Done);
            Ok(LlmResponse {
                content: r#"{"think":"durable stream work","say":"durable stream reply"}"#
                    .to_string(),
                usage: None,
            })
        }
    }

    struct DualProvider {
        calls: AtomicUsize,
    }

    impl DualProvider {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for DualProvider {
        fn id(&self) -> &'static str {
            "openai"
        }

        fn name(&self) -> &'static str {
            "dual test provider"
        }

        async fn call(
            &self,
            _request: &LlmRequest,
        ) -> std::result::Result<LlmResponse, AgentError> {
            // 引擎提示词当前留空，不再用“你只负责内部推理”区分 think/say；按调用顺序区分。
            let content = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "<think>internal plan</think>\nthink body".to_string()
            } else {
                "<think>say reasoning</think>\nsay body".to_string()
            };
            Ok(LlmResponse {
                content,
                usage: None,
            })
        }

        async fn call_stream(
            &self,
            request: &LlmRequest,
            _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> std::result::Result<LlmResponse, AgentError> {
            self.call(request).await
        }
    }

    struct PendingStreamProvider;

    #[async_trait::async_trait]
    impl LlmProvider for PendingStreamProvider {
        fn id(&self) -> &'static str {
            "openai"
        }

        fn name(&self) -> &'static str {
            "pending stream test provider"
        }

        async fn call(
            &self,
            _request: &LlmRequest,
        ) -> std::result::Result<LlmResponse, AgentError> {
            std::future::pending().await
        }

        async fn call_stream(
            &self,
            _request: &LlmRequest,
            _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> std::result::Result<LlmResponse, AgentError> {
            std::future::pending().await
        }
    }

    #[allow(dead_code)]
    struct InvalidOutputProvider;

    #[async_trait::async_trait]
    impl LlmProvider for InvalidOutputProvider {
        fn id(&self) -> &'static str {
            "openai"
        }

        fn name(&self) -> &'static str {
            "invalid output test provider"
        }

        async fn call(
            &self,
            _request: &LlmRequest,
        ) -> std::result::Result<LlmResponse, AgentError> {
            Ok(LlmResponse {
                content: INVALID_RAW_OUTPUT.to_string(),
                usage: None,
            })
        }

        async fn call_stream(
            &self,
            _request: &LlmRequest,
            on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> std::result::Result<LlmResponse, AgentError> {
            on_chunk(StreamChunk::Delta(INVALID_RAW_OUTPUT.to_string()));
            on_chunk(StreamChunk::Done);
            self.call(_request).await
        }
    }

    #[allow(dead_code)]
    struct ThinkOnlyThenSayProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ThinkOnlyThenSayProvider {
        fn id(&self) -> &'static str {
            "openai"
        }

        fn name(&self) -> &'static str {
            "think-only then say test provider"
        }

        async fn call(
            &self,
            _request: &LlmRequest,
        ) -> std::result::Result<LlmResponse, AgentError> {
            let content = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                r#"{"think":"first internal step"}"#
            } else {
                r#"{"think":"second internal step","say":"period reply"}"#
            };
            Ok(LlmResponse {
                content: content.to_string(),
                usage: None,
            })
        }
    }

    fn dummy_model_row() -> crate::data::ModelRow {
        crate::data::ModelRow {
            id: "unit-test".into(),
            name: "Unit".into(),
            provider: "test".into(),
            api_url: "https://example.test".into(),
            api_type: "OpenAI".into(),
            api_protocol: "openai-v1".into(),
            model_id: "test".into(),
            api_key: Some("sk-test".into()),
            config: None,
        }
    }

    fn make_mgr() -> ModeManager {
        let pool = Arc::new(AgentPool::new().0);
        let data_dir = std::path::PathBuf::from("/tmp/cipher-test");
        let thought_store =
            Arc::new(ThoughtStore::open(&data_dir).expect("test thought store should open"));
        let mut assembler = ContextAssembler::new(
            crate::agent::context_assembler::ContextConfig::default(),
            &data_dir,
            None,
        );
        assembler.set_thought_store(Arc::clone(&thought_store));
        ModeManager::new_with_deps(
            ModeContext::default(),
            ProviderRegistry::new(),
            dummy_model_row(),
            thought_store,
            assembler,
            Registry::new(),
            pool,
            None,
        )
    }

    fn make_mgr_at(data_dir: &Path, registry: ProviderRegistry) -> ModeManager {
        let pool = Arc::new(AgentPool::new().0);
        let thought_store =
            Arc::new(ThoughtStore::open(data_dir).expect("test thought store should open"));
        let mut assembler = ContextAssembler::new(
            crate::agent::context_assembler::ContextConfig::default(),
            data_dir,
            None,
        );
        assembler.set_thought_store(Arc::clone(&thought_store));
        ModeManager::new_with_deps(
            ModeContext::default(),
            registry,
            dummy_model_row(),
            thought_store,
            assembler,
            Registry::new(),
            pool,
            None,
        )
    }

    fn make_mgr_with_receivers(
        data_dir: &Path,
        registry: ProviderRegistry,
    ) -> (
        ModeManager,
        crate::agent::agent_pool::channels::MessageReceivers,
    ) {
        let (pool, receivers) = AgentPool::new();
        let pool = Arc::new(pool);
        let thought_store =
            Arc::new(ThoughtStore::open(data_dir).expect("test thought store should open"));
        let mut assembler = ContextAssembler::new(
            crate::agent::context_assembler::ContextConfig::default(),
            data_dir,
            None,
        );
        assembler.set_thought_store(Arc::clone(&thought_store));
        (
            ModeManager::new_with_deps(
                ModeContext::default(),
                registry,
                dummy_model_row(),
                thought_store,
                assembler,
                Registry::new(),
                pool,
                None,
            ),
            receivers,
        )
    }

    #[allow(dead_code)]
    fn read_named_files(root: &Path, filename: &str) -> Vec<String> {
        let mut contents = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                contents.extend(read_named_files(&path, filename));
            } else if path.file_name().and_then(|name| name.to_str()) == Some(filename) {
                contents.push(fs::read_to_string(path).unwrap());
            }
        }
        contents
    }

    fn find_named_file(root: &Path, filename: &str) -> Option<std::path::PathBuf> {
        for entry in fs::read_dir(root).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = find_named_file(&path, filename) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|name| name.to_str()) == Some(filename) {
                return Some(path);
            }
        }
        None
    }

    #[tokio::test]
    async fn mode_manager_default_is_unni() {
        let mgr = make_mgr();
        assert_eq!(mgr.current_name(), "UNNI");
        assert_eq!(mgr.current_kind(), ModeKind::Unni);
    }

    #[tokio::test]
    async fn mode_manager_cycle_unni_keep_loop_unni() {
        let mut mgr = make_mgr();
        assert_eq!(mgr.current_kind(), ModeKind::Unni);
        mgr.cycle_mode().await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Keep);
        mgr.cycle_mode().await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Loop);
        mgr.cycle_mode().await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Unni);
    }

    #[tokio::test]
    async fn mode_manager_cycle_back_unni_loop_keep_unni() {
        let mut mgr = make_mgr();
        assert_eq!(mgr.current_kind(), ModeKind::Unni);
        mgr.cycle_mode_back().await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Loop);
        mgr.cycle_mode_back().await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Keep);
        mgr.cycle_mode_back().await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Unni);
    }

    #[tokio::test]
    async fn mode_manager_switch_direct() {
        let mut mgr = make_mgr();
        mgr.switch_mode(ModeKind::Loop).await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Loop);
        mgr.switch_mode(ModeKind::Keep).await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Keep);
    }

    #[tokio::test]
    async fn mode_manager_switch_same_mode_is_noop() {
        let mut mgr = make_mgr();

        mgr.switch_mode(ModeKind::Unni).await.unwrap();
        assert_eq!(mgr.current_kind(), ModeKind::Unni);
    }

    #[tokio::test]
    async fn mode_manager_switch_syncs_mode_name_and_hint() {
        let mut mgr = make_mgr();
        mgr.switch_mode(ModeKind::Keep).await.unwrap();

        assert_eq!(mgr.current_kind(), ModeKind::Keep);

        assert_eq!(mgr.current_name(), "KEEP");
    }

    #[tokio::test]
    async fn dual_unni_runs_think_and_say() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(DualProvider::new()));
        let mut manager = make_mgr_at(temporary.path(), providers);

        let response = manager.handle_input("hello").await.unwrap();
        assert_eq!(response.think.as_deref(), Some("think body"));
        assert_eq!(response.text, "say body");
    }

    #[tokio::test]
    async fn dual_loop_only_think() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(DualProvider::new()));
        let mut manager = make_mgr_at(temporary.path(), providers);
        manager.switch_mode(ModeKind::Loop).await.unwrap();

        let response = manager.handle_input("continue loop").await.unwrap();
        assert_eq!(response.think.as_deref(), Some("think body"));
        assert!(response.text.is_empty());
    }

    #[tokio::test]
    async fn dual_keep_with_input_runs_think_and_say() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(DualProvider::new()));
        let mut manager = make_mgr_at(temporary.path(), providers);
        manager.switch_mode(ModeKind::Keep).await.unwrap();

        let response = manager.handle_input("do the task").await.unwrap();
        assert_eq!(response.think.as_deref(), Some("think body"));
        assert_eq!(response.text, "say body");
    }

    #[tokio::test]
    async fn dual_keep_without_input_only_think() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(DualProvider::new()));
        let (mut manager, _receivers) = make_mgr_with_receivers(temporary.path(), providers);
        manager.switch_mode(ModeKind::Keep).await.unwrap();
        let (mut stream_rx, _outcome_rx) = manager.take_channels();

        let thought_id = manager
            .spawn_with_override(
                String::new(),
                Some(ThinkingInput::PlatformInsight {
                    summary: "internal insight".to_string(),
                    has_subagent_result: false,
                }),
            )
            .await
            .unwrap();

        let (signal_id, signal) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("think signal should arrive")
                .expect("stream channel should remain open");
        assert_eq!(signal_id, thought_id);
        assert!(matches!(signal, StreamChunk::Think(ref t) if t == "think body"));

        let (_, signal2) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("done signal should arrive")
                .expect("stream channel should remain open");
        assert_eq!(signal2, StreamChunk::Done);
    }

    #[tokio::test]
    async fn streaming_provider_lookup_failure_persists_terminal_output() {
        let temporary = tempfile::tempdir().unwrap();
        let mut manager = make_mgr_at(temporary.path(), ProviderRegistry::new());

        assert!(manager
            .spawn("provider is missing".to_string())
            .await
            .is_err());

        let timeline = manager.thought_store().recover().unwrap();
        assert_eq!(timeline.groups.len(), 1);
        let output = timeline.groups[0].contexts[0].output.as_ref().unwrap();
        assert!(matches!(
            output.terminal_state,
            ThinkingTerminalState::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn streaming_cancel_persists_before_cancelled_signal() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(PendingStreamProvider));
        let mut manager = make_mgr_at(temporary.path(), providers);
        let (mut stream_rx, _outcome_rx) = manager.take_channels();

        let thought_id = manager
            .spawn("cancel this stream".to_string())
            .await
            .unwrap();
        manager.cancel_latest_active();
        let (signal_id, signal) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("cancel signal should arrive")
                .expect("stream channel should remain open");
        assert_eq!(signal_id, thought_id);
        assert_eq!(signal, StreamChunk::Cancelled);

        let timeline = manager.thought_store().recover().unwrap();
        let output = timeline.groups[0].contexts[0].output.as_ref().unwrap();
        assert!(matches!(
            output.terminal_state,
            ThinkingTerminalState::Cancelled { .. }
        ));
    }

    #[tokio::test]
    async fn streaming_cancel_persistence_failure_reports_outcome_for_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(PendingStreamProvider));
        let mut manager = make_mgr_at(temporary.path(), providers);
        let (mut stream_rx, mut outcome_rx) = manager.take_channels();

        let thought_id = manager
            .spawn("cancel after storage disappears".to_string())
            .await
            .unwrap();
        let input_path = find_named_file(manager.thought_store().root(), "input.json")
            .expect("spawn should persist its input before returning");
        fs::remove_dir_all(input_path.parent().unwrap()).unwrap();

        manager.cancel_latest_active();
        let (signal_id, signal) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("persistence error signal should arrive")
                .expect("stream channel should remain open");
        assert_eq!(signal_id, thought_id);
        assert!(matches!(signal, StreamChunk::Error(_)));

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), outcome_rx.recv())
            .await
            .expect("persistence error outcome should arrive")
            .expect("outcome channel should remain open");
        assert_eq!(outcome.id, thought_id);
        assert!(outcome.result.is_err());
        manager.bookkeep(outcome, "");
        assert!(!manager.active.contains_key(&thought_id));
    }
}

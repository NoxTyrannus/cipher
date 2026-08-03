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
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::model::registry::ProviderRegistry;
use crate::logic::model::stream::StreamChunk;
use std::collections::HashMap;
use std::path::PathBuf;
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

    capability_executor: CapabilityExecutor,

    capability_registry: Registry,

    reload_rx: Option<mpsc::Receiver<crate::logic::capability::executor::ReloadEvent>>,

    duckdb: Option<std::sync::Arc<std::sync::Mutex<duckdb::Connection>>>,

    reload_tx: Option<mpsc::Sender<crate::logic::capability::executor::ReloadEvent>>,

    wasm_modules_dir: Option<PathBuf>,

    workspace_root: Option<PathBuf>,
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
        wasm_modules_dir: Option<PathBuf>,
        workspace_root: Option<PathBuf>,
        duckdb: Option<std::sync::Arc<std::sync::Mutex<duckdb::Connection>>>,
    ) -> Self {
        let (stream_tx, _) = mpsc::channel(128);
        let (pool_tx, _) = mpsc::channel(128);

        let mut capability_executor = CapabilityExecutor::new();
        if let (Some(wd), Some(wr)) = (wasm_modules_dir.as_ref(), workspace_root.as_ref()) {
            capability_executor.set_wasm(wd, wr);
        }
        if let Some(db) = &duckdb {
            capability_executor.set_duckdb(std::sync::Arc::clone(db));
        }

        let (reload_tx, reload_rx) =
            mpsc::channel::<crate::logic::capability::executor::ReloadEvent>(32);
        capability_executor.set_reload_tx(reload_tx.clone());

        let thinking_factory = ThinkingFactory::new_without_provider_tools();

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
            capability_executor,
            capability_registry: registry,
            reload_rx: Some(reload_rx),
            reload_tx: Some(reload_tx),
            duckdb,
            wasm_modules_dir,
            workspace_root,
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
                                    self.capability_registry = new_registry;
                                    let dispatcher =
                                        crate::agent::dispatcher::CapabilityDispatcher::new(
                                            &self.capability_registry,
                                            &self.capability_executor,
                                        );
                                    match dispatcher.authorize_provider_tools("agent") {
                                        Ok(auth_tools) => {
                                            tracing::info!(
                                                "registry reloaded: {} tools authorized",
                                                auth_tools.tools().len()
                                            );
                                            self.thinking_factory.update_provider_tools(auth_tools);
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "registry reload: authorize failed: {e}"
                                            );
                                        }
                                    }
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
        &self,
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
                &self.capability_registry,
                &self.capability_executor,
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
            thinking_factory,
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

        if kind == ModeKind::Keep {
            thinking_factory.reset_keep_quota();
        } else {
            thinking_factory.clear_keep_quota();
        }
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
        let mut instance = self.thinking_factory.create_from_mode_with_keep_say_quota(
            mode_name,
            mode_hint,
            input.clone(),
        )?;
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

        let cap_registry = self.capability_registry.clone();
        let cap_executor = Arc::new(std::mem::take(&mut self.capability_executor));

        self.capability_executor = {
            let mut exec = CapabilityExecutor::new();
            if let (Some(wd), Some(wr)) =
                (self.wasm_modules_dir.as_ref(), self.workspace_root.as_ref())
            {
                exec.set_wasm(wd, wr);
            }
            if let Some(db) = self.duckdb.as_ref() {
                exec.set_duckdb(std::sync::Arc::clone(db));
            }
            if let Some(tx) = self.reload_tx.as_ref() {
                exec.set_reload_tx(tx.clone());
            }
            exec
        };
        let handle = instance
            .spawn_streaming(
                self.stream_tx.clone(),
                self.pool_tx.clone(),
                cancel.clone(),
                self.default_model.clone(),
                provider,
                &self.context_assembler,
                cap_registry,
                cap_executor,
                Arc::clone(&self.agent_pool),
                Arc::clone(&self.thought_store),
                thought_context,
            )
            .await;

        self.active.insert(id.clone(), (cancel, handle));
        self.pending_instances.push(instance);

        Ok(id)
    }

    pub fn cancel_latest_active(&mut self) {
        for inst in self.pending_instances.iter().rev() {
            if let Some((cancel, _handle)) = self.active.get(&inst.id) {
                cancel.notify_one();
                return;
            }
        }
    }

    pub fn keep_say_quota_consumed(&self) -> bool {
        self.thinking_factory.keep_say_quota_consumed()
    }

    pub fn keep_period_finished(&self) -> bool {
        self.thinking_factory.keep_period_finished()
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

        let pool = std::sync::Arc::clone(&self.agent_pool);
        tokio::spawn(async move {
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
    use crate::logic::model::provider::{LlmProvider, LlmRequest, LlmResponse, ToolCall};
    use crate::logic::model::registry::ProviderRegistry;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const INVALID_RAW_OUTPUT: &str =
        "NOVA_RAW_OUTPUT_SENTINEL_7f0d4c8e9a2b6f31_do_not_copy_to_metadata";

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
                tool_calls: vec![],
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
                tool_calls: vec![],
                usage: None,
            })
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

        async fn call_stream(
            &self,
            _request: &LlmRequest,
            _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> std::result::Result<LlmResponse, AgentError> {
            std::future::pending().await
        }
    }

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
                tool_calls: vec![],
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
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    struct UnsolicitedToolProvider {
        calls: Arc<AtomicUsize>,
    }

    impl UnsolicitedToolProvider {
        fn response() -> LlmResponse {
            LlmResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "unsolicited-call".to_string(),
                    name: "guessed_tool".to_string(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for UnsolicitedToolProvider {
        fn id(&self) -> &'static str {
            "openai"
        }

        fn name(&self) -> &'static str {
            "unsolicited tool test provider"
        }

        async fn call(
            &self,
            _request: &LlmRequest,
        ) -> std::result::Result<LlmResponse, AgentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Self::response())
        }

        async fn call_stream(
            &self,
            _request: &LlmRequest,
            _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> std::result::Result<LlmResponse, AgentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Self::response())
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
            None,
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
            None,
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
                None,
                None,
            ),
            receivers,
        )
    }

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
    async fn keep_period_quota_survives_noop_and_resets_after_reentry() {
        let mut mgr = make_mgr();
        mgr.switch_mode(ModeKind::Keep).await.unwrap();
        let first_period = Arc::clone(mgr.thinking_factory.current_keep_quota().unwrap());

        mgr.switch_mode(ModeKind::Keep).await.unwrap();
        assert!(Arc::ptr_eq(
            &first_period,
            mgr.thinking_factory.current_keep_quota().unwrap()
        ));

        mgr.switch_mode(ModeKind::Loop).await.unwrap();
        assert!(mgr.thinking_factory.current_keep_quota().is_none());
        mgr.switch_mode(ModeKind::Keep).await.unwrap();
        assert!(!Arc::ptr_eq(
            &first_period,
            mgr.thinking_factory.current_keep_quota().unwrap()
        ));
    }

    #[tokio::test]
    async fn keep_think_only_does_not_consume_say_quota() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(ThinkOnlyThenSayProvider {
            calls: AtomicUsize::new(0),
        }));
        let mut manager = make_mgr_at(temporary.path(), providers);
        manager.switch_mode(ModeKind::Keep).await.unwrap();

        let think_only = manager.handle_input("internal step").await.unwrap();
        assert!(think_only.text.is_empty());
        assert!(!manager
            .thinking_factory
            .current_keep_quota()
            .unwrap()
            .is_consumed());
    }

    #[tokio::test]
    async fn mode_manager_switch_syncs_mode_name_and_hint() {
        let mut mgr = make_mgr();
        mgr.switch_mode(ModeKind::Keep).await.unwrap();

        assert_eq!(mgr.current_kind(), ModeKind::Keep);

        assert_eq!(mgr.current_name(), "KEEP");
    }

    #[tokio::test]
    async fn blocking_input_persists_success_before_returning() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(SuccessfulProvider));
        let mut manager = make_mgr_at(temporary.path(), providers);

        let response = manager
            .handle_input("remember this exact input")
            .await
            .unwrap();
        assert_eq!(response.text, "durable reply");

        let timeline = manager.thought_store().recover().unwrap();
        assert_eq!(timeline.groups.len(), 1);
        assert_eq!(timeline.groups[0].contexts.len(), 1);
        let context = &timeline.groups[0].contexts[0];
        assert_eq!(
            context.input,
            ThinkingInput::User {
                text: "remember this exact input".to_string()
            }
        );
        assert_eq!(
            context.output.as_ref().unwrap().terminal_state,
            ThinkingTerminalState::Completed
        );
    }

    #[tokio::test]
    async fn blocking_unsolicited_tool_call_fails_once_without_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(UnsolicitedToolProvider {
            calls: Arc::clone(&calls),
        }));
        let mut manager = make_mgr_at(temporary.path(), providers);

        let error = manager
            .handle_input("do not expose thinking tools")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("thinking tools are disabled"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let timeline = manager.thought_store().recover().unwrap();
        let output = timeline.groups[0].contexts[0].output.as_ref().unwrap();
        assert!(matches!(
            output.terminal_state,
            ThinkingTerminalState::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn blocking_invalid_output_is_durable_before_failure_execution_handoff() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(InvalidOutputProvider));
        let (mut manager, mut receivers) = make_mgr_with_receivers(temporary.path(), providers);

        let error = manager
            .handle_input("repair malformed output")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid_json_output"));
        assert!(!error.to_string().contains(INVALID_RAW_OUTPUT));

        let timeline = manager.thought_store().recover().unwrap();
        let context = &timeline.groups[0].contexts[0];
        assert!(matches!(
            context.output.as_ref().unwrap().terminal_state,
            ThinkingTerminalState::Failed { .. }
        ));
        let failure = manager
            .thought_store()
            .load_failure_input(context)
            .unwrap()
            .expect("ThinkingFailureInput must be durable");
        assert_eq!(
            failure.raw_model_output_ref,
            crate::agent::thought::RAW_MODEL_OUTPUT_FILE_NAME
        );
        assert_eq!(
            failure.raw_model_output_bytes,
            INVALID_RAW_OUTPUT.len() as u64
        );
        let failure_json = serde_json::to_string(&failure).unwrap();
        assert!(!failure_json.contains(INVALID_RAW_OUTPUT));
        for output_json in read_named_files(manager.thought_store().root(), "output.json") {
            assert!(!output_json.contains(INVALID_RAW_OUTPUT));
        }
        for failure_json in read_named_files(manager.thought_store().root(), "failure.json") {
            assert!(!failure_json.contains(INVALID_RAW_OUTPUT));
        }

        let failure_context = manager
            .agent_pool
            .get_turn_context(&failure.failure_event_id.to_string())
            .await
            .expect("failure handoff context should exist");
        assert!(!failure_context.thinking.goal.contains(INVALID_RAW_OUTPUT));
        assert!(!failure_context
            .thinking
            .message
            .contains(INVALID_RAW_OUTPUT));

        let handoff = receivers.execution_rx.try_recv().unwrap();
        assert!(matches!(
            handoff,
            crate::agent::communication::AgentMessage::Execute { ref turn_id }
                if turn_id == &failure.failure_event_id.to_string()
        ));
    }

    #[tokio::test]
    async fn keep_second_say_is_stripped_think_routes_and_reentry_resets_quota() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(SuccessfulProvider));
        let (mut manager, mut receivers) = make_mgr_with_receivers(temporary.path(), providers);
        manager.switch_mode(ModeKind::Keep).await.unwrap();

        let first = manager.handle_input("first KEEP turn").await.unwrap();
        assert_eq!(first.text, "durable reply");
        manager.switch_mode(ModeKind::Keep).await.unwrap();

        let second = manager.handle_input("second KEEP turn").await.unwrap();
        assert_eq!(
            second.text, "durable reply",
            "user 消息轮 say 不消耗配额, 不应被剥离"
        );
        assert!(
            !manager
                .thinking_factory
                .current_keep_quota()
                .unwrap()
                .is_consumed(),
            "user 消息轮 say 不应消耗 KEEP 配额"
        );

        let timeline = manager.thought_store().recover().unwrap();
        let contexts = timeline
            .groups
            .iter()
            .flat_map(|group| group.contexts.iter())
            .collect::<Vec<_>>();
        assert_eq!(contexts.len(), 2);
        for context in &contexts {
            assert_eq!(
                context.output.as_ref().unwrap().terminal_state,
                ThinkingTerminalState::Completed,
                "both KEEP turns must complete (no failed quota rejection)"
            );
        }

        let first_handoff = receivers.execution_rx.try_recv().unwrap();
        assert!(matches!(
            first_handoff,
            crate::agent::communication::AgentMessage::Execute { .. }
        ));
        let second_handoff = receivers.execution_rx.try_recv().unwrap();
        assert!(matches!(
            second_handoff,
            crate::agent::communication::AgentMessage::Execute { .. }
        ));

        manager.switch_mode(ModeKind::Unni).await.unwrap();
        manager.switch_mode(ModeKind::Keep).await.unwrap();
        let after_reentry = manager.handle_input("new KEEP period").await.unwrap();
        assert_eq!(after_reentry.text, "durable reply");
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
    async fn streaming_success_is_durable_before_done_signal() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(SuccessfulProvider));
        let mut manager = make_mgr_at(temporary.path(), providers);
        let (mut stream_rx, _outcome_rx) = manager.take_channels();

        let thought_id = manager
            .spawn("finish this stream".to_string())
            .await
            .unwrap();

        let (think_id, think_signal) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("think should arrive")
                .expect("stream channel should remain open");
        assert_eq!(think_id, thought_id);
        assert_eq!(
            think_signal,
            StreamChunk::Think("durable stream work".to_string())
        );

        let (signal_id, signal) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("sanitized say should arrive")
                .expect("stream channel should remain open");
        assert_eq!(signal_id, thought_id);
        assert_eq!(
            signal,
            StreamChunk::Delta("durable stream reply".to_string())
        );

        let timeline = manager.thought_store().recover().unwrap();
        let output = timeline.groups[0].contexts[0].output.as_ref().unwrap();
        assert_eq!(output.terminal_state, ThinkingTerminalState::Completed);
        assert_eq!(output.say.as_deref(), Some("durable stream reply"));

        let (_, terminal) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("done signal should arrive")
                .expect("stream channel should remain open");
        assert_eq!(terminal, StreamChunk::Done);
    }

    #[tokio::test]
    async fn concurrent_keep_streams_publish_exactly_one_say() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(SuccessfulProvider));
        let (mut manager, mut receivers) = make_mgr_with_receivers(temporary.path(), providers);
        manager.switch_mode(ModeKind::Keep).await.unwrap();
        let (mut stream_rx, mut outcome_rx) = manager.take_channels();

        let first_id = manager
            .spawn_with_override(
                "first stream echo".to_string(),
                Some(crate::agent::thought::ThinkingInput::PlatformEcho {
                    platform: crate::agent::thought::InternalPlatform::Memory,
                    summary: "first stream".to_string(),
                    artifact_refs: vec![],
                }),
            )
            .await
            .unwrap();
        let second_id = manager
            .spawn_with_override(
                "second stream echo".to_string(),
                Some(crate::agent::thought::ThinkingInput::PlatformEcho {
                    platform: crate::agent::thought::InternalPlatform::Memory,
                    summary: "second stream".to_string(),
                    artifact_refs: vec![],
                }),
            )
            .await
            .unwrap();
        let expected_ids = [first_id.clone(), second_id.clone()]
            .into_iter()
            .collect::<std::collections::HashSet<_>>();

        let mut think_count = 0usize;
        let mut delta_ids = std::collections::HashSet::new();
        let mut done_ids = std::collections::HashSet::new();

        for _ in 0..5 {
            let (id, signal) =
                tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                    .await
                    .expect("KEEP stream terminal signal should arrive")
                    .expect("stream channel should remain open");
            assert!(expected_ids.contains(&id));
            match signal {
                StreamChunk::Think(think) => {
                    assert_eq!(think, "durable stream work");
                    think_count += 1;
                }
                StreamChunk::Delta(say) => {
                    assert_eq!(say, "durable stream reply");
                    delta_ids.insert(id);
                }
                StreamChunk::Done => {
                    done_ids.insert(id);
                }
                StreamChunk::Error(error) => {
                    panic!("no KEEP stream should error under strip semantics: {error}")
                }
                other => panic!("unexpected KEEP stream signal: {other:?}"),
            }
        }
        assert_eq!(
            think_count, 2,
            "both streams emit Think (second say stripped)"
        );
        assert_eq!(
            delta_ids.len(),
            1,
            "only the first stream emits Delta (say)"
        );
        assert_eq!(done_ids.len(), 2, "both streams complete");

        let mut successful_outcomes = 0;
        let mut failed_outcomes = 0;
        for _ in 0..2 {
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(1), outcome_rx.recv())
                    .await
                    .expect("KEEP stream outcome should arrive")
                    .expect("outcome channel should remain open");
            if outcome.result.is_ok() {
                successful_outcomes += 1;
            } else {
                failed_outcomes += 1;
            }
        }
        assert_eq!((successful_outcomes, failed_outcomes), (2, 0));

        let timeline = manager.thought_store().recover().unwrap();
        let contexts = timeline
            .groups
            .iter()
            .flat_map(|group| group.contexts.iter())
            .collect::<Vec<_>>();
        assert_eq!(contexts.len(), 2);
        for context in &contexts {
            assert_eq!(
                context.output.as_ref().unwrap().terminal_state,
                ThinkingTerminalState::Completed,
                "both KEEP streams must complete under strip semantics"
            );
        }
        let first_completed = contexts
            .iter()
            .find(|context| {
                context.output.as_ref().unwrap().say.as_deref() == Some("durable stream reply")
            })
            .expect("first stream keeps its say");
        let second_completed = contexts
            .iter()
            .find(|context| {
                context.output.as_ref().unwrap().say.is_none()
                    && context.output.as_ref().unwrap().think.as_deref()
                        == Some("durable stream work")
            })
            .expect("second stream say must be stripped, think kept");

        let mut routed_ids = std::collections::HashSet::new();
        for _ in 0..2 {
            match receivers.execution_rx.try_recv().unwrap() {
                crate::agent::communication::AgentMessage::Execute { turn_id } => {
                    routed_ids.insert(turn_id);
                }
                other => panic!("unexpected execution message: {other:?}"),
            }
        }
        assert!(routed_ids.contains(&first_completed.thought_id.to_string()));
        assert!(routed_ids.contains(&second_completed.thought_id.to_string()));
    }

    #[tokio::test]
    async fn streaming_unsolicited_tool_call_persists_failure_before_signal() {
        let temporary = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(UnsolicitedToolProvider {
            calls: Arc::clone(&calls),
        }));
        let mut manager = make_mgr_at(temporary.path(), providers);
        let (mut stream_rx, mut outcome_rx) = manager.take_channels();

        let thought_id = manager
            .spawn("reject streaming tool".to_string())
            .await
            .unwrap();
        let (signal_id, signal) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("error signal should arrive")
                .expect("stream channel should remain open");
        assert_eq!(signal_id, thought_id);
        assert!(matches!(
            signal,
            StreamChunk::Error(ref error) if error.contains("thinking tools are disabled")
        ));
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), outcome_rx.recv())
            .await
            .expect("failed outcome should arrive")
            .expect("outcome channel should remain open");
        assert_eq!(outcome.id, thought_id);
        assert!(outcome.result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let timeline = manager.thought_store().recover().unwrap();
        let output = timeline.groups[0].contexts[0].output.as_ref().unwrap();
        assert!(matches!(
            output.terminal_state,
            ThinkingTerminalState::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn streaming_invalid_output_is_durable_before_error_signal() {
        let temporary = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(InvalidOutputProvider));
        let (mut manager, mut receivers) = make_mgr_with_receivers(temporary.path(), providers);
        let (mut stream_rx, mut outcome_rx) = manager.take_channels();

        let thought_id = manager
            .spawn("repair malformed stream output".to_string())
            .await
            .unwrap();
        let (signal_id, signal) =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
                .await
                .expect("error signal should arrive")
                .expect("stream channel should remain open");
        assert_eq!(signal_id, thought_id);
        assert!(matches!(
            signal,
            StreamChunk::Error(ref error) if error.contains("invalid_json_output")
        ));
        if let StreamChunk::Error(error) = &signal {
            assert!(!error.contains(INVALID_RAW_OUTPUT));
        }
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), outcome_rx.recv())
            .await
            .expect("failed outcome should arrive")
            .expect("outcome channel should remain open");
        assert!(outcome.result.is_err());

        let timeline = manager.thought_store().recover().unwrap();
        let context = &timeline.groups[0].contexts[0];
        let failure = manager
            .thought_store()
            .load_failure_input(context)
            .unwrap()
            .expect("ThinkingFailureInput must be durable");
        assert!(!serde_json::to_string(&failure)
            .unwrap()
            .contains(INVALID_RAW_OUTPUT));
        let failure_context = manager
            .agent_pool
            .get_turn_context(&failure.failure_event_id.to_string())
            .await
            .expect("failure handoff context should exist");
        assert!(!failure_context.thinking.goal.contains(INVALID_RAW_OUTPUT));
        assert!(!failure_context
            .thinking
            .message
            .contains(INVALID_RAW_OUTPUT));
        assert!(matches!(
            receivers.execution_rx.try_recv().unwrap(),
            crate::agent::communication::AgentMessage::Execute { ref turn_id }
                if turn_id == &failure.failure_event_id.to_string()
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

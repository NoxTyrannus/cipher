use crate::common::{AgentError, Result, UtcTimestamp};
use crate::data::thought_store::ThoughtStore;
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::provider::LlmProvider;
use crate::logic::model::provider::LlmResponse;
use crate::logic::model::stream::StreamChunk;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::agent_pool::AgentPool;
use super::communication::{ThinkDecision, ThinkingOutput, TurnContext, TurnStatus};
use super::context_assembler::ContextAssembler;
use super::output::AgentOutput;
use super::thought::{
    DownstreamRequest, ThinkingInput as PersistentThinkingInput,
    ThinkingOutput as PersistentThinkingOutput, ThoughtContext, ThoughtId,
};

/// 思考引擎实例心跳句柄：任务结束（任意 return）自动 abort 心跳任务。
struct HeartbeatGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl HeartbeatGuard {
    fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self { handle }
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn persist_terminal_output(
    thought_store: &ThoughtStore,
    context: &mut ThoughtContext,
    output: PersistentThinkingOutput,
) -> Result<()> {
    if context.output.is_some() {
        return Err(AgentError::Parse(
            "attempted to persist more than one terminal thought output".to_string(),
        ));
    }

    context.set_output(output);
    thought_store.persist_output(context)
}

fn persist_completed_agent_output(
    thought_store: &ThoughtStore,
    context: &mut ThoughtContext,
    output: &AgentOutput,
    downstream: Option<DownstreamRequest>,
) -> Result<()> {
    persist_terminal_output(
        thought_store,
        context,
        PersistentThinkingOutput::completed(output.think.clone(), output.say.clone(), downstream),
    )
}

#[derive(Debug)]
pub struct InstanceOutcome {
    pub id: String,
    pub result: std::result::Result<LlmResponse, AgentError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkState {
    Divergence,
    Precipitation,
    Decision,
    Induction,
    Design,
}

impl ThinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Divergence => "divergence",
            Self::Precipitation => "precipitation",
            Self::Decision => "decision",
            Self::Induction => "induction",
            Self::Design => "design",
        }
    }

    pub fn next(self) -> Option<ThinkState> {
        match self {
            Self::Divergence => Some(Self::Precipitation),
            Self::Precipitation => Some(Self::Decision),
            Self::Decision => Some(Self::Induction),
            Self::Induction => Some(Self::Design),
            Self::Design => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThinkingInstance {
    pub id: String,

    pub thought_id: ThoughtId,

    pub occurred_at: UtcTimestamp,
    pub task_id: String,
    pub state: ThinkState,

    pub mode_kind: Option<ThinkingModeKind>,

    pub mode_hint: String,

    pub input: String,

    input_override: Option<super::thought::ThinkingInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingModeKind {
    Unni,
    Keep,
    Loop,
}

impl ThinkingModeKind {
    pub fn from_mode_name(name: &str) -> Result<Self> {
        match name.to_lowercase().as_str() {
            "unni" => Ok(Self::Unni),
            "keep" => Ok(Self::Keep),
            "loop" => Ok(Self::Loop),
            _ => Err(AgentError::Parse(format!("unknown Thinking mode '{name}'"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unni => "unni",
            Self::Keep => "keep",
            Self::Loop => "loop",
        }
    }
}

impl ThinkingInstance {
    pub fn new(thought_id: ThoughtId, occurred_at: UtcTimestamp, task_id: &str) -> Self {
        Self {
            id: thought_id.to_string(),
            thought_id,
            occurred_at,
            task_id: task_id.to_string(),
            state: ThinkState::Divergence,
            mode_kind: None,
            mode_hint: String::new(),
            input: String::new(),
            input_override: None,
        }
    }

    pub fn new_from_mode(
        thought_id: ThoughtId,
        occurred_at: UtcTimestamp,
        mode: &str,
        mode_hint: String,
        input: String,
    ) -> Result<Self> {
        let mode_kind = ThinkingModeKind::from_mode_name(mode)?;
        let canonical_mode_hint = mode_kind.as_str().to_string();
        if !mode_hint.eq_ignore_ascii_case(&canonical_mode_hint) {
            tracing::warn!(
                mode = %mode,
                supplied_mode_hint = %mode_hint,
                canonical_mode_hint = %canonical_mode_hint,
                "ThinkingInstance mode hint did not match mode; using canonical snapshot"
            );
        }
        Ok(Self {
            id: thought_id.to_string(),
            thought_id,
            occurred_at,
            task_id: input.clone(),
            state: ThinkState::Divergence,
            mode_kind: Some(mode_kind),
            mode_hint: canonical_mode_hint,
            input,
            input_override: None,
        })
    }

    pub fn with_input_override(mut self, input: super::thought::ThinkingInput) -> Self {
        self.input_override = Some(input);
        self
    }

    fn input_kind(&self) -> &'static str {
        match &self.input_override {
            None | Some(PersistentThinkingInput::User { .. }) => "user",
            Some(PersistentThinkingInput::PlatformInsight { .. }) => "insight",
            Some(
                PersistentThinkingInput::ModeTrigger { .. }
                | PersistentThinkingInput::CapabilityResult { .. }
                | PersistentThinkingInput::LegacyInternal,
            ) => "echo",
        }
    }

    pub fn thought_context(&self) -> ThoughtContext {
        let input = self
            .input_override
            .clone()
            .unwrap_or(PersistentThinkingInput::User {
                text: self.input.clone(),
            });
        ThoughtContext::new_at(self.thought_id.clone(), self.occurred_at.clone(), input)
    }

    pub fn advance(&mut self) -> Result<ThinkState> {
        match self.state.next() {
            Some(n) => {
                self.state = n;
                Ok(n)
            }
            None => Err(AgentError::NotImplemented("design terminal".to_string())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_streaming(
        &self,
        stream_tx: mpsc::Sender<(String, StreamChunk)>,
        pool_tx: mpsc::Sender<InstanceOutcome>,
        cancel: Arc<Notify>,
        model_row: crate::data::ModelRow,
        provider: Arc<dyn crate::logic::model::provider::LlmProvider>,
        assembler: &ContextAssembler,
        agent_pool: Arc<AgentPool>,
        thought_store: Arc<ThoughtStore>,
        thought_context: ThoughtContext,
    ) -> JoinHandle<()> {
        self.spawn_streaming_dual(
            stream_tx,
            pool_tx,
            cancel,
            model_row,
            provider,
            assembler,
            agent_pool,
            thought_store,
            thought_context,
        )
        .await
    }

    /// 双脑模式最小可用：非流式 Think+Say，在后台任务中执行，不阻塞调用方。
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_streaming_dual(
        &self,
        stream_tx: mpsc::Sender<(String, StreamChunk)>,
        pool_tx: mpsc::Sender<InstanceOutcome>,
        cancel: Arc<Notify>,
        model_row: crate::data::ModelRow,
        provider: Arc<dyn LlmProvider>,
        assembler: &ContextAssembler,
        agent_pool: Arc<AgentPool>,
        thought_store: Arc<ThoughtStore>,
        thought_context: ThoughtContext,
    ) -> JoinHandle<()> {
        let id = self.id.clone();
        let instance = self.clone();
        let input = self.input.clone();
        let is_user_input = instance.input_override.is_none();
        let pool = Arc::clone(&agent_pool);
        let assembler = assembler.clone();
        let cancel = cancel.clone();

        tokio::spawn(async move {
            pool.register_thinking_instance(&id).await;
            let _heartbeat_guard = HeartbeatGuard::new(AgentPool::spawn_core_heartbeat(
                &pool,
                &id,
                "thinking-engine",
            ));

            let mut thought_context = thought_context;
            let result = DualThinkingHandler
                .run_with_dm(
                    instance,
                    &model_row,
                    provider,
                    &assembler,
                    &pool,
                    &thought_store,
                    &input,
                    is_user_input,
                    &mut thought_context,
                    &cancel,
                )
                .await;

            match result {
                Ok(output) => {
                    if let Some(think) = &output.think {
                        let _ = stream_tx
                            .send((id.clone(), StreamChunk::Think(think.clone())))
                            .await;
                    }
                    if let Some(say) = &output.say {
                        let _ = stream_tx
                            .send((id.clone(), StreamChunk::Delta(say.clone())))
                            .await;
                    }
                    let _ = stream_tx.send((id.clone(), StreamChunk::Done)).await;
                    let _ = pool_tx.try_send(InstanceOutcome {
                        id: id.clone(),
                        result: Ok(LlmResponse {
                            content: String::new(),
                            usage: None,
                        }),
                    });
                }
                Err(error) => {
                    let msg = error.to_string();
                    if msg.contains("cancelled") {
                        let _ = stream_tx.send((id.clone(), StreamChunk::Cancelled)).await;
                    } else {
                        let _ = stream_tx
                            .send((id.clone(), StreamChunk::Error(msg.clone())))
                            .await;
                    }
                    let _ = pool_tx.try_send(InstanceOutcome {
                        id: id.clone(),
                        result: Err(error),
                    });
                }
            }
        })
    }
}

#[async_trait::async_trait]
trait ThinkingSchemeHandler: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn run_with_dm(
        &self,
        instance: ThinkingInstance,
        model_row: &crate::data::ModelRow,
        provider: Arc<dyn LlmProvider>,
        assembler: &ContextAssembler,
        pool: &AgentPool,
        thought_store: &ThoughtStore,
        input: &str,
        is_user_input: bool,
        thought_context: &mut ThoughtContext,
        cancel: &Notify,
    ) -> Result<AgentOutput>;
}

struct DualThinkingHandler;

#[async_trait::async_trait]
impl ThinkingSchemeHandler for DualThinkingHandler {
    #[allow(clippy::too_many_arguments)]
    async fn run_with_dm(
        &self,
        instance: ThinkingInstance,
        model_row: &crate::data::ModelRow,
        provider: Arc<dyn LlmProvider>,
        assembler: &ContextAssembler,
        pool: &AgentPool,
        thought_store: &ThoughtStore,
        input: &str,
        is_user_input: bool,
        thought_context: &mut ThoughtContext,
        cancel: &Notify,
    ) -> Result<AgentOutput> {
        use crate::logic::model::api_key::resolve_api_key;
        use crate::logic::model::provider::LlmRequest;

        let turn_id = instance.id.clone();
        let mode_hint = instance.mode_hint.clone();

        let api_key = resolve_api_key(model_row)?;
        let self_awareness = assembler.build_self_awareness().await;
        let has_user_input = is_user_input;
        // 2.0.8 回环轮执行权（决策冻结 2026-08-21）：
        // - UNNI：洞察回环轮（PlatformInsight）且该轮洞察输入含 subagent 结果段
        //   （has_subagent_result=true）→ 无执行权（输出交付后停，等用户下一次输入）；
        // - UNNI 无结果段 → 有执行权（循环继续）；KEEP/LOOP 回环轮恒有执行权（预算/idle 收敛）。
        let is_insight_loop = matches!(
            &instance.input_override,
            Some(PersistentThinkingInput::PlatformInsight { .. })
        );
        let has_subagent_result = matches!(
            &instance.input_override,
            Some(PersistentThinkingInput::PlatformInsight {
                has_subagent_result: true,
                ..
            })
        );
        let internal_no_downstream = match mode_hint.to_ascii_lowercase().as_str() {
            "unni" => is_insight_loop && has_subagent_result,
            _ => false,
        };
        let input_kind = instance.input_kind();
        let should_say = match mode_hint.to_ascii_lowercase().as_str() {
            "unni" => true,
            "keep" => has_user_input,
            "loop" => false,
            _ => true,
        };

        // Think 调用（可取消）
        let (think_system, think_messages) = assembler
            .build_dual_messages("think", input, &mode_hint)
            .await;
        let think_system = if self_awareness.is_empty() {
            think_system
        } else {
            format!("{think_system}\n\n{self_awareness}")
        };
        let mut think_req =
            LlmRequest::from_model_row(model_row, think_messages.clone(), api_key.clone())?;
        think_req.system = Some(think_system);
        let think_resp = tokio::select! {
            resp = provider.call(&think_req) => resp?,
            _ = cancel.notified() => {
                persist_terminal_output(
                    thought_store,
                    thought_context,
                    PersistentThinkingOutput::cancelled(Some("dual think cancelled".to_string())),
                )?;
                return Err(AgentError::Parse("cancelled".into()));
            }
        };
        let think_text = crate::common::json_util::strip_reasoning_preamble(&think_resp.content)
            .trim()
            .to_string();
        // 2.0.5 say-only 死代码清除：think 为空一律报错（无 say-only 宽松，洞察回环轮亦同）。
        if think_text.is_empty() {
            let error = AgentError::Parse("dual think output empty after stripping CoT".into());
            persist_terminal_output(
                thought_store,
                thought_context,
                PersistentThinkingOutput::failed(error.to_string()),
            )?;
            return Err(error);
        }

        let mut output = AgentOutput {
            think: (!think_text.is_empty()).then_some(think_text.clone()),
            say: None,
        };

        if should_say {
            // Say 调用：共享上下文 + Think 输出 + 用户消息（可取消）
            let (say_system, mut say_messages) = assembler
                .build_dual_messages("say", input, &mode_hint)
                .await;
            if let Some(last) = say_messages.last_mut() {
                if let ChatMessage::User { .. } = last {
                    let user_text = match std::mem::replace(
                        last,
                        ChatMessage::System {
                            text: String::new(),
                            kind: SystemKind::Meta,
                        },
                    ) {
                        ChatMessage::User { text } => text,
                        _ => unreachable!(),
                    };
                    *last = ChatMessage::System {
                        text: format!("[Think Engine output]\n{think_text}"),
                        kind: SystemKind::Meta,
                    };
                    say_messages.push(ChatMessage::User { text: user_text });
                }
            }
            let say_system = if self_awareness.is_empty() {
                say_system
            } else {
                format!("{say_system}\n\n{self_awareness}")
            };
            let mut say_req = LlmRequest::from_model_row(model_row, say_messages, api_key)?;
            say_req.system = Some(say_system);
            let say_resp = tokio::select! {
                resp = provider.call(&say_req) => resp?,
                _ = cancel.notified() => {
                    persist_terminal_output(
                        thought_store,
                        thought_context,
                        PersistentThinkingOutput::cancelled(Some("dual say cancelled".to_string())),
                    )?;
                    return Err(AgentError::Parse("cancelled".into()));
                }
            };
            let say_text = crate::common::json_util::strip_reasoning_preamble(&say_resp.content)
                .trim()
                .to_string();
            output.say = (!say_text.is_empty()).then_some(say_text);
        }

        // 双脑模式专用语义收口：LOOP/KEEP 无输入时不产生 Say。
        if mode_hint.eq_ignore_ascii_case("loop")
            || (mode_hint.eq_ignore_ascii_case("keep") && !has_user_input)
        {
            output.say = None;
        }
        if output.think.is_none() && output.say.is_none() {
            return Err(AgentError::Parse(
                "dual output empty: both think and say are empty".into(),
            ));
        }

        persist_completed_agent_output(
            thought_store,
            thought_context,
            &output,
            if internal_no_downstream {
                None
            } else {
                output
                    .think
                    .clone()
                    .map(|intent| DownstreamRequest::Execute { intent })
            },
        )?;

        // think 恒非空（空已在上游报错），无 say-only 分支。
        let ctx = TurnContext {
            turn_id: turn_id.clone(),
            thinking: ThinkingOutput {
                decision: ThinkDecision::Execute,
                think_message: think_text.clone(),
                constraints: vec![],
            },
            execution: None,
            insight: None,
            memory: None,
            status: TurnStatus::Executing,
            user_message: input.to_string(),
            input_kind: input_kind.into(),
            has_subagent_result: false,
        };
        pool.create_turn_context(ctx).await;
        if internal_no_downstream {
            tracing::debug!(
                "run_dual: internal instance think persisted without downstream execute turn_id={} input_kind={}",
                turn_id, input_kind
            );
        } else if let Err(send_err) = pool.send_execute(&turn_id).await {
            tracing::warn!(
                "run_dual: send_execute failed turn_id={}: {}",
                turn_id,
                send_err
            );
        }

        Ok(output)
    }
}

pub struct ThinkingFactory;

impl ThinkingFactory {
    pub fn new() -> Self {
        Self
    }

    pub fn create(&self, task_id: &str) -> ThinkingInstance {
        ThinkingInstance::new(ThoughtId::new(), UtcTimestamp::now(), task_id)
    }

    pub fn create_from_mode(
        &self,
        mode: &str,
        mode_hint: String,
        input: String,
    ) -> Result<ThinkingInstance> {
        ThinkingInstance::new_from_mode(
            ThoughtId::new(),
            UtcTimestamp::now(),
            mode,
            mode_hint,
            input,
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_dm(
        &self,
        mode_hint: &str,
        input: &str,
        model_row: &crate::data::ModelRow,
        registry: &crate::logic::model::registry::ProviderRegistry,
        assembler: &ContextAssembler,
        pool: &AgentPool,
        thought_store: &ThoughtStore,
    ) -> Result<super::output::AgentOutput> {
        self.run_with_dm_in_period(
            mode_hint,
            input,
            model_row,
            registry,
            assembler,
            pool,
            thought_store,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_with_dm_in_period(
        &self,
        mode_hint: &str,
        input: &str,
        model_row: &crate::data::ModelRow,
        registry: &crate::logic::model::registry::ProviderRegistry,
        assembler: &ContextAssembler,
        pool: &AgentPool,
        thought_store: &ThoughtStore,
    ) -> Result<super::output::AgentOutput> {
        let instance =
            self.create_from_mode(mode_hint, mode_hint.to_string(), input.to_string())?;
        let mut thought_context = instance.thought_context();
        thought_store.persist_input(&thought_context)?;
        let is_user_input = instance.input_override.is_none();
        let cancel = Notify::new();
        let provider_kind = model_row.api_type.to_lowercase();
        let provider = registry
            .pick_by_kind(&provider_kind)
            .cloned()
            .ok_or_else(|| {
                AgentError::Llm(format!(
                    "run_with_dm_in_period: no provider for kind '{provider_kind}' (api_type={})",
                    model_row.api_type
                ))
            })?;

        DualThinkingHandler
            .run_with_dm(
                instance,
                model_row,
                provider,
                assembler,
                pool,
                thought_store,
                input,
                is_user_input,
                &mut thought_context,
                &cancel,
            )
            .await
    }
}

impl Default for ThinkingFactory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_states_have_distinct_strs() {
        let ss = [
            ThinkState::Divergence,
            ThinkState::Precipitation,
            ThinkState::Decision,
            ThinkState::Induction,
            ThinkState::Design,
        ];
        let strs: std::collections::HashSet<_> = ss.iter().map(|s| s.as_str()).collect();
        assert_eq!(strs.len(), 5);
    }

    #[test]
    fn state_chain_divergence_to_design() {
        let mut s = ThinkState::Divergence;
        s = s.next().unwrap();
        assert_eq!(s, ThinkState::Precipitation);
        s = s.next().unwrap();
        assert_eq!(s, ThinkState::Decision);
        s = s.next().unwrap();
        assert_eq!(s, ThinkState::Induction);
        s = s.next().unwrap();
        assert_eq!(s, ThinkState::Design);
        assert!(s.next().is_none());
    }

    #[test]
    fn instance_advances_through_states() {
        let mut inst = ThinkingInstance::new(ThoughtId::new(), UtcTimestamp::now(), "t1");
        assert_eq!(inst.state, ThinkState::Divergence);
        let s = inst.advance().unwrap();
        assert_eq!(s, ThinkState::Precipitation);
        inst.advance().unwrap();
        inst.advance().unwrap();
        inst.advance().unwrap();
        assert_eq!(inst.state, ThinkState::Design);
        assert!(inst.advance().is_err());
    }

    #[test]
    fn factory_creates_unique_instances() {
        let f = ThinkingFactory::new();
        let i1 = f.create("t1");
        let i2 = f.create("t2");
        assert_ne!(i1.id, i2.id);
        assert_ne!(i1.thought_id, i2.thought_id);
        assert_eq!(i1.id, i1.thought_id.to_string());
        assert!(ThoughtId::parse(&i1.id).is_ok());
        assert_eq!(i1.task_id, "t1");
        assert_eq!(i2.task_id, "t2");
    }

    #[test]
    fn thinking_mode_kind_from_name_uppercase_and_lowercase() {
        assert_eq!(
            ThinkingModeKind::from_mode_name("UNNI").unwrap(),
            ThinkingModeKind::Unni
        );
        assert_eq!(
            ThinkingModeKind::from_mode_name("unni").unwrap(),
            ThinkingModeKind::Unni
        );
        assert_eq!(
            ThinkingModeKind::from_mode_name("KEEP").unwrap(),
            ThinkingModeKind::Keep
        );
        assert_eq!(
            ThinkingModeKind::from_mode_name("keep").unwrap(),
            ThinkingModeKind::Keep
        );
        assert_eq!(
            ThinkingModeKind::from_mode_name("LOOP").unwrap(),
            ThinkingModeKind::Loop
        );
        assert_eq!(
            ThinkingModeKind::from_mode_name("loop").unwrap(),
            ThinkingModeKind::Loop
        );
        assert!(ThinkingModeKind::from_mode_name("xxx").is_err());
    }

    #[test]
    fn factory_create_from_mode_unni() {
        let f = ThinkingFactory::new();
        let inst = f
            .create_from_mode("UNNI", "unni".into(), "hello".into())
            .unwrap();
        assert_eq!(inst.mode_kind, Some(ThinkingModeKind::Unni));
        assert_eq!(inst.mode_hint, "unni");
        assert_eq!(inst.input, "hello");
        assert_eq!(inst.state, ThinkState::Divergence);
        assert_eq!(inst.task_id, "hello");
    }

    #[test]
    fn factory_create_from_mode_keep_loop() {
        let f = ThinkingFactory::new();
        let k = f
            .create_from_mode("KEEP", "keep".into(), "task A".into())
            .unwrap();
        assert_eq!(k.mode_kind, Some(ThinkingModeKind::Keep));
        assert_eq!(k.mode_hint, "keep");
        let l = f
            .create_from_mode("LOOP", "loop".into(), "task B".into())
            .unwrap();
        assert_eq!(l.mode_kind, Some(ThinkingModeKind::Loop));
        assert_eq!(l.mode_hint, "loop");

        assert_ne!(k.id, l.id);
    }

    #[test]
    fn factory_canonicalizes_a_mismatched_mode_hint() {
        let factory = ThinkingFactory::new();
        let instance = factory
            .create_from_mode("KEEP", "unni".to_string(), "cannot bypass KEEP".to_string())
            .unwrap();

        assert_eq!(instance.mode_kind, Some(ThinkingModeKind::Keep));
        assert_eq!(instance.mode_hint, "keep");
    }

    #[test]
    fn completed_output_persistence_duplicate_terminal_fails() {
        let temporary = tempfile::tempdir().unwrap();
        let thought_store = ThoughtStore::open(temporary.path()).unwrap();
        let mut context = ThoughtContext::new(PersistentThinkingInput::User {
            text: "force duplicate terminal output".to_string(),
        });
        context.set_output(PersistentThinkingOutput::failed("already terminal"));
        let output = AgentOutput {
            think: Some("work".to_string()),
            say: Some("reply".to_string()),
        };

        let result = persist_completed_agent_output(&thought_store, &mut context, &output, None);

        assert!(result.is_err());
    }

    #[test]
    fn input_kind_tracks_message_source() {
        let factory = ThinkingFactory::new();
        let user = factory
            .create_from_mode("UNNI", "unni".into(), "user input".into())
            .unwrap();
        assert_eq!(user.input_kind(), "user");

        let insight = factory
            .create_from_mode("UNNI", "unni".into(), "insight summary".into())
            .unwrap()
            .with_input_override(PersistentThinkingInput::PlatformInsight {
                summary: "insight summary".into(),
                has_subagent_result: false,
            });
        assert_eq!(insight.input_kind(), "insight");
    }

    struct DualScriptedProvider {
        think: String,
        say: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl DualScriptedProvider {
        fn new(think: impl Into<String>, say: impl Into<String>) -> Self {
            Self {
                think: think.into(),
                say: say.into(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::logic::model::provider::LlmProvider for DualScriptedProvider {
        fn id(&self) -> &'static str {
            "openai"
        }

        fn name(&self) -> &'static str {
            "dual scripted test provider"
        }

        async fn call(
            &self,
            _request: &crate::logic::model::provider::LlmRequest,
        ) -> std::result::Result<
            crate::logic::model::provider::LlmResponse,
            crate::common::AgentError,
        > {
            // 引擎提示词当前留空，无法再用角色文案区分；按调用顺序（think → say）返回。
            let content = if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.think.clone()
            } else {
                self.say.clone()
            };
            Ok(crate::logic::model::provider::LlmResponse {
                content,
                usage: None,
            })
        }

        async fn call_stream(
            &self,
            request: &crate::logic::model::provider::LlmRequest,
            on_chunk: &mut (dyn FnMut(crate::logic::model::stream::StreamChunk) + Send),
        ) -> std::result::Result<
            crate::logic::model::provider::LlmResponse,
            crate::common::AgentError,
        > {
            let response = self.call(request).await?;
            on_chunk(crate::logic::model::stream::StreamChunk::Delta(
                response.content.clone(),
            ));
            on_chunk(crate::logic::model::stream::StreamChunk::Done);
            Ok(response)
        }
    }

    fn dual_test_model_row() -> crate::data::ModelRow {
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

    fn dual_test_assembler(data_dir: &std::path::Path) -> ContextAssembler {
        ContextAssembler::new(
            crate::agent::context_assembler::ContextConfig::default(),
            data_dir,
            None,
        )
    }

    fn unni_insight_instance(summary: &str, has_subagent_result: bool) -> ThinkingInstance {
        ThinkingFactory::new()
            .create_from_mode("UNNI", "unni".into(), summary.into())
            .unwrap()
            .with_input_override(PersistentThinkingInput::PlatformInsight {
                summary: summary.into(),
                has_subagent_result,
            })
    }

    async fn run_dual_test_case(
        instance: ThinkingInstance,
        provider: DualScriptedProvider,
    ) -> (String, crate::agent::agent_pool::channels::MessageReceivers) {
        let temporary = tempfile::tempdir().unwrap();
        let thought_store = Arc::new(ThoughtStore::open(temporary.path()).unwrap());
        let (pool, receivers) = AgentPool::new();
        let assembler = dual_test_assembler(temporary.path());
        let id = instance.id.clone();
        let mut context = instance.thought_context();
        thought_store.persist_input(&context).unwrap();

        DualThinkingHandler
            .run_with_dm(
                instance,
                &dual_test_model_row(),
                Arc::new(provider),
                &assembler,
                &pool,
                &thought_store,
                "test input",
                false,
                &mut context,
                &tokio::sync::Notify::new(),
            )
            .await
            .unwrap();

        let _ = pool.get_turn_context(&id).await.unwrap();
        (id, receivers)
    }

    #[tokio::test]
    async fn unni_insight_loop_with_subagent_result_has_no_execution_right() {
        // 2.0.8：UNNI 洞察回环轮，洞察输入含 subagent 结果段 → 无执行权（交付后停）。
        let instance = unni_insight_instance("insight summary", true);
        let (id, mut receivers) = run_dual_test_case(
            instance,
            DualScriptedProvider::new("internal plan", "user visible report"),
        )
        .await;
        let _ = id;
        assert!(matches!(
            receivers.execution_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty
                | tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn unni_insight_loop_without_subagent_result_keeps_execution_right() {
        // 2.0.8：UNNI 洞察回环轮，洞察输入无 subagent 结果段 → 有执行权（循环继续）。
        let instance = unni_insight_instance("insight summary", false);
        let (_id, mut receivers) = run_dual_test_case(
            instance,
            DualScriptedProvider::new("internal plan", "user visible report"),
        )
        .await;
        assert!(matches!(
            receivers.execution_rx.try_recv(),
            Ok(crate::agent::communication::AgentMessage::Execute { .. })
        ));
    }

    #[tokio::test]
    async fn loop_insight_loop_keeps_execution_right() {
        // 2.0.8：LOOP 洞察回环轮恒有执行权（mix 机制删除后 LOOP 为普通 think 实例循环）。
        let temporary = tempfile::tempdir().unwrap();
        let thought_store = Arc::new(ThoughtStore::open(temporary.path()).unwrap());
        let (pool, mut receivers) = AgentPool::new();
        let assembler = dual_test_assembler(temporary.path());
        let instance = ThinkingFactory::new()
            .create_from_mode("LOOP", "loop".into(), "insight summary".into())
            .unwrap()
            .with_input_override(PersistentThinkingInput::PlatformInsight {
                summary: "insight summary".into(),
                has_subagent_result: true,
            });
        let id = instance.id.clone();
        let mut context = instance.thought_context();
        thought_store.persist_input(&context).unwrap();

        DualThinkingHandler
            .run_with_dm(
                instance,
                &dual_test_model_row(),
                Arc::new(DualScriptedProvider::new("internal plan", String::new())),
                &assembler,
                &pool,
                &thought_store,
                "insight summary",
                false,
                &mut context,
                &tokio::sync::Notify::new(),
            )
            .await
            .unwrap();

        let ctx = pool.get_turn_context(&id).await.unwrap();
        assert_eq!(ctx.input_kind, "insight");
        assert!(matches!(
            receivers.execution_rx.try_recv(),
            Ok(crate::agent::communication::AgentMessage::Execute { .. })
        ));
    }

    #[tokio::test]
    async fn unni_user_think_still_sends_execute() {
        let temporary = tempfile::tempdir().unwrap();
        let thought_store = Arc::new(ThoughtStore::open(temporary.path()).unwrap());
        let (pool, mut receivers) = AgentPool::new();
        let assembler = dual_test_assembler(temporary.path());
        let instance = ThinkingFactory::new()
            .create_from_mode("UNNI", "unni".into(), "user task".into())
            .unwrap();
        let id = instance.id.clone();
        let mut context = instance.thought_context();
        thought_store.persist_input(&context).unwrap();

        DualThinkingHandler
            .run_with_dm(
                instance,
                &dual_test_model_row(),
                Arc::new(DualScriptedProvider::new("execute task", "on it")),
                &assembler,
                &pool,
                &thought_store,
                "user task",
                true,
                &mut context,
                &tokio::sync::Notify::new(),
            )
            .await
            .unwrap();

        let ctx = pool.get_turn_context(&id).await.unwrap();
        assert_eq!(ctx.input_kind, "user");
        assert!(matches!(
            receivers.execution_rx.try_recv(),
            Ok(crate::agent::communication::AgentMessage::Execute { .. })
        ));
    }
}

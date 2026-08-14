use crate::common::{AgentError, Result, UtcTimestamp};
use crate::data::thought_store::ThoughtStore;
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::provider::LlmResponse;
use crate::logic::model::stream::StreamChunk;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::agent_pool::AgentPool;
use super::communication::{ThinkDecision, ThinkingOutput, TurnContext, TurnStatus};
use super::context_assembler::ContextAssembler;
use super::output::{
    parse_agent_output, strip_loop_say, validate_agent_output, AgentOutput, OutputValidationError,
};
use super::thought::{
    DownstreamRequest, ThinkingFailureInput, ThinkingInput as PersistentThinkingInput,
    ThinkingOutput as PersistentThinkingOutput, ThoughtContext, ThoughtId,
};

#[derive(Debug, Default)]
pub struct KeepSayQuota {
    consumed: AtomicBool,
}

impl KeepSayQuota {
    pub fn new() -> Self {
        Self::default()
    }

    fn try_reserve(self: &Arc<Self>) -> Option<KeepSayReservation> {
        self.consumed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| KeepSayReservation {
                quota: Arc::clone(self),
                committed: false,
            })
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.consumed.load(Ordering::Acquire)
    }

    pub(crate) fn is_consumed(&self) -> bool {
        self.consumed.load(Ordering::Acquire)
    }
}

struct KeepSayReservation {
    quota: Arc<KeepSayQuota>,
    committed: bool,
}

impl KeepSayReservation {
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for KeepSayReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.quota.consumed.store(false, Ordering::Release);
        }
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
    keep_say_reservation: Option<KeepSayReservation>,
) -> Result<()> {
    persist_terminal_output(
        thought_store,
        context,
        PersistentThinkingOutput::completed(output.think.clone(), output.say.clone(), downstream),
    )?;
    if let Some(reservation) = keep_say_reservation {
        reservation.commit();
    }
    Ok(())
}

enum ValidatedAgentOutput {
    Valid {
        output: AgentOutput,
        keep_say_reservation: Option<KeepSayReservation>,
    },
    Invalid(ThinkingFailureInput),
}

fn validate_output(
    raw_model_output: &str,
    mode_snapshot: &str,
    keep_say_quota: Option<&Arc<KeepSayQuota>>,
    input_kind: &str,
) -> std::result::Result<ValidatedAgentOutput, Vec<OutputValidationError>> {
    let mut output = parse_agent_output(raw_model_output)?;
    strip_loop_say(&mut output, mode_snapshot);

    if mode_snapshot.eq_ignore_ascii_case("keep")
        && input_kind == "echo"
        && keep_say_quota.is_some_and(|quota| quota.is_consumed())
    {
        if let Some(dropped) = output.say.take() {
            tracing::warn!(
                say_len = dropped.len(),
                "KEEP say quota exhausted: say mechanically stripped (period report already sent)"
            );
        }
    }
    validate_agent_output(&output, mode_snapshot)?;

    let keep_say_reservation = if mode_snapshot.eq_ignore_ascii_case("keep") {
        if let Some(say) = &output.say {
            if say.trim().is_empty() {
                None
            } else {
                if input_kind != "echo" {
                    None
                } else {
                    let Some(quota) = keep_say_quota else {
                        return Err(vec![OutputValidationError::new(
                            "keep_say_quota_missing",
                            "KEEP output cannot publish say without a frozen period quota",
                        )]);
                    };
                    match quota.try_reserve() {
                        Some(reservation) => Some(reservation),
                        None => {
                            return Err(vec![OutputValidationError::new(
                                "keep_say_quota_exhausted",
                                "continuous KEEP period permits at most one say output",
                            )]);
                        }
                    }
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    Ok(ValidatedAgentOutput::Valid {
        output,
        keep_say_reservation,
    })
}

fn is_retryable_error(errors: &[OutputValidationError]) -> bool {
    !errors
        .iter()
        .any(|e| matches!(e.code.as_str(), "keep_say_quota_exhausted" | "unknown_mode"))
}

fn validate_and_persist_model_output(
    thought_store: &ThoughtStore,
    context: &mut ThoughtContext,
    mode_snapshot: &str,
    raw_model_output: &str,
    keep_say_quota: Option<&Arc<KeepSayQuota>>,
    input_kind: &str,
) -> Result<ValidatedAgentOutput> {
    match validate_output(raw_model_output, mode_snapshot, keep_say_quota, input_kind) {
        Ok(valid) => Ok(valid),
        Err(errors) => persist_invalid_model_output(
            thought_store,
            context,
            mode_snapshot,
            raw_model_output,
            errors,
        ),
    }
}

fn persist_invalid_model_output(
    thought_store: &ThoughtStore,
    context: &mut ThoughtContext,
    mode_snapshot: &str,
    raw_model_output: &str,
    validation: Vec<OutputValidationError>,
) -> Result<ValidatedAgentOutput> {
    let error_summary = validation_error_summary(&validation);
    let failure = ThinkingFailureInput::new(
        context.thought_id.clone(),
        context.occurred_at.clone(),
        mode_snapshot,
        raw_model_output,
        validation,
    )?;

    persist_terminal_output(
        thought_store,
        context,
        PersistentThinkingOutput::failed(error_summary),
    )?;
    thought_store.persist_failure_input(context, &failure, raw_model_output.as_bytes())?;
    Ok(ValidatedAgentOutput::Invalid(failure))
}

fn validation_error_summary(errors: &[OutputValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn thinking_failure_error(failure: &ThinkingFailureInput) -> AgentError {
    AgentError::ThinkingOutputInvalid(validation_error_summary(&failure.validation_errors))
}

async fn enqueue_thinking_failure(
    pool: &AgentPool,
    failure: &ThinkingFailureInput,
    original_input: &str,
) -> Result<()> {
    let failure_payload = serde_json::to_string(failure)
        .map_err(|error| AgentError::Parse(format!("serialize ThinkingFailureInput: {error}")))?;
    let failure_event_id = failure.failure_event_id.to_string();
    pool.create_turn_context(TurnContext {
        turn_id: failure_event_id.clone(),
        thinking: ThinkingOutput {
            decision: ThinkDecision::Failure,
            goal: failure_payload.clone(),
            constraints: vec![
                "repair invalid Thinking output through the full platform chain".to_string(),
            ],
            message: failure_payload,
        },
        execution: None,
        insight: None,
        memory: None,
        status: TurnStatus::Executing,
        user_message: original_input.to_string(),
        input_kind: "user".into(),
        say_published: false,
    })
    .await;
    if let Err(error) =
        pool.message_bus()
            .send_to_execution(super::communication::AgentMessage::Execute {
                turn_id: failure_event_id,
            })
    {
        tracing::warn!(
            "enqueue_thinking_failure: DM send failed (failure already persisted, \
             execution platform will pick up via cursor scan): {error:?}"
        );
    }
    Ok(())
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

    keep_say_quota: Option<Arc<KeepSayQuota>>,
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
            keep_say_quota: None,
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
            keep_say_quota: None,
            input_override: None,
        })
    }

    fn with_keep_say_quota(mut self, quota: Option<Arc<KeepSayQuota>>) -> Self {
        self.keep_say_quota = quota;
        self
    }

    pub fn with_input_override(mut self, input: super::thought::ThinkingInput) -> Self {
        self.input_override = Some(input);
        self
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

    pub async fn run(
        &mut self,
        model_row: &crate::data::ModelRow,
        registry: &crate::logic::model::registry::ProviderRegistry,
        assembler: &ContextAssembler,
    ) -> Result<LlmResponse> {
        use crate::logic::model::api_key::resolve_api_key;
        use crate::logic::model::provider::LlmRequest;

        let (system_prompt, messages) =
            assembler.build_messages(&self.input, &self.mode_hint).await;

        let self_awareness = assembler.build_self_awareness().await;
        let system_prompt = if self_awareness.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{self_awareness}")
        };

        let api_key = resolve_api_key(model_row)?;

        let provider_kind = model_row.api_type.to_lowercase();
        let provider = registry.pick_by_kind(&provider_kind).ok_or_else(|| {
            AgentError::Llm(format!(
                "run: 无 provider impl for kind '{}' (registry 未注册, api_type={})",
                provider_kind, model_row.api_type
            ))
        })?;

        let mut req = LlmRequest::from_model_row(model_row, messages, api_key)?;
        req.system = Some(system_prompt);

        let resp = provider.call(&req).await?;

        if !resp.tool_calls.is_empty() {
            tracing::warn!(
                "run: thinking engine has no tools; ignoring {} unexpected tool_call(s)",
                resp.tool_calls.len()
            );
        }

        self.state = ThinkState::Design;
        Ok(resp)
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
        mut thought_context: ThoughtContext,
    ) -> JoinHandle<()> {
        use crate::logic::model::api_key::resolve_api_key;
        use crate::logic::model::provider::LlmRequest;

        let id = self.id.clone();

        let (system_prompt, messages) =
            assembler.build_messages(&self.input, &self.mode_hint).await;

        let self_awareness = assembler.build_self_awareness().await;
        let system_prompt = if self_awareness.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{self_awareness}")
        };

        let api_key = match resolve_api_key(&model_row) {
            Ok(k) => k,
            Err(e) => {
                let signal_error = match persist_terminal_output(
                    &thought_store,
                    &mut thought_context,
                    PersistentThinkingOutput::failed(e.to_string()),
                ) {
                    Ok(()) => e,
                    Err(persist_error) => {
                        tracing::error!(
                            "spawn_streaming: failed to persist API-key failure for thought_id={}: {}",
                            id,
                            persist_error
                        );
                        persist_error
                    }
                };
                if let Err(send_e) =
                    stream_tx.try_send((id.clone(), StreamChunk::Error(signal_error.to_string())))
                {
                    tracing::warn!(
                        "spawn_streaming: stream_tx send error turn_id={}, error={}",
                        id,
                        send_e
                    );
                }
                if let Err(send_e) = pool_tx.try_send(InstanceOutcome {
                    id: id.clone(),
                    result: Err(signal_error),
                }) {
                    tracing::warn!(
                        "spawn_streaming: pool_tx send error turn_id={}, error={}",
                        id,
                        send_e
                    );
                }
                return tokio::spawn(async {});
            }
        };

        let id2 = id.clone();
        let user_message = self.input.clone();

        let input_kind2 = match &self.input_override {
            Some(super::thought::ThinkingInput::PlatformEcho { .. }) => "echo".to_string(),
            Some(super::thought::ThinkingInput::ReflectOnly { .. }) => "reflect".to_string(),
            Some(super::thought::ThinkingInput::ModeTrigger { .. }) => "mode".to_string(),
            _ => "user".to_string(),
        };
        let reflect_only = matches!(
            self.input_override,
            Some(super::thought::ThinkingInput::ReflectOnly { .. })
        );
        let stream_tx2 = stream_tx.clone();
        let pool_tx2 = pool_tx.clone();
        let cancel2 = cancel.clone();
        let pool = Arc::clone(&agent_pool);
        let thought_store2 = Arc::clone(&thought_store);
        let mode_snapshot = self.mode_hint.clone();
        let keep_say_quota = self.keep_say_quota.clone();

        tokio::spawn(async move {
            let say_published2: bool;

            let mut on_chunk = {
                let id = id2.clone();
                move |_chunk: StreamChunk| {
                    tracing::trace!(
                        "spawn_streaming: withheld provider chunk until validation, thought_id={}",
                        id
                    );
                }
            };

            let mut thought_context = thought_context;

            let phase_t0 = std::time::Instant::now();
            tracing::info!(
                "spawn_streaming: [timing] request build start thought_id={}",
                id2
            );
            let req = match LlmRequest::from_model_row(&model_row, messages, api_key.clone()) {
                Ok(mut request) => {
                    request.system = Some(system_prompt.clone());
                    request
                }
                Err(e) => {
                    let signal_error = match persist_terminal_output(
                        &thought_store2,
                        &mut thought_context,
                        PersistentThinkingOutput::failed(e.to_string()),
                    ) {
                        Ok(()) => e,
                        Err(persist_error) => {
                            tracing::error!(
                                "spawn_streaming: failed to persist request-build failure for thought_id={}: {}",
                                id2,
                                persist_error
                            );
                            persist_error
                        }
                    };
                    if let Err(send_e) = stream_tx2
                        .try_send((id2.clone(), StreamChunk::Error(signal_error.to_string())))
                    {
                        tracing::warn!(
                            "spawn_streaming: stream_tx2 send error turn_id={}, error={}",
                            id2,
                            send_e
                        );
                    }
                    if let Err(send_e) = pool_tx2.try_send(InstanceOutcome {
                        id: id2.clone(),
                        result: Err(signal_error),
                    }) {
                        tracing::warn!(
                            "spawn_streaming: pool_tx2 send error turn_id={}, error={}",
                            id2,
                            send_e
                        );
                    }
                    return;
                }
            };

            tracing::info!(
                "spawn_streaming: [timing] call_stream start thought_id={}",
                id2
            );
            // 指数退避重试（缺陷2 配套）：可重试错误（429/408/5xx/超时/网络）退避后重试，
            // 起始 3s、60s 封顶、无总次数硬限；进度经 StreamChunk::Status 暴露给用户。
            let mut llm_attempt: u32 = 0;
            let resp = loop {
                let attempt_err = tokio::select! {
                    resp = provider.call_stream(&req, &mut on_chunk) => {
                        match resp {
                            Ok(r) => {
                                tracing::info!("spawn_streaming: [timing] call_stream returned {:?} thought_id={}", phase_t0.elapsed(), id2);
                                break r;
                            }
                            Err(e) => e,
                        }
                    }
                    _ = cancel2.notified() => {
                        match persist_terminal_output(
                            &thought_store2,
                            &mut thought_context,
                            PersistentThinkingOutput::cancelled(Some("stream cancelled".to_string())),
                        ) {
                            Ok(()) => {
                                if let Err(send_e) = stream_tx2.try_send((id2.clone(), StreamChunk::Cancelled)) {
                                    tracing::warn!("spawn_streaming: stream_tx2 send error turn_id={}, error={}", id2, send_e);
                                }
                            }
                            Err(persist_error) => {
                                tracing::error!(
                                    "spawn_streaming: failed to persist cancellation for thought_id={}: {}",
                                    id2,
                                    persist_error
                                );
                                if let Err(send_e) = stream_tx2.try_send((
                                    id2.clone(),
                                    StreamChunk::Error(persist_error.to_string()),
                                )) {
                                    tracing::warn!("spawn_streaming: stream_tx2 send error turn_id={}, error={}", id2, send_e);
                                }
                                if let Err(send_e) = pool_tx2.try_send(InstanceOutcome {
                                    id: id2.clone(),
                                    result: Err(persist_error),
                                }) {
                                    tracing::warn!("spawn_streaming: pool_tx2 send error turn_id={}, error={}", id2, send_e);
                                }
                            }
                        }
                        return;
                    }
                };

                // —— 失败分类：可重试 → 指数退避后继续；永久 → 走原失败路径 ——
                if crate::logic::model::is_retryable_llm_error(&attempt_err) {
                    llm_attempt += 1;
                    let delay_secs = crate::logic::model::backoff_delay_secs(llm_attempt);
                    tracing::warn!(
                        "spawn_streaming: LLM call failed (retryable, attempt={llm_attempt}) thought_id={id2}, backoff {delay_secs}s: {attempt_err}"
                    );
                    let _ = stream_tx2.try_send((
                        id2.clone(),
                        StreamChunk::Status(format!(
                            "思考请求失败（{attempt_err}），{delay_secs}s 后重试（第 {llm_attempt} 次）"
                        )),
                    ));
                    tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                    continue;
                }

                let signal_error = match persist_terminal_output(
                    &thought_store2,
                    &mut thought_context,
                    PersistentThinkingOutput::failed(attempt_err.to_string()),
                ) {
                    Ok(()) => attempt_err,
                    Err(persist_error) => {
                        tracing::error!(
                            "spawn_streaming: failed to persist provider failure for thought_id={}: {}",
                            id2,
                            persist_error
                        );
                        persist_error
                    }
                };
                if let Err(send_e) = stream_tx2.try_send((id2.clone(), StreamChunk::Error(signal_error.to_string()))) {
                    tracing::warn!("spawn_streaming: stream_tx2 send error turn_id={}, error={}", id2, send_e);
                }
                if let Err(send_e) = pool_tx2.try_send(InstanceOutcome { id: id2.clone(), result: Err(signal_error) }) {
                    tracing::warn!("spawn_streaming: pool_tx2 send error turn_id={}, error={}", id2, send_e);
                }
                return;
            };

            if !resp.tool_calls.is_empty() {
                tracing::warn!(
                    "spawn_streaming: thinking engine has no tools; ignoring {} unexpected tool_call(s) thought_id={}",
                    resp.tool_calls.len(),
                    id2
                );
            }

            let final_response = resp;
            let final_content = final_response.content.clone();
            let (output, keep_say_reservation) = match validate_and_persist_model_output(
                &thought_store2,
                &mut thought_context,
                &mode_snapshot,
                &final_content,
                keep_say_quota.as_ref(),
                &input_kind2,
            ) {
                Ok(ValidatedAgentOutput::Valid {
                    output,
                    keep_say_reservation,
                }) => {
                    say_published2 = output.say.is_some();
                    (output, keep_say_reservation)
                }
                Ok(ValidatedAgentOutput::Invalid(failure)) => {
                    let signal_error =
                        match enqueue_thinking_failure(&pool, &failure, &user_message).await {
                            Ok(()) => thinking_failure_error(&failure),
                            Err(enqueue_error) => enqueue_error,
                        };
                    if let Err(send_error) = stream_tx2
                        .try_send((id2.clone(), StreamChunk::Error(signal_error.to_string())))
                    {
                        tracing::warn!(
                            "spawn_streaming: stream_tx2 send error turn_id={}, error={}",
                            id2,
                            send_error
                        );
                    }
                    if let Err(send_error) = pool_tx2.try_send(InstanceOutcome {
                        id: id2.clone(),
                        result: Err(signal_error),
                    }) {
                        tracing::warn!(
                            "spawn_streaming: pool_tx2 send error turn_id={}, error={}",
                            id2,
                            send_error
                        );
                    }
                    return;
                }
                Err(persist_error) => {
                    tracing::error!(
                        "spawn_streaming: failed to persist invalid output for thought_id={}: {}",
                        id2,
                        persist_error
                    );
                    if let Err(send_error) = stream_tx2
                        .try_send((id2.clone(), StreamChunk::Error(persist_error.to_string())))
                    {
                        tracing::warn!(
                            "spawn_streaming: stream_tx2 send error turn_id={}, error={}",
                            id2,
                            send_error
                        );
                    }
                    if let Err(send_error) = pool_tx2.try_send(InstanceOutcome {
                        id: id2.clone(),
                        result: Err(persist_error),
                    }) {
                        tracing::warn!(
                            "spawn_streaming: pool_tx2 send error turn_id={}, error={}",
                            id2,
                            send_error
                        );
                    }
                    return;
                }
            };
            if let Err(persist_error) = persist_completed_agent_output(
                &thought_store2,
                &mut thought_context,
                &output,
                output
                    .think
                    .clone()
                    .map(|intent| DownstreamRequest::Execute { intent }),
                keep_say_reservation,
            ) {
                tracing::error!(
                    "spawn_streaming: failed to persist completed output for thought_id={}: {}",
                    id2,
                    persist_error
                );
                if let Err(send_e) = stream_tx2
                    .try_send((id2.clone(), StreamChunk::Error(persist_error.to_string())))
                {
                    tracing::warn!(
                        "spawn_streaming: stream_tx2 send error turn_id={}, error={}",
                        id2,
                        send_e
                    );
                }
                if let Err(send_e) = pool_tx2.try_send(InstanceOutcome {
                    id: id2.clone(),
                    result: Err(persist_error),
                }) {
                    tracing::warn!(
                        "spawn_streaming: pool_tx2 send error turn_id={}, error={}",
                        id2,
                        send_e
                    );
                }
                return;
            }

            if let Some(think) = output.think {
                if let Err(send_e) = stream_tx2
                    .send((id2.clone(), StreamChunk::Think(think.clone())))
                    .await
                {
                    tracing::warn!(
                        "spawn_streaming: failed to publish think text turn_id={}, error={}",
                        id2,
                        send_e
                    );
                }
                let ctx = TurnContext {
                    turn_id: id2.clone(),
                    thinking: ThinkingOutput {
                        decision: if reflect_only {
                            ThinkDecision::Reply
                        } else {
                            ThinkDecision::Execute
                        },
                        goal: think.clone(),
                        constraints: vec![],
                        message: think,
                    },
                    execution: None,
                    insight: None,
                    memory: None,
                    status: if reflect_only {
                        TurnStatus::Done
                    } else {
                        TurnStatus::Executing
                    },
                    user_message,
                    input_kind: input_kind2.clone(),
                    say_published: say_published2,
                };
                pool.create_turn_context(ctx).await;
                if reflect_only {
                    // 融合思考反思实例：只产出 think 文本，不触发执行链，直接完成。
                    tracing::info!(
                        "spawn_streaming: turn_id={}, reflect-only instance finished (no execution)",
                        id2
                    );
                } else {
                    if let Err(send_err) = pool.send_execute(&id2).await {
                        tracing::warn!(
                            "spawn_streaming: send_execute failed turn_id={}: {}",
                            id2,
                            send_err
                        );
                    }
                    tracing::info!("spawn_streaming: turn_id={}, Execute DM sent", id2);
                }
            } else {
                tracing::debug!(
                    "spawn_streaming: turn_id={}, say-only output, routing to insight+memory chain (no execution)",
                    id2
                );
                let ctx = TurnContext {
                    turn_id: id2.clone(),
                    thinking: ThinkingOutput {
                        decision: ThinkDecision::Reply,
                        goal: output.say.clone().unwrap_or_default(),
                        constraints: vec![],
                        message: output.say.clone().unwrap_or_default(),
                    },
                    execution: None,
                    insight: None,
                    memory: None,
                    status: TurnStatus::Insighting,
                    user_message,
                    input_kind: input_kind2.clone(),
                    say_published: say_published2,
                };
                pool.create_turn_context(ctx).await;
                if let Err(send_err) = pool.send_execution_done(&id2).await {
                    tracing::warn!(
                        "spawn_streaming: say-only send_execution_done (insight) failed turn_id={}: {}",
                        id2,
                        send_err
                    );
                }
            }

            if let Some(say) = output.say {
                if let Err(send_e) = stream_tx2
                    .send((id2.clone(), StreamChunk::Delta(say)))
                    .await
                {
                    tracing::warn!(
                        "spawn_streaming: failed to publish durable say turn_id={}, error={}",
                        id2,
                        send_e
                    );
                }
            }
            if let Err(send_e) = stream_tx2.send((id2.clone(), StreamChunk::Done)).await {
                tracing::warn!(
                    "spawn_streaming: stream_tx2 send error turn_id={}, error={}",
                    id2,
                    send_e
                );
            }
            if let Err(send_e) = pool_tx2.try_send(InstanceOutcome {
                id: id2.clone(),
                result: Ok(final_response),
            }) {
                tracing::warn!(
                    "spawn_streaming: pool_tx2 send error turn_id={}, error={}",
                    id2,
                    send_e
                );
            }
        })
    }
}

pub struct ThinkingFactory {
    current_keep_quota: Option<Arc<KeepSayQuota>>,
}

impl ThinkingFactory {
    pub fn new() -> Self {
        Self {
            current_keep_quota: None,
        }
    }

    pub fn reset_keep_quota(&mut self) {
        self.current_keep_quota = Some(Arc::new(KeepSayQuota::new()));
    }

    pub fn clear_keep_quota(&mut self) {
        self.current_keep_quota = None;
    }

    pub fn current_keep_quota(&self) -> Option<&Arc<KeepSayQuota>> {
        self.current_keep_quota.as_ref()
    }

    pub fn keep_say_quota_consumed(&self) -> bool {
        self.current_keep_quota
            .as_ref()
            .is_some_and(|quota| quota.is_consumed())
    }

    pub fn keep_period_finished(&self) -> bool {
        self.current_keep_quota
            .as_ref()
            .is_some_and(|quota| quota.is_finished())
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
        self.create_from_mode_with_keep_say_quota(mode, mode_hint, input)
    }

    pub(crate) fn create_from_mode_with_keep_say_quota(
        &self,
        mode: &str,
        mode_hint: String,
        input: String,
    ) -> Result<ThinkingInstance> {
        let mode_kind = ThinkingModeKind::from_mode_name(mode)?;
        let keep_say_quota = match mode_kind {
            ThinkingModeKind::Keep => Some(
                self.current_keep_quota
                    .clone()
                    .unwrap_or_else(|| Arc::new(KeepSayQuota::new())),
            ),
            ThinkingModeKind::Unni | ThinkingModeKind::Loop => None,
        };
        Ok(ThinkingInstance::new_from_mode(
            ThoughtId::new(),
            UtcTimestamp::now(),
            mode,
            mode_hint,
            input,
        )?
        .with_keep_say_quota(keep_say_quota))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        mode_hint: &str,
        input: &str,
        model_row: &crate::data::ModelRow,
        registry: &crate::logic::model::registry::ProviderRegistry,
        assembler: &ContextAssembler,
        thought_store: &ThoughtStore,
    ) -> Result<super::output::AgentOutput> {
        let mut instance =
            self.create_from_mode(mode_hint, mode_hint.to_string(), input.to_string())?;
        let mut thought_context = instance.thought_context();
        thought_store.persist_input(&thought_context)?;

        let llm_resp = match instance.run(model_row, registry, assembler).await {
            Ok(response) => response,
            Err(error) => {
                persist_terminal_output(
                    thought_store,
                    &mut thought_context,
                    PersistentThinkingOutput::failed(error.to_string()),
                )?;
                return Err(error);
            }
        };

        let (output, keep_say_reservation) = match validate_and_persist_model_output(
            thought_store,
            &mut thought_context,
            &instance.mode_hint,
            &llm_resp.content,
            instance.keep_say_quota.as_ref(),
            "user",
        )? {
            ValidatedAgentOutput::Valid {
                output,
                keep_say_reservation,
            } => (output, keep_say_reservation),
            ValidatedAgentOutput::Invalid(failure) => {
                return Err(thinking_failure_error(&failure));
            }
        };
        persist_completed_agent_output(
            thought_store,
            &mut thought_context,
            &output,
            None,
            keep_say_reservation,
        )?;

        Ok(output)
    }

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
        use crate::logic::model::api_key::resolve_api_key;
        use crate::logic::model::provider::LlmRequest;

        let mut instance = self.create_from_mode_with_keep_say_quota(
            mode_hint,
            mode_hint.to_string(),
            input.to_string(),
        )?;
        let turn_id = instance.id.clone();
        let mut thought_context = instance.thought_context();
        thought_store.persist_input(&thought_context)?;

        let (base_system, base_messages) = assembler.build_messages(input, mode_hint).await;
        let self_awareness = assembler.build_self_awareness().await;
        let system_prompt = if self_awareness.is_empty() {
            base_system
        } else {
            format!("{base_system}\n\n{self_awareness}")
        };
        let api_key = resolve_api_key(model_row)?;
        let provider_kind = model_row.api_type.to_lowercase();
        let provider = registry.pick_by_kind(&provider_kind).ok_or_else(|| {
            AgentError::Llm(format!(
                "run_with_dm_in_period: no provider for kind '{provider_kind}' (api_type={})",
                model_row.api_type
            ))
        })?;

        let llm_resp = match instance.run(model_row, registry, assembler).await {
            Ok(response) => response,
            Err(error) => {
                persist_terminal_output(
                    thought_store,
                    &mut thought_context,
                    PersistentThinkingOutput::failed(error.to_string()),
                )?;
                return Err(error);
            }
        };

        let max_retries = 3u32;
        let mut retries = 0u32;
        let mut retry_messages = base_messages;
        let mut current_raw = llm_resp.content;

        loop {
            match validate_output(
                &current_raw,
                &instance.mode_hint,
                instance.keep_say_quota.as_ref(),
                "user",
            ) {
                Ok(ValidatedAgentOutput::Valid {
                    output,
                    keep_say_reservation,
                }) => {
                    persist_completed_agent_output(
                        thought_store,
                        &mut thought_context,
                        &output,
                        output
                            .think
                            .clone()
                            .map(|intent| DownstreamRequest::Execute { intent }),
                        keep_say_reservation,
                    )?;

                    if let Some(think) = &output.think {
                        let ctx = TurnContext {
                            turn_id: turn_id.clone(),
                            thinking: ThinkingOutput {
                                decision: ThinkDecision::Execute,
                                goal: think.clone(),
                                constraints: vec![],
                                message: think.clone(),
                            },
                            execution: None,
                            insight: None,
                            memory: None,
                            status: TurnStatus::Executing,
                            user_message: input.to_string(),
                            input_kind: "user".into(),
                            say_published: false,
                        };

                        pool.create_turn_context(ctx).await;
                        if let Err(send_err) = pool.send_execute(&turn_id).await {
                            tracing::warn!(
                                "spawn_streaming: send_execute failed turn_id={}: {}",
                                turn_id,
                                send_err
                            );
                        }

                        tracing::debug!(
                            "ThinkingFactory::run_with_dm: turn_id={turn_id}, think routed to Execute DM"
                        );
                    } else {
                        tracing::debug!(
                            "ThinkingFactory::run_with_dm: turn_id={turn_id}, say-only output, \
                             routing to insight+memory chain (no execution)"
                        );
                        let ctx = TurnContext {
                            turn_id: turn_id.clone(),
                            thinking: ThinkingOutput {
                                decision: ThinkDecision::Reply,
                                goal: output.say.clone().unwrap_or_default(),
                                constraints: vec![],
                                message: output.say.clone().unwrap_or_default(),
                            },
                            execution: None,
                            insight: None,
                            memory: None,
                            status: TurnStatus::Insighting,
                            user_message: input.to_string(),
                            input_kind: "user".into(),
                            say_published: false,
                        };
                        pool.create_turn_context(ctx).await;
                        if let Err(send_err) = pool.send_execution_done(&turn_id).await {
                            tracing::warn!(
                                "spawn_streaming: say-only send_execution_done (insight) failed turn_id={}: {}",
                                turn_id,
                                send_err
                            );
                        }
                    }

                    return Ok(output);
                }
                Err(errors) if is_retryable_error(&errors) && retries < max_retries => {
                    retries += 1;
                    tracing::warn!(
                        "run_with_dm_in_period: output validation failed (retry {retries}/{max_retries}): {:?}",
                        errors
                    );

                    let feedback = format!(
                        "上一轮输出不符合格式要求：{}。请修正后重新输出。",
                        errors
                            .iter()
                            .map(|e| format!("{}: {}", e.code, e.message))
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                    retry_messages.push(ChatMessage::System {
                        text: feedback,
                        kind: SystemKind::Meta,
                    });

                    let mut req = LlmRequest::from_model_row(
                        model_row,
                        retry_messages.clone(),
                        api_key.clone(),
                    )?;
                    req.system = Some(system_prompt.clone());
                    match provider.call(&req).await {
                        Ok(resp) => {
                            current_raw = resp.content;
                            continue;
                        }
                        Err(call_error) => {
                            persist_terminal_output(
                                thought_store,
                                &mut thought_context,
                                PersistentThinkingOutput::failed(call_error.to_string()),
                            )?;
                            return Err(call_error);
                        }
                    }
                }
                Err(errors) => {
                    tracing::warn!(
                        "run_with_dm_in_period: output validation failed (final): {:?}",
                        errors
                    );
                    let failure = match persist_invalid_model_output(
                        thought_store,
                        &mut thought_context,
                        &instance.mode_hint,
                        &current_raw,
                        errors,
                    )? {
                        ValidatedAgentOutput::Invalid(f) => f,
                        _ => unreachable!("persist_invalid_model_output always returns Invalid"),
                    };
                    enqueue_thinking_failure(pool, &failure, input).await?;
                    return Err(thinking_failure_error(&failure));
                }
                Ok(ValidatedAgentOutput::Invalid(_)) => {
                    unreachable!("validate_output never returns Ok(Invalid)")
                }
            }
        }
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
    fn validate_output_strips_loop_say_and_keeps_think() {
        let validated = validate_output(
            r#"{"think":"advance goal","say":"I will do it"}"#,
            "loop",
            None,
            "user",
        )
        .expect("LOOP output with say is mechanically stripped, not rejected");
        match validated {
            ValidatedAgentOutput::Valid {
                output,
                keep_say_reservation,
            } => {
                assert_eq!(output.think.as_deref(), Some("advance goal"));
                assert!(output.say.is_none(), "say must be stripped in LOOP");
                assert!(keep_say_reservation.is_none());
            }
            ValidatedAgentOutput::Invalid(_) => panic!("stripped output must stay valid"),
        }
    }

    #[test]
    fn validate_output_loop_say_only_still_requires_think() {
        let result = validate_output(r#"{"say":"only a say"}"#, "loop", None, "user");
        match result {
            Ok(ValidatedAgentOutput::Valid { .. }) => panic!("say-only LOOP output must fail"),
            Ok(ValidatedAgentOutput::Invalid(_)) => {
                panic!("validation errors must be returned, not a failure input")
            }
            Err(errors) => {
                assert!(
                    errors.iter().any(|e| e.code == "loop_requires_think"),
                    "got: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn validate_output_keep_strips_say_after_quota_exhausted() {
        let quota = Arc::new(KeepSayQuota::new());
        let reservation = quota.try_reserve().expect("first say reserves");
        reservation.commit();
        assert!(quota.is_consumed());

        let validated = validate_output(
            r#"{"think":"count dependencies","say":"I will report again"}"#,
            "keep",
            Some(&quota),
            "echo",
        )
        .expect("KEEP exhausted output with say must be stripped, not rejected");
        match validated {
            ValidatedAgentOutput::Valid {
                output,
                keep_say_reservation,
            } => {
                assert_eq!(output.think.as_deref(), Some("count dependencies"));
                assert!(
                    output.say.is_none(),
                    "say must be stripped once KEEP quota is exhausted"
                );
                assert!(keep_say_reservation.is_none());
            }
            ValidatedAgentOutput::Invalid(_) => panic!("stripped output must stay valid"),
        }
    }

    #[test]
    fn validate_output_keep_say_only_after_exhaustion_still_requires_think() {
        let quota = Arc::new(KeepSayQuota::new());
        let reservation = quota.try_reserve().expect("first say reserves");
        reservation.commit();

        let result = validate_output(r#"{"say":"only say"}"#, "keep", Some(&quota), "echo");
        match result {
            Ok(ValidatedAgentOutput::Valid { .. }) => panic!("say-only KEEP output must fail"),
            Err(errors) => {
                assert!(
                    errors.iter().any(|e| e.code == "keep_requires_think"),
                    "got: {errors:?}"
                );
            }
            Ok(ValidatedAgentOutput::Invalid(_)) => {
                panic!("validation errors must be returned, not a failure input")
            }
        }
    }

    #[test]
    fn validate_output_keep_reserves_first_say_normally() {
        let quota = Arc::new(KeepSayQuota::new());
        let validated = validate_output(
            r#"{"think":"work","say":"final report"}"#,
            "keep",
            Some(&quota),
            "echo",
        )
        .expect("first KEEP say must be accepted");
        match validated {
            ValidatedAgentOutput::Valid {
                output,
                keep_say_reservation,
            } => {
                assert_eq!(output.say.as_deref(), Some("final report"));
                assert!(keep_say_reservation.is_some());
                drop(keep_say_reservation);
            }
            ValidatedAgentOutput::Invalid(_) => panic!("first KEEP say must stay valid"),
        }
    }

    #[test]
    fn validate_output_keep_user_round_say_does_not_consume_quota() {
        let quota = Arc::new(KeepSayQuota::new());
        let first = validate_output(
            r#"{"think":"work","say":"我先开始执行"}"#,
            "keep",
            Some(&quota),
            "user",
        )
        .expect("user 轮首个 say 必须接受");
        match first {
            ValidatedAgentOutput::Valid {
                output,
                keep_say_reservation,
            } => {
                assert_eq!(output.say.as_deref(), Some("我先开始执行"));
                assert!(keep_say_reservation.is_none(), "user 轮不 reserve 配额");
            }
            ValidatedAgentOutput::Invalid(_) => panic!("user 轮 say 必须 valid"),
        }
        assert!(!quota.is_consumed(), "user 轮 say 不应消耗 KEEP 配额");

        let echo_round = validate_output(
            r#"{"think":"work","say":"任务完成：Top5 已写入"}"#,
            "keep",
            Some(&quota),
            "echo",
        )
        .expect("echo 轮首个 say 必须接受");
        let echo_reservation = match echo_round {
            ValidatedAgentOutput::Valid {
                output,
                keep_say_reservation,
            } => {
                assert_eq!(output.say.as_deref(), Some("任务完成：Top5 已写入"));
                assert!(keep_say_reservation.is_some(), "echo 轮应 reserve");
                keep_say_reservation
            }
            ValidatedAgentOutput::Invalid(_) => panic!("echo 轮 say 必须 valid"),
        };

        if let Some(r) = echo_reservation {
            r.commit();
        }
        assert!(quota.is_consumed(), "echo 轮 say 应消耗配额");

        let echo_again = validate_output(
            r#"{"think":"more","say":"再次汇报"}"#,
            "keep",
            Some(&quota),
            "echo",
        )
        .expect("echo 轮超配额 say 应机械剥离而非拒绝");
        match echo_again {
            ValidatedAgentOutput::Valid {
                output,
                keep_say_reservation,
            } => {
                assert!(output.say.is_none(), "echo 轮超配额 say 应剥离");
                assert!(keep_say_reservation.is_none());
            }
            ValidatedAgentOutput::Invalid(_) => panic!("剥离不应是 Invalid"),
        }
        let user_again = validate_output(
            r#"{"think":"more","say":"继续推进"}"#,
            "keep",
            Some(&quota),
            "user",
        )
        .expect("user 轮 say 不受配额影响");
        match user_again {
            ValidatedAgentOutput::Valid { output, .. } => {
                assert_eq!(output.say.as_deref(), Some("继续推进"), "user 轮不剥离");
            }
            ValidatedAgentOutput::Invalid(_) => panic!("user 轮 say 必须 valid"),
        }
    }

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
        assert!(k.keep_say_quota.is_some());
        let l = f
            .create_from_mode("LOOP", "loop".into(), "task B".into())
            .unwrap();
        assert_eq!(l.mode_kind, Some(ThinkingModeKind::Loop));
        assert_eq!(l.mode_hint, "loop");
        assert!(l.keep_say_quota.is_none());

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
        assert!(instance.keep_say_quota.is_some());
    }

    #[test]
    fn factory_keep_say_quota_consumed_tracks_period() {
        let mut factory = ThinkingFactory::new();

        assert!(!factory.keep_say_quota_consumed());

        factory.reset_keep_quota();
        assert!(!factory.keep_say_quota_consumed());

        let instance = factory
            .create_from_mode("KEEP", "keep".into(), "report once".into())
            .unwrap();
        let quota = instance.keep_say_quota.clone().expect("KEEP has quota");
        let reservation = quota.try_reserve().expect("first say reserves");
        reservation.commit();
        assert!(
            factory.keep_say_quota_consumed(),
            "say emitted in KEEP ⇒ quota consumed ⇒ no new instance"
        );

        factory.clear_keep_quota();
        assert!(!factory.keep_say_quota_consumed());
    }

    #[test]
    fn keep_period_finishes_only_when_say_consumed() {
        let mut factory = ThinkingFactory::new();
        factory.reset_keep_quota();
        assert!(!factory.keep_period_finished());

        let quota = factory
            .create_from_mode("KEEP", "keep".into(), "x".into())
            .unwrap()
            .keep_say_quota
            .expect("KEEP quota");

        for _ in 0..10 {
            let result =
                validate_output(r#"{"think":"keep working"}"#, "keep", Some(&quota), "echo");
            assert!(result.is_ok(), "think-only KEEP output stays valid");
            assert!(
                !factory.keep_period_finished(),
                "think-only 推进不结束周期 (D2)"
            );
        }

        let reservation = quota.try_reserve().expect("say reserves");
        reservation.commit();
        assert!(
            factory.keep_period_finished(),
            "say consumed finishes the period"
        );
    }

    #[test]
    fn keep_say_quota_has_exactly_one_atomic_winner() {
        const CONTENDERS: usize = 100;
        let quota = Arc::new(KeepSayQuota::new());
        let barrier = Arc::new(std::sync::Barrier::new(CONTENDERS));
        let handles = (0..CONTENDERS)
            .map(|_| {
                let quota = Arc::clone(&quota);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    match quota.try_reserve() {
                        Some(reservation) => {
                            reservation.commit();
                            true
                        }
                        None => false,
                    }
                })
            })
            .collect::<Vec<_>>();

        let winners = handles
            .into_iter()
            .map(|handle| handle.join().expect("quota contender should not panic"))
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert!(quota.is_consumed());
    }

    #[test]
    fn uncommitted_keep_say_reservation_rolls_back() {
        let quota = Arc::new(KeepSayQuota::new());
        let reservation = quota.try_reserve().expect("first claim should reserve");
        assert!(quota.is_consumed());
        drop(reservation);
        assert!(!quota.is_consumed());
        assert!(quota.try_reserve().is_some());
    }

    #[test]
    fn completed_output_persistence_failure_releases_keep_say_quota() {
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
        let quota = Arc::new(KeepSayQuota::new());
        let reservation = quota.try_reserve().expect("quota should reserve");

        let result = persist_completed_agent_output(
            &thought_store,
            &mut context,
            &output,
            None,
            Some(reservation),
        );

        assert!(result.is_err());
        assert!(!quota.is_consumed());
    }

    #[test]
    fn thinking_instance_freezes_keep_period_quota() {
        let mut factory = ThinkingFactory::new();
        factory.reset_keep_quota();
        let first_period = Arc::clone(factory.current_keep_quota().unwrap());
        let instance = factory
            .create_from_mode_with_keep_say_quota(
                "KEEP",
                "keep".to_string(),
                "in flight".to_string(),
            )
            .unwrap();
        let next_period = Arc::new(KeepSayQuota::new());

        assert!(Arc::ptr_eq(
            instance.keep_say_quota.as_ref().unwrap(),
            &first_period
        ));
        assert!(!Arc::ptr_eq(
            instance.keep_say_quota.as_ref().unwrap(),
            &next_period
        ));
    }
}

use crate::agent::output::OutputValidationError;
use crate::common::{AgentError, UtcTimestamp};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const THOUGHT_RECORD_SCHEMA_VERSION: u32 = 2;
const LEGACY_THOUGHT_RECORD_SCHEMA_VERSION: u32 = 1;
pub const RAW_MODEL_OUTPUT_FILE_NAME: &str = "raw_model_output.txt";

pub use crate::common::types::ThoughtId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThinkingInput {
    User {
        text: String,
    },
    /// 洞察回环轮输入：insight_complete 触发思考引擎下一轮时注入。
    /// `summary` 为洞察输出段原文；`has_subagent_result` 表示该轮洞察输入是否含
    /// subagent 结果段（依据 AgentPool subagent 状态变化，中间/最终结果均计），
    /// 供 UNNI 动态执行权判定（含结果 → 回环轮无执行权，等用户下一次输入）。
    PlatformInsight {
        summary: String,
        has_subagent_result: bool,
    },
    ModeTrigger {
        mode: String,
        reason: String,
    },
    CapabilityResult {
        capability_id: String,
        capability_name: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        artifact_refs: Vec<String>,
    },
    /// 旧版本落盘的内部轮（echo/reflect）兼容兜底：只保证可反序列化，不产生任何新语义。
    #[serde(other)]
    LegacyInternal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownstreamRequest {
    Execute { intent: String },
    Recall { query: String },
    Cancel { reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThinkingTerminalState {
    Completed,
    Failed { error: String },
    Cancelled { reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingFailureInput {
    pub schema_version: u32,
    pub failure_event_id: ThoughtId,
    pub failed_thought_id: ThoughtId,
    pub occurred_at: UtcTimestamp,
    pub mode_snapshot: String,
    pub raw_model_output_ref: String,
    pub raw_model_output_sha256: String,
    pub raw_model_output_bytes: u64,
    pub validation_errors: Vec<OutputValidationError>,
}

impl ThinkingFailureInput {
    pub fn new(
        failed_thought_id: ThoughtId,
        occurred_at: UtcTimestamp,
        mode_snapshot: impl Into<String>,
        raw_model_output: impl Into<String>,
        validation_errors: Vec<OutputValidationError>,
    ) -> Result<Self, AgentError> {
        if validation_errors.is_empty() {
            return Err(AgentError::Parse(
                "ThinkingFailureInput requires at least one validation error".to_string(),
            ));
        }

        let raw_model_output = raw_model_output.into();
        Ok(Self {
            schema_version: 1,
            failure_event_id: ThoughtId::new(),
            failed_thought_id,
            occurred_at,
            mode_snapshot: mode_snapshot.into(),
            raw_model_output_ref: RAW_MODEL_OUTPUT_FILE_NAME.to_string(),
            raw_model_output_sha256: sha256_hex(raw_model_output.as_bytes()),
            raw_model_output_bytes: raw_model_output.len() as u64,
            validation_errors,
        })
    }

    pub fn matches_raw_model_output(&self, bytes: &[u8]) -> bool {
        self.raw_model_output_ref == RAW_MODEL_OUTPUT_FILE_NAME
            && self.raw_model_output_bytes == bytes.len() as u64
            && self.raw_model_output_sha256 == sha256_hex(bytes)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub say: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downstream: Option<DownstreamRequest>,
    pub terminal_state: ThinkingTerminalState,
}

impl ThinkingOutput {
    pub fn completed(
        think: Option<String>,
        say: Option<String>,
        downstream: Option<DownstreamRequest>,
    ) -> Self {
        Self {
            think,
            say,
            downstream,
            terminal_state: ThinkingTerminalState::Completed,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            think: None,
            say: None,
            downstream: None,
            terminal_state: ThinkingTerminalState::Failed {
                error: error.into(),
            },
        }
    }

    pub fn cancelled(reason: Option<String>) -> Self {
        Self {
            think: None,
            say: None,
            downstream: None,
            terminal_state: ThinkingTerminalState::Cancelled { reason },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThoughtLifecycleState {
    Thinking,
    Execution,
    Insight,
    Memory,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThoughtContext {
    pub thought_id: ThoughtId,
    pub occurred_at: UtcTimestamp,
    pub input: ThinkingInput,
    pub output: Option<ThinkingOutput>,
    pub lifecycle_state: ThoughtLifecycleState,
}

impl ThoughtContext {
    pub fn new(input: ThinkingInput) -> Self {
        Self::new_at(ThoughtId::new(), UtcTimestamp::now(), input)
    }

    pub fn new_at(thought_id: ThoughtId, occurred_at: UtcTimestamp, input: ThinkingInput) -> Self {
        Self {
            thought_id,
            occurred_at,
            input,
            output: None,
            lifecycle_state: ThoughtLifecycleState::Thinking,
        }
    }

    pub fn set_output(&mut self, output: ThinkingOutput) {
        self.lifecycle_state = lifecycle_after_output(&output);
        self.output = Some(output);
    }

    pub fn mark_completed(&mut self) {
        self.lifecycle_state = ThoughtLifecycleState::Completed;
    }

    pub fn validate(&self) -> Result<(), AgentError> {
        match &self.output {
            None if self.lifecycle_state == ThoughtLifecycleState::Thinking => Ok(()),
            None => Err(AgentError::Parse(
                "thought without output must remain in thinking state".to_string(),
            )),
            Some(output) if self.lifecycle_state == lifecycle_after_output(output) => Ok(()),
            Some(_) => Err(AgentError::Parse(
                "thought lifecycle state is inconsistent with its thinking output".to_string(),
            )),
        }
    }
}

fn lifecycle_after_output(output: &ThinkingOutput) -> ThoughtLifecycleState {
    match &output.terminal_state {
        ThinkingTerminalState::Failed { .. } => ThoughtLifecycleState::Failed,
        ThinkingTerminalState::Cancelled { .. } => ThoughtLifecycleState::Cancelled,
        ThinkingTerminalState::Completed => match &output.downstream {
            Some(DownstreamRequest::Execute { .. }) => ThoughtLifecycleState::Execution,
            Some(DownstreamRequest::Recall { .. }) => ThoughtLifecycleState::Memory,
            Some(DownstreamRequest::Cancel { .. }) => ThoughtLifecycleState::Cancelled,
            None => ThoughtLifecycleState::Completed,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThoughtInputRecord {
    pub schema_version: u32,
    pub thought_id: ThoughtId,
    pub occurred_at: UtcTimestamp,
    pub input: ThinkingInput,
}

impl From<&ThoughtContext> for ThoughtInputRecord {
    fn from(context: &ThoughtContext) -> Self {
        Self {
            schema_version: THOUGHT_RECORD_SCHEMA_VERSION,
            thought_id: context.thought_id.clone(),
            occurred_at: context.occurred_at.clone(),
            input: context.input.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThoughtOutputRecord {
    pub schema_version: u32,
    pub thought_id: ThoughtId,
    pub occurred_at: UtcTimestamp,
    pub output: ThinkingOutput,
    pub lifecycle_state: ThoughtLifecycleState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThoughtOutputRecordWire {
    schema_version: u32,
    thought_id: ThoughtId,
    occurred_at: UtcTimestamp,
    output: serde_json::Value,
    lifecycle_state: ThoughtLifecycleState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyThinkingOutput {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    downstream: Option<DownstreamRequest>,
    terminal_state: ThinkingTerminalState,
}

impl From<LegacyThinkingOutput> for ThinkingOutput {
    fn from(legacy: LegacyThinkingOutput) -> Self {
        let think = match &legacy.downstream {
            Some(DownstreamRequest::Execute { intent }) => Some(intent.clone()),
            _ => None,
        };
        let say = [legacy.message, legacy.request]
            .into_iter()
            .flatten()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>();

        Self {
            think,
            say: (!say.is_empty()).then(|| say.join("\n")),
            downstream: legacy.downstream,
            terminal_state: legacy.terminal_state,
        }
    }
}

impl<'de> Deserialize<'de> for ThoughtOutputRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ThoughtOutputRecordWire::deserialize(deserializer)?;
        let output = match wire.schema_version {
            LEGACY_THOUGHT_RECORD_SCHEMA_VERSION => {
                serde_json::from_value::<LegacyThinkingOutput>(wire.output)
                    .map(ThinkingOutput::from)
                    .map_err(serde::de::Error::custom)?
            }
            THOUGHT_RECORD_SCHEMA_VERSION => serde_json::from_value::<ThinkingOutput>(wire.output)
                .map_err(serde::de::Error::custom)?,
            version => {
                return Err(serde::de::Error::custom(format!(
                    "unsupported thought output schema version {version}"
                )))
            }
        };

        Ok(Self {
            schema_version: wire.schema_version,
            thought_id: wire.thought_id,
            occurred_at: wire.occurred_at,
            output,
            lifecycle_state: wire.lifecycle_state,
        })
    }
}

impl ThoughtOutputRecord {
    pub fn from_context(context: &ThoughtContext) -> Result<Self, AgentError> {
        context.validate()?;
        let output = context.output.clone().ok_or_else(|| {
            AgentError::Parse("cannot persist thought output before it exists".to_string())
        })?;

        Ok(Self {
            schema_version: THOUGHT_RECORD_SCHEMA_VERSION,
            thought_id: context.thought_id.clone(),
            occurred_at: context.occurred_at.clone(),
            output,
            lifecycle_state: context.lifecycle_state,
        })
    }
}

pub fn context_from_records(
    input: ThoughtInputRecord,
    output: Option<ThoughtOutputRecord>,
) -> Result<ThoughtContext, AgentError> {
    if !matches!(
        input.schema_version,
        LEGACY_THOUGHT_RECORD_SCHEMA_VERSION | THOUGHT_RECORD_SCHEMA_VERSION
    ) {
        return Err(AgentError::Parse(format!(
            "unsupported thought input schema version {}",
            input.schema_version
        )));
    }

    let mut context = ThoughtContext::new_at(input.thought_id, input.occurred_at, input.input);

    if let Some(output) = output {
        if !matches!(
            output.schema_version,
            LEGACY_THOUGHT_RECORD_SCHEMA_VERSION | THOUGHT_RECORD_SCHEMA_VERSION
        ) {
            return Err(AgentError::Parse(format!(
                "unsupported thought output schema version {}",
                output.schema_version
            )));
        }
        if output.thought_id != context.thought_id || output.occurred_at != context.occurred_at {
            return Err(AgentError::Parse(
                "thought input/output records do not identify the same thought".to_string(),
            ));
        }

        context.set_output(output.output);
        if context.lifecycle_state != output.lifecycle_state {
            return Err(AgentError::Parse(
                "thought output record has an inconsistent lifecycle state".to_string(),
            ));
        }
    }

    Ok(context)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThoughtTimestampGroup {
    pub occurred_at: UtcTimestamp,
    pub contexts: Vec<ThoughtContext>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThoughtTimeline {
    pub groups: Vec<ThoughtTimestampGroup>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThoughtCursor {
    pub processed_through: Option<UtcTimestamp>,
    #[serde(default)]
    pub boundary_thought_ids: BTreeSet<ThoughtId>,
}

impl ThoughtCursor {
    pub fn contains(&self, thought_id: &ThoughtId, occurred_at: &UtcTimestamp) -> bool {
        match &self.processed_through {
            None => false,
            Some(watermark) if occurred_at < watermark => true,
            Some(watermark) if occurred_at > watermark => false,
            Some(_) => self.boundary_thought_ids.contains(thought_id),
        }
    }

    pub fn advance(
        &mut self,
        occurred_at: UtcTimestamp,
        thought_ids: impl IntoIterator<Item = ThoughtId>,
    ) -> Result<(), AgentError> {
        let ids: BTreeSet<_> = thought_ids.into_iter().collect();
        if ids.is_empty() {
            return Err(AgentError::Parse(
                "thought cursor cannot advance with an empty timestamp set".to_string(),
            ));
        }

        match &self.processed_through {
            None => {
                self.processed_through = Some(occurred_at);
                self.boundary_thought_ids = ids;
            }
            Some(watermark) if &occurred_at < watermark => {
                return Err(AgentError::Parse(
                    "thought cursor cannot move its timestamp watermark backward".to_string(),
                ));
            }
            Some(watermark) if &occurred_at == watermark => {
                self.boundary_thought_ids.extend(ids);
            }
            Some(_) => {
                self.processed_through = Some(occurred_at);
                self.boundary_thought_ids = ids;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp() -> UtcTimestamp {
        UtcTimestamp::parse("2026-07-15T12:34:56.123456789Z").unwrap()
    }

    fn id(value: &str) -> ThoughtId {
        ThoughtId::parse(value).unwrap()
    }

    #[test]
    fn thought_id_serializes_as_a_canonical_uuid() {
        let thought_id = id("ca761232-ed42-11ce-bacd-00aa0057b223");
        assert_eq!(
            thought_id.to_string(),
            "ca761232-ed42-11ce-bacd-00aa0057b223"
        );
        assert!(ThoughtId::parse("ca761232ed4211cebacd00aa0057b223").is_err());
    }

    #[test]
    fn persistent_output_decodes_v1_semantics_and_rewrites_v2_fields() {
        let legacy = r#"{
            "schema_version":1,
            "thought_id":"ca761232-ed42-11ce-bacd-00aa0057b223",
            "occurred_at":"2026-07-15T12:34:56.123456789Z",
            "output":{
                "message":"legacy visible progress",
                "request":"legacy visible question",
                "downstream":{"kind":"execute","intent":"legacy execution intent"},
                "terminal_state":{"kind":"completed"}
            },
            "lifecycle_state":"execution"
        }"#;
        let legacy_output: ThoughtOutputRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            legacy_output.output.think.as_deref(),
            Some("legacy execution intent")
        );
        assert_eq!(
            legacy_output.output.say.as_deref(),
            Some("legacy visible progress\nlegacy visible question")
        );

        let input = ThoughtInputRecord {
            schema_version: LEGACY_THOUGHT_RECORD_SCHEMA_VERSION,
            thought_id: id("ca761232-ed42-11ce-bacd-00aa0057b223"),
            occurred_at: timestamp(),
            input: ThinkingInput::User {
                text: "legacy user input".to_string(),
            },
        };
        let context = context_from_records(input, Some(legacy_output)).unwrap();
        let current = ThoughtOutputRecord::from_context(&context).unwrap();
        let current_json = serde_json::to_value(current).unwrap();

        assert_eq!(
            current_json["schema_version"],
            THOUGHT_RECORD_SCHEMA_VERSION
        );
        assert_eq!(current_json["output"]["think"], "legacy execution intent");
        assert_eq!(
            current_json["output"]["say"],
            "legacy visible progress\nlegacy visible question"
        );
        assert!(current_json["output"].get("message").is_none());
        assert!(current_json["output"].get("request").is_none());
    }

    #[test]
    fn current_output_rejects_legacy_field_names() {
        let invalid_current = r#"{
            "schema_version":2,
            "thought_id":"ca761232-ed42-11ce-bacd-00aa0057b223",
            "occurred_at":"2026-07-15T12:34:56.123456789Z",
            "output":{
                "message":"legacy field in current schema",
                "terminal_state":{"kind":"completed"}
            },
            "lifecycle_state":"completed"
        }"#;

        assert!(serde_json::from_str::<ThoughtOutputRecord>(invalid_current).is_err());
    }

    #[test]
    fn output_derives_execution_lifecycle_without_turn_semantics() {
        let mut context = ThoughtContext::new_at(
            id("ca761232-ed42-11ce-bacd-00aa0057b223"),
            timestamp(),
            ThinkingInput::User {
                text: "summarize this".to_string(),
            },
        );
        context.set_output(ThinkingOutput::completed(
            Some("I will inspect it.".to_string()),
            None,
            Some(DownstreamRequest::Execute {
                intent: "inspect the supplied content".to_string(),
            }),
        ));

        assert_eq!(context.lifecycle_state, ThoughtLifecycleState::Execution);
        assert!(context.validate().is_ok());
    }

    #[test]
    fn cursor_keeps_late_id_at_same_timestamp_pending() {
        let first = id("ca761232-ed42-11ce-bacd-00aa0057b223");
        let late = id("ca761233-ed42-11ce-bacd-00aa0057b223");
        let mut cursor = ThoughtCursor::default();
        cursor.advance(timestamp(), [first.clone()]).unwrap();

        assert!(cursor.contains(&first, &timestamp()));
        assert!(
            !cursor.contains(&late, &timestamp()),
            "a late ID at the watermark must not be skipped"
        );

        cursor.advance(timestamp(), [late.clone()]).unwrap();
        assert!(cursor.contains(&late, &timestamp()));
    }

    #[test]
    fn record_recovery_rejects_mismatched_ids() {
        let context = ThoughtContext::new_at(
            id("ca761232-ed42-11ce-bacd-00aa0057b223"),
            timestamp(),
            ThinkingInput::User {
                text: "hello".to_string(),
            },
        );
        let input = ThoughtInputRecord::from(&context);
        let output = ThoughtOutputRecord {
            schema_version: THOUGHT_RECORD_SCHEMA_VERSION,
            thought_id: id("ca761233-ed42-11ce-bacd-00aa0057b223"),
            occurred_at: timestamp(),
            output: ThinkingOutput::failed("model unavailable"),
            lifecycle_state: ThoughtLifecycleState::Failed,
        };

        assert!(context_from_records(input, Some(output)).is_err());
    }
}

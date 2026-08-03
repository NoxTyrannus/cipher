use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOutput {
    #[serde(default)]
    pub think: Option<String>,
    #[serde(default)]
    pub say: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputValidationError {
    pub code: String,
    pub message: String,
}

impl OutputValidationError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl fmt::Display for OutputValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

pub fn parse_agent_output(
    content: &str,
) -> std::result::Result<AgentOutput, Vec<OutputValidationError>> {
    let mut output = serde_json::from_str::<AgentOutput>(content.trim()).map_err(|error| {
        let message = match error.classify() {
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => format!(
                "output is not valid JSON at line {} column {}",
                error.line(),
                error.column()
            ),
            serde_json::error::Category::Data => format!(
                "output must be an object containing only string-or-null think/say fields at line {} column {}",
                error.line(),
                error.column()
            ),
            serde_json::error::Category::Io => "output JSON could not be read".to_string(),
        };
        vec![OutputValidationError::new("invalid_json_output", message)]
    })?;
    output.think = normalize_optional_text(output.think);
    output.say = normalize_optional_text(output.say);
    Ok(output)
}

pub fn validate_agent_output(
    output: &AgentOutput,
    mode: &str,
) -> std::result::Result<(), Vec<OutputValidationError>> {
    let has_think = output.think.is_some();
    let has_say = output.say.is_some();
    let mut errors = Vec::new();

    match mode.to_ascii_lowercase().as_str() {
        "unni" => {
            if !has_think && !has_say {
                errors.push(OutputValidationError::new(
                    "unni_requires_output",
                    "UNNI requires at least one non-empty think or say field",
                ));
            }
        }
        "keep" => {
            if !has_think {
                errors.push(OutputValidationError::new(
                    "keep_requires_think",
                    "KEEP requires a non-empty think field",
                ));
            }
        }
        "loop" => {
            if !has_think {
                errors.push(OutputValidationError::new(
                    "loop_requires_think",
                    "LOOP requires a non-empty think field",
                ));
            }
            if has_say {
                errors.push(OutputValidationError::new(
                    "loop_forbids_say",
                    "LOOP forbids say",
                ));
            }
        }
        _ => errors.push(OutputValidationError::new(
            "unknown_mode",
            format!("unknown Thinking mode snapshot '{mode}'"),
        )),
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|text| (!text.trim().is_empty()).then_some(text))
}

pub fn strip_loop_say(output: &mut AgentOutput, mode: &str) -> bool {
    if mode.eq_ignore_ascii_case("loop") {
        if let Some(dropped) = output.say.take() {
            tracing::warn!(
                say_len = dropped.len(),
                "LOOP forbids say: say mechanically stripped (quota = 0)"
            );
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> AgentOutput {
        parse_agent_output(json).expect("output should parse")
    }

    #[test]
    fn parses_think_and_say_without_legacy_fields() {
        let output = parse(r#"{"think":"inspect logs","say":"I am checking."}"#);
        assert_eq!(output.think.as_deref(), Some("inspect logs"));
        assert_eq!(output.say.as_deref(), Some("I am checking."));
    }

    #[test]
    fn rejects_plain_text_fenced_json_and_unknown_fields() {
        assert!(parse_agent_output("").is_err());
        assert!(parse_agent_output("plain text").is_err());
        assert!(parse_agent_output("```json\n{\"think\":\"x\"}\n```").is_err());
        assert!(parse_agent_output(r#"{"think":"x","message":"legacy"}"#).is_err());
        assert!(parse_agent_output(r#"{"think":42}"#).is_err());
    }

    #[test]
    fn parse_errors_do_not_echo_untrusted_output() {
        let secret_marker = "DO_NOT_LOG_THIS_FIELD";
        let errors =
            parse_agent_output(&format!(r#"{{"{secret_marker}":"secret value"}}"#)).unwrap_err();
        let rendered = validation_error_text(&errors);
        assert!(!rendered.contains(secret_marker));
        assert!(!rendered.contains("secret value"));
    }

    #[test]
    fn normalizes_empty_fields_to_missing() {
        let output = parse(r#"{"think":"  ","say":""}"#);
        assert!(output.think.is_none());
        assert!(output.say.is_none());
    }

    #[test]
    fn validates_unni_matrix() {
        assert!(validate_agent_output(&parse(r#"{"think":"work"}"#), "unni").is_ok());
        assert!(validate_agent_output(&parse(r#"{"say":"reply"}"#), "UNNI").is_ok());
        assert!(validate_agent_output(&parse(r#"{"think":"work","say":"reply"}"#), "unni").is_ok());
        assert!(validate_agent_output(&parse("{}"), "unni").is_err());
    }

    #[test]
    fn validates_keep_matrix() {
        assert!(validate_agent_output(&parse(r#"{"think":"work"}"#), "keep").is_ok());
        assert!(validate_agent_output(&parse(r#"{"think":"work","say":"reply"}"#), "keep").is_ok());
        assert!(validate_agent_output(&parse(r#"{"say":"reply"}"#), "keep").is_err());
    }

    #[test]
    fn validates_loop_matrix() {
        assert!(validate_agent_output(&parse(r#"{"think":"work"}"#), "loop").is_ok());
        assert!(
            validate_agent_output(&parse(r#"{"think":"work","say":"reply"}"#), "loop").is_err()
        );
        assert!(validate_agent_output(&parse(r#"{"say":"reply"}"#), "loop").is_err());
    }

    #[test]
    fn strip_loop_say_drops_say_keeps_think() {
        let mut output = parse(r#"{"think":"work","say":"reply"}"#);
        assert!(strip_loop_say(&mut output, "loop"));
        assert_eq!(output.think.as_deref(), Some("work"));
        assert!(output.say.is_none());
    }

    #[test]
    fn strip_loop_say_case_insensitive() {
        let mut output = parse(r#"{"think":"work","say":"reply"}"#);
        assert!(strip_loop_say(&mut output, "LOOP"));
        assert!(output.say.is_none());
    }

    #[test]
    fn strip_loop_say_noop_without_say() {
        let mut output = parse(r#"{"think":"work"}"#);
        assert!(!strip_loop_say(&mut output, "loop"));
        assert_eq!(output.think.as_deref(), Some("work"));
    }

    #[test]
    fn strip_loop_say_noop_outside_loop() {
        for mode in ["unni", "KEEP"] {
            let mut output = parse(r#"{"think":"work","say":"reply"}"#);
            assert!(!strip_loop_say(&mut output, mode));
            assert!(output.say.is_some());
        }
    }

    #[test]
    fn stripped_loop_output_passes_loop_validation() {
        let mut output = parse(r#"{"think":"work","say":"reply"}"#);
        strip_loop_say(&mut output, "loop");
        assert!(validate_agent_output(&output, "loop").is_ok());
    }

    #[test]
    fn stripped_loop_say_only_still_fails_requires_think() {
        let mut output = parse(r#"{"say":"reply"}"#);
        strip_loop_say(&mut output, "loop");
        assert!(output.say.is_none());
        let errors = validate_agent_output(&output, "loop").unwrap_err();
        assert!(errors.iter().any(|e| e.code == "loop_requires_think"));
    }

    fn validation_error_text(errors: &[OutputValidationError]) -> String {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

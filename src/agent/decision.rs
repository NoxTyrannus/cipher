use crate::logic::model::provider::ToolCall;

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: String,

    pub tool_call: Option<ToolCall>,
}

impl Decision {
    pub fn call_capability(call: ToolCall) -> Self {
        Self {
            action: "call_capability".to_string(),
            tool_call: Some(call),
        }
    }

    pub fn respond() -> Self {
        Self {
            action: "respond".to_string(),
            tool_call: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_call_capability_constructs() {
        let tc = ToolCall {
            id: "1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({}),
        };
        let d = Decision::call_capability(tc.clone());
        assert_eq!(d.action, "call_capability");
        assert_eq!(d.tool_call, Some(tc));
    }

    #[test]
    fn decision_respond_has_no_tool_call() {
        let d = Decision::respond();
        assert_eq!(d.action, "respond");
        assert_eq!(d.tool_call, None);
    }
}

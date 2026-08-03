use crate::common::AgentError;

pub fn map_reqwest_error(e: reqwest::Error, provider: &str) -> AgentError {
    if e.is_timeout() {
        AgentError::Timeout(format!("{provider} LLM call timed out"))
    } else if e.is_status() {
        let status = e.status().unwrap();
        let url = e.url().map(|u| u.as_str()).unwrap_or("?");
        AgentError::Llm(format!("{provider} HTTP {status} at {url}"))
    } else {
        AgentError::Io(format!("{provider} LLM transport: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_function_signature_is_stable() {
        let _f: fn(reqwest::Error, &str) -> AgentError = map_reqwest_error;
    }
}

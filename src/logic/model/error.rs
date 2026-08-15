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

/// 从 LLM 错误消息中提取 HTTP 状态码。
///
/// 各 provider 统一把状态码嵌入消息（如 `HTTP 429 at ...` / `anthropic HTTP 429: ...`），
/// 这里做一次宽松提取；提取不到视为“非 HTTP 错误”（解析/参数类，不可重试）。
pub fn extract_http_status(msg: &str) -> Option<u16> {
    let bytes = msg.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        // 找 "HTTP" 关键字（不区分大小写）
        if (bytes[i] == b'h' || bytes[i] == b'H')
            && (bytes[i + 1] == b't' || bytes[i + 1] == b'T')
            && (bytes[i + 2] == b't' || bytes[i + 2] == b'T')
            && (bytes[i + 3] == b'p' || bytes[i + 3] == b'P')
        {
            let mut j = i + 4;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j + 3 <= bytes.len() && bytes[j..j + 3].iter().all(|b| b.is_ascii_digit()) {
                let code = bytes[j..j + 3]
                    .iter()
                    .fold(0u16, |acc, b| acc * 10 + u16::from(b - b'0'));
                return Some(code);
            }
        }
        i += 1;
    }
    None
}

/// 该 HTTP 状态码是否可重试（限流 / 网关超时 / 服务端错误）。
pub fn is_retryable_status(code: u16) -> bool {
    code == 429 || code == 408 || (500..=599).contains(&code)
}

/// LLM 类错误是否可重试。
///
/// - 超时 / 传输（网络）错误 → 可重试；
/// - 带 HTTP 状态码的 LLM 错误 → 按状态码判断（429/408/5xx 可重试；
///   401/403/404/400 等为永久错误，重试无意义）；
/// - 不带状态码的 LLM 错误（解析/参数类）→ 不可重试，避免死循环。
pub fn is_retryable_llm_error(e: &AgentError) -> bool {
    match e {
        AgentError::Timeout(_) => true,
        AgentError::Io(_) => true,
        AgentError::Llm(msg) => extract_http_status(msg).is_some_and(is_retryable_status),
        _ => false,
    }
}

/// 指数退避等待时长（秒）：3s → 6s → 12s → 24s → 48s → 60s 封顶。
pub fn backoff_delay_secs(attempt: u32) -> u64 {
    let exp = 3u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(8));
    exp.min(60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_function_signature_is_stable() {
        let _f: fn(reqwest::Error, &str) -> AgentError = map_reqwest_error;
    }

    #[test]
    fn extract_status_from_messages() {
        assert_eq!(
            extract_http_status("openai HTTP 429 at http://x/"),
            Some(429)
        );
        assert_eq!(extract_http_status("anthropic HTTP 500: boom"), Some(500));
        assert_eq!(extract_http_status("HTTP 404 at ..."), Some(404));
        assert_eq!(extract_http_status("no status here"), None);
        assert_eq!(
            extract_http_status("parse error: not a valid structured output"),
            None
        );
    }

    #[test]
    fn retryable_classification() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));

        assert!(is_retryable_llm_error(&AgentError::Timeout("t".into())));
        assert!(is_retryable_llm_error(&AgentError::Io("net".into())));
        assert!(is_retryable_llm_error(&AgentError::Llm("HTTP 429".into())));
        assert!(!is_retryable_llm_error(&AgentError::Llm("HTTP 404".into())));
        assert!(!is_retryable_llm_error(&AgentError::Llm(
            "parse error: x".into()
        )));
        assert!(!is_retryable_llm_error(&AgentError::ThinkingOutputInvalid(
            "x".into()
        )));
    }

    #[test]
    fn backoff_caps_at_60s() {
        assert_eq!(backoff_delay_secs(1), 3);
        assert_eq!(backoff_delay_secs(2), 6);
        assert_eq!(backoff_delay_secs(3), 12);
        assert_eq!(backoff_delay_secs(4), 24);
        assert_eq!(backoff_delay_secs(5), 48);
        assert_eq!(backoff_delay_secs(6), 60);
        assert_eq!(backoff_delay_secs(50), 60);
    }
}

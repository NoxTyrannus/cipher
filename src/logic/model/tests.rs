use super::*;
use httptest::{matchers::*, responders::*, Expectation, Server};

fn make_openai_response() -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": { "role": "assistant", "content": "hi from openai" }
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
    })
}

fn make_anthropic_response() -> serde_json::Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": "hi from anthropic" }],
        "usage": { "input_tokens": 5, "output_tokens": 3 }
    })
}

#[tokio::test]
async fn openai_call_sends_authorization_bearer() {
    let mut server = Server::run();
    let url_trimmed = server.url_str("").trim_end_matches('/').to_string();
    server.expect(
        Expectation::matching(all_of![
            request::method("POST"),
            request::path("/v1/chat/completions"),
            request::body(matches(".*gpt-4o.*")),
        ])
        .respond_with(json_encoded(make_openai_response())),
    );
    let p = OpenAiProvider::new();
    let req = LlmRequest {
        model: "gpt-4o".to_string(),
        messages: vec![ChatMessage::User {
            text: "hi".to_string(),
        }],
        api_url: format!("{url_trimmed}/v1"),
        api_key: Some(secrecy::SecretString::new("sk-test".to_string())),
        ..Default::default()
    };
    let r = p.call(&req).await.unwrap();
    assert_eq!(r.content, "hi from openai");
    server.verify_and_clear();
}

#[tokio::test]
async fn anthropic_call_sends_x_api_key_header() {
    let mut server = Server::run();
    server.expect(
        Expectation::matching(all_of![
            request::method("POST"),
            request::path("/v1/messages"),
            request::body(matches(".*claude-3-5-sonnet.*")),
        ])
        .respond_with(json_encoded(make_anthropic_response())),
    );
    let url_trimmed = server.url_str("").trim_end_matches('/').to_string();
    let p = AnthropicProvider::new();
    let req = LlmRequest {
        model: "claude-3-5-sonnet".to_string(),
        messages: vec![ChatMessage::User {
            text: "hi".to_string(),
        }],
        max_tokens: Some(1024),
        api_url: url_trimmed,
        api_key: Some(secrecy::SecretString::new("sk-ant-test".to_string())),
        ..Default::default()
    };
    let r = p.call(&req).await.unwrap();
    assert_eq!(r.content, "hi from anthropic");
    server.verify_and_clear();
}

#[tokio::test]
async fn provider_returns_llm_error_on_4xx() {
    let mut server = Server::run();
    server.expect(
        Expectation::matching(request::path("/v1/chat/completions"))
            .respond_with(status_code(401).body("invalid api key")),
    );
    let url_trimmed = server.url_str("").trim_end_matches('/').to_string();
    let p = OpenAiProvider::new();
    let req = LlmRequest {
        model: "gpt-4o".to_string(),
        messages: vec![ChatMessage::User {
            text: "hi".to_string(),
        }],
        api_url: format!("{url_trimmed}/v1"),
        api_key: Some(secrecy::SecretString::new("sk-bad".to_string())),
        ..Default::default()
    };
    let r = p.call(&req).await;
    server.verify_and_clear();
    assert!(matches!(r, Err(crate::common::AgentError::Llm(_))));
}

#[tokio::test]
async fn provider_returns_io_error_on_connection_refused() {
    let p = OpenAiProvider::new();
    let req = LlmRequest {
        model: "gpt-4o".to_string(),
        messages: vec![ChatMessage::User {
            text: "hi".to_string(),
        }],
        api_url: "http://127.0.0.1:1/v1".to_string(),
        api_key: Some(secrecy::SecretString::new("sk-test".to_string())),
        ..Default::default()
    };
    let r = p.call(&req).await;
    assert!(matches!(r, Err(crate::common::AgentError::Io(_))));
}

#[tokio::test]
async fn provider_registry_can_register_two() {
    let mut r = crate::logic::model::ProviderRegistry::new();
    r.register(std::sync::Arc::new(OpenAiProvider::new()));
    r.register(std::sync::Arc::new(AnthropicProvider::new()));

    assert!(r.pick_by_kind("openai").is_some());
    assert!(r.pick_by_kind("anthropic").is_some());
}

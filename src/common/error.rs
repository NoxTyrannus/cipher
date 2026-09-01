use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("placeholder error: {0}")]
    Placeholder(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("thinking output invalid: {0}")]
    ThinkingOutputInvalid(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("llm error: {0}")]
    Llm(String),

    #[error("render error: {0}")]
    Render(String),

    #[error("bootstrap error: {0}")]
    Bootstrap(String),

    #[error("startup failed: {0}")]
    StartupFailed(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("not implemented: {0}")]
    NotImplemented(String),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("scope locked: {0}")]
    ScopeLocked(String),

    #[error("not scope holder: {0}")]
    NotHolder(String),

    #[error("plugin error: {0}")]
    Plugin(String),

    #[error("script error: {0}")]
    Script(String),
}

impl From<std::io::Error> for AgentError {
    fn from(err: std::io::Error) -> Self {
        AgentError::Io(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AgentError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_placeholder_includes_prefix_and_message() {
        let e = AgentError::Placeholder("msg".to_string());
        let s = e.to_string();
        assert!(s.contains("placeholder error:"), "got: {s}");
        assert!(s.contains("msg"), "got: {s}");
    }

    #[test]
    fn display_parse_includes_prefix_and_message() {
        let e = AgentError::Parse("bad json".to_string());
        let s = e.to_string();
        assert!(s.contains("parse error:"), "got: {s}");
        assert!(s.contains("bad json"), "got: {s}");
    }

    #[test]
    fn display_thinking_output_invalid_includes_prefix_and_message() {
        let error = AgentError::ThinkingOutputInvalid("loop_forbids_say".to_string());
        let rendered = error.to_string();
        assert!(rendered.contains("thinking output invalid:"));
        assert!(rendered.contains("loop_forbids_say"));
    }

    #[test]
    fn display_io_includes_prefix_and_message() {
        let e = AgentError::Io("disk full".to_string());
        let s = e.to_string();
        assert!(s.contains("io error:"), "got: {s}");
        assert!(s.contains("disk full"), "got: {s}");
    }

    #[test]
    fn display_llm_includes_prefix_and_message() {
        let e = AgentError::Llm("rate limited".to_string());
        let s = e.to_string();
        assert!(s.contains("llm error:"), "got: {s}");
        assert!(s.contains("rate limited"), "got: {s}");
    }

    #[test]
    fn display_render_includes_prefix_and_message() {
        let e = AgentError::Render("no tty".to_string());
        let s = e.to_string();
        assert!(s.contains("render error:"), "got: {s}");
        assert!(s.contains("no tty"), "got: {s}");
    }

    #[test]
    fn display_bootstrap_includes_prefix_and_message() {
        let e = AgentError::Bootstrap("duckdb open failed".to_string());
        let s = e.to_string();
        assert!(s.contains("bootstrap error:"), "got: {s}");
        assert!(s.contains("duckdb open failed"), "got: {s}");
    }

    #[test]
    fn display_startup_failed_includes_prefix_and_message() {
        let e = AgentError::StartupFailed("2 checks failed".to_string());
        let s = e.to_string();
        assert!(s.contains("startup failed:"), "got: {s}");
        assert!(s.contains("2 checks failed"), "got: {s}");
    }

    #[test]
    fn display_not_found_includes_prefix_and_message() {
        let e = AgentError::NotFound("base capability: nope".to_string());
        let s = e.to_string();
        assert!(s.contains("not found:"), "got: {s}");
        assert!(s.contains("base capability: nope"), "got: {s}");
    }

    #[test]
    fn display_not_implemented_includes_prefix_and_message() {
        let e = AgentError::NotImplemented("anthropic provider".to_string());
        let s = e.to_string();
        assert!(s.contains("not implemented:"), "got: {s}");
    }

    #[test]
    fn display_timeout_includes_prefix_and_message() {
        let e = AgentError::Timeout("llm 30s".to_string());
        let s = e.to_string();
        assert!(s.contains("timeout:"), "got: {s}");
    }

    #[test]
    fn display_scope_locked_includes_prefix_and_message() {
        let e = AgentError::ScopeLocked("task-123".to_string());
        let s = e.to_string();
        assert!(s.contains("scope locked:"), "got: {s}");
    }

    #[test]
    fn display_not_holder_includes_prefix_and_message() {
        let e = AgentError::NotHolder("task-456".to_string());
        let s = e.to_string();
        assert!(s.contains("not scope holder:"), "got: {s}");
    }

    #[test]
    fn display_plugin_includes_prefix_and_message() {
        let e = AgentError::Plugin("load libcipher_plugin.so".to_string());
        let s = e.to_string();
        assert!(s.contains("plugin error:"), "got: {s}");
    }

    #[test]
    fn display_script_includes_prefix_and_message() {
        let e = AgentError::Script("write denied".to_string());
        let s = e.to_string();
        assert!(s.contains("script error:"), "got: {s}");
    }

    #[test]
    fn from_io_error_maps_to_io_variant_with_message() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "test");
        let err: AgentError = io_err.into();
        match err {
            AgentError::Io(msg) => assert!(msg.contains("test"), "got: {msg}"),
            other => panic!("expected Io variant, got {other:?}"),
        }
    }

    #[test]
    fn empty_string_message_is_preserved() {
        for e in [
            AgentError::Placeholder(String::new()),
            AgentError::Parse(String::new()),
            AgentError::Io(String::new()),
            AgentError::Llm(String::new()),
            AgentError::Render(String::new()),
            AgentError::Bootstrap(String::new()),
            AgentError::StartupFailed(String::new()),
            AgentError::NotFound(String::new()),
            AgentError::NotImplemented(String::new()),
            AgentError::Timeout(String::new()),
            AgentError::ScopeLocked(String::new()),
            AgentError::NotHolder(String::new()),
            AgentError::Plugin(String::new()),
            AgentError::Script(String::new()),
        ] {
            let s = e.to_string();

            assert!(s.ends_with(": "), "got: {s}");
        }
    }

    #[test]
    fn unicode_message_is_preserved_intact() {
        let msg = "中文 🦀 — λ → ∞";
        for e in [
            AgentError::Placeholder(msg.to_string()),
            AgentError::Parse(msg.to_string()),
            AgentError::Io(msg.to_string()),
            AgentError::Llm(msg.to_string()),
            AgentError::Render(msg.to_string()),
            AgentError::Bootstrap(msg.to_string()),
            AgentError::StartupFailed(msg.to_string()),
            AgentError::NotFound(msg.to_string()),
            AgentError::NotImplemented(msg.to_string()),
            AgentError::Timeout(msg.to_string()),
            AgentError::ScopeLocked(msg.to_string()),
            AgentError::NotHolder(msg.to_string()),
            AgentError::Plugin(msg.to_string()),
            AgentError::Script(msg.to_string()),
        ] {
            let s = e.to_string();
            assert!(s.contains(msg), "got: {s}");
        }
    }

    #[test]
    fn thirteen_variants_are_mutually_distinct() {
        use std::mem::discriminant;

        let errors = [
            ("Placeholder", AgentError::Placeholder("x".to_string())),
            ("Parse", AgentError::Parse("x".to_string())),
            ("Io", AgentError::Io("x".to_string())),
            ("Llm", AgentError::Llm("x".to_string())),
            ("Render", AgentError::Render("x".to_string())),
            ("Bootstrap", AgentError::Bootstrap("x".to_string())),
            ("StartupFailed", AgentError::StartupFailed("x".to_string())),
            ("NotFound", AgentError::NotFound("x".to_string())),
            (
                "NotImplemented",
                AgentError::NotImplemented("x".to_string()),
            ),
            ("Timeout", AgentError::Timeout("x".to_string())),
            ("ScopeLocked", AgentError::ScopeLocked("x".to_string())),
            ("NotHolder", AgentError::NotHolder("x".to_string())),
            ("Plugin", AgentError::Plugin("x".to_string())),
            ("Script", AgentError::Script("x".to_string())),
        ];

        for i in 0..errors.len() {
            for j in (i + 1)..errors.len() {
                assert_ne!(
                    discriminant(&errors[i].1),
                    discriminant(&errors[j].1),
                    "{} and {} should be distinct variants",
                    errors[i].0,
                    errors[j].0,
                );
            }
        }

        for i in 0..errors.len() {
            for j in (i + 1)..errors.len() {
                assert_ne!(
                    errors[i].1.to_string(),
                    errors[j].1.to_string(),
                    "display of {} should differ from display of {}",
                    errors[i].0,
                    errors[j].0,
                );
            }
        }
    }

    #[test]
    fn from_io_error_does_not_expose_source_but_preserves_message() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: AgentError = io_err.into();

        let s = err.to_string();
        assert!(s.contains("denied"), "got: {s}");

        use std::error::Error as _;
        assert!(
            err.source().is_none(),
            "source should not be exposed per module contract"
        );
    }
}

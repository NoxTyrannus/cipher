use crate::common::AgentError;
use std::path::Path;

pub fn map_wasmtime_error(e: wasmtime::Error, ctx: &str) -> AgentError {
    AgentError::Script(format!("wasmtime {ctx}: {e}"))
}

pub fn load_module(engine: &wasmtime::Engine, path: &Path) -> Result<wasmtime::Module, AgentError> {
    wasmtime::Module::from_file(engine, path).map_err(|e| map_wasmtime_error(e, "compile"))
}

pub fn instantiate(
    mut store: impl wasmtime::AsContextMut,
    module: &wasmtime::Module,
) -> Result<wasmtime::Instance, AgentError> {
    wasmtime::Instance::new(store.as_context_mut(), module, &[])
        .map_err(|e| map_wasmtime_error(e, "instantiate"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_wasmtime_error_includes_ctx() {
        let e = wasmtime::Error::msg("test boom");
        let mapped = map_wasmtime_error(e, "compile");
        match mapped {
            AgentError::Script(s) => {
                assert!(s.contains("wasmtime compile"), "ctx missing: {s}");
                assert!(s.contains("test boom"), "msg missing: {s}");
            }
            other => panic!("expected Script, got {other:?}"),
        }
    }

    #[test]
    fn load_module_nonexistent_returns_script() {
        let engine = wasmtime::Engine::default();
        let r = load_module(&engine, Path::new("/nonexistent/hello.wasm"));
        assert!(matches!(r, Err(AgentError::Script(_))));
    }
}

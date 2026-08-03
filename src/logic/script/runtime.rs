use super::error::{load_module, map_wasmtime_error};
use super::policy::SandboxPolicy;
use crate::common::AgentError;
use std::path::Path;

pub struct WasmRuntime {
    engine: wasmtime::Engine,
    policy: SandboxPolicy,
    fuel: u64,
}

impl WasmRuntime {
    pub fn run_with_host(
        &self,
        module_path: &Path,
        input: &str,
        host_context: super::host_context::HostContext,
    ) -> Result<String, AgentError> {
        let module = load_module(&self.engine, module_path)?;
        let mut store = wasmtime::Store::new(&self.engine, host_context);
        store
            .set_fuel(self.fuel)
            .map_err(|e| map_wasmtime_error(e, "set fuel"))?;

        let mut linker: wasmtime::Linker<super::host_context::HostContext> =
            wasmtime::Linker::new(&self.engine);
        super::host_functions::register_host_functions(&mut linker)?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| map_wasmtime_error(e, "instantiate with host"))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| AgentError::Script("missing export 'memory'".to_string()))?;
        let input_bytes = input.as_bytes();
        memory
            .write(&mut store, 0, input_bytes)
            .map_err(|e| map_wasmtime_error(e.into(), "write input"))?;

        let run_func = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .map_err(|e| map_wasmtime_error(e, "get run"))?;
        let output_ptr = run_func
            .call(&mut store, (0, input_bytes.len() as i32))
            .map_err(|e| map_wasmtime_error(e, "call run"))?;

        let len_func = instance
            .get_typed_func::<(), i32>(&mut store, "output_len")
            .map_err(|e| map_wasmtime_error(e, "get output_len"))?;
        let output_len = len_func
            .call(&mut store, ())
            .map_err(|e| map_wasmtime_error(e, "call output_len"))?
            .max(0) as usize;

        let mut buf = vec![0u8; output_len];
        memory
            .read(&store, output_ptr as usize, &mut buf)
            .map_err(|e| map_wasmtime_error(e.into(), "read output"))?;
        String::from_utf8(buf).map_err(|e| AgentError::Script(format!("output not utf-8: {e}")))
    }

    pub fn new() -> Result<Self, AgentError> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine =
            wasmtime::Engine::new(&config).map_err(|e| map_wasmtime_error(e, "engine init"))?;
        Ok(Self {
            engine,
            policy: SandboxPolicy::default_for_scripts(),
            fuel: 1_000_000,
        })
    }

    pub fn with_policy(mut self, policy: SandboxPolicy) -> Self {
        self.policy = policy;
        self
    }
}

impl Default for WasmRuntime {
    fn default() -> Self {
        Self::new().expect("WasmRuntime::new should not fail with default config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_runtime_uses_deny_all_and_default_fuel() {
        let r = WasmRuntime::new().unwrap();
        assert_eq!(r.policy, SandboxPolicy::DenyAll);
        assert_eq!(r.fuel, 1_000_000);
    }

    #[test]
    fn with_policy_overrides() {
        let r = WasmRuntime::new()
            .unwrap()
            .with_policy(SandboxPolicy::AllowRandom);
        assert_eq!(r.policy, SandboxPolicy::AllowRandom);
    }
}

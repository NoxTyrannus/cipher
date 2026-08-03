pub mod abi;
pub mod error;
pub mod host_context;
pub mod host_functions;
pub mod policy;
pub mod runtime;

pub use error::{instantiate, load_module, map_wasmtime_error};
pub use policy::SandboxPolicy;
pub use runtime::WasmRuntime;

use crate::common::Result;

pub trait Script: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    fn run(&self, input: &str) -> Result<String> {
        Ok(format!("script exec: {} (input: {})", self.id(), input))
    }
}

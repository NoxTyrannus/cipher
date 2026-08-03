use crate::common::AgentError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPolicy {
    DenyAll,

    AllowRandom,

    AllowFsRead,
}

impl SandboxPolicy {
    pub fn default_for_scripts() -> Self {
        SandboxPolicy::DenyAll
    }

    pub fn configure_linker(&self, _linker: &mut wasmtime::Linker<()>) -> Result<(), AgentError> {
        match self {
            SandboxPolicy::DenyAll => Ok(()),
            SandboxPolicy::AllowRandom => Err(AgentError::Script(
                "AllowRandom not implemented in iter58 (per Plan 3 spec §8)".to_string(),
            )),
            SandboxPolicy::AllowFsRead => Err(AgentError::Script(
                "AllowFsRead not implemented in iter58 (per Plan 3 spec §8)".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_for_scripts_is_deny_all() {
        assert_eq!(SandboxPolicy::default_for_scripts(), SandboxPolicy::DenyAll);
    }

    #[test]
    fn deny_all_configure_linker_is_noop() {
        let engine = wasmtime::Engine::default();
        let mut linker: wasmtime::Linker<()> = wasmtime::Linker::new(&engine);
        assert!(SandboxPolicy::DenyAll.configure_linker(&mut linker).is_ok());
    }

    #[test]
    fn allow_random_returns_script_error_in_iter58() {
        let engine = wasmtime::Engine::default();
        let mut linker: wasmtime::Linker<()> = wasmtime::Linker::new(&engine);
        let r = SandboxPolicy::AllowRandom.configure_linker(&mut linker);
        assert!(matches!(r, Err(AgentError::Script(_))));
    }

    #[test]
    fn allow_fs_read_returns_script_error_in_iter58() {
        let engine = wasmtime::Engine::default();
        let mut linker: wasmtime::Linker<()> = wasmtime::Linker::new(&engine);
        let r = SandboxPolicy::AllowFsRead.configure_linker(&mut linker);
        assert!(matches!(r, Err(AgentError::Script(_))));
    }
}

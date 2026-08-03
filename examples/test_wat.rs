use cipher::logic::script::host_context::{BudgetSnapshot, HostContext, PermissionSnapshot};
use cipher::logic::script::runtime::WasmRuntime;
use std::path::PathBuf;

fn main() {
    let runtime = WasmRuntime::new().unwrap();
    let wasm_path = PathBuf::from("data/wasm/file_read.wat");

    let host_ctx = HostContext {
        permission: PermissionSnapshot {
            file_read_roots: vec![std::env::current_dir().unwrap()],
            file_write_roots: vec![std::env::current_dir().unwrap()],
            shell_exec_allowed: true,
            ..Default::default()
        },
        budget: BudgetSnapshot::default(),
        duckdb: None,
        triviumdb: None,
    };

    let input = r#"{"path":"Cargo.toml"}"#;
    println!("Input: {input}");
    println!("Input len: {}", input.len());

    match runtime.run_with_host(&wasm_path, input, host_ctx) {
        Ok(output) => println!("Output: {output}"),
        Err(e) => println!("Error: {e}"),
    }
}

use cipher::common::Result;
use cipher::data::cognitive_seed::{ensure_default_capabilities, import_factory_defaults};
use cipher::data::duckdb::loader::Registry;
use cipher::logic::capability::executor::CapabilityExecutor;
use cipher::logic::capability::service::{CapabilityCall, CapabilityService};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct Args {
    data_dir: PathBuf,
    workspace: PathBuf,
    import_file: PathBuf,
    capability: String,
    arguments: serde_json::Value,
}

fn parse_args() -> Result<Args> {
    let mut data_dir = None;
    let mut workspace = None;
    let mut import_file = None;
    let mut capability = None;
    let mut arguments = serde_json::json!({});
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                data_dir = Some(PathBuf::from(args.next().ok_or_else(|| {
                    cipher::common::AgentError::Parse("missing --data-dir value".into())
                })?))
            }
            "--workspace" => {
                workspace = Some(PathBuf::from(args.next().ok_or_else(|| {
                    cipher::common::AgentError::Parse("missing --workspace value".into())
                })?))
            }
            "--import-file" => {
                import_file = Some(PathBuf::from(args.next().ok_or_else(|| {
                    cipher::common::AgentError::Parse("missing --import-file value".into())
                })?))
            }
            "--capability" => {
                capability = Some(args.next().ok_or_else(|| {
                    cipher::common::AgentError::Parse("missing --capability value".into())
                })?)
            }
            "--arguments" => {
                let raw = args.next().ok_or_else(|| {
                    cipher::common::AgentError::Parse("missing --arguments value".into())
                })?;
                arguments = serde_json::from_str(&raw).map_err(|e| {
                    cipher::common::AgentError::Parse(format!("parse --arguments: {e}"))
                })?;
            }
            other => {
                return Err(cipher::common::AgentError::Parse(format!(
                    "unknown arg {other}"
                )))
            }
        }
    }
    Ok(Args {
        data_dir: data_dir
            .ok_or_else(|| cipher::common::AgentError::Parse("--data-dir required".into()))?,
        workspace: workspace
            .ok_or_else(|| cipher::common::AgentError::Parse("--workspace required".into()))?,
        import_file: import_file
            .ok_or_else(|| cipher::common::AgentError::Parse("--import-file required".into()))?,
        capability: capability
            .ok_or_else(|| cipher::common::AgentError::Parse("--capability required".into()))?,
        arguments,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    std::fs::create_dir_all(&args.data_dir)?;
    std::fs::create_dir_all(&args.workspace)?;

    let app = cipher::data::bootstrap(&args.data_dir)?;
    ensure_default_capabilities(&args.data_dir)?;
    import_factory_defaults(&app.duckdb, &args.data_dir)?;

    let duckdb = Arc::new(Mutex::new(app.duckdb));
    let mut executor = CapabilityExecutor::new();
    executor.set_duckdb(Arc::clone(&duckdb));
    executor.set_workspace_root(&args.workspace);
    let executor = Arc::new(executor);

    let registry = load_registry(&duckdb)?;
    let import_json = std::fs::read_to_string(&args.import_file)?;
    let import_args: serde_json::Value = serde_json::from_str(&import_json)
        .map_err(|e| cipher::common::AgentError::Parse(format!("parse import file: {e}")))?;
    let call = CapabilityCall {
        capability_id: "capability.import".to_string(),
        capability_name: "Import Capability".to_string(),
        arguments: import_args,
    };
    let import_result =
        CapabilityService::new(&registry, &executor)?.execute_for_agent("agent", &call)?;
    let import_text = serde_json::to_string_pretty(&import_result.output)
        .map_err(|e| cipher::common::AgentError::Parse(format!("serialize import: {e}")))?;
    println!("IMPORT {import_text}");

    let registry = load_registry(&duckdb)?;
    let capability_name = if let Some(row) = registry.base_capabilities.get(&args.capability) {
        row.name.clone()
    } else if let Some(row) = registry.composite_capabilities.get(&args.capability) {
        row.name.clone()
    } else {
        args.capability.clone()
    };
    let exec_call = CapabilityCall {
        capability_id: args.capability.clone(),
        capability_name,
        arguments: args.arguments,
    };
    let result =
        CapabilityService::new(&registry, &executor)?.execute_for_agent("agent", &exec_call)?;
    let exec_text = serde_json::to_string_pretty(&result.output)
        .map_err(|e| cipher::common::AgentError::Parse(format!("serialize exec: {e}")))?;
    println!("EXEC {exec_text}");
    Ok(())
}

fn load_registry(duckdb: &Arc<Mutex<duckdb::Connection>>) -> Result<Registry> {
    let guard = duckdb
        .lock()
        .map_err(|_| cipher::common::AgentError::Io("duckdb lock poisoned".into()))?;
    cipher::data::duckdb::loader::load_all_into_memory(&guard)
}

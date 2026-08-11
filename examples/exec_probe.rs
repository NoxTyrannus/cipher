//! 执行中台测试探针: 不起 TUI, 直接驱动执行中台跑一个目标文本。
//!
//! ```bash
//! cargo run --release --example exec_probe -- \
//!   --config ~/.config/cipher/config.toml \
//!   --data-dir ~/.local/share/cipher \
//!   --workspace /tmp/probe_ws \
//!   --goal "统计 data/sales_2024.csv 的销售额 top5 品类并写入 top5.md" \
//!   --out /tmp/probe_result.jsonl
//! ```
//!
//! 输出 JSONL (每行一个 JSON 对象):
//! - `run_start` / `design` / `node` × N / `run_end`

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use cipher::agent::agent_pool::AgentPool;
use cipher::agent::execution_platform::ExecutionPlatform;
use cipher::agent::subagent::SubAgentPool;
use cipher::common::UtcTimestamp;

struct Args {
    config: PathBuf,
    data_dir: Option<PathBuf>,
    workspace: PathBuf,
    goal: String,
    out: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "usage: exec_probe --config <path> [--data-dir <path>] --workspace <dir> --goal <text> [--out <jsonl>]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut raw = std::env::args().skip(1);
    let mut config: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut goal: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--config" => config = raw.next().map(PathBuf::from),
            "--data-dir" => data_dir = raw.next().map(PathBuf::from),
            "--workspace" => workspace = raw.next().map(PathBuf::from),
            "--goal" => goal = raw.next(),
            "--out" => out = raw.next().map(PathBuf::from),
            "--help" | "-h" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    let Some(config) = config else {
        eprintln!("missing --config");
        usage();
    };
    let Some(workspace) = workspace else {
        eprintln!("missing --workspace");
        usage();
    };
    let Some(goal) = goal else {
        eprintln!("missing --goal");
        usage();
    };
    Args {
        config,
        data_dir,
        workspace,
        goal,
        out,
    }
}

fn write_jsonl(report: &cipher::agent::execution_platform::ProbeRunReport, out: Option<&Path>) {
    let ts = UtcTimestamp::now().to_string();
    let emit = |value: serde_json::Value| {
        let line = serde_json::to_string(&value).expect("serialize jsonl line");
        if let Some(path) = out {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open --out file");
            let _ = writeln!(f, "{line}");
        } else {
            println!("{line}");
        }
    };

    emit(serde_json::json!({
        "type": "run_start",
        "goal": report.goal,
        "ts": ts,
    }));

    emit(serde_json::json!({
        "type": "design",
        "parse_attempts": report.design.parse_attempts,
        "parse_ok": report.design.parse_ok,
        "node_count": report.design.node_count,
        "kind": report.design.kind,
        "error": report.design.error,
    }));

    for node in &report.nodes {
        emit(serde_json::json!({
            "type": "node",
            "node_id": node.node_id,
            "capability": node.capability,
            "path": node.path,
            "status": node.status,
            "tool_calls": node.tool_calls,
            "turns": node.turns,
            "duration_ms": node.duration_ms,
            "error": node.error,
            "logs": node.logs,
        }));
    }

    emit(serde_json::json!({
        "type": "run_end",
        "ok": report.ok,
        "total_duration_ms": report.total_duration_ms,
        "usage": report.usage.clone().map(|u| serde_json::json!({
            "prompt": u.prompt,
            "completion": u.completion,
        })),
    }));
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = parse_args();

    std::fs::create_dir_all(&args.workspace).expect("create --workspace dir");
    let ws_root = args.workspace.canonicalize().unwrap_or(args.workspace);

    let mut config = match cipher::startup::init::init(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config load failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(dir) = &args.data_dir {
        config.data_dir = dir.clone();
    }
    if let Err(e) = cipher::startup::config::migrate_data_dir() {
        eprintln!("migrate_data_dir failed: {e}");
        return ExitCode::FAILURE;
    }

    let app_state = match cipher::data::bootstrap(&config.data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bootstrap failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let registry = match cipher::data::duckdb::loader::load_all_into_memory(&app_state.duckdb) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("load_all_into_memory failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let model = match config
        .default_model
        .as_ref()
        .and_then(|id| registry.models.get(id).cloned())
        .or_else(|| {
            registry
                .models
                .values()
                .find(|m| m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false))
                .cloned()
        }) {
        Some(m) => m,
        None => {
            eprintln!("model 表无已配置模型, 请先运行 `cipher setup`");
            return ExitCode::FAILURE;
        }
    };

    let provider_registry = match cipher::startup::init_flow::build_provider_registry(&model) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("build_provider_registry failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let provider = match provider_registry.pick_by_kind(&model.api_type.to_lowercase()) {
        Some(p) => Arc::clone(p),
        None => {
            eprintln!("no provider impl for api_type '{}'", model.api_type);
            return ExitCode::FAILURE;
        }
    };
    let api_key = match cipher::logic::model::api_key::resolve_api_key(&model) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("resolve_api_key failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut executor = cipher::logic::capability::executor::CapabilityExecutor::new();
    executor.set_wasm(&config.data_dir.join("wasm"), &ws_root);
    match duckdb::Connection::open(app_state.paths.duckdb()) {
        Ok(conn) => executor.set_duckdb(Arc::new(std::sync::Mutex::new(conn))),
        Err(e) => {
            eprintln!("duckdb open for executor failed (db.* 不可用): {e}");
        }
    }

    let (execution_tx, execution_rx) = tokio::sync::mpsc::channel(8);
    drop(execution_tx);
    let pool = Arc::new(AgentPool::new().0);
    let subagent_pool = Arc::new(SubAgentPool::new());

    let platform = ExecutionPlatform::new(
        execution_rx,
        pool,
        provider,
        model.clone(),
        api_key,
        subagent_pool,
        None,
        None,
        None,
        Some(config.data_dir.join("prompts")),
        vec![
            "file.read".to_string(),
            "file.write".to_string(),
            "file.list".to_string(),
            "file.delete".to_string(),
            "file.move".to_string(),
            "text.grep".to_string(),
            cipher::data::factory::default_shell_capability_id().to_string(),
        ],
        Some(registry),
        Some(Arc::new(executor)),
    );

    let report = platform.probe_goal(&args.goal).await;
    write_jsonl(&report, args.out.as_deref());

    if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

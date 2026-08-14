//! PTY 测试辅助：向指定数据目录的 DuckDB 插入一条模型行。
//!
//! 默认插入指向本地 mock LLM 的模型；真实 API 冒烟（--real）时通过
//! `--api-key <minimax key>` 传入真实密钥（模型行 api_url 仍指向本地
//! mock 代理，由 mock 透明转发到上游）。
//!
//! ```bash
//! cargo run --example insert_mock_model -- \
//!   --data-dir /tmp/cipher-ptytest/data \
//!   --id mock-1-mock \
//!   --api-url http://127.0.0.1:PORT \
//!   --model-id mock-model [--api-key <key>]
//! ```

use cipher::common::AgentError;
use cipher::data::duckdb::loader::{insert_model, ModelRow};
use std::path::PathBuf;
use std::process::ExitCode;

struct Args {
    data_dir: PathBuf,
    id: String,
    api_url: String,
    model_id: String,
    api_key: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: insert_mock_model --data-dir <dir> --id <id> --api-url <url> --model-id <mid> [--api-key <key>]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut raw = std::env::args().skip(1);
    let mut data_dir: Option<PathBuf> = None;
    let mut id: Option<String> = None;
    let mut api_url: Option<String> = None;
    let mut model_id: Option<String> = None;
    let mut api_key: Option<String> = None;
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--data-dir" => data_dir = raw.next().map(PathBuf::from),
            "--id" => id = raw.next(),
            "--api-url" => api_url = raw.next(),
            "--model-id" => model_id = raw.next(),
            "--api-key" => api_key = raw.next(),
            "--help" | "-h" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    let Some(data_dir) = data_dir else { usage() };
    let Some(id) = id else { usage() };
    let Some(api_url) = api_url else { usage() };
    let Some(model_id) = model_id else { usage() };
    Args {
        data_dir,
        id,
        api_url,
        model_id,
        api_key: api_key.unwrap_or_else(|| "mock-key".to_string()),
    }
}

fn main() -> ExitCode {
    let args = parse_args();
    match run(&args) {
        Ok(()) => {
            println!("inserted model {} -> {}", args.id, args.api_url);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("insert_mock_model failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), AgentError> {
    let app_state = cipher::data::bootstrap(&args.data_dir)?;
    // 模拟 `cipher setup` 的 init_flow：真实启动（run_normal）要求 agent 行已存在，
    // 否则执行平台的能力注册表缺 agent → shell.exec/file.* 真实执行全部失败。
    // 注意与 init_flow 一致：tool_caps 留 NULL（能力行与 tool_caps 由 entry.rs
    // 启动种子在 bootstrap 之后写入并重载注册表，此处填全量会在 bootstrap 校验报
    // "unknown or non-executable capability_id"）。
    app_state.duckdb
        .execute(
            "INSERT INTO agent (id, name, mode, is_default) \
             SELECT 'agent', 'Agent', 'unni', true \
             WHERE NOT EXISTS (SELECT 1 FROM agent WHERE id = 'agent')",
            [],
        )
        .map_err(|e| AgentError::Bootstrap(format!("seed agent: {e}")))?;
    let row = ModelRow {
        id: args.id.clone(),
        name: "mock".to_string(),
        provider: "mock".to_string(),
        api_url: args.api_url.clone(),
        api_type: "OpenAI".to_string(),
        api_protocol: "openai-v1".to_string(),
        api_key: Some(args.api_key.clone()),
        model_id: args.model_id.clone(),
        config: Some(serde_json::json!({})),
    };
    insert_model(&app_state.duckdb, &row)?;
    Ok(())
}

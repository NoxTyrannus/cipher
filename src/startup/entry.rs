use super::config::{Config, MemoryMode};
use super::{init, self_check};
use crate::agent::context_assembler::{ContextAssembler, ContextConfig};
use crate::common::AgentError;
use crate::data::duckdb::loader::{
    has_configured_model, insert_model, load_all_into_memory, rename_agent,
    update_model_api_key_by_provider, ModelRow,
};
use crate::logic::model::stream::StreamChunk;
use crate::mode_runtime::ModeManager;
use crate::ui::backend::UiBackend;
use crate::ui::tui::config_panel::{ActionResult, ConfigView, DbRequest};
use crate::ui::tui::event::{key_event_to_action, TuiAction, BACKTAB_SENTINEL};
use crate::ui::tui::state::{TuiMode, TuiState};
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::time;
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let log_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cipher");
    let log_path = log_dir.join("cipher.log");
    let _ = std::fs::create_dir_all(&log_dir);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(move || -> Box<dyn std::io::Write + Send> {
            match OpenOptions::new().create(true).append(true).open(&log_path) {
                Ok(f) => Box::new(f),

                Err(_) => Box::new(std::io::sink()),
            }
        })
        .try_init();
}

const LEGACY_DEFAULT_PROMPT_SHA256: [(&str, &str); 4] = [
    (
        "system.md",
        "a3e6f5e733ad55b953092b7f2b980d28540e2c18970615cd2fe6eac23ff3ebd4",
    ),
    (
        "mode_unni.md",
        "27e594c8521f912e1be178c4259bfbe209e9558fe099ba7ffd1285823157940b",
    ),
    (
        "mode_keep.md",
        "5a1f25824a9ed22366eca179f9b89d2f6f70bcaa09f0cb7a7a4ee3877e29d7e7",
    ),
    (
        "mode_loop.md",
        "8df327d53f1cb7c672ee25667d8b4f4cfd1e42463d1ca578fae50c2828722afd",
    ),
];

fn ensure_default_prompts(data_dir: &Path) -> Result<(), AgentError> {
    ensure_default_prompts_with_legacy_hashes(data_dir, &LEGACY_DEFAULT_PROMPT_SHA256)
}

fn ensure_default_prompts_with_legacy_hashes(
    data_dir: &Path,
    legacy_hashes: &[(&str, &str)],
) -> Result<(), AgentError> {
    let prompts_dir = data_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)
        .map_err(|e| AgentError::Io(format!("create prompts dir: {e}")))?;

    let obsolete = prompts_dir.join("5_state_cycle.md");
    if obsolete.exists() {
        let _ = std::fs::remove_file(&obsolete);
        tracing::info!("ensure_default_prompts: removed obsolete 5_state_cycle.md");
    }
    for (name, content) in crate::logic::model::prompts::DEFAULT_PROMPTS {
        let path = prompts_dir.join(name);

        if name == "SOUL.md" {
            match std::fs::read(&path) {
                Ok(existing) => {
                    if existing != content.as_bytes() {
                        tracing::info!(
                            "ensure_default_prompts: 数据目录 SOUL.md 与出厂默认不同 \
                             (用户自定义或旧版本) — 以文件内容为准, 不覆盖; \
                             若需恢复出厂默认请删除该文件后重启"
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::write(&path, content)
                        .map_err(|e| AgentError::Io(format!("write SOUL.md: {e}")))?;
                }
                Err(error) => {
                    return Err(AgentError::Io(format!("read SOUL.md: {error}")));
                }
            }
            continue;
        }
        match std::fs::read(&path) {
            Ok(existing) => {
                let is_legacy_default = legacy_hashes
                    .iter()
                    .find_map(|(legacy_name, hash)| (*legacy_name == name).then_some(*hash))
                    .is_some_and(|hash| sha256_bytes(&existing) == hash);
                if is_legacy_default {
                    std::fs::write(&path, content)
                        .map_err(|e| AgentError::Io(format!("upgrade {name}: {e}")))?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&path, content)
                    .map_err(|e| AgentError::Io(format!("write {name}: {e}")))?;
            }
            Err(error) => {
                return Err(AgentError::Io(format!("read {name}: {error}")));
            }
        }
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn load_config(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
) -> Result<Config, AgentError> {
    let mut config = init::init(config_path)?;
    if let Some(dir) = data_dir_override {
        config.data_dir = dir;
    }
    Ok(config)
}

pub async fn run_setup(
    config_path: PathBuf,
    data_dir_override: Option<PathBuf>,
) -> Result<(), AgentError> {
    init_tracing();

    super::config::migrate_data_dir()?;
    let config = load_config(&config_path, data_dir_override)?;
    let app_state = crate::data::bootstrap(&config.data_dir)?;
    ensure_default_prompts(&config.data_dir)?;
    crate::data::factory::ensure_default_wasm_modules(&config.data_dir)?;
    crate::data::cognitive_seed::ensure_default_cognitive_seed(&config.data_dir)?;
    tracing::info!(data_dir = ?config.data_dir, "setup: bootstrap ready");
    if has_configured_model(&app_state.duckdb)? {
        print_already_configured();
    } else {
        print_welcome_and_help();
        crate::startup::init_flow::init_flow(&app_state, &config.data_dir).await?;
    }
    tracing::info!("setup: 初始化完成");
    Ok(())
}

pub async fn run_normal(
    config_path: PathBuf,
    data_dir_override: Option<PathBuf>,
) -> Result<(), AgentError> {
    init_tracing();

    super::config::migrate_data_dir()?;
    let config = load_config(&config_path, data_dir_override)?;
    let app_state = crate::data::bootstrap(&config.data_dir)?;
    ensure_default_prompts(&config.data_dir)?;
    tracing::info!(data_dir = ?config.data_dir, "normal: bootstrap ready");
    crate::data::factory::ensure_default_wasm_modules(&config.data_dir)?;
    crate::data::cognitive_seed::ensure_default_cognitive_seed(&config.data_dir)?;

    if !has_configured_model(&app_state.duckdb)? {
        return Err(AgentError::StartupFailed(
            "未配置模型, 请先运行 `cipher setup`".to_string(),
        ));
    }

    let reg = crate::data::duckdb::loader::load_all_into_memory(&app_state.duckdb)?;
    let default_model = config
        .default_model
        .as_ref()
        .and_then(|id| reg.models.get(id).cloned())
        .or_else(|| {
            reg.models
                .values()
                .find(|m| m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false))
                .cloned()
        })
        .ok_or_else(|| {
            AgentError::StartupFailed("model 表无已配置模型, 请先运行 `cipher setup`".to_string())
        })?;
    let check_results = self_check::run_all(&config, &default_model).await?;
    self_check::report(&check_results)?;
    tracing::info!("normal: self_check 通过");

    let provider_registry = crate::startup::init_flow::build_provider_registry(&default_model)?;
    let exec_provider = provider_registry
        .pick_by_kind(&default_model.api_type.to_lowercase())
        .cloned()
        .ok_or_else(|| {
            AgentError::StartupFailed(format!(
                "no provider impl for api_type '{}'",
                default_model.api_type
            ))
        })?;
    let exec_api_key = crate::logic::model::api_key::resolve_api_key(&default_model)?;
    let insight_api_key = exec_api_key.clone();
    let subagent_pool = std::sync::Arc::new(crate::agent::subagent::SubAgentPool::new());
    let insight_provider = std::sync::Arc::clone(&exec_provider);
    let memory_provider = std::sync::Arc::clone(&exec_provider);
    let memory_api_key = exec_api_key.clone();
    let prompts_dir = Some(config.data_dir.join("prompts"));

    let (pool, receivers) = crate::agent::agent_pool::AgentPool::new();
    let pool = std::sync::Arc::new(pool);

    pool.register_platform(
        "execution-platform",
        crate::agent::agent_pool::registry::AgentIdentity::ExecutionPlatform,
    )
    .await;
    pool.register_platform(
        "insight-platform",
        crate::agent::agent_pool::registry::AgentIdentity::InsightPlatform,
    )
    .await;
    pool.register_platform(
        "memory-platform",
        crate::agent::agent_pool::registry::AgentIdentity::MemoryPlatform,
    )
    .await;
    tracing::info!("P0-a: 三中台注册完成");

    let triviumdb_dir = app_state.paths.triviumdb_dir();
    crate::data::permissions::ensure_private_directory(&triviumdb_dir)?;
    let triviumdb_path = app_state.paths.triviumdb();
    let trivium_db = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::data::triviumdb::TriviumDb::open(
            &triviumdb_path,
            crate::data::triviumdb::DEFAULT_DIM,
        )
        .map_err(|e| AgentError::Bootstrap(format!("TriviumDB open: {e}")))?,
    ));

    {
        let mut db_guard = trivium_db.lock().await;
        crate::data::cognitive_seed::seed_cognitive_memory(&config.data_dir, &mut db_guard)
            .map_err(|e| AgentError::Bootstrap(format!("cognitive seed: {e}")))?;
    }

    {
        let shell_id = crate::data::factory::default_shell_capability_id();
        let shell_name = crate::data::factory::default_shell_capability_name();
        let shell_executor = format!("wasm:{}", shell_id.replace('.', "_"));

        let seed_sql = [
            "INSERT OR REPLACE INTO base_capability (id, name, type, description, schema_in, schema_out, executor, version, enabled) VALUES
             ('file.read', 'Read File', 'function', 'Read file content',
              '{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"]}',
              '{\"type\":\"object\",\"properties\":{\"content\":{\"type\":\"string\"},\"size\":{\"type\":\"integer\"}}}',
              'wasm:file.read', '1.0.0', true)",
            "INSERT OR REPLACE INTO base_capability (id, name, type, description, schema_in, schema_out, executor, version, enabled) VALUES
             ('file.write', 'Write File', 'function', 'Write content to file',
              '{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"content\":{\"type\":\"string\"}},\"required\":[\"path\",\"content\"]}',
              '{\"type\":\"object\",\"properties\":{\"success\":{\"type\":\"boolean\"}}}',
              'wasm:file.write', '1.0.0', true)",
            "INSERT OR REPLACE INTO base_capability (id, name, type, description, schema_in, schema_out, executor, version, enabled) VALUES
             ('file.list', 'List Directory', 'function', 'List directory entries',
              '{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"]}',
              '{\"type\":\"object\",\"properties\":{\"entries\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}}}',
              'wasm:file.list', '1.0.0', true)",
            "INSERT OR REPLACE INTO base_capability (id, name, type, description, schema_in, schema_out, executor, version, enabled) VALUES
             ('file.delete', 'Delete File', 'function', 'Delete a file',
              '{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"]}',
              '{\"type\":\"object\",\"properties\":{\"success\":{\"type\":\"boolean\"}}}',
              'wasm:file.delete', '1.0.0', true)",
            "INSERT OR REPLACE INTO base_capability (id, name, type, description, schema_in, schema_out, executor, version, enabled) VALUES
             ('file.move', 'Move File', 'function', 'Move or rename a file',
              '{\"type\":\"object\",\"properties\":{\"from\":{\"type\":\"string\"},\"to\":{\"type\":\"string\"}},\"required\":[\"from\",\"to\"]}',
              '{\"type\":\"object\",\"properties\":{\"success\":{\"type\":\"boolean\"}}}',
              'wasm:file.move', '1.0.0', true)",
            "INSERT OR REPLACE INTO base_capability (id, name, type, description, schema_in, schema_out, executor, version, enabled) VALUES
             ('text.grep', 'Grep Text', 'function', 'Search text pattern in file',
              '{\"type\":\"object\",\"properties\":{\"pattern\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"}},\"required\":[\"pattern\",\"path\"]}',
              '{\"type\":\"object\",\"properties\":{\"matches\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}}}',
              'wasm:text.grep', '1.0.0', true)",
        ];
        for sql in &seed_sql {
            app_state
                .duckdb
                .execute(sql, [])
                .map_err(|e| AgentError::Bootstrap(format!("seed base_capability: {e}")))?;
        }

        app_state
            .duckdb
            .execute(
                &format!(
                    "INSERT OR REPLACE INTO base_capability (id, name, type, description, schema_in, schema_out, executor, version, enabled) VALUES
                     ('{}', '{}', 'function', '{} command in workspace',
                      '{{\"type\":\"object\",\"properties\":{{\"command\":{{\"type\":\"string\"}}}},\"required\":[\"command\"]}}',
                      '{{\"type\":\"object\",\"properties\":{{\"stdout\":{{\"type\":\"string\"}},\"stderr\":{{\"type\":\"string\"}},\"exit_code\":{{\"type\":\"integer\"}}}}}}',
                      '{}', '1.0.0', true)",
                    shell_id, shell_name, shell_name, shell_executor
                ),
                [],
            )
            .map_err(|e| AgentError::Bootstrap(format!("seed shell capability: {e}")))?;

        let mut caps = vec![
            "file.read",
            "file.write",
            "file.list",
            "file.delete",
            "file.move",
            "text.grep",
        ];
        caps.push(shell_id);
        let caps_json = serde_json::to_string(&caps)
            .map_err(|e| AgentError::Bootstrap(format!("serialize tool_caps: {e}")))?;
        app_state
            .duckdb
            .execute(
                &format!(
                    "UPDATE agent SET tool_caps = '{}' WHERE id = 'agent'",
                    caps_json
                ),
                [],
            )
            .ok();
        tracing::info!("factory: seeded 7 base capabilities + agent tool_caps (shell={shell_id})");
    }

    let memory_db = {
        let memory_db_path = config.data_dir.join("memory.duckdb");
        let conn = duckdb::Connection::open(&memory_db_path)
            .map_err(|e| AgentError::Bootstrap(format!("open memory.duckdb: {e}")))?;
        crate::agent::memory::memory_version::create_memory_version_tables(&conn)?;
        std::sync::Arc::new(std::sync::Mutex::new(conn))
    };

    let pool_exec = std::sync::Arc::clone(&pool);
    let exec_provider_clone = exec_provider;
    let exec_model = default_model.clone();
    let exec_subagent_pool = std::sync::Arc::clone(&subagent_pool);
    let exec_trivium_db = Some(std::sync::Arc::clone(&trivium_db));
    let exec_product_store = Some(std::sync::Arc::new(
        crate::data::platform_product_store::PlatformProductStore::open(&config.data_dir)?,
    ));
    let exec_cursor_store = Some(std::sync::Arc::new(
        crate::data::platform_cursor::CursorStore::open(&config.data_dir, "execution")?,
    ));
    let exec_prompts_dir = prompts_dir.clone();
    let exec_capability_ids = crate::data::factory::default_shell_capability_ids();

    let exec_registry = Some(app_state.registry.clone());
    let exec_executor = {
        let mut ex = crate::logic::capability::executor::CapabilityExecutor::new();
        ex.set_wasm(
            &config.data_dir.join("wasm"),
            &std::env::current_dir().unwrap_or_default(),
        );
        let duckdb_path = app_state.paths.duckdb();
        match duckdb::Connection::open(&duckdb_path) {
            Ok(conn) => {
                ex.set_duckdb(std::sync::Arc::new(std::sync::Mutex::new(conn)));
            }
            Err(e) => {
                tracing::warn!(
                    "execution_platform: duckdb open for executor failed (db.* 不可用): {e}"
                );
            }
        }
        Some(std::sync::Arc::new(ex))
    };
    let execution_task = tokio::spawn(async move {
        crate::agent::execution_platform::run(
            pool_exec,
            receivers.execution_rx,
            exec_provider_clone,
            exec_model,
            exec_api_key,
            exec_subagent_pool,
            exec_trivium_db,
            exec_product_store,
            exec_cursor_store,
            exec_prompts_dir,
            exec_capability_ids,
            exec_registry,
            exec_executor,
        )
        .await;
    });

    let (tool_memory_tx, tool_memory_rx): (
        mpsc::Sender<Vec<crate::agent::communication::ToolMemoryUpdate>>,
        mpsc::Receiver<Vec<crate::agent::communication::ToolMemoryUpdate>>,
    ) = mpsc::channel(32);

    let tool_memory_conn = duckdb::Connection::open(app_state.paths.duckdb())
        .map_err(|e| AgentError::Bootstrap(format!("open duckdb for tool_memory consumer: {e}")))?;
    tokio::spawn(async move {
        let mut rx = tool_memory_rx;
        while let Some(updates) = rx.recv().await {
            for update in updates {
                if let Err(e) = crate::data::duckdb::loader::write_usage_observation(
                    &tool_memory_conn,
                    &update.capability_id,
                    &update.description_patch,
                    &update.rating,
                    &update.note,
                ) {
                    tracing::warn!(
                        "tool_memory: write_usage_observation failed for {}: {e}",
                        update.capability_id
                    );
                } else {
                    tracing::debug!(
                        "tool_memory: usage_method updated for capability={}, rating={}",
                        update.capability_id,
                        update.rating
                    );
                }
            }
        }
        tracing::info!("tool_memory consumer: rx closed, shutting down");
    });
    let pool_insight = std::sync::Arc::clone(&pool);
    let insight_model = default_model.clone();
    let insight_prompts_dir = prompts_dir.clone();
    let insight_task = tokio::spawn(async move {
        crate::agent::insight_platform::run(
            pool_insight,
            receivers.insight_rx,
            insight_provider,
            insight_model,
            insight_api_key,
            tool_memory_tx,
            insight_prompts_dir,
        )
        .await;
    });

    let pool_memory = std::sync::Arc::clone(&pool);
    let memory_model = default_model.clone();
    let memory_triviumdb_path = Some(triviumdb_path.clone());
    let memory_prompts_dir = prompts_dir.clone();

    let (experience_tx, experience_rx) =
        mpsc::channel::<crate::agent::communication::AttentionRetireBatch>(32);
    let (preference_tx, preference_rx) =
        mpsc::channel::<crate::agent::communication::AttentionRetireBatch>(32);
    let (cognitive_tx, cognitive_rx) = mpsc::channel::<()>(32);

    {
        let exp_provider = std::sync::Arc::clone(&memory_provider);
        let exp_model = memory_model.clone();
        let exp_api_key = memory_api_key.clone();
        let exp_prompts = memory_prompts_dir.clone();

        let exp_trivium = std::sync::Arc::clone(&trivium_db);
        tokio::spawn(async move {
            let agent = crate::agent::memory::experience_agent::ExperienceMemoryAgent::new(
                exp_provider,
                exp_model,
                Some(exp_api_key),
                Some(exp_trivium),
                exp_prompts,
                experience_rx,
            );
            agent.run().await;
        });
        tracing::info!("ExperienceAgent spawned");
    }

    {
        let pref_provider = std::sync::Arc::clone(&memory_provider);
        let pref_model = memory_model.clone();
        let pref_api_key = memory_api_key.clone();
        let pref_prompts = memory_prompts_dir.clone();

        let pref_trivium = std::sync::Arc::clone(&trivium_db);
        tokio::spawn(async move {
            let agent = crate::agent::memory::preference_agent::PreferenceMemoryAgent::new(
                pref_provider,
                pref_model,
                Some(pref_api_key),
                Some(pref_trivium),
                pref_prompts,
                preference_rx,
            );
            agent.run().await;
        });
        tracing::info!("PreferenceAgent spawned");
    }

    {
        let cog_provider = std::sync::Arc::clone(&memory_provider);
        let cog_model = memory_model.clone();
        let cog_api_key = memory_api_key.clone();
        let cog_prompts = memory_prompts_dir.clone();

        let cog_trivium = std::sync::Arc::clone(&trivium_db);
        let cog_memory_db = std::sync::Arc::clone(&memory_db);
        let cog_thought_store =
            crate::data::thought_store::ThoughtStore::open(app_state.paths.thoughts_data_root())
                .ok()
                .map(std::sync::Arc::new);
        tokio::spawn(async move {
            let agent = crate::agent::memory::cognitive_agent::CognitiveAgent::new(
                cog_provider,
                cog_model,
                Some(cog_api_key),
                Some(cog_trivium),
                cog_prompts,
                cognitive_rx,
                Some(cog_memory_db),
                cog_thought_store,
            );
            agent.run().await;
        });
        tracing::info!("CognitiveAgent spawned");
    }

    let memory_db_for_platform = std::sync::Arc::clone(&memory_db);
    let trivium_for_platform = std::sync::Arc::clone(&trivium_db);
    let memory_task = tokio::spawn(async move {
        crate::agent::memory_platform::run(
            pool_memory,
            receivers.memory_rx,
            memory_provider,
            memory_model,
            memory_api_key,
            memory_triviumdb_path,
            Some(trivium_for_platform),
            Some(memory_db_for_platform),
            memory_prompts_dir,
            Some(experience_tx),
            Some(preference_tx),
            Some(cognitive_tx),
        )
        .await;
    });
    tracing::info!("P0-a: 三中台 poll 循环已启动");

    use crate::agent::agent_pool::channels::TriggerEvent;
    let (trigger_fwd_tx, trigger_fwd_rx) = mpsc::channel::<TriggerEvent>(32);
    let _pool_trigger = std::sync::Arc::clone(&pool);
    let trigger_task = tokio::spawn(async move {
        let mut trigger_rx = receivers.trigger_rx;
        tracing::info!("trigger_receiver: started, polling trigger_rx");
        while let Some(event) = trigger_rx.recv().await {
            tracing::info!(
                "trigger_receiver: received TriggerEvent(turn_id={}, reason={})",
                event.turn_id,
                event.reason
            );

            let tid = event.turn_id.clone();
            if let Err(e) = trigger_fwd_tx.try_send(event) {
                tracing::warn!(
                    "trigger_receiver: fwd send error turn_id={}, error={}",
                    tid,
                    e
                );
            }
        }
        tracing::info!("trigger_receiver: rx closed, shutting down");
    });
    tracing::info!("P0-b: trigger 接收任务已启动");

    let ctx = crate::mode_runtime::ModeContext::default();

    let assembler_triviumdb_path = Some(triviumdb_path.clone());
    let thought_store = std::sync::Arc::new(crate::data::thought_store::ThoughtStore::open(
        app_state.paths.thoughts_data_root(),
    )?);
    let mut assembler = ContextAssembler::new_with_roots(
        ContextConfig::from(&config.context),
        app_state.paths.storage_root(),
        app_state.paths.root(),
        assembler_triviumdb_path,
    );
    assembler.set_thought_store(std::sync::Arc::clone(&thought_store));
    assembler.set_memory_db(std::sync::Arc::clone(&memory_db));
    assembler.set_shared_trivium(std::sync::Arc::clone(&trivium_db));

    let duckdb_for_mgr = {
        let duckdb_path = app_state.paths.duckdb();
        let conn = duckdb::Connection::open(&duckdb_path)
            .map_err(|e| AgentError::Bootstrap(format!("open duckdb for manager: {e}")))?;
        Some(std::sync::Arc::new(std::sync::Mutex::new(conn)))
    };

    let pool_for_status = std::sync::Arc::clone(&pool);
    let mut mode_manager = crate::mode_runtime::ModeManager::new_with_deps(
        ctx,
        provider_registry,
        default_model,
        thought_store,
        assembler,
        app_state.registry.clone(),
        pool,
        duckdb_for_mgr,
    );
    tracing::info!("mode_init: ModeManager ready (default: UNNI)");

    if !config.default_mode.eq_ignore_ascii_case("unni") {
        let default_kind = config
            .default_mode
            .parse::<crate::mode_runtime::ModeKind>()
            .map_err(|e| AgentError::Bootstrap(format!("invalid default_mode: {e}")))?;
        mode_manager.switch_mode(default_kind).await?;
        tracing::info!(
            "mode_init: switched to default_mode={}",
            default_kind.name()
        );
    }

    if std::io::stdout().is_terminal() {
        tracing::info!("tui_run: 真 ratatui TUI streaming loop starting");

        let memory_mode_shared = std::sync::Arc::new(std::sync::Mutex::new(config.memory_mode));
        run_streaming_loop(
            &mut mode_manager,
            trigger_fwd_rx,
            &app_state,
            pool_for_status,
            memory_mode_shared,
        )
        .await?;
    } else {
        let mut tui = crate::ui::TuiBackend::new();
        tracing::info!("tui_run: non-TTY blocking loop");
        run_main_loop(&mut mode_manager, &mut tui, || {
            crate::startup::config_flow::run(&app_state)
        })
        .await?;
    }

    let mut platform_handles = vec![execution_task, insight_task, memory_task, trigger_task];
    for handle in platform_handles.drain(..) {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
            Ok(Ok(())) => tracing::info!("platform task exited cleanly"),
            Ok(Err(e)) => tracing::error!("platform task panicked: {e}"),
            Err(_) => tracing::warn!("platform task shutdown timeout (5s)"),
        }
    }

    tracing::info!("cipher exited cleanly");
    Ok(())
}

pub async fn run_config(
    config_path: PathBuf,
    data_dir_override: Option<PathBuf>,
) -> Result<(), AgentError> {
    init_tracing();

    super::config::migrate_data_dir()?;
    let config = load_config(&config_path, data_dir_override)?;
    let app_state = crate::data::bootstrap(&config.data_dir)?;
    ensure_default_prompts(&config.data_dir)?;
    crate::data::factory::ensure_default_wasm_modules(&config.data_dir)?;
    tracing::info!(data_dir = ?config.data_dir, "config: bootstrap ready");
    crate::data::cognitive_seed::ensure_default_cognitive_seed(&config.data_dir)?;
    crate::startup::config_flow::run(&app_state)?;
    tracing::info!("config: 完成");
    Ok(())
}

pub async fn run_main_loop(
    mode_manager: &mut ModeManager,
    tui: &mut impl UiBackend,
    mut on_config: impl FnMut() -> Result<(), AgentError>,
) -> Result<(), AgentError> {
    tui.show_mode_status(
        mode_manager.current_name(),
        mode_manager.current_mode().render_status().as_str(),
    )
    .await?;
    loop {
        let input = tui.wait_for_input().await?;
        if tui.check_cancel() {
            break;
        }
        if input == "\t" {
            mode_manager.cycle_mode().await?;
            tui.show_mode_status(
                mode_manager.current_name(),
                mode_manager.current_mode().render_status().as_str(),
            )
            .await?;
            continue;
        }
        if input == BACKTAB_SENTINEL {
            mode_manager.cycle_mode_back().await?;
            tui.show_mode_status(
                mode_manager.current_name(),
                mode_manager.current_mode().render_status().as_str(),
            )
            .await?;
            continue;
        }
        if input.is_empty() {
            continue;
        }
        if input == "/exit" || input == "/quit" {
            break;
        }
        if input == "/config" {
            on_config()?;
            continue;
        }
        match mode_manager.handle_input(&input).await {
            Ok(response) => tui.show_response(&response).await?,
            Err(AgentError::ThinkingOutputInvalid(message)) => {
                tracing::warn!("main_loop: thinking output rejected: {message}");
                tui.show_error(&message).await?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub async fn run_streaming_loop(
    mode_manager: &mut ModeManager,
    mut trigger_rx: mpsc::Receiver<crate::agent::agent_pool::channels::TriggerEvent>,
    app: &crate::data::bootstrap::AppState,
    pool: std::sync::Arc<crate::agent::agent_pool::AgentPool>,
    memory_mode_shared: std::sync::Arc<std::sync::Mutex<MemoryMode>>,
) -> Result<(), AgentError> {
    use crossterm::event::{Event, EventStream};
    use futures::StreamExt;
    use std::time::Duration;

    let (mut stream_rx, mut pool_rx) = mode_manager.take_channels();

    let mut state = TuiState::new();
    state.current_mode = mode_manager.current_kind();
    if let Some(name) = load_default_agent_display_name(&app.duckdb) {
        state.agent_name = name;
    }
    let mut guard = StreamingTerminalGuard::new()?;

    let mut pool_state_rx = pool.subscribe_state();
    state.status_line.update(pool.snapshot_detailed().await);

    guard
        .get_mut()
        .draw(|f| crate::ui::tui::render::render(&state, f))
        .map_err(|e| AgentError::Io(format!("initial draw: {e}")))?;

    let mut event_stream = EventStream::new();

    let mut render_tick = time::interval(Duration::from_millis(16));

    let mut pending_settle: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    let mut settle_tick = time::interval(Duration::from_millis(250));

    let mut should_exit = false;
    loop {
        if should_exit {
            break;
        }

        tokio::select! {

            Some(Ok(event)) = event_stream.next() => {
                if let Event::Key(key) = event {

                    if state.mode == TuiMode::Config {

                        let action = state.config_panel.handle_key(key.code);

                        let db_req = state.config_panel.pending_db_request();
                        match db_req {
                            DbRequest::LoadModels => {
                                let reg = load_all_into_memory(&app.duckdb)
                                    .map_err(|e| AgentError::Parse(format!("load models: {e}")))?;
                                state.config_panel.reload_models(reg.models.values().cloned().collect());
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::LoadProviders => {
                                let reg = load_all_into_memory(&app.duckdb)?;
                                let mut providers: Vec<String> = reg.models.values()
                                    .filter(|m| m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false))
                                    .map(|m| m.provider.clone())
                                    .collect();
                                providers.sort(); providers.dedup();
                                if let ConfigView::QuickAddSelectProvider { providers: p, .. } = &mut state.config_panel.view {
                                    *p = providers;
                                }
                            }
                            DbRequest::LoadDefaultCandidates => {
                                let reg = load_all_into_memory(&app.duckdb)?;
                                let candidates: Vec<ModelRow> = reg.models.values()
                                    .filter(|m| m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false))
                                    .cloned().collect();
                                if let ConfigView::SetDefault(sel) = &mut state.config_panel.view {
                                    sel.candidates = candidates;
                                }
                            }
                            DbRequest::SubmitAddModel { provider, api_url, api_type, api_key, name, model_id } => {
                                let row = ModelRow {
                                    id: format!("{}-{}", provider, model_id),
                                    name, provider: provider.clone(),
                                    api_protocol: crate::data::duckdb::loader::default_api_protocol(&api_type),
                                    api_url, api_type, model_id,
                                    api_key: Some(api_key.clone()),
                                    config: None,
                                };
                                match insert_model(&app.duckdb, &row) {
                                    Ok(_) => {
                                        let secret = SecretString::new(api_key);
                                        let n = update_model_api_key_by_provider(&app.duckdb, &provider, &secret)?;
                                        state.config_panel.message = Some((format!("已新增 {} ({} 行 key 同步)", row.id, n), false));
                                        state.config_panel.view = ConfigView::ModelList;
                                    }
                                    Err(e) => state.config_panel.message = Some((format!("新增失败: {e}"), true)),
                                }
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::SubmitQuickAdd { provider, name, model_id } => {

                                let reg = load_all_into_memory(&app.duckdb)?;
                                let sample = reg.models.values()
                                    .find(|m| m.provider == provider && m.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false))
                                    .cloned();
                                match sample {
                                    Some(em) => {
                                        let row = ModelRow {
                                            id: format!("{}-{}", provider, model_id),
                                            name, provider: provider.clone(),
                                            api_protocol: crate::data::duckdb::loader::default_api_protocol(&em.api_type),
                                            api_url: em.api_url, api_type: em.api_type, model_id,
                                            api_key: em.api_key.clone(), config: None,
                                        };
                                        insert_model(&app.duckdb, &row)?;
                                        let n = update_model_api_key_by_provider(&app.duckdb, &provider, &SecretString::new(em.api_key.unwrap()))?;
                                        state.config_panel.message = Some((format!("已快速新增 {} ({} 行同步)", row.id, n), false));
                                        state.config_panel.view = ConfigView::ModelList;
                                    }
                                    None => state.config_panel.message = Some(("找不到该 provider 的带 key 样本行".into(), true)),
                                }
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::SubmitChangeKey { provider, api_key } => {
                                let secret = SecretString::new(api_key);
                                let n = update_model_api_key_by_provider(&app.duckdb, &provider, &secret)?;
                                state.config_panel.message = Some((format!("已 update provider={} 的 {} 行 api_key", provider, n), n > 0));
                                state.config_panel.view = ConfigView::ModelList;
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::SubmitSetDefault { model_id } => {
                                let config_path = crate::startup::Config::default_path();
                                let mut config = crate::startup::init::init(&config_path)?;
                                config.default_model = Some(model_id.clone());
                                config.save(&config_path)?;
                                state.config_panel.message = Some((format!("已切默认模型 → {}", model_id), false));
                                state.config_panel.view = ConfigView::ModelList;
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::SaveMemoryMode { mode } => {
                                let parsed = match mode.as_str() {
                                    "sync" => MemoryMode::Sync,
                                    "async" => MemoryMode::Async,
                                    _ => MemoryMode::Mixed,
                                };


                                if !mode_manager.active_is_empty() {
                                    state.config_panel.message = Some((
                                        format!("拒绝切换: 仍有运行中实例 ({} 个), 请等待完成后再试", mode_manager.active_count()),
                                        true,
                                    ));
                                } else if !matches!(
                                    state.current_mode,
                                    crate::mode_runtime::ModeKind::Unni | crate::mode_runtime::ModeKind::Keep
                                ) {
                                    state.config_panel.message = Some((
                                        "拒绝切换: 仅允许在 UNNI/KEEP 模式下切换 (LOOP 运行中不可切换)".to_string(),
                                        true,
                                    ));
                                } else {
                                    let config_path = crate::startup::Config::default_path();
                                    let mut config = crate::startup::init::init(&config_path)?;
                                    config.memory_mode = parsed;
                                    config.save(&config_path)?;
                                    *memory_mode_shared.lock().unwrap() = parsed;
                                    state.config_panel.message = Some((
                                        format!("记忆中台模式已切换 → {} (运行时生效)", parsed.as_str()),
                                        false,
                                    ));
                                }
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::SubmitRenameAgent { display_name } => {
                                match rename_agent(&app.duckdb, "agent", &display_name) {
                                    Ok(_) => {
                                        state.agent_name = display_name.clone();
                                        state.config_panel.message = Some((
                                            format!("agent 已改名为: {}", display_name),
                                            false,
                                        ));
                                        state.config_panel.view = ConfigView::Menu;
                                        state.config_panel.expanded = None;
                                    }
                                    Err(e) => {
                                        state.config_panel.message = Some((
                                            format!("改名失败: {e}"),
                                            true,
                                        ));
                                    }
                                }
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::None => {}
                        }
                        if matches!(action, ActionResult::Exit) {
                            state.exit_config();
                        }
                        guard
                            .get_mut()
                            .draw(|f| crate::ui::tui::render::render(&state, f))
                            .map_err(|e| AgentError::Io(format!("draw config: {e}")))?;
                        continue;
                    }

                    match key_event_to_action(key) {
                        TuiAction::ForwardTab => {
                            mode_manager.cycle_mode().await?;
                            state.current_mode = mode_manager.current_kind();
                        }
                        TuiAction::BackwardTab => {
                            mode_manager.cycle_mode_back().await?;
                            state.current_mode = mode_manager.current_kind();
                        }
                        TuiAction::Cancel => {
                            mode_manager.cancel_latest_active();
                        }
                        TuiAction::Char(c) => {
                            state.input_push(c);
                        }
                        TuiAction::Backspace => {
                            state.input_backspace();
                        }
                        TuiAction::ScrollUp => {

                            state.scroll_up(15);
                        }
                        TuiAction::ScrollDown => {

                            state.scroll_down(15);
                        }
                        TuiAction::Submit => {
                            let input = state.take_input();
                            if !input.is_empty() {
                                if input == "/exit" || input == "/quit" {
                                    should_exit = true;
                                    continue;
                                }
                                if input == "/config" {
                                    state.enter_config();
                                    guard
                                        .get_mut()
                                        .draw(|f| crate::ui::tui::render::render(&state, f))
                                        .map_err(|e| AgentError::Io(format!("draw config: {e}")))?;
                                    continue;
                                }
                                state.push_user(input.clone());

                                match mode_manager.spawn(input).await {
                                    Ok(id) => {

                                        state.push_streaming(id);
                                    }
                                    Err(e) => {
                                        state.set_error(e.to_string());
                                        tracing::warn!("UI-DEBUG: spawn error: {e}");
                                    }
                                }
                            }
                        }
                        TuiAction::Quit => {
                            should_exit = true;
                        }
                        TuiAction::Ignore => {}
                    }

                    guard
                        .get_mut()
                        .draw(|f| crate::ui::tui::render::render(&state, f))
                        .map_err(|e| AgentError::Io(format!("draw: {e}")))?;
                }
            }


            Some((id, chunk)) = stream_rx.recv() => {
                match chunk {
                    StreamChunk::Delta(text) => {
                        state.append_delta(&id, &text);

                    }
                    StreamChunk::Think(think) => {
                        state.push_think(&id, &think);

                        guard
                            .get_mut()
                            .draw(|f| crate::ui::tui::render::render(&state, f))
                            .map_err(|e| AgentError::Io(format!("draw: {e}")))?;
                    }
                    StreamChunk::Done => {
                        state.finalize_stream(&id);
                        guard
                            .get_mut()
                            .draw(|f| crate::ui::tui::render::render(&state, f))
                            .map_err(|e| AgentError::Io(format!("draw: {e}")))?;
                    }
                    StreamChunk::Cancelled => {
                        state.mark_cancelled(&id);
                        mode_manager.remove_active(&id);
                        guard
                            .get_mut()
                            .draw(|f| crate::ui::tui::render::render(&state, f))
                            .map_err(|e| AgentError::Io(format!("draw: {e}")))?;
                    }
                    StreamChunk::Error(msg) => {
                        state.mark_error(&id, &msg);
                        mode_manager.remove_active(&id);
                        guard
                            .get_mut()
                            .draw(|f| crate::ui::tui::render::render(&state, f))
                            .map_err(|e| AgentError::Io(format!("draw: {e}")))?;
                    }



                    StreamChunk::ToolCallStart { .. } => {}
                    StreamChunk::ToolCallResult { .. } => {}
                }
            }


            Some(outcome) = pool_rx.recv() => {
                mode_manager.bookkeep(outcome, &state.last_user_message());
                guard
                    .get_mut()
                    .draw(|f| crate::ui::tui::render::render(&state, f))
                    .map_err(|e| AgentError::Io(format!("draw: {e}")))?;
            }


            _ = render_tick.tick() => {
                guard
                    .get_mut()
                    .draw(|f| crate::ui::tui::render::render(&state, f))
                    .map_err(|e| AgentError::Io(format!("draw: {e}")))?;
            }


            Some(event) = trigger_rx.recv() => {
                tracing::info!(
                    "streaming_loop: trigger event thought_id={}, reason={}",
                    event.turn_id, event.reason
                );


                let flywheel = matches!(
                    state.current_mode,
                    crate::mode_runtime::ModeKind::Loop
                        | crate::mode_runtime::ModeKind::Keep
                        | crate::mode_runtime::ModeKind::Unni
                );
                let mem_mode = *memory_mode_shared.lock().unwrap();
                match (mem_mode, event.reason.as_str(), flywheel) {

                    (MemoryMode::Sync, "memory_complete", true) => {
                        spawn_flywheel_echo(
                            mode_manager,
                            &mut state,
                            &pool,
                            &event.turn_id,
                            &event.reason,
                        )
                        .await;
                    }

                    (MemoryMode::Async, "insight_complete", true) => {
                        spawn_flywheel_echo(
                            mode_manager,
                            &mut state,
                            &pool,
                            &event.turn_id,
                            &event.reason,
                        )
                        .await;
                    }


                    (MemoryMode::Mixed, "insight_complete", true) => {
                        let settled = pool
                            .get_turn_context(&event.turn_id)
                            .await
                            .is_some_and(|ctx| ctx.memory.is_some());
                        if settled {
                            spawn_flywheel_echo(
                                mode_manager,
                                &mut state,
                                &pool,
                                &event.turn_id,
                                &event.reason,
                            )
                            .await;
                        } else {
                            pending_settle.insert(
                                event.turn_id.clone(),
                                std::time::Instant::now() + SETTLE_TIMEOUT,
                            );
                        }
                    }

                    (MemoryMode::Mixed, "memory_complete", _) => {
                        if pending_settle.remove(&event.turn_id).is_some() {
                            spawn_flywheel_echo(
                                mode_manager,
                                &mut state,
                                &pool,
                                &event.turn_id,
                                &event.reason,
                            )
                            .await;
                        }
                    }
                    _ => {
                        tracing::debug!(
                            "streaming_loop: trigger ignored (mem_mode={mem_mode:?}, reason={}, flywheel={flywheel})",
                            event.reason
                        );
                    }
                }
            }


            _ = settle_tick.tick() => {
                if pending_settle.is_empty() {
                    continue;
                }



                if !matches!(
                    state.current_mode,
                    crate::mode_runtime::ModeKind::Loop
                        | crate::mode_runtime::ModeKind::Keep
                        | crate::mode_runtime::ModeKind::Unni
                ) {
                    if !pending_settle.is_empty() {
                        tracing::info!(
                            "streaming_loop: dropped {} pending settle(s) — not in KEEP/LOOP flywheel",
                            pending_settle.len()
                        );
                        pending_settle.clear();
                    }
                    continue;
                }
                let now = std::time::Instant::now();
                let due: Vec<String> = pending_settle
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in due {
                    pending_settle.remove(&id);
                    tracing::info!(
                        "streaming_loop: mixed-mode settle timeout for thought_id={id}, spawning with settled portion"
                    );
                    spawn_flywheel_echo(mode_manager, &mut state, &pool, &id, "settle_timeout").await;
                }
            }


            Ok(()) = pool_state_rx.changed() => {
                let snapshot = pool_state_rx.borrow_and_update().clone();
                state.status_line.update(snapshot);
            }
        }
    }
    Ok(())
}

const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 构造 echo 轮注入摘要（纯函数, 便于单测）:
/// 执行结果段逐节点带 summary（失败附 error）, 记忆沉淀段带注意力 content（200 截断）, 整体 4000 兜底。
fn build_echo_summary(
    ctx: &crate::agent::communication::TurnContext,
    turn_id: &str,
    reason: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("既定目标: {}", ctx.thinking.goal));
    if let Some(exec) = &ctx.execution {
        let node_lines: Vec<String> = exec
            .node_results
            .iter()
            .map(|n| match &n.error {
                Some(err) => format!("{}: 失败[{}]", n.node_id, err),
                None => format!("{}: {}", n.node_id, n.summary),
            })
            .collect();
        parts.push(format!(
            "执行结果: {:?}, 节点明细 [{}]",
            exec.status,
            node_lines.join("; ")
        ));
    }
    if let Some(ins) = &ctx.insight {
        parts.push(format!(
            "洞察: 越界={} 需跟进={}",
            ins.insight.boundary_check.crossed, ins.insight.needs_followup
        ));
    }
    if let Some(mem) = &ctx.memory {
        if !mem.attention.is_empty() {
            let lines: Vec<String> = mem
                .attention
                .iter()
                .map(|a| {
                    format!(
                        "{}: {}",
                        a.focus,
                        crate::common::json_util::truncate_head_tail(&a.content, 200)
                    )
                })
                .collect();
            parts.push(format!(
                "记忆沉淀: 新增注意力 {} 条 [{}]",
                mem.attention.len(),
                lines.join("; ")
            ));
        }
        if !mem.experience.is_empty() {
            parts.push(format!("记忆沉淀: 新增经验 {} 条", mem.experience.len()));
        }
    }
    parts.push(format!(
        "记忆中台已整理上一轮 (thought_id={turn_id}, reason={reason}). 请基于此继续推进目标."
    ));
    crate::common::json_util::truncate_head_tail(&parts.join("\n"), 4000)
}

async fn spawn_flywheel_echo(
    mode_manager: &mut ModeManager,
    state: &mut TuiState,
    pool: &std::sync::Arc<crate::agent::agent_pool::AgentPool>,
    turn_id: &str,
    reason: &str,
) {
    if state.current_mode == crate::mode_runtime::ModeKind::Keep {
        let say_consumed = mode_manager.keep_say_quota_consumed();
        let no_exec_intent = pool.get_turn_context(turn_id).await.is_some_and(|ctx| {
            ctx.thinking.decision != crate::agent::communication::ThinkDecision::Execute
        });
        if say_consumed && no_exec_intent {
            tracing::info!(
                "streaming_loop: KEEP period finished (final report), \
                 flywheel stops for thought_id={turn_id}"
            );
            return;
        }
    }
    if state.current_mode == crate::mode_runtime::ModeKind::Unni {
        let period_done = pool
            .get_turn_context(turn_id)
            .await
            .is_some_and(|ctx| ctx.input_kind == "echo" && ctx.say_published);
        if period_done {
            tracing::info!(
                "streaming_loop: UNNI period finished (echo round reported via say), \
                 flywheel stops for thought_id={turn_id}"
            );
            return;
        }
    }

    let echo_ctx = pool.get_turn_context(turn_id).await;
    let summary = match &echo_ctx {
        Some(ctx) => build_echo_summary(ctx, turn_id, reason),
        None => format!(
            "记忆中台已整理上一轮 (thought_id={turn_id}, reason={reason}). 请基于此继续推进目标."
        ),
    };

    match mode_manager
        .spawn_with_override(
            summary.clone(),
            Some(crate::agent::thought::ThinkingInput::PlatformEcho {
                platform: crate::agent::thought::InternalPlatform::Memory,
                summary,
                artifact_refs: vec![],
            }),
        )
        .await
    {
        Ok(id) => state.push_streaming(id),
        Err(e) => state.set_error(e.to_string()),
    }
}

struct StreamingTerminalGuard {
    terminal: Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>,
}

impl StreamingTerminalGuard {
    fn new() -> Result<Self, AgentError> {
        use crossterm::execute;
        use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
        use ratatui::backend::CrosstermBackend;
        use ratatui::Terminal;

        enable_raw_mode().map_err(|e| AgentError::Io(format!("enable_raw_mode: {e}")))?;
        let mut stdout = std::io::stdout();
        match execute!(stdout, EnterAlternateScreen) {
            Ok(_) => {}
            Err(e) => {
                let _ = crossterm::terminal::disable_raw_mode();
                return Err(AgentError::Io(format!("EnterAlternateScreen: {e}")));
            }
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(t) => Ok(Self { terminal: Some(t) }),
            Err(e) => {
                let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                let _ = crossterm::terminal::disable_raw_mode();
                Err(AgentError::Io(format!("Terminal::new: {e}")))
            }
        }
    }

    fn get_mut(
        &mut self,
    ) -> &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>> {
        self.terminal
            .as_mut()
            .expect("StreamingTerminalGuard already dropped")
    }
}

impl Drop for StreamingTerminalGuard {
    fn drop(&mut self) {
        use crossterm::execute;
        use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
        drop(self.terminal.take());
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

fn print_already_configured() {
    let v = env!("CARGO_PKG_VERSION");
    println!();
    println!("    cipher v{v} — 终端原生 AI 代理");
    println!();
    println!("    模型已配置, 无需重新初始化。");
    println!("    直接运行 `cipher` 进入 TUI, 或 `cipher config` 管理配置。");
    println!();
    println!("    进入 TUI 后:");
    println!("      Tab / Shift+Tab   切换模式 (UNNI / KEEP / LOOP)");
    println!("      /config           打开配置管理 (改模型 / 切默认)");
    println!("      /exit              退出");
    println!();
}

fn print_welcome_and_help() {
    let v = env!("CARGO_PKG_VERSION");
    println!();
    println!("    cipher v{v} — 终端原生 AI 代理");
    println!("    首次启动");
    println!();
    println!("    欢迎！检测到尚未配置模型, 接下来引导你完成首次配置。");
    println!("    依次: 选模型模板 → 填 model_id / api_key → ping 验证。");
    println!("    (配置失败会要求重填, 无逃生口; 随时可 Ctrl+C 退出)");
    println!();
    println!("    进入 TUI 后:");
    println!("      Tab / Shift+Tab   切换模式 (UNNI / KEEP / LOOP)");
    println!("      /config           打开配置管理 (改模型 / 切默认)");
    println!("      /exit              退出");
    println!();
}

fn load_default_agent_display_name(conn: &duckdb::Connection) -> Option<String> {
    conn.query_row(
        "SELECT COALESCE(display_name, name) FROM agent WHERE is_default = true LIMIT 1",
        [],
        |row| row.get(0),
    )
    .ok()
}

#[cfg(test)]
mod prompt_install_tests {
    use super::*;

    #[test]
    fn fresh_install_writes_every_factory_prompt() {
        let temporary = tempfile::tempdir().unwrap();

        ensure_default_prompts(temporary.path()).unwrap();

        for (name, expected) in crate::logic::model::prompts::DEFAULT_PROMPTS {
            assert_eq!(
                std::fs::read_to_string(temporary.path().join("prompts").join(name)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn matching_legacy_factory_prompt_is_upgraded() {
        let temporary = tempfile::tempdir().unwrap();
        let prompts_dir = temporary.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let legacy = b"synthetic legacy factory prompt";
        std::fs::write(prompts_dir.join("system.md"), legacy).unwrap();
        let legacy_hash = sha256_bytes(legacy);

        ensure_default_prompts_with_legacy_hashes(
            temporary.path(),
            &[("system.md", legacy_hash.as_str())],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(prompts_dir.join("system.md")).unwrap(),
            crate::logic::model::prompts::SYSTEM_DEFAULT
        );
    }

    #[test]
    fn customized_and_unrelated_existing_prompts_are_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let prompts_dir = temporary.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("mode_loop.md"), "custom loop contract").unwrap();
        std::fs::write(prompts_dir.join("local_notes.md"), "keep this too").unwrap();

        ensure_default_prompts(temporary.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(prompts_dir.join("mode_loop.md")).unwrap(),
            "custom loop contract"
        );
        assert_eq!(
            std::fs::read_to_string(prompts_dir.join("local_notes.md")).unwrap(),
            "keep this too"
        );
    }
}

#[cfg(test)]
mod echo_summary_tests {
    use super::*;
    use crate::agent::communication::{
        AttentionFragment, ExecutionOutput, ExecutionStatus, ExperienceFragment, InsightOutput,
        MemoryOutput, NodeResult, NodeStatus, ThinkDecision, ThinkingOutput, TurnContext,
        TurnStatus,
    };

    fn node_result(id: &str, summary: &str, error: Option<&str>) -> NodeResult {
        NodeResult {
            node_id: id.into(),
            status: if error.is_some() {
                NodeStatus::Failed
            } else {
                NodeStatus::Completed
            },
            summary: summary.into(),
            error: error.map(str::to_string),
            tool_call_count: 1,
            tool_call_logs: vec![],
        }
    }

    fn turn_context(
        execution: Option<ExecutionOutput>,
        memory: Option<MemoryOutput>,
    ) -> TurnContext {
        TurnContext {
            turn_id: "t1".into(),
            thinking: ThinkingOutput {
                decision: ThinkDecision::Execute,
                goal: "统计 ERROR 总数".into(),
                constraints: vec![],
                message: String::new(),
            },
            execution,
            insight: Some(InsightOutput {
                insight: crate::agent::communication::InsightResult {
                    boundary_check: crate::agent::communication::BoundaryCheck {
                        crossed: false,
                        violations: vec![],
                        analysis: String::new(),
                    },
                    goal_alignment: crate::agent::communication::GoalAlignment {
                        aligned: true,
                        deviation: None,
                        analysis: String::new(),
                    },
                    growth_check: crate::agent::communication::GrowthCheck {
                        growth_detected: false,
                        growth_type: None,
                        analysis: String::new(),
                    },
                    needs_followup: false,
                    followup_hint: None,
                },
                tool_memory: vec![],
            }),
            memory,
            status: TurnStatus::Memorizing,
            user_message: String::new(),
            input_kind: "echo".into(),
            say_published: true,
        }
    }

    #[test]
    fn echo_summary_includes_node_summaries_and_attention_content() {
        let ctx = turn_context(
            Some(ExecutionOutput {
                dag: crate::agent::communication::ExecutionDag::Single {
                    template_kind: "normal".into(),
                    capability_ids: vec!["shell.exec".into()],
                    task_context: String::new(),
                },
                node_results: vec![
                    node_result("n1", "ERROR 计数: a=3, b=2, c=2", None),
                    node_result("n2", "counted all", Some("文件缺失")),
                ],
                status: ExecutionStatus::PartialFailure,
            }),
            Some(MemoryOutput {
                attention: vec![AttentionFragment {
                    focus: "ERROR统计结果-a.log".into(),
                    content: "logs/a.log 中 ERROR 出现 3 次".into(),
                }],
                experience: vec![ExperienceFragment {
                    title: "经验1".into(),
                    summary: "s".into(),
                }],
                preference: vec![],
                cognitive: vec![],
            }),
        );
        let summary = build_echo_summary(&ctx, "t1", "echo");
        assert!(
            summary.contains("n1: ERROR 计数: a=3, b=2, c=2"),
            "{summary}"
        );
        assert!(summary.contains("n2: 失败[文件缺失]"), "{summary}");
        assert!(
            summary.contains("logs/a.log 中 ERROR 出现 3 次"),
            "{summary}"
        );
        assert!(summary.contains("ERROR统计结果-a.log"), "{summary}");
        assert!(summary.contains("新增经验 1 条"), "{summary}");
        assert!(summary.contains("既定目标: 统计 ERROR 总数"), "{summary}");
    }

    #[test]
    fn echo_summary_empty_context_does_not_panic() {
        let ctx = turn_context(None, None);
        let summary = build_echo_summary(&ctx, "t1", "echo");
        assert!(summary.contains("既定目标: 统计 ERROR 总数"), "{summary}");
        assert!(
            summary.contains("记忆中台已整理上一轮 (thought_id=t1, reason=echo)"),
            "{summary}"
        );
        assert!(!summary.contains("节点明细"), "{summary}");
    }

    #[test]
    fn echo_summary_truncates_overlong_total() {
        let ctx = turn_context(
            Some(ExecutionOutput {
                dag: crate::agent::communication::ExecutionDag::Single {
                    template_kind: "normal".into(),
                    capability_ids: vec!["shell.exec".into()],
                    task_context: String::new(),
                },
                node_results: vec![node_result("n1", &"x".repeat(5000), None)],
                status: ExecutionStatus::Success,
            }),
            None,
        );
        let summary = build_echo_summary(&ctx, "t1", "echo");
        assert!(
            summary.contains("truncated"),
            "must carry truncation marker"
        );
        assert!(summary.contains("请基于此继续推进目标"), "{summary}");
        assert!(
            summary.chars().count() < 4000 + 64,
            "summary must stay near 4000-char budget, got {}",
            summary.chars().count()
        );
    }
}

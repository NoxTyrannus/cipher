use super::config::{CollaborationStyle, Config, ModeStyles, TriggerNode};
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

const LEGACY_DEFAULT_PROMPT_SHA256: [(&str, &str); 23] = [
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
    // v0.2.6 出厂版本（本轮 v0.3.0 提示词重构前的旧默认值）
    (
        "system.md",
        "0b58bb08ab4401da0dc577cd56c357ffc81bf1d3c3406c5eb112111d4230ebf3",
    ),
    (
        "mode_unni.md",
        "225b9b8f80749d27296664df11eeee2dbf9b17179c00d812f1d5b4c0b38b5e0a",
    ),
    (
        "mode_keep.md",
        "8f109418b5bf46df6fa5b851386a6a68411137c0a0e93a83cb50b67de66631b6",
    ),
    (
        "mode_loop.md",
        "32e5818ede42ea5fff031a9d2f27ddb2fb4fd4aad818c02882a4de70e4c6c2c8",
    ),
    // v0.3.0 出厂版本（v0.3.1 上下文工程分层重构前）
    (
        "system.md",
        "706cc23d73b26e3dd3a85c9df036c03f2d238b1953ad0b0e9d3ad69121e5685a",
    ),
    (
        "mode_unni.md",
        "ffc113372e325745ab307fd31c8588fda21630ce425d6b6b4c2b3c13b3edb1a4",
    ),
    (
        "mode_keep.md",
        "e0ba5abeef3e0e00f2f35c2fa6f49cf4d7a91171c22e2e7809610a0bf2f35a48",
    ),
    (
        "mode_loop.md",
        "16d403e3c612cc01cb422ab6a40972bdcaebfba5ee17fa3ce73e98a3705bf4a1",
    ),
    // v0.3.1 三模式提示词最小化前
    (
        "mode_unni.md",
        "543379fde888c5797ff825f766631d091dc6db9f4479af9ca25a78ef663514e0",
    ),
    (
        "mode_keep.md",
        "2c65fe42e7fab4874da52d35c4a61ce34b8781a2ae1e61d69e6acda7612f421f",
    ),
    (
        "mode_loop.md",
        "1e9ac1d869ad03697a429c256ccc0a793240571bf6092b3a63d7c4990ed42c3d",
    ),
    (
        "execution_platform.md",
        "05a9fb25d879b533b3bb9386a2d5969a2328141adfd99b20e542ed68a939902b",
    ),
    (
        "insight_platform.md",
        "17dafb1fbec2dd44f7ab7fe4aeae31c73b417cfd9ee607661c8fc73b9bfcda8e",
    ),
    (
        "execution_platform.md",
        "b823ae4de996af91ff4e3ea3cadbcf62574a53c2677f1e3282788c15da08cb7f",
    ),
    (
        "insight_platform.md",
        "12f80b0dbd7af353874a781235ed018d7f595dd943ca12210d2bddaf7f556ca8",
    ),
    (
        "memory_attention.md",
        "6ad9bb4811b7c859b620daf515f64434e8aa9d63fea3084cd93474d30f0d5148",
    ),
    (
        "memory_experience.md",
        "ba41710538354e05e791034907c14e0b851a8cab6412bb57b5e90e4e1dcdd264",
    ),
    (
        "memory_preference.md",
        "fc6b7fca92ba9da97847d367275fe869a15809da0197718ac8dbb01afdfe7e81",
    ),
    (
        "memory_cognitive.md",
        "61a00a1086ae32931b54b5b14c952c303dc2a7effd9f6cc9f200b30961524e74",
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
    crate::data::cognitive_seed::ensure_default_cognitive_seed(&config.data_dir)?;
    tracing::info!(data_dir = ?config.data_dir, "setup: bootstrap ready");
    if has_configured_model(&app_state.duckdb)? {
        print_already_configured();
    } else {
        print_welcome_and_help();
        crate::startup::init_flow::init_flow(&app_state, &config.data_dir).await?;
    }
    crate::data::cognitive_seed::ensure_default_capabilities(&config.data_dir)?;
    crate::data::cognitive_seed::import_factory_defaults(&app_state.duckdb, &config.data_dir)?;
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
    let mut app_state = crate::data::bootstrap(&config.data_dir)?;
    ensure_default_prompts(&config.data_dir)?;
    tracing::info!(data_dir = ?config.data_dir, "normal: bootstrap ready");
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
    let trivium_db = std::sync::Arc::new(std::sync::Mutex::new(
        crate::data::triviumdb::TriviumDb::open(
            &triviumdb_path,
            crate::data::triviumdb::DEFAULT_DIM,
        )
        .map_err(|e| AgentError::Bootstrap(format!("TriviumDB open: {e}")))?,
    ));

    {
        let mut db_guard = trivium_db.lock().map_err(|e| {
            AgentError::Bootstrap(format!("TriviumDB lock for cognitive seed: {e}"))
        })?;
        crate::data::cognitive_seed::seed_cognitive_memory(&config.data_dir, &mut db_guard)
            .map_err(|e| AgentError::Bootstrap(format!("cognitive seed: {e}")))?;
    }

    crate::data::cognitive_seed::ensure_default_capabilities(&config.data_dir)?;
    crate::data::cognitive_seed::import_factory_defaults(&app_state.duckdb, &config.data_dir)?;

    // 关键：能力/agent 种子在 import_factory_defaults 中才写入 DuckDB，
    // 而 `app_state.registry` 在 bootstrap() 时已加载（早于种子）。
    // 必须在此重载，执行平台/记忆 agent 才能拿到最新 tool_caps 与能力行。
    app_state.registry = crate::data::duckdb::loader::load_all_into_memory(&app_state.duckdb)?;

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

    let shared_thought_store = std::sync::Arc::new(
        crate::data::thought_store::ThoughtStore::open(app_state.paths.thoughts_data_root())
            .map_err(|e| AgentError::Bootstrap(format!("ThoughtStore open: {e}")))?,
    );

    let exec_registry = Some(app_state.registry.clone());
    let exec_executor = {
        let mut ex = crate::logic::capability::executor::CapabilityExecutor::new();
        ex.set_workspace_root(&std::env::current_dir().unwrap_or_default());
        ex.set_triviumdb(std::sync::Arc::clone(&trivium_db));
        ex.set_thought_store(std::sync::Arc::clone(&shared_thought_store));
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
    let memory_executor =
        std::sync::Arc::clone(exec_executor.as_ref().expect("executor configured"));
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

    let memory_registry = Some(app_state.registry.clone());
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
        let exp_registry = memory_registry.clone();
        let exp_executor = std::sync::Arc::clone(&memory_executor);
        tokio::spawn(async move {
            let agent = crate::agent::memory::experience_agent::ExperienceMemoryAgent::new(
                exp_provider,
                exp_model,
                Some(exp_api_key),
                Some(exp_trivium),
                exp_prompts,
                experience_rx,
                exp_registry,
                Some(exp_executor),
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
        let pref_registry = memory_registry.clone();
        let pref_executor = std::sync::Arc::clone(&memory_executor);
        tokio::spawn(async move {
            let agent = crate::agent::memory::preference_agent::PreferenceMemoryAgent::new(
                pref_provider,
                pref_model,
                Some(pref_api_key),
                Some(pref_trivium),
                pref_prompts,
                preference_rx,
                pref_registry,
                Some(pref_executor),
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
        let cog_thought_store = std::sync::Arc::clone(&shared_thought_store);
        let cog_registry = memory_registry.clone();
        let cog_executor = std::sync::Arc::clone(&memory_executor);
        tokio::spawn(async move {
            let agent = crate::agent::memory::cognitive_agent::CognitiveAgent::new(
                cog_provider,
                cog_model,
                Some(cog_api_key),
                Some(cog_trivium),
                cog_prompts,
                cognitive_rx,
                Some(cog_memory_db),
                Some(cog_thought_store),
                cog_registry,
                Some(cog_executor),
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
            memory_registry,
            Some(memory_executor),
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
    let thought_store = std::sync::Arc::clone(&shared_thought_store);
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

        let mode_styles_shared = std::sync::Arc::new(std::sync::Mutex::new(config.mode_styles));
        let keep_budget_tracker =
            std::sync::Arc::new(std::sync::Mutex::new(KeepBudgetTracker::new(
                config.mode_styles.keep.token_budget,
                config.mode_styles.keep.time_budget_secs,
            )));
        run_streaming_loop(
            &mut mode_manager,
            trigger_fwd_rx,
            &app_state,
            pool_for_status,
            mode_styles_shared,
            keep_budget_tracker,
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
    tracing::info!(data_dir = ?config.data_dir, "config: bootstrap ready");
    crate::data::cognitive_seed::ensure_default_cognitive_seed(&config.data_dir)?;
    crate::data::cognitive_seed::ensure_default_capabilities(&config.data_dir)?;
    crate::data::cognitive_seed::import_factory_defaults(&app_state.duckdb, &config.data_dir)?;
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
    mode_styles_shared: std::sync::Arc<std::sync::Mutex<ModeStyles>>,
    keep_budget_tracker: std::sync::Arc<std::sync::Mutex<KeepBudgetTracker>>,
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

    let mut should_exit = false;

    let mut mix_state = MixThinkingState::default();

    // Mix join（缺陷2）：依赖实例注册表 + 待推进状态，事件驱动不阻塞主循环。
    let mix_registry = MixDepRegistry::default();
    let mut mix_join: Option<PendingMix> = None;
    let mut final_wanted = false;

    // 自动修复（F1）：按用户输入原文计数的防循环上限。
    let mut auto_repair_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
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
                            DbRequest::SaveModeStyle { target, value } => {
                                let config_path = crate::startup::Config::default_path();
                                let mut config = crate::startup::init::init(&config_path)?;
                                let mut styles = *mode_styles_shared.lock().unwrap();
                                let msg = match target {
                                    0 => {
                                        let next = match value.as_str() {
                                            "follow" => CollaborationStyle::Follow,
                                            _ => CollaborationStyle::Autonomous,
                                        };
                                        config.mode_styles.unni.style = next;
                                        styles.unni.style = next;
                                        format!("UNNI 协同方式已切换 → {:?}", next)
                                    }
                                    1 => {
                                        let next = match value.as_str() {
                                            "insight" => TriggerNode::Insight,
                                            "memory" => TriggerNode::Memory,
                                            _ => TriggerNode::Execution,
                                        };
                                        config.mode_styles.unni.node = next;
                                        styles.unni.node = next;
                                        format!("UNNI 协同节点已切换 → {:?}", next)
                                    }
                                    2 => {
                                        let budget = value.parse::<u64>().unwrap_or(100_000).max(100_000);
                                        config.mode_styles.keep.token_budget = budget;
                                        styles.keep.token_budget = budget;
                                        // 同步运行时预算追踪器（保持周期内已用 token 不回退）
                                        keep_budget_tracker.lock().unwrap().token_budget = budget;
                                        format!("KEEP Token 预算已切换 → {}K", budget / 1000)
                                    }
                                    3 => {
                                        let secs = value.parse::<u64>().unwrap_or(300).max(300);
                                        config.mode_styles.keep.time_budget_secs = secs;
                                        styles.keep.time_budget_secs = secs;
                                        keep_budget_tracker.lock().unwrap().time_budget_secs = secs;
                                        format!("KEEP 时间预算已切换 → {}min", secs / 60)
                                    }
                                    _ => {
                                        let on = value == "on";
                                        config.mode_styles.r#loop.mix_thinking = on;
                                        styles.r#loop.mix_thinking = on;
                                        format!("LOOP 融合思考已切换 → {}", if on { "开" } else { "关" })
                                    }
                                };
                                config.save(&config_path)?;
                                *mode_styles_shared.lock().unwrap() = styles;
                                state.config_panel.message = Some((msg, false));
                                state.config_panel.view = ConfigView::ModeStyleSubMenu { cursor: 0 };
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

                                // 跟随模式：合并 pending context（协同节点完成后的摘要）进本次输入
                                let spawn_input = if let Some((pending_turn, pending_summary)) =
                                    state.take_pending_context()
                                {
                                    tracing::info!(
                                        "streaming_loop: merging pending context (thought_id={pending_turn}) into user input"
                                    );
                                    format!(
                                        "[上一轮整理上下文 (thought_id={pending_turn})]\n{pending_summary}\n\n——用户新输入——\n{input}"
                                    )
                                } else {
                                    input
                                };

                                match mode_manager.spawn(spawn_input).await {
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
                    StreamChunk::Status(msg) => {
                        // 过程状态（如思考请求指数退避重试进度）：消息面板错误条暴露
                        state.set_error(msg.clone());
                        tracing::info!("streaming_loop: status (thought_id={id}): {msg}");
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
                // Mix 依赖注册表更新（缺陷2）：实例结果到达 → 状态更新 + 事件驱动推进 join
                let oid = outcome.id.clone();
                let outcome_ok = outcome.result.is_ok();
                let outcome_err = outcome.result.as_ref().err().map(|e| e.to_string());
                tracing::debug!(
                    "streaming_loop: pool outcome id={oid} ok={outcome_ok} join={mix_join:?}"
                );
                mix_registry.on_outcome(&oid, outcome_ok, outcome_err);
                try_progress_mix(
                    mode_manager, &mut state, &pool, &mut mix_state,
                    &mix_registry, &mut mix_join, &mut final_wanted,
                )
                .await;

                // F1: invalid_json 重试耗尽后保留用户输入意图，自动续跑修复轮（不回到询问状态）。
                let repair_hint = match &outcome.result {
                    Err(AgentError::ThinkingOutputInvalid(msg)) => Some(msg.clone()),
                    _ => None,
                };
                let failed_id = outcome.id.clone();
                mode_manager.bookkeep(outcome, &state.last_user_message());

                if let Some(failure_msg) = repair_hint {
                    // 内部实例（融合思考反思/echo 轮）失败不触发自动修复——
                    // 它们不是用户可见的请求轮，自动重试会打乱 Mix 状态机。
                    let failed_ctx = pool.get_turn_context(&failed_id).await;
                    let is_internal = failed_ctx
                        .as_ref()
                        .is_some_and(|c| c.input_kind == "reflect" || c.input_kind == "echo");
                    let original = failed_ctx
                        .map(|c| c.user_message)
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| state.last_user_message());
                    if is_internal {
                        tracing::debug!(
                            "streaming_loop: internal instance ({failed_id}) failed with invalid output — skipped auto-repair"
                        );
                    } else {
                        let count = auto_repair_counts.entry(original.clone()).or_insert(0u32);
                        if *count < MAX_AUTO_REPAIRS {
                            *count += 1;
                            tracing::info!(
                                "streaming_loop: auto-repair round {count}/{MAX_AUTO_REPAIRS} for user intent (failed instance {failed_id})",
                            );
                            let repair_input = format!(
                                "[自动修复] 你上一轮对用户请求的输出格式无效，未获得执行授权。\n\
                                 用户请求: {original}\n\
                                 失败原因: {failure_msg}\n\
                                 请重新思考并输出符合格式要求的回复，继续完成用户请求。"
                            );
                            match mode_manager.spawn(repair_input).await {
                                Ok(id) => state.push_streaming(id),
                                Err(e) => state.set_error(e.to_string()),
                            }
                        } else {
                            tracing::warn!(
                                "streaming_loop: auto-repair exhausted for user intent (failed instance {failed_id}), giving up to avoid loop",
                            );
                        }
                    }
                }
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

                // 统一触发调度（分组 C）：
                // 1. 当前模式 → style（UNNI 用用户配置, KEEP/LOOP 固定）
                // 2. 事件 reason → platform
                // 3. 仅当 platform == 协同节点时触发/暂存；协同节点后的异步中台只沉淀不触发。
                let mode_name = mode_manager.current_name().to_ascii_lowercase();
                let styles = *mode_styles_shared.lock().unwrap();
                let style = styles.style_for(&mode_name);

                let platform = match event.reason.as_str() {
                    "execution_complete" => Some(TriggerNode::Execution),
                    "insight_complete" => Some(TriggerNode::Insight),
                    "memory_complete" => Some(TriggerNode::Memory),
                    _ => None,
                };
                let Some(platform) = platform else {
                    tracing::debug!(
                        "streaming_loop: trigger ignored (unknown reason={})",
                        event.reason
                    );
                    continue;
                };

                // LOOP + 融合思考（Mix Thinking）on：三阶段流水线并行 + 拼接合并。
                //
                // 轮结构（每个执行实例 = 一轮 think_0 及其后继）：
                //   execution_complete(T) → 实例1 (ReflectOnly, 执行反思)
                //   insight_complete(T)   → 实例2 (ReflectOnly, 洞察反思, 拼接实例1)
                //   memory_complete(T)    → 实例3 (PlatformEcho, 记忆综合, 拼接实例1+2) = 下一轮 think_0
                //
                // 并行：实例1 spawn 时洞察中台已被 ExecutionDone 并行驱动；实例2 同理。
                // join：事件到达时对应中台结果已写入 ctx；实例1/2 的 think 从 store 读取。
                // 反思实例（ReflectOnly）think 后不执行、不驱动中台，故不会产生新完成事件。
                let mix_on = mode_name == "loop"
                    && styles.r#loop.mix_thinking
                    && style.node == TriggerNode::Memory;
                if mix_on {
                    match platform {
                        TriggerNode::Execution => {
                            // 新一轮开始（执行实例完成）
                            mix_state.begin_round(&event.turn_id);
                            // 防御：上一轮 final 尚未 spawn 时不会到达这里（顺序保证），
                            // 若异常残留则清空 pending，避免跨轮错配。
                            mix_join = None;
                            final_wanted = false;
                            let summary = mix_summary(&pool, &event.turn_id, None, None, &event.reason).await;
                            let new_id = spawn_mix_reflect(
                                mode_manager, &mut state, &summary,
                            )
                            .await;
                            if let Some(id1) = &new_id {
                                mix_state.set_reflect1(Some(id1.clone()), &event.turn_id);
                                mix_registry.register(id1.clone());
                            }
                        }
                        TriggerNode::Insight => {
                            if mix_state.is_current_round(&event.turn_id) {
                                // join：等实例1 就绪后 spawn 实例2（事件驱动，不阻塞）
                                if let Some(r1) = mix_state.reflect1() {
                                    mix_join = Some(PendingMix::AwaitReflect1 {
                                        base: event.turn_id.clone(),
                                        reflect1: r1.to_string(),
                                    });
                                }
                                try_progress_mix(
                                    mode_manager, &mut state, &pool, &mut mix_state,
                                    &mix_registry, &mut mix_join, &mut final_wanted,
                                )
                                .await;
                            } else {
                                tracing::debug!(
                                    "streaming_loop: mix insight_complete for non-current round ({}) ignored",
                                    event.turn_id
                                );
                            }
                        }
                        TriggerNode::Memory => {
                            if mix_state.is_current_round(&event.turn_id) {
                                // join：base 记忆完成，等实例1+2 就绪后 spawn final（事件驱动，不阻塞）
                                final_wanted = true;
                                try_progress_mix(
                                    mode_manager, &mut state, &pool, &mut mix_state,
                                    &mix_registry, &mut mix_join, &mut final_wanted,
                                )
                                .await;
                            } else {
                                tracing::debug!(
                                    "streaming_loop: mix memory_complete for non-current round ({}) ignored",
                                    event.turn_id
                                );
                            }
                        }
                    }
                    continue;
                }

                if platform == style.node {
                    // 协同节点完成 → 按协同方式处理
                    match style.style {
                        CollaborationStyle::Autonomous => {
                            // KEEP 预算检查：预算耗尽则暂停（不 spawn）
                            if mode_name == "keep" {
                                let mut tracker = keep_budget_tracker.lock().unwrap();
                                if !keep_budget_allows(&mut tracker, &mut state, &event.turn_id) {
                                    continue;
                                }
                                tracker.record_instance();
                            }
                            spawn_flywheel_echo(
                                mode_manager,
                                &mut state,
                                &pool,
                                &event.turn_id,
                                &event.reason,
                            )
                            .await;
                        }
                        CollaborationStyle::Follow => {
                            // 跟随：暂存为 pending context，等用户下次输入合并
                            let ctx = pool.get_turn_context(&event.turn_id).await;
                            let summary = match &ctx {
                                Some(c) => build_echo_summary(c, &event.turn_id, &event.reason),
                                None => summary_closing(&event.turn_id, &event.reason),
                            };
                            state.stash_pending_context(&event.turn_id, &summary);
                        }
                    }
                } else if style.node.async_after().contains(&platform) {
                    // 协同节点后的异步中台：只沉淀记忆，不触发新实例
                    tracing::info!(
                        "streaming_loop: async platform {platform:?} after trigger node {:?} — \
                         only sinking memory, not triggering (thought_id={})",
                        style.node, event.turn_id
                    );
                } else {
                    // 协同节点前的中台：忽略
                    tracing::debug!(
                        "streaming_loop: trigger ignored (platform {platform:?} before trigger node {:?})",
                        style.node
                    );
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
    parts.push(summary_closing(turn_id, reason));
    crate::common::json_util::truncate_head_tail(&parts.join("\n"), 4000)
}

/// 摘要结尾文案，按触发原因动态生成（兼容 Mix Thinking 各阶段）。
fn summary_closing(turn_id: &str, reason: &str) -> String {
    match reason {
        "execution_complete" => {
            format!("执行已完成 (thought_id={turn_id}). 请基于执行结果做一轮反思，不重复执行.")
        }
        "insight_complete" => {
            format!("洞察已完成 (thought_id={turn_id}). 请结合执行+洞察结果做一轮反思，不重复执行.")
        }
        "memory_complete" => {
            format!("记忆中台已整理上一轮 (thought_id={turn_id}). 请基于此继续推进目标.")
        }
        _ => format!("上一轮已整理 (thought_id={turn_id}, reason={reason}). 请基于此继续推进目标."),
    }
}

/// KEEP 周期是否应停止飞轮续跑。
///
/// - say 配额已用（KEEP 周期内成功 say 过一次，用于目标对齐）
/// - 且当前轮决策既不是执行（Execute）也不是失败（Failure）
///
/// 失败轮（think 解析失败）不属于"任务结束"信号，应继续修复续跑。
fn should_stop_keep_flywheel(
    decision: Option<crate::agent::communication::ThinkDecision>,
    say_consumed: bool,
) -> bool {
    if !say_consumed {
        return false;
    }
    decision.is_some_and(|d| {
        d != crate::agent::communication::ThinkDecision::Execute
            && d != crate::agent::communication::ThinkDecision::Failure
    })
}

/// KEEP 预算追踪器：Token 累计（按每实例估值）+ 时间累计。
///
/// - `token_budget`：KEEP 周期内累计输出 token 上限（默认 100K）
/// - `time_budget_secs`：KEEP 周期内累计执行时间上限（默认 5min）
/// - 每实例 token 估值 `ESTIMATED_TOKENS_PER_INSTANCE`（8K），因为 provider usage
///   未贯通到调度层；时间按周期起点精确计时。
/// - 周期起点由 KEEP 首次触发时置为 now；`token_exceeded`/`time_exceeded` 任一为真
///   表示预算耗尽（暂停 + 提示）。
#[derive(Debug, Clone)]
pub struct KeepBudgetTracker {
    token_budget: u64,
    time_budget_secs: u64,
    tokens_used: u64,
    period_started_at: Option<std::time::Instant>,
}

/// 每实例输出 token 估值（KEEP 飞轮轮次的近似值）。
const ESTIMATED_TOKENS_PER_INSTANCE: u64 = 8_000;

/// F1: invalid_json 自动修复轮上限（按用户输入原文计数，防无限循环）。
const MAX_AUTO_REPAIRS: u32 = 2;

impl KeepBudgetTracker {
    pub fn new(token_budget: u64, time_budget_secs: u64) -> Self {
        Self {
            token_budget,
            time_budget_secs,
            tokens_used: 0,
            period_started_at: None,
        }
    }

    /// 开始一个 KEEP 周期（首次飞轮触发时调用）。
    pub fn start_period(&mut self) {
        if self.period_started_at.is_none() {
            self.period_started_at = Some(std::time::Instant::now());
        }
    }

    /// 记录一实例的 token 消耗。
    pub fn record_instance(&mut self) {
        self.start_period();
        self.tokens_used = self
            .tokens_used
            .saturating_add(ESTIMATED_TOKENS_PER_INSTANCE);
    }

    /// Token 预算是否耗尽。
    pub fn token_exceeded(&self) -> bool {
        self.tokens_used >= self.token_budget
    }

    /// 时间预算是否耗尽。
    pub fn time_exceeded(&self) -> bool {
        self.period_started_at
            .map(|t| t.elapsed().as_secs() >= self.time_budget_secs)
            .unwrap_or(false)
    }

    /// 预算是否耗尽（任一维度）。
    pub fn exceeded(&self) -> bool {
        self.token_exceeded() || self.time_exceeded()
    }

    /// 状态摘要（供 UI 提示）。
    pub fn status(&self) -> String {
        let time = self
            .period_started_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        format!(
            "KEEP 预算: token {}/{}K, 时间 {}s/{}s",
            self.tokens_used / 1000,
            self.token_budget / 1000,
            time,
            self.time_budget_secs
        )
    }
}

/// 通过 KEEP 预算检查？预算耗尽则暂停（不 spawn）+ 提示。
fn keep_budget_allows(
    tracker: &mut KeepBudgetTracker,
    state: &mut TuiState,
    turn_id: &str,
) -> bool {
    if tracker.exceeded() {
        tracing::info!(
            "streaming_loop: KEEP budget exhausted, pausing flywheel for thought_id={turn_id} ({})",
            tracker.status()
        );
        state.set_error(format!("KEEP 预算已耗尽, 周期暂停. {}", tracker.status()));
        return false;
    }
    true
}

/// 融合思考（Mix Thinking）轮状态机：
/// 当前轮由执行实例（base_turn）驱动，记录其反思实例（实例1/2）的 turn_id，
/// 供下一阶段拼接读取 think 输出。
#[derive(Debug, Default)]
struct MixThinkingState {
    base_turn: Option<String>,
    reflect1: Option<String>,
    reflect2: Option<String>,
}

impl MixThinkingState {
    /// 执行实例完成 → 新一轮开始（若 turn 不同）。
    fn begin_round(&mut self, turn_id: &str) {
        if self.base_turn.as_deref() != Some(turn_id) {
            self.base_turn = Some(turn_id.to_string());
            self.reflect1 = None;
            self.reflect2 = None;
        }
    }

    fn is_current_round(&self, turn_id: &str) -> bool {
        self.base_turn.as_deref() == Some(turn_id)
    }

    fn set_reflect1(&mut self, id: Option<String>, turn_id: &str) {
        if self.is_current_round(turn_id) {
            self.reflect1 = id;
        }
    }

    fn set_reflect2(&mut self, id: Option<String>, turn_id: &str) {
        if self.is_current_round(turn_id) {
            self.reflect2 = id;
        }
    }

    fn reflect1(&self) -> Option<&str> {
        self.reflect1.as_deref()
    }

    #[allow(dead_code)] // 仅测试使用（生产路径由 PendingMix 持有实例2 id）
    fn reflect2(&self) -> Option<&str> {
        self.reflect2.as_deref()
    }

    /// 实例3 = 下一轮 think_0：成为新的 base_turn，反思位清空。
    fn advance_round(&mut self, new_base: Option<String>) {
        self.base_turn = new_base;
        self.reflect1 = None;
        self.reflect2 = None;
    }
}

/// Mix 依赖实例（实例1/2）状态。
#[derive(Debug, Clone)]
enum MixDepState {
    /// 已 spawn，等待结果（含实例内部指数退避重试中）
    Running,
    /// think 已落库（可从 pool 读取）
    Ready,
    /// 永久失败（错误已暴露，摘要缺段继续，不中断）
    Permanent(String),
}

/// Mix 依赖注册表：turn_id → 状态，配合 Notify 做事件驱动唤醒（替代轮询）。
#[derive(Default)]
struct MixDepRegistry {
    inner: std::sync::Mutex<std::collections::HashMap<String, MixDepState>>,
    notify: tokio::sync::Notify,
}

impl MixDepRegistry {
    fn register(&self, id: String) {
        self.inner.lock().unwrap().insert(id, MixDepState::Running);
    }

    fn state(&self, id: &str) -> MixDepState {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or(MixDepState::Permanent(format!("未注册的依赖实例 {id}")))
    }

    /// 主循环处理完一个实例结果后调用；仅更新已注册的 mix 依赖并唤醒等待者。
    fn on_outcome(&self, id: &str, ok: bool, err: Option<String>) {
        let mut m = self.inner.lock().unwrap();
        if !m.contains_key(id) {
            return;
        }
        m.insert(
            id.to_string(),
            if ok {
                MixDepState::Ready
            } else {
                MixDepState::Permanent(err.unwrap_or_else(|| "未知错误".to_string()))
            },
        );
        drop(m);
        self.notify.notify_waiters();
    }
}

/// Mix join 待推进状态：事件驱动、不阻塞主循环。
///
/// - `AwaitReflect1`：实例2 尚未 spawn，等实例1 就绪（或永久失败）后 spawn 实例2；
/// - `AwaitFinal`：实例2 已 spawn，等实例1+2 就绪（或永久失败）后 spawn final。
#[derive(Debug, Clone)]
enum PendingMix {
    AwaitReflect1 {
        base: String,
        reflect1: String,
    },
    AwaitFinal {
        base: String,
        reflect1: String,
        reflect2: String,
    },
}

/// 推进 Mix join：依赖就绪/永久失败即前进（spawn 实例2 / spawn final）。
///
/// 事件驱动：每次实例结果到达或阶段触发后调用；依赖仍在跑（含退避重试）时立即返回，
/// 不阻塞主循环——依赖的结果到达后会再次进入本函数。
#[allow(clippy::too_many_arguments)]
async fn try_progress_mix(
    mode_manager: &mut ModeManager,
    state: &mut TuiState,
    pool: &std::sync::Arc<crate::agent::agent_pool::AgentPool>,
    mix_state: &mut MixThinkingState,
    registry: &MixDepRegistry,
    mix_join: &mut Option<PendingMix>,
    final_wanted: &mut bool,
) {
    loop {
        let Some(pending) = mix_join.clone() else {
            return;
        };
        tracing::debug!("try_progress_mix: pending={pending:?} final_wanted={final_wanted}");
        match pending {
            PendingMix::AwaitReflect1 { base, reflect1 } => {
                let s1 = registry.state(&reflect1);
                match &s1 {
                    MixDepState::Running => return, // 等实例1（含退避重试中）的结果
                    MixDepState::Permanent(err) => {
                        tracing::warn!(
                            "streaming_loop: mix dep reflect1 ({reflect1}) permanent failed: {err}"
                        );
                        state.set_error(format!("反思实例1 永久失败（{err}），实例2 将缺该段继续"));
                    }
                    MixDepState::Ready => {}
                }
                let r1_for_summary = matches!(s1, MixDepState::Ready).then(|| reflect1.clone());
                let summary = mix_summary(
                    pool,
                    &base,
                    r1_for_summary.as_deref(),
                    None,
                    "insight_complete",
                )
                .await;
                let new_id = spawn_mix_reflect(mode_manager, state, &summary).await;
                if let Some(id2) = &new_id {
                    mix_state.set_reflect2(Some(id2.clone()), &base);
                    registry.register(id2.clone());
                }
                *mix_join = Some(PendingMix::AwaitFinal {
                    base,
                    reflect1,
                    reflect2: new_id.unwrap_or_default(),
                });
                // 继续循环：若 reflect2 已就绪，则顺势推进 final
            }
            PendingMix::AwaitFinal {
                base,
                reflect1,
                reflect2,
            } => {
                // final 的输入含 base 轮的记忆摘要 → 必须等 memory_complete 到达后再拼
                if !*final_wanted {
                    return;
                }
                let s1 = registry.state(&reflect1);
                let s2 = registry.state(&reflect2);
                if matches!(s1, MixDepState::Running) || matches!(s2, MixDepState::Running) {
                    return; // 还有依赖在跑（含退避重试中）
                }
                if let MixDepState::Permanent(err) = &s1 {
                    tracing::warn!(
                        "streaming_loop: mix dep reflect1 ({reflect1}) permanent failed: {err}"
                    );
                    state.set_error(format!("反思实例1 永久失败（{err}），final 将缺该段继续"));
                }
                if let MixDepState::Permanent(err) = &s2 {
                    tracing::warn!(
                        "streaming_loop: mix dep reflect2 ({reflect2}) permanent failed: {err}"
                    );
                    state.set_error(format!("反思实例2 永久失败（{err}），final 将缺该段继续"));
                }
                let r1 = matches!(s1, MixDepState::Ready).then(|| reflect1.clone());
                let r2 = matches!(s2, MixDepState::Ready).then(|| reflect2.clone());
                let summary =
                    mix_summary(pool, &base, r1.as_deref(), r2.as_deref(), "memory_complete").await;
                let new_id = spawn_mix_final(mode_manager, state, &summary).await;
                // 实例3 = 下一轮 think_0：成为新的 base_turn，反思位清空
                mix_state.advance_round(new_id);
                *mix_join = None;
                *final_wanted = false;
                return;
            }
        }
    }
}

/// 融合思考拼接：把当前轮中台结果 + 实例1/2 的 think 输出拼进下一实例输入。
async fn mix_summary(
    pool: &std::sync::Arc<crate::agent::agent_pool::AgentPool>,
    turn_id: &str,
    reflect1: Option<&str>,
    reflect2: Option<&str>,
    reason: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ctx) = pool.get_turn_context(turn_id).await {
        parts.push(build_echo_summary(&ctx, turn_id, reason));
    }
    for (label, id) in [("实例1 反思", reflect1), ("实例2 反思", reflect2)] {
        if let Some(id) = id {
            if let Some(ctx) = pool.get_turn_context(id).await {
                let think = ctx.thinking.message.clone();
                if !think.trim().is_empty() {
                    parts.push(format!("[{label} (thought_id={id})]\n{think}"));
                }
            }
        }
    }
    if parts.is_empty() {
        parts.push(summary_closing(turn_id, reason));
    }
    crate::common::json_util::truncate_head_tail(&parts.join("\n\n"), 4000)
}

/// spawn 融合思考中间反思实例（ReflectOnly：think 后不执行）。
async fn spawn_mix_reflect(
    mode_manager: &mut ModeManager,
    state: &mut TuiState,
    summary: &str,
) -> Option<String> {
    match mode_manager
        .spawn_with_override(
            summary.to_string(),
            Some(crate::agent::thought::ThinkingInput::ReflectOnly {
                summary: summary.to_string(),
            }),
        )
        .await
    {
        Ok(id) => {
            state.push_streaming(id.clone());
            Some(id)
        }
        Err(e) => {
            state.set_error(e.to_string());
            None
        }
    }
}

/// spawn 融合思考最终实例（PlatformEcho：think 后执行 = 下一轮 think_0）。
async fn spawn_mix_final(
    mode_manager: &mut ModeManager,
    state: &mut TuiState,
    summary: &str,
) -> Option<String> {
    match mode_manager
        .spawn_with_override(
            summary.to_string(),
            Some(crate::agent::thought::ThinkingInput::PlatformEcho {
                platform: crate::agent::thought::InternalPlatform::Memory,
                summary: summary.to_string(),
                artifact_refs: vec![],
            }),
        )
        .await
    {
        Ok(id) => {
            state.push_streaming(id.clone());
            Some(id)
        }
        Err(e) => {
            state.set_error(e.to_string());
            None
        }
    }
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
        let decision = pool
            .get_turn_context(turn_id)
            .await
            .map(|ctx| ctx.thinking.decision);
        if should_stop_keep_flywheel(decision, say_consumed) {
            tracing::info!(
                "streaming_loop: KEEP period finished (final report), \
                 flywheel stops for thought_id={turn_id}"
            );
            return;
        }
    }
    if state.current_mode == crate::mode_runtime::ModeKind::Unni {
        let ctx = pool.get_turn_context(turn_id).await;
        let period_done = ctx
            .as_ref()
            .is_some_and(|c| c.input_kind == "echo" && c.say_published);
        let say_only_user_round = ctx.as_ref().is_some_and(|c| {
            c.thinking.decision == crate::agent::communication::ThinkDecision::Reply
                && c.input_kind == "user"
        });
        if period_done || say_only_user_round {
            tracing::info!(
                "streaming_loop: UNNI period finished (say-only user round or echo reported), \
                 flywheel stops for thought_id={turn_id}"
            );
            return;
        }
    }

    let echo_ctx = pool.get_turn_context(turn_id).await;
    let summary = match &echo_ctx {
        Some(ctx) => build_echo_summary(ctx, turn_id, reason),
        None => summary_closing(turn_id, reason),
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

    use crate::agent::communication::ThinkDecision as D;

    #[test]
    fn keep_flywheel_failure_decision_does_not_stop() {
        assert!(
            !should_stop_keep_flywheel(Some(D::Failure), true),
            "失败轮(think 解析失败)不应停止 KEEP 飞轮, 应继续修复续跑"
        );
        assert!(
            !should_stop_keep_flywheel(Some(D::Execute), true),
            "执行轮不应停止"
        );
        assert!(
            !should_stop_keep_flywheel(Some(D::Execute), false),
            "say 未消耗时不应停止"
        );
    }

    #[test]
    fn keep_flywheel_stops_only_on_non_exec_intent_after_say() {
        assert!(
            should_stop_keep_flywheel(Some(D::Reply), true),
            "say 已用 + 非执行/非失败决策 → 停止"
        );
        assert!(
            should_stop_keep_flywheel(Some(D::Inherit), true),
            "say 已用 + Inherit → 停止"
        );
        assert!(
            should_stop_keep_flywheel(Some(D::Cancel), true),
            "say 已用 + Cancel → 停止"
        );
        assert!(
            !should_stop_keep_flywheel(None, true),
            "上下文缺失(None) → 保守不停止, 避免临时丢失终止循环"
        );
        assert!(
            !should_stop_keep_flywheel(Some(D::Reply), false),
            "say 未消耗 → 不停止"
        );
    }

    #[test]
    fn mix_state_tracks_rounds_and_reflect_ids() {
        let mut st = MixThinkingState::default();
        assert!(!st.is_current_round("t0"));

        st.begin_round("t0");
        assert!(st.is_current_round("t0"));

        st.set_reflect1(Some("r1".into()), "t0");
        assert_eq!(st.reflect1(), Some("r1"));

        st.set_reflect2(Some("r2".into()), "t0");
        assert_eq!(st.reflect2(), Some("r2"));

        // 新一轮开始（不同 base_turn）→ 反思位清空
        st.begin_round("t1");
        assert!(st.is_current_round("t1"));
        assert_eq!(st.reflect1(), None);
        assert_eq!(st.reflect2(), None);

        // advance_round：实例3 成为新 base_turn
        st.set_reflect1(Some("r1".into()), "t1");
        st.advance_round(Some("final1".into()));
        assert!(st.is_current_round("final1"));
        assert_eq!(st.reflect1(), None);
        assert_eq!(st.reflect2(), None);
    }

    #[test]
    fn mix_state_ignores_stale_round_writes() {
        let mut st = MixThinkingState::default();
        st.begin_round("t0");
        st.set_reflect1(Some("r1".into()), "t0");
        // 旧轮 turn 写入被忽略
        st.set_reflect2(Some("stale".into()), "old");
        assert_eq!(st.reflect2(), None);
        assert!(!st.is_current_round("old"));
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
                    source_refs: vec![],
                }],
                experience: vec![ExperienceFragment {
                    title: "经验1".into(),
                    summary: "s".into(),
                    source_refs: vec![],
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
        let summary = build_echo_summary(&ctx, "t1", "memory_complete");
        assert!(summary.contains("既定目标: 统计 ERROR 总数"), "{summary}");
        assert!(
            summary.contains("记忆中台已整理上一轮 (thought_id=t1)"),
            "{summary}"
        );
        assert!(!summary.contains("节点明细"), "{summary}");
    }

    #[test]
    fn summary_closing_adapts_to_reason() {
        let exec = summary_closing("t1", "execution_complete");
        assert!(exec.contains("执行已完成"), "{exec}");
        assert!(exec.contains("不重复执行"), "{exec}");

        let insight = summary_closing("t1", "insight_complete");
        assert!(insight.contains("洞察已完成"), "{insight}");
        assert!(insight.contains("执行+洞察"), "{insight}");

        let mem = summary_closing("t1", "memory_complete");
        assert!(mem.contains("记忆中台已整理上一轮"), "{mem}");
        assert!(mem.contains("继续推进目标"), "{mem}");
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

    #[test]
    fn keep_budget_defaults_allow_start() {
        let mut t = KeepBudgetTracker::new(100_000, 300);
        assert!(!t.exceeded());
        assert!(keep_budget_allows(&mut t, &mut TuiState::new(), "t1"));
    }

    #[test]
    fn keep_budget_token_exhausts_after_instances() {
        // 每实例估值 8K, 预算 24K → 第 3 次 record 后耗尽
        let mut t = KeepBudgetTracker::new(24_000, 300);
        t.record_instance(); // 8K
        t.record_instance(); // 16K
        assert!(!t.token_exceeded());
        t.record_instance(); // 24K ≥ 24K
        assert!(t.token_exceeded());
        assert!(t.exceeded());
    }

    #[test]
    fn keep_budget_status_reports_usage() {
        let mut t = KeepBudgetTracker::new(100_000, 300);
        t.record_instance();
        let s = t.status();
        assert!(s.contains("token 8/100K"), "got: {s}");
        assert!(s.contains("300s"), "time budget visible: {s}");
    }
}

use super::config::{Config, RuntimeStyles, UnniStyle};
use super::{init, self_check};
use crate::agent::context_assembler::{ContextAssembler, ContextConfig};
use crate::common::AgentError;
use crate::data::duckdb::loader::{
    count_models, delete_model, has_configured_model, insert_model, load_all_into_memory,
    rename_agent, update_model_api_key_by_provider, ModelRow,
};
use crate::logic::model::stream::StreamChunk;
use crate::mode_runtime::ModeManager;
use crate::startup::manifest::{self, UpgradeChoice};
use crate::ui::backend::UiBackend;
use crate::ui::tui::config_panel::{ActionResult, ConfigView, DbRequest};
use crate::ui::tui::event::{key_event_to_action, TuiAction, BACKTAB_SENTINEL};
use crate::ui::tui::state::{TuiMessage, TuiMode, TuiState};
use secrecy::SecretString;
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::time;
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cipher");
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

#[allow(dead_code)]
fn ensure_default_prompts(unified_root: &Path) -> Result<(), AgentError> {
    // 非交互安全模式：用户改过的文件默认跳过（保留用户内容）。
    let report = manifest::upgrade_prompts(
        unified_root,
        &crate::logic::model::prompts::DEFAULT_PROMPTS,
        |_| UpgradeChoice::Cancel,
    )
    .map_err(|e| AgentError::Io(format!("ensure_default_prompts: {e}")))?;
    if !report.skipped.is_empty() {
        tracing::info!(
            "ensure_default_prompts: skipped user-modified files: {:?}",
            report.skipped
        );
    }
    Ok(())
}

fn ensure_default_prompts_interactive(unified_root: &Path) -> Result<(), AgentError> {
    let defaults = crate::logic::model::prompts::DEFAULT_PROMPTS;
    // 如果没有 manifest，先全新安装（不会询问）。
    if !unified_root.join("manifest.json").exists() {
        manifest::ensure_fresh_install(unified_root, &defaults)
            .map_err(|e| AgentError::Io(format!("fresh install: {e}")))?;
        return Ok(());
    }

    let report = manifest::upgrade_prompts(unified_root, &defaults, |name| {
        let items = vec![
            "备份后升级".to_string(),
            "销毁旧文件升级".to_string(),
            "取消（保留用户文件）".to_string(),
        ];
        let sel = dialoguer::Select::new()
            .with_prompt(format!("提示词 {name} 已被用户修改，如何处理？"))
            .items(&items)
            .default(0)
            .interact()
            .unwrap_or(2);
        match sel {
            0 => UpgradeChoice::Backup,
            1 => UpgradeChoice::Overwrite,
            _ => UpgradeChoice::Cancel,
        }
    })
    .map_err(|e| AgentError::Io(format!("upgrade prompts: {e}")))?;

    if !report.skipped.is_empty() {
        tracing::info!(
            "ensure_default_prompts_interactive: skipped {:?}",
            report.skipped
        );
    }
    Ok(())
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
    let unified_root = crate::startup::manifest::unified_root();
    ensure_default_prompts_interactive(&unified_root)?;
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
    crate::data::cognitive_seed::upgrade_seed_deltas(&app_state.duckdb, &config.data_dir)?;
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
    let unified_root = crate::startup::manifest::unified_root();
    ensure_default_prompts_interactive(&unified_root)?;
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
    let insight_provider = std::sync::Arc::clone(&exec_provider);
    let memory_provider = std::sync::Arc::clone(&exec_provider);
    let memory_api_key = exec_api_key.clone();
    let prompts_dir = Some(unified_root.join("prompts"));

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
    crate::data::cognitive_seed::upgrade_seed_deltas(&app_state.duckdb, &config.data_dir)?;

    // 关键：能力/agent 种子在 import_factory_defaults 中才写入 DuckDB，
    // 而 `app_state.registry` 在 bootstrap() 时已加载（早于种子）。
    // 必须在此重载，执行平台/记忆 agent 才能拿到最新 capability_allowlist 与能力行。
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
    let exec_prompts_dir = prompts_dir.clone();

    let shared_thought_store = std::sync::Arc::new(
        crate::data::thought_store::ThoughtStore::open(app_state.paths.thoughts_data_root())
            .map_err(|e| AgentError::Bootstrap(format!("ThoughtStore open: {e}")))?,
    );

    let exec_duckdb_conn = std::sync::Arc::new(std::sync::Mutex::new(
        duckdb::Connection::open(app_state.paths.duckdb()).map_err(|e| {
            AgentError::Bootstrap(format!("open DuckDB for execution runtime: {e}"))
        })?,
    ));
    let exec_registry = Some(app_state.registry.clone());
    let exec_duckdb = Some(std::sync::Arc::clone(&exec_duckdb_conn));
    let exec_storage_root = Some(app_state.paths.storage_root().to_path_buf());
    let exec_executor = {
        let mut ex = crate::logic::capability::executor::CapabilityExecutor::new();
        ex.set_workspace_root(&std::env::current_dir().unwrap_or_default());
        // v0.4.8：[fs] read_roots 追加文件读根（缺省空=仅 workspace_root，行为与现状一致）。
        ex.set_extra_read_roots(&config.fs.read_roots);
        ex.set_triviumdb(std::sync::Arc::clone(&trivium_db));
        ex.set_thought_store(std::sync::Arc::clone(&shared_thought_store));
        ex.set_duckdb(std::sync::Arc::clone(&exec_duckdb_conn));
        ex.set_storage_root(app_state.paths.storage_root());
        // v0.4.6 web.fetch.public 域名白名单（[web] allowed_domains，缺省空=拒绝全部）。
        ex.set_web_allowed_domains(config.web.allowed_domains.clone());
        std::sync::Arc::new(ex)
    };

    // 安装 subagent runtime：TA 分子通过 executor hook 通知，TB RuntimeSpawnHook 负责
    // resolve model -> resolve api key -> pick provider -> spawn async runtime。
    {
        let finish_duckdb = std::sync::Arc::clone(&exec_duckdb_conn);
        let finish_pool = std::sync::Arc::clone(&pool);
        let finish_storage_root = app_state.paths.storage_root().to_path_buf();
        let finish: std::sync::Arc<
            dyn Fn(&crate::agent::subagent_runtime::SubagentFinish) + Send + Sync,
        > = std::sync::Arc::new(
            move |finish: &crate::agent::subagent_runtime::SubagentFinish| {
                let lifecycle = match finish.outcome {
                    crate::agent::subagent_runtime::SubagentOutcome::Done { .. } => {
                        crate::agent::execution_types::SubagentLifecycle::Idle
                    }
                    crate::agent::subagent_runtime::SubagentOutcome::Failed { .. } => {
                        crate::agent::execution_types::SubagentLifecycle::Failed
                    }
                };
                let (final_state, error) = match &finish.outcome {
                    crate::agent::subagent_runtime::SubagentOutcome::Done { .. } => {
                        ("completed", None)
                    }
                    crate::agent::subagent_runtime::SubagentOutcome::Failed { reason } => {
                        ("failed", Some(reason.as_str()))
                    }
                };
                if let Ok(conn) = finish_duckdb.lock() {
                    if let Err(e) = crate::agent::subagent_capability::set_subagent_lifecycle(
                        &conn,
                        &finish.subagent_id,
                        lifecycle,
                    ) {
                        tracing::warn!("subagent finish: set_subagent_lifecycle failed: {e}");
                    }
                }
                if let Err(e) = crate::agent::subagent_capability::close_subagent_invocation(
                    &finish_storage_root,
                    &finish.invocation_id,
                    final_state,
                    error,
                ) {
                    tracing::warn!("subagent finish: close_subagent_invocation failed: {e}");
                }
                let subagent_id = finish.subagent_id.clone();
                let finish_pool = finish_pool.clone();
                let runtime = tokio::runtime::Handle::try_current();
                if let Ok(runtime) = runtime {
                    runtime.spawn(async move {
                        finish_pool
                            .update_subagent_lifecycle(&subagent_id, lifecycle)
                            .await;
                    });
                }
            },
        );

        let subagent_provider_registry =
            crate::startup::init_flow::build_provider_registry(&default_model)?;
        let runtime_hook = crate::agent::subagent_runtime::RuntimeSpawnHook::new(
            std::sync::Arc::clone(&pool),
            app_state.registry.clone(),
            Some(std::sync::Arc::clone(&exec_duckdb_conn)),
            std::sync::Arc::clone(&exec_executor),
            app_state.paths.storage_root().to_path_buf(),
            subagent_provider_registry,
            None,
            finish,
        );
        let bridge = crate::agent::execution_platform::SubagentRuntimeBridge::new(runtime_hook);
        exec_executor.set_subagent_spawn_hook(std::sync::Arc::new(bridge));
    }

    let memory_executor = std::sync::Arc::clone(&exec_executor);
    let execution_task = tokio::spawn(async move {
        crate::agent::execution_platform::run(
            pool_exec,
            receivers.execution_rx,
            exec_provider_clone,
            exec_model,
            exec_api_key,
            exec_prompts_dir,
            exec_registry,
            Some(exec_executor),
            exec_duckdb,
            exec_storage_root,
            config.execution.merge_enabled,
        )
        .await;
    });

    let (capability_memory_tx, capability_memory_rx) = mpsc::channel::<String>(64);

    let pool_insight = std::sync::Arc::clone(&pool);
    let insight_model = default_model.clone();
    let insight_prompts_dir = prompts_dir.clone();
    let insight_storage_root = Some(app_state.paths.storage_root().to_path_buf());
    let cm_provider = std::sync::Arc::clone(&insight_provider);
    let cm_model = default_model.clone();
    let cm_api_key = insight_api_key.clone();
    let cm_registry = app_state.registry.clone();
    let cm_executor = std::sync::Arc::clone(&memory_executor);
    let insight_task = tokio::spawn(async move {
        crate::agent::insight_platform::run(
            pool_insight,
            receivers.insight_rx,
            insight_provider,
            insight_model,
            insight_api_key,
            capability_memory_tx,
            insight_prompts_dir,
            insight_storage_root,
            config.insight.merge_enabled,
        )
        .await;
    });

    // 洞察域常驻节点：能力记忆 agent（滑动窗口，工具=usage_method.observe）。
    tokio::spawn(async move {
        crate::agent::insight_capability_memory::CapabilityMemoryAgent::new(
            cm_provider,
            cm_model,
            cm_api_key,
            cm_registry,
            cm_executor,
            capability_memory_rx,
        )
        .run()
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

        let exp_registry = memory_registry.clone();
        let exp_executor = std::sync::Arc::clone(&memory_executor);
        tokio::spawn(async move {
            let agent = crate::agent::memory::experience_agent::ExperienceMemoryAgent::new(
                exp_provider,
                exp_model,
                Some(exp_api_key),
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

        let pref_registry = memory_registry.clone();
        let pref_executor = std::sync::Arc::clone(&memory_executor);
        tokio::spawn(async move {
            let agent = crate::agent::memory::preference_agent::PreferenceMemoryAgent::new(
                pref_provider,
                pref_model,
                Some(pref_api_key),
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
            config.memory.merge_enabled,
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
            // v0.4.9 P2 退出关断：trigger 任务收到 Shutdown 后 break 自然退出。
            if event.shutdown {
                tracing::info!("trigger_receiver: received Shutdown, exiting");
                break;
            }
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
    assembler.set_agent_pool(std::sync::Arc::clone(&pool));

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
    let mode_styles_shared =
        std::sync::Arc::new(std::sync::Mutex::new(RuntimeStyles::from_config(&config)));
    let keep_budget_tracker = std::sync::Arc::new(std::sync::Mutex::new(KeepBudgetTracker::new(
        config.mode_styles.keep.token_budget,
        config.mode_styles.keep.time_budget_secs,
    )));
    crate::startup::self_monitor::spawn_self_monitor(
        unified_root.clone(),
        mode_styles_shared.clone(),
        keep_budget_tracker.clone(),
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
            crate::startup::config_flow::run(&app_state)?;
            if let Ok(Some(cfg)) = Config::load(&Config::default_path()) {
                *mode_styles_shared.lock().unwrap() = RuntimeStyles::from_config(&cfg);
                let mut tracker = keep_budget_tracker.lock().unwrap();
                tracker.set_token_budget(cfg.mode_styles.keep.token_budget);
                tracker.set_time_budget_secs(cfg.mode_styles.keep.time_budget_secs);
            }
            Ok(())
        })
        .await?;
    }

    // v0.4.9 P2：退出时给四平台任务发关断信号（关闭三中台 + trigger 通道），
    // 使其 `rx.recv()` 返回 None 自然退出，消除/显著减少下面 5s 超时等待；
    // 下方 5s 超时保留作为兜底（关断失败时不阻塞退出）。
    mode_manager.shutdown_channels();

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
    let unified_root = crate::startup::manifest::unified_root();
    ensure_default_prompts_interactive(&unified_root)?;
    tracing::info!(data_dir = ?config.data_dir, "config: bootstrap ready");
    crate::data::cognitive_seed::ensure_default_cognitive_seed(&config.data_dir)?;
    crate::data::cognitive_seed::ensure_default_capabilities(&config.data_dir)?;
    crate::data::cognitive_seed::import_factory_defaults(&app_state.duckdb, &config.data_dir)?;
    crate::data::cognitive_seed::upgrade_seed_deltas(&app_state.duckdb, &config.data_dir)?;
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
    mode_styles_shared: std::sync::Arc<std::sync::Mutex<RuntimeStyles>>,
    keep_budget_tracker: std::sync::Arc<std::sync::Mutex<KeepBudgetTracker>>,
) -> Result<(), AgentError> {
    use crossterm::event::{Event, EventStream};
    use futures::StreamExt;
    use std::time::Duration;

    let (mut stream_rx, mut pool_rx) = mode_manager.take_channels();

    let mut state = TuiState::new();
    state.current_mode = mode_manager.current_kind();
    // v0.4.6 think 显示开关：全局 [ui] show_think + UNNI per-mode 覆盖（经 RuntimeStyles 快照）。
    let styles = *mode_styles_shared.lock().unwrap();
    state.ui_show_think = styles.ui_show_think;
    state.unni_show_think = styles.unni_show_think;
    if let Some(name) = load_default_agent_display_name(&app.duckdb) {
        state.agent_name = name;
    }
    // v0.4.9 退出快照恢复：TuiState 初始化后、主循环前注入「上次中断」消息（失败→空启动）。
    restore_snapshot(&mut state, app.paths.storage_root());
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
                            DbRequest::SubmitSetDefault { model_id } => {
                                let config_path = crate::startup::Config::default_path();
                                let mut config = crate::startup::init::init(&config_path)?;
                                config.default_model = Some(model_id.clone());
                                config.save(&config_path)?;
                                state.config_panel.message = Some((format!("已切默认模型 → {}", model_id), false));
                                state.config_panel.view = ConfigView::ModelList;
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::DeleteModel { model_id } => {
                                let config_path = crate::startup::Config::default_path();
                                let mut config = crate::startup::init::init(&config_path)?;
                                let total = count_models(&app.duckdb)?;
                                if total <= 1 {
                                    state.config_panel.message = Some((
                                        "至少保留一个模型，当前仅剩 1 个模型，不能删除".to_string(),
                                        true,
                                    ));
                                } else {
                                    let was_default = config.default_model.as_deref() == Some(model_id.as_str());
                                    match delete_model(&app.duckdb, &model_id) {
                                        Ok(_) => {
                                            let mut msg = format!("已删除模型 {model_id}");
                                            if was_default {
                                                let reg = load_all_into_memory(&app.duckdb)?;
                                                let next_default = reg.models.values().next().map(|model| model.id.clone());
                                                config.default_model = next_default;
                                                config.save(&config_path)?;
                                                if let Some(default_id) = &config.default_model {
                                                    msg.push_str(&format!("；默认模型已切换 → {default_id}"));
                                                }
                                            }
                                            let reg = load_all_into_memory(&app.duckdb)?;
                                            state.config_panel.reload_models(reg.models.values().cloned().collect());
                                            state.config_panel.message = Some((msg, false));
                                        }
                                        Err(error) => {
                                            state.config_panel.message = Some((format!("删除失败: {error}"), true));
                                        }
                                    }
                                }
                                state.config_panel.view = ConfigView::ModelList;
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::SaveModeStyle { target, value } => {
                                let config_path = crate::startup::Config::default_path();
                                let mut config = crate::startup::init::init(&config_path)?;
                                let mut styles = *mode_styles_shared.lock().unwrap();
                                // 放弃项 1/2/10：协同节点固定洞察 + mix 机制整体删除，
                                // 模式设置仅保留 KEEP 预算（target 0=Token，1=时间）。
                                let msg = match target {
                                    0 => {
                                        // 0=无限；非 0 时 clamp 到默认最小值 100K。
                                        let budget = normalize_keep_token_budget(&value);
                                        config.mode_styles.keep.token_budget = budget;
                                        styles.keep.token_budget = budget;
                                        // 同步运行时预算追踪器（保持周期内已用 token 不回退）
                                        keep_budget_tracker.lock().unwrap().token_budget = budget;
                                        if budget == 0 {
                                            "KEEP Token 预算已切换 → 无限 (0)".to_string()
                                        } else {
                                            format!("KEEP Token 预算已切换 → {}K", budget / 1000)
                                        }
                                    }
                                    1 => {
                                        // 0=无限；非 0 时 clamp 到默认最小值 5min。
                                        let secs = normalize_keep_time_budget_secs(&value);
                                        config.mode_styles.keep.time_budget_secs = secs;
                                        styles.keep.time_budget_secs = secs;
                                        keep_budget_tracker.lock().unwrap().time_budget_secs = secs;
                                        if secs == 0 {
                                            "KEEP 时间预算已切换 → 无限 (0)".to_string()
                                        } else {
                                            format!("KEEP 时间预算已切换 → {}min", secs / 60)
                                        }
                                    }
                                    _ => "未知模式设置项".to_string(),
                                };
                                config.save(&config_path)?;
                                *mode_styles_shared.lock().unwrap() = styles;
                                state.config_panel.message = Some((msg, false));
                                state.config_panel.view = ConfigView::ModeStyleSubMenu { cursor: 0 };
                                state.config_panel.clear_db_request();
                            }
                            DbRequest::SaveShowThink { show } => {
                                let config_path = crate::startup::Config::default_path();
                                let mut config = crate::startup::init::init(&config_path)?;
                                // 只写 [mode_styles.unni] show_think：菜单不暴露全局 [ui] show_think，
                                // 全局字段保留在 config/渲染层（程序默认 true、手动高级配置可用）。
                                config
                                    .mode_styles
                                    .unni
                                    .get_or_insert_with(UnniStyle::default)
                                    .show_think = Some(show);
                                config.save(&config_path)?;
                                // 同步运行期快照（RuntimeStyles 整体重建，避免与 config 漂移）
                                // 与 TuiState 两字段（KEEP 分支无此步，此处必须有——
                                // 否则退出面板后 render_messages 仍按旧值渲染）。
                                let styles = RuntimeStyles::from_config(&config);
                                *mode_styles_shared.lock().unwrap() = styles;
                                state.ui_show_think = styles.ui_show_think;
                                state.unni_show_think = styles.unni_show_think;
                                state.config_panel.message = Some((
                                    format!(
                                        "UNNI 思考输出已切换 → {}",
                                        if show { "开" } else { "关" }
                                    ),
                                    false,
                                ));
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



                }
            }


            Some(outcome) = pool_rx.recv() => {
                // F1: invalid_json 重试耗尽后保留用户输入意图，自动续跑修复轮（不回到询问状态）。
                let oid = outcome.id.clone();
                let outcome_ok = outcome.result.is_ok();
                tracing::debug!(
                    "streaming_loop: pool outcome id={oid} ok={outcome_ok}"
                );
                let repair_hint = match &outcome.result {
                    Err(AgentError::ThinkingOutputInvalid(msg)) => Some(msg.clone()),
                    _ => None,
                };
                let failed_id = outcome.id.clone();
                mode_manager.bookkeep(outcome, &state.last_user_message());

                if let Some(failure_msg) = repair_hint {
                    // 内部实例（洞察回环轮/legacy 内部轮）失败不触发自动修复——
                    // 它们不是用户可见的请求轮，自动重试会打乱回环执行权判定。
                    let failed_ctx = pool.get_turn_context(&failed_id).await;
                    let is_internal = failed_ctx
                        .as_ref()
                        .is_some_and(|c| c.input_kind == "insight" || c.input_kind == "echo");
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

                // 2.0.1 统一触发调度（决策冻结 2026-08-21）：
                // 核心循环 = 思考引擎 → 执行中台 → 洞察中台 → 记忆中台（异步沉淀）→ 思考引擎；
                // 协同节点固定洞察：execution_complete 只走消息通道触发洞察（不在此 spawn）；
                // subagent_complete 退化为纯数据事件（不触发任何轮）；memory_complete 只沉淀不触发。
                let mode_name = mode_manager.current_name().to_ascii_lowercase();

                match event.reason.as_str() {
                    // subagent_complete：纯数据事件（状态变化已落盘 AgentPool / memory.json），
                    // 不触发任何轮；subagent 结果段由洞察中台在输入组装时按状态变化纳入。
                    "subagent_complete" => {
                        tracing::debug!(
                            "streaming_loop: subagent_complete is a data event, no round triggered (subagent_id={})",
                            event.turn_id
                        );
                        continue;
                    }
                    // 记忆中台沉淀完成：只沉淀不触发（异步链）。
                    "memory_complete" => {
                        tracing::debug!(
                            "streaming_loop: memory_complete only sinks memory, no round triggered (thought_id={})",
                            event.turn_id
                        );
                        continue;
                    }
                    // 执行完成：洞察中台已由消息通道（ExecutionDone）直接触发，此处无 spawn。
                    "execution_complete" => {
                        tracing::debug!(
                            "streaming_loop: execution_complete routes to insight via message channel (thought_id={})",
                            event.turn_id
                        );
                        continue;
                    }
                    // 洞察完成 → 触发思考引擎下一轮（2.0.10 PlatformInsight，输入含洞察输出段）；
                    // 轮次完成异步触发记忆中台（insight 完成已驱动 memory platform，不阻塞循环）。
                    "insight_complete" => {
                        if mode_name == "keep" {
                            let mut tracker = keep_budget_tracker.lock().unwrap();
                            if !keep_budget_allows(&mut tracker, &mut state, &event.turn_id) {
                                continue;
                            }
                            tracker.record_instance();
                        }
                        // UNNI 跟随用户停止规则（任务书 §2.2，2026-08-26 用户确认）：
                        // 无 subagent 场景：用户轮后恰一轮回环（输出轮），随后停，等用户下一次输入
                        // （不多跑）；回环轮带新执行意图（actions 非空）→ 照常继续（协同处理）；
                        // 有 subagent 场景：等待链由 subagents_running 条件保护（running 不满足停止
                        // 条件）；B 语义交付轮（has_subagent_result=true）经 internal_no_downstream
                        // 在 thinking 层自然断（不派发 execute，不产生 insight_complete），本规则
                        // 不咨询该轮——交付 say 先于任何停止判定流式送达用户。
                        if mode_name == "unni" {
                            let ctx = pool.get_turn_context(&event.turn_id).await;
                            let is_loopback = ctx
                                .as_ref()
                                .map(|c| c.input_kind == "insight")
                                .unwrap_or(false);
                            let actions_empty = ctx
                                .as_ref()
                                .and_then(|c| c.execution.as_ref())
                                .map(|e| e.lifecycle_actions.is_empty())
                                .unwrap_or(true);
                            let subagents_running = pool
                                .subagent_states()
                                .await
                                .iter()
                                .any(|s| s.lifecycle == crate::agent::execution_types::SubagentLifecycle::Running);
                            if unni_follow_user_should_stop(is_loopback, actions_empty, subagents_running) {
                                tracing::info!(
                                    "streaming_loop: UNNI follow-user stop reached (loopback idle), waiting for user input (thought_id={})",
                                    event.turn_id
                                );
                                continue;
                            }
                        }
                        spawn_platform_insight(mode_manager, &mut state, &pool, &event.turn_id).await;
                    }
                    _ => {
                        tracing::debug!(
                            "streaming_loop: trigger ignored (unknown reason={})",
                            event.reason
                        );
                    }
                }
            }


            Ok(()) = pool_state_rx.changed() => {
                let snapshot = pool_state_rx.borrow_and_update().clone();
                state.status_line.update(snapshot);
            }
        }
    }

    // v0.4.9 退出冻结：should_exit break 后、平台收尾前，把未完成实例的 think/say 片段
    // 冻结并原子写快照（失败仅日志 + 继续退出，不阻塞退出）。
    freeze_and_save_snapshot(&state, mode_manager, app.paths.storage_root());

    Ok(())
}

/// KEEP 预算追踪器：token 与时间预算，0=无限。
/// 周期起点由 KEEP 首次触发时置为 now；`token_exceeded`/`time_exceeded` 任一为真表示预算耗尽（暂停 + 提示）。
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

    pub fn set_token_budget(&mut self, budget: u64) {
        self.token_budget = budget;
    }

    pub fn set_time_budget_secs(&mut self, secs: u64) {
        self.time_budget_secs = secs;
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
        self.token_budget != 0 && self.tokens_used >= self.token_budget
    }

    /// 时间预算是否耗尽。
    pub fn time_exceeded(&self) -> bool {
        self.time_budget_secs != 0
            && self
                .period_started_at
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

/// 解析 TUI/CLI 输入的 KEEP Token 预算：0=无限，非 0 clamp 到默认最小值 100K。
fn normalize_keep_token_budget(value: &str) -> u64 {
    match value.trim().parse::<u64>() {
        // TUI 输入单位为 K；保存到 config 的单位为 token。
        Ok(0) => 0,
        Ok(budget_k) => budget_k.saturating_mul(1000).max(100_000),
        Err(_) => 100_000,
    }
}

/// 解析 TUI/CLI 输入的 KEEP 时间预算：0=无限，非 0 clamp 到默认最小值 5min。
fn normalize_keep_time_budget_secs(value: &str) -> u64 {
    match value.trim().parse::<u64>() {
        // TUI 输入单位为 min；保存到 config 的单位为秒。
        Ok(0) => 0,
        Ok(minutes) => minutes.saturating_mul(60).max(300),
        Err(_) => 300,
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

/// insight_complete → 触发思考引擎下一轮（2.0.10 PlatformInsight，输入含洞察输出段）。
/// `has_subagent_result` = 该轮洞察输入是否含 subagent 结果段（洞察中台组装时按 AgentPool
/// subagent 状态变化记录，中间/最终结果均计），供 UNNI 动态执行权判定。
async fn spawn_platform_insight(
    mode_manager: &mut ModeManager,
    state: &mut TuiState,
    pool: &std::sync::Arc<crate::agent::agent_pool::AgentPool>,
    turn_id: &str,
) {
    let Some(ctx) = pool.get_turn_context(turn_id).await else {
        tracing::warn!("streaming_loop: insight_complete without turn context: {turn_id}");
        return;
    };
    let Some(ins) = &ctx.insight else {
        tracing::warn!("streaming_loop: insight_complete without insight output: {turn_id}");
        return;
    };
    let summary = ins.insight.insight.clone();
    let has_subagent_result = ctx.has_subagent_result;
    match mode_manager
        .spawn_with_override(
            summary.clone(),
            Some(crate::agent::thought::ThinkingInput::PlatformInsight {
                summary,
                has_subagent_result,
            }),
        )
        .await
    {
        Ok(id) => state.push_streaming(id),
        Err(e) => state.set_error(e.to_string()),
    }
}

/// UNNI follow-user stop 判定（任务书 §2.2，2026-08-26 用户确认）。
///
/// 触发条件（三者同时满足）：回环轮（input_kind=insight，即由 PlatformInsight 派生）
/// + 该轮执行动作为空 + 无 running subagent → true（停止，不 spawn 下一轮回环）。
///
/// 语义边界：
/// - 用户轮（input_kind=user）：is_loopback=false，恒不停止；
/// - 回环轮带执行意图（actions 非空）：协同链，继续；
/// - 等待链（subagent running）：由 subagents_running 条件保护，继续；
/// - B 语义交付轮（has_subagent_result=true）：在 thinking 层经 internal_no_downstream
///   终止（thinking.rs run_dual 不派发 send_execute），不产生 insight_complete 事件链，
///   本规则（仅在 insight_complete 时被咨询）对该轮天然不触发。
///
/// 仅由 UNNI 模式咨询（调用侧 `if mode_name == "unni"` 门控）：KEEP 走预算机制
/// （insight_complete 分支前置检查），LOOP 无 idle 收敛（持续迭代，由用户中断）。
fn unni_follow_user_should_stop(
    is_loopback: bool,
    actions_empty: bool,
    subagents_running: bool,
) -> bool {
    is_loopback && actions_empty && !subagents_running
}

/// v0.4.9 退出快照：从 `state.messages` 提取未完成实例的 think/say 片段。
///
/// 依赖：`mode_manager.active_ids()`（仍在跑、output 未终态落盘的实例 id）。
/// think/say 片段分别取自 `TuiMessage::Think.text` 与 `TuiMessage::Streaming.content`；
/// phase 为最小信息性判断（say 有内容→"say"；仅 think→"think"；否则"executing"）。
fn snapshot_incomplete(
    state: &TuiState,
    mode_manager: &ModeManager,
) -> Vec<crate::data::session_snapshot::IncompleteInstance> {
    let mut incomplete = Vec::new();
    for id in mode_manager.active_ids() {
        let think = state
            .messages
            .iter()
            .find_map(|m| {
                if let TuiMessage::Think { id: mid, text } = m {
                    if mid == &id {
                        Some(text.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let say = state
            .messages
            .iter()
            .find_map(|m| {
                if let TuiMessage::Streaming {
                    id: mid, content, ..
                } = m
                {
                    if mid == &id {
                        Some(content.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let phase = if !say.is_empty() {
            "say"
        } else if !think.is_empty() {
            "think"
        } else {
            "executing"
        };
        incomplete.push(crate::data::session_snapshot::IncompleteInstance::new(
            id, phase, think, say,
        ));
    }
    incomplete
}

/// v0.4.9 退出冻结 + 保存（在 `should_exit` break 后、平台收尾前调用）。
///
/// 无未完成实例 → 不写快照（干净退出，避免空快照噪音）；有则原子写。
/// 保存失败仅日志 + 继续退出（降级层 1），不阻塞退出。
fn freeze_and_save_snapshot(state: &TuiState, mode_manager: &ModeManager, storage_root: &Path) {
    let incomplete = snapshot_incomplete(state, mode_manager);
    if incomplete.is_empty() {
        tracing::debug!("session_snapshot: no incomplete instances, skip save");
        return;
    }
    let snapshot = crate::data::session_snapshot::SessionSnapshot::new(
        mode_manager.current_name().to_lowercase(),
        incomplete,
    );
    if let Err(e) = crate::data::session_snapshot::save_snapshot(storage_root, &snapshot) {
        tracing::warn!("session_snapshot: save failed (降级继续退出): {e}");
    } else {
        tracing::info!(
            "session_snapshot: saved {} incomplete instance(s) -> {}",
            snapshot.incomplete.len(),
            crate::data::session_snapshot::snapshot_path(storage_root).display()
        );
    }
}

/// v0.4.9 启动恢复（TuiState 初始化后、主循环前）。
///
/// 读快照 → 将 each incomplete 以可见形式注入消息流 → 轮转快照（防陈旧）。
/// 失败/缺失/schema 不兼容 → 空启动（降级层 2/3，仅日志提示）。
fn restore_snapshot(state: &mut TuiState, storage_root: &Path) {
    let Some(snapshot) = crate::data::session_snapshot::load_snapshot(storage_root) else {
        tracing::info!("session_snapshot: no restorable snapshot, empty start");
        return;
    };
    for inst in &snapshot.incomplete {
        // 最小呈现：用户视角「能看到上次没说完的话」即达标。
        let text = if !inst.say_partial.is_empty() {
            format!("[上次中断] {}", inst.say_partial)
        } else if !inst.think_partial.is_empty() {
            format!("[上次中断·思考阶段] {}", inst.think_partial)
        } else {
            "[上次中断] 上次会话在思考阶段中断。".to_string()
        };
        state.push_assistant(text);
    }
    crate::data::session_snapshot::clear_snapshot(storage_root);
    tracing::info!(
        "session_snapshot: restored {} interrupted instance(s), snapshot rotated",
        snapshot.incomplete.len()
    );
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
    fn fresh_install_writes_every_factory_prompt_and_manifest() {
        let temporary = tempfile::tempdir().unwrap();

        ensure_default_prompts(temporary.path()).unwrap();

        for (name, expected) in crate::logic::model::prompts::DEFAULT_PROMPTS {
            assert_eq!(
                std::fs::read_to_string(temporary.path().join("prompts").join(name)).unwrap(),
                expected
            );
        }
        assert!(temporary.path().join("manifest.json").exists());
    }

    #[test]
    fn matching_manifest_default_is_upgraded() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let old_defaults = [("system.md", "old-default")];
        crate::startup::manifest::ensure_fresh_install(root, &old_defaults).unwrap();

        let new_defaults = [("system.md", "new-default")];
        crate::startup::manifest::upgrade_prompts(root, &new_defaults, |_| UpgradeChoice::Cancel)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("prompts/system.md")).unwrap(),
            "new-default"
        );
    }

    #[test]
    fn customized_file_is_preserved_with_cancel() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let defaults = crate::logic::model::prompts::DEFAULT_PROMPTS;
        crate::startup::manifest::ensure_fresh_install(root, &defaults).unwrap();
        std::fs::write(root.join("prompts/mode_loop.md"), "custom loop contract").unwrap();
        std::fs::write(root.join("prompts/local_notes.md"), "keep this too").unwrap();

        ensure_default_prompts(root).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("prompts/mode_loop.md")).unwrap(),
            "custom loop contract"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("prompts/local_notes.md")).unwrap(),
            "keep this too"
        );
    }
}

#[cfg(test)]
mod keep_budget_tests {
    use super::*;

    #[test]
    fn keep_budget_defaults_allow_start() {
        let mut t = KeepBudgetTracker::new(100_000, 300);
        assert!(!t.exceeded());
        assert!(keep_budget_allows(&mut t, &mut TuiState::new(), "t1"));
    }

    #[test]
    fn keep_budget_token_exhausts_after_instances() {
        let mut t = KeepBudgetTracker::new(24_000, 300);
        t.record_instance();
        t.record_instance();
        assert!(!t.token_exceeded());
        t.record_instance();
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

    #[test]
    fn keep_budget_normalizers_zero_means_unlimited() {
        assert_eq!(normalize_keep_token_budget("0"), 0);
        assert_eq!(normalize_keep_token_budget("50"), 100_000);
        assert_eq!(normalize_keep_token_budget("200"), 200_000);
        assert_eq!(normalize_keep_token_budget("bad"), 100_000);

        assert_eq!(normalize_keep_time_budget_secs("0"), 0);
        assert_eq!(normalize_keep_time_budget_secs("2"), 300);
        assert_eq!(normalize_keep_time_budget_secs("10"), 600);
        assert_eq!(normalize_keep_time_budget_secs("bad"), 300);
    }
}

#[cfg(test)]
mod unni_follow_user_stop_tests {
    use super::*;

    /// 用户轮（input_kind=user）：is_loopback=false，无论动作/子代理状态如何都不停
    /// （用户轮后回环正常 spawn，不误停）。
    #[test]
    fn unni_user_round_never_stops() {
        assert!(!unni_follow_user_should_stop(false, true, false));
        assert!(!unni_follow_user_should_stop(false, false, false));
        assert!(!unni_follow_user_should_stop(false, true, true));
        assert!(!unni_follow_user_should_stop(false, false, true));
    }

    /// 回环轮 + 0 动作 + 无 running subagent → 停（单次回环后不多跑）。
    #[test]
    fn unni_loopback_idle_stops() {
        assert!(unni_follow_user_should_stop(true, true, false));
    }

    /// 回环轮 + 0 动作 + 有 running subagent → 继续 spawn（等待链，subagent running
    /// 不满足停止条件）。
    #[test]
    fn unni_loopback_idle_keeps_wait_chain_when_subagents_running() {
        assert!(!unni_follow_user_should_stop(true, true, true));
    }

    /// 回环轮 + 非空 actions → 继续 spawn（协同链：模型主动补执行属于协同处理，不是空转）。
    #[test]
    fn unni_loopback_with_actions_keeps_collaboration_chain() {
        assert!(!unni_follow_user_should_stop(true, false, false));
        assert!(!unni_follow_user_should_stop(true, false, true));
    }

    /// B 语义路径回归（has_subagent_result=true 交付轮不因新规则提前断）：
    /// B 交付轮（PlatformInsight + has_subagent_result=true）在 thinking 层经
    /// internal_no_downstream 终止（thinking.rs run_dual：不派发 send_execute，
    /// 555-566 行），因此不产生 execution → insight → insight_complete 事件链——
    /// 本规则仅在 insight_complete 时被咨询，对该轮天然不触发；交付 say 在 thinking
    /// 阶段已流式送达用户，先于任何停止判定。
    /// 守卫：若未来链路变更使交付轮意外进入本判定，谓词对其理论输入
    /// （loopback=true, 0 动作, 无 running）返回 true，即 B 语义回归报警点；
    /// 当前冻结的 internal_no_downstream 语义（v0.4.4 产物）保证该轮不进入
    /// execution/insight 链，此断言固化的正是「交付轮不因新规则提前断」的不变量。
    #[test]
    fn unni_b_semantics_delivery_round_regression() {
        // 交付轮理论输入下谓词会停（若被咨询）——回归点信号。
        assert!(unni_follow_user_should_stop(true, true, false));
        // 等待链保护：交付轮之前的轮次若仍见 running subagent，链不被截断。
        assert!(!unni_follow_user_should_stop(true, true, true));
    }
}

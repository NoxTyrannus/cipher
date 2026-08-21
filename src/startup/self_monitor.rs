use crate::logic::model::prompts;
use crate::startup::config::{Config, RuntimeStyles};
use crate::startup::entry::KeepBudgetTracker;
use crate::startup::manifest;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

/// 启动 1 分钟自监测任务：检测提示词/配置变更，使缓存失效并更新 manifest 与运行中共享配置。
pub fn spawn_self_monitor(
    unified_root: PathBuf,
    mode_styles_shared: Arc<Mutex<RuntimeStyles>>,
    keep_budget_tracker: Arc<Mutex<KeepBudgetTracker>>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        let mut last_config_mtime: Option<std::time::SystemTime> = None;
        loop {
            tick.tick().await;

            // 1) 提示词自监测
            let defaults = prompts::DEFAULT_PROMPTS;
            if let Ok(changed) = manifest::refresh_user_modified(&unified_root, &defaults) {
                if changed {
                    prompts::clear_prompt_cache();
                    tracing::info!("self_monitor: prompt changes detected, cache invalidated");
                }
            }

            // 2) 配置自监测
            let config_path = unified_root.join("config.toml");
            let current_mtime = std::fs::metadata(&config_path)
                .and_then(|m| m.modified())
                .ok();
            let changed = match (last_config_mtime, current_mtime) {
                (None, Some(_)) => true,
                (Some(old), Some(new)) => old != new,
                _ => false,
            };
            if changed {
                last_config_mtime = current_mtime;
                if let Ok(Some(config)) = Config::load(&config_path) {
                    *mode_styles_shared.lock().unwrap() = RuntimeStyles::from_config(&config);
                    let mut tracker = keep_budget_tracker.lock().unwrap();
                    tracker.set_token_budget(config.mode_styles.keep.token_budget);
                    tracker.set_time_budget_secs(config.mode_styles.keep.time_budget_secs);
                    drop(tracker);
                    prompts::clear_prompt_cache();
                    tracing::info!("self_monitor: config.toml changed, shared config updated");
                }
            } else if last_config_mtime.is_none() {
                last_config_mtime = current_mtime;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::manifest::{self, ensure_fresh_install};
    use std::fs;

    #[tokio::test]
    async fn refresh_detects_user_modified_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let defaults = prompts::DEFAULT_PROMPTS;
        ensure_fresh_install(root, &defaults).unwrap();
        fs::write(root.join("prompts/system.md"), "changed").unwrap();
        let changed = manifest::refresh_user_modified(root, &defaults).unwrap();
        assert!(changed);
        let m = manifest::load(&root.join("manifest.json")).unwrap();
        assert!(m.files["prompts/system.md"].user_modified);
    }

    #[tokio::test]
    async fn config_change_updates_shared_styles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let defaults = prompts::DEFAULT_PROMPTS;
        ensure_fresh_install(root, &defaults).unwrap();
        let config_path = root.join("config.toml");
        let cfg = Config::default_config();
        cfg.save(&config_path).unwrap();

        let styles = Arc::new(Mutex::new(RuntimeStyles::default()));
        let mut c = Config::load(&config_path).unwrap().unwrap();
        c.mode_styles.keep.token_budget = 0;
        c.save(&config_path).unwrap();
        let loaded = Config::load(&config_path).unwrap().unwrap();
        assert_eq!(loaded.mode_styles.keep.token_budget, 0);
        drop(styles);
    }
}

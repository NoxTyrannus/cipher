use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    #[deprecated(
        note = "iter78+: use model 表 (ADR-130 设计点 16); config 字段废弃, 仅 audit trail"
    )]
    pub provider: String,
    #[serde(default)]
    #[deprecated(note = "iter78+: use model 表 model_id; config 字段废弃, 仅 audit trail")]
    pub model_id: String,
    #[serde(default)]
    #[deprecated(
        note = "iter78+: use model 表 api_key (ADR-130 设计点 16); config 字段废弃, 仅 audit trail"
    )]
    pub api_key: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_mode")]
    pub default_mode: String,

    /// 模式附加配置。旧 `[mode_styles.unni]` / `[mode_styles.loop]` 字段在读取时忽略+warn，
    /// 不迁移落盘（放弃项 3）；v0.4.6 起 `[mode_styles.unni] show_think` 恢复为受支持字段
    /// （UNNI per-mode 思考显示覆盖，缺省 None=跟随全局）。
    #[serde(default)]
    pub mode_styles: ModeStyles,

    #[serde(default)]
    pub default_model: Option<String>,

    #[serde(default)]
    pub context: ContextSection,

    /// UI 显示设置（v0.4.6）。思考面板显示开关：
    /// - 缺省 `show_think = true`（保持 v0.4.5 及以前行为——思考面板恒显示）；
    /// - UNNI 单独关闭思考显示：`[mode_styles.unni] show_think = false`（跟随/覆盖全局）。
    #[serde(default)]
    pub ui: UiSection,

    /// 网络能力设置（v0.4.6）。`web.fetch.public` 域名白名单：
    /// - 缺省空列表 = 拒绝全部域名（安全默认）；
    /// - 需用时手写，如 `allowed_domains = ["kaggle.com", "www.kaggle.com"]`；
    /// - 匹配规则：host 精确匹配或为其子域（`www.kaggle.com` 匹配 `kaggle.com`），端口忽略。
    #[serde(default)]
    pub web: WebSection,

    /// 三中台机制式排队合并开关（v0.4.7）：`[execution] merge_enabled` /
    /// `[insight] merge_enabled` / `[memory] merge_enabled`，缺省均 true。
    /// - true = 批 = 连续处理组（飞行缓冲消除队列空隙，状态驱动、无定时窗口）；
    /// - false = 完全回退逐条现状（与 v0.4.6 及以前行为一致）。
    #[serde(default)]
    pub execution: MergeSection,
    #[serde(default)]
    pub insight: MergeSection,
    #[serde(default)]
    pub memory: MergeSection,
}

/// 三中台合并开关段（v0.4.7）：`merge_enabled` 缺省 true。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeSection {
    /// 机制式排队合并开关（缺省 true）。
    #[serde(default = "default_true")]
    pub merge_enabled: bool,
}

impl Default for MergeSection {
    fn default() -> Self {
        Self {
            merge_enabled: default_true(),
        }
    }
}

/// `[ui]` 段：UI 显示开关（v0.4.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSection {
    /// 全局思考面板显示开关（缺省 true=显示；UNNI per-mode 覆盖见 [`UnniStyle`]）。
    #[serde(default = "default_true")]
    pub show_think: bool,
}

fn default_true() -> bool {
    true
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            show_think: default_true(),
        }
    }
}

/// `[web]` 段：网络能力配置（v0.4.6）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSection {
    /// `web.fetch.public` 允许抓取的域名白名单（缺省空=拒绝全部）。
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cipher")
        .join("data")
}

fn default_mode() -> String {
    "unni".to_string()
}

/// KEEP 附加设置：成本护栏。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepStyle {
    /// Token 预算：0=无限，其余为最小 100K 起的 token 数。
    #[serde(default = "default_keep_token_budget")]
    pub token_budget: u64,
    /// 时间预算（秒）：0=无限。
    #[serde(default = "default_keep_time_budget_secs")]
    pub time_budget_secs: u64,
}

fn default_keep_token_budget() -> u64 {
    0
}
fn default_keep_time_budget_secs() -> u64 {
    0
}

impl Default for KeepStyle {
    fn default() -> Self {
        Self {
            token_budget: default_keep_token_budget(),
            time_budget_secs: default_keep_time_budget_secs(),
        }
    }
}

/// 模式附加配置；旧 unni/r#loop 字段由 serde 忽略（读取不报错），仅 KEEP 预算生效；
/// v0.4.6 起 `[mode_styles.unni] show_think` 恢复为受支持字段（UNNI per-mode 思考显示覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeStyles {
    #[serde(default = "default_keep_style")]
    pub keep: KeepStyle,
    /// UNNI per-mode 覆盖（缺省 None=跟随全局 `[ui] show_think`）。
    #[serde(default)]
    pub unni: Option<UnniStyle>,
}

/// UNNI 模式附加设置（v0.4.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UnniStyle {
    /// UNNI 思考面板显示覆盖（缺省 None=跟随全局；`false`=UNNI 下关闭思考显示）。
    #[serde(default)]
    pub show_think: Option<bool>,
}

fn default_keep_style() -> KeepStyle {
    KeepStyle::default()
}

impl Default for ModeStyles {
    fn default() -> Self {
        Self {
            keep: default_keep_style(),
            unni: None,
        }
    }
}

/// 运行期共享的协作配置快照（协同节点固定洞察 + mix 机制已删除；KEEP 预算 + v0.4.6
/// 思考显示开关——全局 `ui.show_think` 与 UNNI per-mode 覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStyles {
    pub keep: KeepStyle,
    /// 全局思考显示开关（来自 `[ui] show_think`）。
    pub ui_show_think: bool,
    /// UNNI per-mode 思考显示覆盖（来自 `[mode_styles.unni] show_think`）。
    pub unni_show_think: Option<bool>,
}

impl RuntimeStyles {
    pub fn from_config(config: &Config) -> Self {
        Self {
            keep: config.mode_styles.keep,
            ui_show_think: config.ui.show_think,
            unni_show_think: config.mode_styles.unni.and_then(|style| style.show_think),
        }
    }
}

impl Default for RuntimeStyles {
    fn default() -> Self {
        Self {
            keep: KeepStyle::default(),
            ui_show_think: true,
            unni_show_think: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSection {
    #[serde(default = "default_recent_turns")]
    pub recent_turns: usize,
    #[serde(default = "default_raw_threshold_pct")]
    pub raw_threshold_pct: f64,
    #[serde(default = "default_rag_reserve_pct")]
    pub rag_reserve_pct: f64,
    #[serde(default = "default_cognitive_quota_pct")]
    pub cognitive_quota_pct: f64,
    #[serde(default = "default_attention_quota_pct")]
    pub attention_quota_pct: f64,
    #[serde(default = "default_experience_quota_pct")]
    pub experience_quota_pct: f64,
    #[serde(default = "default_preference_quota_pct")]
    pub preference_quota_pct: f64,
}

fn default_recent_turns() -> usize {
    3
}
fn default_raw_threshold_pct() -> f64 {
    30.0
}
fn default_rag_reserve_pct() -> f64 {
    10.0
}
fn default_cognitive_quota_pct() -> f64 {
    5.0
}
fn default_attention_quota_pct() -> f64 {
    5.0
}
fn default_experience_quota_pct() -> f64 {
    5.0
}
fn default_preference_quota_pct() -> f64 {
    3.0
}

impl Default for ContextSection {
    fn default() -> Self {
        Self {
            recent_turns: default_recent_turns(),
            raw_threshold_pct: default_raw_threshold_pct(),
            rag_reserve_pct: default_rag_reserve_pct(),
            cognitive_quota_pct: default_cognitive_quota_pct(),
            attention_quota_pct: default_attention_quota_pct(),
            experience_quota_pct: default_experience_quota_pct(),
            preference_quota_pct: default_preference_quota_pct(),
        }
    }
}

impl From<&ContextSection> for crate::agent::context_assembler::ContextConfig {
    fn from(s: &ContextSection) -> Self {
        Self {
            recent_turns: s.recent_turns,
            raw_threshold_pct: s.raw_threshold_pct,
            rag_reserve_pct: s.rag_reserve_pct,
            cognitive_quota_pct: s.cognitive_quota_pct,
            attention_quota_pct: s.attention_quota_pct,
            experience_quota_pct: s.experience_quota_pct,
            preference_quota_pct: s.preference_quota_pct,
            context_window: 1_000_000,
        }
    }
}

impl Config {
    #[allow(deprecated)]
    pub fn default_config() -> Self {
        Self {
            provider: String::new(),
            model_id: String::new(),
            api_key: String::new(),
            data_dir: default_data_dir(),
            default_mode: default_mode(),
            mode_styles: ModeStyles::default(),
            default_model: None,
            context: ContextSection::default(),
            ui: UiSection::default(),
            web: WebSection::default(),
            execution: MergeSection::default(),
            insight: MergeSection::default(),
            memory: MergeSection::default(),
        }
    }

    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cipher")
            .join("config.toml")
    }

    pub fn load(path: &Path) -> Result<Option<Self>, crate::common::AgentError> {
        if fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(None);
        }
        crate::data::permissions::secure_existing_file(path)?;
        let content = fs::read_to_string(path)
            .map_err(|e| crate::common::AgentError::Io(format!("read config {:?}: {}", path, e)))?;
        // 放弃项 1-3：旧 `[collaboration] node/mix_thinking` 与 `[mode_styles].unni/loop`
        // 读取忽略 + warn，不迁移落盘（协同节点固定洞察，mix 机制整体删除）。
        warn_legacy_collaboration_keys(&content);
        let config: Config = toml::from_str(&content).map_err(|e| {
            crate::common::AgentError::Parse(format!("parse config {:?}: {}", path, e))
        })?;
        Ok(Some(config))
    }

    pub fn save(&self, path: &Path) -> Result<(), crate::common::AgentError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.exists() || path == Self::default_path() {
            crate::data::permissions::ensure_private_directory(parent)?;
        } else if !parent.is_dir() {
            return Err(crate::common::AgentError::Bootstrap(format!(
                "config parent is not a directory: {:?}",
                parent
            )));
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::common::AgentError::Parse(format!("serialize config: {}", e)))?;
        if fs::symlink_metadata(path).is_ok() {
            crate::data::permissions::secure_existing_file(path)?;
        }

        let temporary_path = parent.join(format!(".config.{}.tmp", uuid::Uuid::new_v4()));
        let write_result = (|| -> Result<(), crate::common::AgentError> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary_path).map_err(|error| {
                crate::common::AgentError::Io(format!(
                    "create config temporary file {:?}: {error}",
                    temporary_path
                ))
            })?;
            file.write_all(content.as_bytes()).map_err(|error| {
                crate::common::AgentError::Io(format!(
                    "write config temporary file {:?}: {error}",
                    temporary_path
                ))
            })?;
            file.sync_all().map_err(|error| {
                crate::common::AgentError::Io(format!(
                    "flush config temporary file {:?}: {error}",
                    temporary_path
                ))
            })?;
            drop(file);
            crate::data::permissions::secure_existing_file(&temporary_path)?;

            #[cfg(windows)]
            if path.exists() {
                fs::remove_file(path).map_err(|error| {
                    crate::common::AgentError::Io(format!(
                        "replace existing config {:?}: {error}",
                        path
                    ))
                })?;
            }
            fs::rename(&temporary_path, path).map_err(|error| {
                crate::common::AgentError::Io(format!(
                    "publish config {:?} to {:?}: {error}",
                    temporary_path, path
                ))
            })?;
            crate::data::permissions::secure_existing_file(path)?;
            sync_directory(parent)
        })();

        if write_result.is_err() && temporary_path.exists() {
            let _ = fs::remove_file(&temporary_path);
        }
        write_result
    }
}

/// 放弃项 1-3：旧配置键读取忽略 + warn（不迁移落盘）。
/// - `[collaboration] node`（三选一）→ 协同节点固定洞察；
/// - `[collaboration] mix_thinking` → mix 机制整体删除（LOOP 无 mix）；
/// - `[mode_styles.unni] node` / `[mode_styles.loop] mix_thinking` → 旧字段迁移已删除。
///
/// v0.4.6 注意：`[mode_styles.unni]` 段本身不再是「旧键」——`show_think` 已是受支持字段
/// （UNNI per-mode 思考显示覆盖）；只有段内旧字段 `node`/`mix_thinking` 仍触发忽略+warn。
fn warn_legacy_collaboration_keys(content: &str) {
    let Ok(value) = content.parse::<toml::Value>() else {
        return; // 解析失败交给 Config 反序列化报错处理
    };
    let mut legacy_found = false;
    if let Some(collab) = value.get("collaboration") {
        if collab.get("node").is_some() {
            tracing::warn!(
                "config: 旧键 [collaboration] node 已忽略（协同节点固定洞察，不迁移落盘）"
            );
            legacy_found = true;
        }
        if collab.get("mix_thinking").is_some() {
            tracing::warn!("config: 旧键 [collaboration] mix_thinking 已忽略（mix 机制整体删除）");
            legacy_found = true;
        }
    }
    if let Some(styles) = value.get("mode_styles") {
        if let Some(unni) = styles.get("unni") {
            if unni.get("node").is_some() || unni.get("mix_thinking").is_some() {
                tracing::warn!(
                    "config: 旧键 [mode_styles.unni] node/mix_thinking 已忽略（旧字段迁移已删除，不迁移落盘）"
                );
                legacy_found = true;
            }
        }
        if let Some(loop_style) = styles.get("loop") {
            if loop_style.get("mix_thinking").is_some() {
                tracing::warn!(
                    "config: 旧键 [mode_styles.loop] mix_thinking 已忽略（旧字段迁移已删除，不迁移落盘）"
                );
                legacy_found = true;
            }
        }
    }
    if legacy_found {
        tracing::info!("config: 已忽略旧协作配置键，未迁移落盘");
    }
}

#[allow(unused_variables)]
fn sync_directory(path: &Path) -> Result<(), crate::common::AgentError> {
    #[cfg(unix)]
    {
        use std::fs::File;
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                crate::common::AgentError::Io(format!("flush config directory {:?}: {error}", path))
            })?;
    }

    Ok(())
}

pub fn migrate_data_dir() -> Result<bool, crate::common::AgentError> {
    let old_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cipher")
        .join("data");
    let new_dir = default_data_dir();

    if old_dir.exists() && !new_dir.exists() {
        if std::fs::symlink_metadata(&old_dir)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(crate::common::AgentError::Bootstrap(format!(
                "migrate: legacy data directory cannot be a symlink: {:?}",
                old_dir
            )));
        }

        if let Some(parent) = new_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                crate::common::AgentError::Io(format!(
                    "migrate: create parent dir {:?}: {}",
                    parent, e
                ))
            })?;
        }
        tracing::info!(
            old = ?old_dir,
            new = ?new_dir,
            "migrating data dir from old XDG-incompatible path"
        );
        std::fs::rename(&old_dir, &new_dir).map_err(|e| {
            crate::common::AgentError::Io(format!(
                "migrate: rename {:?} → {:?}: {}",
                old_dir, new_dir, e
            ))
        })?;
        crate::data::permissions::ensure_private_directory(&new_dir)?;
        let duckdb_path = new_dir.join("cipher.duckdb");
        let duckdb = duckdb_path.to_string_lossy();
        for suffix in ["", ".wal", ".wal.checkpoint", ".wal.recovery"] {
            crate::data::permissions::secure_existing_file(Path::new(&format!(
                "{duckdb}{suffix}"
            )))?;
        }
        let triviumdb_dir = new_dir.join("triviumdb");
        if triviumdb_dir.is_dir() {
            crate::data::permissions::ensure_private_directory(&triviumdb_dir)?;
            for entry in std::fs::read_dir(&triviumdb_dir).map_err(|error| {
                crate::common::AgentError::Io(format!(
                    "migrate: read TriviumDB directory {:?}: {error}",
                    triviumdb_dir
                ))
            })? {
                let path = entry
                    .map_err(|error| {
                        crate::common::AgentError::Io(format!(
                            "migrate: read TriviumDB entry: {error}"
                        ))
                    })?
                    .path();
                if path.is_file() {
                    crate::data::permissions::secure_existing_file(&path)?;
                }
            }
        }
        tracing::info!("data dir migrated successfully");
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    #[test]
    fn default_data_dir_is_under_home_dot_cipher() {
        let d = default_data_dir();
        assert!(d.ends_with(".cipher/data"), "got: {d:?}");
    }

    #[test]
    fn default_mode_is_unni() {
        assert_eq!(default_mode(), "unni");
    }

    #[test]
    fn mode_styles_defaults_keep_only() {
        let m = ModeStyles::default();
        assert_eq!(m.keep.token_budget, 0);
        assert_eq!(m.keep.time_budget_secs, 0);
        assert_eq!(m.unni, None, "缺省无 UNNI per-mode 覆盖（跟随全局）");
    }

    #[test]
    fn runtime_styles_carries_keep_budget_and_ui_show_think() {
        // 放弃项 1/2/10：协同节点固定洞察 + mix 删除后，RuntimeStyles 承载 KEEP 预算
        // + v0.4.6 思考显示开关（全局 + UNNI per-mode 覆盖）。
        let c = Config::default_config();
        let styles = RuntimeStyles::from_config(&c);
        assert_eq!(styles.keep.token_budget, 0);
        assert_eq!(styles.keep.time_budget_secs, 0);
        assert!(styles.ui_show_think, "缺省显示思考面板（保持既有行为）");
        assert_eq!(styles.unni_show_think, None, "缺省 UNNI 跟随全局");

        let mut cfg = Config::default_config();
        cfg.ui.show_think = false;
        cfg.mode_styles.unni = Some(UnniStyle {
            show_think: Some(false),
        });
        let styles = RuntimeStyles::from_config(&cfg);
        assert!(!styles.ui_show_think);
        assert_eq!(styles.unni_show_think, Some(false));
    }

    #[test]
    fn ui_section_defaults_to_show_think_true() {
        let ui = UiSection::default();
        assert!(ui.show_think, "缺省 show_think=true（思考面板显示）");
        let parsed: Config = toml::from_str("").unwrap();
        assert!(
            parsed.ui.show_think,
            "空配置反序列化后 show_think 缺省 true"
        );
    }

    #[test]
    fn ui_section_explicit_and_unni_override_parse() {
        // 缺省：无 [ui] 段 → show_think=true、无 UNNI 覆盖。
        let parsed: Config = toml::from_str("").unwrap();
        assert!(parsed.ui.show_think);
        assert_eq!(parsed.mode_styles.unni, None);

        // 显式：[ui] show_think=false + [mode_styles.unni] show_think=false。
        let explicit = r#"
            [ui]
            show_think = false

            [mode_styles.unni]
            show_think = false
        "#;
        let parsed: Config = toml::from_str(explicit).unwrap();
        assert!(!parsed.ui.show_think);
        assert_eq!(
            parsed.mode_styles.unni,
            Some(UnniStyle {
                show_think: Some(false)
            })
        );

        // 覆盖：全局 false、UNNI Some(true)（UNNI 下反而显示）。
        let override_true = r#"
            [ui]
            show_think = false

            [mode_styles.unni]
            show_think = true
        "#;
        let parsed: Config = toml::from_str(override_true).unwrap();
        assert!(!parsed.ui.show_think);
        assert_eq!(
            parsed.mode_styles.unni,
            Some(UnniStyle {
                show_think: Some(true)
            })
        );
    }

    #[test]
    fn unni_style_show_think_defaults_none() {
        // [mode_styles.unni] 段存在但无 show_think → None（跟随全局）。
        let parsed: Config = toml::from_str("[mode_styles.unni]").unwrap();
        assert_eq!(
            parsed.mode_styles.unni,
            Some(UnniStyle { show_think: None })
        );
        assert!(parsed.ui.show_think);
    }

    #[test]
    fn web_section_allowed_domains_default_empty_and_parse() {
        let parsed: Config = toml::from_str("").unwrap();
        assert!(
            parsed.web.allowed_domains.is_empty(),
            "缺省白名单为空 = 拒绝全部域名（安全默认）"
        );

        let explicit = r#"
            [web]
            allowed_domains = ["kaggle.com", "www.kaggle.com"]
        "#;
        let parsed: Config = toml::from_str(explicit).unwrap();
        assert_eq!(
            parsed.web.allowed_domains,
            vec!["kaggle.com".to_string(), "www.kaggle.com".to_string()]
        );
    }

    #[test]
    fn keep_style_defaults() {
        let b = KeepStyle::default();
        assert_eq!(b.token_budget, 0);
        assert_eq!(b.time_budget_secs, 0);
    }

    #[test]
    fn mode_styles_keep_toml_roundtrip() {
        let explicit = r#"
            [mode_styles.keep]
            token_budget = 200000
            time_budget_secs = 600
        "#;
        let cfg: Config = toml::from_str(explicit).unwrap();
        assert_eq!(cfg.mode_styles.keep.token_budget, 200_000);
        assert_eq!(cfg.mode_styles.keep.time_budget_secs, 600);

        let serialized = toml::to_string(&cfg.mode_styles).unwrap();
        let decoded: ModeStyles = toml::from_str(&serialized).unwrap();
        assert_eq!(decoded, cfg.mode_styles);
    }

    #[test]
    fn mode_styles_keep_negative_token_fails_parse() {
        assert!(toml::from_str::<ModeStyles>("keep = { token_budget = -1 }").is_err());
    }

    #[test]
    fn legacy_collaboration_keys_ignored_without_migration() {
        // 放弃项 1-3：旧 [collaboration] node/mix_thinking 与 [mode_styles].unni/loop
        // 读取忽略 + warn（不迁移落盘），解析不报错。
        let legacy = r#"
            [collaboration]
            node = "memory"
            mix_thinking = true

            [mode_styles.unni]
            node = "insight"

            [mode_styles.loop]
            mix_thinking = true
        "#;
        let cfg: Config = toml::from_str(legacy).unwrap();
        assert_eq!(cfg.mode_styles.keep, KeepStyle::default());
    }

    #[test]
    fn merge_sections_default_to_enabled_and_parse_explicit() {
        // v0.4.7：三中台合并开关缺省均 true；显式 false 可解析；缺省段不报错。
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.execution.merge_enabled);
        assert!(cfg.insight.merge_enabled);
        assert!(cfg.memory.merge_enabled);

        let explicit = r#"
            [execution]
            merge_enabled = false

            [memory]
            merge_enabled = false
        "#;
        let cfg: Config = toml::from_str(explicit).unwrap();
        assert!(!cfg.execution.merge_enabled);
        assert!(cfg.insight.merge_enabled, "未写段缺省 true");
        assert!(!cfg.memory.merge_enabled);

        // 旧配置无新段：缺省 true，且与旧字段共存。
        let legacy: Config =
            toml::from_str("[web]\nallowed_domains = [\"kaggle.com\"]\n[ui]\nshow_think = false\n")
                .unwrap();
        assert!(legacy.memory.merge_enabled);
        assert_eq!(legacy.web.allowed_domains, vec!["kaggle.com".to_string()]);
    }

    #[test]
    fn load_ignores_legacy_collaboration_keys_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[collaboration]\nnode = \"execution\"\nmix_thinking = true\n[mode_styles.unni]\nnode = \"memory\"\n[mode_styles.loop]\nmix_thinking = false\n",
        )
        .unwrap();
        let loaded = Config::load(&path).unwrap().unwrap();
        // 协同节点固定洞察 + mix 删除：旧值被忽略，配置仍可正常加载。
        assert_eq!(loaded.mode_styles.keep, KeepStyle::default());
        assert_eq!(loaded.default_mode, "unni");
    }

    #[test]
    fn legacy_warn_detects_old_keys() {
        // warn_legacy_collaboration_keys 对无旧键内容静默（不 panic、不产生副作用）。
        warn_legacy_collaboration_keys("[mode_styles.keep]\ntoken_budget = 200000\n");
        warn_legacy_collaboration_keys("not toml {{");
    }

    #[test]
    fn default_path_ends_with_cipher_config_toml() {
        let p = Config::default_path();
        let s = p.to_string_lossy();
        assert!(s.ends_with(".cipher/config.toml"), "got: {s}");
    }

    #[test]
    fn load_missing_returns_ok_none() {
        let p = PathBuf::from("/tmp/__cipher_nonexistent_config__.toml");
        let _ = std::fs::remove_file(&p);
        let r = Config::load(&p).expect("load missing should not error");
        assert!(r.is_none());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp =
            std::env::temp_dir().join(format!("cipher-cfg-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&tmp).unwrap();
        let p = tmp.join("config.toml");

        let cfg = Config {
            provider: "openai".into(),
            model_id: "gpt-4".into(),
            api_key: "sk-test-roundtrip".into(),
            data_dir: PathBuf::from("/tmp/data"),
            default_mode: "keep".into(),
            mode_styles: ModeStyles::default(),
            default_model: None,
            context: ContextSection::default(),
            ui: UiSection::default(),
            web: WebSection::default(),
            execution: MergeSection::default(),
            insight: MergeSection::default(),
            memory: MergeSection::default(),
        };
        cfg.save(&p).expect("save ok");
        let loaded = Config::load(&p).expect("load ok").expect("exists");
        assert_eq!(loaded.provider, "openai");
        assert_eq!(loaded.model_id, "gpt-4");
        assert_eq!(loaded.api_key, "sk-test-roundtrip");
        assert_eq!(loaded.data_dir, PathBuf::from("/tmp/data"));
        assert_eq!(loaded.default_mode, "keep");
        assert_eq!(loaded.mode_styles.keep, KeepStyle::default());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn save_chmods_600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!(
            "cipher-cfg-chmod-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let p = tmp.join("config.toml");

        let cfg = Config {
            provider: "x".into(),
            model_id: "y".into(),
            api_key: "z".into(),
            data_dir: PathBuf::from("."),
            default_mode: "unni".into(),
            mode_styles: ModeStyles::default(),
            default_model: None,
            context: ContextSection::default(),
            ui: UiSection::default(),
            web: WebSection::default(),
            execution: MergeSection::default(),
            insight: MergeSection::default(),
            memory: MergeSection::default(),
        };
        cfg.save(&p).expect("save ok");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config should be chmod 600, got: {mode:o}");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn load_repairs_old_mode_and_atomic_save_preserves_custom_parent() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("custom-parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = parent.join("config.toml");
        let config = Config::default_config();

        config.save(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o755,
            "a custom parent directory must not be chmodded"
        );
        assert!(std::fs::read_dir(&parent).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        Config::load(&path).unwrap().unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_and_load_reject_config_symlinks() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target.toml");
        Config::default_config().save(&target).unwrap();
        let link = temporary.path().join("linked.toml");
        symlink(&target, &link).unwrap();

        assert!(Config::load(&link).is_err());
        assert!(Config::default_config().save(&link).is_err());
    }
}

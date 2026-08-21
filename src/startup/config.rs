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

    /// 全局协作配置（UNNI/KEEP/LOOP 共用同一协同节点与 Mix Thinking 开关）。
    #[serde(default)]
    pub collaboration: CollaborationSection,

    /// 模式附加配置。unni/r#loop 仅用于旧配置迁移，迁移完成后不落盘。
    #[serde(default)]
    pub mode_styles: ModeStyles,

    #[serde(default)]
    pub default_model: Option<String>,

    #[serde(default)]
    pub context: ContextSection,
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

/// 协同节点：哪个中台完成时触发思考引擎。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TriggerNode {
    /// 执行中台。
    #[default]
    Execution,
    /// 洞察中台。
    Insight,
    /// 记忆中台。
    Memory,
}

impl TriggerNode {
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerNode::Execution => "execution",
            TriggerNode::Insight => "insight",
            TriggerNode::Memory => "memory",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TriggerNode::Execution => "执行中台（执行完成即触发，最快）",
            TriggerNode::Insight => "洞察中台（洞察完成即触发，含执行分析）",
            TriggerNode::Memory => "记忆中台（记忆完成即触发，洞察+记忆完整沉淀）",
        }
    }

    /// 协同节点之后的中台（异步执行，只沉淀记忆不触发）。
    pub fn async_after(self) -> &'static [TriggerNode] {
        match self {
            TriggerNode::Execution => &[TriggerNode::Insight, TriggerNode::Memory],
            TriggerNode::Insight => &[TriggerNode::Memory],
            TriggerNode::Memory => &[],
        }
    }
}

/// 全局协作配置：`[collaboration]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationSection {
    #[serde(default = "default_collaboration_node")]
    pub node: TriggerNode,
    #[serde(default = "default_collaboration_mix_thinking")]
    pub mix_thinking: bool,
}

fn default_collaboration_node() -> TriggerNode {
    TriggerNode::Execution
}

fn default_collaboration_mix_thinking() -> bool {
    true
}

impl Default for CollaborationSection {
    fn default() -> Self {
        Self {
            node: default_collaboration_node(),
            mix_thinking: default_collaboration_mix_thinking(),
        }
    }
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

/// 旧 `[mode_styles.unni]` 的迁移载体。`style` 已删除，serde 会忽略旧字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyUnniStyle {
    #[serde(default)]
    pub node: Option<TriggerNode>,
}

/// 旧 `[mode_styles.loop]` 的迁移载体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyLoopStyle {
    #[serde(default)]
    pub mix_thinking: bool,
}

/// 模式附加配置；unni/r#loop 字段仅用于读取旧配置并在 `Config::load` 中迁移。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeStyles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unni: Option<LegacyUnniStyle>,
    #[serde(default = "default_keep_style")]
    pub keep: KeepStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#loop: Option<LegacyLoopStyle>,
}

fn default_keep_style() -> KeepStyle {
    KeepStyle::default()
}

impl Default for ModeStyles {
    fn default() -> Self {
        Self {
            unni: None,
            keep: default_keep_style(),
            r#loop: None,
        }
    }
}

/// 运行期共享的协作配置快照（全局节点 + Mix 开关 + KEEP 预算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeStyles {
    pub collaboration: CollaborationSection,
    pub keep: KeepStyle,
}

impl RuntimeStyles {
    pub fn from_config(config: &Config) -> Self {
        Self {
            collaboration: config.collaboration,
            keep: config.mode_styles.keep,
        }
    }

    pub fn node(&self) -> TriggerNode {
        self.collaboration.node
    }

    pub fn mix_on(&self, node: TriggerNode) -> bool {
        node != TriggerNode::Execution && self.collaboration.mix_thinking
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
            collaboration: CollaborationSection::default(),
            mode_styles: ModeStyles::default(),
            default_model: None,
            context: ContextSection::default(),
        }
    }

    /// 将旧 `mode_styles.unni.node` / `mode_styles.loop.mix_thinking` 迁移到 `[collaboration]`。
    /// 旧值优先；node=execution 时 Mix Thinking 自动视为关闭；迁移后清理旧字段。
    pub fn migrate_collaboration(&mut self) {
        let mut collaboration = self.collaboration;
        if let Some(unni) = self.mode_styles.unni {
            if let Some(node) = unni.node {
                collaboration.node = node;
            }
        }
        if let Some(r#loop) = self.mode_styles.r#loop {
            collaboration.mix_thinking = r#loop.mix_thinking;
        }
        if collaboration.node == TriggerNode::Execution {
            collaboration.mix_thinking = false;
        }
        self.collaboration = collaboration;
        self.mode_styles.unni = None;
        self.mode_styles.r#loop = None;
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
        let mut config: Config = toml::from_str(&content).map_err(|e| {
            crate::common::AgentError::Parse(format!("parse config {:?}: {}", path, e))
        })?;
        config.migrate_collaboration();
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
    fn collaboration_defaults() {
        let c = CollaborationSection::default();
        assert_eq!(c.node, TriggerNode::Execution);
        assert!(c.mix_thinking);
    }

    #[test]
    fn mode_styles_defaults_keep_only() {
        let m = ModeStyles::default();
        assert_eq!(m.keep.token_budget, 0);
        assert_eq!(m.keep.time_budget_secs, 0);
        assert!(m.unni.is_none());
        assert!(m.r#loop.is_none());
    }

    #[test]
    fn runtime_styles_mix_auto_off_for_execution_node() {
        let mut c = Config::default_config();
        c.collaboration.node = TriggerNode::Execution;
        c.collaboration.mix_thinking = true;
        let styles = RuntimeStyles::from_config(&c);
        assert!(!styles.mix_on(TriggerNode::Execution));

        c.collaboration.node = TriggerNode::Insight;
        let styles = RuntimeStyles::from_config(&c);
        assert!(styles.mix_on(TriggerNode::Insight));
        assert!(styles.mix_on(TriggerNode::Memory));

        c.collaboration.mix_thinking = false;
        let styles = RuntimeStyles::from_config(&c);
        assert!(!styles.mix_on(TriggerNode::Memory));
    }

    #[test]
    fn trigger_node_serde_roundtrip_lowercase() {
        for (node, text) in [
            (TriggerNode::Execution, "execution"),
            (TriggerNode::Insight, "insight"),
            (TriggerNode::Memory, "memory"),
        ] {
            let encoded = serde_json::to_string(&node).unwrap();
            assert_eq!(encoded, format!("\"{text}\""), "encode {text}");
            let decoded: TriggerNode = serde_json::from_str(&format!("\"{text}\"")).unwrap();
            assert_eq!(decoded, node, "decode {text}");
        }
        assert!(serde_json::from_str::<TriggerNode>("\"quantum\"").is_err());
    }

    #[test]
    fn trigger_node_async_after_semantics() {
        assert_eq!(
            TriggerNode::Execution.async_after(),
            &[TriggerNode::Insight, TriggerNode::Memory]
        );
        assert_eq!(TriggerNode::Insight.async_after(), &[TriggerNode::Memory]);
        assert_eq!(TriggerNode::Memory.async_after(), &[]);
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
    fn mode_styles_unknown_value_fails_parse() {
        assert!(toml::from_str::<ModeStyles>("unni = { node = \"quantum\" }").is_err());
        assert!(toml::from_str::<ModeStyles>("keep = { token_budget = -1 }").is_err());
    }

    #[test]
    fn legacy_unni_node_migrates_to_collaboration_with_old_value_priority() {
        let legacy = r#"
            [collaboration]
            node = "memory"
            mix_thinking = false

            [mode_styles.unni]
            style = "follow"
            node = "insight"

            [mode_styles.loop]
            mix_thinking = true
        "#;
        let mut cfg: Config = toml::from_str(legacy).unwrap();
        cfg.migrate_collaboration();
        assert_eq!(cfg.collaboration.node, TriggerNode::Insight);
        assert!(cfg.collaboration.mix_thinking);
        assert!(cfg.mode_styles.unni.is_none());
        assert!(cfg.mode_styles.r#loop.is_none());
    }

    #[test]
    fn legacy_execution_node_forces_mix_off() {
        let legacy = r#"
            [mode_styles.unni]
            node = "execution"
            [mode_styles.loop]
            mix_thinking = true
        "#;
        let mut cfg: Config = toml::from_str(legacy).unwrap();
        cfg.migrate_collaboration();
        assert_eq!(cfg.collaboration.node, TriggerNode::Execution);
        assert!(!cfg.collaboration.mix_thinking);
    }

    #[test]
    fn load_migrates_legacy_fields_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[mode_styles.unni]\nnode = \"memory\"\n[mode_styles.loop]\nmix_thinking = false\n",
        )
        .unwrap();
        let loaded = Config::load(&path).unwrap().unwrap();
        assert_eq!(loaded.collaboration.node, TriggerNode::Memory);
        assert!(!loaded.collaboration.mix_thinking);
        assert!(loaded.mode_styles.unni.is_none());
        assert!(loaded.mode_styles.r#loop.is_none());
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
            collaboration: CollaborationSection {
                node: TriggerNode::Insight,
                mix_thinking: true,
            },
            mode_styles: ModeStyles::default(),
            default_model: None,
            context: ContextSection::default(),
        };
        cfg.save(&p).expect("save ok");
        let loaded = Config::load(&p).expect("load ok").expect("exists");
        assert_eq!(loaded.provider, "openai");
        assert_eq!(loaded.model_id, "gpt-4");
        assert_eq!(loaded.api_key, "sk-test-roundtrip");
        assert_eq!(loaded.data_dir, PathBuf::from("/tmp/data"));
        assert_eq!(loaded.default_mode, "keep");
        assert_eq!(loaded.collaboration.node, TriggerNode::Insight);
        assert!(loaded.collaboration.mix_thinking);
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
            collaboration: CollaborationSection::default(),
            mode_styles: ModeStyles::default(),
            default_model: None,
            context: ContextSection::default(),
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

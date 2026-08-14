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

    #[serde(default)]
    pub mode_styles: ModeStyles,

    #[serde(default)]
    pub default_model: Option<String>,

    #[serde(default)]
    pub context: ContextSection,
}

fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cipher")
        .join("data")
}

fn default_mode() -> String {
    "unni".to_string()
}

/// 协同方式：中台完成触发思考引擎后的行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CollaborationStyle {
    /// 自主：协同节点完成 → 立即触发思考引擎新实例。
    #[default]
    Autonomous,
    /// 跟随：协同节点完成 → 存为 pending context，等用户下次输入时合并。
    Follow,
}

impl CollaborationStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            CollaborationStyle::Autonomous => "autonomous",
            CollaborationStyle::Follow => "follow",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CollaborationStyle::Autonomous => "自主（立即触发新实例）",
            CollaborationStyle::Follow => "跟随（存为 pending，等用户下次输入合并）",
        }
    }
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

/// 单模式协同风格：协同方式 + 协同节点。
/// 实现形态：形态 B（struct + 每模式默认常量，非 trait）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeStyle {
    pub style: CollaborationStyle,
    pub node: TriggerNode,
}

impl Default for ModeStyle {
    fn default() -> Self {
        Self::UNNI_DEFAULT
    }
}

impl ModeStyle {
    /// UNNI 默认：自主 + 执行中台（用户可配置）。
    pub const UNNI_DEFAULT: Self = Self {
        style: CollaborationStyle::Autonomous,
        node: TriggerNode::Execution,
    };
    /// KEEP 固定：自主 + 洞察中台（不存 config，由常量表达）。
    pub const KEEP_DEFAULT: Self = Self {
        style: CollaborationStyle::Autonomous,
        node: TriggerNode::Insight,
    };
    /// LOOP 固定：自主 + 记忆中台（不存 config，由常量表达）。
    pub const LOOP_DEFAULT: Self = Self {
        style: CollaborationStyle::Autonomous,
        node: TriggerNode::Memory,
    };
}

/// UNNI 协同风格（用户可配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnniStyle {
    #[serde(default = "default_unni_collaboration_style")]
    pub style: CollaborationStyle,
    #[serde(default = "default_unni_trigger_node")]
    pub node: TriggerNode,
}

fn default_unni_collaboration_style() -> CollaborationStyle {
    ModeStyle::UNNI_DEFAULT.style
}
fn default_unni_trigger_node() -> TriggerNode {
    ModeStyle::UNNI_DEFAULT.node
}

impl Default for UnniStyle {
    fn default() -> Self {
        Self {
            style: ModeStyle::UNNI_DEFAULT.style,
            node: ModeStyle::UNNI_DEFAULT.node,
        }
    }
}

impl From<UnniStyle> for ModeStyle {
    fn from(u: UnniStyle) -> Self {
        Self {
            style: u.style,
            node: u.node,
        }
    }
}

/// KEEP 附加设置：成本护栏（协同方式/节点固定为 KEEP_DEFAULT）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepStyle {
    /// Token 预算：100K ~ ∞（默认 100K）。
    #[serde(default = "default_keep_token_budget")]
    pub token_budget: u64,
    /// 时间预算（秒）：300s ~ ∞（默认 5min）。
    #[serde(default = "default_keep_time_budget_secs")]
    pub time_budget_secs: u64,
}

fn default_keep_token_budget() -> u64 {
    100_000
}
fn default_keep_time_budget_secs() -> u64 {
    300
}

impl Default for KeepStyle {
    fn default() -> Self {
        Self {
            token_budget: default_keep_token_budget(),
            time_budget_secs: default_keep_time_budget_secs(),
        }
    }
}

/// LOOP 附加设置：融合思考开关（协同方式/节点固定为 LOOP_DEFAULT）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopStyle {
    /// 融合思考（Mix Thinking）开关，默认 off。
    #[serde(default = "default_loop_mix_thinking")]
    pub mix_thinking: bool,
}

fn default_loop_mix_thinking() -> bool {
    false
}

impl Default for LoopStyle {
    fn default() -> Self {
        Self {
            mix_thinking: default_loop_mix_thinking(),
        }
    }
}

/// 三模式协同风格配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeStyles {
    #[serde(default = "default_unni_style")]
    pub unni: UnniStyle,
    #[serde(default = "default_keep_style")]
    pub keep: KeepStyle,
    #[serde(default = "default_loop_style")]
    pub r#loop: LoopStyle,
}

fn default_unni_style() -> UnniStyle {
    UnniStyle::default()
}
fn default_keep_style() -> KeepStyle {
    KeepStyle::default()
}
fn default_loop_style() -> LoopStyle {
    LoopStyle::default()
}

impl Default for ModeStyles {
    fn default() -> Self {
        Self {
            unni: default_unni_style(),
            keep: default_keep_style(),
            r#loop: default_loop_style(),
        }
    }
}

impl ModeStyles {
    /// 取某模式的协同风格（KEEP/LOOP 返回固定常量，UNNI 返回用户配置）。
    pub fn style_for(&self, mode: &str) -> ModeStyle {
        match mode {
            "keep" => ModeStyle::KEEP_DEFAULT,
            "loop" => ModeStyle::LOOP_DEFAULT,
            _ => self.unni.into(),
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
        }
    }

    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cipher")
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
        assert!(d.ends_with("cipher/data"), "got: {d:?}");
    }

    #[test]
    fn default_mode_is_unni() {
        assert_eq!(default_mode(), "unni");
    }

    #[test]
    fn mode_styles_defaults() {
        let m = ModeStyles::default();
        assert_eq!(m.unni.style, CollaborationStyle::Autonomous);
        assert_eq!(m.unni.node, TriggerNode::Execution);
        assert_eq!(m.keep.token_budget, 100_000);
        assert_eq!(m.keep.time_budget_secs, 300);
        assert!(!m.r#loop.mix_thinking);
    }

    #[test]
    fn mode_style_default_constants() {
        assert_eq!(ModeStyle::UNNI_DEFAULT.node, TriggerNode::Execution);
        assert_eq!(ModeStyle::KEEP_DEFAULT.node, TriggerNode::Insight);
        assert_eq!(ModeStyle::LOOP_DEFAULT.node, TriggerNode::Memory);
        // 三模式协同方式均为自主
        assert_eq!(
            ModeStyle::UNNI_DEFAULT.style,
            CollaborationStyle::Autonomous
        );
        assert_eq!(
            ModeStyle::KEEP_DEFAULT.style,
            CollaborationStyle::Autonomous
        );
        assert_eq!(
            ModeStyle::LOOP_DEFAULT.style,
            CollaborationStyle::Autonomous
        );
    }

    #[test]
    fn style_for_returns_fixed_constants_for_keep_loop() {
        let m = ModeStyles {
            unni: UnniStyle {
                style: CollaborationStyle::Follow,
                node: TriggerNode::Insight,
            },
            ..ModeStyles::default()
        };
        // UNNI 用用户配置
        assert_eq!(m.style_for("unni"), m.unni.into());
        // KEEP/LOOP 用固定常量，忽略任何配置
        assert_eq!(m.style_for("keep"), ModeStyle::KEEP_DEFAULT);
        assert_eq!(m.style_for("loop"), ModeStyle::LOOP_DEFAULT);
    }

    #[test]
    fn collaboration_style_serde_roundtrip_lowercase() {
        for (style, text) in [
            (CollaborationStyle::Autonomous, "autonomous"),
            (CollaborationStyle::Follow, "follow"),
        ] {
            let encoded = serde_json::to_string(&style).unwrap();
            assert_eq!(encoded, format!("\"{text}\""), "encode {text}");
            let decoded: CollaborationStyle = serde_json::from_str(&format!("\"{text}\"")).unwrap();
            assert_eq!(decoded, style, "decode {text}");
        }
        assert!(serde_json::from_str::<CollaborationStyle>("\"quantum\"").is_err());
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
        // 协同节点后的中台异步执行，只沉淀记忆不触发
        assert_eq!(
            TriggerNode::Execution.async_after(),
            &[TriggerNode::Insight, TriggerNode::Memory]
        );
        assert_eq!(TriggerNode::Insight.async_after(), &[TriggerNode::Memory]);
        assert_eq!(TriggerNode::Memory.async_after(), &[]);
    }

    #[test]
    fn mode_styles_toml_roundtrip() {
        // 默认值在 config.toml 中可省略
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.mode_styles.unni.style, CollaborationStyle::Autonomous);
        assert_eq!(cfg.mode_styles.unni.node, TriggerNode::Execution);
        assert_eq!(cfg.mode_styles.keep.token_budget, 100_000);
        assert!(!cfg.mode_styles.r#loop.mix_thinking);

        // 显式配置 roundtrip
        let explicit = r#"
            [mode_styles.unni]
            style = "follow"
            node = "insight"
            [mode_styles.keep]
            token_budget = 200000
            time_budget_secs = 600
            [mode_styles.loop]
            mix_thinking = true
        "#;
        let cfg: Config = toml::from_str(explicit).unwrap();
        assert_eq!(cfg.mode_styles.unni.style, CollaborationStyle::Follow);
        assert_eq!(cfg.mode_styles.unni.node, TriggerNode::Insight);
        assert_eq!(cfg.mode_styles.keep.token_budget, 200_000);
        assert_eq!(cfg.mode_styles.keep.time_budget_secs, 600);
        assert!(cfg.mode_styles.r#loop.mix_thinking);

        let serialized = toml::to_string(&cfg.mode_styles).unwrap();
        let decoded: ModeStyles = toml::from_str(&serialized).unwrap();
        assert_eq!(decoded, cfg.mode_styles);
    }

    #[test]
    fn keep_style_defaults() {
        let b = KeepStyle::default();
        assert_eq!(b.token_budget, 100_000);
        assert_eq!(b.time_budget_secs, 300);
    }

    #[test]
    fn loop_style_defaults() {
        assert!(!LoopStyle::default().mix_thinking);
    }

    #[test]
    fn mode_styles_unknown_value_fails_parse() {
        assert!(toml::from_str::<ModeStyles>("unni = { style = \"quantum\" }").is_err());
        assert!(toml::from_str::<ModeStyles>("unni = { node = \"quantum\" }").is_err());
        assert!(toml::from_str::<ModeStyles>("keep = { token_budget = -1 }").is_err());
    }

    #[test]
    fn default_path_ends_with_cipher_config_toml() {
        let p = Config::default_path();
        let s = p.to_string_lossy();
        assert!(s.ends_with("cipher/config.toml"), "got: {s}");
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
            mode_styles: ModeStyles {
                unni: UnniStyle {
                    style: CollaborationStyle::Follow,
                    node: TriggerNode::Insight,
                },
                ..ModeStyles::default()
            },
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
        assert_eq!(loaded.mode_styles.unni.style, CollaborationStyle::Follow);
        assert_eq!(loaded.mode_styles.unni.node, TriggerNode::Insight);
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

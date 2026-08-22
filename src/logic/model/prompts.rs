use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

static PROMPT_CACHE: OnceLock<RwLock<HashMap<PathBuf, String>>> = OnceLock::new();

fn prompt_cache() -> &'static RwLock<HashMap<PathBuf, String>> {
    PROMPT_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

pub const SOUL_DEFAULT: &str = include_str!("../../../prompts/SOUL.md");
pub const SYSTEM_DEFAULT: &str = include_str!("../../../prompts/system.md");
pub const MODE_UNNI_DEFAULT: &str = include_str!("../../../prompts/mode_unni.md");
pub const MODE_KEEP_DEFAULT: &str = include_str!("../../../prompts/mode_keep.md");
pub const MODE_LOOP_DEFAULT: &str = include_str!("../../../prompts/mode_loop.md");
pub const EXECUTION_PLATFORM_DEFAULT: &str = include_str!("../../../prompts/execution_platform.md");
pub const INSIGHT_PLATFORM_DEFAULT: &str = include_str!("../../../prompts/insight_platform.md");
pub const MEMORY_ATTENTION_DEFAULT: &str = include_str!("../../../prompts/memory_attention.md");
pub const MEMORY_EXPERIENCE_DEFAULT: &str = include_str!("../../../prompts/memory_experience.md");
pub const MEMORY_PREFERENCE_DEFAULT: &str = include_str!("../../../prompts/memory_preference.md");
pub const MEMORY_COGNITIVE_DEFAULT: &str = include_str!("../../../prompts/memory_cognitive.md");
pub const THINK_ENGINE_DEFAULT: &str = include_str!("../../../prompts/think_engine.md");
pub const SAY_ENGINE_DEFAULT: &str = include_str!("../../../prompts/say_engine.md");

/// 能力调用规范统一片段（v0.3.1 §8）。
///
/// **不纳入 DEFAULT_PROMPTS 全局拼接**：仅当 agent 的 available_capabilities 非空时，
/// 通过 `compose_agent_capability_prompt` 按需拼接到记忆 agent / subagent 模板 prompt。
pub const CAPABILITY_CALL_DEFAULT: &str = include_str!("../../../prompts/capability_call.md");

/// 能力表注入条目（LLM 可见的能力元信息：id / name / description）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityPromptEntry {
    pub capability_id: String,
    pub capability_name: String,
    pub description: String,
}

/// 按需组装能力调用提示词片段。
///
/// 仅当 `available` 非空时把 `prompts/capability_call.md` 的固定片段与可用能力表拼接到
/// `base` 之后；`available` 为空时原样返回 `base`（不加载片段）。
/// 记忆 agent 与 subagent 模板 prompt 必须通过本函数统一拼接，不得复制第二套协议文本。
pub fn compose_agent_capability_prompt(base: &str, available: &[CapabilityPromptEntry]) -> String {
    if available.is_empty() {
        return base.to_string();
    }
    let mut table = String::new();
    for entry in available {
        table.push_str(&format!(
            "- `{}` / {}: {}\n",
            entry.capability_id, entry.capability_name, entry.description
        ));
    }
    format!("{base}\n\n## 可用能力\n{table}{CAPABILITY_CALL_DEFAULT}")
}

pub const DEFAULT_PROMPTS: [(&str, &str); 13] = [
    ("system.md", SYSTEM_DEFAULT),
    ("SOUL.md", SOUL_DEFAULT),
    ("mode_unni.md", MODE_UNNI_DEFAULT),
    ("mode_keep.md", MODE_KEEP_DEFAULT),
    ("mode_loop.md", MODE_LOOP_DEFAULT),
    ("execution_platform.md", EXECUTION_PLATFORM_DEFAULT),
    ("insight_platform.md", INSIGHT_PLATFORM_DEFAULT),
    ("memory_attention.md", MEMORY_ATTENTION_DEFAULT),
    ("memory_experience.md", MEMORY_EXPERIENCE_DEFAULT),
    ("memory_preference.md", MEMORY_PREFERENCE_DEFAULT),
    ("memory_cognitive.md", MEMORY_COGNITIVE_DEFAULT),
    ("think_engine.md", THINK_ENGINE_DEFAULT),
    ("say_engine.md", SAY_ENGINE_DEFAULT),
];

fn read_prompt(prompts_dir: &Path, name: &str) -> String {
    let path = prompts_dir.join(name);
    if let Ok(cache) = prompt_cache().read() {
        if let Some(content) = cache.get(&path) {
            return content.clone();
        }
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    if let Ok(mut cache) = prompt_cache().write() {
        cache.insert(path, content.clone());
    }
    content
}

#[cfg(test)]
pub fn prompt_cache_len() -> usize {
    prompt_cache().read().map(|c| c.len()).unwrap_or(0)
}

pub fn clear_prompt_cache() {
    if let Ok(mut cache) = prompt_cache().write() {
        cache.clear();
    }
}

pub fn read_platform_prompt(prompts_dir: &Path, name: &str) -> String {
    let content = read_prompt(prompts_dir, name);
    // T3：文件缺失/为空时回退 DEFAULT_PROMPTS 内嵌默认（记忆 agent 无 prompts_dir 时的
    // 角色/判断标准兜底；md 与内嵌常量同源 include_str，语义一致）。
    if content.trim().is_empty() {
        if let Some((_, default)) = DEFAULT_PROMPTS.iter().find(|(n, _)| *n == name) {
            return default.to_string();
        }
    }
    content
}

#[cfg(test)]
pub fn compose_prompt(prompts_dir: &Path, mode: &str) -> String {
    let system = read_prompt(prompts_dir, "system.md");
    let soul = read_prompt(prompts_dir, "SOUL.md");
    let mode_specific = match mode {
        "unni" => read_prompt(prompts_dir, "mode_unni.md"),
        "keep" => read_prompt(prompts_dir, "mode_keep.md"),
        "loop" => read_prompt(prompts_dir, "mode_loop.md"),
        _ => return String::new(),
    };
    format!("{system}\n\n{soul}\n\n{mode_specific}")
}

/// 双脑模式提示词组装：Think / Say 引擎 + 当前模式一行说明。
pub fn compose_dual_prompt(prompts_dir: &Path, role: &str, mode: &str) -> String {
    let engine = match role {
        "think" => {
            let p = read_prompt(prompts_dir, "think_engine.md");
            if p.trim().is_empty() {
                THINK_ENGINE_DEFAULT.to_string()
            } else {
                p
            }
        }
        "say" => {
            let p = read_prompt(prompts_dir, "say_engine.md");
            if p.trim().is_empty() {
                SAY_ENGINE_DEFAULT.to_string()
            } else {
                p
            }
        }
        _ => return String::new(),
    };
    let soul = read_prompt(prompts_dir, "SOUL.md");
    let soul = if soul.trim().is_empty() {
        SOUL_DEFAULT.to_string()
    } else {
        soul
    };
    let mode_line = match mode {
        "unni" => "Current mode: UNNI — collaborative, concise, user-facing; think and say are both allowed, at least one non-empty.",
        "keep" => "Current mode: KEEP — autonomous execution; think is required, say is allowed at most once and only for alignment or final delivery.",
        "loop" => "Current mode: LOOP — continuous autonomous iteration; think is required, say is forbidden.",
        _ => "",
    };
    if mode_line.is_empty() {
        format!("{engine}\n\n{soul}")
    } else {
        format!("{engine}\n\n{soul}\n\n{mode_line}")
    }
}

#[cfg(test)]
pub fn estimate_tokens(prompts_dir: &Path, mode: &str) -> usize {
    compose_prompt(prompts_dir, mode).chars().count() / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_prompt_contains_core_sections_for_all_modes() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
        for mode in ["unni", "keep", "loop"] {
            let p = compose_prompt(&dir, mode);

            assert!(
                p.contains("cipher"),
                "compose_prompt({mode}) missing cipher"
            );
        }
    }

    #[test]
    fn capability_call_prompt_is_not_part_of_default_prompts() {
        assert!(
            !DEFAULT_PROMPTS
                .iter()
                .any(|(name, _)| *name == "capability_call.md"),
            "capability_call.md 必须按需加载，不得进入 DEFAULT_PROMPTS 全局拼接"
        );
        assert!(!CAPABILITY_CALL_DEFAULT.trim().is_empty());
    }

    #[test]
    fn compose_agent_capability_prompt_only_loads_when_available_non_empty() {
        let base = "角色提示词";
        let empty = compose_agent_capability_prompt(base, &[]);
        assert_eq!(empty, base);
        assert!(!empty.contains("可用能力"));

        let available = vec![CapabilityPromptEntry {
            capability_id: "file.read".to_string(),
            capability_name: "Read File".to_string(),
            description: "读取文件".to_string(),
        }];
        let composed = compose_agent_capability_prompt(base, &available);
        assert!(composed.contains("file.read"));
        assert!(composed.contains("Read File"));
        assert!(composed.contains("capability_call"));
        assert!(composed.contains("capability_calls"));
        assert!(composed.contains("done"));
        assert!(composed.starts_with(base));
    }

    #[test]
    fn default_prompts_have_no_dev_or_internal_architecture_references() {
        const FORBIDDEN: &[&str] = &[
            "ADR",
            "iter78",
            "per spec",
            "设计点",
            "五态",
            "中台",
            "spec 4",
            "iter59",
            "ADR-095",
            "ADR-064",
            "ADR-091",
        ];
        for (name, content) in DEFAULT_PROMPTS {
            for &word in FORBIDDEN {
                assert!(
                    !content.contains(word),
                    "{name} 不应包含开发/内部架构引用: '{word}'"
                );
            }
        }
    }

    #[test]
    fn default_prompts_include_dual_engine_files_with_concise_io_guidance() {
        assert!(DEFAULT_PROMPTS.iter().any(|(n, _)| *n == "think_engine.md"));
        assert!(DEFAULT_PROMPTS.iter().any(|(n, _)| *n == "say_engine.md"));
        assert!(!THINK_ENGINE_DEFAULT.trim().is_empty());
        assert!(!SAY_ENGINE_DEFAULT.trim().is_empty());
    }

    #[test]
    fn compose_dual_prompt_contains_engine_io_guidance_and_mode_line() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
        let think = compose_dual_prompt(&dir, "think", "unni");
        assert!(think.contains("Think Engine"));
        assert!(think.contains("Input:"));
        assert!(think.contains("Output:"));
        assert!(think.contains("Current mode: UNNI"));

        let say = compose_dual_prompt(&dir, "say", "loop");
        assert!(say.contains("Say Engine"));
        assert!(say.contains("Input:"));
        assert!(say.contains("Output:"));
        assert!(say.contains("Current mode: LOOP"));
        assert!(say.contains("say is forbidden"));
    }
}

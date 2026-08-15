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

pub const DEFAULT_PROMPTS: [(&str, &str); 11] = [
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

#[cfg(test)]
pub fn clear_prompt_cache() {
    if let Ok(mut cache) = prompt_cache().write() {
        cache.clear();
    }
}

pub fn read_platform_prompt(prompts_dir: &Path, name: &str) -> String {
    read_prompt(prompts_dir, name)
}

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

pub fn estimate_tokens(prompts_dir: &Path, mode: &str) -> usize {
    compose_prompt(prompts_dir, mode).chars().count() / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_prompt_unni_contains_markers() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
        let p = compose_prompt(&dir, "unni");
        assert!(p.contains("UNNI"), "compose_prompt(unni) missing 'UNNI'");
        assert!(
            p.contains("cipher"),
            "compose_prompt(unni) missing 'cipher'"
        );
    }

    #[test]
    fn compose_prompt_keep_contains_markers() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
        let p = compose_prompt(&dir, "keep");
        assert!(p.contains("KEEP"), "compose_prompt(keep) missing 'KEEP'");

        assert!(
            p.contains("整个连续期间最多一次"),
            "compose_prompt(keep) missing shared say quota"
        );
    }

    #[test]
    fn compose_prompt_loop_contains_markers() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
        let p = compose_prompt(&dir, "loop");
        assert!(p.contains("LOOP"), "compose_prompt(loop) missing 'LOOP'");
        assert!(
            p.contains("禁止 `say`"),
            "compose_prompt(loop) missing say prohibition"
        );
    }

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
    fn compose_prompt_3_modes_are_distinct() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
        let unni = compose_prompt(&dir, "unni");
        let keep = compose_prompt(&dir, "keep");
        let loop_p = compose_prompt(&dir, "loop");
        assert_ne!(unni, keep, "UNNI == KEEP 提示词 (per P1.5 应互不相同)");
        assert_ne!(unni, loop_p, "UNNI == LOOP 提示词 (per P1.5 应互不相同)");
        assert_ne!(keep, loop_p, "KEEP == LOOP 提示词 (per P1.5 应互不相同)");
    }

    #[test]
    fn default_prompts_have_no_dev_or_internal_architecture_references() {
        const FORBIDDEN: &[&str] = &[
            "ADR",
            "iter",
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
}

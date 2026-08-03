use std::path::Path;

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
    std::fs::read_to_string(&path).unwrap_or_default()
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
        assert!(p.contains("五态"), "compose_prompt(unni) missing '五态'");
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
        assert!(p.contains("飞轮"), "compose_prompt(loop) missing '飞轮'");
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
}

use super::config::Config;
#[cfg(test)]
use super::config::{ContextSection, ModeStyles};
use crate::common::AgentError;
use std::path::Path;

pub fn init(config_path: &Path) -> Result<Config, AgentError> {
    if let Some(config) = Config::load(config_path)? {
        tracing::info!(path = ?config_path, "config loaded from disk");
        return Ok(config);
    }

    tracing::info!(path = ?config_path, "config not found, 首启建默认 config (data_dir + default_mode)");
    let config = Config::default_config();
    config.save(config_path)?;
    tracing::info!(path = ?config_path, "config saved (chmod 600, 废弃字段空)");
    Ok(config)
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn init_with_existing_config_loads_without_prompt() {
        let tmp =
            std::env::temp_dir().join(format!("cipher-init-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("config.toml");

        let saved = Config {
            provider: "openai".into(),
            model_id: "gpt-4o".into(),
            api_key: "sk-test".into(),
            data_dir: PathBuf::from("/tmp/data"),
            default_mode: "keep".into(),
            mode_styles: ModeStyles::default(),
            default_model: None,
            context: ContextSection::default(),
            ui: crate::startup::config::UiSection::default(),
            web: crate::startup::config::WebSection::default(),
        };
        saved.save(&path).unwrap();

        let loaded = init(&path).expect("init loads existing");
        assert_eq!(loaded.provider, "openai");
        assert_eq!(loaded.default_mode, "keep");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn init_creates_default_config_when_missing() {
        let tmp = std::env::temp_dir().join(format!(
            "cipher-init-default-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("config.toml");
        assert!(!path.exists());

        let created = init(&path).expect("init creates default");

        assert!(created.provider.is_empty());
        assert!(created.model_id.is_empty());
        assert!(created.api_key.is_empty());

        assert_eq!(created.default_mode, "unni");
        assert!(created.data_dir.to_string_lossy().ends_with("data"));

        assert!(path.exists());

        std::fs::remove_dir_all(&tmp).ok();
    }
}

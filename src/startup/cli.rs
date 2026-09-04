use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "cipher", about = "终端原生 AI 代理", version)]
pub struct Cli {
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum Commands {
    Setup,

    Run,

    Config,

    #[command(name = "workspace", subcommand)]
    Workspace(WorkspaceCommand),
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceCommand {
    List,
    Add { path: PathBuf },
    Delete { id: String },
    Use { id: String },
    SetDefault { id: String },
}

pub fn parse() -> Cli {
    Cli::parse()
}

pub fn parse_command() -> Commands {
    Cli::parse().command.unwrap_or(Commands::Run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_defaults_to_run() {
        let cli = Cli::try_parse_from(["cipher"]).unwrap();
        assert_eq!(cli.command.unwrap_or(Commands::Run), Commands::Run);
    }

    #[test]
    fn setup_subcommand() {
        let cli = Cli::try_parse_from(["cipher", "setup"]).unwrap();
        assert_eq!(cli.command, Some(Commands::Setup));
    }

    #[test]
    fn config_subcommand() {
        let cli = Cli::try_parse_from(["cipher", "config"]).unwrap();
        assert_eq!(cli.command, Some(Commands::Config));
    }

    #[test]
    fn run_subcommand_explicit() {
        let cli = Cli::try_parse_from(["cipher", "run"]).unwrap();
        assert_eq!(cli.command, Some(Commands::Run));
    }

    #[test]
    fn data_dir_global_flag() {
        let cli = Cli::try_parse_from(["cipher", "--data-dir", "/tmp/x", "setup"]).unwrap();
        assert_eq!(
            cli.data_dir.as_deref(),
            Some(std::path::Path::new("/tmp/x"))
        );
        assert_eq!(cli.command, Some(Commands::Setup));
    }

    #[test]
    fn config_flag_no_subcommand() {
        let cli = Cli::try_parse_from(["cipher", "--config", "/tmp/c.toml"]).unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/c.toml"))
        );
        assert_eq!(cli.command.unwrap_or(Commands::Run), Commands::Run);
    }

    #[test]
    fn unknown_subcommand_errors() {
        assert!(Cli::try_parse_from(["cipher", "bogus"]).is_err());
    }

    #[test]
    fn parse_command_defaults_run() {
        let cli = Cli::try_parse_from(["cipher"]).unwrap();
        assert_eq!(cli.command.unwrap_or(Commands::Run), Commands::Run);
    }

    // ---- v0.5.0 workspace 子命令族（任务书 §5）----

    #[test]
    fn workspace_list_subcommand() {
        let cli = Cli::try_parse_from(["cipher", "workspace", "list"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Workspace(WorkspaceCommand::List))
        );
    }

    #[test]
    fn workspace_add_subcommand_takes_path() {
        let cli = Cli::try_parse_from(["cipher", "workspace", "add", "/tmp/project-a"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Workspace(WorkspaceCommand::Add {
                path: PathBuf::from("/tmp/project-a")
            }))
        );
    }

    #[test]
    fn workspace_delete_subcommand_takes_id() {
        let cli = Cli::try_parse_from(["cipher", "workspace", "delete", "project-a"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Workspace(WorkspaceCommand::Delete {
                id: "project-a".to_string()
            }))
        );
    }

    #[test]
    fn workspace_use_subcommand_takes_id() {
        let cli = Cli::try_parse_from(["cipher", "workspace", "use", "project-a"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Workspace(WorkspaceCommand::Use {
                id: "project-a".to_string()
            }))
        );
    }

    #[test]
    fn workspace_set_default_subcommand_takes_id() {
        let cli = Cli::try_parse_from(["cipher", "workspace", "set-default", "project-a"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Workspace(WorkspaceCommand::SetDefault {
                id: "project-a".to_string()
            }))
        );
    }

    #[test]
    fn workspace_missing_subcommand_errors() {
        assert!(Cli::try_parse_from(["cipher", "workspace"]).is_err());
        assert!(Cli::try_parse_from(["cipher", "workspace", "list", "extra"]).is_err());
        assert!(Cli::try_parse_from(["cipher", "workspace", "add"]).is_err());
    }
}

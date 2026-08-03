use cipher::common::AgentError;
use cipher::startup;
use cipher::startup::cli::{parse, Commands};
use cipher::startup::config::Config;

#[tokio::main]
async fn main() {
    let cli = parse();
    let config_path = cli.config.unwrap_or_else(Config::default_path);
    let data_dir = cli.data_dir;
    let cmd = cli.command.unwrap_or(Commands::Run);

    let result = match cmd {
        Commands::Setup => startup::entry::run_setup(config_path, data_dir).await,
        Commands::Run => startup::entry::run_normal(config_path, data_dir).await,
        Commands::Config => startup::entry::run_config(config_path, data_dir).await,
    };

    if let Err(e) = result {
        eprintln!("cipher failed: {}", e);
        let exit_code = match e {
            AgentError::StartupFailed(_) => 1,
            AgentError::Bootstrap(_) => 2,
            _ => 3,
        };
        std::process::exit(exit_code);
    }
}

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
        Commands::Workspace(cmd) => {
            startup::entry::run_workspace_command(cmd, config_path, data_dir).await
        }
        // v0.5.5 看门狗：静默叶子进程，绝不 bootstrap（不碰数据库/日志），
        // 只做一件事——stdin（管道读端）EOF 即主进程死亡，清算在册子进程树。
        Commands::Watchdog => {
            #[cfg(unix)]
            unsafe {
                // 必须比主进程活得久：终端 Ctrl+C / 常规终止信号不应把它带走。
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            let stdin = std::io::stdin();
            cipher::common::watchdog::run(stdin.lock())
                .map_err(|e| AgentError::Io(format!("watchdog: {e}")))
        }
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

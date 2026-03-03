mod commands;
mod tui;

use clap::Parser;

#[derive(Parser)]
#[command(name = "enki", about = "Multi-agent orchestrator for ACP coding agents")]
enum Cli {
    /// Launch the interactive TUI (default when no subcommand given).
    Tui,
    /// Initialize the enki workspace database.
    Init,
    /// Manage projects.
    Project {
        #[command(subcommand)]
        cmd: commands::ProjectCmd,
    },
    /// Manage tasks.
    Task {
        #[command(subcommand)]
        cmd: commands::TaskCmd,
    },
    /// Run a workflow template, creating tasks and wiring dependencies.
    Exec {
        #[command(subcommand)]
        cmd: commands::ExecCmd,
    },
    /// Run a single task via an ACP agent.
    Run {
        /// Task ID to run.
        task_id: String,
        /// Agent command (default: "bunx").
        #[arg(long, default_value = "bunx")]
        agent: String,
        /// Additional agent args.
        #[arg(long, default_value = "@zed-industries/claude-code-acp")]
        agent_args: String,
        /// Keep the worktree after the run instead of cleaning it up.
        #[arg(long)]
        keep: bool,
    },
    /// Show workspace status.
    Status,
}

impl Default for Cli {
    fn default() -> Self {
        Cli::Tui
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::try_parse().unwrap_or_default();

    let is_tui = matches!(cli, Cli::Tui);
    // Keep the guard alive for the entire process — dropping it flushes
    // and closes the background writer thread.
    let _log_guard = init_logging(is_tui);

    // Resolve our own binary path once so spawned agents can call `enki`
    // via $ENKI_BIN regardless of the user's PATH.
    let enki_bin = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|e| {
            eprintln!("warning: could not resolve enki binary path: {e}");
            std::path::PathBuf::from("enki")
        });

    let result = match cli {
        Cli::Tui => {
            let db_path = commands::db_path();
            let db_path_str = db_path.to_str().unwrap().to_string();
            let db = match commands::open_db() {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            tui::run(db, db_path_str, enki_bin).await
        }
        Cli::Init => commands::init().await,
        Cli::Project { cmd } => commands::project(cmd).await,
        Cli::Task { cmd } => commands::task(cmd).await,
        Cli::Exec { cmd } => commands::exec(cmd).await,
        Cli::Run {
            task_id,
            agent,
            agent_args,
            keep,
        } => commands::run(&task_id, &agent, &agent_args, keep, enki_bin).await,
        Cli::Status => commands::status().await,
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Initialize the tracing subscriber.
///
/// TUI mode: logs to `~/.enki/logs/enki.log` (file only — stderr would
/// corrupt the raw-mode terminal). Returns a WorkerGuard that must stay
/// alive for the duration of the process.
///
/// Non-TUI mode: logs to stderr as before. Returns None.
fn init_logging(is_tui: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    if is_tui {
        let log_dir = home::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".enki")
            .join("logs");
        std::fs::create_dir_all(&log_dir).ok();

        let file_appender = tracing_appender::rolling::never(&log_dir, "enki.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new("enki=debug,enki_core=debug,enki_acp=debug")
                }),
            )
            .init();

        tracing::info!("══════════════════ SESSION START ══════════════════");
        Some(guard)
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("enki=info".parse().unwrap()),
            )
            .init();
        None
    }
}

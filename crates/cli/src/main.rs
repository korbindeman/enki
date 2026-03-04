mod commands;
mod tui;

use clap::Parser;

#[derive(Parser)]
#[command(name = "enki", about = "Multi-agent orchestrator for ACP coding agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Initialize enki in the current project.
    Init,
    /// Manage tasks.
    Task {
        #[command(subcommand)]
        cmd: commands::TaskCmd,
    },
    /// Run a single task via an ACP agent.
    Run {
        /// Task ID to run.
        task_id: String,
        /// Agent command (default: "bunx").
        #[arg(long, default_value = "bunx")]
        agent: String,
        /// Additional agent args.
        #[arg(long, default_value = "@zed-industries/claude-agent-acp")]
        agent_args: String,
        /// Keep the copy after the run instead of cleaning it up.
        #[arg(long)]
        keep: bool,
    },
    /// Show workspace status.
    Status,
    /// Show past session history.
    History {
        /// Session ID to show details for.
        session_id: Option<String>,
    },
    /// Diagnose project health (copies, agent, logs).
    Doctor,
    /// Stop all running workers immediately.
    Stop,
    /// Run as an MCP stdio server (used by ACP agents, not for direct use).
    Mcp {
        /// Agent role: planner, merger, or worker. Controls which tools are exposed.
        #[arg(long, default_value = "planner")]
        role: String,
        /// Task ID for worker-role processes (used by enki_worker_report).
        #[arg(long)]
        task_id: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let is_tui = cli.cmd.is_none();
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

    let result = match cli.cmd {
        None => {
            // Bare `enki` launches the TUI. Auto-initialize if needed.
            if let Err(e) = commands::init().await {
                eprintln!("error: failed to initialize project: {e}");
                std::process::exit(1);
            }

            match commands::db_path().and_then(|p| {
                let db_path_str = p.to_str().unwrap().to_string();
                let db = commands::open_db()?;
                Ok((db, db_path_str))
            }) {
                Ok((db, db_path_str)) => tui::run(db, db_path_str, enki_bin).await,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Cmd::Init) => commands::init().await,
        Some(Cmd::Task { cmd }) => commands::task(cmd).await,
        Some(Cmd::Run {
            task_id,
            agent,
            agent_args,
            keep,
        }) => commands::run(&task_id, &agent, &agent_args, keep, enki_bin).await,
        Some(Cmd::Status) => commands::status().await,
        Some(Cmd::History { session_id }) => commands::history(session_id).await,
        Some(Cmd::Doctor) => commands::doctor().await,
        Some(Cmd::Stop) => commands::stop().await,
        Some(Cmd::Mcp { role, task_id }) => commands::mcp::run(&role, task_id.as_deref()),
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
        let log_dir = commands::global_dir().join("logs");
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

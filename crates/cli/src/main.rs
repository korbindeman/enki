mod commands;

use clap::Parser;

#[derive(Parser)]
#[command(name = "enki", about = "Multi-agent orchestrator for ACP coding agents")]
enum Cli {
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
    },
    /// Show workspace status.
    Status,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("enki=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    let result = match cli {
        Cli::Init => commands::init().await,
        Cli::Project { cmd } => commands::project(cmd).await,
        Cli::Task { cmd } => commands::task(cmd).await,
        Cli::Run {
            task_id,
            agent,
            agent_args,
        } => commands::run(&task_id, &agent, &agent_args).await,
        Cli::Status => commands::status().await,
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

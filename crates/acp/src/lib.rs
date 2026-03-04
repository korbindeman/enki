pub mod client;
pub mod manager;

/// Re-export ACP schema types needed by callers (e.g., MCP server config).
pub use agent_client_protocol as acp_schema;

pub use manager::AgentManager;

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("acp protocol: {0}")]
    Protocol(#[from] agent_client_protocol::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("agent process exited")]
    ProcessExited,
}

pub type Result<T> = std::result::Result<T, AcpError>;

/// Callback for streaming session updates from the agent.
/// First argument is the session ID that produced the update.
pub type UpdateCallback = Box<dyn Fn(&str, SessionUpdate) + 'static>;

/// Simplified session update for the orchestrator.
#[derive(Debug, Clone)]
pub enum SessionUpdate {
    /// Agent produced text output.
    Text(String),
    /// Agent started a tool call.
    ToolCallStarted {
        id: String,
        title: String,
    },
    /// Agent finished a tool call.
    ToolCallDone {
        id: String,
    },
    /// Agent updated its plan.
    Plan(serde_json::Value),
}

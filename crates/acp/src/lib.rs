use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use agent_client_protocol as acp;
use tokio::process::Command;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("acp protocol: {0}")]
    Protocol(#[from] acp::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("agent process exited")]
    ProcessExited,
}

pub type Result<T> = std::result::Result<T, AcpError>;

/// Callback for streaming session updates from the agent.
pub type UpdateCallback = Box<dyn Fn(SessionUpdate) + 'static>;

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

/// A handle to an active ACP session and its owning agent process.
struct SessionEntry {
    conn: Rc<acp::ClientSideConnection>,
    _child_ref: Rc<RefCell<Option<tokio::process::Child>>>,
}

/// Manages ACP agent processes and sessions.
///
/// All methods must be called from within a `tokio::task::LocalSet` because
/// ACP futures are `!Send`.
pub struct AgentManager {
    sessions: Rc<RefCell<HashMap<String, SessionEntry>>>,
    update_callback: Rc<RefCell<Option<UpdateCallback>>>,
    /// Permission auto-approve (for non-interactive orchestrator use).
    auto_approve_permissions: bool,
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            sessions: Rc::new(RefCell::new(HashMap::new())),
            update_callback: Rc::new(RefCell::new(None)),
            auto_approve_permissions: true,
        }
    }

    /// Set a callback to receive session updates from all agents.
    pub fn on_update(&self, callback: impl Fn(SessionUpdate) + 'static) {
        *self.update_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Spawn a new agent process, create a session, and return the session ID.
    ///
    /// `agent_cmd` is the command to run (e.g., "claude" or "npx").
    /// `agent_args` are additional arguments (e.g., ["--acp"] or ["@zed-industries/claude-code-acp"]).
    /// `cwd` is the working directory for the session (typically a git worktree path).
    pub async fn start_session(
        &self,
        agent_cmd: &str,
        agent_args: &[&str],
        cwd: PathBuf,
    ) -> Result<String> {
        // Spawn agent subprocess
        let mut child = Command::new(agent_cmd)
            .args(agent_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child.stdin.take().unwrap().compat_write();
        let stdout = child.stdout.take().unwrap().compat();

        // Create ACP client
        let client = EnkiClient {
            update_callback: self.update_callback.clone(),
            auto_approve: self.auto_approve_permissions,
        };

        let (conn, handle_io) = acp::ClientSideConnection::new(client, stdin, stdout, |fut| {
            tokio::task::spawn_local(fut);
        });
        let conn = Rc::new(conn);

        // Drive the I/O loop in background
        tokio::task::spawn_local(async move {
            if let Err(e) = handle_io.await {
                tracing::error!("ACP I/O error: {e}");
            }
        });

        // Initialize ACP handshake
        acp::Agent::initialize(
            conn.as_ref(),
            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(
                            acp::FileSystemCapability::new()
                                .read_text_file(true)
                                .write_text_file(true),
                        )
                        .terminal(true),
                )
                .client_info(acp::Implementation::new("enki", env!("CARGO_PKG_VERSION")).title("Enki Orchestrator")),
        )
        .await?;

        // Create session
        let session_resp =
            acp::Agent::new_session(conn.as_ref(), acp::NewSessionRequest::new(cwd))
                .await?;

        let session_id = session_resp.session_id.to_string();
        tracing::info!(session_id, "ACP session created");

        // Store session
        let child_ref = Rc::new(RefCell::new(Some(child)));
        self.sessions.borrow_mut().insert(
            session_id.clone(),
            SessionEntry {
                conn: conn.clone(),
                _child_ref: child_ref,
            },
        );

        Ok(session_id)
    }

    /// Send a prompt to an existing session and wait for completion.
    /// Returns the stop reason as a string.
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<String> {
        let conn = {
            let sessions = self.sessions.borrow();
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| AcpError::SessionNotFound(session_id.to_string()))?;
            entry.conn.clone()
        };

        let resp = acp::Agent::prompt(
            conn.as_ref(),
            acp::PromptRequest::new(
                acp::SessionId::from(session_id.to_string()),
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    text.to_string(),
                ))],
            ),
        )
        .await?;

        Ok(format!("{:?}", resp.stop_reason))
    }

    /// Cancel a running prompt on a session (soft cancel — session stays alive).
    pub async fn cancel(&self, session_id: &str) -> Result<()> {
        let conn = {
            let sessions = self.sessions.borrow();
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| AcpError::SessionNotFound(session_id.to_string()))?;
            entry.conn.clone()
        };

        acp::Agent::cancel(
            conn.as_ref(),
            acp::CancelNotification::new(acp::SessionId::from(session_id.to_string())),
        )
        .await?;

        Ok(())
    }

    /// Kill a session and its agent process.
    pub fn kill_session(&self, session_id: &str) {
        self.sessions.borrow_mut().remove(session_id);
        // Dropping SessionEntry drops the child_ref, which kills the process via kill_on_drop
    }

    /// List all active session IDs.
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.borrow().keys().cloned().collect()
    }
}

/// ACP Client implementation for Enki orchestrator.
struct EnkiClient {
    update_callback: Rc<RefCell<Option<UpdateCallback>>>,
    auto_approve: bool,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for EnkiClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let tool_title = args
            .tool_call
            .fields
            .title
            .as_deref()
            .unwrap_or("unknown");

        if self.auto_approve {
            tracing::debug!(tool = tool_title, "auto-approving permission");
            // Find AllowOnce or AllowAlways, prefer AllowOnce
            let option_id = args
                .options
                .iter()
                .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
                .or_else(|| {
                    args.options
                        .iter()
                        .find(|o| o.kind == acp::PermissionOptionKind::AllowAlways)
                })
                .map(|o| o.option_id.clone())
                .unwrap_or_else(|| args.options[0].option_id.clone());

            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new(option_id),
                ),
            ))
        } else {
            tracing::warn!(tool = tool_title, "permission denied (non-interactive mode)");
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            ))
        }
    }

    async fn session_notification(
        &self,
        args: acp::SessionNotification,
    ) -> acp::Result<()> {
        let cb = self.update_callback.borrow();
        let Some(callback) = cb.as_ref() else {
            return Ok(());
        };

        match args.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                if let acp::ContentBlock::Text(text) = chunk.content {
                    callback(SessionUpdate::Text(text.text));
                }
            }
            acp::SessionUpdate::ToolCall(tc) => {
                callback(SessionUpdate::ToolCallStarted {
                    id: tc.tool_call_id.to_string(),
                    title: tc.title.clone(),
                });
            }
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                callback(SessionUpdate::ToolCallDone {
                    id: tcu.tool_call_id.to_string(),
                });
            }
            acp::SessionUpdate::Plan(plan) => {
                if let Ok(value) = serde_json::to_value(&plan) {
                    callback(SessionUpdate::Plan(value));
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        let content = tokio::fs::read_to_string(&args.path)
            .await
            .map_err(acp::Error::into_internal_error)?;
        Ok(acp::ReadTextFileResponse::new(content))
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        tokio::fs::write(&args.path, &args.content)
            .await
            .map_err(acp::Error::into_internal_error)?;
        Ok(acp::WriteTextFileResponse::default())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        Err(acp::Error::method_not_found())
    }
}

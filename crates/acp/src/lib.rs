use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use agent_client_protocol as acp;
use acp::{StreamMessageContent, StreamMessageDirection};

/// Re-export ACP schema types needed by callers (e.g., MCP server config).
pub use agent_client_protocol as acp_schema;
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

/// A handle to an active ACP session and its owning agent process.
struct SessionEntry {
    conn: Rc<acp::ClientSideConnection>,
    _child_ref: Rc<RefCell<Option<tokio::process::Child>>>,
}

/// Manages ACP agent processes and sessions.
///
/// All methods must be called from within a `tokio::task::LocalSet` because
/// ACP futures are `!Send`.
#[derive(Clone)]
pub struct AgentManager {
    sessions: Rc<RefCell<HashMap<String, SessionEntry>>>,
    update_callback: Rc<RefCell<Option<UpdateCallback>>>,
    /// Permission auto-approve (for non-interactive orchestrator use).
    auto_approve_permissions: bool,
    /// Extra environment variables injected into every spawned agent and terminal process.
    extra_env: Rc<HashMap<String, String>>,
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            sessions: Rc::new(RefCell::new(HashMap::new())),
            update_callback: Rc::new(RefCell::new(None)),
            auto_approve_permissions: true,
            extra_env: Rc::new(HashMap::new()),
        }
    }

    /// Set environment variables that will be injected into every spawned subprocess
    /// (both agent processes and terminal commands).
    pub fn set_env(&mut self, env: HashMap<String, String>) {
        self.extra_env = Rc::new(env);
    }

    /// Set a callback to receive session updates from all agents.
    /// The callback receives `(session_id, update)`.
    pub fn on_update(&self, callback: impl Fn(&str, SessionUpdate) + 'static) {
        *self.update_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Spawn a new agent process, create a session, and return the session ID.
    ///
    /// `agent_cmd` is the command to run (e.g., "claude" or "npx").
    /// `agent_args` are additional arguments (e.g., ["--acp"] or ["@zed-industries/claude-code-acp"]).
    /// `cwd` is the working directory for the session (typically a copy path).
    pub async fn start_session(
        &self,
        agent_cmd: &str,
        agent_args: &[&str],
        cwd: PathBuf,
        label: &str,
    ) -> Result<String> {
        self.start_session_with_mcp(agent_cmd, agent_args, cwd, vec![], label).await
    }

    /// Spawn a new agent process with MCP servers configured.
    pub async fn start_session_with_mcp(
        &self,
        agent_cmd: &str,
        agent_args: &[&str],
        cwd: PathBuf,
        mcp_servers: Vec<acp::McpServer>,
        label: &str,
    ) -> Result<String> {
        tracing::debug!(cmd = agent_cmd, args = ?agent_args, cwd = %cwd.display(), "spawning agent process");
        // Spawn agent subprocess
        let mut cmd = Command::new(agent_cmd);
        cmd.args(agent_args)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Clear CLAUDECODE so the agent doesn't think it's nested inside
        // another Claude Code session and refuse to start.
        cmd.env_remove("CLAUDECODE");
        for (k, v) in self.extra_env.iter() {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take().unwrap().compat_write();
        let stdout = child.stdout.take().unwrap().compat();

        // Capture agent stderr in a background task and log it line-by-line.
        if let Some(stderr) = child.stderr.take() {
            let agent_cmd_owned = agent_cmd.to_string();
            let cwd_owned = cwd.display().to_string();
            tokio::task::spawn_local(async move {
                use tokio::io::AsyncBufReadExt;
                let reader = tokio::io::BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.is_empty() {
                        tracing::warn!(agent = agent_cmd_owned, cwd = cwd_owned, "agent stderr: {}", line);
                    }
                }
            });
        }

        // Create ACP client
        let client = EnkiClient {
            update_callback: self.update_callback.clone(),
            auto_approve: self.auto_approve_permissions,
            terminals: Rc::new(RefCell::new(HashMap::new())),
            next_terminal_id: Rc::new(std::cell::Cell::new(1)),
            extra_env: self.extra_env.clone(),
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

        // Subscribe to all JSON-RPC messages and log to per-session file.
        let mut stream_rx = conn.subscribe();
        let log_label = label.to_string();
        tokio::task::spawn_local(async move {
            let log_dir = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".enki/logs/sessions");
            if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
                tracing::warn!("failed to create session log dir: {e}");
                return;
            }
            let log_path = log_dir.join(format!("{log_label}.log"));
            let file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("failed to open session log {}: {e}", log_path.display());
                    return;
                }
            };
            use tokio::io::AsyncWriteExt;
            let mut writer = tokio::io::BufWriter::new(file);

            while let Ok(msg) = stream_rx.recv().await {
                let arrow = match msg.direction {
                    StreamMessageDirection::Outgoing => "→",
                    StreamMessageDirection::Incoming => "←",
                };
                let now = chrono::Local::now().format("%H:%M:%S");
                let line = match &msg.message {
                    StreamMessageContent::Request { id, method, params } => {
                        let params_str = truncate_json(params.as_ref());
                        format!("[{now}] {arrow} {method} id={id} params={params_str}\n")
                    }
                    StreamMessageContent::Response { id, result } => {
                        let result_str = match result {
                            Ok(val) => truncate_json(val.as_ref()),
                            Err(e) => format!("error: {e}"),
                        };
                        format!("[{now}] {arrow} response id={id} result={result_str}\n")
                    }
                    StreamMessageContent::Notification { method, params } => {
                        let params_str = truncate_json(params.as_ref());
                        format!("[{now}] {arrow} {method} params={params_str}\n")
                    }
                };
                if let Err(e) = writer.write_all(line.as_bytes()).await {
                    tracing::warn!("session log write error: {e}");
                    break;
                }
                let _ = writer.flush().await;
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
            acp::Agent::new_session(
                conn.as_ref(),
                acp::NewSessionRequest::new(cwd).mcp_servers(mcp_servers),
            )
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
        tracing::debug!(session_id, chars = text.len(), "sending prompt to agent");
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
        tracing::debug!(session_id, "killing ACP session");
        self.sessions.borrow_mut().remove(session_id);
        // Dropping SessionEntry drops the child_ref, which kills the process via kill_on_drop
    }

    /// List all active session IDs.
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.borrow().keys().cloned().collect()
    }
}

/// State for a spawned terminal process.
/// Starts with a running child, transitions to completed with output.
enum TerminalState {
    Running {
        child: tokio::process::Child,
        output_limit: usize,
    },
    Done {
        output: String,
        truncated: bool,
        exit_status: acp::TerminalExitStatus,
    },
}

/// ACP Client implementation for Enki orchestrator.
struct EnkiClient {
    update_callback: Rc<RefCell<Option<UpdateCallback>>>,
    auto_approve: bool,
    terminals: Rc<RefCell<HashMap<String, TerminalState>>>,
    next_terminal_id: Rc<std::cell::Cell<u64>>,
    extra_env: Rc<HashMap<String, String>>,
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

        let session_id = args.session_id.to_string();

        match args.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                if let acp::ContentBlock::Text(text) = chunk.content {
                    callback(&session_id, SessionUpdate::Text(text.text));
                }
            }
            acp::SessionUpdate::ToolCall(tc) => {
                callback(&session_id, SessionUpdate::ToolCallStarted {
                    id: tc.tool_call_id.to_string(),
                    title: tc.title.clone(),
                });
            }
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                callback(&session_id, SessionUpdate::ToolCallDone {
                    id: tcu.tool_call_id.to_string(),
                });
            }
            acp::SessionUpdate::Plan(plan) => {
                if let Ok(value) = serde_json::to_value(&plan) {
                    callback(&session_id, SessionUpdate::Plan(value));
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
        let tagged = enki_core::hashline::tag_content(&content);
        Ok(acp::ReadTextFileResponse::new(tagged))
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        let path = std::path::Path::new(&args.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(acp::Error::into_internal_error)?;
        }

        // If the content has hashline prefixes, verify hashes against current
        // file content (stale edit detection), then strip before writing.
        let content = if enki_core::hashline::looks_like_tagged(&args.content) {
            if path.exists() {
                let current = tokio::fs::read_to_string(&args.path)
                    .await
                    .map_err(acp::Error::into_internal_error)?;
                enki_core::hashline::verify_hashlines(&args.content, &current)
                    .map_err(|e| acp::Error::into_internal_error(
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                    ))?;
            }
            enki_core::hashline::strip_hashlines(&args.content)
        } else {
            args.content.clone()
        };

        tokio::fs::write(&args.path, &content)
            .await
            .map_err(acp::Error::into_internal_error)?;
        Ok(acp::WriteTextFileResponse::default())
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        // Claude Code sends the full command as a single string (e.g. "ls -R /path").
        // We need to run it through a shell so it's parsed correctly.
        let mut full_command = args.command.clone();
        for arg in &args.args {
            full_command.push(' ');
            full_command.push_str(arg);
        }
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-c", &full_command]);

        if let Some(cwd) = &args.cwd {
            cmd.current_dir(cwd);
        }

        for env_var in &args.env {
            cmd.env(&env_var.name, &env_var.value);
        }
        for (k, v) in self.extra_env.iter() {
            cmd.env(k, v);
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::null());

        let child = cmd.spawn().map_err(|e| {
            acp::Error::into_internal_error(std::io::Error::new(std::io::ErrorKind::Other, format!("failed to spawn terminal: {e}")))
        })?;

        let id_num = self.next_terminal_id.get();
        self.next_terminal_id.set(id_num + 1);
        let terminal_id = format!("term-{id_num}");

        let output_limit = args.output_byte_limit.unwrap_or(100_000) as usize;

        self.terminals.borrow_mut().insert(
            terminal_id.clone(),
            TerminalState::Running {
                child,
                output_limit,
            },
        );

        Ok(acp::CreateTerminalResponse::new(acp::TerminalId::from(
            terminal_id,
        )))
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        let tid = args.terminal_id.to_string();
        self.finish_if_exited(&tid).await;

        let terminals = self.terminals.borrow();
        let state = terminals
            .get(&tid)
            .ok_or_else(|| acp::Error::into_internal_error(std::io::Error::new(std::io::ErrorKind::NotFound, "terminal not found")))?;

        match state {
            TerminalState::Running { .. } => {
                Ok(acp::TerminalOutputResponse::new(String::new(), false))
            }
            TerminalState::Done { output, truncated, exit_status, .. } => {
                let mut resp = acp::TerminalOutputResponse::new(output.clone(), *truncated);
                resp.exit_status = Some(exit_status.clone());
                Ok(resp)
            }
        }
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        let tid = args.terminal_id.to_string();

        // Take the child out so we can await without holding the borrow
        let mut child_and_limit = None;
        {
            let mut terminals = self.terminals.borrow_mut();
            let state = terminals
                .get_mut(&tid)
                .ok_or_else(|| acp::Error::into_internal_error(std::io::Error::new(std::io::ErrorKind::NotFound, "terminal not found")))?;

            match state {
                TerminalState::Done { exit_status, .. } => {
                    return Ok(acp::WaitForTerminalExitResponse::new(exit_status.clone()));
                }
                TerminalState::Running { .. } => {
                    // Take ownership of the child to await it
                    let taken = std::mem::replace(
                        state,
                        // Temporary placeholder — will be replaced below
                        TerminalState::Done {
                            output: String::new(),
                            truncated: false,
                            exit_status: acp::TerminalExitStatus::new(),
                        },
                    );
                    if let TerminalState::Running { child, output_limit } = taken {
                        child_and_limit = Some((child, output_limit));
                    }
                }
            }
        }

        // Now await outside the borrow
        let (child, output_limit) = child_and_limit.unwrap();
        let result = child.wait_with_output().await;

        let (output, truncated, exit_status) = match result {
            Ok(output) => {
                let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
                combined.push_str(&String::from_utf8_lossy(&output.stderr));

                let truncated = combined.len() > output_limit;
                if truncated {
                    let start = combined.len() - output_limit;
                    let start = combined.ceil_char_boundary(start);
                    combined = combined[start..].to_string();
                }

                let exit_status = acp::TerminalExitStatus::new()
                    .exit_code(output.status.code().map(|c| c as u32));
                (combined, truncated, exit_status)
            }
            Err(e) => {
                let exit_status = acp::TerminalExitStatus::new()
                    .signal(format!("io_error: {e}"));
                (String::new(), false, exit_status)
            }
        };

        // Store the result
        self.terminals.borrow_mut().insert(
            tid,
            TerminalState::Done {
                output,
                truncated,
                exit_status: exit_status.clone(),
            },
        );

        Ok(acp::WaitForTerminalExitResponse::new(exit_status))
    }

    async fn kill_terminal_command(
        &self,
        args: acp::KillTerminalCommandRequest,
    ) -> acp::Result<acp::KillTerminalCommandResponse> {
        let tid = args.terminal_id.to_string();
        let mut terminals = self.terminals.borrow_mut();
        let state = terminals
            .get_mut(&tid)
            .ok_or_else(|| acp::Error::into_internal_error(std::io::Error::new(std::io::ErrorKind::NotFound, "terminal not found")))?;

        if let TerminalState::Running { child, .. } = state {
            let _ = child.kill().await;
        }

        Ok(acp::KillTerminalCommandResponse::default())
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        let tid = args.terminal_id.to_string();
        let entry = self.terminals.borrow_mut().remove(&tid);

        // If still running, the child is dropped which kills it (kill_on_drop isn't set
        // on these, but dropping the pipes will cause the process to get SIGPIPE).
        drop(entry);

        Ok(acp::ReleaseTerminalResponse::default())
    }
}

impl EnkiClient {
    /// If the terminal's child has exited, transition to Done state.
    async fn finish_if_exited(&self, terminal_id: &str) {
        // Check if already done or if process exited
        let mut should_wait = false;
        {
            let terminals = self.terminals.borrow();
            if let Some(TerminalState::Running { .. }) = terminals.get(terminal_id) {
                should_wait = true;
            }
        }

        if !should_wait {
            return;
        }

        // Try non-blocking check
        let exited = {
            let mut terminals = self.terminals.borrow_mut();
            if let Some(TerminalState::Running { child, .. }) = terminals.get_mut(terminal_id) {
                matches!(child.try_wait(), Ok(Some(_)))
            } else {
                false
            }
        };

        if exited {
            // Take child out and wait for output
            let child_and_limit = {
                let mut terminals = self.terminals.borrow_mut();
                let state = terminals.get_mut(terminal_id).unwrap();
                let taken = std::mem::replace(
                    state,
                    TerminalState::Done {
                        output: String::new(),
                        truncated: false,
                        exit_status: acp::TerminalExitStatus::new(),
                    },
                );
                if let TerminalState::Running { child, output_limit } = taken {
                    Some((child, output_limit))
                } else {
                    None
                }
            };

            if let Some((child, output_limit)) = child_and_limit {
                let result = child.wait_with_output().await;
                let (output, truncated, exit_status) = match result {
                    Ok(output) => {
                        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
                        combined.push_str(&String::from_utf8_lossy(&output.stderr));
                        let truncated = combined.len() > output_limit;
                        if truncated {
                            let start = combined.len() - output_limit;
                            let start = combined.ceil_char_boundary(start);
                            combined = combined[start..].to_string();
                        }
                        let exit_status = acp::TerminalExitStatus::new()
                            .exit_code(output.status.code().map(|c| c as u32));
                        (combined, truncated, exit_status)
                    }
                    Err(e) => {
                        let exit_status = acp::TerminalExitStatus::new()
                            .signal(format!("io_error: {e}"));
                        (String::new(), false, exit_status)
                    }
                };

                self.terminals.borrow_mut().insert(
                    terminal_id.to_string(),
                    TerminalState::Done { output, truncated, exit_status },
                );
            }
        }
    }
}

/// Truncate a JSON value to a reasonable size for logging.
fn truncate_json(value: Option<&serde_json::Value>) -> String {
    const MAX_LEN: usize = 500;
    let Some(value) = value else {
        return "null".to_string();
    };
    let s = value.to_string();
    if s.len() <= MAX_LEN {
        s
    } else {
        let truncated = s.len() - MAX_LEN;
        format!("{}[...truncated {truncated} chars]", &s[..MAX_LEN])
    }
}

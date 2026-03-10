use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use agent_client_protocol as acp;
use acp::{StreamMessageContent, StreamMessageDirection};
use tokio::process::Command;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{AcpError, Result, UpdateCallback};
use crate::client::EnkiClient;

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

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
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
    pub fn on_update(&self, callback: impl Fn(&str, crate::SessionUpdate) + 'static) {
        *self.update_callback.borrow_mut() = Some(Box::new(callback));
    }

    /// Spawn a new agent process, create a session, and return the session ID.
    ///
    /// `agent_cmd` is the command to run (e.g., "claude" or "npx").
    /// `agent_args` are additional arguments (e.g., ["--acp"] or ["@zed-industries/claude-agent-acp"]).
    /// `cwd` is the working directory for the session (typically a copy path).
    pub async fn start_session(
        &self,
        agent_cmd: &str,
        agent_args: &[&str],
        cwd: PathBuf,
        label: &str,
    ) -> Result<String> {
        self.start_session_with_mcp(agent_cmd, agent_args, cwd, vec![], label, false, &HashMap::new()).await
    }

    /// Spawn a new agent process with MCP servers configured.
    ///
    /// If `sonnet_only` is true, attempts to set the model to sonnet via
    /// `set_session_config_option` after session creation.
    ///
    /// `agent_env` contains role-specific environment variables from the agent config.
    /// These are merged on top of the base env (from `set_env()`), so role-specific
    /// values take precedence.
    pub async fn start_session_with_mcp(
        &self,
        agent_cmd: &str,
        agent_args: &[&str],
        cwd: PathBuf,
        mcp_servers: Vec<acp::McpServer>,
        label: &str,
        sonnet_only: bool,
        agent_env: &HashMap<String, String>,
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
        // Base env (ENKI_* vars) first, then role-specific agent env on top.
        for (k, v) in self.extra_env.iter() {
            cmd.env(k, v);
        }
        for (k, v) in agent_env {
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
                        if line.contains("onPostToolUseHook") || line.starts_with("[{") {
                            tracing::debug!(agent = agent_cmd_owned, cwd = cwd_owned, stderr_line = %line, "agent stderr");
                        } else {
                            tracing::warn!(agent = agent_cmd_owned, cwd = cwd_owned, stderr_line = %line, "agent stderr");
                        }
                    }
                }
            });
        }

        // Create ACP client
        let client = EnkiClient::new(
            self.update_callback.clone(),
            self.auto_approve_permissions,
            self.extra_env.clone(),
        );

        let (conn, handle_io) = acp::ClientSideConnection::new(client, stdin, stdout, |fut| {
            tokio::task::spawn_local(fut);
        });
        let conn = Rc::new(conn);

        // Drive the I/O loop in background
        tokio::task::spawn_local(async move {
            if let Err(e) = handle_io.await {
                tracing::error!(error = %e, "ACP I/O error");
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
                tracing::warn!(error = %e, "failed to create session log dir");
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
                    tracing::warn!(path = %log_path.display(), error = %e, "failed to open session log");
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
                    tracing::warn!(error = %e, "session log write error");
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
                            acp::FileSystemCapabilities::new()
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

        // If sonnet_only, find the model config option and switch to sonnet.
        if sonnet_only {
            if let Some(config_options) = &session_resp.config_options {
                if let Some(sonnet_value) = find_sonnet_model(config_options) {
                    tracing::info!(session_id, model = %sonnet_value, "setting sonnet-only model");
                    let set_result = acp::Agent::set_session_config_option(
                        conn.as_ref(),
                        acp::SetSessionConfigOptionRequest::new(
                            session_resp.session_id.clone(),
                            "model",
                            sonnet_value,
                        ),
                    )
                    .await;
                    if let Err(e) = set_result {
                        tracing::warn!(session_id, error = %e, "failed to set sonnet model");
                    }
                } else {
                    tracing::warn!(session_id, "sonnet_only enabled but no sonnet model found in agent config options");
                }
            } else {
                tracing::warn!(session_id, "sonnet_only enabled but agent returned no config options");
            }
        }

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
    pub async fn prompt(&self, session_id: &str, content: Vec<acp::ContentBlock>) -> Result<String> {
        tracing::debug!(session_id, blocks = content.len(), "sending prompt to agent");
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
                content,
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

/// Find a sonnet model value from the agent's config options.
///
/// Searches for the model config option (by `id == "model"` or `category == Model`),
/// then finds an option value containing "sonnet" (case-insensitive).
fn find_sonnet_model(config_options: &[acp::SessionConfigOption]) -> Option<String> {
    let model_option = config_options.iter().find(|opt| {
        opt.id.0.as_ref() == "model"
            || opt.category == Some(acp::SessionConfigOptionCategory::Model)
    })?;

    let acp::SessionConfigKind::Select(select) = &model_option.kind else {
        return None;
    };
    let all_options: Vec<&acp::SessionConfigSelectOption> = match &select.options {
        acp::SessionConfigSelectOptions::Ungrouped(opts) => opts.iter().collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|g| g.options.iter()).collect()
        }
        _ => return None,
    };

    // Find the first option whose value or name contains "sonnet"
    let matched = all_options.iter().find(|opt| {
        opt.value.0.to_lowercase().contains("sonnet")
            || opt.name.to_lowercase().contains("sonnet")
    })?;

    Some(matched.value.0.to_string())
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
        // Find the largest valid char boundary at or before MAX_LEN
        let end = s.floor_char_boundary(MAX_LEN);
        let truncated = s.len() - end;
        format!("{}[...truncated {truncated} bytes]", &s[..end])
    }
}

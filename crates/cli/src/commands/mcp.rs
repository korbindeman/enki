use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};

use chrono::Utc;
use enki_core::types::{
    Execution, ExecutionStatus, Id, Message, MessagePriority, MessageType, Task, TaskStatus, Tier,
};
use serde_json::{Value, json};

use super::open_db;

fn tools_for_role(role: &str) -> &'static [&'static str] {
    match role {
        "planner" => &[
            "enki_status",
            "enki_task_create",
            "enki_task_list",
            "enki_task_retry",
            "enki_execution_create",
            "enki_pause",
            "enki_cancel",
            "enki_stop_all",
            "enki_mail_send",
            "enki_mail_check",
            "enki_mail_read",
            "enki_mail_inbox",
        ],
        "worker" => &[
            "enki_status",
            "enki_task_list",
            "enki_worker_report",
            "enki_mail_send",
            "enki_mail_check",
            "enki_mail_read",
            "enki_mail_inbox",
        ],
        _ => &[],
    }
}

/// Run the MCP stdio server. Reads JSON-RPC messages from stdin, writes responses to stdout.
pub fn run(role: &str, task_id: Option<&str>) -> anyhow::Result<()> {
    let role = role.to_string();
    let task_id = task_id.map(|s| s.to_string());
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");

        let response = match method {
            "initialize" => Some(handle_initialize(id)),
            "notifications/initialized" => None,
            "tools/list" => Some(handle_tools_list(id, &role)),
            "tools/call" => Some(handle_tools_call(id, &req["params"], &role, task_id.as_deref())),
            _ => id.map(|id| error_response(id, -32601, "method not found")),
        };

        if let Some(resp) = response {
            let mut out = serde_json::to_string(&resp)?;
            out.push('\n');
            stdout.write_all(out.as_bytes())?;
            stdout.flush()?;
        }
    }

    Ok(())
}

fn handle_initialize(id: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "enki",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

fn all_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "enki_status",
            "description": "Show task counts by status.",
            "inputSchema": {
                "type": "object",
                "properties": {},
            }
        }),
        json!({
            "name": "enki_task_create",
            "description": "Create a single standalone task. Starts with status 'ready' and will be automatically picked up by a worker agent. For multi-step work with dependencies, use enki_execution_create instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short task title."
                    },
                    "description": {
                        "type": "string",
                        "description": "Detailed task description with acceptance criteria."
                    },
                    "tier": {
                        "type": "string",
                        "enum": ["light", "standard", "heavy"],
                        "description": "Complexity tier. Defaults to 'standard'."
                    }
                },
                "required": ["title"]
            }
        }),
        json!({
            "name": "enki_task_list",
            "description": "List all tasks, showing ID, status, tier, and title.",
            "inputSchema": {
                "type": "object",
                "properties": {},
            }
        }),
        json!({
            "name": "enki_execution_create",
            "description": "Create a multi-step execution with dependencies between steps. Steps with no dependencies start immediately; others wait for their dependencies to complete. Use this for any work involving 2+ related steps.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Unique step identifier within this execution (e.g. 'scaffold', 'auth', 'tests')."
                                },
                                "title": {
                                    "type": "string",
                                    "description": "Short task title."
                                },
                                "description": {
                                    "type": "string",
                                    "description": "Detailed task description with acceptance criteria."
                                },
                                "tier": {
                                    "type": "string",
                                    "enum": ["light", "standard", "heavy"],
                                    "description": "Complexity tier. Defaults to 'standard'."
                                },
                                "needs": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Step IDs this step depends on. Those steps must complete before this one starts."
                                }
                            },
                            "required": ["id", "title", "description"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["steps"]
            }
        }),
        json!({
            "name": "enki_stop_all",
            "description": "Stop all running workers immediately. Use when the user asks to stop, halt, or cancel all tasks.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "enki_task_retry",
            "description": "Retry a failed task within its execution. Resets the task to 'ready', unblocks sibling tasks that were blocked by this failure, and restores the execution to 'running' so the scheduler picks it back up. Use this instead of recreating an entire execution when only one step failed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "ID of the failed task to retry."
                    }
                },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "enki_pause",
            "description": "Pause an execution or a single step within an execution. Paused items stop accepting new work; running workers are allowed to finish. Use enki_cancel instead if you want to stop immediately.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "execution_id": {
                        "type": "string",
                        "description": "Execution ID to pause."
                    },
                    "step_id": {
                        "type": "string",
                        "description": "Optional step ID within the execution. If provided, only that step is paused."
                    }
                },
                "required": ["execution_id"]
            }
        }),
        json!({
            "name": "enki_cancel",
            "description": "Cancel an execution or a single step. Running workers are killed. Cancelling a step cascades to all transitive dependents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "execution_id": {
                        "type": "string",
                        "description": "Execution ID to cancel."
                    },
                    "step_id": {
                        "type": "string",
                        "description": "Optional step ID within the execution. If provided, only that step (and its dependents) are cancelled."
                    }
                },
                "required": ["execution_id"]
            }
        }),
        json!({
            "name": "enki_worker_report",
            "description": "Report your current high-level activity. Call this periodically to let the user see what you're working on.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Brief description of what you're doing (e.g. 'analyzing codebase', 'running tests', 'implementing auth middleware')."
                    }
                },
                "required": ["status"]
            }
        }),
        json!({
            "name": "enki_mail_send",
            "description": "Send a message to another worker, the coordinator, or the user. Addresses: 'coordinator', 'worker/<task_id>', '@workers' (broadcast), 'user'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient address (e.g. 'coordinator', 'worker/task-01JXX...', '@workers', 'user')."
                    },
                    "subject": {
                        "type": "string",
                        "description": "Brief subject line."
                    },
                    "body": {
                        "type": "string",
                        "description": "Message body."
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "normal", "high", "urgent"],
                        "description": "Message priority. Defaults to 'normal'."
                    },
                    "thread_id": {
                        "type": "string",
                        "description": "Optional thread ID to group related messages."
                    },
                    "reply_to": {
                        "type": "string",
                        "description": "Optional message ID this is a reply to."
                    }
                },
                "required": ["to", "subject", "body"]
            }
        }),
        json!({
            "name": "enki_mail_check",
            "description": "Check your inbox for unread messages. Returns count and summary of unread messages.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "enki_mail_read",
            "description": "Read a specific message by ID and mark it as read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "string",
                        "description": "ID of the message to read."
                    }
                },
                "required": ["message_id"]
            }
        }),
        json!({
            "name": "enki_mail_inbox",
            "description": "List all messages in your inbox (read and unread).",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ]
}

fn handle_tools_list(id: Option<Value>, role: &str) -> Value {
    let allowed = tools_for_role(role);
    let tools: Vec<Value> = all_tool_definitions()
        .into_iter()
        .filter(|t| {
            t["name"].as_str().map_or(false, |n| allowed.contains(&n))
        })
        .collect();

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "tools": tools }
    })
}

fn handle_tools_call(id: Option<Value>, params: &Value, role: &str, task_id: Option<&str>) -> Value {
    let tool_name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];

    let allowed = tools_for_role(role);
    if !allowed.contains(&tool_name) {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("tool '{tool_name}' is not available for role '{role}'") }],
                "isError": true
            }
        });
    }

    let my_addr = caller_addr(role, task_id);
    let result = match tool_name {
        "enki_status" => tool_status(),
        "enki_task_create" => tool_task_create(args),
        "enki_task_list" => tool_task_list(),
        "enki_execution_create" => tool_execution_create(args),
        "enki_stop_all" => tool_stop_all(),
        "enki_task_retry" => tool_task_retry(args),
        "enki_pause" => tool_pause(args),
        "enki_cancel" => tool_cancel(args),
        "enki_worker_report" => tool_worker_report(args, task_id),
        "enki_mail_send" => tool_mail_send(args, &my_addr),
        "enki_mail_check" => tool_mail_check(&my_addr),
        "enki_mail_read" => tool_mail_read(args),
        "enki_mail_inbox" => tool_mail_inbox(&my_addr),
        _ => Err(format!("unknown tool: {tool_name}")),
    };

    // Piggyback: only attach mail notice on worker_report (the periodic heartbeat tool).
    let result = if tool_name == "enki_worker_report" {
        match result {
            Ok(mut text) => {
                if let Ok(notice) = mail_notice(&my_addr) {
                    if !notice.is_empty() {
                        text.push_str("\n\n---\n");
                        text.push_str(&notice);
                    }
                }
                Ok(text)
            }
            err => err,
        }
    } else {
        result
    };

    match result {
        Ok(text) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": text }]
            }
        }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": e }],
                "isError": true
            }
        }),
    }
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Write a signal file to `.enki/events/` so the coordinator picks up the change.
fn write_signal_file(signal: &Value) -> Result<(), String> {
    let enki_dir = super::enki_dir().map_err(|e| e.to_string())?;
    let events_dir = enki_dir.join("events");
    std::fs::create_dir_all(&events_dir).map_err(|e| e.to_string())?;
    let filename = format!("{}.json", Id::new("sig"));
    let path = events_dir.join(filename);
    let content = serde_json::to_string(signal).map_err(|e| e.to_string())?;
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(())
}

// --- Tool implementations ---

fn tool_status() -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let tasks = db.list_tasks().map_err(|e| e.to_string())?;

    let open = tasks.iter().filter(|t| matches!(t.status, TaskStatus::Open | TaskStatus::Ready)).count();
    let running = tasks.iter().filter(|t| t.status == TaskStatus::Running).count();
    let done = tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    let failed = tasks.iter().filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked)).count();

    Ok(format!("tasks: {} open, {} running, {} done, {} failed ({} total)", open, running, done, failed, tasks.len()))
}

fn tool_task_create(args: &Value) -> Result<String, String> {
    let title = args["title"].as_str().ok_or("missing required parameter: title")?;
    let description = args["description"].as_str().map(String::from);
    let tier_str = args["tier"].as_str().unwrap_or("standard");

    let tier = Tier::from_str(tier_str).ok_or_else(|| format!("invalid tier: {tier_str}"))?;
    let db = open_db().map_err(|e| e.to_string())?;

    let now = Utc::now();
    let task = Task {
        id: Id::new("task"),
        title: title.to_string(),
        description,
        status: TaskStatus::Ready,
        assigned_to: None,
        worktree: None,
        branch: None,
        tier: Some(tier),
        current_activity: None,
        created_at: now,
        updated_at: now,
    };

    db.insert_task(&task).map_err(|e| e.to_string())?;
    write_signal_file(&json!({"type": "task_created", "task_id": task.id.as_str()}))?;
    Ok(format!("Created task '{}' ({}) — status: ready, tier: {}", title, task.id, tier_str))
}

fn tool_task_list() -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let tasks = db.list_tasks().map_err(|e| e.to_string())?;

    if tasks.is_empty() {
        return Ok("No tasks.".into());
    }

    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            let tier = t.tier.map(|t| t.as_str()).unwrap_or("-");
            let activity = t.current_activity.as_deref().unwrap_or("");
            if activity.is_empty() {
                format!("{} | {} | {} | {}", t.id, t.status.as_str(), tier, t.title)
            } else {
                format!("{} | {} | {} | {} [{}]", t.id, t.status.as_str(), tier, t.title, activity)
            }
        })
        .collect();
    Ok(lines.join("\n"))
}

fn tool_execution_create(args: &Value) -> Result<String, String> {
    let steps = args["steps"]
        .as_array()
        .ok_or("missing required parameter: steps")?;

    if steps.is_empty() {
        return Err("steps array must not be empty".into());
    }

    // Parse and validate all steps up front.
    struct StepDef {
        id: String,
        title: String,
        description: String,
        tier: Tier,
        needs: Vec<String>,
    }

    let mut defs: Vec<StepDef> = Vec::new();
    let mut step_ids: HashSet<String> = HashSet::new();

    for step in steps {
        let id = step["id"]
            .as_str()
            .ok_or("each step must have an 'id'")?
            .to_string();
        let title = step["title"]
            .as_str()
            .ok_or("each step must have a 'title'")?
            .to_string();
        let description = step["description"]
            .as_str()
            .ok_or("each step must have a 'description'")?
            .to_string();
        let tier_str = step["tier"].as_str().unwrap_or("standard");
        let tier =
            Tier::from_str(tier_str).ok_or_else(|| format!("invalid tier: {tier_str}"))?;
        let needs: Vec<String> = step["needs"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if !step_ids.insert(id.clone()) {
            return Err(format!("duplicate step id: {id}"));
        }
        defs.push(StepDef {
            id,
            title,
            description,
            tier,
            needs,
        });
    }

    // Validate dependency references.
    for def in &defs {
        for dep in &def.needs {
            if !step_ids.contains(dep) {
                return Err(format!(
                    "step '{}' depends on unknown step '{}'",
                    def.id, dep
                ));
            }
            if dep == &def.id {
                return Err(format!("step '{}' cannot depend on itself", def.id));
            }
        }
    }

    // Create execution + tasks + steps + dependencies atomically.
    let db = open_db().map_err(|e| e.to_string())?;
    let execution_id = Id::new("exec");
    let now = Utc::now();

    let execution = Execution {
        id: execution_id.clone(),
        status: ExecutionStatus::Running,
        created_at: now,
    };
    db.insert_execution(&execution).map_err(|e| e.to_string())?;

    let mut step_task_ids: HashMap<String, Id> = HashMap::new();

    for def in &defs {
        let task_id = Id::new("task");
        let status = if def.needs.is_empty() {
            TaskStatus::Ready
        } else {
            TaskStatus::Open
        };
        let task = Task {
            id: task_id.clone(),
            title: def.title.clone(),
            description: Some(def.description.clone()),
            status,
            assigned_to: None,
            worktree: None,
            branch: None,
            tier: Some(def.tier),
            current_activity: None,
            created_at: now,
            updated_at: now,
        };
        db.insert_task(&task).map_err(|e| e.to_string())?;
        db.insert_execution_step(&execution_id, &def.id, &task_id)
            .map_err(|e| e.to_string())?;
        step_task_ids.insert(def.id.clone(), task_id);
    }

    // Wire up dependencies (task-level).
    for def in &defs {
        let task_id = &step_task_ids[&def.id];
        for dep_step_id in &def.needs {
            let dep_task_id = &step_task_ids[dep_step_id];
            db.insert_dependency(task_id, dep_task_id)
                .map_err(|e| e.to_string())?;
        }
    }

    write_signal_file(&json!({"type": "execution_created", "execution_id": execution_id.as_str()}))?;

    // Build response.
    let mut lines = vec![format!("execution: {}", execution_id)];
    for def in &defs {
        let task_id = &step_task_ids[&def.id];
        let status = if def.needs.is_empty() {
            "ready"
        } else {
            "open"
        };
        let deps = if def.needs.is_empty() {
            String::new()
        } else {
            format!(" (needs: {})", def.needs.join(", "))
        };
        lines.push(format!(
            "  {} → {} | {} | {}{}",
            def.id, task_id, status, def.title, deps
        ));
    }
    Ok(lines.join("\n"))
}

fn tool_task_retry(args: &Value) -> Result<String, String> {
    let task_id_str = args["task_id"].as_str().ok_or("missing required parameter: task_id")?;
    let db = open_db().map_err(|e| e.to_string())?;
    let task_id = Id(task_id_str.to_string());

    // Verify the task exists and is actually failed.
    let task = db.get_task(&task_id).map_err(|e| e.to_string())?;
    if task.status != TaskStatus::Failed {
        return Err(format!(
            "task {} is '{}', not 'failed' — can only retry failed tasks",
            task_id_str,
            task.status.as_str()
        ));
    }

    // Find the execution this task belongs to.
    let Some((exec_id, _step_id)) = db.get_execution_for_task(&task_id).map_err(|e| e.to_string())? else {
        // Standalone task — just reset to ready.
        db.update_task_status(&task_id, TaskStatus::Ready).map_err(|e| e.to_string())?;
        return Ok(format!("Task {} reset to ready (standalone, no execution).", task_id_str));
    };

    // Reset the failed task to ready.
    db.update_task_status(&task_id, TaskStatus::Ready).map_err(|e| e.to_string())?;

    // Reset any sibling tasks that were blocked by this failure back to open
    // (the scheduler's DAG rebuild will re-evaluate their readiness).
    let steps = db.get_execution_steps(&exec_id).map_err(|e| e.to_string())?;
    let mut unblocked = 0;
    for (_step_id, sibling_task_id) in &steps {
        if sibling_task_id == &task_id {
            continue;
        }
        let sibling = db.get_task(sibling_task_id).map_err(|e| e.to_string())?;
        if sibling.status == TaskStatus::Blocked {
            db.update_task_status(sibling_task_id, TaskStatus::Open).map_err(|e| e.to_string())?;
            unblocked += 1;
        }
    }

    // Reset the execution status to running so the poll loop rediscovers it.
    db.update_execution_status(&exec_id, ExecutionStatus::Running).map_err(|e| e.to_string())?;

    write_signal_file(&json!({"type": "task_created", "task_id": task_id_str}))?;

    let mut result = format!("Task {} reset to ready.", task_id_str);
    if unblocked > 0 {
        result.push_str(&format!(" {} blocked sibling task(s) unblocked.", unblocked));
    }
    result.push_str(&format!(" Execution {} restored to running.", exec_id));
    Ok(result)
}

fn tool_pause(args: &Value) -> Result<String, String> {
    let execution_id = args["execution_id"].as_str().ok_or("missing required parameter: execution_id")?;
    let step_id = args["step_id"].as_str();

    let mut signal = json!({"type": "pause", "execution_id": execution_id});
    if let Some(sid) = step_id {
        signal["step_id"] = json!(sid);
    }
    write_signal_file(&signal)?;

    match step_id {
        Some(sid) => Ok(format!("Pause signal sent for step '{}' in execution {}.", sid, execution_id)),
        None => Ok(format!("Pause signal sent for execution {}.", execution_id)),
    }
}

fn tool_cancel(args: &Value) -> Result<String, String> {
    let execution_id = args["execution_id"].as_str().ok_or("missing required parameter: execution_id")?;
    let step_id = args["step_id"].as_str();

    let mut signal = json!({"type": "cancel", "execution_id": execution_id});
    if let Some(sid) = step_id {
        signal["step_id"] = json!(sid);
    }
    write_signal_file(&signal)?;

    match step_id {
        Some(sid) => Ok(format!("Cancel signal sent for step '{}' in execution {}.", sid, execution_id)),
        None => Ok(format!("Cancel signal sent for execution {}.", execution_id)),
    }
}

fn tool_stop_all() -> Result<String, String> {
    let enki_dir = super::enki_dir().map_err(|e| e.to_string())?;
    // Write legacy stop file (coordinator checks this directly).
    let stop_file = enki_dir.join("stop");
    std::fs::write(&stop_file, "").map_err(|e| e.to_string())?;
    // Also write a signal file for the orchestrator path.
    write_signal_file(&json!({"type": "stop_all"}))?;
    Ok("Stop signal sent. All workers will be killed on the next coordinator poll.".into())
}

fn tool_worker_report(args: &Value, task_id: Option<&str>) -> Result<String, String> {
    let status = args["status"]
        .as_str()
        .ok_or("missing required parameter: status")?;
    let task_id = task_id.ok_or("worker report requires --task-id (not available outside worker context)")?;

    write_signal_file(&json!({
        "type": "worker_report",
        "task_id": task_id,
        "status": status
    }))?;

    Ok(format!("Status reported: {status}"))
}

// --- Mail helpers ---

/// Derive the caller's mail address from their role and task_id.
fn caller_addr(role: &str, task_id: Option<&str>) -> String {
    match role {
        "worker" => {
            if let Some(tid) = task_id {
                format!("worker/{tid}")
            } else {
                "worker/unknown".to_string()
            }
        }
        "planner" => "coordinator".to_string(),
        _ => "unknown".to_string(),
    }
}

/// Build a piggyback notice string if the caller has unread mail.
fn mail_notice(my_addr: &str) -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let (total, urgent) = db.count_unread(my_addr).map_err(|e| e.to_string())?;
    // Also check broadcast messages
    let (bc_total, bc_urgent) = db.count_unread("@workers").map_err(|e| e.to_string())?;
    let total = total + bc_total;
    let urgent = urgent + bc_urgent;
    if total == 0 {
        return Ok(String::new());
    }
    if urgent > 0 {
        Ok(format!("MAIL: You have {total} unread message(s) ({urgent} urgent). Use enki_mail_check to read them."))
    } else {
        Ok(format!("MAIL: You have {total} unread message(s). Use enki_mail_check to read them."))
    }
}

// --- Mail tool implementations ---

fn tool_mail_send(args: &Value, from_addr: &str) -> Result<String, String> {
    let to = args["to"].as_str().ok_or("missing required parameter: to")?;
    let subject = args["subject"].as_str().ok_or("missing required parameter: subject")?;
    let body = args["body"].as_str().ok_or("missing required parameter: body")?;
    let priority_str = args["priority"].as_str().unwrap_or("normal");
    let priority = MessagePriority::from_str(priority_str)
        .ok_or_else(|| format!("invalid priority: {priority_str}"))?;
    let thread_id = args["thread_id"].as_str().map(String::from);
    let reply_to = args["reply_to"].as_str().map(String::from);

    let db = open_db().map_err(|e| e.to_string())?;
    let now = Utc::now();
    let msg = Message {
        id: Id::new("msg"),
        from_addr: from_addr.to_string(),
        to_addr: to.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
        priority,
        msg_type: MessageType::Info,
        thread_id,
        reply_to,
        read: false,
        created_at: now,
    };

    db.insert_message(&msg).map_err(|e| e.to_string())?;

    // Signal the coordinator so it can surface in TUI.
    write_signal_file(&json!({
        "type": "mail",
        "message_id": msg.id.as_str(),
        "from": from_addr,
        "to": to,
        "subject": subject,
        "priority": priority_str,
    }))?;

    Ok(format!("Message sent to {to}: \"{subject}\" ({})", msg.id))
}

fn tool_mail_check(my_addr: &str) -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let mut messages = db.list_messages(my_addr, true).map_err(|e| e.to_string())?;
    // Also include broadcast messages.
    let broadcast = db.list_broadcast_messages("@workers", true).map_err(|e| e.to_string())?;
    messages.extend(broadcast);
    // Sort by priority desc, then by time desc.
    messages.sort_by(|a, b| {
        b.priority.sort_key().cmp(&a.priority.sort_key())
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    if messages.is_empty() {
        return Ok("No unread messages.".into());
    }

    let mut lines = vec![format!("{} unread message(s):", messages.len())];
    for msg in &messages {
        let ts = msg.created_at.format("%H:%M:%S");
        lines.push(format!(
            "  {} | [{}] {} | from: {} | {}",
            msg.id, msg.priority.as_str(), msg.subject, msg.from_addr, ts
        ));
    }
    Ok(lines.join("\n"))
}

fn tool_mail_read(args: &Value) -> Result<String, String> {
    let message_id = args["message_id"].as_str().ok_or("missing required parameter: message_id")?;
    let db = open_db().map_err(|e| e.to_string())?;
    let msg = db.get_message(message_id).map_err(|e| e.to_string())?;
    db.mark_message_read(message_id).map_err(|e| e.to_string())?;

    let mut lines = vec![
        format!("From: {}", msg.from_addr),
        format!("To: {}", msg.to_addr),
        format!("Subject: {}", msg.subject),
        format!("Priority: {}", msg.priority.as_str()),
        format!("Time: {}", msg.created_at.to_rfc3339()),
    ];
    if let Some(ref tid) = msg.thread_id {
        lines.push(format!("Thread: {tid}"));
    }
    if let Some(ref rid) = msg.reply_to {
        lines.push(format!("Reply-To: {rid}"));
    }
    lines.push(String::new());
    lines.push(msg.body.clone());

    Ok(lines.join("\n"))
}

fn tool_mail_inbox(my_addr: &str) -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let mut messages = db.list_messages(my_addr, false).map_err(|e| e.to_string())?;
    let broadcast = db.list_broadcast_messages("@workers", false).map_err(|e| e.to_string())?;
    messages.extend(broadcast);
    messages.sort_by(|a, b| {
        b.priority.sort_key().cmp(&a.priority.sort_key())
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    if messages.is_empty() {
        return Ok("Inbox empty.".into());
    }

    let mut lines = vec![format!("{} message(s):", messages.len())];
    for msg in &messages {
        let ts = msg.created_at.format("%H:%M:%S");
        let read_marker = if msg.read { " " } else { "*" };
        lines.push(format!(
            "  {}{} | [{}] {} | from: {} | {}",
            read_marker, msg.id, msg.priority.as_str(), msg.subject, msg.from_addr, ts
        ));
    }
    Ok(lines.join("\n"))
}

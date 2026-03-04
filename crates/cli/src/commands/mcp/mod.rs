mod handlers;
mod tools;

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use handlers::*;
use tools::all_tool_definitions;

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
            "enki_edit_file",
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
        "enki_edit_file" => tool_edit_file(args),
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

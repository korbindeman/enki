use serde_json::{Value, json};

pub(super) fn all_tool_definitions() -> Vec<Value> {
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
                        "description": "ID of the failed task to retry. Accepts full ID or short prefix (e.g. 'a1b2')."
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
            "name": "enki_edit_file",
            "description": "Edit a file using hashline anchors from your last read. Lines with a {line}:{hash}| prefix reference existing lines (anchors). Lines without a prefix are new content. The region between the first and last anchor is replaced.\n\nExamples:\n- Replace lines 3-4: anchor line 2, new content, anchor line 5\n- Insert after line 2: anchor line 2, new content\n- Delete lines 3-4: anchor line 2, anchor line 5",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the file to edit."
                    },
                    "content": {
                        "type": "string",
                        "description": "Edit content mixing hashline anchors and new lines."
                    }
                },
                "required": ["path", "content"]
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

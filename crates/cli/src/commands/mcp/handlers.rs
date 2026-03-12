use std::collections::{HashMap, HashSet};

use ascii_dag::graph::DAG;
use chrono::Utc;
use enki_core::dag::EdgeCondition;
use enki_core::orchestrator::{StepDef, StepDep};
use enki_core::types::{
    BacklogItem, Execution, ExecutionStatus, Id, Message, MessagePriority, MessageStatus, MessageType, Task, TaskStatus, Tier,
};
use serde_json::{Value, json};

use super::super::open_db;

/// Write a signal file to `.enki/events/` so the coordinator picks up the change.
pub(super) fn write_signal_file(signal: &Value) -> Result<(), String> {
    let enki_dir = super::super::enki_dir().map_err(|e| e.to_string())?;
    let events_dir = enki_dir.join("events");
    std::fs::create_dir_all(&events_dir).map_err(|e| e.to_string())?;
    let filename = format!("{}.json", Id::new("sig"));
    let path = events_dir.join(filename);
    let content = serde_json::to_string(signal).map_err(|e| e.to_string())?;
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(())
}

// --- Tool implementations ---

pub(super) fn tool_status() -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let session_id = std::env::var("ENKI_SESSION_ID").ok();
    let tasks = if let Some(ref sid) = session_id {
        db.list_session_tasks(sid).map_err(|e| e.to_string())?
    } else {
        db.list_tasks().map_err(|e| e.to_string())?
    };

    let pending = tasks.iter().filter(|t| t.status == TaskStatus::Pending).count();
    let running = tasks.iter().filter(|t| t.status == TaskStatus::Running).count();
    let done = tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    let failed = tasks.iter().filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked)).count();

    Ok(format!("tasks: {} pending, {} running, {} done, {} failed ({} total)", pending, running, done, failed, tasks.len()))
}

pub(super) fn tool_task_create(args: &Value) -> Result<String, String> {
    let title = args["title"].as_str().ok_or("missing required parameter: title")?;
    let description = args["description"].as_str().map(String::from);
    let tier_str = args["tier"].as_str().unwrap_or("heavy");

    let tier = Tier::from_str(tier_str).ok_or_else(|| format!("invalid tier: {tier_str}"))?;
    let db = open_db().map_err(|e| e.to_string())?;
    let session_id = std::env::var("ENKI_SESSION_ID").ok();

    // Parse optional needs (task_ids only for standalone tasks).
    let task_deps: Vec<(String, EdgeCondition)> = if let Some(arr) = args["needs"].as_array() {
        let mut deps = Vec::new();
        for v in arr {
            if let Some(s) = v.as_str() {
                // Bare string → task_id with default Merged condition.
                let tid = Id(s.to_string());
                db.get_task(&tid).map_err(|_| format!("depends on unknown task '{s}'"))?;
                deps.push((s.to_string(), EdgeCondition::Merged));
            } else if let Some(task_id) = v.get("task").and_then(|t| t.as_str()) {
                let tid = Id(task_id.to_string());
                db.get_task(&tid).map_err(|_| format!("depends on unknown task '{task_id}'"))?;
                deps.push((task_id.to_string(), parse_condition(v)));
            } else {
                return Err("standalone task needs only accepts task IDs (bare string or {\"task\": \"...\"})"
                    .to_string());
            }
        }
        deps
    } else {
        Vec::new()
    };

    let now = Utc::now();
    let task = Task {
        id: Id::new("task"),
        session_id: session_id.clone(),
        title: title.to_string(),
        description,
        status: TaskStatus::Pending,
        assigned_to: None,
        copy_path: None,
        branch: None,
        base_branch: None,
        tier: Some(tier),
        current_activity: None,
        created_at: now,
        updated_at: now,
    };

    db.insert_task(&task).map_err(|e| e.to_string())?;

    // Wire up task-level dependencies.
    for (dep_tid, _condition) in &task_deps {
        let dep_id = Id(dep_tid.clone());
        db.insert_dependency(&task.id, &dep_id).map_err(|e| e.to_string())?;
    }

    write_signal_file(&json!({"type": "task_created", "task_id": task.id.as_str()}))?;

    let mut result = format!("Created task '{}' ({}) — status: ready, tier: {}", title, task.id.short(), tier_str);
    if !task_deps.is_empty() {
        let dep_strs: Vec<&str> = task_deps.iter().map(|(tid, _)| tid.as_str()).collect();
        result.push_str(&format!(", needs: {}", dep_strs.join(", ")));
    }
    Ok(result)
}

pub(super) fn tool_task_list() -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let session_id = std::env::var("ENKI_SESSION_ID").ok();
    let tasks = if let Some(ref sid) = session_id {
        db.list_session_tasks(sid).map_err(|e| e.to_string())?
    } else {
        db.list_tasks().map_err(|e| e.to_string())?
    };

    if tasks.is_empty() {
        return Ok("No tasks.".into());
    }

    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            let tier = t.tier.map(|t| t.as_str()).unwrap_or("-");
            let activity = t.current_activity.as_deref().unwrap_or("");
            let short = t.id.short();
            if activity.is_empty() {
                format!("{short} | {} | {} | {}", t.status.as_str(), tier, t.title)
            } else {
                format!("{short} | {} | {} | {} [{activity}]", t.status.as_str(), tier, t.title)
            }
        })
        .collect();
    Ok(lines.join("\n"))
}

/// A parsed dependency — either referencing a step within the same execution,
/// or a global task_id for cross-group dependencies.
#[allow(dead_code)]
enum ParsedDep {
    /// Step ID within the same execution (bare string or {"step": "..."}).
    Step(StepDep),
    /// Global task_id reference ({"task": "..."}).
    /// The condition is stored for the DAG construction layer; the
    /// `task_dependencies` DB table only records the existence of the edge.
    Task { task_id: String, condition: EdgeCondition },
}

fn parse_condition(v: &Value) -> EdgeCondition {
    match v.get("condition").and_then(|c| c.as_str()) {
        Some("completed") => EdgeCondition::Completed,
        Some("started") => EdgeCondition::Started,
        _ => EdgeCondition::Merged,
    }
}

/// Parse a dependency from JSON — accepts:
/// - bare string → step_id within same execution
/// - {"step": "..."} → step_id within same execution
/// - {"task": "..."} → global task_id reference
fn parse_dep(v: &Value) -> Option<ParsedDep> {
    if let Some(s) = v.as_str() {
        Some(ParsedDep::Step(StepDep::from(s.to_string())))
    } else if let Some(task_id) = v.get("task").and_then(|s| s.as_str()) {
        Some(ParsedDep::Task {
            task_id: task_id.to_string(),
            condition: parse_condition(v),
        })
    } else if let Some(step_id) = v.get("step").and_then(|s| s.as_str()) {
        Some(ParsedDep::Step(StepDep {
            step_id: step_id.to_string(),
            condition: parse_condition(v),
        }))
    } else {
        None
    }
}

pub(super) fn tool_execution_create(args: &Value) -> Result<String, String> {
    let steps = args["steps"]
        .as_array()
        .ok_or("missing required parameter: steps")?;

    if steps.is_empty() {
        return Err("steps array must not be empty".into());
    }

    // Parse and validate all steps up front.
    struct StepWithDeps {
        def: StepDef,
        parsed_deps: Vec<ParsedDep>,
    }

    let mut parsed_steps: Vec<StepWithDeps> = Vec::new();
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
        let tier_str = step["tier"].as_str().unwrap_or("heavy");
        let tier =
            Tier::from_str(tier_str).ok_or_else(|| format!("invalid tier: {tier_str}"))?;
        let parsed_deps: Vec<ParsedDep> = step["needs"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_dep).collect())
            .unwrap_or_default();
        let checkpoint = step["checkpoint"].as_bool().unwrap_or(false);
        let role = step["role"].as_str().map(|s| s.to_string());

        // Extract step-local deps for StepDef (used by orchestrator).
        let needs: Vec<StepDep> = parsed_deps
            .iter()
            .filter_map(|d| match d {
                ParsedDep::Step(sd) => Some(sd.clone()),
                ParsedDep::Task { .. } => None,
            })
            .collect();

        if !step_ids.insert(id.clone()) {
            return Err(format!("duplicate step id: {id}"));
        }
        parsed_steps.push(StepWithDeps {
            def: StepDef { id, title, description, tier, needs, checkpoint, role },
            parsed_deps,
        });
    }

    // Validate step-local dependency references.
    for ps in &parsed_steps {
        for dep in &ps.parsed_deps {
            match dep {
                ParsedDep::Step(sd) => {
                    if !step_ids.contains(&sd.step_id) {
                        return Err(format!(
                            "step '{}' depends on unknown step '{}'",
                            ps.def.id, sd.step_id
                        ));
                    }
                    if sd.step_id == ps.def.id {
                        return Err(format!("step '{}' cannot depend on itself", ps.def.id));
                    }
                }
                ParsedDep::Task { .. } => {} // validated below against DB
            }
        }
    }

    // Create execution + tasks + steps + dependencies atomically.
    let db = open_db().map_err(|e| e.to_string())?;

    // Validate task-based deps exist in DB.
    for ps in &parsed_steps {
        for dep in &ps.parsed_deps {
            if let ParsedDep::Task { task_id, .. } = dep {
                let tid = Id(task_id.clone());
                db.get_task(&tid).map_err(|_| {
                    format!(
                        "step '{}' depends on unknown task '{}'",
                        ps.def.id, task_id
                    )
                })?;
            }
        }
    }

    let execution_id = Id::new("exec");
    let now = Utc::now();
    let session_id = std::env::var("ENKI_SESSION_ID").ok();

    let execution = Execution {
        id: execution_id.clone(),
        session_id: session_id.clone(),
        status: ExecutionStatus::Running,
        created_at: now,
    };
    db.insert_execution(&execution).map_err(|e| e.to_string())?;

    let mut step_task_ids: HashMap<String, Id> = HashMap::new();

    for ps in &parsed_steps {
        let task_id = Id::new("task");
        let task = Task {
            id: task_id.clone(),
            session_id: session_id.clone(),
            title: ps.def.title.clone(),
            description: Some(ps.def.description.clone()),
            status: TaskStatus::Pending,
            assigned_to: None,
            copy_path: None,
            branch: None,
            base_branch: None,
            tier: Some(ps.def.tier),
            current_activity: None,
            created_at: now,
            updated_at: now,
        };
        db.insert_task(&task).map_err(|e| e.to_string())?;
        db.insert_execution_step(&execution_id, &ps.def.id, &task_id)
            .map_err(|e| e.to_string())?;
        step_task_ids.insert(ps.def.id.clone(), task_id);
    }

    // Wire up dependencies (task-level).
    for ps in &parsed_steps {
        let task_id = &step_task_ids[&ps.def.id];
        for dep in &ps.parsed_deps {
            match dep {
                ParsedDep::Step(sd) => {
                    let dep_task_id = &step_task_ids[&sd.step_id];
                    db.insert_dependency(task_id, dep_task_id)
                        .map_err(|e| e.to_string())?;
                }
                ParsedDep::Task { task_id: dep_tid, .. } => {
                    let dep_task_id = Id(dep_tid.clone());
                    db.insert_dependency(task_id, &dep_task_id)
                        .map_err(|e| e.to_string())?;
                }
            }
        }
    }

    write_signal_file(&json!({"type": "execution_created", "execution_id": execution_id.as_str()}))?;

    // Build response.
    let mut lines = vec![format!("execution: {}", execution_id.short())];
    for ps in &parsed_steps {
        let task_id = &step_task_ids[&ps.def.id];
        let status = "pending";
        let deps = if ps.parsed_deps.is_empty() {
            String::new()
        } else {
            let dep_strs: Vec<String> = ps.parsed_deps.iter().map(|d| match d {
                ParsedDep::Step(sd) => sd.step_id.clone(),
                ParsedDep::Task { task_id, .. } => format!("task:{}", &task_id[..task_id.len().min(12)]),
            }).collect();
            format!(" (needs: {})", dep_strs.join(", "))
        };
        lines.push(format!(
            "  {} → {} | {} | {}{}",
            ps.def.id, task_id.short(), status, ps.def.title, deps
        ));
    }
    Ok(lines.join("\n"))
}

pub(super) fn tool_execution_add_steps(args: &Value) -> Result<String, String> {
    let exec_id_str = args["execution_id"]
        .as_str()
        .ok_or("missing required parameter: execution_id")?;
    let steps = args["steps"]
        .as_array()
        .ok_or("missing required parameter: steps")?;

    if steps.is_empty() {
        return Err("steps array must not be empty".into());
    }

    let db = open_db().map_err(|e| e.to_string())?;
    let exec_id = Id(exec_id_str.to_string());

    // Load existing step IDs to validate deps.
    let existing_steps = db
        .get_execution_steps(&exec_id)
        .map_err(|e| e.to_string())?;
    let existing_step_ids: HashSet<String> = existing_steps.iter().map(|(sid, _)| sid.clone()).collect();

    // Parse new steps.
    struct StepWithDeps {
        def: StepDef,
        parsed_deps: Vec<ParsedDep>,
    }

    let mut parsed_steps: Vec<StepWithDeps> = Vec::new();
    let mut new_step_ids: HashSet<String> = HashSet::new();

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
        let tier_str = step["tier"].as_str().unwrap_or("heavy");
        let tier =
            Tier::from_str(tier_str).ok_or_else(|| format!("invalid tier: {tier_str}"))?;
        let parsed_deps: Vec<ParsedDep> = step["needs"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_dep).collect())
            .unwrap_or_default();
        let checkpoint = step["checkpoint"].as_bool().unwrap_or(false);
        let role = step["role"].as_str().map(|s| s.to_string());

        let needs: Vec<StepDep> = parsed_deps
            .iter()
            .filter_map(|d| match d {
                ParsedDep::Step(sd) => Some(sd.clone()),
                ParsedDep::Task { .. } => None,
            })
            .collect();

        if existing_step_ids.contains(&id) {
            return Err(format!("step id '{id}' already exists in this execution"));
        }
        if !new_step_ids.insert(id.clone()) {
            return Err(format!("duplicate step id: {id}"));
        }
        parsed_steps.push(StepWithDeps {
            def: StepDef { id, title, description, tier, needs, checkpoint, role },
            parsed_deps,
        });
    }

    // Validate step-local deps reference existing or new steps.
    let all_step_ids: HashSet<String> = existing_step_ids.union(&new_step_ids).cloned().collect();
    for ps in &parsed_steps {
        for dep in &ps.parsed_deps {
            match dep {
                ParsedDep::Step(sd) => {
                    if !all_step_ids.contains(&sd.step_id) {
                        return Err(format!(
                            "step '{}' depends on unknown step '{}'",
                            ps.def.id, sd.step_id
                        ));
                    }
                    if sd.step_id == ps.def.id {
                        return Err(format!("step '{}' cannot depend on itself", ps.def.id));
                    }
                }
                ParsedDep::Task { .. } => {} // validated below against DB
            }
        }
    }

    // Validate task-based deps exist in DB.
    for ps in &parsed_steps {
        for dep in &ps.parsed_deps {
            if let ParsedDep::Task { task_id, .. } = dep {
                let tid = Id(task_id.clone());
                db.get_task(&tid).map_err(|_| {
                    format!(
                        "step '{}' depends on unknown task '{}'",
                        ps.def.id, task_id
                    )
                })?;
            }
        }
    }

    // Write tasks + steps + deps to DB.
    let session_id = std::env::var("ENKI_SESSION_ID").ok();
    let now = Utc::now();
    let mut step_task_ids: HashMap<String, Id> = HashMap::new();

    // Map existing step_id → task_id for dep wiring.
    let existing_task_map: HashMap<String, Id> = existing_steps.into_iter().collect();

    for ps in &parsed_steps {
        let task_id = Id::new("task");
        let task = Task {
            id: task_id.clone(),
            session_id: session_id.clone(),
            title: ps.def.title.clone(),
            description: Some(ps.def.description.clone()),
            status: TaskStatus::Pending,
            assigned_to: None,
            copy_path: None,
            branch: None,
            base_branch: None,
            tier: Some(ps.def.tier),
            current_activity: None,
            created_at: now,
            updated_at: now,
        };
        db.insert_task(&task).map_err(|e| e.to_string())?;
        db.insert_execution_step(&exec_id, &ps.def.id, &task_id)
            .map_err(|e| e.to_string())?;
        step_task_ids.insert(ps.def.id.clone(), task_id);
    }

    // Wire dependencies.
    let all_task_map: HashMap<String, Id> = existing_task_map
        .into_iter()
        .chain(step_task_ids.iter().map(|(k, v)| (k.clone(), v.clone())))
        .collect();

    for ps in &parsed_steps {
        let task_id = &step_task_ids[&ps.def.id];
        for dep in &ps.parsed_deps {
            match dep {
                ParsedDep::Step(sd) => {
                    if let Some(dep_task_id) = all_task_map.get(&sd.step_id) {
                        db.insert_dependency(task_id, dep_task_id)
                            .map_err(|e| e.to_string())?;
                    }
                }
                ParsedDep::Task { task_id: dep_tid, .. } => {
                    let dep_task_id = Id(dep_tid.clone());
                    db.insert_dependency(task_id, &dep_task_id)
                        .map_err(|e| e.to_string())?;
                }
            }
        }
    }

    write_signal_file(&json!({
        "type": "steps_added",
        "execution_id": exec_id.as_str()
    }))?;

    let mut lines = vec![format!("added {} steps to execution {}", parsed_steps.len(), exec_id.short())];
    for ps in &parsed_steps {
        let task_id = &step_task_ids[&ps.def.id];
        let deps = if ps.parsed_deps.is_empty() {
            String::new()
        } else {
            let dep_strs: Vec<String> = ps.parsed_deps.iter().map(|d| match d {
                ParsedDep::Step(sd) => sd.step_id.clone(),
                ParsedDep::Task { task_id, .. } => format!("task:{}", &task_id[..task_id.len().min(12)]),
            }).collect();
            format!(" (needs: {})", dep_strs.join(", "))
        };
        lines.push(format!(
            "  {} → {} | pending | {}{}",
            ps.def.id, task_id.short(), ps.def.title, deps
        ));
    }
    Ok(lines.join("\n"))
}

pub(super) fn tool_resume(args: &Value) -> Result<String, String> {
    let exec_id_str = args["execution_id"]
        .as_str()
        .ok_or("missing required parameter: execution_id")?;
    let step_id = args["step_id"].as_str();

    let mut signal = json!({
        "type": "resume",
        "execution_id": exec_id_str
    });
    if let Some(sid) = step_id {
        signal["step_id"] = json!(sid);
    }
    write_signal_file(&signal)?;

    if let Some(sid) = step_id {
        Ok(format!("Resume signal sent for step '{sid}' in execution {exec_id_str}."))
    } else {
        Ok(format!("Resume signal sent for execution {exec_id_str}."))
    }
}

pub(super) fn tool_task_retry(args: &Value) -> Result<String, String> {
    let task_id_str = args["task_id"].as_str().ok_or("missing required parameter: task_id")?;
    let db = open_db().map_err(|e| e.to_string())?;
    let task_id = db.resolve_task_id(task_id_str, None).map_err(|e| e.to_string())?;

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
        db.update_task_status(&task_id, TaskStatus::Pending).map_err(|e| e.to_string())?;
        return Ok(format!("Task {} reset to pending (standalone, no execution).", task_id_str));
    };

    // Reset the failed task to pending.
    db.update_task_status(&task_id, TaskStatus::Pending).map_err(|e| e.to_string())?;

    // Reset any blocked siblings to pending
    // (the scheduler's DAG will re-evaluate their readiness).
    let steps = db.get_execution_steps(&exec_id).map_err(|e| e.to_string())?;
    let mut unblocked = 0;
    for (_step_id, sibling_task_id) in &steps {
        if sibling_task_id == &task_id {
            continue;
        }
        let sibling = db.get_task(sibling_task_id).map_err(|e| e.to_string())?;
        if sibling.status == TaskStatus::Blocked {
            db.update_task_status(sibling_task_id, TaskStatus::Pending).map_err(|e| e.to_string())?;
            unblocked += 1;
        }
    }

    // Reset the execution status to running so the poll loop rediscovers it.
    db.update_execution_status(&exec_id, ExecutionStatus::Running).map_err(|e| e.to_string())?;

    write_signal_file(&json!({"type": "retry_task", "task_id": task_id.0}))?;

    let mut result = format!("Task {} reset to pending.", task_id_str);
    if unblocked > 0 {
        result.push_str(&format!(" {} blocked sibling task(s) unblocked.", unblocked));
    }
    result.push_str(&format!(" Execution {} restored to running.", exec_id));
    Ok(result)
}

pub(super) fn tool_pause(args: &Value) -> Result<String, String> {
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

pub(super) fn tool_cancel(args: &Value) -> Result<String, String> {
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

pub(super) fn tool_stop_all() -> Result<String, String> {
    let enki_dir = super::super::enki_dir().map_err(|e| e.to_string())?;
    // Write legacy stop file (coordinator checks this directly).
    let stop_file = enki_dir.join("stop");
    std::fs::write(&stop_file, "").map_err(|e| e.to_string())?;
    // Also write a signal file for the orchestrator path.
    write_signal_file(&json!({"type": "stop_all"}))?;
    Ok("Stop signal sent. All workers will be killed on the next coordinator poll.".into())
}

pub(super) fn tool_worker_report(args: &Value, task_id: Option<&str>) -> Result<String, String> {
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

pub(super) fn tool_edit_file(args: &Value) -> Result<String, String> {
    let path = args["path"].as_str().ok_or("missing required parameter: path")?;
    let content = args["content"].as_str().ok_or("missing required parameter: content")?;

    let current = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {path}: {e}"))?;

    let result = enki_core::hashline::apply_edit(content, &current)?;

    std::fs::write(path, &result)
        .map_err(|e| format!("failed to write {path}: {e}"))?;

    Ok("ok".to_string())
}

pub(super) fn tool_dag(args: &Value) -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let session_id = std::env::var("ENKI_SESSION_ID").ok();

    // If execution_id provided, show that execution's DAG (existing behavior).
    if let Some(id_str) = args["execution_id"].as_str() {
        let exec_id = Id(id_str.to_string());
        let dag = db
            .load_dag(&exec_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("No DAG found for execution {}.", exec_id))?;

        let nodes = dag.nodes();
        if nodes.is_empty() {
            return Ok(format!("Execution {} has an empty DAG.", exec_id));
        }

        let labels: Vec<String> = nodes
            .iter()
            .map(|n| format!("{} [{}]", n.id, n.status.as_str()))
            .collect();

        let node_entries: Vec<(usize, &str)> =
            labels.iter().enumerate().map(|(i, l)| (i, l.as_str())).collect();
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            for dep in &node.deps {
                edges.push((dep.target, i));
            }
        }

        let ascii = DAG::from_edges(&node_entries, &edges);
        return Ok(ascii.render());
    }

    // No execution_id → show global graph from all session tasks.
    let tasks = if let Some(ref sid) = session_id {
        db.list_session_tasks(sid).map_err(|e| e.to_string())?
    } else {
        db.list_tasks().map_err(|e| e.to_string())?
    };

    if tasks.is_empty() {
        return Ok("No tasks.".into());
    }

    // Build task index and labels.
    let task_index: HashMap<String, usize> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.as_str().to_string(), i))
        .collect();

    let labels: Vec<String> = tasks
        .iter()
        .map(|t| {
            // Include step_id if task belongs to an execution.
            let exec_info = db.get_execution_for_task(&t.id).ok().flatten();
            match exec_info {
                Some((_, step_id)) => {
                    format!("{} ({}) [{}]", step_id, t.id.short(), t.status.as_str())
                }
                None => {
                    format!("{} [{}]", t.id.short(), t.status.as_str())
                }
            }
        })
        .collect();

    let node_entries: Vec<(usize, &str)> =
        labels.iter().enumerate().map(|(i, l)| (i, l.as_str())).collect();

    // Build edges from task_dependencies.
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        let deps = db.get_dependencies(&task.id).map_err(|e| e.to_string())?;
        for dep_id in deps {
            if let Some(&dep_idx) = task_index.get(dep_id.as_str()) {
                edges.push((dep_idx, i));
            }
        }
    }

    let ascii = DAG::from_edges(&node_entries, &edges);
    Ok(ascii.render())
}

pub(super) fn tool_quick_task(args: &Value) -> Result<String, String> {
    let prompt = args["prompt"]
        .as_str()
        .ok_or("missing required parameter: prompt")?;

    write_signal_file(&json!({
        "type": "quick_task",
        "prompt": prompt,
    }))?;

    Ok("queued".to_string())
}

// --- Backlog tool implementations ---

pub(super) fn tool_backlog_add(args: &Value) -> Result<String, String> {
    let body = args["body"].as_str().ok_or("missing required parameter: body")?;
    let db = open_db().map_err(|e| e.to_string())?;
    let session_id = std::env::var("ENKI_SESSION_ID")
        .map_err(|_| "ENKI_SESSION_ID not set".to_string())?;
    let now = Utc::now();

    let item = BacklogItem {
        id: Id::new("bl"),
        session_id,
        body: body.to_string(),
        created_at: now,
        updated_at: now,
    };
    db.insert_backlog_item(&item).map_err(|e| e.to_string())?;
    Ok(format!("backlog: added {}", item.id))
}

pub(super) fn tool_backlog_list() -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let session_id = std::env::var("ENKI_SESSION_ID")
        .map_err(|_| "ENKI_SESSION_ID not set".to_string())?;
    let items = db.list_backlog_items(&session_id).map_err(|e| e.to_string())?;

    if items.is_empty() {
        return Ok("No backlog items.".into());
    }

    let lines: Vec<String> = items
        .iter()
        .map(|item| {
            let short = item.id.short();
            let truncated: String = if item.body.len() > 100 {
                format!("{}...", &item.body[..100])
            } else {
                item.body.clone()
            };
            let ts = item.created_at.format("%Y-%m-%d %H:%M");
            format!("{short} | {truncated} | {ts}")
        })
        .collect();
    Ok(lines.join("\n"))
}

pub(super) fn tool_backlog_update(args: &Value) -> Result<String, String> {
    let id_str = args["id"].as_str().ok_or("missing required parameter: id")?;
    let body = args["body"].as_str().ok_or("missing required parameter: body")?;
    let db = open_db().map_err(|e| e.to_string())?;
    let id = Id(id_str.to_string());
    db.update_backlog_item(&id, body).map_err(|e| e.to_string())?;
    Ok(format!("backlog: updated {id_str}"))
}

pub(super) fn tool_backlog_remove(args: &Value) -> Result<String, String> {
    let id_str = args["id"].as_str().ok_or("missing required parameter: id")?;
    let db = open_db().map_err(|e| e.to_string())?;
    let id = Id(id_str.to_string());
    db.delete_backlog_item(&id).map_err(|e| e.to_string())?;
    Ok(format!("backlog: removed {id_str}"))
}

pub(super) fn tool_backlog_pick(args: &Value) -> Result<String, String> {
    let id_str = args["id"].as_str().ok_or("missing required parameter: id")?;
    let db = open_db().map_err(|e| e.to_string())?;
    let id = Id(id_str.to_string());
    let item = db.get_backlog_item(&id).map_err(|e| e.to_string())?;
    db.delete_backlog_item(&id).map_err(|e| e.to_string())?;
    Ok(item.body)
}

// --- Mail helpers ---

/// Derive the caller's mail address from their role and task_id.
pub(super) fn caller_addr(role: &str, task_id: Option<&str>) -> String {
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
pub(super) fn mail_notice(my_addr: &str) -> Result<String, String> {
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

pub(super) fn tool_mail_send(args: &Value, from_addr: &str) -> Result<String, String> {
    let to = args["to"].as_str().ok_or("missing required parameter: to")?;
    let subject = args["subject"].as_str().ok_or("missing required parameter: subject")?;
    let body = args["body"].as_str().ok_or("missing required parameter: body")?;
    let priority_str = args["priority"].as_str().unwrap_or("normal");
    let priority = MessagePriority::from_str(priority_str)
        .ok_or_else(|| format!("invalid priority: {priority_str}"))?;

    let msg_type_str = args["msg_type"].as_str().unwrap_or("info");
    let msg_type = MessageType::from_str(msg_type_str)
        .ok_or_else(|| format!("invalid msg_type: {msg_type_str} (use: info, request)"))?;

    let thread_id = if let Some(tid) = args["thread_id"].as_str() {
        Some(tid.to_string())
    } else if msg_type == MessageType::Request {
        Some(Id::new("thread").to_string())
    } else {
        None
    };
    let reply_to = args["reply_to"].as_str().map(String::from);

    let expires_at = args["ttl_seconds"].as_u64().map(|ttl| {
        Utc::now() + chrono::Duration::seconds(ttl as i64)
    });

    let status = if msg_type == MessageType::Request {
        MessageStatus::Pending
    } else {
        MessageStatus::Completed
    };

    let db = open_db().map_err(|e| e.to_string())?;
    let now = Utc::now();

    // Broadcast fan-out: create individual messages per active worker.
    if to == "@workers" {
        let session_id = std::env::var("ENKI_SESSION_ID")
            .map_err(|_| "ENKI_SESSION_ID not set")?;
        let worker_addrs = db.list_running_worker_addrs(&session_id)
            .map_err(|e| e.to_string())?;

        // Insert the canonical @workers message for thread tracking.
        let canonical = Message {
            id: Id::new("msg"),
            from_addr: from_addr.to_string(),
            to_addr: "@workers".to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
            priority,
            msg_type,
            thread_id: thread_id.clone(),
            reply_to: reply_to.clone(),
            read: true, // canonical copy is not for reading directly
            status,
            expires_at,
            created_at: now,
        };
        db.insert_message(&canonical).map_err(|e| e.to_string())?;

        // Fan out to each active worker.
        let mut count = 0;
        for addr in &worker_addrs {
            let msg = Message {
                id: Id::new("msg"),
                from_addr: from_addr.to_string(),
                to_addr: addr.clone(),
                subject: subject.to_string(),
                body: body.to_string(),
                priority,
                msg_type,
                thread_id: thread_id.clone(),
                reply_to: reply_to.clone(),
                read: false,
                status,
                expires_at,
                created_at: now,
            };
            db.insert_message(&msg).map_err(|e| e.to_string())?;
            count += 1;
        }

        write_signal_file(&json!({
            "type": "mail",
            "message_id": canonical.id.as_str(),
            "from": from_addr,
            "to": "@workers",
            "subject": subject,
            "priority": priority_str,
        }))?;

        return Ok(format!("Broadcast sent to {count} worker(s): \"{subject}\" ({})", canonical.id));
    }

    let msg = Message {
        id: Id::new("msg"),
        from_addr: from_addr.to_string(),
        to_addr: to.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
        priority,
        msg_type,
        thread_id,
        reply_to,
        read: false,
        status,
        expires_at,
        created_at: now,
    };

    db.insert_message(&msg).map_err(|e| e.to_string())?;

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

pub(super) fn tool_mail_check(my_addr: &str) -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let mut messages = db.list_messages(my_addr, true).map_err(|e| e.to_string())?;
    // Also include broadcast messages.
    let broadcast = db.list_messages("@workers", true).map_err(|e| e.to_string())?;
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

pub(super) fn tool_mail_read(args: &Value) -> Result<String, String> {
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

pub(super) fn tool_mail_inbox(my_addr: &str) -> Result<String, String> {
    let db = open_db().map_err(|e| e.to_string())?;
    let mut messages = db.list_messages(my_addr, false).map_err(|e| e.to_string())?;
    let broadcast = db.list_messages("@workers", false).map_err(|e| e.to_string())?;
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

pub(super) fn tool_mail_reply(args: &Value, from_addr: &str) -> Result<String, String> {
    let message_id = args["message_id"].as_str().ok_or("missing required parameter: message_id")?;
    let body = args["body"].as_str().ok_or("missing required parameter: body")?;
    let status_str = args["status"].as_str().unwrap_or("accepted");
    let status = MessageStatus::from_str(status_str)
        .ok_or_else(|| format!("invalid status: {status_str} (use: accepted, rejected, completed)"))?;

    let db = open_db().map_err(|e| e.to_string())?;
    let original = db.get_message(message_id).map_err(|e| e.to_string())?;

    if original.msg_type != MessageType::Request {
        return Err(format!("message {} is not a request (type: {})", message_id, original.msg_type.as_str()));
    }

    db.update_message_status(message_id, status).map_err(|e| e.to_string())?;

    let now = Utc::now();
    let reply = Message {
        id: Id::new("msg"),
        from_addr: from_addr.to_string(),
        to_addr: original.from_addr.clone(),
        subject: format!("Re: {}", original.subject),
        body: body.to_string(),
        priority: original.priority,
        msg_type: MessageType::Response,
        thread_id: original.thread_id.clone(),
        reply_to: Some(message_id.to_string()),
        read: false,
        status: MessageStatus::Completed,
        expires_at: None,
        created_at: now,
    };

    db.insert_message(&reply).map_err(|e| e.to_string())?;

    write_signal_file(&json!({
        "type": "mail",
        "message_id": reply.id.as_str(),
        "from": from_addr,
        "to": original.from_addr,
        "subject": reply.subject,
        "priority": reply.priority.as_str(),
    }))?;

    Ok(format!("Reply sent to {}: status={} ({})", original.from_addr, status_str, reply.id))
}

pub(super) fn tool_mail_thread(args: &Value) -> Result<String, String> {
    let thread_id = args["thread_id"].as_str().ok_or("missing required parameter: thread_id")?;
    let db = open_db().map_err(|e| e.to_string())?;
    let messages = db.list_thread(thread_id).map_err(|e| e.to_string())?;

    if messages.is_empty() {
        return Ok(format!("No messages in thread {thread_id}."));
    }

    let mut lines = vec![format!("Thread {thread_id} ({} message(s)):", messages.len())];
    for msg in &messages {
        let ts = msg.created_at.format("%H:%M:%S");
        lines.push(format!(
            "  {} | [{}] {} -> {} | {} | status: {} | {}",
            msg.id, msg.msg_type.as_str(), msg.from_addr, msg.to_addr,
            msg.subject, msg.status.as_str(), ts
        ));
        if !msg.body.is_empty() {
            for line in msg.body.lines() {
                lines.push(format!("    > {line}"));
            }
        }
    }
    Ok(lines.join("\n"))
}

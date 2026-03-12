use chrono::{DateTime, Utc};
use rusqlite::Row;

use crate::types::*;

// --- Schema DDL ---

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    started_at  TEXT NOT NULL,
    ended_at    TEXT
);

CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    session_id  TEXT,
    title       TEXT NOT NULL,
    description TEXT,
    status      TEXT NOT NULL DEFAULT 'open',
    assigned_to TEXT,
    copy_path   TEXT,
    branch      TEXT,
    base_branch TEXT,
    tier        TEXT,
    current_activity TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS task_dependencies (
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    depends_on  TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, depends_on)
);

CREATE TABLE IF NOT EXISTS executions (
    id          TEXT PRIMARY KEY,
    session_id  TEXT,
    status      TEXT NOT NULL DEFAULT 'running',
    dag         TEXT,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS execution_steps (
    execution_id TEXT NOT NULL REFERENCES executions(id),
    step_id      TEXT NOT NULL,
    task_id      TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (execution_id, step_id)
);

CREATE TABLE IF NOT EXISTS merge_requests (
    id          TEXT PRIMARY KEY,
    session_id  TEXT,
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    branch      TEXT NOT NULL,
    base_branch TEXT NOT NULL DEFAULT 'main',
    status      TEXT NOT NULL DEFAULT 'queued',
    priority    INTEGER NOT NULL DEFAULT 2,
    diff_stats  TEXT,
    review_note TEXT,
    execution_id TEXT,
    step_id      TEXT,
    queued_at   TEXT NOT NULL,
    started_at  TEXT,
    merged_at   TEXT
);

CREATE TABLE IF NOT EXISTS task_outputs (
    task_id     TEXT PRIMARY KEY REFERENCES tasks(id),
    output      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS backlog_items (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id          TEXT PRIMARY KEY,
    from_addr   TEXT NOT NULL,
    to_addr     TEXT NOT NULL,
    subject     TEXT NOT NULL,
    body        TEXT NOT NULL DEFAULT '',
    priority    TEXT NOT NULL DEFAULT 'normal',
    msg_type    TEXT NOT NULL DEFAULT 'info',
    thread_id   TEXT,
    reply_to    TEXT,
    read        INTEGER NOT NULL DEFAULT 0,
    status      TEXT NOT NULL DEFAULT 'pending',
    expires_at  TEXT,
    created_at  TEXT NOT NULL
);

";

pub(super) const INDEXES: &str = "
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id);
CREATE INDEX IF NOT EXISTS idx_executions_session ON executions(session_id);
CREATE INDEX IF NOT EXISTS idx_merge_requests_status ON merge_requests(status);
CREATE INDEX IF NOT EXISTS idx_merge_requests_session ON merge_requests(session_id);
CREATE INDEX IF NOT EXISTS idx_backlog_items_session ON backlog_items(session_id);
CREATE INDEX IF NOT EXISTS idx_backlog_items_created ON backlog_items(created_at);
CREATE INDEX IF NOT EXISTS idx_messages_to_addr ON messages(to_addr);
CREATE INDEX IF NOT EXISTS idx_messages_unread ON messages(to_addr, read);
CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(thread_id);
CREATE INDEX IF NOT EXISTS idx_messages_expires ON messages(expires_at);
";

// --- Schema parsing for auto-migration ---

pub(super) fn parse_schema_columns(schema: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut tables = Vec::new();
    let mut remaining = schema;

    while !remaining.is_empty() {
        let upper = remaining.to_uppercase();
        let Some(pos) = upper.find("CREATE TABLE") else { break };

        if upper[..pos].ends_with("VIRTUAL ") {
            remaining = &remaining[pos + 12..];
            continue;
        }

        let after_create = &remaining[pos + 12..];
        let after_create = after_create
            .strip_prefix(" IF NOT EXISTS")
            .unwrap_or(after_create)
            .trim_start();

        let table_end = after_create.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(0);
        let table_name = after_create[..table_end].to_string();

        let Some(paren_start) = after_create.find('(') else {
            remaining = &after_create[table_end..];
            continue;
        };
        let block = &after_create[paren_start + 1..];

        let mut depth = 1;
        let mut end = 0;
        for (i, ch) in block.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let columns_block = &block[..end];

        let mut columns = Vec::new();
        let mut current = String::new();
        let mut paren_depth = 0;
        for ch in columns_block.chars() {
            match ch {
                '(' => { paren_depth += 1; current.push(ch); }
                ')' => { paren_depth -= 1; current.push(ch); }
                ',' if paren_depth == 0 => {
                    let trimmed = current.trim().to_string();
                    if !trimmed.is_empty() {
                        columns.push(trimmed);
                    }
                    current.clear();
                }
                _ => current.push(ch),
            }
        }
        let trimmed = current.trim().to_string();
        if !trimmed.is_empty() {
            columns.push(trimmed);
        }

        let mut parsed = Vec::new();
        for col_line in &columns {
            let first_word = col_line.split_whitespace().next().unwrap_or("");
            let skip = ["PRIMARY", "UNIQUE", "FOREIGN", "CHECK", "CONSTRAINT"];
            if skip.contains(&first_word.to_uppercase().as_str()) {
                continue;
            }
            let mut parts = col_line.splitn(2, char::is_whitespace);
            let name = parts.next().unwrap_or("").to_string();
            let def = parts.next().unwrap_or("").trim().to_string();
            if !name.is_empty() {
                let safe_def = sanitize_column_def(&def);
                parsed.push((name, safe_def));
            }
        }

        tables.push((table_name, parsed));
        remaining = &block[end..];
    }

    tables
}

fn sanitize_column_def(def: &str) -> String {
    let has_default = def.to_uppercase().contains("DEFAULT");
    let has_not_null = def.to_uppercase().contains("NOT NULL");

    if has_not_null && !has_default {
        let mut result = String::new();
        let upper = def.to_uppercase();
        let mut i = 0;
        while let Some(pos) = upper[i..].find("NOT NULL") {
            result.push_str(&def[i..i + pos]);
            i += pos + 8;
        }
        result.push_str(&def[i..]);
        result.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        def.to_string()
    }
}

// --- Row mapping functions ---

pub(super) fn parse_dt(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_default()
}

pub(super) fn parse_opt_dt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.map(parse_dt)
}

pub(super) fn row_to_task(row: &Row) -> rusqlite::Result<Task> {
    let status_str: String = row.get(4)?;
    let tier_str: Option<String> = row.get(9)?;
    Ok(Task {
        id: Id(row.get(0)?),
        session_id: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::Pending),
        assigned_to: row.get::<_, Option<String>>(5)?.map(Id),
        copy_path: row.get(6)?,
        branch: row.get(7)?,
        base_branch: row.get(8)?,
        tier: tier_str.and_then(|s| Tier::from_str(&s)),
        current_activity: row.get(10)?,
        created_at: parse_dt(row.get(11)?),
        updated_at: parse_dt(row.get(12)?),
    })
}

pub(super) fn row_to_session(row: &Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: Id(row.get(0)?),
        started_at: parse_dt(row.get(1)?),
        ended_at: parse_opt_dt(row.get(2)?),
    })
}

pub(super) fn row_to_execution(row: &Row) -> rusqlite::Result<Execution> {
    let status_str: String = row.get(2)?;
    Ok(Execution {
        id: Id(row.get(0)?),
        session_id: row.get(1)?,
        status: ExecutionStatus::from_str(&status_str).unwrap_or(ExecutionStatus::Running),
        created_at: parse_dt(row.get(3)?),
    })
}

pub(super) fn row_to_merge_request(row: &Row) -> rusqlite::Result<MergeRequest> {
    let status_str: String = row.get(4)?;
    let diff_stats_str: Option<String> = row.get(6)?;
    Ok(MergeRequest {
        id: Id(row.get(0)?),
        task_id: Id(row.get(1)?),
        branch: row.get(2)?,
        base_branch: row.get(3)?,
        status: MergeStatus::from_str(&status_str).unwrap_or(MergeStatus::Queued),
        priority: row.get(5)?,
        diff_stats: diff_stats_str.and_then(|s| serde_json::from_str(&s).ok()),
        review_note: row.get(7)?,
        execution_id: row.get::<_, Option<String>>(8)?.map(Id),
        step_id: row.get(9)?,
        queued_at: parse_dt(row.get(10)?),
        started_at: parse_opt_dt(row.get(11)?),
        merged_at: parse_opt_dt(row.get(12)?),
    })
}

pub(super) fn row_to_backlog_item(row: &Row) -> rusqlite::Result<BacklogItem> {
    Ok(BacklogItem {
        id: Id(row.get(0)?),
        session_id: row.get(1)?,
        body: row.get(2)?,
        created_at: parse_dt(row.get(3)?),
        updated_at: parse_dt(row.get(4)?),
    })
}

pub(super) fn row_to_message(row: &Row) -> rusqlite::Result<Message> {
    let priority_str: String = row.get(5)?;
    let msg_type_str: String = row.get(6)?;
    let read_int: i32 = row.get(9)?;
    let status_str: String = row.get(10)?;
    Ok(Message {
        id: Id(row.get(0)?),
        from_addr: row.get(1)?,
        to_addr: row.get(2)?,
        subject: row.get(3)?,
        body: row.get(4)?,
        priority: MessagePriority::from_str(&priority_str).unwrap_or(MessagePriority::Normal),
        msg_type: MessageType::from_str(&msg_type_str).unwrap_or(MessageType::Info),
        thread_id: row.get(7)?,
        reply_to: row.get(8)?,
        read: read_int != 0,
        status: MessageStatus::from_str(&status_str).unwrap_or(MessageStatus::Pending),
        expires_at: parse_opt_dt(row.get(11)?),
        created_at: parse_dt(row.get(12)?),
    })
}

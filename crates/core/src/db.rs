use chrono::{DateTime, Utc};
use rusqlite::{Connection, Row, params};

use crate::types::*;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid data: {0}")]
    InvalidData(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        self.auto_migrate()?;
        Ok(())
    }

    fn auto_migrate(&self) -> Result<()> {
        for (table, columns) in parse_schema_columns(SCHEMA) {
            let existing: Vec<String> = {
                let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({})", table))?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };

            for (col_name, col_def) in &columns {
                if !existing.iter().any(|e| e == col_name) {
                    let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, col_name, col_def);
                    tracing::info!("auto-migrate: {}", sql);
                    self.conn.execute_batch(&sql)?;
                }
            }
        }
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // --- Tasks ---

    pub fn insert_task(&self, task: &Task) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tasks (id, title, description, status, assigned_to, worktree, branch, tier, current_activity, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                task.id.as_str(),
                task.title,
                task.description,
                task.status.as_str(),
                task.assigned_to.as_ref().map(Id::as_str),
                task.worktree,
                task.branch,
                task.tier.map(|t| t.as_str()),
                task.current_activity,
                task.created_at.to_rfc3339(),
                task.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn update_task_activity(&self, id: &Id, activity: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET current_activity = ?1 WHERE id = ?2",
            params![activity, id.as_str()],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &Id) -> Result<Task> {
        self.conn
            .query_row(
                "SELECT id, title, description, status, assigned_to, worktree, branch, tier, current_activity, created_at, updated_at
                 FROM tasks WHERE id = ?1",
                params![id.as_str()],
                row_to_task,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    DbError::NotFound(format!("task {id}"))
                }
                other => DbError::Sqlite(other),
            })
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, description, status, assigned_to, worktree, branch, tier, current_activity, created_at, updated_at
             FROM tasks ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn update_task_status(&self, id: &Id, status: TaskStatus) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.as_str(), Utc::now().to_rfc3339(), id.as_str()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound(format!("task {id}")));
        }
        Ok(())
    }

    pub fn assign_task(&self, task_id: &Id, agent_id: &Id, worktree: &str, branch: &str) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE tasks SET assigned_to = ?1, worktree = ?2, branch = ?3, status = 'running', updated_at = ?4 WHERE id = ?5",
            params![
                agent_id.as_str(),
                worktree,
                branch,
                Utc::now().to_rfc3339(),
                task_id.as_str(),
            ],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound(format!("task {task_id}")));
        }
        Ok(())
    }

    // --- Task Dependencies ---

    pub fn insert_dependency(&self, task_id: &Id, depends_on: &Id) -> Result<()> {
        self.conn.execute(
            "INSERT INTO task_dependencies (task_id, depends_on) VALUES (?1, ?2)",
            params![task_id.as_str(), depends_on.as_str()],
        )?;
        Ok(())
    }

    pub fn get_dependencies(&self, task_id: &Id) -> Result<Vec<Id>> {
        let mut stmt = self.conn.prepare(
            "SELECT depends_on FROM task_dependencies WHERE task_id = ?1",
        )?;
        let rows = stmt.query_map(params![task_id.as_str()], |row| {
            let s: String = row.get(0)?;
            Ok(Id(s))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn get_dependents(&self, task_id: &Id) -> Result<Vec<Id>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id FROM task_dependencies WHERE depends_on = ?1",
        )?;
        let rows = stmt.query_map(params![task_id.as_str()], |row| {
            let s: String = row.get(0)?;
            Ok(Id(s))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    // --- Agents ---

    pub fn insert_agent(&self, agent: &Agent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agents (id, acp_session, pid, status, current_task, started_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                agent.id.as_str(),
                agent.acp_session,
                agent.pid,
                agent.status.as_str(),
                agent.current_task.as_ref().map(Id::as_str),
                agent.started_at.to_rfc3339(),
                agent.last_seen.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn get_agent(&self, id: &Id) -> Result<Agent> {
        self.conn
            .query_row(
                "SELECT id, acp_session, pid, status, current_task, started_at, last_seen
                 FROM agents WHERE id = ?1",
                params![id.as_str()],
                row_to_agent,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    DbError::NotFound(format!("agent {id}"))
                }
                other => DbError::Sqlite(other),
            })
    }

    pub fn update_agent_status(&self, id: &Id, status: AgentStatus) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE agents SET status = ?1, last_seen = ?2 WHERE id = ?3",
            params![status.as_str(), Utc::now().to_rfc3339(), id.as_str()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound(format!("agent {id}")));
        }
        Ok(())
    }

    pub fn update_agent_last_seen(&self, id: &Id) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE agents SET last_seen = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.as_str()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound(format!("agent {id}")));
        }
        Ok(())
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, acp_session, pid, status, current_task, started_at, last_seen
             FROM agents ORDER BY started_at",
        )?;
        let rows = stmt.query_map([], row_to_agent)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn list_active_agents(&self) -> Result<Vec<Agent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, acp_session, pid, status, current_task, started_at, last_seen
             FROM agents WHERE status IN ('busy', 'stuck') ORDER BY started_at",
        )?;
        let rows = stmt.query_map([], row_to_agent)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    // --- Executions ---

    pub fn insert_execution(&self, exec: &Execution) -> Result<()> {
        self.conn.execute(
            "INSERT INTO executions (id, status, created_at)
             VALUES (?1, ?2, ?3)",
            params![
                exec.id.as_str(),
                exec.status.as_str(),
                exec.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn insert_execution_step(&self, execution_id: &Id, step_id: &str, task_id: &Id) -> Result<()> {
        self.conn.execute(
            "INSERT INTO execution_steps (execution_id, step_id, task_id) VALUES (?1, ?2, ?3)",
            params![execution_id.as_str(), step_id, task_id.as_str()],
        )?;
        Ok(())
    }

    pub fn update_execution_status(&self, id: &Id, status: ExecutionStatus) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE executions SET status = ?1 WHERE id = ?2",
            params![status.as_str(), id.as_str()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound(format!("execution {id}")));
        }
        Ok(())
    }

    pub fn get_execution_steps(&self, execution_id: &Id) -> Result<Vec<(String, Id)>> {
        let mut stmt = self.conn.prepare(
            "SELECT step_id, task_id FROM execution_steps WHERE execution_id = ?1",
        )?;
        let rows = stmt.query_map(params![execution_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, Id(row.get::<_, String>(1)?)))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn get_running_executions(&self) -> Result<Vec<Execution>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, created_at
             FROM executions WHERE status = 'running'",
        )?;
        let rows = stmt.query_map([], row_to_execution)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn get_execution_for_task(&self, task_id: &Id) -> Result<Option<(Id, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT execution_id, step_id FROM execution_steps WHERE task_id = ?1 LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![task_id.as_str()], |row| {
            Ok((Id(row.get::<_, String>(0)?), row.get::<_, String>(1)?))
        })?;
        match rows.next() {
            Some(Ok(pair)) => Ok(Some(pair)),
            Some(Err(e)) => Err(DbError::Sqlite(e)),
            None => Ok(None),
        }
    }

    // --- Task Outputs ---

    pub fn insert_task_output(&self, task_id: &Id, output: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO task_outputs (task_id, output) VALUES (?1, ?2)",
            params![task_id.as_str(), output],
        )?;
        Ok(())
    }

    pub fn get_task_output(&self, task_id: &Id) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT output FROM task_outputs WHERE task_id = ?1",
            params![task_id.as_str()],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(output) => Ok(Some(output)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DbError::Sqlite(e)),
        }
    }

    pub fn get_orphan_ready_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.title, t.description, t.status, t.assigned_to, t.worktree, t.branch, t.tier, t.current_activity, t.created_at, t.updated_at
             FROM tasks t
             WHERE t.status = 'ready'
             AND t.id NOT IN (SELECT es.task_id FROM execution_steps es)"
        )?;
        let tasks = stmt.query_map([], |row| row_to_task(row))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    // --- Merge Requests ---

    pub fn insert_merge_request(&self, mr: &MergeRequest) -> Result<()> {
        self.conn.execute(
            "INSERT INTO merge_requests (id, task_id, branch, base_branch, status, priority, diff_stats, review_note, execution_id, step_id, queued_at, started_at, merged_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                mr.id.as_str(),
                mr.task_id.as_str(),
                mr.branch,
                mr.base_branch,
                mr.status.as_str(),
                mr.priority,
                mr.diff_stats.as_ref().map(|v| v.to_string()),
                mr.review_note,
                mr.execution_id.as_ref().map(Id::as_str),
                mr.step_id.as_deref(),
                mr.queued_at.to_rfc3339(),
                mr.started_at.map(|t| t.to_rfc3339()),
                mr.merged_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn update_merge_status(&self, id: &Id, status: MergeStatus) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let merged_at = if status == MergeStatus::Merged {
            Some(now.clone())
        } else {
            None
        };
        let updated = self.conn.execute(
            "UPDATE merge_requests SET status = ?1, started_at = COALESCE(started_at, ?2), merged_at = COALESCE(?3, merged_at) WHERE id = ?4",
            params![status.as_str(), now, merged_at, id.as_str()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound(format!("merge_request {id}")));
        }
        Ok(())
    }

    pub fn get_queued_merge_requests(&self) -> Result<Vec<MergeRequest>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, branch, base_branch, status, priority, diff_stats, review_note, execution_id, step_id, queued_at, started_at, merged_at
             FROM merge_requests
             WHERE status = 'queued'
             ORDER BY priority, queued_at",
        )?;
        let rows = stmt.query_map([], row_to_merge_request)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn get_merge_request(&self, id: &Id) -> Result<MergeRequest> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, branch, base_branch, status, priority, diff_stats, review_note, execution_id, step_id, queued_at, started_at, merged_at
             FROM merge_requests WHERE id = ?1",
        )?;
        stmt.query_row(params![id.as_str()], row_to_merge_request)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    DbError::NotFound(format!("merge_request {id}"))
                }
                other => DbError::Sqlite(other),
            })
    }

    pub fn update_merge_review_note(&self, id: &Id, note: &str) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE merge_requests SET review_note = ?1 WHERE id = ?2",
            params![note, id.as_str()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound(format!("merge_request {id}")));
        }
        Ok(())
    }

    pub fn get_active_merge_requests(&self) -> Result<Vec<MergeRequest>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, branch, base_branch, status, priority, diff_stats, review_note, execution_id, step_id, queued_at, started_at, merged_at
             FROM merge_requests
             WHERE status NOT IN ('merged', 'conflicted', 'failed')
             ORDER BY priority, queued_at",
        )?;
        let rows = stmt.query_map([], row_to_merge_request)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn get_merge_request_for_task(&self, task_id: &Id) -> Result<Option<MergeRequest>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, branch, base_branch, status, priority, diff_stats, review_note, execution_id, step_id, queued_at, started_at, merged_at
             FROM merge_requests WHERE task_id = ?1 ORDER BY queued_at DESC LIMIT 1",
        )?;
        let result = stmt.query_row(params![task_id.as_str()], row_to_merge_request);
        match result {
            Ok(mr) => Ok(Some(mr)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DbError::Sqlite(e)),
        }
    }

    pub fn reset_stuck_merge_requests(&self) -> Result<u32> {
        let updated = self.conn.execute(
            "UPDATE merge_requests SET status = 'queued', started_at = NULL
             WHERE status IN ('processing', 'rebasing', 'verifying')",
            [],
        )?;
        Ok(updated as u32)
    }

    // --- Messages ---

    pub fn insert_message(&self, msg: &Message) -> Result<()> {
        self.conn.execute(
            "INSERT INTO messages (id, from_addr, to_addr, subject, body, priority, msg_type, thread_id, reply_to, read, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                msg.id.as_str(),
                msg.from_addr,
                msg.to_addr,
                msg.subject,
                msg.body,
                msg.priority.as_str(),
                msg.msg_type.as_str(),
                msg.thread_id,
                msg.reply_to,
                msg.read as i32,
                msg.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_messages(&self, to_addr: &str, unread_only: bool) -> Result<Vec<Message>> {
        let sql = if unread_only {
            "SELECT id, from_addr, to_addr, subject, body, priority, msg_type, thread_id, reply_to, read, created_at
             FROM messages WHERE to_addr = ?1 AND read = 0
             ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC"
        } else {
            "SELECT id, from_addr, to_addr, subject, body, priority, msg_type, thread_id, reply_to, read, created_at
             FROM messages WHERE to_addr = ?1
             ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![to_addr], row_to_message)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    /// List messages for a broadcast address pattern (e.g., "@workers") by querying all messages
    /// sent to that address.
    pub fn list_broadcast_messages(&self, addr: &str, unread_only: bool) -> Result<Vec<Message>> {
        self.list_messages(addr, unread_only)
    }

    pub fn get_message(&self, id: &str) -> Result<Message> {
        self.conn
            .query_row(
                "SELECT id, from_addr, to_addr, subject, body, priority, msg_type, thread_id, reply_to, read, created_at
                 FROM messages WHERE id = ?1",
                params![id],
                row_to_message,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    DbError::NotFound(format!("message {id}"))
                }
                other => DbError::Sqlite(other),
            })
    }

    pub fn mark_message_read(&self, id: &str) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE messages SET read = 1 WHERE id = ?1",
            params![id],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound(format!("message {id}")));
        }
        Ok(())
    }

    pub fn count_unread(&self, to_addr: &str) -> Result<(i64, i64)> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE to_addr = ?1 AND read = 0",
            params![to_addr],
            |row| row.get(0),
        )?;
        let urgent: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE to_addr = ?1 AND read = 0 AND priority IN ('urgent', 'high')",
            params![to_addr],
            |row| row.get(0),
        )?;
        Ok((total, urgent))
    }

    pub fn list_thread(&self, thread_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_addr, to_addr, subject, body, priority, msg_type, thread_id, reply_to, read, created_at
             FROM messages WHERE thread_id = ?1
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], row_to_message)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }
}

// --- Row mapping functions ---

fn parse_dt(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_default()
}

fn parse_opt_dt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.map(parse_dt)
}

fn row_to_task(row: &Row) -> rusqlite::Result<Task> {
    let status_str: String = row.get(3)?;
    let tier_str: Option<String> = row.get(7)?;
    Ok(Task {
        id: Id(row.get(0)?),
        title: row.get(1)?,
        description: row.get(2)?,
        status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::Open),
        assigned_to: row.get::<_, Option<String>>(4)?.map(Id),
        worktree: row.get(5)?,
        branch: row.get(6)?,
        tier: tier_str.and_then(|s| Tier::from_str(&s)),
        current_activity: row.get(8)?,
        created_at: parse_dt(row.get(9)?),
        updated_at: parse_dt(row.get(10)?),
    })
}

fn row_to_agent(row: &Row) -> rusqlite::Result<Agent> {
    let status_str: String = row.get(3)?;
    Ok(Agent {
        id: Id(row.get(0)?),
        acp_session: row.get(1)?,
        pid: row.get(2)?,
        status: AgentStatus::from_str(&status_str).unwrap_or(AgentStatus::Idle),
        current_task: row.get::<_, Option<String>>(4)?.map(Id),
        started_at: parse_dt(row.get(5)?),
        last_seen: parse_opt_dt(row.get(6)?),
    })
}

fn row_to_execution(row: &Row) -> rusqlite::Result<Execution> {
    let status_str: String = row.get(1)?;
    Ok(Execution {
        id: Id(row.get(0)?),
        status: ExecutionStatus::from_str(&status_str).unwrap_or(ExecutionStatus::Running),
        created_at: parse_dt(row.get(2)?),
    })
}

fn row_to_merge_request(row: &Row) -> rusqlite::Result<MergeRequest> {
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

fn row_to_message(row: &Row) -> rusqlite::Result<Message> {
    let priority_str: String = row.get(5)?;
    let msg_type_str: String = row.get(6)?;
    let read_int: i32 = row.get(9)?;
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
        created_at: parse_dt(row.get(10)?),
    })
}

// --- Schema parsing for auto-migration ---

fn parse_schema_columns(schema: &str) -> Vec<(String, Vec<(String, String)>)> {
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

// --- Schema ---

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    description TEXT,
    status      TEXT NOT NULL DEFAULT 'open',
    assigned_to TEXT,
    worktree    TEXT,
    branch      TEXT,
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
    status      TEXT NOT NULL DEFAULT 'running',
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS execution_steps (
    execution_id TEXT NOT NULL REFERENCES executions(id),
    step_id      TEXT NOT NULL,
    task_id      TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (execution_id, step_id)
);

CREATE TABLE IF NOT EXISTS agents (
    id          TEXT PRIMARY KEY,
    acp_session TEXT,
    pid         INTEGER,
    status      TEXT NOT NULL DEFAULT 'idle',
    current_task TEXT REFERENCES tasks(id),
    started_at  TEXT NOT NULL,
    last_seen   TEXT
);

CREATE TABLE IF NOT EXISTS merge_requests (
    id          TEXT PRIMARY KEY,
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
    created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);
CREATE INDEX IF NOT EXISTS idx_merge_requests_status ON merge_requests(status);
CREATE INDEX IF NOT EXISTS idx_messages_to_addr ON messages(to_addr);
CREATE INDEX IF NOT EXISTS idx_messages_unread ON messages(to_addr, read);
CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(thread_id);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Db {
        Db::open_in_memory().expect("failed to create test db")
    }

    fn make_task(title: &str) -> Task {
        let now = Utc::now();
        Task {
            id: Id::new("task"),
            title: title.into(),
            description: None,
            status: TaskStatus::Open,
            assigned_to: None,
            worktree: None,
            branch: None,
            tier: None,
            current_activity: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn task_crud() {
        let db = test_db();

        let mut task = make_task("Implement auth");
        task.description = Some("Add JWT auth middleware".into());
        task.tier = Some(Tier::Standard);
        db.insert_task(&task).unwrap();

        let loaded = db.get_task(&task.id).unwrap();
        assert_eq!(loaded.title, "Implement auth");
        assert_eq!(loaded.status, TaskStatus::Open);
        assert_eq!(loaded.tier, Some(Tier::Standard));

        db.update_task_status(&task.id, TaskStatus::Running).unwrap();
        let loaded = db.get_task(&task.id).unwrap();
        assert_eq!(loaded.status, TaskStatus::Running);

        let tasks = db.list_tasks().unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn task_dependencies() {
        let db = test_db();

        let mut design = make_task("Design");
        design.status = TaskStatus::Done;
        let implement = make_task("Implement");
        db.insert_task(&design).unwrap();
        db.insert_task(&implement).unwrap();

        db.insert_dependency(&implement.id, &design.id).unwrap();

        let deps = db.get_dependencies(&implement.id).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].0, design.id.0);

        let dependents = db.get_dependents(&design.id).unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].0, implement.id.0);
    }

    #[test]
    fn agent_crud() {
        let db = test_db();

        let agent = Agent {
            id: Id::new("agent"),
            acp_session: Some("sess_abc123".into()),
            pid: Some(12345),
            status: AgentStatus::Idle,
            current_task: None,
            started_at: Utc::now(),
            last_seen: None,
        };
        db.insert_agent(&agent).unwrap();

        let loaded = db.get_agent(&agent.id).unwrap();
        assert_eq!(loaded.pid, Some(12345));

        db.update_agent_status(&agent.id, AgentStatus::Busy).unwrap();
        let loaded = db.get_agent(&agent.id).unwrap();
        assert_eq!(loaded.status, AgentStatus::Busy);
    }

    #[test]
    fn auto_migrate_adds_missing_columns() {
        let conn = Connection::open_in_memory().unwrap();

        conn.execute_batch(
            "CREATE TABLE tasks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                description TEXT,
                status TEXT NOT NULL DEFAULT 'open',
                assigned_to TEXT,
                worktree TEXT,
                branch TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );"
        ).unwrap();

        let db = Db { conn };
        db.init_schema().unwrap();

        let mut stmt = db.conn.prepare("PRAGMA table_info(tasks)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(cols.contains(&"tier".to_string()), "tier column should be auto-added, got: {:?}", cols);
    }

    #[test]
    fn parse_schema_columns_works() {
        let parsed = parse_schema_columns(SCHEMA);
        let tasks = parsed.iter().find(|(name, _)| name == "tasks").expect("tasks table not found");
        let col_names: Vec<&str> = tasks.1.iter().map(|(n, _)| n.as_str()).collect();
        assert!(col_names.contains(&"id"));
        assert!(col_names.contains(&"title"));
        assert!(col_names.contains(&"tier"));
        assert!(col_names.contains(&"updated_at"));
    }
}

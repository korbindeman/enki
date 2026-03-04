mod schema;

use chrono::Utc;
use rusqlite::{Connection, params};

use crate::types::*;
use schema::*;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("ambiguous short id '{0}': matches {1}")]
    Ambiguous(String, String),
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
        self.conn.execute_batch(INDEXES)?;
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
            "INSERT INTO tasks (id, session_id, title, description, status, assigned_to, copy_path, branch, tier, current_activity, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                task.id.as_str(),
                task.session_id,
                task.title,
                task.description,
                task.status.as_str(),
                task.assigned_to.as_ref().map(Id::as_str),
                task.copy_path,
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
                "SELECT id, session_id, title, description, status, assigned_to, copy_path, branch, base_branch, tier, current_activity, created_at, updated_at
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

    /// Resolve a short ID prefix to a full task ID.
    /// Accepts full IDs, "task-" prefixed shorts, or bare hex prefixes.
    pub fn resolve_task_id(&self, input: &str, session_id: Option<&str>) -> Result<Id> {
        // If it looks like a full ID, try direct lookup first.
        if input.starts_with("task-") && input.len() > 12 {
            return Ok(Id(input.to_string()));
        }

        // Normalize: strip "task-" prefix if present to get the hex prefix.
        let hex_prefix = input.strip_prefix("task-").unwrap_or(input);
        let pattern = format!("task-{hex_prefix}%");

        let query = if session_id.is_some() {
            "SELECT id FROM tasks WHERE id LIKE ?1 AND session_id = ?2"
        } else {
            "SELECT id FROM tasks WHERE id LIKE ?1"
        };

        let mut stmt = self.conn.prepare(query)?;
        let rows: Vec<String> = if let Some(sid) = session_id {
            stmt.query_map(params![pattern, sid], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![pattern], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        match rows.len() {
            0 => Err(DbError::NotFound(format!("task matching '{input}'"))),
            1 => Ok(Id(rows.into_iter().next().unwrap())),
            _ => {
                let shorts: Vec<&str> = rows.iter().map(|id| short_id(id)).collect();
                Err(DbError::Ambiguous(input.to_string(), shorts.join(", ")))
            }
        }
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, title, description, status, assigned_to, copy_path, branch, base_branch, tier, current_activity, created_at, updated_at
             FROM tasks ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn list_session_tasks(&self, session_id: &str) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, title, description, status, assigned_to, copy_path, branch, base_branch, tier, current_activity, created_at, updated_at
             FROM tasks WHERE session_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![session_id], row_to_task)?;
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

    pub fn assign_task(&self, task_id: &Id, agent_id: &Id, copy_path: &str, branch: &str, base_branch: &str) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE tasks SET assigned_to = ?1, copy_path = ?2, branch = ?3, base_branch = ?4, status = 'running', updated_at = ?5 WHERE id = ?6",
            params![
                agent_id.as_str(),
                copy_path,
                branch,
                base_branch,
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

    // --- Sessions ---

    pub fn insert_session(&self, session: &Session) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (id, started_at) VALUES (?1, ?2)",
            params![session.id.as_str(), session.started_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn end_session(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    pub fn get_latest_session(&self) -> Result<Option<Session>> {
        let result = self.conn.query_row(
            "SELECT id, started_at, ended_at FROM sessions ORDER BY started_at DESC LIMIT 1",
            [],
            row_to_session,
        );
        match result {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DbError::Sqlite(e)),
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, ended_at FROM sessions ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    /// Mark all in-flight tasks for a session as abandoned.
    pub fn abandon_session_tasks(&self, session_id: &str) -> Result<u32> {
        let updated = self.conn.execute(
            "UPDATE tasks SET status = 'abandoned', updated_at = ?1
             WHERE session_id = ?2 AND status IN ('pending', 'running', 'paused')",
            params![Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(updated as u32)
    }

    /// Mark all in-flight merge requests for a session as failed.
    pub fn abandon_session_merges(&self, session_id: &str) -> Result<u32> {
        let updated = self.conn.execute(
            "UPDATE merge_requests SET status = 'failed'
             WHERE session_id = ?1 AND status IN ('queued', 'processing', 'rebasing', 'verifying')",
            params![session_id],
        )?;
        Ok(updated as u32)
    }

    // --- Executions ---

    pub fn insert_execution(&self, exec: &Execution) -> Result<()> {
        self.conn.execute(
            "INSERT INTO executions (id, session_id, status, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                exec.id.as_str(),
                exec.session_id,
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

    pub fn save_dag(&self, execution_id: &Id, dag: &crate::dag::Dag) -> Result<()> {
        let json = serde_json::to_string(dag)
            .map_err(|e| DbError::InvalidData(format!("serialize dag: {e}")))?;
        self.conn.execute(
            "UPDATE executions SET dag = ?1 WHERE id = ?2",
            params![json, execution_id.as_str()],
        )?;
        Ok(())
    }

    pub fn load_dag(&self, execution_id: &Id) -> Result<Option<crate::dag::Dag>> {
        let mut stmt = self.conn.prepare(
            "SELECT dag FROM executions WHERE id = ?1",
        )?;
        let json: Option<String> = stmt.query_row(params![execution_id.as_str()], |row| row.get(0))
            .map_err(DbError::Sqlite)?;
        match json {
            Some(s) => {
                let dag: crate::dag::Dag = serde_json::from_str(&s)
                    .map_err(|e| DbError::InvalidData(format!("deserialize dag: {e}")))?;
                Ok(Some(dag))
            }
            None => Ok(None),
        }
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

    pub fn get_running_executions(&self, session_id: Option<&str>) -> Result<Vec<Execution>> {
        if let Some(sid) = session_id {
            let mut stmt = self.conn.prepare(
                "SELECT id, session_id, status, created_at
                 FROM executions WHERE status = 'running' AND session_id = ?1",
            )?;
            let rows = stmt.query_map(params![sid], row_to_execution)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(DbError::Sqlite)
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, session_id, status, created_at
                 FROM executions WHERE status = 'running'",
            )?;
            let rows = stmt.query_map([], row_to_execution)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(DbError::Sqlite)
        }
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

    pub fn get_orphan_ready_tasks(&self, session_id: &str) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.session_id, t.title, t.description, t.status, t.assigned_to, t.copy_path, t.branch, t.base_branch, t.tier, t.current_activity, t.created_at, t.updated_at
             FROM tasks t
             WHERE t.status = 'pending'
             AND t.session_id = ?1
             AND t.id NOT IN (SELECT es.task_id FROM execution_steps es)"
        )?;
        let tasks = stmt.query_map(params![session_id], row_to_task)?
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
            session_id: None,
            title: title.into(),
            description: None,
            status: TaskStatus::Pending,
            assigned_to: None,
            copy_path: None,
            branch: None,
            base_branch: None,
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
        assert_eq!(loaded.status, TaskStatus::Pending);
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
    fn auto_migrate_adds_missing_columns() {
        let conn = Connection::open_in_memory().unwrap();

        conn.execute_batch(
            "CREATE TABLE tasks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                description TEXT,
                status TEXT NOT NULL DEFAULT 'open',
                assigned_to TEXT,
                copy_path TEXT,
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

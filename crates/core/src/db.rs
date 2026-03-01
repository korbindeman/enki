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
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // --- Projects ---

    pub fn insert_project(&self, project: &Project) -> Result<()> {
        self.conn.execute(
            "INSERT INTO projects (id, name, repo_url, local_path, bare_repo, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                project.id.as_str(),
                project.name,
                project.repo_url,
                project.local_path,
                project.bare_repo,
                project.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_project(&self, id: &Id) -> Result<Project> {
        self.conn
            .query_row(
                "SELECT id, name, repo_url, local_path, bare_repo, created_at FROM projects WHERE id = ?1",
                params![id.as_str()],
                row_to_project,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    DbError::NotFound(format!("project {id}"))
                }
                other => DbError::Sqlite(other),
            })
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, repo_url, local_path, bare_repo, created_at FROM projects ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_project)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    // --- Tasks ---

    pub fn insert_task(&self, task: &Task) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tasks (id, project_id, title, description, status, assigned_to, worktree, branch, tier, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                task.id.as_str(),
                task.project_id.as_str(),
                task.title,
                task.description,
                task.status.as_str(),
                task.assigned_to.as_ref().map(Id::as_str),
                task.worktree,
                task.branch,
                task.tier.map(|t| t.as_str()),
                task.created_at.to_rfc3339(),
                task.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &Id) -> Result<Task> {
        self.conn
            .query_row(
                "SELECT id, project_id, title, description, status, assigned_to, worktree, branch, tier, created_at, updated_at
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

    pub fn list_tasks(&self, project_id: &Id) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, title, description, status, assigned_to, worktree, branch, tier, created_at, updated_at
             FROM tasks WHERE project_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![project_id.as_str()], row_to_task)?;
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

    // --- Task Groups ---

    pub fn insert_task_group(&self, group: &TaskGroup) -> Result<()> {
        self.conn.execute(
            "INSERT INTO task_groups (id, name, status, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                group.id.as_str(),
                group.name,
                group.status.as_str(),
                group.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn add_task_to_group(&self, group_id: &Id, task_id: &Id) -> Result<()> {
        self.conn.execute(
            "INSERT INTO task_group_members (group_id, task_id) VALUES (?1, ?2)",
            params![group_id.as_str(), task_id.as_str()],
        )?;
        Ok(())
    }

    pub fn get_task_group(&self, id: &Id) -> Result<TaskGroup> {
        self.conn
            .query_row(
                "SELECT id, name, status, created_at FROM task_groups WHERE id = ?1",
                params![id.as_str()],
                row_to_task_group,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    DbError::NotFound(format!("task_group {id}"))
                }
                other => DbError::Sqlite(other),
            })
    }

    pub fn list_tasks_in_group(&self, group_id: &Id) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.project_id, t.title, t.description, t.status, t.assigned_to, t.worktree, t.branch, t.tier, t.created_at, t.updated_at
             FROM tasks t
             JOIN task_group_members tgm ON t.id = tgm.task_id
             WHERE tgm.group_id = ?1
             ORDER BY t.created_at",
        )?;
        let rows = stmt.query_map(params![group_id.as_str()], row_to_task)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
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
            "INSERT INTO agents (id, role, project_id, acp_session, pid, status, current_task, started_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                agent.id.as_str(),
                agent.role.as_str(),
                agent.project_id.as_ref().map(Id::as_str),
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
                "SELECT id, role, project_id, acp_session, pid, status, current_task, started_at, last_seen
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

    pub fn list_agents(&self, project_id: Option<&Id>) -> Result<Vec<Agent>> {
        match project_id {
            Some(pid) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, role, project_id, acp_session, pid, status, current_task, started_at, last_seen
                     FROM agents WHERE project_id = ?1 ORDER BY started_at",
                )?;
                let rows = stmt.query_map(params![pid.as_str()], row_to_agent)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DbError::Sqlite)
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, role, project_id, acp_session, pid, status, current_task, started_at, last_seen
                     FROM agents ORDER BY started_at",
                )?;
                let rows = stmt.query_map([], row_to_agent)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(DbError::Sqlite)
            }
        }
    }

    // --- Messages ---

    pub fn insert_message(&self, msg: &Message) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (type, from_agent, to_agent, task_id, payload, read, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                msg.msg_type.as_str(),
                msg.from_agent.as_ref().map(Id::as_str),
                msg.to_agent.as_ref().map(Id::as_str),
                msg.task_id.as_ref().map(Id::as_str),
                msg.payload.to_string(),
                msg.read as i32,
                msg.created_at.to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_unread_messages(&self, agent_id: &Id) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, type, from_agent, to_agent, task_id, payload, read, created_at
             FROM messages
             WHERE (to_agent = ?1 OR to_agent IS NULL) AND read = 0
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![agent_id.as_str()], row_to_message)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    pub fn mark_message_read(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE messages SET read = 1 WHERE id = ?1", params![id])?;
        Ok(())
    }

    // --- Executions ---

    pub fn insert_execution(&self, exec: &Execution) -> Result<()> {
        self.conn.execute(
            "INSERT INTO executions (id, template, group_id, status, vars, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                exec.id.as_str(),
                exec.template,
                exec.group_id.as_ref().map(Id::as_str),
                exec.status.as_str(),
                exec.vars.as_ref().map(|v| v.to_string()),
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

    // --- Merge Requests ---

    pub fn insert_merge_request(&self, mr: &MergeRequest) -> Result<()> {
        self.conn.execute(
            "INSERT INTO merge_requests (id, project_id, task_id, group_id, branch, base_branch, status, priority, diff_stats, review_note, queued_at, started_at, merged_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                mr.id.as_str(),
                mr.project_id.as_str(),
                mr.task_id.as_str(),
                mr.group_id.as_ref().map(Id::as_str),
                mr.branch,
                mr.base_branch,
                mr.status.as_str(),
                mr.priority,
                mr.diff_stats.as_ref().map(|v| v.to_string()),
                mr.review_note,
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

    pub fn get_queued_merge_requests(&self, project_id: &Id) -> Result<Vec<MergeRequest>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, task_id, group_id, branch, base_branch, status, priority, diff_stats, review_note, queued_at, started_at, merged_at
             FROM merge_requests
             WHERE project_id = ?1 AND status = 'queued'
             ORDER BY priority, queued_at",
        )?;
        let rows = stmt.query_map(params![project_id.as_str()], row_to_merge_request)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::Sqlite)
    }

    // --- Usage ---

    pub fn insert_usage(&self, usage: &Usage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO usage (agent_id, task_id, model, input_tokens, output_tokens, duration_ms, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                usage.agent_id.as_ref().map(Id::as_str),
                usage.task_id.as_ref().map(Id::as_str),
                usage.model,
                usage.input_tokens,
                usage.output_tokens,
                usage.duration_ms,
                usage.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
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

fn row_to_project(row: &Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: Id(row.get(0)?),
        name: row.get(1)?,
        repo_url: row.get(2)?,
        local_path: row.get(3)?,
        bare_repo: row.get(4)?,
        created_at: parse_dt(row.get(5)?),
    })
}

fn row_to_task(row: &Row) -> rusqlite::Result<Task> {
    let status_str: String = row.get(4)?;
    let tier_str: Option<String> = row.get(8)?;
    Ok(Task {
        id: Id(row.get(0)?),
        project_id: Id(row.get(1)?),
        title: row.get(2)?,
        description: row.get(3)?,
        status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::Open),
        assigned_to: row.get::<_, Option<String>>(5)?.map(Id),
        worktree: row.get(6)?,
        branch: row.get(7)?,
        tier: tier_str.and_then(|s| Tier::from_str(&s)),
        created_at: parse_dt(row.get(9)?),
        updated_at: parse_dt(row.get(10)?),
    })
}

fn row_to_task_group(row: &Row) -> rusqlite::Result<TaskGroup> {
    let status_str: String = row.get(2)?;
    Ok(TaskGroup {
        id: Id(row.get(0)?),
        name: row.get(1)?,
        status: TaskGroupStatus::from_str(&status_str).unwrap_or(TaskGroupStatus::Active),
        created_at: parse_dt(row.get(3)?),
    })
}

fn row_to_agent(row: &Row) -> rusqlite::Result<Agent> {
    let role_str: String = row.get(1)?;
    let status_str: String = row.get(5)?;
    Ok(Agent {
        id: Id(row.get(0)?),
        role: AgentRole::from_str(&role_str).unwrap_or(AgentRole::Worker),
        project_id: row.get::<_, Option<String>>(2)?.map(Id),
        acp_session: row.get(3)?,
        pid: row.get(4)?,
        status: AgentStatus::from_str(&status_str).unwrap_or(AgentStatus::Idle),
        current_task: row.get::<_, Option<String>>(6)?.map(Id),
        started_at: parse_dt(row.get(7)?),
        last_seen: parse_opt_dt(row.get(8)?),
    })
}

fn row_to_message(row: &Row) -> rusqlite::Result<Message> {
    let type_str: String = row.get(1)?;
    let payload_str: String = row.get(5)?;
    let read_int: i32 = row.get(6)?;
    Ok(Message {
        id: row.get(0)?,
        msg_type: MessageType::from_str(&type_str).unwrap_or(MessageType::Info),
        from_agent: row.get::<_, Option<String>>(2)?.map(Id),
        to_agent: row.get::<_, Option<String>>(3)?.map(Id),
        task_id: row.get::<_, Option<String>>(4)?.map(Id),
        payload: serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null),
        read: read_int != 0,
        created_at: parse_dt(row.get(7)?),
    })
}

fn row_to_merge_request(row: &Row) -> rusqlite::Result<MergeRequest> {
    let status_str: String = row.get(6)?;
    let diff_stats_str: Option<String> = row.get(8)?;
    Ok(MergeRequest {
        id: Id(row.get(0)?),
        project_id: Id(row.get(1)?),
        task_id: Id(row.get(2)?),
        group_id: row.get::<_, Option<String>>(3)?.map(Id),
        branch: row.get(4)?,
        base_branch: row.get(5)?,
        status: MergeStatus::from_str(&status_str).unwrap_or(MergeStatus::Queued),
        priority: row.get(7)?,
        diff_stats: diff_stats_str.and_then(|s| serde_json::from_str(&s).ok()),
        review_note: row.get(9)?,
        queued_at: parse_dt(row.get(10)?),
        started_at: parse_opt_dt(row.get(11)?),
        merged_at: parse_opt_dt(row.get(12)?),
    })
}

// --- Schema ---

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    repo_url    TEXT,
    local_path  TEXT NOT NULL,
    bare_repo   TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    title       TEXT NOT NULL,
    description TEXT,
    status      TEXT NOT NULL DEFAULT 'open',
    assigned_to TEXT,
    worktree    TEXT,
    branch      TEXT,
    tier        TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS task_groups (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'active',
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS task_group_members (
    group_id    TEXT NOT NULL REFERENCES task_groups(id),
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (group_id, task_id)
);

CREATE TABLE IF NOT EXISTS task_dependencies (
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    depends_on  TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, depends_on)
);

CREATE TABLE IF NOT EXISTS executions (
    id          TEXT PRIMARY KEY,
    template    TEXT NOT NULL,
    group_id    TEXT REFERENCES task_groups(id),
    status      TEXT NOT NULL DEFAULT 'running',
    vars        TEXT,
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
    role        TEXT NOT NULL,
    project_id  TEXT REFERENCES projects(id),
    acp_session TEXT,
    pid         INTEGER,
    status      TEXT NOT NULL DEFAULT 'idle',
    current_task TEXT REFERENCES tasks(id),
    started_at  TEXT NOT NULL,
    last_seen   TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    type        TEXT NOT NULL,
    from_agent  TEXT REFERENCES agents(id),
    to_agent    TEXT,
    task_id     TEXT REFERENCES tasks(id),
    payload     TEXT NOT NULL,
    read        INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS usage (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id    TEXT REFERENCES agents(id),
    task_id     TEXT REFERENCES tasks(id),
    model       TEXT NOT NULL,
    input_tokens  INTEGER,
    output_tokens INTEGER,
    duration_ms   INTEGER,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS merge_requests (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    group_id    TEXT REFERENCES task_groups(id),
    branch      TEXT NOT NULL,
    base_branch TEXT NOT NULL DEFAULT 'main',
    status      TEXT NOT NULL DEFAULT 'queued',
    priority    INTEGER NOT NULL DEFAULT 2,
    diff_stats  TEXT,
    review_note TEXT,
    queued_at   TEXT NOT NULL,
    started_at  TEXT,
    merged_at   TEXT
);

CREATE TABLE IF NOT EXISTS memories (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    scope       TEXT NOT NULL,
    project_id  TEXT REFERENCES projects(id),
    content     TEXT NOT NULL,
    tags        TEXT,
    source_task TEXT REFERENCES tasks(id),
    created_at  TEXT NOT NULL,
    accessed_at TEXT
);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    content,
    tags,
    content='memories',
    content_rowid='id'
);

-- Triggers to keep FTS in sync
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content, tags) VALUES (new.id, new.content, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, tags) VALUES('delete', old.id, old.content, old.tags);
END;

-- Indexes
CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project_id);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_agents_project ON agents(project_id);
CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);
CREATE INDEX IF NOT EXISTS idx_messages_to ON messages(to_agent, read);
CREATE INDEX IF NOT EXISTS idx_merge_requests_project ON merge_requests(project_id, status);
CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope, project_id);
";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_db() -> Db {
        Db::open_in_memory().expect("failed to create test db")
    }

    fn make_project() -> Project {
        Project {
            id: Id::new("proj"),
            name: "test-project".into(),
            repo_url: Some("https://github.com/test/test".into()),
            local_path: "/tmp/test".into(),
            bare_repo: "/tmp/test/.repo.git".into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn project_roundtrip() {
        let db = test_db();
        let project = make_project();
        db.insert_project(&project).unwrap();
        let loaded = db.get_project(&project.id).unwrap();
        assert_eq!(loaded.name, "test-project");
        assert_eq!(loaded.local_path, "/tmp/test");

        let all = db.list_projects().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn task_crud() {
        let db = test_db();
        let project = make_project();
        db.insert_project(&project).unwrap();

        let now = Utc::now();
        let task = Task {
            id: Id::new("task"),
            project_id: project.id.clone(),
            title: "Implement auth".into(),
            description: Some("Add JWT auth middleware".into()),
            status: TaskStatus::Open,
            assigned_to: None,
            worktree: None,
            branch: None,
            tier: Some(Tier::Standard),
            created_at: now,
            updated_at: now,
        };
        db.insert_task(&task).unwrap();

        let loaded = db.get_task(&task.id).unwrap();
        assert_eq!(loaded.title, "Implement auth");
        assert_eq!(loaded.status, TaskStatus::Open);
        assert_eq!(loaded.tier, Some(Tier::Standard));

        db.update_task_status(&task.id, TaskStatus::Running).unwrap();
        let loaded = db.get_task(&task.id).unwrap();
        assert_eq!(loaded.status, TaskStatus::Running);

        let tasks = db.list_tasks(&project.id).unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn task_group_and_members() {
        let db = test_db();
        let project = make_project();
        db.insert_project(&project).unwrap();

        let group = TaskGroup {
            id: Id::new("grp"),
            name: "Auth System".into(),
            status: TaskGroupStatus::Active,
            created_at: Utc::now(),
        };
        db.insert_task_group(&group).unwrap();

        let now = Utc::now();
        let task1 = Task {
            id: Id::new("task"),
            project_id: project.id.clone(),
            title: "Design".into(),
            description: None,
            status: TaskStatus::Open,
            assigned_to: None,
            worktree: None,
            branch: None,
            tier: None,
            created_at: now,
            updated_at: now,
        };
        let task2 = Task {
            id: Id::new("task"),
            project_id: project.id.clone(),
            title: "Implement".into(),
            description: None,
            status: TaskStatus::Open,
            assigned_to: None,
            worktree: None,
            branch: None,
            tier: None,
            created_at: now,
            updated_at: now,
        };
        db.insert_task(&task1).unwrap();
        db.insert_task(&task2).unwrap();
        db.add_task_to_group(&group.id, &task1.id).unwrap();
        db.add_task_to_group(&group.id, &task2.id).unwrap();

        let members = db.list_tasks_in_group(&group.id).unwrap();
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn task_dependencies() {
        let db = test_db();
        let project = make_project();
        db.insert_project(&project).unwrap();

        let now = Utc::now();
        let design = Task {
            id: Id::new("task"),
            project_id: project.id.clone(),
            title: "Design".into(),
            description: None,
            status: TaskStatus::Done,
            assigned_to: None,
            worktree: None,
            branch: None,
            tier: None,
            created_at: now,
            updated_at: now,
        };
        let implement = Task {
            id: Id::new("task"),
            project_id: project.id.clone(),
            title: "Implement".into(),
            description: None,
            status: TaskStatus::Open,
            assigned_to: None,
            worktree: None,
            branch: None,
            tier: None,
            created_at: now,
            updated_at: now,
        };
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
        let project = make_project();
        db.insert_project(&project).unwrap();

        let agent = Agent {
            id: Id::new("agent"),
            role: AgentRole::Worker,
            project_id: Some(project.id.clone()),
            acp_session: Some("sess_abc123".into()),
            pid: Some(12345),
            status: AgentStatus::Idle,
            current_task: None,
            started_at: Utc::now(),
            last_seen: None,
        };
        db.insert_agent(&agent).unwrap();

        let loaded = db.get_agent(&agent.id).unwrap();
        assert_eq!(loaded.role, AgentRole::Worker);
        assert_eq!(loaded.pid, Some(12345));

        db.update_agent_status(&agent.id, AgentStatus::Busy).unwrap();
        let loaded = db.get_agent(&agent.id).unwrap();
        assert_eq!(loaded.status, AgentStatus::Busy);
    }

    #[test]
    fn message_roundtrip() {
        let db = test_db();
        let project = make_project();
        db.insert_project(&project).unwrap();

        let agent = Agent {
            id: Id::new("agent"),
            role: AgentRole::Coordinator,
            project_id: Some(project.id.clone()),
            acp_session: None,
            pid: None,
            status: AgentStatus::Idle,
            current_task: None,
            started_at: Utc::now(),
            last_seen: None,
        };
        db.insert_agent(&agent).unwrap();

        let worker = Agent {
            id: Id::new("agent"),
            role: AgentRole::Worker,
            project_id: Some(project.id.clone()),
            acp_session: None,
            pid: None,
            status: AgentStatus::Idle,
            current_task: None,
            started_at: Utc::now(),
            last_seen: None,
        };
        db.insert_agent(&worker).unwrap();

        let msg = Message {
            id: 0, // auto-increment
            msg_type: MessageType::StatusUpdate,
            from_agent: Some(worker.id.clone()),
            to_agent: Some(agent.id.clone()),
            task_id: None,
            payload: json!({"progress": 50, "summary": "halfway done"}),
            read: false,
            created_at: Utc::now(),
        };
        db.insert_message(&msg).unwrap();

        let unread = db.get_unread_messages(&agent.id).unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].msg_type, MessageType::StatusUpdate);

        db.mark_message_read(unread[0].id).unwrap();
        let unread = db.get_unread_messages(&agent.id).unwrap();
        assert_eq!(unread.len(), 0);
    }

    #[test]
    fn project_not_found() {
        let db = test_db();
        let result = db.get_project(&Id("nonexistent".into()));
        assert!(matches!(result, Err(DbError::NotFound(_))));
    }
}

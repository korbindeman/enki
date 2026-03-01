use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// --- ID type ---

/// Wrapper around ULID strings used as primary keys.
/// Format: "prefix-01JXXXXXXXXXXXXXXXXXXXXXXXXX"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(pub String);

impl Id {
    pub fn new(prefix: &str) -> Self {
        let ulid = ulid::Ulid::new();
        Self(format!("{prefix}-{ulid}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// --- Enums ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    Ready,
    Running,
    Done,
    Failed,
    Blocked,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "ready" => Some(Self::Ready),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGroupStatus {
    Active,
    Landed,
    Aborted,
}

impl TaskGroupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Landed => "landed",
            Self::Aborted => "aborted",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "landed" => Some(Self::Landed),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Coordinator,
    Worker,
    Monitor,
    Merger,
}

impl AgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Worker => "worker",
            Self::Monitor => "monitor",
            Self::Merger => "merger",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "coordinator" => Some(Self::Coordinator),
            "worker" => Some(Self::Worker),
            "monitor" => Some(Self::Monitor),
            "merger" => Some(Self::Merger),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Busy,
    Stuck,
    Dead,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Stuck => "stuck",
            Self::Dead => "dead",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "busy" => Some(Self::Busy),
            "stuck" => Some(Self::Stuck),
            "dead" => Some(Self::Dead),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    StatusUpdate,
    Escalation,
    Handoff,
    Review,
    Info,
}

impl MessageType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StatusUpdate => "status_update",
            Self::Escalation => "escalation",
            Self::Handoff => "handoff",
            Self::Review => "review",
            Self::Info => "info",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "status_update" => Some(Self::StatusUpdate),
            "escalation" => Some(Self::Escalation),
            "handoff" => Some(Self::Handoff),
            "review" => Some(Self::Review),
            "info" => Some(Self::Info),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Light,
    Standard,
    Heavy,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Standard => "standard",
            Self::Heavy => "heavy",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "light" => Some(Self::Light),
            "standard" => Some(Self::Standard),
            "heavy" => Some(Self::Heavy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    Queued,
    Processing,
    Rebasing,
    Verifying,
    Reviewing,
    Merged,
    Conflicted,
    Failed,
    NeedsChanges,
}

impl MergeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Rebasing => "rebasing",
            Self::Verifying => "verifying",
            Self::Reviewing => "reviewing",
            Self::Merged => "merged",
            Self::Conflicted => "conflicted",
            Self::Failed => "failed",
            Self::NeedsChanges => "needs_changes",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "processing" => Some(Self::Processing),
            "rebasing" => Some(Self::Rebasing),
            "verifying" => Some(Self::Verifying),
            "reviewing" => Some(Self::Reviewing),
            "merged" => Some(Self::Merged),
            "conflicted" => Some(Self::Conflicted),
            "failed" => Some(Self::Failed),
            "needs_changes" => Some(Self::NeedsChanges),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Running,
    Done,
    Failed,
    Aborted,
}

impl ExecutionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Global,
    Project,
    Task,
}

impl MemoryScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Task => "task",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "global" => Some(Self::Global),
            "project" => Some(Self::Project),
            "task" => Some(Self::Task),
            _ => None,
        }
    }
}

// --- Domain structs ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Id,
    pub name: String,
    pub repo_url: Option<String>,
    pub local_path: String,
    pub bare_repo: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Id,
    pub project_id: Id,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub assigned_to: Option<Id>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub tier: Option<Tier>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGroup {
    pub id: Id,
    pub name: String,
    pub status: TaskGroupStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: Id,
    pub role: AgentRole,
    pub project_id: Option<Id>,
    pub acp_session: Option<String>,
    pub pid: Option<u32>,
    pub status: AgentStatus,
    pub current_task: Option<Id>,
    pub started_at: DateTime<Utc>,
    pub last_seen: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub msg_type: MessageType,
    pub from_agent: Option<Id>,
    pub to_agent: Option<Id>,
    pub task_id: Option<Id>,
    pub payload: serde_json::Value,
    pub read: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: Id,
    pub template: String,
    pub group_id: Option<Id>,
    pub status: ExecutionStatus,
    pub vars: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRequest {
    pub id: Id,
    pub project_id: Id,
    pub task_id: Id,
    pub group_id: Option<Id>,
    pub branch: String,
    pub base_branch: String,
    pub status: MergeStatus,
    pub priority: i32,
    pub diff_stats: Option<serde_json::Value>,
    pub review_note: Option<String>,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub merged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub id: i64,
    pub agent_id: Option<Id>,
    pub task_id: Option<Id>,
    pub model: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: i64,
    pub scope: MemoryScope,
    pub project_id: Option<Id>,
    pub content: String,
    pub tags: Option<serde_json::Value>,
    pub source_task: Option<Id>,
    pub created_at: DateTime<Utc>,
    pub accessed_at: Option<DateTime<Utc>>,
}

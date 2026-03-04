use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};

// --- ID type ---

/// Wrapper around random hex strings used as primary keys.
/// Format: "prefix-a1b2c3d4" (8 lowercase hex chars)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Id(pub String);

impl Id {
    pub fn new(prefix: &str) -> Self {
        let bytes: [u8; 4] = rand::rng().random();
        let hex = format!("{:08x}", u32::from_be_bytes(bytes));
        Self(format!("{prefix}-{hex}"))
    }

    /// Short display form (first 4 hex chars after prefix), like git short refs.
    pub fn short(&self) -> &str {
        short_id(&self.0)
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

/// Extract short display form from an ID string: "task-a1b2c3d4" → "a1b2"
pub fn short_id(id: &str) -> &str {
    if let Some(hex) = id.split('-').last() {
        &hex[..4.min(hex.len())]
    } else {
        id
    }
}

// --- Enums ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    Blocked,
    Paused,
    Cancelled,
    Abandoned,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            // Back-compat: old DB rows may have "open" or "ready".
            "open" | "ready" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "blocked" => Some(Self::Blocked),
            "paused" => Some(Self::Paused),
            "cancelled" => Some(Self::Cancelled),
            "abandoned" => Some(Self::Abandoned),
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
    Merged,
    Conflicted,
    Failed,
}

impl MergeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Rebasing => "rebasing",
            Self::Verifying => "verifying",
            Self::Merged => "merged",
            Self::Conflicted => "conflicted",
            Self::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "processing" => Some(Self::Processing),
            "rebasing" => Some(Self::Rebasing),
            "verifying" => Some(Self::Verifying),
            "merged" => Some(Self::Merged),
            "conflicted" => Some(Self::Conflicted),
            "failed" => Some(Self::Failed),
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
    Paused,
}

impl ExecutionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::Paused => "paused",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "aborted" => Some(Self::Aborted),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePriority {
    Low,
    Normal,
    High,
    Urgent,
}

impl MessagePriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "normal" => Some(Self::Normal),
            "high" => Some(Self::High),
            "urgent" => Some(Self::Urgent),
            _ => None,
        }
    }

    /// Ordering value for sorting (higher = more important).
    pub fn sort_key(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::High => 2,
            Self::Urgent => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Info,
    Request,
    Response,
    Protocol,
}

impl MessageType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Request => "request",
            Self::Response => "response",
            Self::Protocol => "protocol",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Self::Info),
            "request" => Some(Self::Request),
            "response" => Some(Self::Response),
            "protocol" => Some(Self::Protocol),
            _ => None,
        }
    }
}

// --- Domain structs ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Id,
    pub session_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub assigned_to: Option<Id>,
    pub copy_path: Option<String>,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub tier: Option<Tier>,
    pub current_activity: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Id,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: Id,
    pub session_id: Option<String>,
    pub status: ExecutionStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRequest {
    pub id: Id,
    pub task_id: Id,
    pub branch: String,
    pub base_branch: String,
    pub status: MergeStatus,
    pub priority: i32,
    pub diff_stats: Option<serde_json::Value>,
    pub review_note: Option<String>,
    pub execution_id: Option<Id>,
    pub step_id: Option<String>,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub merged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub from_addr: String,
    pub to_addr: String,
    pub subject: String,
    pub body: String,
    pub priority: MessagePriority,
    pub msg_type: MessageType,
    pub thread_id: Option<String>,
    pub reply_to: Option<String>,
    pub read: bool,
    pub created_at: DateTime<Utc>,
}

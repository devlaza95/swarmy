//! A live session of any harness: what runs now, in which pane, in what state.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::harness::Harness;
use crate::core::ids::{SeatId, SessionId};
use crate::core::pane::Pane;

#[derive(Debug, Clone, Serialize)]
pub struct LiveSession {
    pub seat: SeatId,
    #[serde(default)]
    pub harness: Harness,
    pub pid: u32,
    pub session_id: SessionId,
    pub cwd: String,
    pub name: Option<String>,
    pub status: SessionStatus,
    /// Its tmux pane or kitty window, for `J`.
    pub pane: Option<Pane>,
    pub version: Option<String>,
    pub kind: Option<String>,
    pub started_at: DateTime<Utc>,
    pub status_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SessionStatus {
    Busy,
    Idle,
    /// Blocked on the user: a permission prompt, a question, a dialog…
    Waiting(String),
    Other(String),
}

impl SessionStatus {
    pub fn parse(status: Option<&str>, waiting_for: Option<&str>) -> Self {
        match status.unwrap_or("") {
            "busy" => SessionStatus::Busy,
            "idle" | "" => SessionStatus::Idle,
            "waiting" => SessionStatus::Waiting(waiting_for.unwrap_or("input").to_string()),
            s => SessionStatus::Other(s.to_string()),
        }
    }
}

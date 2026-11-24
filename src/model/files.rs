//! A transcript file of any harness, as the index tracks it, and an agent's side file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::harness::Harness;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FileKind {
    Main,
    Subagent,
    Workflow(String),
}

impl FileKind {
    /// Every tag `tag` returns; the index's `CHECK` on `files.kind` and `agents.kind` is
    /// generated from it.
    pub const TAGS: [&'static str; 3] = ["main", "subagent", "workflow"];

    pub fn tag(&self) -> &'static str {
        match self {
            FileKind::Main => Self::TAGS[0],
            FileKind::Subagent => Self::TAGS[1],
            FileKind::Workflow(_) => Self::TAGS[2],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptFile {
    pub path: PathBuf,
    pub seat: crate::core::ids::SeatId,
    pub session_id: String,
    /// Empty for the main thread.
    pub agent_id: String,
    pub kind: FileKind,
    /// Which harness wrote this file.
    pub harness: Harness,
    /// Session working directory supplied out-of-band (used by harnesses like `agy` whose
    /// transcript lines do not repeat `cwd`).
    pub default_cwd: Option<String>,
    /// Session title supplied out-of-band (e.g. `agy`'s `conversation_summaries.db`).
    pub session_title: Option<String>,
    /// Agent metadata supplied out-of-band (instead of Claude Code's `agent-*.meta.json`).
    pub inline_meta: Option<AgentMeta>,
}

impl TranscriptFile {
    pub fn meta_path(&self) -> Option<PathBuf> {
        (self.harness == Harness::Claude && self.kind != FileKind::Main)
            .then(|| self.path.with_extension("meta.json"))
    }
}

/// `agent-<id>.meta.json`, written by Claude Code when it spawns a subagent.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentMeta {
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub tool_use_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub spawn_depth: Option<u32>,
    pub request_shape: Option<String>,
    /// As requested (`opus`, `fable`, a full id…).
    pub model: Option<String>,
    pub effort: Option<String>,
    pub picked_effort_model: Option<String>,
    pub finished: bool,
}

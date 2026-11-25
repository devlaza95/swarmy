//! A seat: one config dir of a harness (`~/.claude-work`, `~/.gemini/antigravity-cli`,
//! `~/.codex`). Each harness's reader finds its own; the type is theirs in common.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::harness::Harness;
use crate::core::ids::SeatId;

#[derive(Debug, Clone, Serialize)]
pub struct Seat {
    /// The seat's identity, from its canonical dir: what the index and the vault store.
    pub id: SeatId,
    /// Short name for display only: `default` for `~/.claude`, `work` for `~/.claude-work`, else
    /// the dir name, or the `label` of `config.toml`; may change between runs.
    pub label: String,
    pub dir: PathBuf,
    /// `~/.claude.json` for the default Claude dir, `<dir>/.claude.json` for other Claude dirs;
    /// empty for non-Claude harnesses.
    pub claude_json: PathBuf,
    pub is_default: bool,
    pub found_by: Vec<&'static str>,
    /// Which agent harness owns this directory.
    #[serde(default)]
    pub harness: Harness,
}

/// The id of a config dir: [`SeatId::of_canonical_dir`] of its canonical path (the path as
/// given when it cannot be resolved, e.g. a dir that is gone).
pub fn seat_id(dir: &Path) -> SeatId {
    let canon = crate::fsutil::canonical(dir).unwrap_or_else(|_| dir.to_path_buf());
    SeatId::of_canonical_dir(&canon)
}

impl Seat {
    /// Whether `[[seat]]` of `config.toml` lists this dir (the user asked for it: always shown).
    pub fn configured(&self) -> bool {
        self.found_by.contains(&"config.toml")
    }

    /// A Claude Code seat of `dir`, its id derived from the path.
    pub fn new(
        label: impl Into<String>,
        dir: PathBuf,
        claude_json: PathBuf,
        is_default: bool,
    ) -> Seat {
        Seat {
            id: seat_id(&dir),
            label: label.into(),
            dir,
            claude_json,
            is_default,
            found_by: Vec::new(),
            harness: Harness::Claude,
        }
    }

    /// An Antigravity (`agy`) seat of `dir`, its id derived from the path.
    pub fn agy(label: impl Into<String>, dir: PathBuf) -> Seat {
        Seat {
            id: seat_id(&dir),
            label: label.into(),
            dir,
            claude_json: PathBuf::new(),
            is_default: false,
            found_by: Vec::new(),
            harness: Harness::Agy,
        }
    }

    /// A Codex seat of `dir` (a Codex home), its id derived from the path.
    pub fn codex(label: impl Into<String>, dir: PathBuf) -> Seat {
        Seat {
            id: seat_id(&dir),
            label: label.into(),
            dir,
            claude_json: PathBuf::new(),
            is_default: false,
            found_by: Vec::new(),
            harness: Harness::Codex,
        }
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.dir.join("projects")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.dir.join("sessions")
    }

    /// Directories holding this seat's transcripts on disk (for `SYSTEM` disk usage).
    pub fn disk_dirs(&self) -> Vec<PathBuf> {
        match self.harness {
            Harness::Claude => vec![self.projects_dir()],
            Harness::Agy => vec![self.dir.join("brain"), self.dir.join("conversations")],
            Harness::Codex => vec![
                self.dir.join("sessions"),
                self.dir.join("archived_sessions"),
            ],
        }
    }

    pub fn credentials_file(&self) -> PathBuf {
        match self.harness {
            Harness::Claude => self.dir.join(".credentials.json"),
            Harness::Agy => self.dir.join("antigravity-oauth-token"),
            Harness::Codex => self.dir.join("auth.json"),
        }
    }

    /// Days Claude Code keeps this seat's transcripts: `cleanupPeriodDays` of its
    /// `settings.json` (at least 1), `None` when not set (Claude Code's default applies,
    /// `core::storage::DEFAULT_RETENTION_DAYS`). Harnesses without automatic transcript deletion
    /// return `Ok(None)`. A file that exists but cannot be read or parsed is an error, never "not
    /// set".
    pub fn cleanup_period_days(&self) -> Result<Option<u32>, crate::errors::ConfigError> {
        if !self.harness.has_transcript_retention() {
            return Ok(None);
        }
        let path = self.dir.join("settings.json");
        let damaged = |message: String| crate::errors::ConfigError::SeatSettings {
            path: path.clone(),
            message,
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(damaged(e.to_string())),
        };
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| damaged(e.to_string()))?;
        let Some(days) = v
            .get("cleanupPeriodDays")
            .and_then(serde_json::Value::as_f64)
        else {
            return Ok(None);
        };
        Ok((days.is_finite() && days >= 1.0).then(|| days.min(f64::from(u32::MAX)) as u32))
    }
}

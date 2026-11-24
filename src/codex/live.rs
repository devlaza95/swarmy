//! Which Codex threads run now, and what they do.
//!
//! Codex 0.172 holds an exclusive lock on `<home>/thread-writer-locks/<thread id>.lock` while a
//! thread is open (the holder is its `codex app-server`, a child of the `codex` the user runs);
//! `fsutil::locks` says which are held without a lock of ours. Whether a turn runs comes from the
//! end of the thread's rollout (`task_started` without a `task_complete` after it), read again only
//! when the file changed. The `codex` process of a thread (for `J` and CPU) is looked up only when
//! the set of open threads changed, so a per-second tick reads no process table.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use super::rollout::{self, TailState};
use crate::core::harness::Harness;
use crate::core::ids::{SeatId, SessionId};
use crate::model::live::{LiveSession, SessionStatus};
use crate::procs::{CodexProc, ProcTable};

/// The lock directory of a Codex home.
pub const LOCK_DIR: &str = "thread-writer-locks";

/// The process a thread runs in.
#[derive(Debug, Clone)]
pub struct CodexProcInfo {
    pub pid: u32,
    pub start_time: u64,
    pub pane: Option<crate::core::pane::Pane>,
}

/// A rollout's end, read at a file stamp.
type Tail = (Option<(SystemTime, u64)>, TailState);

#[derive(Debug, Default)]
pub struct CodexLiveCache {
    locks: crate::fsutil::locks::LockDirs,
    by_thread: HashMap<String, CodexProcInfo>,
    /// The open threads the last process look-up was made for, per home.
    scanned_for: HashMap<PathBuf, Vec<String>>,
    tails: HashMap<PathBuf, Tail>,
}

fn stamp(p: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn canon(p: &Path) -> PathBuf {
    crate::fsutil::canonical(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The process of a thread among the `codex` agents of its home: the one in the thread's
/// directory when only one is; else the lock's holder or, when the holder is the app server, the
/// `codex` that started it; else the only agent of the home.
fn pick(
    agents: &[&CodexProc],
    cwd: Option<&str>,
    holder: Option<u32>,
    parent_of: impl Fn(u32) -> Option<u32>,
) -> Option<u32> {
    let in_dir: Vec<u32> = agents
        .iter()
        .filter(|a| cwd.is_some_and(|c| a.cwd.as_deref().is_some_and(|d| d == Path::new(c))))
        .map(|a| a.pid)
        .collect();
    if let [one] = in_dir.as_slice() {
        return Some(*one);
    }
    let is_agent = |pid: u32| agents.iter().any(|a| a.pid == pid);
    if let Some(h) = holder {
        if is_agent(h) {
            return Some(h);
        }
        if let Some(p) = parent_of(h).filter(|p| is_agent(*p)) {
            return Some(p);
        }
    }
    match agents {
        [one] => Some(one.pid),
        [] | [_, _, ..] => None,
    }
}

impl CodexLiveCache {
    /// The threads of `home` that are open now.
    pub fn open_threads(&mut self, home: &Path) -> Vec<String> {
        self.locks.held(&home.join(LOCK_DIR))
    }

    /// Pids of the processes of open threads (for `ProcTable::for_pids`).
    pub fn tracked_pids(&self) -> Vec<u32> {
        self.by_thread.values().map(|p| p.pid).collect()
    }

    /// The end of a rollout, read again only when the file changed.
    fn tail(&mut self, path: &Path) -> TailState {
        let now = stamp(path);
        if let Some((st, t)) = self.tails.get(path)
            && *st == now
        {
            return t.clone();
        }
        let t = rollout::tail_state(path).unwrap_or_default();
        if self.tails.len() > 256 {
            self.tails.clear();
        }
        self.tails.insert(path.to_path_buf(), (now, t.clone()));
        t
    }

    /// Finds the processes of `open` threads: only when the set changed and one of them has no
    /// live process yet, so a process that cannot be found costs one look-up, not one a second.
    pub fn resolve_procs(&mut self, home: &Path, open: &[String]) {
        self.by_thread
            .retain(|tid, info| open.contains(tid) && crate::procs::pid_alive_once(info.pid));
        let missing = open.iter().any(|t| !self.by_thread.contains_key(t));
        let mut key = open.to_vec();
        key.sort();
        if !missing || self.scanned_for.get(home) == Some(&key) {
            return;
        }
        self.scanned_for.insert(home.to_path_buf(), key);
        let table = ProcTable::snapshot();
        let home_c = canon(home);
        let agents: Vec<&CodexProc> = table
            .codex
            .iter()
            .filter(|p| p.home.as_deref().is_some_and(|h| canon(h) == home_c))
            .collect();
        let holders = crate::fsutil::locks::holders(&home.join(LOCK_DIR), open);
        let parent_of = |pid: u32| table.nodes.get(&pid).and_then(|n| n.parent);
        for tid in open {
            if self.by_thread.contains_key(tid) {
                continue;
            }
            let cwd = super::meta::find_rollout(home, tid)
                .map(|p| self.tail(&p))
                .and_then(|t| t.cwd);
            let Some(pid) = pick(
                &agents,
                cwd.as_deref(),
                holders.get(tid).copied(),
                parent_of,
            ) else {
                continue;
            };
            let start_time = agents
                .iter()
                .find(|a| a.pid == pid)
                .map_or(0, |a| a.start_time);
            self.by_thread.insert(
                tid.clone(),
                CodexProcInfo {
                    pid,
                    start_time,
                    pane: crate::terminal::of(None, pid, None),
                },
            );
        }
    }

    /// The open threads of a Codex seat as live sessions.
    pub fn live_sessions(
        &mut self,
        seat: &SeatId,
        home: &Path,
        open: &[String],
        now: DateTime<Utc>,
    ) -> Vec<LiveSession> {
        let titles = super::meta::titles(home);
        let mut out = Vec::new();
        for tid in open {
            let tail = super::meta::find_rollout(home, tid)
                .map(|p| self.tail(&p))
                .unwrap_or_default();
            let info = self.by_thread.get(tid);
            let started_at = info
                .and_then(|i| DateTime::<Utc>::from_timestamp(i.start_time as i64, 0))
                .filter(|_| info.is_some_and(|i| i.start_time > 0))
                .or(tail.since)
                .unwrap_or(now);
            out.push(LiveSession {
                seat: seat.clone(),
                harness: Harness::Codex,
                pid: info.map_or(0, |i| i.pid),
                session_id: SessionId::new(tid.clone()),
                cwd: tail.cwd.clone().unwrap_or_default(),
                name: titles.get(tid).cloned(),
                status: if tail.busy {
                    SessionStatus::Busy
                } else {
                    SessionStatus::Idle
                },
                pane: info.and_then(|i| i.pane.clone()),
                version: None,
                kind: Some("codex".into()),
                started_at,
                status_since: tail.since,
            });
        }
        out.sort_by_key(|s| s.started_at);
        out
    }
}

/// One look without a cache (CLI commands).
pub fn open_threads_once(home: &Path) -> Vec<String> {
    crate::fsutil::locks::held_once(&home.join(LOCK_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pid: u32, cwd: &str) -> CodexProc {
        CodexProc {
            pid,
            parent: None,
            cwd: Some(PathBuf::from(cwd)),
            home: Some(PathBuf::from("/h/.codex")),
            start_time: 0,
            cpu: 0.0,
            memory: 0,
        }
    }

    #[test]
    fn a_thread_runs_in_the_codex_of_its_directory_or_of_its_app_server() {
        let (a, b) = (agent(10, "/w/shop"), agent(20, "/w/docs"));
        let both = [&a, &b];
        let no_parent = |_| None;
        assert_eq!(pick(&both, Some("/w/docs"), None, no_parent), Some(20));
        // Two agents in one directory: the app server holding the lock was started by 10.
        let c = agent(30, "/w/shop");
        let same_dir = [&a, &c];
        let server_of_10 = |pid| (pid == 99).then_some(10);
        assert_eq!(
            pick(&same_dir, Some("/w/shop"), Some(99), server_of_10),
            Some(10)
        );
        assert_eq!(pick(&same_dir, Some("/w/shop"), None, no_parent), None);
        assert_eq!(pick(&[&a], Some("/elsewhere"), None, no_parent), Some(10));
        assert_eq!(pick(&[], Some("/w/shop"), Some(99), no_parent), None);
    }

    #[test]
    fn an_open_thread_is_live_and_busy_while_its_turn_runs() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let tid = "0a1b2c3d-0000-4000-8000-0000000000f1";
        let day = home.join("sessions/2026/11/24");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join(format!("rollout-2026-11-24T10-00-00-{tid}.jsonl"));
        std::fs::write(
            &rollout,
            format!(
                "{}\n{}\n",
                serde_json::json!({"timestamp": "2026-11-24T10:00:00Z", "type": "session_meta",
                    "payload": {"id": tid, "cwd": "/w/shop"}}),
                serde_json::json!({"timestamp": "2026-11-24T10:00:01Z", "type": "event_msg",
                    "payload": {"type": "task_started"}}),
            ),
        )
        .unwrap();
        std::fs::create_dir_all(home.join(LOCK_DIR)).unwrap();
        let lock_path = home.join(LOCK_DIR).join(format!("{tid}.lock"));
        std::fs::write(&lock_path, "").unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder.try_lock().unwrap();

        let mut cache = CodexLiveCache::default();
        let open = cache.open_threads(home);
        assert_eq!(open, vec![tid.to_string()]);
        let seat = SeatId::from_stored("00000000000000c1");
        let now = Utc::now();
        let live = cache.live_sessions(&seat, home, &open, now);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].harness, Harness::Codex);
        assert_eq!(live[0].cwd, "/w/shop");
        assert_eq!(live[0].status, SessionStatus::Busy);

        // The turn ends: idle, read again because the file changed.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        use std::io::Write as _;
        writeln!(
            f,
            "{}",
            serde_json::json!({"timestamp": "2026-11-24T10:00:09Z", "type": "event_msg",
                "payload": {"type": "task_complete", "duration_ms": 8000}})
        )
        .unwrap();
        let live = cache.live_sessions(&seat, home, &open, now);
        assert_eq!(live[0].status, SessionStatus::Idle);

        holder.unlock().unwrap();
        drop(holder);
        assert!(cache.open_threads(home).is_empty());
    }
}

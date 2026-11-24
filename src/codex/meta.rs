//! Where a Codex home keeps its threads: one rollout per thread under
//! `sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl` (archived ones flat under
//! `archived_sessions/`), and the names given to threads in `session_index.jsonl`
//! (`{"id", "thread_name", "updated_at"}`, one line per rename, the last one wins).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use crate::core::harness::Harness;
use crate::core::ids::SeatId;
use crate::model::files::{FileKind, TranscriptFile};

/// The thread id of a rollout file: the uuid at the end of its name.
pub fn thread_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    // A uuid is 36 characters; the time before it is `YYYY-MM-DDTHH-MM-SS-`.
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.len() == 36 && id.chars().filter(|c| *c == '-').count() == 4).then(|| id.to_string())
}

fn rollouts_in(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rollouts_in(&p, out);
        } else if thread_of(&p).is_some() {
            out.push(p);
        }
    }
}

/// Every rollout of a Codex home, archived ones too.
pub fn list_rollouts(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    rollouts_in(&home.join("sessions"), &mut out);
    rollouts_in(&home.join("archived_sessions"), &mut out);
    out.sort();
    out
}

/// Names by thread id, with the stamp of the file they were read at.
type Titles = Mutex<HashMap<PathBuf, (Option<(SystemTime, u64)>, Arc<HashMap<String, String>>)>>;

static TITLES: OnceLock<Titles> = OnceLock::new();

/// The names of a home's threads, read again only when `session_index.jsonl` changed.
pub fn titles(home: &Path) -> Arc<HashMap<String, String>> {
    let path = home.join("session_index.jsonl");
    let stamp = std::fs::metadata(&path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())));
    let cache = TITLES.get_or_init(Default::default);
    if let Ok(c) = cache.lock()
        && let Some((st, t)) = c.get(&path)
        && *st == stamp
    {
        return t.clone();
    }
    let mut names = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let get = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
            if let (Some(id), Some(name)) = (get("id"), get("thread_name"))
                && !name.trim().is_empty()
            {
                names.insert(id.to_string(), name.trim().to_string());
            }
        }
    }
    let names = Arc::new(names);
    if let Ok(mut c) = cache.lock() {
        c.insert(path, (stamp, names.clone()));
    }
    names
}

/// The transcript of one rollout file.
pub fn transcript_file(
    seat: &SeatId,
    path: PathBuf,
    titles: &HashMap<String, String>,
) -> Option<TranscriptFile> {
    let tid = thread_of(&path)?;
    Some(TranscriptFile {
        path,
        seat: seat.clone(),
        session_title: titles.get(&tid).cloned(),
        session_id: tid,
        agent_id: String::new(),
        kind: FileKind::Main,
        harness: Harness::Codex,
        default_cwd: None,
        inline_meta: None,
    })
}

/// Every transcript of a Codex seat.
pub fn list_transcripts(seat: &SeatId, home: &Path) -> Vec<TranscriptFile> {
    let titles = titles(home);
    list_rollouts(home)
        .into_iter()
        .filter_map(|p| transcript_file(seat, p, &titles))
        .collect()
}

/// Rollout paths found by thread id, per home.
type Found = Mutex<HashMap<(PathBuf, String), PathBuf>>;

static FOUND: OnceLock<Found> = OnceLock::new();

/// Threads whose rollout was not found, per home, with the stamp of the newest day dir then.
type Missed = Mutex<HashMap<(PathBuf, String), Option<DayStamp>>>;

/// The newest day dir, when it last changed, and how many files it holds (a change within one
/// tick of a coarse clock still shows in the count).
type DayStamp = (PathBuf, std::time::SystemTime, usize);

static MISSED: OnceLock<Missed> = OnceLock::new();

/// The newest day dir of `sessions/` and when it last changed: a new rollout of a running thread
/// lands there. Three small listings, not a walk of every day.
fn newest_day(home: &Path) -> Option<DayStamp> {
    let y = sorted_dirs(&home.join("sessions")).into_iter().next()?;
    let m = sorted_dirs(&y).into_iter().next()?;
    let d = sorted_dirs(&m).into_iter().next()?;
    let t = std::fs::metadata(&d).and_then(|m| m.modified()).ok()?;
    let n = std::fs::read_dir(&d).map_or(0, |rd| rd.count());
    Some((d, t, n))
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v.reverse();
    v
}

/// The rollout of thread `tid`, newest day first; remembered once found (a rollout never moves
/// while its thread runs), so a per-second look-up lists no directory.
pub fn find_rollout(home: &Path, tid: &str) -> Option<PathBuf> {
    let key = (home.to_path_buf(), tid.to_string());
    let found = FOUND.get_or_init(Default::default);
    if let Ok(f) = found.lock()
        && let Some(p) = f.get(&key)
        && p.is_file()
    {
        return Some(p.clone());
    }
    // A miss is not looked for again until a day dir changes: an open thread whose rollout is
    // not written yet walked every day dir each second (review B-L15).
    let stamp = newest_day(home);
    let missed = MISSED.get_or_init(Default::default);
    if let Ok(m) = missed.lock()
        && m.get(&key) == Some(&stamp)
    {
        return None;
    }
    let suffix = format!("-{tid}.jsonl");
    let matches = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
    };
    let mut hit = None;
    'days: for y in sorted_dirs(&home.join("sessions")) {
        for m in sorted_dirs(&y) {
            for d in sorted_dirs(&m) {
                if let Ok(rd) = std::fs::read_dir(&d)
                    && let Some(p) = rd.flatten().map(|e| e.path()).find(|p| matches(p))
                {
                    hit = Some(p);
                    break 'days;
                }
            }
        }
    }
    let Some(hit) = hit else {
        if let Ok(mut m) = missed.lock() {
            if m.len() > 4096 {
                m.clear();
            }
            m.insert(key, stamp);
        }
        return None;
    };
    if let Ok(mut m) = missed.lock() {
        m.remove(&key);
    }
    if let Ok(mut f) = found.lock() {
        if f.len() > 4096 {
            f.clear();
        }
        f.insert(key, hit.clone());
    }
    Some(hit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review B-L15: a miss is remembered until the newest day dir changes; a rollout written
    /// then is found.
    #[test]
    fn a_missing_rollout_is_found_once_written() {
        let t = tempfile::tempdir().unwrap();
        let day = t.path().join("sessions/2026/11/24");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-11-24T10-00-00-other.jsonl"), "").unwrap();
        let tid = "0c0d0e0f-0000-4000-8000-0000000000aa";
        assert_eq!(find_rollout(t.path(), tid), None);
        assert_eq!(find_rollout(t.path(), tid), None, "remembered");
        let p = day.join(format!("rollout-2026-11-24T10-05-00-{tid}.jsonl"));
        std::fs::write(&p, "").unwrap();
        assert_eq!(find_rollout(t.path(), tid), Some(p));
    }

    const TID: &str = "0a1b2c3d-0000-4000-8000-00000000000a";

    #[test]
    fn names_threads_by_the_uuid_at_the_end_of_the_file_name() {
        let p = PathBuf::from(format!(
            "/h/sessions/2026/11/24/rollout-2026-11-24T13-02-15-{TID}.jsonl"
        ));
        assert_eq!(thread_of(&p).as_deref(), Some(TID));
        assert_eq!(thread_of(Path::new("/h/sessions/notes.jsonl")), None);
        assert_eq!(thread_of(Path::new("/h/rollout-short.jsonl")), None);
    }

    #[test]
    fn lists_finds_and_names_rollouts() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let day = home.join("sessions/2026/11/24");
        std::fs::create_dir_all(&day).unwrap();
        let older = home.join("sessions/2026/10/01");
        std::fs::create_dir_all(&older).unwrap();
        let p = day.join(format!("rollout-2026-11-24T10-00-00-{TID}.jsonl"));
        std::fs::write(&p, "").unwrap();
        let other = "0a1b2c3d-0000-4000-8000-00000000000b";
        std::fs::write(
            older.join(format!("rollout-2026-10-01T10-00-00-{other}.jsonl")),
            "",
        )
        .unwrap();
        std::fs::write(day.join("unrelated.txt"), "").unwrap();
        std::fs::write(
            home.join("session_index.jsonl"),
            format!(
                "{{\"id\":\"{TID}\",\"thread_name\":\"First name\",\"updated_at\":\"x\"}}\n\
                 {{\"id\":\"{TID}\",\"thread_name\":\"Renamed\",\"updated_at\":\"y\"}}\n"
            ),
        )
        .unwrap();
        let seat = SeatId::from_stored("00000000000000c1");
        let files = list_transcripts(&seat, home);
        assert_eq!(files.len(), 2);
        let f = files.iter().find(|f| f.session_id == TID).unwrap();
        assert_eq!(f.harness, Harness::Codex);
        assert_eq!(f.session_title.as_deref(), Some("Renamed"));
        assert_eq!(find_rollout(home, TID).as_deref(), Some(p.as_path()));
        assert!(find_rollout(home, other).is_some());
        assert!(find_rollout(home, "0a1b2c3d-0000-4000-8000-00000000000c").is_none());
    }
}

//! Read-only SQLite reader for Google Antigravity's `conversation_summaries.db`: a reader of
//! agy's files, so it lives with them (review A-M2: it sat in `src/index/` only to pass the
//! SQL check). `tests/architecture.rs::sql_stays_in_the_index` allows it alone outside the
//! index, and only opened read-only. Opens the database with a short busy timeout and falls back
//! to `cache/conversation_metadata.json` when absent or busy; a failure other than a missing
//! file is logged on the debug channel, never silent.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};

use super::meta::{
    ConversationMeta, RunStatus, parse_timestamp, parse_workspace_uris, read_metadata_json,
};

const BUSY_TIMEOUT: Duration = Duration::from_millis(200);

fn open_ro(seat_dir: &Path) -> Option<Connection> {
    let db = seat_dir.join("conversation_summaries.db");
    if !db.is_file() {
        return None;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match Connection::open_with_flags(&db, flags) {
        Ok(c) => c,
        Err(e) => {
            crate::diag::log("agy", &format!("cannot open {}: {e}", db.display()));
            return None;
        }
    };
    if let Err(e) = conn.busy_timeout(BUSY_TIMEOUT) {
        crate::diag::log("agy", &format!("{}: {e}", db.display()));
        return None;
    }
    Some(conn)
}

fn row_to_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationMeta> {
    let cid: String = r.get(0)?;
    let title: String = r.get(1)?;
    let workspace_uris: String = r.get(2)?;
    let status: String = r.get(3)?;
    let parent: String = r.get(4)?;
    let depth: i64 = r.get(5)?;
    let agent_name: String = r.get(6)?;
    let not_fully_idle: i64 = r.get(7)?;
    let killed: i64 = r.get(8)?;
    let modified: String = r.get(9)?;
    Ok(ConversationMeta {
        conversation_id: cid,
        title: (!title.trim().is_empty()).then_some(title),
        cwd: parse_workspace_uris(&workspace_uris),
        status: RunStatus::parse(&status),
        parent_conversation_id: (!parent.trim().is_empty()).then_some(parent),
        nesting_depth: depth.max(0) as u32,
        agent_name: (!agent_name.trim().is_empty()).then_some(agent_name),
        not_fully_idle: not_fully_idle != 0,
        killed: killed != 0,
        last_modified: parse_timestamp(&modified),
    })
}

const COLS: &str = "conversation_id, title, workspace_uris, status, parent_conversation_id, \
                    nesting_depth, agent_name, not_fully_idle, killed, last_modified_time";

/// Every conversation of a seat, by id.
pub type Metas = HashMap<String, ConversationMeta>;

/// Each seat's last read and the stamp it was made at.
type Cache = std::sync::Mutex<HashMap<std::path::PathBuf, (crate::agy::meta::Stamp, Arc<Metas>)>>;

/// The last read of each seat, kept while its files are unchanged: the dashboard asks every
/// second, Antigravity writes the database only when a conversation changes.
static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();

/// Every conversation summary of an Antigravity seat, read again only when its files changed.
/// The whole table is small (one row per conversation), and holding all of it lets a subagent's
/// lineage resolve to the same root on a hot refresh as on a full one, whatever its depth.
pub fn read_all(seat_dir: &Path) -> Arc<Metas> {
    let now = crate::agy::meta::summaries_stamp(seat_dir);
    let cache = CACHE.get_or_init(Default::default);
    if let Ok(c) = cache.lock()
        && let Some((st, metas)) = c.get(seat_dir)
        && *st == now
    {
        return metas.clone();
    }
    let metas = Arc::new(read_uncached(seat_dir));
    if let Ok(mut c) = cache.lock() {
        c.insert(seat_dir.to_path_buf(), (now, metas.clone()));
    }
    metas
}

/// Falls back to `cache/conversation_metadata.json` when the SQLite DB is absent or unreadable.
fn read_uncached(seat_dir: &Path) -> Metas {
    let Some(conn) = open_ro(seat_dir) else {
        return read_metadata_json(seat_dir);
    };
    let sql = format!("SELECT {COLS} FROM conversation_summaries");
    let mut st = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return read_metadata_json(seat_dir),
    };
    let rows = match st.query_map([], row_to_meta) {
        Ok(r) => r,
        Err(_) => return read_metadata_json(seat_dir),
    };
    let mut out = HashMap::new();
    for r in rows.flatten() {
        out.insert(r.conversation_id.clone(), r);
    }
    if out.is_empty() {
        return read_metadata_json(seat_dir);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_all_and_targeted_summaries_from_sqlite() {
        let t = tempfile::tempdir().unwrap();
        let db = t.path().join("conversation_summaries.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversation_summaries (
               conversation_id TEXT PRIMARY KEY,
               title TEXT NOT NULL DEFAULT '',
               workspace_uris TEXT NOT NULL DEFAULT '',
               status TEXT NOT NULL DEFAULT '',
               parent_conversation_id TEXT NOT NULL DEFAULT '',
               nesting_depth INTEGER NOT NULL DEFAULT 0,
               agent_name TEXT NOT NULL DEFAULT '',
               not_fully_idle NUMERIC NOT NULL DEFAULT 0,
               killed NUMERIC NOT NULL DEFAULT 0,
               last_modified_time TEXT NOT NULL DEFAULT ''
             );
             INSERT INTO conversation_summaries VALUES
               ('c-root', 'Main task', '[\"file:///w/shop\"]', 'CASCADE_RUN_STATUS_RUNNING', '', 0, '', 1, 0, '2026-11-23 20:00:00+00:00'),
               ('c-sub', 'Research', '[\"file:///w/shop\"]', 'CASCADE_RUN_STATUS_RUNNING', 'c-root', 1, 'research', 1, 0, '2026-11-23 20:01:00+00:00'),
               ('c-other', 'Old task', '[\"file:///w/docs\"]', 'CASCADE_RUN_STATUS_IDLE', '', 0, '', 0, 0, '2026-11-20 10:00:00+00:00');",
        )
        .unwrap();
        drop(conn);

        let all = read_all(t.path());
        assert_eq!(all.len(), 3);
        assert_eq!(all["c-root"].status, RunStatus::Running);
        assert_eq!(all["c-root"].cwd.as_deref(), Some("/w/shop"));
        assert_eq!(
            all["c-sub"].parent_conversation_id.as_deref(),
            Some("c-root")
        );

        // Unchanged files: the same read, without opening the database again.
        let again = read_all(t.path());
        assert!(Arc::ptr_eq(&all, &again));
    }
}

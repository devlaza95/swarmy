//! Read marks: what the writer must not read again because it was rolled up already
//! (`docs/design/storage.md` §4.3).
//!
//! After a compaction the rolled-up records' keys (`msg_id`, tool ids) are gone, so a record read
//! again would be inserted as new and counted twice against its rollup. Bytes past a file's
//! stored offset were never read, so an incremental read is always new. A file read from its
//! start (replaced, truncated, found again, or every file after a cache rebuild) may repeat
//! records the index already rolled up: those older than `rolled_until` are skipped when the
//! file belongs to what was indexed before ([`Marks::of`], the one rule). Nothing is skipped on
//! a guess: a file the index never read is new information unless every old response in it is
//! one the archive holds ([`all_held`]).

use std::collections::HashSet;

use rusqlite::{Connection, params};

use super::compact::canonical;
use super::ms;
use crate::claude::files::TranscriptFile;
use crate::claude::seats::{Seat, SeatId};
use crate::claude::transcript::Event;
use crate::core::period::BUCKET_MS;

/// What a file read now may repeat of the rolled-up records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    /// Nothing: read past its stored offset, or never indexed (its records are new).
    New,
    /// Records the index read before: those older than `rolled_until` are skipped.
    Repeats,
    /// A session the index never saw, in a seat a compaction ran over: a fork copies old
    /// records (old message ids and times under a new session id), a transcript synced from
    /// another machine brings new ones. Decided when the file is read ([`all_held`]).
    MaybeCopies,
}

/// The seats a compaction ran over, for one refresh.
pub(super) struct Marks {
    /// The seats whose config dir is in `rollup_seats`.
    rolled_seats: HashSet<SeatId>,
}

impl Marks {
    pub(super) fn load(conn: &Connection, seats: &[Seat]) -> rusqlite::Result<Marks> {
        let dirs: HashSet<String> = conn
            .prepare_cached("SELECT seat_dir FROM rollup_seats")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let rolled_seats = if dirs.is_empty() {
            HashSet::new()
        } else {
            seats
                .iter()
                .filter(|s| dirs.contains(&canonical(&s.dir)))
                .map(|s| s.id.clone())
                .collect()
        };
        Ok(Marks { rolled_seats })
    }

    /// What `file`, read from byte `start`, may repeat. Decided before the refresh writes
    /// anything (a session first seen in this refresh is new information however its files are
    /// ordered):
    /// 1. read past its stored offset: nothing ([`Mark::New`]);
    /// 2. its agent (session and agent id) was indexed before, on any seat (a row in `agents`,
    ///    which outlives the raw rows): a known file read again, a resumed session, a seat whose
    ///    dir was moved or relabelled (session ids stay, the seat id does not) — [`Mark::Repeats`];
    /// 3. its session was indexed but this agent never was (a subagent file never read): its
    ///    records were never written — [`Mark::New`];
    /// 4. a session never seen, in a seat indexed when a compaction ran: a fork or a transcript
    ///    from elsewhere — [`Mark::MaybeCopies`];
    /// 5. otherwise (a seat found for the first time): new; its old records go into raw rows,
    ///    rolled up by the refresh at once (`Index::roll_late`).
    pub(super) fn of(
        &self,
        conn: &Connection,
        file: &TranscriptFile,
        start: u64,
    ) -> rusqlite::Result<Mark> {
        if start > 0 {
            return Ok(Mark::New);
        }
        let mut st = conn.prepare_cached(
            "SELECT MAX(agent_id = ?2) FROM agents INDEXED BY agents_session WHERE session_id = ?1",
        )?;
        let known: Option<bool> =
            st.query_row(params![file.session_id, file.agent_id], |r| r.get(0))?;
        Ok(match known {
            Some(true) => Mark::Repeats,
            Some(false) => Mark::New,
            None if self.rolled_seats.contains(&file.seat) => Mark::MaybeCopies,
            None => Mark::New,
        })
    }
}

/// Whether every response older than `rolled_until` in `events` is one the archive holds: a
/// rollup row of its quarter hour and model whose token sums are at least the response's own
/// (any seat, session and account: a fork keeps the times, models, dirs and usage of what it
/// copies). The dir narrows it: a rollup row sums a whole agent's quarter hour, so a response
/// from another machine in a busy local quarter hour of another dir passed for a copy, and its
/// file was skipped (review B-M9).
/// A file with no response that old repeats nothing. A response the archive cannot hold is new,
/// and then the whole file is (a fork copies only what was there).
pub(super) fn all_held(
    conn: &Connection,
    events: &[Event],
    rolled_until: i64,
) -> rusqlite::Result<bool> {
    let mut st = conn.prepare_cached(
        "SELECT 1 FROM usage_rollup WHERE bucket = ?1 AND model = ?2 AND input >= ?3 AND cw5 >= ?4
           AND cw1h >= ?5 AND cread >= ?6 AND output >= ?7 AND cwd IS NULLIF(?8, '') LIMIT 1",
    )?;
    let mut seen: HashSet<&str> = HashSet::new();
    let mut any = false;
    for ev in events {
        let Event::Request(r) = ev else { continue };
        if !seen.insert(r.msg_id.as_str()) {
            continue;
        }
        let t = ms(r.ts);
        if t >= rolled_until {
            continue;
        }
        any = true;
        let u = &r.usage;
        let tok = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        if !st.exists(params![
            t.div_euclid(BUCKET_MS),
            r.model,
            tok(u.input),
            tok(u.cache_write_5m),
            tok(u.cache_write_1h),
            tok(u.cache_read),
            tok(u.output),
            r.cwd.as_deref().unwrap_or("")
        ])? {
            return Ok(false);
        }
    }
    Ok(any)
}

/// Whether a record at `ts` of a file whose marks apply (see [`Marks::of`]) was rolled up
/// already and is skipped. A response is decided once, by its first record (the time its raw row
/// carried when it was rolled up), never block by block (`write::write_file`).
pub(super) fn rolled_up(applies: bool, rolled_until: i64, ts: i64) -> bool {
    applies && ts < rolled_until
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::files::FileKind;
    use crate::index::schema;

    fn file(seat: &Seat, session: &str) -> TranscriptFile {
        TranscriptFile {
            path: format!("/x/{session}.jsonl").into(),
            seat: seat.id.clone(),
            session_id: session.into(),
            agent_id: String::new(),
            kind: FileKind::Main,
            harness: seat.harness,
            default_cwd: None,
            session_title: None,
            inline_meta: None,
        }
    }

    #[test]
    fn the_rule() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("old")).unwrap();
        let seat = |label: &str| {
            let dir = t.path().join(label);
            Seat::new(label, dir.clone(), dir.join(".claude.json"), false)
        };
        let seats = [seat("old"), seat("new")];
        let (old, new) = (&seats[0], &seats[1]);
        let c = Connection::open_in_memory().unwrap();
        schema::migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO agents(seat, session_id, agent_id, kind) VALUES ('elsewhere', 'known', '', 'main');",
        )
        .unwrap();
        let sub = |s: &Seat, session: &str| TranscriptFile {
            agent_id: "a1".into(),
            kind: FileKind::Subagent,
            ..file(s, session)
        };
        // Nothing compacted yet: only known agents repeat.
        let m = Marks::load(&c, &seats).unwrap();
        assert_eq!(m.of(&c, &file(new, "known"), 0).unwrap(), Mark::Repeats);
        assert_eq!(m.of(&c, &file(old, "fork"), 0).unwrap(), Mark::New);
        c.execute(
            "INSERT INTO rollup_seats VALUES (?1, 'old')",
            [canonical(&seats[0].dir)],
        )
        .unwrap();
        let m = Marks::load(&c, &seats).unwrap();
        // A known agent (on any seat); a subagent never read of a known session is new; a new
        // session in a compacted seat is decided by its records; never past the stored offset.
        assert_eq!(m.of(&c, &file(new, "known"), 0).unwrap(), Mark::Repeats);
        assert_eq!(m.of(&c, &sub(old, "known"), 0).unwrap(), Mark::New);
        assert_eq!(m.of(&c, &file(old, "fork"), 0).unwrap(), Mark::MaybeCopies);
        assert_eq!(m.of(&c, &file(new, "fresh"), 0).unwrap(), Mark::New);
        assert_eq!(m.of(&c, &file(old, "known"), 10).unwrap(), Mark::New);
        assert!(rolled_up(true, 100, 99));
        assert!(!rolled_up(true, 100, 100));
        assert!(!rolled_up(false, 100, 99));
    }
}

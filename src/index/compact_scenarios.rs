//! The compactor, proved on the populated world of `archive_scenarios`: compacting changes no
//! number on any surface (compared with the same index never compacted), a run interrupted
//! mid-way and resumed ends where an uninterrupted one does, and a second run changes nothing.
//! Then the read marks: files read again after a compaction (appended, replaced, truncated,
//! forked, every file after a cache rebuild, a seat found later) count every record once. Then
//! what changes old spend after it was rolled up: a price change, a new revision of the project
//! rules; and what does not: a login change found late (review H1).

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::NaiveDate;
    use serde_json::Value;

    use crate::core::period::{BUCKET_MS, Period};
    use crate::core::pricing::{LongContext, PriceBook, PriceEntry, PriceSource, RateOverrides};
    use crate::index::archive;
    use crate::index::archive_scenarios::{NOW, diff, populate, readings, refresh, snapshot};
    use crate::index::compact::{self, CompactOpts, CompactReport, FAIL_IN_CHUNK};
    use crate::index::query::Filter;
    use crate::index::scenarios::{World, account, assistant, at, owner};
    use crate::index::{Index, SeatInfo, schema};

    /// The populated world, refreshed, with readings, in `base.sqlite` (closed). The seat
    /// `late` stays empty: transcripts show up there only after a compaction.
    fn base() -> (World, PathBuf) {
        let w = World::new(&["work", "home", "lab", "late"]);
        populate(&w);
        let path = w.dir.path().join("base.sqlite");
        let mut idx = Index::open(&path).unwrap();
        refresh(&w, &mut idx);
        readings(&idx);
        drop(idx);
        (w, path)
    }

    /// A copy of the closed index at `from`, opened.
    fn copy(w: &World, from: &Path, name: &str) -> Index {
        let to = w.dir.path().join(name);
        std::fs::copy(from, &to).unwrap();
        Index::open(&to).unwrap()
    }

    fn compact(w: &World, idx: &mut Index, keep: i64, max_chunks: Option<usize>) -> CompactReport {
        try_compact(w, idx, keep, max_chunks).unwrap()
    }

    fn try_compact(
        w: &World,
        idx: &mut Index,
        keep: i64,
        max_chunks: Option<usize>,
    ) -> anyhow::Result<CompactReport> {
        idx.compact(&CompactOpts {
            keep_raw_days: keep,
            now: at(NOW),
            prices: &PriceBook::builtin(),
            seats: &w.seats,
            max_chunks,
        })
    }

    fn count(idx: &Index, sql: &str) -> i64 {
        idx.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Every row of every table, as sorted text: two indexes in the same state dump the same.
    /// What a run records about itself (the day's size sample, the last compaction's time) is
    /// left out.
    fn dump(idx: &Index) -> Vec<String> {
        let mut out = Vec::new();
        let tables = schema::RAW_TABLES
            .iter()
            .chain(schema::DIMENSION_TABLES)
            .chain(schema::ARCHIVE_TABLES)
            .chain(schema::FACT_TABLES);
        for t in tables {
            let mut st = idx.conn.prepare(&format!("SELECT * FROM {t}")).unwrap();
            let n = st.column_count();
            let mut rows: Vec<String> = st
                .query_map([], |r| {
                    use rusqlite::types::ValueRef as V;
                    Ok((0..n)
                        .map(|i| match r.get_ref(i).unwrap() {
                            V::Null => "NULL".to_string(),
                            V::Integer(x) => x.to_string(),
                            V::Real(x) => format!("{x:?}"),
                            V::Text(s) | V::Blob(s) => String::from_utf8_lossy(s).into_owned(),
                        })
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap()
                .map(|r| format!("{t}: {}", r.unwrap()))
                .collect();
            rows.sort();
            out.extend(rows.into_iter().filter(|r| {
                !r.starts_with("storage_samples:") && !r.starts_with("meta: last_compaction|")
            }));
        }
        out
    }

    fn assert_same(before: &Value, after: &Value, what: &str) {
        let mut d = Vec::new();
        diff(before, after, "", &mut d);
        assert!(
            d.is_empty(),
            "{what}: {} differences, the first: {:#?}",
            d.len(),
            &d[..d.len().min(20)]
        );
    }

    /// Review B-M6: the compacting process does not know every seat (one found only through
    /// another run's processes). The seats whose rows it rolls up are recorded all the same, so
    /// a later re-read of their old transcripts is judged by the archive.
    #[test]
    fn a_seat_this_process_did_not_find_is_recorded_as_rolled_up() {
        let (w, path) = base();
        let mut idx = copy(&w, &path, "partial.sqlite");
        let lab = w.seats.iter().find(|s| s.label == "lab").unwrap().clone();
        let others: Vec<_> = w.seats.iter().filter(|s| s.id != lab.id).cloned().collect();
        let had_rows = count(
            &idx,
            &format!("SELECT COUNT(*) FROM requests WHERE seat = '{}'", lab.id),
        );
        assert!(had_rows > 0);
        idx.compact(&CompactOpts {
            keep_raw_days: 8,
            now: at(NOW),
            prices: &PriceBook::builtin(),
            seats: &others,
            max_chunks: None,
        })
        .unwrap();
        let rolled_lab = count(
            &idx,
            &format!(
                "SELECT COUNT(*) FROM rollup_seats WHERE seat = '{}'",
                lab.id
            ),
        );
        assert_eq!(rolled_lab, 1);
        let lab_rollups = count(
            &idx,
            &format!(
                "SELECT COUNT(*) FROM usage_rollup WHERE seat = '{}'",
                lab.id
            ),
        );
        assert!(lab_rollups > 0, "lab's old rows were rolled up");
    }

    #[test]
    fn compaction_changes_no_number() {
        let (w, path) = base();
        let before = snapshot(&w, &Index::open(&path).unwrap());
        for keep in [14, 8, 28] {
            let mut idx = copy(&w, &path, &format!("k{keep}.sqlite"));
            let raw_before = count(&idx, "SELECT COUNT(*) FROM requests");
            let (prompts, chars, errors) = (
                count(&idx, "SELECT COUNT(*) FROM prompts"),
                count(&idx, "SELECT SUM(chars) FROM prompts"),
                count(&idx, "SELECT COUNT(*) FROM api_errors"),
            );
            // An older owner of the first day's session (owners only label requests that have no
            // account; the snapshot does not see it).
            idx.conn
                .execute(
                    "INSERT INTO owners VALUES (?1, 's00', -5, 'acc-z')",
                    [w.id("work")],
                )
                .unwrap();
            let r = compact(&w, &mut idx, keep, None);
            assert!(r.done && r.waiting.is_none(), "{r:?}");
            assert_eq!(r.cutoff, compact::cutoff(at(NOW), keep));
            assert!(r.requests > 100 && r.tool_calls > 100, "{keep}: {r:?}");
            assert!(r.prompts > 0 && r.api_errors > 0, "{keep}: {r:?}");
            assert_eq!(r.rolled_until % BUCKET_MS, 0);
            assert_eq!(r.rolled_until, r.cutoff, "{keep}: {r:?}");
            // Every old row moved, once.
            for t in ["requests", "tool_calls", "prompts", "api_errors"] {
                let old = format!("SELECT COUNT(*) FROM {t} WHERE ts < {}", r.cutoff);
                assert_eq!(count(&idx, &old), 0, "{keep}: {t}");
            }
            assert_eq!(
                count(&idx, "SELECT COUNT(*) FROM requests")
                    + count(&idx, "SELECT SUM(requests) FROM usage_rollup"),
                raw_before
            );
            // Prompt characters show nowhere yet; they add up all the same.
            assert_eq!(
                (
                    count(&idx, "SELECT COUNT(*) FROM prompts")
                        + count(&idx, "SELECT SUM(prompts) FROM prompt_rollup"),
                    count(&idx, "SELECT SUM(chars) FROM prompts")
                        + count(&idx, "SELECT SUM(chars) FROM prompt_rollup"),
                    count(&idx, "SELECT COUNT(*) FROM api_errors")
                        + count(&idx, "SELECT SUM(n) FROM api_error_rollup"),
                ),
                (prompts, chars, errors)
            );
            assert_eq!(
                count(&idx, "SELECT SUM(calls) FROM tool_rollup"),
                r.tool_calls as i64
            );
            // `agent_rollup`, grown chunk by chunk, is what it derives from `usage_rollup`.
            let grown = dump(&idx);
            idx.conn
                .execute_batch(&archive::agent_rollup_sql("1"))
                .unwrap();
            let derived = dump(&idx);
            let agents = |d: &[String]| -> Vec<String> {
                d.iter()
                    .filter(|r| r.starts_with("agent_rollup:"))
                    .map(|r| {
                        // The cost (a float summed in another order) to nine decimals.
                        let f: Vec<&str> = r.split('|').collect();
                        let cost: f64 = f[4].parse().unwrap();
                        format!("{}|{cost:.9}|{}", f[..4].join("|"), f[5..].join("|"))
                    })
                    .collect()
            };
            assert!(!agents(&grown).is_empty());
            assert_eq!(agents(&grown), agents(&derived), "{keep}");
            // A finished session keeps only its newest owner row.
            assert_eq!(r.owners_pruned, 1, "{keep}");
            assert_eq!(
                count(&idx, "SELECT COUNT(*) FROM owners WHERE session_id = 's00'"),
                1
            );
            assert_eq!(
                count(
                    &idx,
                    "SELECT COUNT(*) FROM owners WHERE session_id = 's00' AND account_uuid = 'acc-a'"
                ),
                1
            );
            drop(idx);
            let idx = Index::open(&w.dir.path().join(format!("k{keep}.sqlite"))).unwrap();
            assert!(
                archive::rolled_until(&idx.conn).unwrap() == r.rolled_until,
                "{keep}"
            );
            assert_same(&before, &snapshot(&w, &idx), &format!("keep {keep}"));
        }
    }

    #[test]
    fn compaction_is_idempotent_and_survives_an_interruption() {
        let (w, path) = base();
        let mut once = copy(&w, &path, "once.sqlite");
        let r = compact(&w, &mut once, 14, None);
        assert!(r.chunks > 3, "{r:?}");
        let done = dump(&once);
        // Twice is the same as once.
        let again = compact(&w, &mut once, 14, None);
        assert_eq!(again.chunks, 0);
        assert!(again.done);
        assert_eq!(again.rolled_until, r.rolled_until);
        assert_eq!(dump(&once), done, "a second run changes nothing");

        // A fault after chunk 3's inserts, before its deletes: chunks 1–2 stand, 3 rolls back.
        let mut cut = copy(&w, &path, "cut.sqlite");
        FAIL_IN_CHUNK.set(Some(3));
        let err = try_compact(&w, &mut cut, 14, None);
        FAIL_IN_CHUNK.set(None);
        assert!(err.is_err());
        let mut two = copy(&w, &path, "two.sqlite");
        let r2 = compact(&w, &mut two, 14, Some(2));
        assert_eq!(r2.chunks, 2);
        assert!(!r2.done);
        assert_eq!(dump(&cut), dump(&two), "the state after chunk 2");
        // Resumed, it ends where the uninterrupted run did.
        let rest = compact(&w, &mut cut, 14, None);
        assert_eq!(rest.chunks + 2, r.chunks);
        assert_eq!(dump(&cut), done);
    }

    #[test]
    fn a_chunk_with_pending_requests_waits() {
        let (w, path) = base();
        let mut idx = copy(&w, &path, "p.sqlite");
        // One old request is not priced yet (its price comes with the next refresh).
        let victim: i64 = count(
            &idx,
            "SELECT ts FROM requests ORDER BY ts LIMIT 1 OFFSET 200",
        );
        idx.conn
            .execute(
                &format!(
                    "UPDATE requests SET {} WHERE ts = ?1",
                    schema::unpriced_sql()
                ),
                [victim],
            )
            .unwrap();
        let r = compact(&w, &mut idx, 14, None);
        let chunk = victim - victim.rem_euclid(compact::CHUNK_MS);
        assert_eq!(r.waiting, Some(chunk));
        assert!(!r.done);
        assert!(
            r.rolled_until <= chunk,
            "nothing at or after the waiting chunk"
        );
        assert!(
            count(
                &idx,
                &format!("SELECT COUNT(*) FROM requests WHERE ts = {victim}")
            ) == 1
        );
        assert!(
            count(
                &idx,
                &format!("SELECT COUNT(*) FROM requests WHERE ts < {chunk}")
            ) == 0
        );
        // Priced, it goes on and ends like a run that never waited.
        idx.price_all(&PriceBook::builtin()).unwrap();
        let r = compact(&w, &mut idx, 14, None);
        assert!(r.done);
        let mut clean = copy(&w, &path, "c.sqlite");
        compact(&w, &mut clean, 14, None);
        let rollups = |i: &Index| -> Vec<String> {
            dump(i)
                .into_iter()
                .filter(|r| r.contains("_rollup:"))
                .collect()
        };
        assert_eq!(rollups(&idx), rollups(&clean));
    }

    #[test]
    fn the_raw_window_is_never_shorter_than_the_weekly_window() {
        let (w, path) = base();
        let mut idx = copy(&w, &path, "s.sqlite");
        assert!(try_compact(&w, &mut idx, archive::MIN_RAW_DAYS - 1, None).is_err());
        assert_eq!(count(&idx, "SELECT COUNT(*) FROM usage_rollup"), 0);
    }

    #[test]
    fn new_requests_never_reuse_a_deleted_rowid() {
        // The compactor may delete the row with the largest rowid (an old row backfilled last);
        // rows written later still number above it, so `post_pass` finds them.
        let (w, path) = base();
        let mut idx = copy(&w, &path, "r.sqlite");
        let top = count(&idx, "SELECT MAX(rowid) FROM requests");
        idx.conn
            .execute(
                "UPDATE requests SET rowid = ?1 + 1 WHERE rowid = (SELECT rowid FROM requests ORDER BY ts LIMIT 1)",
                [top],
            )
            .unwrap();
        compact(&w, &mut idx, 14, None);
        assert!(count(&idx, "SELECT MAX(rowid) FROM requests") <= top);
        assert_eq!(compact::requests_rowid_high(&idx.conn).unwrap(), top + 1);
    }

    /// A seat's project dir of the populated world.
    fn proj(w: &World, seat: &str) -> PathBuf {
        w.seats
            .iter()
            .find(|s| s.label == seat)
            .unwrap()
            .projects_dir()
            .join("-w-proj")
    }

    fn lines(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    fn text(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    /// A new response of the last day.
    fn fresh(id: &str, t: &str) -> String {
        assistant(id, t, "claude-sonnet-5-5", "/w/shop", (1200, 300), &[])
    }

    /// Changes to the transcripts on disk after a compaction, as Claude Code and the user make
    /// them: an append, files replaced (another inode) and truncated in place (shorter, same
    /// inode), each for an old and a recent session, and a fork of an old session.
    fn rewrite(w: &World) {
        let work = proj(w, "work");
        let mut long = lines(&work.join("long.jsonl"));
        long.push(fresh("long-new", "2026-10-30T20:00:00Z"));
        std::fs::write(work.join("long.jsonl"), text(&long)).unwrap();
        // Replaced with a copy that has one more response.
        for (seat, session) in [("lab", "s05"), ("lab", "s35")] {
            let p = proj(w, seat).join(format!("{session}.jsonl"));
            let mut l = lines(&p);
            l.push(fresh(&format!("{session}-new"), "2026-10-30T21:00:00Z"));
            let tmp = p.with_extension("tmp");
            std::fs::write(&tmp, text(&l)).unwrap();
            std::fs::rename(&tmp, &p).unwrap();
        }
        // Rewritten in place without the title line: shorter, so read again from the start.
        for (seat, session) in [("home", "s10"), ("work", "s36")] {
            let p = proj(w, seat).join(format!("{session}.jsonl"));
            let l: Vec<String> = lines(&p)
                .into_iter()
                .filter(|l| !l.contains("custom-title"))
                .collect();
            std::fs::write(&p, text(&l)).unwrap();
        }
        // A fork of the third day's session: a new session id, the old records (same message,
        // tool and record ids, same times), then a new response. Its prompts and API errors are
        // the old ones again, counted once (review B-M8: keyed by session, they counted twice).
        // The fork is the larger file, read first by a rebuild: the copies still belong to the
        // session that started first (`index::scenarios::a_forks_copies_belong_to_…`).
        let old = lines(&work.join("s03.jsonl"));
        let mut fork = vec![owner("acc-a")];
        fork.extend(old);
        fork.push(fresh("fork-new", "2026-10-30T22:00:00Z"));
        std::fs::write(work.join("fork1.jsonl"), text(&fork)).unwrap();
    }

    fn skipped(idx: &Index) -> i64 {
        idx.meta("skipped_records")
            .unwrap()
            .map_or(0, |v| v.parse().unwrap())
    }

    /// Marks the raw tables stale, as a new `CACHE_VERSION` does: the next open drops them.
    fn bump(path: &Path) {
        let mut idx = Index::open(path).unwrap();
        let other = (schema::CACHE_VERSION.parse::<u32>().unwrap() - 1).to_string();
        idx.set_meta("schema_version", &other).unwrap();
    }

    #[test]
    fn reread_after_compaction_counts_once() {
        let (w, path) = base();
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        compact(&w, &mut done, 14, None);
        rewrite(&w);
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        assert_eq!(skipped(&never), 0, "nothing is skipped before a compaction");
        assert!(skipped(&done) >= 20, "{} records skipped", skipped(&done));
        assert_eq!(
            count(
                &done,
                "SELECT COUNT(*) FROM requests WHERE msg_id LIKE '%-new'"
            ),
            4,
            "the new responses are read"
        );
        let expected = snapshot(&w, &never);
        assert_same(&expected, &snapshot(&w, &done), "files read again");
        // A rolled-up file read once more (another replace) still adds nothing.
        let p = proj(&w, "lab").join("s05.jsonl");
        let l = lines(&p);
        std::fs::remove_file(&p).unwrap();
        std::fs::write(&p, text(&l)).unwrap();
        refresh(&w, &mut done);
        assert_same(&expected, &snapshot(&w, &done), "read a third time");
        drop((never, done));

        // A cache rebuild drops every raw row and reads the transcripts again: the rolled-up
        // part stays in the archive and is not read twice ...
        bump(&w.dir.path().join("never.sqlite"));
        bump(&w.dir.path().join("done.sqlite"));
        let mut never = Index::open(&w.dir.path().join("never.sqlite")).unwrap();
        refresh(&w, &mut never);
        let rebuilt = snapshot(&w, &never);
        assert_same(
            &expected,
            &rebuilt,
            "the never-compacted index after a rebuild",
        );
        let mut done = Index::open(&w.dir.path().join("done.sqlite")).unwrap();
        assert_eq!(count(&done, "SELECT COUNT(*) FROM requests"), 0);
        refresh(&w, &mut done);
        assert_same(&expected, &snapshot(&w, &done), "after a rebuild");
        drop(done);
        // ... even when the transcripts of the first twenty days are gone by then (Claude Code
        // deleted them): the archive is the only record of them. (Day 5's stays: it got a new
        // response above, which is raw.)
        for d in (0..20).filter(|d| *d != 5) {
            let seat = ["work", "home", "lab"][d % 3];
            let p = proj(&w, seat).join(format!("s{d:02}.jsonl"));
            std::fs::remove_file(&p).unwrap();
            std::fs::remove_dir_all(p.with_extension("")).unwrap();
        }
        bump(&w.dir.path().join("done.sqlite"));
        let mut done = Index::open(&w.dir.path().join("done.sqlite")).unwrap();
        refresh(&w, &mut done);
        assert_same(
            &expected,
            &snapshot(&w, &done),
            "a rebuild without the old transcripts",
        );
    }

    /// RFC 3339 of `t` epoch ms.
    fn iso(t: i64) -> String {
        crate::index::from_ms(t).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// A session of `work` with a response written as Claude Code writes it, one record per
    /// content block, whose blocks lie on both sides of `cut`: a text block a minute before it,
    /// a tool call 30 s after it (same message id, same usage), the tool's result, and a
    /// response on each side.
    fn straddling(cut: i64) -> Vec<String> {
        let m = |id: &str, t: i64, tools: &[&str]| {
            assistant(id, &iso(t), "claude-opus-5-5", "/w/api", (4000, 700), tools)
        };
        vec![
            owner("acc-a"),
            m("split-0", cut - 3_600_000, &[]),
            m("split-1", cut - 60_000, &[]),
            m("split-1", cut + 30_000, &["split-t1"]),
            serde_json::json!({"type": "user", "timestamp": iso(cut + 90_000), "sessionId": "s",
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "split-t1",
                    "content": "ok", "is_error": false}]}})
            .to_string(),
            m("split-2", cut + 3_600_000, &[]),
        ]
    }

    #[test]
    fn a_response_straddling_the_cut_counts_once_when_read_again() {
        // The raw row keeps the first block's time, so the compactor rolls the whole response
        // up; read again from the start, its second block (after rolled_until) must not come
        // back as a new raw row.
        let (w, path) = base();
        let cut = compact::cutoff(at(NOW), 14);
        let lines = straddling(cut);
        std::fs::write(proj(&w, "work").join("split.jsonl"), text(&lines)).unwrap();
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        let r = compact(&w, &mut done, 14, None);
        assert_eq!(r.rolled_until, cut);
        let raw = |i: &Index| {
            count(
                i,
                "SELECT COUNT(*) FROM requests WHERE msg_id LIKE 'split-%'",
            )
        };
        assert_eq!(raw(&done), 1, "split-2 alone stays raw");
        // Replaced (another inode): read again from its start.
        let p = proj(&w, "work").join("split.jsonl");
        std::fs::remove_file(&p).unwrap();
        std::fs::write(&p, text(&lines)).unwrap();
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        assert_eq!(raw(&done), 1, "the second block is no new response");
        assert_eq!(
            count(
                &done,
                "SELECT COUNT(*) FROM tool_calls WHERE id = 'split-t1' AND done_ts IS NOT NULL"
            ),
            1,
            "its tool call (after the cut) stays raw, finished"
        );
        assert_same(
            &snapshot(&w, &never),
            &snapshot(&w, &done),
            "a straddling response read again",
        );
    }

    #[test]
    fn records_never_read_count_after_a_compaction_and_none_are_lost_silently() {
        // After a compaction, records the index never read are new information, whatever their
        // age: a session synced from another machine into a compacted seat, a subagent file of a
        // known session that was never read. Only what the archive holds is skipped (a fork, in
        // `reread_after_compaction_counts_once`).
        let (w, path) = base();
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        let r = compact(&w, &mut done, 14, None);
        let big = |id: &str, t: &str| {
            assistant(id, t, "claude-opus-5-5", "/w/api", (900_000, 9_000), &[])
        };
        let synced = [
            owner("acc-a"),
            big("sync-0", "2026-10-02T03:01:00Z"),
            big("sync-1", "2026-10-02T03:20:00Z"),
            fresh("sync-2", "2026-10-29T09:00:00Z"),
        ];
        std::fs::write(proj(&w, "work").join("synced.jsonl"), text(&synced)).unwrap();
        let sub = proj(&w, "work").join("s03/subagents/agent-zz.jsonl");
        std::fs::write(
            &sub,
            text(&[
                big("zz-0", "2026-09-24T03:05:00Z"),
                big("zz-1", "2026-09-24T03:09:00Z"),
            ]),
        )
        .unwrap();
        // A damaged line in a known file: not counted, and said so.
        let p = proj(&w, "lab").join("s35.jsonl");
        let mut l = lines(&p);
        l.push("{\"type\": \"assistant\", oops".into());
        std::fs::write(&p, text(&l)).unwrap();
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        assert_eq!(
            count(
                &done,
                &format!(
                    "SELECT COUNT(*) FROM requests WHERE ts < {}",
                    r.rolled_until
                )
            ),
            0,
            "the old ones are rolled up at once"
        );
        assert_eq!(
            count(
                &done,
                "SELECT SUM(requests) FROM usage_rollup WHERE input >= 900000"
            ),
            4
        );
        assert_same(
            &snapshot(&w, &never),
            &snapshot(&w, &done),
            "records never read before",
        );
        assert_eq!(
            count(
                &done,
                &format!(
                    "SELECT count FROM record_types WHERE type = '{}'",
                    crate::claude::transcript::MALFORMED
                )
            ),
            1
        );
        // A transcript that cannot be read is recorded and reported until it can be (Unix
        // permissions make it unreadable; elsewhere this part is skipped).
        #[cfg(unix)]
        {
            let locked = proj(&w, "home").join("locked.jsonl");
            std::fs::write(
                &locked,
                text(&[owner("acc-b"), fresh("lk-0", "2026-10-30T10:00:00Z")]),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::File::open(&locked).is_err() {
                refresh(&w, &mut done);
                let bad = done.unreadable_files().unwrap();
                assert_eq!(bad.len(), 1, "{bad:?}");
                assert!(bad[0].0.ends_with("locked.jsonl") && !bad[0].1.is_empty());
                assert_eq!(
                    count(&done, "SELECT COUNT(*) FROM requests WHERE msg_id = 'lk-0'"),
                    0
                );
            }
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        assert!(done.unreadable_files().unwrap().is_empty());
        assert_same(
            &snapshot(&w, &never),
            &snapshot(&w, &done),
            "read once it can be",
        );
    }

    #[test]
    fn a_cache_rebuild_keeps_raw_rows_no_transcript_gives_back() {
        // Raw rows are the only record of their days once the transcripts are gone, and of
        // old records found after a compaction until the next one: a cache rebuild keeps both.
        let (w, path) = base();
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        let r = compact(&w, &mut done, 14, None);
        // A response with a clock far behind, appended to a known file after the compaction:
        // read incrementally, it stays raw below rolled_until.
        let long = proj(&w, "work").join("long.jsonl");
        let mut l = lines(&long);
        l.push(fresh("behind-0", "2026-10-03T05:00:00Z"));
        std::fs::write(&long, text(&l)).unwrap();
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        let below = format!(
            "SELECT COUNT(*) FROM requests WHERE ts < {}",
            r.rolled_until
        );
        assert_eq!(count(&done, &below), 1);
        let expected = snapshot(&w, &never);
        assert_same(&expected, &snapshot(&w, &done), "before the rebuild");
        drop(done);
        // The seat `lab` is gone (its dir removed), and so are `home`'s recent transcripts.
        let lab = w.seats.iter().find(|s| s.label == "lab").unwrap();
        std::fs::remove_dir_all(&lab.dir).unwrap();
        for d in (20..40).filter(|d| d % 3 == 1) {
            let p = proj(&w, "home").join(format!("s{d:02}.jsonl"));
            std::fs::remove_file(&p).unwrap();
            std::fs::remove_dir_all(p.with_extension("")).unwrap();
        }
        bump(&w.dir.path().join("done.sqlite"));
        let mut done = Index::open(&w.dir.path().join("done.sqlite")).unwrap();
        assert!(
            count(&done, "SELECT COUNT(*) FROM requests") > 0,
            "rows no transcript gives back are kept"
        );
        refresh(&w, &mut done);
        assert_eq!(count(&done, &below), 0, "the late row is rolled up");
        assert_same(&expected, &snapshot(&w, &done), "after the rebuild");
    }

    #[test]
    fn every_kind_of_reread_after_compaction_reads_as_never_compacted() {
        // All at once, as a real index meets them: a response straddling the cut, a key with
        // priced and unpriced requests, then after the compaction files appended, replaced,
        // truncated, forked, a session and a subagent never read, an old record appended, and
        // finally a cache rebuild after a seat and recent transcripts are gone; then a second
        // compaction with a shorter raw window. Every number on every surface, by every
        // dimension, equals the never-compacted index's.
        let (w, path) = base();
        let cut = compact::cutoff(at(NOW), 14);
        std::fs::write(proj(&w, "work").join("split.jsonl"), text(&straddling(cut))).unwrap();
        let mx = |id: &str, t: &str, tok| assistant(id, t, "claude-mystery-1", "/w/api", tok, &[]);
        let mystery = [
            owner("acc-b"),
            mx("mx-0", "2026-10-04T10:01:00Z", (0, 0)),
            mx("mx-1", "2026-10-04T10:03:00Z", (900, 90)),
        ];
        std::fs::write(proj(&w, "home").join("mystery.jsonl"), text(&mystery)).unwrap();
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        let r = compact(&w, &mut done, 14, None);
        assert_eq!(r.rolled_until, cut);
        assert_same(&snapshot(&w, &never), &snapshot(&w, &done), "compacted");

        rewrite(&w);
        let split = proj(&w, "work").join("split.jsonl");
        let l = lines(&split);
        std::fs::remove_file(&split).unwrap();
        std::fs::write(&split, text(&l)).unwrap();
        let big = |id: &str, t: &str| {
            assistant(id, t, "claude-opus-5-5", "/w/api", (900_000, 9_000), &[])
        };
        std::fs::write(
            proj(&w, "lab").join("synced.jsonl"),
            text(&[owner("acc-a"), big("sync-0", "2026-10-05T03:01:00Z")]),
        )
        .unwrap();
        std::fs::write(
            proj(&w, "home").join("s04/subagents/agent-zz.jsonl"),
            text(&[big("zz-0", "2026-09-25T03:05:00Z")]),
        )
        .unwrap();
        let long = proj(&w, "work").join("long.jsonl");
        let mut l = lines(&long);
        l.push(fresh("behind-0", "2026-10-03T05:00:00Z"));
        std::fs::write(&long, text(&l)).unwrap();
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        let expected = snapshot(&w, &never);
        assert_same(
            &expected,
            &snapshot(&w, &done),
            "read again after compacting",
        );
        drop((never, done));

        let lab = w.seats.iter().find(|s| s.label == "lab").unwrap();
        std::fs::remove_dir_all(&lab.dir).unwrap();
        for d in (20..40).filter(|d| d % 3 == 1) {
            let p = proj(&w, "home").join(format!("s{d:02}.jsonl"));
            std::fs::remove_file(&p).unwrap();
            std::fs::remove_dir_all(p.with_extension("")).unwrap();
        }
        for name in ["never.sqlite", "done.sqlite"] {
            let p = w.dir.path().join(name);
            bump(&p);
            let mut idx = Index::open(&p).unwrap();
            refresh(&w, &mut idx);
            assert_same(&expected, &snapshot(&w, &idx), &format!("{name} rebuilt"));
        }
        let mut done = Index::open(&w.dir.path().join("done.sqlite")).unwrap();
        compact(&w, &mut done, 8, None);
        assert_same(&expected, &snapshot(&w, &done), "compacted again, 8 days");
    }

    #[test]
    fn requests_without_a_price_stay_counted_in_the_archive() {
        // Two responses of a model nobody priced, in one rollup key: one has no tokens (priced
        // at $0, as a synthetic record is), the other has tokens (no price known). The archive
        // keeps how many had no price, before and after a reprice.
        let (w, path) = base();
        let m = |id: &str, t: &str, tok| assistant(id, t, "claude-mystery-1", "/w/api", tok, &[]);
        let lines = [
            owner("acc-a"),
            m("mx-0", "2026-10-02T10:01:00Z", (0, 0)),
            m("mx-1", "2026-10-02T10:03:00Z", (900, 90)),
        ];
        std::fs::write(proj(&w, "work").join("mystery.jsonl"), text(&lines)).unwrap();
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        compact(&w, &mut done, 14, None);
        let span = Period::Range(day("2026-10-02"), day("2026-10-02")).span(at(NOW), None);
        let f = Filter {
            model: Some("mystery".into()),
            ..Default::default()
        };
        let unpriced = |i: &Index| i.total(span, &f).unwrap().unpriced;
        let expected = unpriced(&never);
        assert!(expected >= 1);
        assert_eq!(unpriced(&done), expected, "compacted");
        assert_same(&snapshot(&w, &never), &snapshot(&w, &done), "compacted");
        // A price change elsewhere reprices every rollup row from its sums: still no price for
        // the model, still the same count.
        let mut book = PriceBook::builtin();
        book.push_override(
            "claude-sonnet-5-5",
            Some(day("2026-10-01")),
            4.0,
            20.0,
            &RateOverrides::default(),
            day("2026-10-30"),
        )
        .unwrap();
        for i in [&mut never, &mut done] {
            i.price_all(&book).unwrap();
        }
        assert_eq!(unpriced(&done), expected, "repriced");
        assert_same(&snapshot(&w, &never), &snapshot(&w, &done), "repriced");
    }

    #[test]
    fn a_seat_found_after_compaction_is_counted_once_and_rolled_up_at_once() {
        // The seat `late` was not there when the compaction ran; its transcripts, a month old,
        // are new information: read into raw rows, and the refresh rolls the old ones into the
        // buckets that exist right away, so a cache rebuild before the next compaction (which
        // skips records older than rolled_until of known sessions) loses nothing.
        let (w, path) = base();
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        let r = done
            .compact(&CompactOpts {
                keep_raw_days: 14,
                now: at(NOW),
                prices: &PriceBook::builtin(),
                seats: &w.seats[..3],
                max_chunks: None,
            })
            .unwrap();
        let mut l = vec![owner("acc-b")];
        for (k, t) in [
            "2026-10-01T10:01:00Z",
            "2026-10-01T10:05:00Z",
            "2026-10-29T09:00:00Z",
        ]
        .iter()
        .enumerate()
        {
            l.push(fresh(&format!("late-{k}"), t));
        }
        std::fs::write(proj(&w, "late").join("z1.jsonl"), text(&l)).unwrap();
        let rolled = |i: &Index| count(i, "SELECT SUM(requests) FROM usage_rollup");
        let before = rolled(&done);
        refresh(&w, &mut never);
        refresh(&w, &mut done);
        assert_eq!(skipped(&done), 0);
        let below = format!(
            "SELECT COUNT(*) FROM requests WHERE ts < {}",
            r.rolled_until
        );
        assert_eq!(count(&done, &below), 0, "nothing raw below rolled_until");
        assert_eq!(
            rolled(&done),
            before + 2,
            "the two old responses are rolled up"
        );
        assert_eq!(
            count(
                &done,
                "SELECT COUNT(*) FROM requests WHERE msg_id = 'late-2'"
            ),
            1,
            "the recent one stays raw"
        );
        assert_eq!(
            archive::rolled_until(&done.conn).unwrap(),
            r.rolled_until,
            "rolled_until never moves"
        );
        let expected = snapshot(&w, &never);
        assert_same(&expected, &snapshot(&w, &done), "a late seat");
        let again = compact(&w, &mut done, 14, None);
        assert_eq!(again.requests, 0, "nothing left for the next compaction");
        drop(done);
        // A cache rebuild before any further compaction: the late seat's old records are in the
        // archive, read again and skipped, counted once.
        bump(&w.dir.path().join("done.sqlite"));
        let mut done = Index::open(&w.dir.path().join("done.sqlite")).unwrap();
        refresh(&w, &mut done);
        assert_same(
            &expected,
            &snapshot(&w, &done),
            "a late seat, then a rebuild",
        );
    }

    fn day(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn reprice_after_compaction() {
        // A new Sonnet price from a day long rolled up, a price for the model nobody priced, and
        // Haiku's long-context multipliers changed (same threshold): the compacted index reprices
        // its rollups from their token sums to what the never-compacted one gets request by
        // request.
        let (w, path) = base();
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        let r = compact(&w, &mut done, 14, None);
        let unpriced = |i: &Index| count(i, "SELECT COUNT(*) FROM usage_rollup WHERE priced = 0");
        assert!(
            unpriced(&done) > 0,
            "the mystery model is rolled up unpriced"
        );
        let today = day("2026-10-30");
        let mut book = PriceBook::builtin();
        book.push_override(
            "claude-sonnet-5-5",
            Some(day("2026-10-05")),
            4.0,
            20.0,
            &RateOverrides::default(),
            today,
        )
        .unwrap();
        book.push_override(
            "claude-mystery-1",
            None,
            1.0,
            5.0,
            &RateOverrides::default(),
            today,
        )
        .unwrap();
        let haiku = |from: &str, threshold: u64| {
            let mut rates = PriceBook::builtin()
                .lookup("claude-haiku-5-5", today)
                .unwrap()
                .rates;
            rates.long_context = Some(LongContext {
                threshold,
                input_multiplier: 3.0,
                output_multiplier: 2.0,
            });
            PriceEntry {
                model: "claude-haiku-5-5".into(),
                from: Some(day(from)),
                rates,
                source: PriceSource::User,
            }
        };
        book.push(haiku("2026-09-25", 100_000));
        let spend = |i: &Index| {
            i.total(Period::All.span(at(NOW), None), &Filter::default())
                .unwrap()
                .cost
        };
        let before = spend(&never);
        for i in [&mut never, &mut done] {
            i.price_all(&book).unwrap();
        }
        assert!(spend(&never) > before * 1.01, "the prices moved the total");
        assert_eq!(
            unpriced(&done),
            0,
            "a price that appears later prices old rows"
        );
        assert_same(
            &snapshot(&w, &never),
            &snapshot(&w, &done),
            "repriced after compaction",
        );
        assert!(done.band_drift(&book).unwrap().is_empty());
        // A later version moves Haiku's threshold: rows rolled up under the old one keep their
        // band, and `doctor` lists the days.
        book.push(haiku("2026-10-08", 50_000));
        done.price_all(&book).unwrap();
        let drift = done.band_drift(&book).unwrap();
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert_eq!(drift[0].model, "claude-haiku-5-5");
        assert!(drift[0].first >= day("2026-10-08"), "{drift:?}");
        let last_rolled = crate::index::from_ms(r.rolled_until - 1)
            .with_timezone(&chrono::Local)
            .date_naive();
        assert!(
            drift[0].last <= last_rolled && drift[0].rows > 0,
            "{drift:?}"
        );
    }

    /// The seats' logins: `work` logged into `work_account` at `since`, the others as ever.
    fn logins(work_account: &str, since: &str) -> Vec<SeatInfo> {
        [
            ("work", work_account, since),
            ("home", "acc-b", "2026-09-01T00:00:00Z"),
            ("lab", "acc-a", "2026-09-01T00:00:00Z"),
        ]
        .into_iter()
        .map(|(label, acc, since)| SeatInfo {
            seat: Default::default(),
            label: label.into(),
            account: Some(account(acc, since)),
            utilization: None,
        })
        .collect()
    }

    /// Review H1: swarmy was off for weeks and finds `work` on another login (a vault switch
    /// restored a profile fetched on 10-10). The change is dated when it is seen, so no request
    /// and no rollup before it moves: the owners in the transcripts decided those, and the
    /// archive is never re-keyed on the seat's word. Compacted and never compacted still agree.
    #[test]
    fn a_login_change_found_after_compaction_moves_nothing() {
        let w = World::new(&["work", "home", "lab", "late"]);
        populate(&w);
        let book = PriceBook::builtin();
        let path = w.dir.path().join("base.sqlite");
        let mut idx = Index::open(&path).unwrap();
        w.refresh(&mut idx, &logins("acc-a", "2026-09-01T00:00:00Z"), &book);
        readings(&idx);
        drop(idx);
        let mut never = copy(&w, &path, "never.sqlite");
        let mut done = copy(&w, &path, "done.sqlite");
        let r = compact(&w, &mut done, 14, None);
        assert!(at("2026-10-10T12:00:00Z").timestamp_millis() < r.rolled_until);
        let accounts = |i: &Index| {
            crate::index::scenarios::dump(
                i,
                "SELECT seat, session_id, agent_id, first_ts, account_uuid FROM usage_rollup ORDER BY 1, 2, 3, 4, 5",
            )
        };
        let rolled = accounts(&done);
        let raw = |i: &Index| {
            crate::index::scenarios::dump(i, "SELECT msg_id, account_uuid FROM requests ORDER BY 1")
        };
        let raw_before = raw(&never);
        for i in [&mut never, &mut done] {
            w.refresh(i, &logins("acc-b", "2026-10-10T12:00:00Z"), &book);
        }
        assert_eq!(accounts(&done), rolled, "no rollup moved");
        assert_eq!(raw(&never), raw_before, "no request moved");
        let seen = count(
            &done,
            &format!(
                "SELECT MAX(ts) FROM seat_accounts WHERE seat = '{}' AND account_uuid = 'acc-b'",
                w.id("work")
            ),
        );
        assert!(
            seen > r.rolled_until,
            "dated when seen, not at the profile's time"
        );
        assert_same(
            &snapshot(&w, &never),
            &snapshot(&w, &done),
            "a login change found after compaction",
        );
    }

    #[test]
    fn projects_come_from_the_union_and_are_never_downgraded() {
        let (w, path) = base();
        let mut idx = copy(&w, &path, "p.sqlite");
        let r = compact(&w, &mut idx, 14, None);
        let until = r.rolled_until / BUCKET_MS;
        let long_cwd: String = idx
            .conn
            .query_row(
                "SELECT cwd FROM sessions WHERE session_id = 'long'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        idx.conn
            .execute_batch(&format!(
                "-- A worktree of the shop only rolled-up days name, and a scratch dir of the long
                 -- session (it runs in {long_cwd}) only the archive remembers.
                 UPDATE usage_rollup SET cwd = '/w/shop/wt-old' WHERE cwd = '/w/shop' AND bucket < {until} - 400;
                 UPDATE usage_rollup SET cwd = '/w/scratch' WHERE session_id = 'long' AND cwd != '{long_cwd}';
                 -- A repository gone since its project was recorded, and a wrong row.
                 INSERT OR REPLACE INTO projects VALUES ('/w/api', '/r/api', 'api-repo', NULL);
                 INSERT OR REPLACE INTO projects VALUES ('/w/docs', 'x', 'x', NULL);"
            ))
            .unwrap();
        for c in ["/w/shop/wt-old", "/w/scratch"] {
            let raw = format!("SELECT COUNT(*) FROM requests WHERE cwd = '{c}'");
            let rolled = format!("SELECT COUNT(*) FROM usage_rollup WHERE cwd = '{c}'");
            assert!(count(&idx, &raw) == 0 && count(&idx, &rolled) > 0, "{c}");
        }
        // A new revision of the rules recomputes every cwd.
        idx.set_meta("projects_rev", "0").unwrap();
        refresh(&w, &mut idx);
        let name = |cwd: &str| -> String {
            idx.conn
                .query_row("SELECT name FROM projects WHERE cwd = ?1", [cwd], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        assert_eq!(name("/w/shop/wt-old"), "shop");
        assert_eq!(name("/w/scratch"), name(&long_cwd), "the session's project");
        assert_eq!(name("/w/docs"), "docs");
        assert_eq!(name("/w/api"), "api-repo", "never downgraded");
    }

    #[test]
    fn space_is_given_back_once_the_index_is_incremental() {
        use crate::index::compact::{FileStats, VacuumSkip, vacuum_verdict};
        let f = |pages, free_pages, incremental| FileStats {
            page_size: 4096,
            pages,
            free_pages,
            incremental,
        };
        // The dashboard: never twice, on mains only, once a quarter of the file is free, with
        // room for the copies (2 × 400 MB + 64 MB); the user asking skips the first two rules.
        let big = f(100_000, 30_000, false);
        assert_eq!(
            vacuum_verdict(&f(10, 9, true), false, None, true),
            Err(VacuumSkip::Done)
        );
        assert_eq!(
            vacuum_verdict(&big, true, None, false),
            Err(VacuumSkip::OnBattery)
        );
        assert_eq!(
            vacuum_verdict(&f(100_000, 20_000, false), false, None, false),
            Err(VacuumSkip::FewFreePages)
        );
        let need = 2 * 409_600_000 + (64 << 20);
        assert_eq!(
            vacuum_verdict(&big, false, Some(need - 1), false),
            Err(VacuumSkip::Disk {
                need,
                free: need - 1
            })
        );
        assert_eq!(vacuum_verdict(&big, false, Some(need), false), Ok(()));
        assert_eq!(
            vacuum_verdict(&f(100_000, 0, false), true, None, true),
            Ok(())
        );

        let (w, path) = base();
        let mut idx = copy(&w, &path, "v.sqlite");
        let r = compact(&w, &mut idx, 14, None);
        let st = idx.file_stats().unwrap();
        assert!(!st.incremental && st.free_pages > 0, "{st:?}");
        assert_eq!(
            r.freed_bytes, 0,
            "without auto-vacuum the pages stay in the file"
        );
        let before = snapshot(&w, &idx);
        // On battery it waits, and says why.
        assert_eq!(
            idx.vacuum_to_incremental(true, None, false).unwrap(),
            Err(VacuumSkip::OnBattery)
        );
        assert_eq!(
            idx.vacuum_skipped().unwrap().as_deref(),
            Some("waits for mains power")
        );
        let shrank = idx
            .vacuum_to_incremental(false, None, true)
            .unwrap()
            .unwrap();
        assert!(shrank >= st.free_bytes(), "{shrank} < {}", st.free_bytes());
        let st = idx.file_stats().unwrap();
        assert!(st.incremental && st.free_pages == 0, "{st:?}");
        assert_eq!(idx.vacuum_skipped().unwrap(), None);
        assert_eq!(
            idx.vacuum_to_incremental(false, None, true).unwrap(),
            Err(VacuumSkip::Done)
        );
        assert_same(&before, &snapshot(&w, &idx), "after the VACUUM");
        // From now on a compaction gives its pages back to the file system: none stay free in
        // the file (without auto-vacuum they did, above). Whether the file ends up smaller
        // depends on how the rollups it added fill their pages, which varies with the seat ids
        // (hashes of the temp dirs), so the freelist is what is checked.
        let r = compact(&w, &mut idx, 8, None);
        assert!(r.chunks > 0, "{r:?}");
        assert_eq!(idx.file_stats().unwrap().free_pages, 0);
    }
}

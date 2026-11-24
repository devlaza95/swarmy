//! The compactor: raw rows older than the raw window move into the archive's quarter-hour
//! rollups (`archive`, `docs/design/storage.md` §4.1).
//!
//! It works oldest first in chunks of [`CHUNK_MS`], each chunk one immediate transaction that
//! adds the chunk's rows to the rollups and deletes them, so every raw row is either raw or
//! rolled up, never both and never neither. A run that is killed loses only the open chunk; a
//! second run finds nothing older than its cutoff and changes nothing.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Duration, Local, Utc};
use rusqlite::{Connection, TransactionBehavior, named_params, params};

use super::archive::{self, MIN_RAW_DAYS, file_hash};
use super::insights::command_key;
use super::{Index, from_ms, ms, schema};
use crate::claude::seats::{Seat, SeatId};
use crate::core::period::{BUCKET_MS, local_midnight};
use crate::core::pricing::PriceBook;
use crate::core::storage::{TOOL_ROLLUP_BYTES, USAGE_ROLLUP_BYTES, raw_bytes};
use crate::core::tools::ToolClass;

/// One chunk: six UTC hours, a whole number of quarter hours.
pub const CHUNK_MS: i64 = 6 * 3600 * 1000;

/// What to compact.
pub struct CompactOpts<'a> {
    /// Days of raw detail to keep (at least [`MIN_RAW_DAYS`]): everything before the local
    /// midnight that many days before `now` is rolled up.
    pub keep_raw_days: i64,
    /// The clock (never later than the real one: the raw window is measured from it).
    pub now: DateTime<Utc>,
    /// The prices the index is priced with: they give each model's long-context threshold.
    pub prices: &'a PriceBook,
    /// The seats whose transcripts are indexed now (`rollup_seats`, see `marks`).
    pub seats: &'a [Seat],
    /// At most this many chunks in this run (the dashboard catches up a day at a time).
    pub max_chunks: Option<usize>,
}

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct CompactReport {
    /// Rows before this (epoch ms) are rolled up by this run.
    pub cutoff: i64,
    /// Chunks committed.
    pub chunks: usize,
    pub requests: u64,
    pub tool_calls: u64,
    pub prompts: u64,
    pub api_errors: u64,
    /// `owners` rows of finished sessions dropped (the newest per session stays).
    pub owners_pruned: u64,
    /// `files` rows of transcripts that are gone.
    pub files_pruned: u64,
    /// `meta.rolled_until` after the run.
    pub rolled_until: i64,
    /// The start of a chunk that waits for its requests to be priced (`price_all` on the next
    /// refresh); nothing at or after it was rolled up.
    pub waiting: Option<i64>,
    /// Whether nothing older than the cutoff is left raw (false: stopped by `max_chunks` or by a
    /// chunk that waits).
    pub done: bool,
    /// Bytes the database file shrank by when the space was given back (`Index::free_space`).
    pub freed_bytes: u64,
}

/// The tables whose rows the compactor rolls up, all with a `ts` and an index on it.
const ROLLED: [&str; 4] = ["requests", "tool_calls", "prompts", "api_errors"];

#[cfg(test)]
thread_local! {
    /// Test hook: the chunk (1-based) that fails after its inserts, before its deletes.
    pub(crate) static FAIL_IN_CHUNK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The largest rowid `requests` has ever had. `write_file` numbers new rows from above it, and
/// the compactor records it before deleting rows, so a rowid is never given out twice and the
/// rows a refresh adds are exactly those above the value it read first (`post_pass`).
pub(super) fn requests_rowid_high(conn: &Connection) -> rusqlite::Result<i64> {
    conn.prepare_cached(
        "SELECT MAX(COALESCE((SELECT MAX(rowid) FROM requests), 0),
                    COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'requests_rowid_high'), 0))",
    )?
    .query_row([], |r| r.get(0))
}

/// The local midnight `keep_raw_days` days before `now`'s local day.
pub fn cutoff(now: DateTime<Utc>, keep_raw_days: i64) -> i64 {
    let today = now.with_timezone(&Local).date_naive();
    ms(local_midnight(today - Duration::days(keep_raw_days)))
}

/// The oldest raw row of any rolled table, when it is older than `before`.
fn oldest_raw(conn: &Connection, before: i64) -> rusqlite::Result<Option<i64>> {
    let mut oldest: Option<i64> = None;
    for t in ROLLED {
        let m: Option<i64> = conn
            .prepare_cached(&format!("SELECT MIN(ts) FROM {t}"))?
            .query_row([], |r| r.get(0))?;
        oldest = match (oldest, m) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    Ok(oldest.filter(|t| *t < before))
}

/// The long-context threshold of `model`'s price on the local day of `t` (0: none).
fn lc_threshold(book: &PriceBook, model: &str, t: i64) -> i64 {
    lc_threshold_on(book, model, from_ms(t).with_timezone(&Local).date_naive())
}

/// The long-context threshold of `model`'s price on `day` (0: none).
pub(super) fn lc_threshold_on(book: &PriceBook, model: &str, day: chrono::NaiveDate) -> i64 {
    book.lookup(model, day)
        .and_then(|e| e.rates.long_context.as_ref())
        .map_or(0, |l| l.threshold as i64)
}

/// `[c0, c1)` split at the local midnight inside it, if any: each part lies in one local day,
/// so one price entry applies to each model in it.
fn day_parts(c0: i64, c1: i64) -> Vec<(i64, i64)> {
    let day = from_ms(c0).with_timezone(&Local).date_naive();
    let next = ms(local_midnight(day + Duration::days(1)));
    if next > c0 && next < c1 {
        vec![(c0, next), (next, c1)]
    } else {
        vec![(c0, c1)]
    }
}

/// Counts of one chunk.
#[derive(Default)]
struct Moved {
    requests: u64,
    tool_calls: u64,
    prompts: u64,
    api_errors: u64,
    owners: u64,
}

/// Tool calls of one quarter hour, tool and command: (calls, errors, busy ms).
type ToolSums = HashMap<(i64, String, String), (i64, i64, i64)>;

impl Index {
    /// Rolls every raw row older than the cutoff up into the archive, oldest first, one chunk per
    /// transaction (see the module doc). Reads that start before `meta.rolled_until` take the
    /// rollups into account (`archive::plan`), so no number changes. Then the space is given back
    /// (`free_space`).
    pub fn compact(&mut self, opts: &CompactOpts) -> anyhow::Result<CompactReport> {
        self.compact_with(opts, &mut |_| {})
    }

    /// [`Index::compact`], telling `progress` after each chunk.
    pub fn compact_with(
        &mut self,
        opts: &CompactOpts,
        progress: &mut dyn FnMut(&CompactReport),
    ) -> anyhow::Result<CompactReport> {
        self.compact_untyped(opts, progress).map_err(super::typed)
    }

    fn compact_untyped(
        &mut self,
        opts: &CompactOpts,
        progress: &mut dyn FnMut(&CompactReport),
    ) -> anyhow::Result<CompactReport> {
        anyhow::ensure!(
            opts.keep_raw_days >= MIN_RAW_DAYS,
            "keep_raw_days must be at least {MIN_RAW_DAYS} (the current weekly window stays raw)"
        );
        let started = std::time::Instant::now();
        let now = opts.now.min(Utc::now());
        let cut = cutoff(now, opts.keep_raw_days);
        let mut report = self.roll(cut, opts.prices, opts.seats, opts.max_chunks, progress)?;
        report.files_pruned = self.prune_files(report.rolled_until)?;
        if report.chunks > 0 || report.files_pruned > 0 {
            report.freed_bytes = self.free_space()?;
        }
        if report.chunks > 0 {
            self.record_compaction(&report, started.elapsed().as_millis() as u64)?;
        }
        self.sample_storage(now.with_timezone(&Local).date_naive())?;
        Ok(report)
    }

    /// Rolls up raw rows older than `rolled_until` at once: the records of a seat found after a
    /// compaction (new information, read into raw rows, `marks`). Left raw, a cache rebuild before
    /// the next compaction would skip them as rolled up (their session is known by then) and lose
    /// them. The refresh calls it when it read a file from its start; a no-op check otherwise
    /// (four indexed `MIN(ts)`). Returns the requests rolled up.
    pub(super) fn roll_late(&mut self, prices: &PriceBook, seats: &[Seat]) -> anyhow::Result<u64> {
        let until = archive::rolled_until(&self.conn)?;
        if until == 0 || oldest_raw(&self.conn, until)?.is_none() {
            return Ok(0);
        }
        Ok(self.roll(until, prices, seats, None, &mut |_| {})?.requests)
    }

    /// The chunks from the oldest raw row up to `cut`.
    fn roll(
        &mut self,
        cut: i64,
        prices: &PriceBook,
        seats: &[Seat],
        max_chunks: Option<usize>,
        progress: &mut dyn FnMut(&CompactReport),
    ) -> anyhow::Result<CompactReport> {
        let book = self.dated_prices(prices, self.now())?;
        let seat_dirs: Vec<(String, SeatId)> = seats
            .iter()
            .map(|s| (canonical(&s.dir), s.id.clone()))
            .collect();
        let mut report = CompactReport {
            cutoff: cut,
            ..Default::default()
        };
        loop {
            if max_chunks.is_some_and(|m| report.chunks >= m) {
                break;
            }
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(oldest) = oldest_raw(&tx, cut)? else {
                // Nothing raw is left before the cutoff: everything before it is rolled up, so
                // the mark goes to the cutoff itself, wherever the last non-empty chunk ended
                // (otherwise it depends on how the chunk grid meets local midnight).
                let until = archive::rolled_until(&tx)?.max(cut);
                schema::set_meta(&tx, "rolled_until", &until.to_string())?;
                tx.commit()?;
                report.done = true;
                break;
            };
            let c0 = oldest - oldest.rem_euclid(CHUNK_MS);
            let c1 = (c0 + CHUNK_MS).min(cut);
            let pending = tx
                .prepare_cached(
                    "SELECT 1 FROM requests WHERE ts >= ?1 AND ts < ?2 AND priced IS NULL LIMIT 1",
                )?
                .exists([c0, c1])?;
            if pending {
                // Pending rows are never rolled up: their cost is not known yet.
                report.waiting = Some(c0);
                break;
            }
            let m = roll_chunk(&tx, &book, c0, c1)?;
            #[cfg(test)]
            if FAIL_IN_CHUNK.get() == Some(report.chunks + 1) {
                anyhow::bail!("test fault in chunk {}", report.chunks + 1);
            }
            // The seats whose rows this chunk moves, whichever this process found: a seat seen
            // only through another run's processes must be known as rolled up too, or a fork of
            // one of its old sessions counts its copied messages again (review B-M6).
            tx.execute(
                "INSERT INTO rollup_seats(seat_dir, seat)
                 SELECT st.dir, st.seat FROM seats st
                 WHERE st.dir IS NOT NULL
                   AND st.seat IN (SELECT DISTINCT seat FROM requests WHERE ts >= ?1 AND ts < ?2)
                 ON CONFLICT(seat_dir) DO UPDATE SET seat = excluded.seat",
                params![c0, c1],
            )?;
            let m = Moved {
                owners: delete_chunk(&tx, c0, c1)?,
                ..m
            };
            for (dir, id) in &seat_dirs {
                tx.execute(
                    "INSERT INTO rollup_seats(seat_dir, seat) VALUES (?1, ?2)
                     ON CONFLICT(seat_dir) DO UPDATE SET seat = excluded.seat",
                    params![dir, id],
                )?;
            }
            let until = archive::rolled_until(&tx)?.max(c1);
            schema::set_meta(&tx, "rolled_until", &until.to_string())?;
            tx.commit()?;
            report.chunks += 1;
            report.requests += m.requests;
            report.tool_calls += m.tool_calls;
            report.prompts += m.prompts;
            report.api_errors += m.api_errors;
            report.owners_pruned += m.owners;
            progress(&report);
        }
        report.rolled_until = archive::rolled_until(&self.conn)?;
        Ok(report)
    }

    /// Drops `files` rows of transcripts that no longer exist and hold nothing newer than
    /// `rolled_until`: nothing is left to read of them. A file that comes back is read from the
    /// start, and its rolled-up records are skipped (`marks`).
    fn prune_files(&mut self, rolled_until: i64) -> anyhow::Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Vec<String> = tx
            .prepare("SELECT path FROM files WHERE last_ts < ?1")?
            .query_map([rolled_until], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut n = 0;
        for p in old.iter().filter(|p| !Path::new(p).exists()) {
            n += tx.execute("DELETE FROM files WHERE path = ?1", [p])? as u64;
        }
        tx.commit()?;
        Ok(n)
    }
}

/// Free pages are given back to the file system this many at a time (16 MB of 4 KB pages), each
/// step its own short write transaction.
const VACUUM_STEP_PAGES: i64 = 4096;

/// The one-time `VACUUM` writes a copy of the database and, through the WAL, the database again:
/// it needs twice the file free on disk, and this much more.
const VACUUM_DISK_MARGIN: u64 = 64 << 20;

/// The dashboard switches to incremental auto-vacuum on its own once this share of the file is
/// free pages (a compaction left them); `swarmy compact --vacuum` does it at any time.
const VACUUM_FREE_SHARE: f64 = 0.25;

/// The database file in pages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct FileStats {
    pub page_size: u64,
    pub pages: u64,
    /// Pages in the free list: inside the file, reused by new rows.
    pub free_pages: u64,
    /// `auto_vacuum = INCREMENTAL`: free pages can be given back to the file system.
    pub incremental: bool,
}

impl FileStats {
    pub fn bytes(&self) -> u64 {
        self.pages * self.page_size
    }

    pub fn free_bytes(&self) -> u64 {
        self.free_pages * self.page_size
    }
}

/// Why the one-time `VACUUM` to incremental auto-vacuum did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VacuumSkip {
    /// The index is incremental already.
    Done,
    /// It waits for mains power (the dashboard on battery).
    OnBattery,
    /// Less than [`VACUUM_FREE_SHARE`] of the file is free: not worth rewriting it yet.
    FewFreePages,
    /// Not enough free disk next to the index.
    Disk { need: u64, free: u64 },
}

impl VacuumSkip {
    /// For `doctor` (stored in `meta.vacuum_skipped`) and the CLI.
    pub fn describe(&self) -> String {
        let mb = |b: u64| b as f64 / 1e6;
        match self {
            VacuumSkip::Done => "auto-vacuum is incremental already".into(),
            VacuumSkip::OnBattery => "waits for mains power".into(),
            VacuumSkip::FewFreePages => "less than a quarter of the file is free".into(),
            VacuumSkip::Disk { need, free } => format!(
                "needs {:.0} MB free next to the index, {:.0} MB are",
                mb(*need),
                mb(*free)
            ),
        }
    }
}

/// Whether the one-time `VACUUM` that switches the index to incremental auto-vacuum runs now:
/// never twice, only with room for its copies on disk (`free_disk` unknown: assumed enough), and,
/// unless the user `asked` for it, only on mains power and once a compaction left a quarter of
/// the file free (design §4.2).
pub fn vacuum_verdict(
    f: &FileStats,
    on_battery: bool,
    free_disk: Option<u64>,
    asked: bool,
) -> Result<(), VacuumSkip> {
    if f.incremental {
        return Err(VacuumSkip::Done);
    }
    if !asked && on_battery {
        return Err(VacuumSkip::OnBattery);
    }
    if !asked && (f.free_pages as f64) < VACUUM_FREE_SHARE * f.pages as f64 {
        return Err(VacuumSkip::FewFreePages);
    }
    let need = 2 * f.bytes() + VACUUM_DISK_MARGIN;
    match free_disk {
        Some(free) if free < need => Err(VacuumSkip::Disk { need, free }),
        Some(_) | None => Ok(()),
    }
}

/// Free bytes on the disk that holds `path` (`None`: unknown).
pub fn free_disk(path: &Path) -> Option<u64> {
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else {
        return None;
    };
    disks
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// What a compaction would do now (`swarmy compact --dry-run`): nothing is written.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Preview {
    pub cutoff: i64,
    pub requests: u64,
    pub tool_calls: u64,
    pub prompts: u64,
    pub api_errors: u64,
    /// Requests not priced yet: their chunk waits (`price_all` on the next refresh).
    pub pending: u64,
    /// Rollup rows the requests and tool calls would become (at least; the long-context band can
    /// split a few more).
    pub usage_rows: u64,
    pub tool_rows: u64,
    /// Bytes the raw rows take, less the rollups they become (measured bytes per row).
    pub bytes: u64,
    pub file: FileStats,
}

impl Index {
    /// The database file in pages.
    pub fn file_stats(&self) -> anyhow::Result<FileStats> {
        let n = |p: &str| -> rusqlite::Result<u64> {
            self.conn
                .query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0))
                .map(|v| v as u64)
        };
        Ok(FileStats {
            page_size: n("page_size")?,
            pages: n("page_count")?,
            free_pages: n("freelist_count")?,
            incremental: n("auto_vacuum")? == 2,
        })
    }

    /// Gives free pages back to the file system (with incremental auto-vacuum; without it they
    /// stay in the free list and new rows reuse them), then checkpoints the WAL and truncates it
    /// when no reader holds it (`journal_size_limit` caps it otherwise). Returns the bytes the
    /// file shrank by.
    pub fn free_space(&mut self) -> anyhow::Result<u64> {
        let before = self.file_stats()?;
        if before.incremental {
            let mut free = before.free_pages;
            while free > 0 {
                self.conn
                    .execute_batch(&format!("PRAGMA incremental_vacuum({VACUUM_STEP_PAGES})"))?;
                let now = self.file_stats()?.free_pages;
                if now >= free {
                    break;
                }
                free = now;
            }
        }
        self.checkpoint()?;
        Ok(before.bytes().saturating_sub(self.file_stats()?.bytes()))
    }

    /// Copies the WAL into the database; truncates it when that copied everything, waiting at
    /// most a second for readers (the dashboard reads all the time).
    fn checkpoint(&mut self) -> anyhow::Result<()> {
        let (log, done): (i64, i64) =
            self.conn
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                    Ok((r.get(1)?, r.get(2)?))
                })?;
        if log > 0 && log == done {
            self.conn.busy_timeout(std::time::Duration::from_secs(1))?;
            let truncated = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    r.get::<_, i64>(0)
                });
            self.conn.busy_timeout(super::BUSY_TIMEOUT)?;
            truncated?;
        }
        Ok(())
    }

    /// The one-time `VACUUM` that switches the index to incremental auto-vacuum, when
    /// [`vacuum_verdict`] allows it; a skip is recorded for `doctor` (`meta.vacuum_skipped`).
    /// Returns the bytes the file shrank by. The copy goes to SQLite's temp dir, which swarmy
    /// points into its data dir (`use_temp_dir`), never a RAM-backed `/tmp`.
    pub fn vacuum_to_incremental(
        &mut self,
        on_battery: bool,
        free_disk: Option<u64>,
        asked: bool,
    ) -> anyhow::Result<Result<u64, VacuumSkip>> {
        let before = self.file_stats()?;
        if let Err(skip) = vacuum_verdict(&before, on_battery, free_disk, asked) {
            if skip != VacuumSkip::Done {
                self.set_meta("vacuum_skipped", &skip.describe())?;
            }
            return Ok(Err(skip));
        }
        self.conn
            .execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
        self.conn
            .execute("DELETE FROM meta WHERE key = 'vacuum_skipped'", [])?;
        self.checkpoint()?;
        Ok(Ok(before
            .bytes()
            .saturating_sub(self.file_stats()?.bytes())))
    }

    /// A consistent copy of the index at `path` (`VACUUM INTO`, while others may write): what
    /// `swarmy compact --verify` works on.
    pub fn copy_to(&self, path: &Path) -> anyhow::Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", [path.to_string_lossy()])?;
        Ok(())
    }

    /// Why the one-time `VACUUM` last did not run, if it has not run yet.
    pub fn vacuum_skipped(&self) -> anyhow::Result<Option<String>> {
        if self.file_stats()?.incremental {
            return Ok(None);
        }
        self.meta("vacuum_skipped")
    }

    /// What [`Index::compact`] with `keep_raw_days` would roll up now, read without writing.
    pub fn compact_preview(
        &self,
        keep_raw_days: i64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Preview> {
        anyhow::ensure!(
            keep_raw_days >= MIN_RAW_DAYS,
            "keep_raw_days must be at least {MIN_RAW_DAYS} (the current weekly window stays raw)"
        );
        let cut = cutoff(now.min(Utc::now()), keep_raw_days);
        let n = |sql: &str| -> rusqlite::Result<u64> {
            self.conn
                .query_row(sql, [cut], |r| r.get::<_, i64>(0))
                .map(|v| v as u64)
        };
        let keys: Vec<&str> = archive::KEYS
            .iter()
            .filter(|k| !k.from_raw.contains(":lc"))
            .map(|k| k.from_raw)
            .collect();
        let mut p = Preview {
            cutoff: cut,
            requests: n("SELECT COUNT(*) FROM requests WHERE ts < ?1")?,
            tool_calls: n("SELECT COUNT(*) FROM tool_calls WHERE ts < ?1")?,
            prompts: n("SELECT COUNT(*) FROM prompts WHERE ts < ?1")?,
            api_errors: n("SELECT COUNT(*) FROM api_errors WHERE ts < ?1")?,
            pending: n("SELECT COUNT(*) FROM requests WHERE ts < ?1 AND priced IS NULL")?,
            usage_rows: n(&format!(
                "SELECT COUNT(*) FROM (SELECT 1 FROM requests WHERE ts < ?1 GROUP BY {})",
                keys.join(", ")
            ))?,
            tool_rows: n(&format!(
                "SELECT COUNT(*) FROM (SELECT 1 FROM tool_calls WHERE ts < ?1 GROUP BY ts / {BUCKET_MS}, name)"
            ))?,
            file: self.file_stats()?,
            ..Default::default()
        };
        p.bytes = raw_bytes(p.requests, p.tool_calls)
            .saturating_sub(p.usage_rows * USAGE_ROLLUP_BYTES + p.tool_rows * TOOL_ROLLUP_BYTES);
        Ok(p)
    }
}

/// A config dir as `rollup_seats` stores it: canonical when it can be resolved.
pub(super) fn canonical(dir: &Path) -> String {
    crate::fsutil::canonical(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Adds the raw rows of `[c0, c1)` to the rollups (requests by the generated insert, per model
/// and local day with its long-context threshold; tool calls by command key; edited files by
/// hash; prompts and errors as counts) and to `agent_rollup`.
fn roll_chunk(tx: &Connection, book: &PriceBook, c0: i64, c1: i64) -> anyhow::Result<Moved> {
    let models: Vec<String> = tx
        .prepare_cached("SELECT DISTINCT model FROM requests WHERE ts >= ?1 AND ts < ?2")?
        .query_map([c0, c1], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut insert = tx.prepare_cached(&archive::rollup_insert_sql(
        "ts >= :a AND ts < :b AND model = :model",
    ))?;
    for model in &models {
        let mut parts: Vec<(i64, i64, i64)> = Vec::new();
        for (a, b) in day_parts(c0, c1) {
            let lc = lc_threshold(book, model, a);
            match parts.last_mut() {
                Some(last) if last.2 == lc => last.1 = b,
                Some(_) | None => parts.push((a, b, lc)),
            }
        }
        for (a, b, lc) in parts {
            insert.execute(named_params! {":a": a, ":b": b, ":model": model, ":lc": lc})?;
        }
    }
    // An agent's rolled-up lifetime grows by the chunk's requests.
    tx.prepare_cached(
        "INSERT INTO agent_rollup(seat, session_id, agent_id, requests, cost, unpriced, first_ts, last_ts, first_model, last_model)
         SELECT seat, session_id, agent_id, COUNT(*), TOTAL(cost), SUM(priced = 0), MIN(ts), MAX(ts),
           (SELECT x.model FROM requests x WHERE x.seat = r.seat AND x.session_id = r.session_id AND x.agent_id = r.agent_id
              AND x.ts >= ?1 AND x.ts < ?2 ORDER BY x.ts ASC LIMIT 1),
           (SELECT x.model FROM requests x WHERE x.seat = r.seat AND x.session_id = r.session_id AND x.agent_id = r.agent_id
              AND x.ts >= ?1 AND x.ts < ?2 ORDER BY x.ts DESC LIMIT 1)
         FROM requests r WHERE ts >= ?1 AND ts < ?2 GROUP BY seat, session_id, agent_id
         ON CONFLICT(seat, session_id, agent_id) DO UPDATE SET
           requests = requests + excluded.requests, cost = cost + excluded.cost, unpriced = unpriced + excluded.unpriced,
           first_model = CASE WHEN excluded.first_ts < first_ts THEN excluded.first_model ELSE first_model END,
           last_model = CASE WHEN excluded.last_ts >= last_ts THEN excluded.last_model ELSE last_model END,
           first_ts = MIN(first_ts, excluded.first_ts), last_ts = MAX(last_ts, excluded.last_ts)",
    )?
    .execute([c0, c1])?;
    let tool_calls = roll_tools(tx, c0, c1)?;
    let prompts = tx
        .prepare_cached(&format!(
            "INSERT INTO prompt_rollup(bucket, prompts, chars)
             SELECT ts / {BUCKET_MS}, COUNT(*), SUM(chars) FROM prompts WHERE ts >= ?1 AND ts < ?2 GROUP BY 1
             ON CONFLICT(bucket) DO UPDATE SET prompts = prompts + excluded.prompts, chars = chars + excluded.chars"
        ))?
        .execute([c0, c1])?;
    let api_errors = tx
        .prepare_cached(&format!(
            "INSERT INTO api_error_rollup(bucket, kind, n)
             SELECT ts / {BUCKET_MS}, kind, COUNT(*) FROM api_errors WHERE ts >= ?1 AND ts < ?2 GROUP BY 1, 2
             ON CONFLICT(bucket, kind) DO UPDATE SET n = n + excluded.n"
        ))?
        .execute([c0, c1])?;
    let count = |t: &str| -> rusqlite::Result<u64> {
        tx.prepare_cached(&format!(
            "SELECT COUNT(*) FROM {t} WHERE ts >= ?1 AND ts < ?2"
        ))?
        .query_row([c0, c1], |r| r.get::<_, i64>(0))
        .map(|n| n as u64)
    };
    Ok(Moved {
        requests: count("requests")?,
        tool_calls,
        // Rows the inserts touched are buckets; the counts are of raw rows.
        prompts: if prompts > 0 { count("prompts")? } else { 0 },
        api_errors: if api_errors > 0 {
            count("api_errors")?
        } else {
            0
        },
        owners: 0,
    })
}

/// The chunk's tool calls into `tool_rollup` (per quarter hour, tool and command key of a shell
/// command) and the edited files into `edit_rollup`. Busy time sums `done_ts - ts` of finished
/// calls, as `efficiency` does. Returns the number of calls.
fn roll_tools(tx: &Connection, c0: i64, c1: i64) -> anyhow::Result<u64> {
    let mut sums: ToolSums = HashMap::new();
    let mut edits: Vec<(i64, i64)> = Vec::new();
    let mut n = 0;
    {
        let mut sel = tx.prepare_cached(
            "SELECT ts, name, summary, target, done_ts, is_error FROM tool_calls WHERE ts >= ?1 AND ts < ?2",
        )?;
        let mut rows = sel.query([c0, c1])?;
        while let Some(r) = rows.next()? {
            let t: i64 = r.get(0)?;
            let name: String = r.get(1)?;
            let summary: Option<String> = r.get(2)?;
            let target: Option<String> = r.get(3)?;
            let done: Option<i64> = r.get(4)?;
            let err: i64 = r.get(5)?;
            let class = ToolClass::of(&name);
            let cmd = if let (Some(s), ToolClass::Shell) = (&summary, class) {
                command_key(s)
            } else {
                String::new()
            };
            let bucket = t.div_euclid(BUCKET_MS);
            if let (ToolClass::Edit, Some(target)) = (class, &target) {
                edits.push((bucket, file_hash(target)));
            }
            let e = sums.entry((bucket, name, cmd)).or_default();
            e.0 += 1;
            e.1 += err;
            e.2 += done.map_or(0, |d| (d - t).max(0));
            n += 1;
        }
    }
    let mut up = tx.prepare_cached(
        "INSERT INTO tool_rollup(bucket, name, cmd, calls, errors, busy_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(bucket, name, cmd) DO UPDATE SET calls = calls + excluded.calls,
           errors = errors + excluded.errors, busy_ms = busy_ms + excluded.busy_ms",
    )?;
    for ((bucket, name, cmd), (calls, errors, busy)) in &sums {
        up.execute(params![bucket, name, cmd, calls, errors, busy])?;
    }
    let mut edit =
        tx.prepare_cached("INSERT OR IGNORE INTO edit_rollup(bucket, file_h) VALUES (?1, ?2)")?;
    for (bucket, h) in edits {
        edit.execute(params![bucket, h])?;
    }
    Ok(n)
}

/// Deletes the chunk's raw rows (after recording the rowid high mark, see
/// [`requests_rowid_high`]), and the `owners` rows of sessions finished before the chunk's end
/// except the newest of each (all a later request of the session can need). Returns the number
/// of `owners` rows dropped.
fn delete_chunk(tx: &Connection, c0: i64, c1: i64) -> anyhow::Result<u64> {
    let high = requests_rowid_high(tx)?;
    schema::set_meta(tx, "requests_rowid_high", &high.to_string())?;
    for t in ROLLED {
        tx.prepare_cached(&format!("DELETE FROM {t} WHERE ts >= ?1 AND ts < ?2"))?
            .execute([c0, c1])?;
    }
    let owners = tx
        .prepare_cached(
            "DELETE FROM owners WHERE (seat, session_id) IN
               (SELECT seat, session_id FROM agents GROUP BY seat, session_id HAVING MAX(last_ts) < ?1)
             AND ts < (SELECT MAX(o.ts) FROM owners o WHERE o.seat = owners.seat AND o.session_id = owners.session_id)",
        )?
        .execute([c1])?;
    Ok(owners as u64)
}

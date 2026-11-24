//! The index's own size (`docs/design/storage.md` §5): a sample a day for the growth, and the
//! figures `doctor` and the SYSTEM panel show. Sizes come from `PRAGMA page_count` and row
//! counts times measured bytes per row (`core::storage`), never a table walk.

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::compact::{CompactReport, FileStats};
use super::{Index, ms};
use crate::core::storage::{self, Outlook, Sample};

/// What the last compaction that rolled something up did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastCompaction {
    /// When it ended (epoch ms).
    pub at: i64,
    pub chunks: usize,
    pub requests: u64,
    pub tool_calls: u64,
    /// Wall time it took.
    pub ms: u64,
}

/// The index's size and what is in it.
#[derive(Debug, Clone, Default)]
pub struct StorageReport {
    pub file: FileStats,
    pub wal_bytes: u64,
    pub raw_requests: u64,
    pub raw_tool_calls: u64,
    /// The oldest raw request (epoch ms).
    pub oldest_raw: Option<i64>,
    pub usage_rows: u64,
    pub tool_rows: u64,
    pub edit_rows: u64,
    /// The first quarter hour in the rollups (epoch ms).
    pub archive_since: Option<i64>,
    pub rolled_until: i64,
    /// Daily samples, oldest first (the last 30).
    pub samples: Vec<Sample>,
    pub last_compaction: Option<LastCompaction>,
    /// Records found again after they were rolled up, and skipped (`marks`), all time.
    pub skipped_records: u64,
}

impl StorageReport {
    /// Bytes the rollups take (measured bytes per row).
    pub fn rollup_bytes(&self) -> u64 {
        self.usage_rows * storage::USAGE_ROLLUP_BYTES
            + self.tool_rows * storage::TOOL_ROLLUP_BYTES
            + self.edit_rows * storage::EDIT_ROLLUP_BYTES
    }

    /// Days the raw rows span up to `now` (at least one).
    pub fn raw_days(&self, now: DateTime<Utc>) -> f64 {
        self.oldest_raw
            .map_or(1.0, |t| ((ms(now) - t) as f64 / 86_400_000.0).max(1.0))
    }

    /// Where the index is going with a raw window of `keep_raw_days`.
    pub fn outlook(&self, now: DateTime<Utc>, keep_raw_days: i64) -> Outlook {
        storage::outlook(
            self.file.bytes(),
            self.file.free_bytes(),
            self.raw_requests,
            self.raw_tool_calls,
            self.raw_days(now),
            keep_raw_days,
            &self.samples,
        )
    }
}

fn day_number(d: NaiveDate) -> i64 {
    i64::from(d.num_days_from_ce())
}

impl Index {
    /// Records today's sample once (the first call of the day wins). Its size is the bytes in
    /// use, the file less its free pages: those are reused before the file grows.
    pub fn sample_storage(&self, today: NaiveDate) -> anyhow::Result<()> {
        let f = self.file_stats()?;
        let used = f.bytes() - f.free_bytes();
        self.conn.execute(
            "INSERT OR IGNORE INTO storage_samples(day, file_bytes, raw_requests, raw_tool_calls, rollup_rows)
             VALUES (?1, ?2, (SELECT COUNT(*) FROM requests), (SELECT COUNT(*) FROM tool_calls),
                     (SELECT COUNT(*) FROM usage_rollup) + (SELECT COUNT(*) FROM tool_rollup) + (SELECT COUNT(*) FROM edit_rollup))",
            rusqlite::params![today.to_string(), used as i64],
        )?;
        Ok(())
    }

    /// Remembers a compaction that rolled something up, for `doctor`.
    pub(super) fn record_compaction(
        &mut self,
        r: &CompactReport,
        took_ms: u64,
    ) -> anyhow::Result<()> {
        let last = LastCompaction {
            at: ms(self.now()),
            chunks: r.chunks,
            requests: r.requests,
            tool_calls: r.tool_calls,
            ms: took_ms,
        };
        self.set_meta("last_compaction", &serde_json::to_string(&last)?)
    }

    /// The index's size and contents now (a few counts over indexes: run it rarely).
    pub fn storage_report(&self) -> anyhow::Result<StorageReport> {
        let n = |sql: &str| -> rusqlite::Result<u64> {
            self.conn
                .query_row(sql, [], |r| r.get::<_, i64>(0))
                .map(|v| v as u64)
        };
        // No WAL file (checkpointed and truncated, or an in-memory index): nothing in it.
        let wal_bytes = match self
            .conn
            .path()
            .map(|p| std::fs::metadata(format!("{p}-wal")))
        {
            Some(Ok(m)) => m.len(),
            Some(Err(_)) | None => 0,
        };
        let mut samples: Vec<Sample> = self
            .conn
            .prepare(
                "SELECT day, file_bytes, raw_requests, raw_tool_calls FROM storage_samples
                 ORDER BY day DESC LIMIT 30",
            )?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Sample {
                        day: 0,
                        file_bytes: r.get::<_, i64>(1)? as u64,
                        raw_requests: r.get::<_, i64>(2)? as u64,
                        raw_tool_calls: r.get::<_, i64>(3)? as u64,
                    },
                ))
            })?
            .map(|row| {
                let (day, s) = row?;
                Ok(Sample {
                    day: day_number(day.parse()?),
                    ..s
                })
            })
            .collect::<anyhow::Result<_>>()?;
        samples.reverse();
        let last_compaction = self
            .meta("last_compaction")?
            .map(|j| serde_json::from_str(&j))
            .transpose()?;
        Ok(StorageReport {
            file: self.file_stats()?,
            wal_bytes,
            raw_requests: n("SELECT COUNT(*) FROM requests")?,
            raw_tool_calls: n("SELECT COUNT(*) FROM tool_calls")?,
            oldest_raw: self
                .conn
                .query_row("SELECT MIN(ts) FROM requests", [], |r| r.get(0))?,
            usage_rows: n("SELECT COUNT(*) FROM usage_rollup")?,
            tool_rows: n("SELECT COUNT(*) FROM tool_rollup")?,
            edit_rows: n("SELECT COUNT(*) FROM edit_rollup")?,
            archive_since: self
                .conn
                .query_row("SELECT MIN(bucket) FROM usage_rollup", [], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .optional()?
                .flatten()
                .map(|b| b * crate::core::period::BUCKET_MS),
            rolled_until: super::archive::rolled_until(&self.conn)?,
            samples,
            last_compaction,
            skipped_records: self.meta("skipped_records")?.map_or(Ok(0), |v| v.parse())?,
        })
    }
}

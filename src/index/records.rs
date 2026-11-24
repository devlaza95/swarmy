//! What the index says about its own reading: counts, cross-checks against Claude Code's own
//! totals, files it cannot read, record types per harness (format drift, `doctor`) and API
//! errors.

use chrono::{DateTime, Utc};

use super::archive;
use super::query::Counted;
use super::{Index, ms};
use crate::core::harness::Harness;
use crate::core::period::Span;

impl Index {
    /// Claude Code's own totals vs. the transcript sums, over sessions that have both:
    /// (sessions, claude code USD, transcript USD).
    /// A session's transcript sum covers its rolled-up part too: sessions resumed after weeks
    /// compare in full.
    pub fn cost_crosscheck(&self, span: Span) -> anyhow::Result<(u64, f64, f64)> {
        let source = if archive::rolled_until(&self.conn)? > 0 {
            "(SELECT seat, session_id, cost, ts FROM requests
              UNION ALL SELECT seat, session_id, cost, last_ts FROM usage_rollup)"
        } else {
            "requests"
        };
        let sql = format!(
            "SELECT COUNT(*), SUM(cs.total_usd), SUM(t.cost) FROM cost_states cs
             JOIN (SELECT seat, session_id, TOTAL(cost) AS cost, MAX(ts) AS last FROM {source} GROUP BY 1, 2) t
               ON t.seat = cs.seat AND t.session_id = cs.session_id
             WHERE t.last >= ?1 AND t.last < ?2"
        );
        Ok(self.conn.query_row(
            &sql,
            rusqlite::params![
                ms(span.start.max(DateTime::<Utc>::UNIX_EPOCH)),
                ms(span.end)
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                ))
            },
        )?)
    }

    /// The rows of one table (a table walk: for `doctor` and tests, never a per-second path).
    pub fn count(&self, t: Counted) -> anyhow::Result<u64> {
        Ok(self
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {}", t.table()), [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
    }

    /// Transcripts the last refresh that tried could not read, whose file is still there: (path,
    /// error, since when in epoch ms), oldest first.
    pub fn unreadable_files(&self) -> anyhow::Result<Vec<(String, String, i64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT path, error, since FROM unreadable_files ORDER BY since, path",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let all: Vec<(String, String, i64)> = rows.collect::<Result<_, _>>()?;
        Ok(all
            .into_iter()
            .filter(|(p, ..)| std::path::Path::new(p).exists())
            .collect())
    }

    pub fn record_types(&self) -> anyhow::Result<Vec<(String, u64)>> {
        let mut st = self
            .conn
            .prepare("SELECT type, SUM(count) FROM record_types GROUP BY 1 ORDER BY 2 DESC")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// [`Index::record_types`] per harness of the file's seat (`claude` for a seat no longer
    /// listed), so a type is judged by the reader that met it (review B-M10: `doctor` let any
    /// harness's known type pass for every file).
    pub fn record_types_by_harness(&self) -> anyhow::Result<Vec<(Harness, String, u64)>> {
        let mut st = self.conn.prepare(
            "SELECT COALESCE(s.harness, 'claude'), r.type, SUM(r.count) FROM record_types r
               LEFT JOIN files f ON f.path = r.path LEFT JOIN seats s ON s.seat = f.seat
             GROUP BY 1, 2 ORDER BY 3 DESC",
        )?;
        let rows = st.query_map([], |r| {
            let h: String = r.get(0)?;
            Ok((
                Harness::from_key(&h).unwrap_or(Harness::Claude),
                r.get(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn api_errors(&self, span: Span) -> anyhow::Result<Vec<(String, u64)>> {
        archive::read(&self.conn, span, false, self.now(), |c, p| {
            let (b0, b1) = p.buckets();
            let sql = if p.archive {
                format!(
                    "SELECT kind, SUM(n) FROM (SELECT kind, 1 AS n FROM api_errors WHERE ts >= ?1 AND ts < ?2
                       UNION ALL SELECT kind, n FROM api_error_rollup WHERE bucket >= {b0} AND bucket < {b1})
                     GROUP BY 1 ORDER BY 2 DESC, 1"
                )
            } else {
                "SELECT kind, COUNT(*) FROM api_errors WHERE ts >= ?1 AND ts < ?2 GROUP BY 1 ORDER BY 2 DESC, 1".into()
            };
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(rusqlite::params![p.start, p.end], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? as u64))
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }
}

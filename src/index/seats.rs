//! Seat identities in the index (review H3). Every `seat` column holds a [`SeatId`], derived from
//! the seat's config dir; the `seats` table names each id for display (its latest label, its dir).
//! A label change therefore changes no row: attribution (`seat_accounts`), sessions, agents and
//! rollups stay with the dir.
//!
//! Older indexes keyed every table by the seat's label. `FACTS_VERSION` 2 lists those labels in
//! `seats` as legacy entries (no dir); [`Index::adopt_seats`] moves a legacy label's rows to the
//! id of the seat that carries that label now, the first time such a seat is seen. A label no
//! current seat carries stays as it is (shown by its label), and is adopted when a seat with that
//! label turns up (a seat found only through a running `claude`, for example). Nothing is lost.

use rusqlite::{Connection, params};

use super::{Index, ms};
use crate::claude::seats::{Seat, SeatId};
use crate::core::harness::Harness;

/// Tables with a `seat` column that holds a seat (all but `seats` itself).
pub(crate) fn seat_tables(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             AND name != 'seats' ORDER BY name",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for t in tables {
        let has_seat = conn
            .prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = 'seat'")?
            .exists([&t])?;
        if has_seat {
            out.push(t);
        }
    }
    Ok(out)
}

/// `FACTS_VERSION` 1 → 2: every seat value that is not an id is a label written by an older
/// swarmy; each becomes a legacy entry of `seats` (label, no dir) until a seat adopts it.
pub(super) fn list_legacy_labels(conn: &Connection) -> rusqlite::Result<usize> {
    let mut labels = std::collections::BTreeSet::new();
    for t in seat_tables(conn)? {
        let mut st = conn.prepare(&format!(
            "SELECT DISTINCT seat FROM {t} WHERE seat IS NOT NULL"
        ))?;
        for v in st.query_map([], |r| r.get::<_, String>(0))? {
            let v = v?;
            if !SeatId::looks_like_id(&v) {
                labels.insert(v);
            }
        }
    }
    for l in &labels {
        conn.execute(
            "INSERT INTO seats(seat, label, dir, seen_at) VALUES (?1, ?1, NULL, 0) ON CONFLICT(seat) DO NOTHING",
            [l],
        )?;
    }
    Ok(labels.len())
}

impl Index {
    /// Names the current seats in `seats` (label and dir, for display), and moves the rows of a
    /// legacy label (see the module docs) to the id of the seat that carries that label now.
    /// Runs inside every refresh; does nothing when the seats are the ones it saw last.
    pub(super) fn adopt_seats(&mut self, seats: &[Seat]) -> anyhow::Result<()> {
        let sig: Vec<(SeatId, String, Harness)> = seats
            .iter()
            .map(|s| (s.id.clone(), s.label.clone(), s.harness))
            .collect();
        if self.adopted.as_ref() == Some(&sig) {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let legacy: Vec<String> = tx
            .prepare_cached("SELECT seat FROM seats WHERE dir IS NULL")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut moved = 0;
        for s in seats {
            if !legacy.contains(&s.label) {
                continue;
            }
            let mut left = 0;
            for t in seat_tables(&tx)? {
                // A row whose key the id already has (nothing an older swarmy wrote) stays under
                // the label rather than being dropped.
                moved += tx.execute(
                    &format!("UPDATE OR IGNORE {t} SET seat = ?1 WHERE seat = ?2"),
                    params![s.id, s.label],
                )?;
                left += tx.query_row(
                    &format!("SELECT COUNT(*) FROM {t} WHERE seat = ?1"),
                    [&s.label],
                    |r| r.get::<_, i64>(0),
                )?;
            }
            if left == 0 {
                tx.execute(
                    "DELETE FROM seats WHERE seat = ?1 AND dir IS NULL",
                    [&s.label],
                )?;
            }
        }
        let now = ms(chrono::Utc::now());
        for s in seats {
            tx.execute(
                "INSERT INTO seats(seat, label, dir, harness, seen_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(seat) DO UPDATE SET label = excluded.label, dir = excluded.dir, harness = excluded.harness, seen_at = excluded.seen_at",
                params![s.id, s.label, s.dir.to_string_lossy(), s.harness.key(), now],
            )?;
        }
        if moved > 0 {
            // Requests moved between seat keys: settled spend per seat changed.
            super::bump_settled(&tx)?;
        }
        tx.commit()?;
        self.adopted = Some(sig);
        Ok(())
    }

    /// The label to show for each seat id the index knows (current seats under their current
    /// label, gone ones under the last label seen, legacy labels as themselves).
    pub fn seat_labels(&self) -> anyhow::Result<Vec<(SeatId, String)>> {
        Ok(self
            .conn
            .prepare_cached("SELECT seat, label FROM seats ORDER BY label")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    /// The seats of which the index has read a transcript: those it reads now (`files`) and
    /// those whose transcripts are gone but whose sessions it keeps (`sessions` outlives them).
    /// A few thousand rows: read on the analytics cadence, never per tick.
    pub fn seats_with_transcripts(&self) -> anyhow::Result<std::collections::BTreeSet<SeatId>> {
        Ok(self
            .conn
            .prepare_cached("SELECT seat FROM files UNION SELECT seat FROM sessions")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }
}

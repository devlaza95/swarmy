//! Prices in the index: dating Claude Code's cached prices, and pricing every request.

use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::archive::KEYS;
use super::{Index, from_ms, ms, schema};
use crate::core::ids::SeatId;
use crate::core::period::{BUCKET_MS, local_midnight};
use crate::core::pricing::{CostParts, PriceBook, PriceEntry, PriceSource, Rates};
use crate::core::usage::Usage;

impl Index {
    /// `book` with Claude Code's prices dated. Each version of a model's cached price is recorded
    /// in `observed_prices` (a fact table) the first time it is seen, and applies from that day
    /// on, so a price change in the cache never reprices the past. The first version of a model
    /// also covers the days before it was seen when nothing else prices the model then (a model
    /// newer than the built-in table), so it is never left unpriced.
    pub fn dated_prices(
        &mut self,
        book: &PriceBook,
        now: DateTime<Utc>,
    ) -> anyhow::Result<PriceBook> {
        // The book and what `observed_prices` holds: another process may have dated a version
        // this one has not seen (review B-L15: the memo kept the book alone, and two processes
        // took turns repricing).
        let key = |c: &Connection| -> rusqlite::Result<String> {
            let (n, last): (i64, i64) = c.query_row(
                "SELECT COUNT(*), COALESCE(MAX(seen_at), 0) FROM observed_prices",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(format!("{}|{n}|{last}", book.fingerprint()))
        };
        let before = key(&self.conn)?;
        if let Some((k, dated)) = &self.dated
            && *k == before
        {
            return Ok(dated.clone());
        }
        let today = now.with_timezone(&Local).date_naive();
        // Claude Code's entries per model, in seat order.
        let mut current: Vec<(&str, Vec<(&PriceEntry, &SeatId)>)> = Vec::new();
        for (e, seat) in book.entries().iter().filter_map(|e| match &e.source {
            PriceSource::ClaudeCode(seat) => Some((e, seat)),
            PriceSource::Builtin | PriceSource::User => None,
        }) {
            match current.iter_mut().find(|(m, _)| *m == e.model) {
                Some((_, v)) => v.push((e, seat)),
                None => current.push((e.model.as_str(), vec![(e, seat)])),
            }
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (model, versions) in &current {
            let latest: Option<String> = tx
                .query_row(
                    "SELECT rates FROM observed_prices WHERE model = ?1 ORDER BY seen_at DESC LIMIT 1",
                    [model],
                    |r| r.get(0),
                )
                .optional()?;
            let latest: Option<Rates> = latest.map(|j| serde_json::from_str(&j)).transpose()?;
            // Seats may carry different versions of the cache; any of them still current is
            // enough, so two seats never take turns.
            if latest.is_some_and(|l| versions.iter().any(|(e, _)| e.rates == l)) {
                continue;
            }
            let (first, seat) = versions[0];
            tx.execute(
                "INSERT INTO observed_prices(model, seen_at, seen_on, seat, rates) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(model, seen_at) DO NOTHING",
                params![model, ms(now), today.to_string(), seat, serde_json::to_string(&first.rates)?],
            )?;
        }
        let mut observed = Vec::new();
        {
            // Newest first, so a version seen later the same day wins the tie on `from`.
            let mut st = tx.prepare(
                "SELECT model, seen_on, seat, rates,
                        seen_at = (SELECT MIN(o.seen_at) FROM observed_prices o WHERE o.model = p.model)
                 FROM observed_prices p ORDER BY model, seen_at DESC",
            )?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, SeatId>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })?;
            for row in rows {
                let (model, seen_on, seat, rates, first) = row?;
                let seen: NaiveDate = seen_on.parse()?;
                let from = if first && !book.priced_without_cache(&model, seen) {
                    None
                } else {
                    Some(seen)
                };
                observed.push(PriceEntry {
                    model,
                    from,
                    rates: serde_json::from_str(&rates)?,
                    source: PriceSource::ClaudeCode(seat),
                });
            }
        }
        tx.commit()?;
        let dated = book.with_observed(observed);
        // Keyed by what the table holds now, this pass's own rows included.
        self.dated = Some((key(&self.conn)?, dated.clone()));
        Ok(dated)
    }

    /// Prices new requests, and every request when any price changed. One transaction: other
    /// readers see either the old costs or the new ones, never a table half reset.
    pub fn price_all(&mut self, prices: &PriceBook) -> anyhow::Result<()> {
        let book = self.dated_prices(prices, self.now())?;
        let fp = book.fingerprint();
        // Nothing to do (the usual case, every second in the dashboard): no write at all.
        let pending = |c: &rusqlite::Connection| {
            c.prepare_cached("SELECT 1 FROM requests WHERE priced IS NULL LIMIT 1")?
                .exists([])
        };
        if self.meta("price_fp")?.as_deref() == Some(fp.as_str()) && !pending(&self.conn)? {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stale = schema::meta(&tx, "price_fp")?.as_deref() != Some(fp.as_str());
        if stale {
            super::bump_settled(&tx)?;
            tx.execute(
                &format!("UPDATE requests SET {}", schema::unpriced_sql()),
                [],
            )?;
        }
        {
            let mut sel = tx.prepare("SELECT msg_id, model, ts, fast, input, cw5, cw1h, cread, output, thinking, web FROM requests WHERE priced IS NULL")?;
            let mut upd = tx.prepare(&format!(
                "UPDATE requests SET cost = ?1, priced = ?2, {} WHERE msg_id = ?3",
                schema::COST_PARTS
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{c} = ?{}", i + 4))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))?;
            let rows: Vec<(String, String, i64, bool, Usage)> = sel
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get::<_, i64>(3)? != 0,
                        Usage {
                            input: r.get::<_, i64>(4)? as u64,
                            cache_write_5m: r.get::<_, i64>(5)? as u64,
                            cache_write_1h: r.get::<_, i64>(6)? as u64,
                            cache_read: r.get::<_, i64>(7)? as u64,
                            output: r.get::<_, i64>(8)? as u64,
                            thinking: r.get::<_, i64>(9)? as u64,
                            web_search: r.get::<_, i64>(10)? as u64,
                        },
                    ))
                })?
                .collect::<Result<_, _>>()?;
            for (id, model, ts, fast, usage) in rows {
                let day = from_ms(ts).with_timezone(&Local).date_naive();
                let p = book.cost_parts(&model, day, &usage, fast);
                let part = cost_parts(p);
                upd.execute(params![
                    p.map(|p| p.total().get()),
                    p.is_some() as i64,
                    id,
                    part[0],
                    part[1],
                    part[2],
                    part[3],
                    part[4],
                    part[5]
                ])?;
            }
        }
        if stale {
            // The archive in the same transaction: no reader sees raw and rolled-up spend at
            // different prices.
            reprice_rollups(&tx, &book)?;
            schema::set_meta(&tx, "price_fp", &fp)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Rolled-up spend whose long-context band was taken against another threshold than the
    /// one the prices in force give its model on its day (design §3.6): such rows are priced by
    /// their old band, the one inexact case of repricing. (model, first day, last day, rows),
    /// by model and first day.
    pub fn band_drift(&mut self, prices: &PriceBook) -> anyhow::Result<Vec<BandDrift>> {
        let book = self.dated_prices(prices, self.now())?;
        let mut st = self.conn.prepare(
            "SELECT model, bucket, lc_threshold, COUNT(*) FROM usage_rollup GROUP BY model, bucket, lc_threshold",
        )?;
        let mut rows = st.query([])?;
        let mut out: Vec<BandDrift> = Vec::new();
        while let Some(r) = rows.next()? {
            let (model, bucket, was, n): (String, i64, i64, i64) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            let day = from_ms(bucket * BUCKET_MS)
                .with_timezone(&Local)
                .date_naive();
            if super::compact::lc_threshold_on(&book, &model, day) == was {
                continue;
            }
            match out.iter_mut().find(|d| d.model == model) {
                Some(d) => {
                    d.first = d.first.min(day);
                    d.last = d.last.max(day);
                    d.rows += n as u64;
                }
                None => out.push(BandDrift {
                    model,
                    first: day,
                    last: day,
                    rows: n as u64,
                }),
            }
        }
        out.sort_by(|a, b| (&a.model, a.first).cmp(&(&b.model, b.first)));
        Ok(out)
    }
}

/// Rolled-up rows of one model priced by a long-context band another threshold decided.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BandDrift {
    pub model: String,
    pub first: NaiveDate,
    pub last: NaiveDate,
    pub rows: u64,
}

/// Prices every rollup row again from its token sums (design §3.6), at the price in force for
/// its model on its local day (a quarter hour lies in one local day in every zone), in the band
/// its `long_ctx` flag records: cost is linear in the tokens within a band, so the sums cost what
/// their requests would. A model without a price stays unpriced; one that got a price is priced.
/// Then `agent_rollup` takes the new costs. One local day at a time, so memory stays small.
fn reprice_rollups(tx: &Connection, book: &PriceBook) -> anyhow::Result<()> {
    let (lo, hi): (Option<i64>, Option<i64>) = tx.query_row(
        "SELECT MIN(bucket), MAX(bucket) FROM usage_rollup",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (Some(lo), Some(hi)) = (lo, hi) else {
        return Ok(());
    };
    let pk: Vec<&str> = KEYS.iter().map(|k| k.col).collect();
    let at = |c: &str| pk.iter().position(|k| *k == c);
    let (Some(i_model), Some(i_fast), Some(i_long)) = (at("model"), at("fast"), at("long_ctx"))
    else {
        anyhow::bail!("usage_rollup lost a key column");
    };
    let mut sel = tx.prepare(&format!(
        "SELECT {}, input, cw5, cw1h, cread, output, thinking, web, requests, n_empty FROM usage_rollup
         WHERE bucket >= ?1 AND bucket < ?2",
        pk.join(", ")
    ))?;
    let parts = schema::COST_PARTS;
    let mut upd = tx.prepare(&format!(
        "UPDATE usage_rollup SET cost = ?1, priced = ?2, n_unpriced = ?3, {} WHERE {}",
        parts
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c} = ?{}", i + 4))
            .collect::<Vec<_>>()
            .join(", "),
        pk.iter()
            .enumerate()
            .map(|(i, c)| format!("{c} = ?{}", i + 4 + parts.len()))
            .collect::<Vec<_>>()
            .join(" AND ")
    ))?;
    let n = pk.len();
    let mut day = from_ms(lo * BUCKET_MS).with_timezone(&Local).date_naive();
    loop {
        let (a, b) = (
            ms(local_midnight(day)).div_euclid(BUCKET_MS),
            ms(local_midnight(day + Duration::days(1))).div_euclid(BUCKET_MS),
        );
        if a > hi {
            break;
        }
        let rows: Vec<(Vec<rusqlite::types::Value>, Usage, i64, i64)> = sel
            .query_map([a, b], |r| {
                let key = (0..n)
                    .map(|i| r.get::<_, rusqlite::types::Value>(i))
                    .collect::<Result<Vec<_>, _>>()?;
                let tok =
                    |i: usize| -> rusqlite::Result<u64> { Ok(r.get::<_, i64>(n + i)? as u64) };
                Ok((
                    key,
                    Usage {
                        input: tok(0)?,
                        cache_write_5m: tok(1)?,
                        cache_write_1h: tok(2)?,
                        cache_read: tok(3)?,
                        output: tok(4)?,
                        thinking: tok(5)?,
                        web_search: tok(6)?,
                    },
                    r.get(n + 7)?,
                    r.get(n + 8)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        for (key, usage, requests, empty) in rows {
            let int = |i: usize| matches!(key[i], rusqlite::types::Value::Integer(1));
            let rusqlite::types::Value::Text(model) = &key[i_model] else {
                anyhow::bail!("a rollup row without a model");
            };
            let mut p = book.cost_parts_in_band(model, day, &usage, int(i_fast), int(i_long));
            // Without a price, the requests without tokens still cost $0 each, as they do raw
            // (`PriceBook::cost_parts`): the row's cost is known (0) when it has any.
            let unpriced = if p.is_some() { 0 } else { requests - empty };
            if p.is_none() && empty > 0 {
                p = Some(CostParts::default());
            }
            let mut values: Vec<rusqlite::types::Value> = vec![
                p.map(|p| p.total().get()).into(),
                i64::from(p.is_some()).into(),
                unpriced.into(),
            ];
            values.extend(cost_parts(p).iter().map(|v| (*v).into()));
            values.extend(key);
            upd.execute(params_from_iter(values))?;
        }
        day += Duration::days(1);
    }
    tx.execute_batch(
        "UPDATE agent_rollup SET cost = s.cost, unpriced = s.unpriced
         FROM (SELECT seat, session_id, agent_id, TOTAL(cost) AS cost,
                      SUM(n_unpriced) AS unpriced
               FROM usage_rollup GROUP BY seat, session_id, agent_id) AS s
         WHERE agent_rollup.seat = s.seat AND agent_rollup.session_id = s.session_id
           AND agent_rollup.agent_id = s.agent_id",
    )?;
    Ok(())
}

/// The parts of a cost in the order of `schema::COST_PARTS` (NULL without a price).
fn cost_parts(p: Option<CostParts>) -> [Option<f64>; 6] {
    let v = p.map(|p| {
        [
            p.input,
            p.cache_write,
            p.cache_read,
            p.output,
            p.web,
            p.cache_saved,
        ]
        .map(f64::from)
    });
    std::array::from_fn(|i| v.map(|v| v[i]))
}

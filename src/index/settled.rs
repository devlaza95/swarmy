//! Settled parts of recurring sums, remembered between the dashboard's passes.
//!
//! Spend older than a few minutes changes only when the index says so: a late request, a
//! re-attribution or a repricing bumps the settled generation (`super::bump_settled`), new prices
//! change the price fingerprint, both kept in `meta` (so another process's changes count too).
//! The dashboard sums the same long spans again and again: the period every two minutes, today
//! every second, a limit window every minute. Such a span is cut at the settled horizon, a UTC
//! quarter hour at least a quarter hour old (well past `super::SETTLED_AFTER_MS`); the part before
//! it is read once per (settled generation, price fingerprint, horizon) and kept, the part after
//! it is read every time. Both parts begin and end on the span's own ends or on that quarter
//! hour, so rolled-up spend (kept per quarter hour) splits exactly, and every sum that is a sum
//! of rows adds up to what one read of the whole span gives.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, params};

use super::query::{BucketRow, Cell, Row};
use super::{Index, ms, schema};
use crate::core::period::{Span, quarter_floor};

/// What a settled part was read under: (settled generation, price fingerprint).
type Stamp = (Option<String>, Option<String>);

/// A pair of names and how many requests have it (`model_substitutions`).
type Count = (String, String, u64);

/// A kept part: what it was read under, and the value.
type Kept = (Stamp, Box<dyn Any>);

/// At most this many settled parts are kept (a pass reads about a dozen per horizon).
const MAX_PARTS: usize = 64;

#[derive(Default)]
pub(super) struct Parts {
    map: RefCell<HashMap<String, Kept>>,
}

impl Parts {
    fn get<T: Clone + 'static>(&self, key: &str, stamp: &Stamp) -> Option<T> {
        let m = self.map.borrow();
        let (s, v) = m.get(key)?;
        if s != stamp {
            return None;
        }
        v.downcast_ref::<T>().cloned()
    }

    fn put<T: Clone + 'static>(&self, key: String, stamp: Stamp, v: T) {
        let mut m = self.map.borrow_mut();
        if m.len() >= MAX_PARTS {
            m.clear();
        }
        m.insert(key, (stamp, Box::new(v)));
    }
}

/// The settled horizon at `now`: the quarter hour before the current one.
pub(super) fn horizon(now: DateTime<Utc>) -> DateTime<Utc> {
    quarter_floor(now) - Duration::minutes(15)
}

/// `span` cut at the settled horizon of `now`, when it runs across it.
pub(super) fn split(span: Span, now: DateTime<Utc>) -> Option<(Span, Span)> {
    let h = horizon(now);
    (span.start < h && h < span.end).then_some((
        Span {
            start: span.start,
            end: h,
        },
        Span {
            start: h,
            end: span.end,
        },
    ))
}

impl Index {
    /// Reads `span` with `read`, in one snapshot. When the span runs across the settled horizon
    /// the result comes in two parts, (settled part, the rest); the settled part is remembered
    /// under `what` (the query and every argument but the span) and read again only when the
    /// settled generation, the prices or the horizon moved. Without a split: (None, all of it).
    /// A settled part with requests still waiting for a price is not remembered.
    pub(super) fn settled_read<T: Clone + 'static>(
        &self,
        what: &str,
        span: Span,
        read: impl Fn(&Connection, Span) -> anyhow::Result<T>,
    ) -> anyhow::Result<(Option<T>, T)> {
        let Some((old, new)) = split(span, self.now()) else {
            return Ok((None, read(&self.conn, span)?));
        };
        let tx = if self.conn.is_autocommit() {
            Some(self.conn.unchecked_transaction()?)
        } else {
            None
        };
        let c: &Connection = &self.conn;
        let stamp = (
            schema::meta(c, "settled_gen")?,
            schema::meta(c, "price_fp")?,
        );
        let key = format!("{what}|{}|{}", ms(old.start), ms(old.end));
        let settled = match self.parts.get::<T>(&key, &stamp) {
            Some(v) => v,
            None => {
                let v = read(c, old)?;
                if !pending(c, old)? {
                    self.parts.put(key, stamp, v.clone());
                }
                v
            }
        };
        let rest = read(c, new)?;
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok((Some(settled), rest))
    }
}

/// Rows by key (counts, tokens and costs add up; a label is the larger of the two, as the
/// queries' `MAX(...)` labels are), most expensive first, like the queries order them.
pub(super) fn merge_rows((old, new): (Option<Vec<Row>>, Vec<Row>)) -> Vec<Row> {
    let Some(old) = old else { return new };
    let mut by_key: BTreeMap<String, Row> = BTreeMap::new();
    for r in old.into_iter().chain(new) {
        match by_key.get_mut(&r.key) {
            Some(acc) => add_row(acc, &r),
            None => {
                by_key.insert(r.key.clone(), r);
            }
        }
    }
    let mut out: Vec<Row> = by_key.into_values().collect();
    out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    out
}

pub(super) fn add_row(acc: &mut Row, r: &Row) {
    acc.requests += r.requests;
    acc.usage.add(&r.usage);
    acc.cost += r.cost;
    acc.unpriced += r.unpriced;
    acc.pending += r.pending;
    if r.label > acc.label {
        acc.label = r.label.clone();
    }
}

/// Quarter-hour sums by bucket (a bucket never runs across the horizon, a quarter hour).
pub(super) fn merge_buckets(
    (old, new): (Option<Vec<BucketRow>>, Vec<BucketRow>),
) -> Vec<BucketRow> {
    let Some(old) = old else { return new };
    let mut by: BTreeMap<i64, BucketRow> = BTreeMap::new();
    for b in old.into_iter().chain(new) {
        match by.get_mut(&b.0) {
            Some(acc) => {
                acc.1 += b.1;
                acc.2.add(&b.2);
                acc.3 += b.3;
                acc.4 += b.4;
                acc.5 += b.5;
            }
            None => {
                by.insert(b.0, b);
            }
        }
    }
    by.into_values().collect()
}

/// Matrix cells by (first key, second key), in key order.
pub(super) fn merge_cells((old, new): (Option<Vec<Cell>>, Vec<Cell>)) -> Vec<Cell> {
    let Some(old) = old else { return new };
    let mut by: BTreeMap<(String, String), (String, f64)> = BTreeMap::new();
    for (ka, la, kb, c) in old.into_iter().chain(new) {
        let e = by.entry((ka, kb)).or_insert_with(|| (la.clone(), 0.0));
        e.1 += c;
        if la > e.0 {
            e.0 = la;
        }
    }
    by.into_iter()
        .map(|((ka, kb), (la, c))| (ka, la, kb, c))
        .collect()
}

/// Counts by (first, second), largest first.
pub(super) fn merge_counts((old, new): (Option<Vec<Count>>, Vec<Count>)) -> Vec<Count> {
    let Some(old) = old else { return new };
    let mut by: BTreeMap<(String, String), u64> = BTreeMap::new();
    for (a, b, n) in old.into_iter().chain(new) {
        *by.entry((a, b)).or_default() += n;
    }
    let mut out: Vec<Count> = by.into_iter().map(|((a, b), n)| (a, b, n)).collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.2));
    out
}

/// Requests in `span` still waiting for a price: their cost is not known yet.
fn pending(c: &Connection, span: Span) -> rusqlite::Result<bool> {
    c.prepare_cached(
        "SELECT 1 FROM requests INDEXED BY requests_pending WHERE priced IS NULL AND ts >= ?1 AND ts < ?2 LIMIT 1",
    )?
    .exists(params![ms(span.start), ms(span.end)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pricing::PriceBook;
    use crate::index::query::{Dim, Filter, buckets_on, matrix_on, series_on, spend_by_on};
    use crate::index::scenarios::{World, assistant, owner};
    use chrono::TimeZone;

    fn ts(t: DateTime<Utc>) -> String {
        t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1e-3)
    }

    /// What the dashboard asks for, read once over the whole span (the old way) and through the
    /// kept settled parts: every number is the same, on a first read, a second one (kept parts),
    /// after a late request (the settled generation moves) and after a new one.
    #[test]
    fn kept_settled_parts_add_up_to_one_read() {
        let w = World::new(&["work"]);
        let now = Utc::now();
        let models = ["claude-sonnet-5-5", "claude-opus-5-5", "claude-mystery-1"];
        let cwds = ["/w/shop", "/w/docs"];
        let line = |i: i64, t: DateTime<Utc>| {
            assistant(
                &format!("m{i}"),
                &ts(t),
                models[i as usize % 3],
                cwds[i as usize % 2],
                (100 + i as u64, 40),
                &[],
            )
        };
        let mut lines = vec![owner("acc-a")];
        lines.extend((0..300).map(|i| line(i, now - Duration::minutes(9 * i + 1))));
        w.append("work", "s1", &lines);
        let mut idx = w.index("i.sqlite");
        let book = PriceBook::builtin();
        w.refresh(&mut idx, &[], &book);
        let span = Span {
            start: quarter_floor(now - Duration::days(1)),
            end: now + Duration::seconds(60),
        };
        let f = Filter::default();
        let check = |idx: &Index, when: &str| {
            assert!(split(span, Utc::now()).is_some());
            let c = &idx.conn;
            for dim in [Dim::Project, Dim::Model] {
                let one = spend_by_on(c, dim, span, &f, Utc::now()).unwrap();
                let two = idx.spend_by(dim, span, &f).unwrap();
                assert_eq!(one.len(), two.len(), "{when} {dim:?}");
                for (a, b) in one.iter().zip(&two) {
                    assert_eq!((&a.key, &a.label), (&b.key, &b.label), "{when} {dim:?}");
                    assert_eq!(
                        (a.requests, a.unpriced, a.pending),
                        (b.requests, b.unpriced, b.pending)
                    );
                    assert_eq!(a.usage.input, b.usage.input, "{when} {dim:?}");
                    assert!(
                        close(a.cost, b.cost),
                        "{when} {dim:?}: {} vs {}",
                        a.cost,
                        b.cost
                    );
                }
            }
            let one = crate::index::archive::read(c, span, false, Utc::now(), |c, p| {
                super::super::query::total_on(c, p, &f)
            })
            .unwrap();
            let two = idx.total(span, &f).unwrap();
            assert_eq!(
                (one.requests, one.unpriced),
                (two.requests, two.unpriced),
                "{when}"
            );
            assert!(
                close(one.cost, two.cost),
                "{when}: {} vs {}",
                one.cost,
                two.cost
            );
            let one = matrix_on(c, Dim::Project, Dim::Model, span, &f, Utc::now()).unwrap();
            let two = idx.matrix(Dim::Project, Dim::Model, span, &f).unwrap();
            assert_eq!(one.len(), two.len(), "{when}");
            for (a, b) in one.iter().zip(&two) {
                assert_eq!((&a.1, &a.2), (&b.0, &b.1), "{when}");
                assert!(close(a.3, b.2), "{when}");
            }
            let one = buckets_on(c, span, &f, Utc::now()).unwrap();
            let two = idx.buckets(span, &f).unwrap();
            assert_eq!(one.len(), two.len(), "{when}");
            for (a, b) in one.iter().zip(&two) {
                assert_eq!((a.0, a.1), (b.0, b.1), "{when}");
                assert!(close(a.3, b.3), "{when}");
            }
            let n = 25;
            let one = series_on(c, span, ms(span.start), 3_600_000, n, &f, Utc::now()).unwrap();
            let two = idx.spend_series(span, 60, &f).unwrap();
            assert_eq!(one.len(), two.len(), "{when}");
            assert!(one.iter().zip(&two).all(|(a, b)| close(*a, *b)), "{when}");
            let since = now - Duration::hours(20);
            let one: f64 = c
                .query_row(
                    "SELECT TOTAL(cost) FROM requests WHERE ts >= ?1",
                    [ms(since)],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(close(one, idx.spend_since(since).unwrap()), "{when}");
            let start = now - Duration::minutes(60);
            let mut one: Vec<(i64, String, f64)> = c
                .prepare("SELECT (ts - ?1) / 60000, model, TOTAL(cost) FROM requests WHERE ts >= ?1 GROUP BY 1, 2")
                .unwrap()
                .query_map([ms(start)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let mut two = idx.spend_per_minute_by_model(start).unwrap();
            one.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            two.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            assert_eq!(one.len(), two.len(), "{when}");
            for (a, b) in one.iter().zip(&two) {
                assert_eq!((a.0, &a.1), (b.0, &b.1), "{when}");
                assert!(close(a.2, b.2), "{when}");
            }
        };
        check(&idx, "first read");
        assert!(!idx.parts.map.borrow().is_empty(), "settled parts are kept");
        check(&idx, "kept parts");
        // A request from three hours ago written only now: settled spend changed.
        w.append("work", "s1", &[line(1000, now - Duration::hours(3))]);
        w.refresh(&mut idx, &[], &book);
        check(&idx, "late request");
        w.append("work", "s1", &[line(1001, Utc::now())]);
        w.refresh(&mut idx, &[], &book);
        check(&idx, "new request");
    }

    /// A kept sum asked again later with a later span end (the limit curve: a fixed window
    /// start, `end = now + 1s` every minute) reads what a fresh read of that span gives, length
    /// included: the kept part was summed for the first call's end.
    #[test]
    fn kept_parts_follow_a_later_span_end() {
        let w = World::new(&["work"]);
        let now = Utc::now();
        let mut lines = vec![owner("acc-a")];
        lines.extend((0..120).map(|i| {
            assistant(
                &format!("m{i}"),
                &ts(now - Duration::minutes(2 * i + 1)),
                "claude-sonnet-5-5",
                "/w/shop",
                (100 + i as u64, 40),
                &[],
            )
        }));
        w.append("work", "s1", &lines);
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        let start = quarter_floor(now - Duration::hours(3));
        let f = Filter::default();
        for later in [0, 5, 9] {
            let span = Span {
                start,
                end: now + Duration::seconds(1) + Duration::minutes(later),
            };
            let n = ((ms(span.end) - ms(start)) as f64 / 60_000.0).ceil() as usize;
            let fresh = series_on(&idx.conn, span, ms(start), 60_000, n, &f, Utc::now()).unwrap();
            let kept = idx.spend_series(span, 1, &f).unwrap();
            assert_eq!(kept.len(), fresh.len(), "{later} min later");
            assert!(
                kept.iter().zip(&fresh).all(|(a, b)| close(*a, *b)),
                "{later} min later"
            );
            let one = crate::index::archive::read(&idx.conn, span, false, Utc::now(), |c, p| {
                super::super::query::total_on(c, p, &f)
            })
            .unwrap();
            assert!(close(one.cost, idx.total(span, &f).unwrap().cost));
        }
    }

    /// Old requests that get their account only later (the seat's login became known after
    /// they were indexed) change an account's settled spend: a kept total must not keep the
    /// old value.
    #[test]
    fn a_late_account_moves_the_settled_totals() {
        use crate::index::SeatInfo;
        use crate::index::scenarios::account;
        let w = World::new(&["work"]);
        let now = Utc::now();
        let line = |id: &str, t: DateTime<Utc>| {
            assistant(id, &ts(t), "claude-sonnet-5-5", "/w/shop", (5000, 400), &[])
        };
        // No owner record and no known login: the request has no account yet.
        w.append("work", "s1", &[line("m1", now - Duration::hours(3))]);
        let mut idx = w.index("i.sqlite");
        let book = PriceBook::builtin();
        w.refresh(&mut idx, &[], &book);
        let span = Span {
            start: now - Duration::hours(6),
            end: now - Duration::hours(1),
        };
        assert_eq!(idx.settled_total("acc-a", span, now).unwrap(), 0.0);
        // The seat's login is known now (no time it was recorded at); a new response arrives.
        let mut login = account("acc-a", "2026-01-01T00:00:00Z");
        login.profile_fetched_at = None;
        let info = [SeatInfo {
            seat: Default::default(),
            label: "work".into(),
            account: Some(login),
            utilization: None,
        }];
        w.append("work", "s1", &[line("m2", now)]);
        w.refresh(&mut idx, &info, &book);
        let fresh = crate::index::archive::read(&idx.conn, span, true, Utc::now(), |c, p| {
            let f = Filter {
                account: Some(crate::core::ids::AccountId::new("acc-a")),
                ..Default::default()
            };
            super::super::query::total_on(c, p, &f)
        })
        .unwrap()
        .cost;
        assert!(fresh > 0.0);
        assert!(close(idx.settled_total("acc-a", span, now).unwrap(), fresh));
    }

    #[test]
    fn the_horizon_is_a_quarter_hour_at_least_fifteen_minutes_old() {
        let now = Utc.with_ymd_and_hms(2026, 3, 2, 10, 7, 30).unwrap();
        assert_eq!(
            horizon(now),
            Utc.with_ymd_and_hms(2026, 3, 2, 9, 45, 0).unwrap()
        );
        let on = Utc.with_ymd_and_hms(2026, 3, 2, 10, 0, 0).unwrap();
        assert_eq!(
            horizon(on),
            Utc.with_ymd_and_hms(2026, 3, 2, 9, 45, 0).unwrap()
        );
        assert!(horizon(now) <= now - Duration::milliseconds(super::super::SETTLED_AFTER_MS));
    }

    #[test]
    fn only_spans_across_the_horizon_split() {
        let now = Utc.with_ymd_and_hms(2026, 3, 2, 10, 7, 30).unwrap();
        let h = horizon(now);
        let week = Span {
            start: now - Duration::days(7),
            end: now,
        };
        let (a, b) = split(week, now).unwrap();
        assert_eq!((a.start, a.end, b.start, b.end), (week.start, h, h, now));
        let recent = Span { start: h, end: now };
        assert!(split(recent, now).is_none());
        let old = Span {
            start: now - Duration::days(7),
            end: h,
        };
        assert!(split(old, now).is_none());
    }
}

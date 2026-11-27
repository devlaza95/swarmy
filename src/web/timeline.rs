//! The expanded agent row's data shaped for a browser: the agent's latest tool calls as plain
//! facts, and its life (one point per request, up to 2000) summed into a fixed number of time
//! buckets so the page draws a sparkline without receiving every request. Pure functions over
//! what `live::Reads::detail` returns; nothing here reads the index.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::live::{LifePoint, State, ToolLine, compactions};

/// Buckets of the life chart: one bar of a few pixels each.
pub const BUCKETS: usize = 48;
/// A chart covers at least this long: a young agent's first requests do not fill the width.
const MIN_SPAN: Duration = Duration::seconds(60);
/// The longest tool summary sent. A path or a command is cut here (`…`), not in the page.
const SUMMARY_CHARS: usize = 200;

#[derive(Debug, Serialize)]
pub struct ToolView {
    pub at: DateTime<Utc>,
    pub name: String,
    /// What the tool was asked (a command, a path, a pattern); empty under `--mask`.
    pub summary: String,
    pub took_ms: Option<i64>,
    /// `ok`, `failed`, `running` or `unfinished` (no result, and the agent is not at work).
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct LifeView {
    /// Requests in the chart (the agent's last 2000).
    pub requests: usize,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub total_cost: f64,
    pub max_request_cost: f64,
    /// Dollars per bucket, [`BUCKETS`] of them from `from` to `to`.
    pub cost: Vec<f64>,
    /// The context as a percentage of the model's window at the end of each bucket (the last
    /// request's, carried over buckets with none).
    pub context_pct: Vec<f64>,
    /// Buckets where the context fell to under half (`live::compactions`).
    pub compactions: Vec<usize>,
}

/// Whether an agent in this state may have a tool at work. A tool call with no result of an agent
/// that is idle, done or gone never finished: it is not running.
pub fn is_at_work(s: &State) -> bool {
    match s {
        State::Tool { .. }
        | State::Thinking
        | State::NeedsYou(_)
        | State::WaitingAgents(_)
        | State::Stalled => true,
        State::Idle | State::Done | State::Gone => false,
    }
}

fn status_of(t: &ToolLine, at_work: bool) -> &'static str {
    match (t.done, t.is_error, at_work) {
        (Some(_), true, _) => "failed",
        (Some(_), false, _) => "ok",
        (None, _, true) => "running",
        (None, _, false) => "unfinished",
    }
}

fn cut(text: &str) -> String {
    if text.chars().count() <= SUMMARY_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(SUMMARY_CHARS).collect();
    out.push('…');
    out
}

/// The tool calls as the page lists them, newest first as given. `--mask` hides what the tools
/// were asked, as the dashboard does: a command or a path can name anyone.
pub fn tool_views(tools: &[ToolLine], at_work: bool, mask: bool) -> Vec<ToolView> {
    tools
        .iter()
        .map(|t| ToolView {
            at: t.ts,
            name: t.name.clone(),
            summary: if mask { String::new() } else { cut(&t.summary) },
            took_ms: t.done.map(|d| (d - t.ts).num_milliseconds().max(0)),
            status: status_of(t, at_work),
        })
        .collect()
}

fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

/// The life of an agent as [`BUCKETS`] equal slices of time from its first request to `now`.
/// `None` for an agent with no request yet. `window` is the model's context window in tokens.
pub fn life_view(points: &[LifePoint], window: u64, now: DateTime<Utc>) -> Option<LifeView> {
    let (first, last) = (points.first()?, points.last()?);
    let from = first.ts;
    let to = now.max(last.ts).max(from + MIN_SPAN);
    let span_ms = (to - from).num_milliseconds().max(1);
    let bucket_of = |ts: DateTime<Utc>| {
        let off = (ts - from).num_milliseconds().max(0);
        usize::try_from(off.saturating_mul(BUCKETS as i64) / span_ms)
            .unwrap_or(BUCKETS - 1)
            .min(BUCKETS - 1)
    };
    let pct = |context: u64| round(context as f64 / window.max(1) as f64 * 100.0, 1);

    let mut cost = vec![0.0; BUCKETS];
    let mut last_context: Vec<Option<u64>> = vec![None; BUCKETS];
    for p in points {
        let b = bucket_of(p.ts);
        cost[b] += p.cost;
        last_context[b] = Some(p.context);
    }
    // The first request is in the first bucket, so the carried value always starts known.
    let mut carried = first.context;
    let context_pct = last_context
        .iter()
        .map(|c| {
            carried = c.unwrap_or(carried);
            pct(carried)
        })
        .collect();
    let mut cuts: Vec<usize> = compactions(points)
        .into_iter()
        .filter_map(|i| points.get(i))
        .map(|p| bucket_of(p.ts))
        .collect();
    cuts.dedup();
    Some(LifeView {
        requests: points.len(),
        from,
        to,
        total_cost: round(points.iter().map(|p| p.cost).sum(), 6),
        max_request_cost: round(points.iter().map(|p| p.cost).fold(0.0, f64::max), 6),
        cost: cost.into_iter().map(|c| round(c, 6)).collect(),
        context_pct,
        compactions: cuts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn point(secs: i64, cost: f64, context: u64) -> LifePoint {
        LifePoint {
            ts: t0() + Duration::seconds(secs),
            cost,
            context,
        }
    }

    fn tool(secs: i64, name: &str, summary: &str, done: Option<i64>, err: bool) -> ToolLine {
        ToolLine {
            ts: t0() + Duration::seconds(secs),
            name: name.into(),
            summary: summary.into(),
            done: done.map(|d| t0() + Duration::seconds(d)),
            is_error: err,
        }
    }

    #[test]
    fn a_tool_call_is_ok_failed_running_or_unfinished() {
        let ok = tool(0, "Bash", "ls", Some(2), false);
        let bad = tool(0, "Bash", "ls", Some(2), true);
        let open = tool(0, "Bash", "ls", None, false);
        // A result decides, whatever the agent does now.
        assert_eq!(status_of(&ok, true), "ok");
        assert_eq!(status_of(&ok, false), "ok");
        assert_eq!(status_of(&bad, true), "failed");
        assert_eq!(status_of(&bad, false), "failed");
        // No result: running while the agent is at work, else it never finished.
        assert_eq!(status_of(&open, true), "running");
        assert_eq!(status_of(&open, false), "unfinished");
    }

    #[test]
    fn only_idle_done_and_gone_agents_are_not_at_work() {
        let tool = State::Tool {
            name: "Bash".into(),
            summary: String::new(),
        };
        for s in [
            &tool,
            &State::Thinking,
            &State::NeedsYou("q".into()),
            &State::WaitingAgents(2),
            &State::Stalled,
        ] {
            assert!(is_at_work(s), "{s:?}");
        }
        for s in [&State::Idle, &State::Done, &State::Gone] {
            assert!(!is_at_work(s), "{s:?}");
        }
    }

    #[test]
    fn tools_keep_their_order_and_say_how_long_they_took() {
        let tools = [
            tool(30, "Edit", "src/a.rs", None, false),
            tool(10, "Bash", "cargo test", Some(12), true),
            tool(0, "Read", "src/a.rs", Some(0), false),
        ];
        let v = tool_views(&tools, true, false);
        let seen: Vec<_> = v
            .iter()
            .map(|t| (t.name.as_str(), t.status, t.took_ms))
            .collect();
        assert_eq!(
            seen,
            [
                ("Edit", "running", None),
                ("Bash", "failed", Some(2000)),
                ("Read", "ok", Some(0)),
            ]
        );
        assert_eq!(v[1].summary, "cargo test");
    }

    /// A result stamped before its call (clock skew between two records) is a zero duration,
    /// not a negative one.
    #[test]
    fn a_result_before_its_call_is_a_zero_duration() {
        let v = tool_views(&[tool(10, "Bash", "x", Some(5), false)], false, false);
        assert_eq!(v[0].took_ms, Some(0));
    }

    #[test]
    fn mask_hides_what_the_tools_were_asked_and_a_long_summary_is_cut() {
        let long = "x".repeat(SUMMARY_CHARS + 50);
        let tools = [tool(0, "Bash", &long, Some(1), false)];
        let shown = &tool_views(&tools, false, false)[0];
        assert_eq!(shown.summary.chars().count(), SUMMARY_CHARS + 1);
        assert!(shown.summary.ends_with('…'));
        let masked = &tool_views(&tools, false, true)[0];
        assert_eq!(masked.summary, "");
        assert_eq!(masked.name, "Bash", "the tool's name is not personal");
        // Cut by characters, never in the middle of one.
        let wide = "é".repeat(SUMMARY_CHARS + 1);
        assert_eq!(cut(&wide).chars().count(), SUMMARY_CHARS + 1);
    }

    #[test]
    fn an_agent_with_no_request_has_no_life() {
        assert!(life_view(&[], 200_000, t0()).is_none());
    }

    /// Requests at 0 s, 60 s and 119 s of a two-minute life, now at 120 s: 48 buckets of 2.5 s.
    /// 0 s → bucket 0, 60 s → 24, 119 s → 47. Costs add inside a bucket, the context is the
    /// bucket's last request and carries over the empty ones. 20k, 100k, 50k of 200k = 10, 50,
    /// 25 %.
    #[test]
    fn requests_are_summed_into_equal_time_buckets() {
        let pts = [
            point(0, 0.25, 20_000),
            point(1, 0.25, 20_000), // same bucket as the first: 0.50 together
            point(60, 0.10, 100_000),
            point(119, 0.40, 50_000),
        ];
        let v = life_view(&pts, 200_000, t0() + Duration::seconds(120)).unwrap();
        assert_eq!((v.requests, v.cost.len(), v.context_pct.len()), (4, 48, 48));
        let lit: Vec<(usize, f64)> = v
            .cost
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 0.0)
            .map(|(i, c)| (i, *c))
            .collect();
        assert_eq!(lit, [(0, 0.5), (24, 0.1), (47, 0.4)]);
        assert_eq!(v.total_cost, 1.0);
        assert_eq!(v.max_request_cost, 0.4);
        assert_eq!(v.context_pct[0], 10.0);
        assert_eq!(v.context_pct[23], 10.0, "carried over the empty buckets");
        assert_eq!(v.context_pct[24], 50.0);
        assert_eq!(v.context_pct[46], 50.0);
        assert_eq!(v.context_pct[47], 25.0);
        assert!(v.compactions.is_empty());
    }

    /// The chart runs to `now`: an agent quiet for an hour after its last request has its
    /// context carried to the end and no cost there.
    #[test]
    fn the_chart_runs_to_now_and_a_young_agent_does_not_fill_it() {
        let pts = [point(0, 0.2, 100_000), point(30, 0.2, 120_000)];
        let quiet = life_view(&pts, 200_000, t0() + Duration::hours(1)).unwrap();
        assert_eq!(quiet.to, t0() + Duration::hours(1));
        assert_eq!(quiet.cost[BUCKETS - 1], 0.0);
        assert_eq!(quiet.context_pct[BUCKETS - 1], 60.0);
        // Now is 30 s after the first request: the chart still covers a minute (1.25 s a
        // bucket), so the second request, at 30 s, is in bucket 24, not the last one.
        let young = life_view(&pts, 200_000, t0() + Duration::seconds(30)).unwrap();
        assert_eq!(young.to, t0() + MIN_SPAN);
        assert_eq!(young.cost[24], 0.2);
    }

    #[test]
    fn a_compaction_is_marked_in_the_bucket_of_the_request_after_the_fall() {
        let pts = [
            point(0, 0.1, 180_000),
            point(60, 0.1, 40_000), // under half of 180k
            point(119, 0.1, 50_000),
        ];
        let v = life_view(&pts, 200_000, t0() + Duration::seconds(120)).unwrap();
        assert_eq!(v.compactions, [24]);
        assert_eq!(v.context_pct[23], 90.0);
        assert_eq!(v.context_pct[24], 20.0);
    }

    /// A context bigger than the window (a stale window size) is shown as it is, and a window
    /// of zero does not divide by it.
    #[test]
    fn the_context_percentage_survives_odd_windows() {
        let pts = [point(0, 0.0, 300_000)];
        assert_eq!(
            life_view(&pts, 200_000, t0()).unwrap().context_pct[0],
            150.0
        );
        assert!(life_view(&pts, 0, t0()).unwrap().context_pct[0].is_finite());
    }
}

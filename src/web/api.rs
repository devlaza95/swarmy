//! The JSON the web view reads. Every number comes from the same code as the commands and the
//! dashboard (`Ctx::limit_reports`, `Ctx::spend_report`, `live::build`); this module only
//! shapes it for a browser. `--mask` is applied there, not here, except for the few fields only
//! this view shows.

use clap::ValueEnum;
use serde::Serialize;
use serde_json::{Value, json};

use super::Hub;
use super::http::Request;
use crate::app::context::Ctx;
use crate::core::harness::Harness;
use crate::core::limits::Level;
use crate::core::models::{context_window, short_name};
use crate::core::period::Period;
use crate::index::query::{Dim, Filter};
use crate::limits::WindowReport;
use crate::live::{Agent, Snapshot, State};
use crate::ui::fmt;
use crate::ui::glossary::{self, Group};

pub enum ApiError {
    /// The request named something that does not exist: the caller can fix it.
    BadRequest(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

type ApiResult = Result<Value, ApiError>;

/// The state as the page needs it: a stable id for styling and the words that go with it.
fn state_parts(s: &State) -> (&'static str, String) {
    match s {
        State::NeedsYou(what) => ("needs_you", what.clone()),
        State::Tool { name, summary } => (
            "working",
            if summary.is_empty() {
                name.clone()
            } else {
                format!("{name} {summary}")
            },
        ),
        State::Thinking => ("thinking", String::new()),
        State::WaitingAgents(n) => (
            "waiting",
            format!("{n} agent{}", if *n == 1 { "" } else { "s" }),
        ),
        State::Idle => ("idle", String::new()),
        State::Stalled => ("stalled", String::new()),
        State::Done => ("done", String::new()),
        State::Gone => ("gone", String::new()),
    }
}

/// What the dashboard hides for `--mask` in a name: an email.
fn shown_name(ctx: &Ctx, s: &str) -> String {
    if ctx.mask && s.contains('@') {
        "•••".into()
    } else {
        s.to_string()
    }
}

fn agent_json(ctx: &Ctx, a: &Agent) -> Value {
    let (state, detail) = state_parts(&a.state);
    let quiet = ctx.now - a.last_activity;
    let window = a.model.as_deref().map(context_window).unwrap_or(1_000_000);
    json!({
        "key": a.key(),
        "name": shown_name(ctx, &a.name),
        "project": a.project,
        "depth": a.depth,
        "main": a.is_main(),
        "state": state,
        "detail": shown_name(ctx, &detail),
        "hint": a.state.hint(a.is_main(), &fmt::duration(quiet)),
        "model": a.model.as_deref().map(short_name),
        "effort": a.effort,
        "since": a.since,
        "last_activity": a.last_activity,
        "cost": a.cost,
        "requests": a.requests,
        "unpriced": a.unpriced,
        "session_cost": a.is_main().then_some(a.session_cost),
        "session_burn": a.is_main().then_some(a.session_burn),
        "context_tokens": a.context_tokens,
        "context_window": window,
        "context_pct": a.context_tokens as f64 / window as f64 * 100.0,
        "seat": a.seat_label,
        "harness": a.harness.display_name(),
        "agent_type": a.agent_type,
        "checkout": a.checkout.as_ref().map(|c| c.label(&a.project)),
        "branch": a.checkout.as_ref().and_then(|c| c.branch_label()),
        "shared_checkout": a.checkout.as_ref().is_some_and(|c| c.is_shared()),
    })
}

fn live_json(ctx: &Ctx, snap: &Snapshot) -> Value {
    let count = |kind: &str| {
        snap.agents
            .iter()
            .filter(|a| state_parts(&a.state).0 == kind)
            .count()
    };
    json!({
        "taken_at": snap.taken_at,
        "today_cost": snap.today_cost,
        "burn_per_hour": snap.burn_per_hour,
        "counts": {
            "agents": snap.agents.len(),
            "working": count("working"),
            "thinking": count("thinking"),
            "needs_you": count("needs_you"),
            "stalled": count("stalled"),
        },
        "agents": snap.agents.iter().map(|a| agent_json(ctx, a)).collect::<Vec<_>>(),
        "recent_done": snap.recent_done.iter().map(|d| json!({
            "name": shown_name(ctx, &d.name),
            "project": d.project,
            "model": d.model.as_deref().map(short_name),
            "started": d.started,
            "finished": d.finished,
            "cost": d.cost,
        })).collect::<Vec<_>>(),
        "conflicts": snap.conflicts.iter().map(|(file, n)| json!({
            "file": ctx.show_path(file),
            "agents": n,
        })).collect::<Vec<_>>(),
    })
}

pub fn live(hub: &Hub) -> ApiResult {
    let Some(snap) = &hub.snapshot else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "the live view is not ready yet"
        )));
    };
    Ok(live_json(&hub.ctx, snap))
}

#[derive(Serialize)]
struct WindowView<'a> {
    #[serde(flatten)]
    report: &'a WindowReport,
    /// One set of thresholds for every front end: the page only maps this to a colour.
    level: Level,
    label: String,
    resets_at: chrono::DateTime<chrono::Utc>,
    runs_out_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Utilization at the reset at the current pace, when known.
    projected_at_reset: Option<f64>,
}

#[derive(Serialize)]
struct AccountView<'a> {
    account_uuid: &'a str,
    name: &'a str,
    tier: Option<&'a str>,
    windows: Vec<WindowView<'a>>,
}

pub fn limits(hub: &Hub) -> ApiResult {
    let reports = hub.ctx.limit_reports(&hub.idx)?;
    let views: Vec<AccountView> = reports
        .iter()
        .map(|r| AccountView {
            account_uuid: r.account.as_str(),
            name: &r.name,
            tier: r.tier.as_deref(),
            windows: r
                .windows
                .iter()
                .map(|w| WindowView {
                    report: w,
                    level: w.level(),
                    label: w.reading.kind.label().to_string(),
                    resets_at: w.window.end,
                    runs_out_at: w.runs_out_at,
                    projected_at_reset: w.projection_at_reset(hub.ctx.now).map(|p| p.get()),
                })
                .collect(),
        })
        .collect();
    Ok(json!({ "now": hub.ctx.now, "accounts": views }))
}

fn name_of<T: ValueEnum>(v: &T) -> String {
    v.to_possible_value()
        .map(|p| p.get_name().to_string())
        .unwrap_or_default()
}

fn parse_enum<T: ValueEnum>(what: &str, s: &str) -> Result<T, ApiError> {
    T::from_str(s, true).map_err(|_| {
        let known: Vec<String> = T::value_variants().iter().map(name_of).collect();
        ApiError::BadRequest(format!(
            "unknown {what} `{s}`; one of: {}",
            known.join(", ")
        ))
    })
}

/// The filter a request asks for. Names are resolved the way the commands resolve them, so
/// `?seat=work` and `--seat work` mean the same thing.
fn filter_of(hub: &Hub, req: &Request) -> Result<Filter, ApiError> {
    // A name that matches nothing is the caller's to fix: the resolvers' message lists what
    // exists (`no seat 'x' (seats: a, b)`).
    let bad = |e: anyhow::Error| ApiError::BadRequest(format!("{e:#}"));
    let seat = match req.param("seat") {
        Some(s) => Some(hub.ctx.resolve_seat(s).map_err(bad)?.id.clone()),
        None => None,
    };
    let account = match req.param("account") {
        Some(a) => Some(hub.ctx.resolve_account(&hub.idx, a).map_err(bad)?),
        None => None,
    };
    let harness = match req.param("harness") {
        Some(h) => Some(parse_enum::<Harness>("harness", h)?),
        None => None,
    };
    Ok(Filter {
        account,
        seat,
        harness,
        project: req.param("project").map(str::to_string),
        model: req.param("model").map(str::to_string),
        in_worktree: None,
    })
}

pub fn spend(hub: &Hub, req: &Request) -> ApiResult {
    let by = match req.param("by") {
        Some(s) => parse_enum::<Dim>("grouping", s)?,
        None => Dim::Project,
    };
    let period: Period = req
        .param("period")
        .unwrap_or("week")
        .parse()
        .map_err(|e: String| ApiError::BadRequest(e))?;
    let filter = filter_of(hub, req)?;
    let report = hub.ctx.spend_report(&hub.idx, by, &period, &filter)?;
    Ok(json!({ "title": by.title(), "report": report }))
}

pub fn status(hub: &Hub) -> ApiResult {
    let ctx = &hub.ctx;
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "now": ctx.now,
        "mask": ctx.mask,
        "problems": ctx.problems,
        "seats": ctx.seats.iter().map(|s| json!({
            "label": s.label,
            "harness": s.harness.display_name(),
        })).collect::<Vec<_>>(),
        "groupings": Dim::value_variants().iter().map(|d| json!({
            "id": name_of(d),
            "title": d.title(),
        })).collect::<Vec<_>>(),
    }))
}

/// The glossary: the same table `swarmy explain` and the dashboard's help read.
pub fn glossary() -> Value {
    let groups: Vec<Value> = Group::ALL
        .iter()
        .map(|g| {
            json!({
                "title": g.title(),
                "terms": glossary::TERMS.iter().filter(|t| t.group == *g).map(|t| json!({
                    "name": t.name,
                    "aliases": t.aliases,
                    "short": t.short,
                    "long": t.long,
                    "see": t.see,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "groups": groups })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_a_stable_id_the_page_can_style() {
        let ids: Vec<&str> = [
            State::NeedsYou("q".into()),
            State::Tool {
                name: "Bash".into(),
                summary: "ls".into(),
            },
            State::Thinking,
            State::WaitingAgents(1),
            State::Idle,
            State::Stalled,
            State::Done,
            State::Gone,
        ]
        .iter()
        .map(|s| state_parts(s).0)
        .collect();
        assert_eq!(
            ids,
            [
                "needs_you",
                "working",
                "thinking",
                "waiting",
                "idle",
                "stalled",
                "done",
                "gone"
            ]
        );
    }

    #[test]
    fn a_tool_state_names_the_tool_and_what_it_was_asked() {
        let s = State::Tool {
            name: "Bash".into(),
            summary: "cargo test".into(),
        };
        assert_eq!(state_parts(&s).1, "Bash cargo test");
        assert_eq!(state_parts(&State::WaitingAgents(2)).1, "2 agents");
        assert_eq!(state_parts(&State::WaitingAgents(1)).1, "1 agent");
    }

    #[test]
    fn an_unknown_grouping_lists_the_known_ones() {
        let Err(ApiError::BadRequest(msg)) = parse_enum::<Dim>("grouping", "bogus") else {
            panic!("an unknown grouping must be a bad request");
        };
        assert!(
            msg.contains("`bogus`") && msg.contains("project, "),
            "{msg}"
        );
        assert!(matches!(parse_enum::<Dim>("grouping", "DAY"), Ok(Dim::Day)));
    }

    #[test]
    fn the_glossary_json_has_every_group_and_term() {
        let v = glossary();
        let groups = v["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), Group::ALL.len());
        let terms: usize = groups
            .iter()
            .map(|g| g["terms"].as_array().map_or(0, Vec::len))
            .sum();
        assert_eq!(terms, glossary::TERMS.len());
    }
}

//! The JSON the web view reads. Every number comes from the same code as the commands and the
//! dashboard (`Ctx::limit_reports`, `Ctx::spend_report`, `live::build`); this module only
//! shapes it for a browser. `--mask` is applied there, not here, except for the few fields only
//! this view shows.

use clap::ValueEnum;
use serde::Serialize;
use serde_json::{Value, json};

use super::Hub;
use super::http::Request;
use super::timeline;
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
    /// The request was well formed but what it asks for is not there (any more): an agent that
    /// left the list between two calls.
    NotFound(String),
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

/// The context window of the agent's model, as the AGENT card counts it.
fn window_of(a: &Agent) -> u64 {
    a.model.as_deref().map(context_window).unwrap_or(1_000_000)
}

fn agent_json(ctx: &Ctx, a: &Agent) -> Value {
    let (state, detail) = state_parts(&a.state);
    let quiet = ctx.now - a.last_activity;
    let window = window_of(a);
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

/// One agent's expanded row: its latest tool calls and its life. Asked for by the agent's `key`
/// (as `/api/live` gives it), only for the rows a person opened; `Reads::detail` reads the index
/// again only when that agent has something new, so a poll of an open row is a lookup.
pub fn agent(hub: &mut Hub, req: &Request) -> ApiResult {
    let Some(key) = req.param("key") else {
        return Err(ApiError::BadRequest(
            "missing `key`: the agent's key, as /api/live gives it".into(),
        ));
    };
    // The key is only compared with the agents shown now, never parsed or looked up elsewhere.
    let found = hub
        .snapshot
        .as_ref()
        .and_then(|s| s.agents.iter().find(|a| a.key() == key))
        .cloned();
    let Some(a) = found else {
        return Err(ApiError::NotFound(
            "no such agent: it may have finished and left the list".into(),
        ));
    };
    let detail = hub.reads.detail(&hub.idx, &a)?;
    let now = hub.ctx.now;
    Ok(json!({
        "key": a.key(),
        "taken_at": now,
        "tools": timeline::tool_views(&detail.tools, timeline::is_at_work(&a.state), hub.ctx.mask),
        "life": timeline::life_view(&detail.life, window_of(&a), now),
    }))
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

    // ---- /api/agent: a Hub over a synthetic world in a temp dir ----------------------------------

    use crate::index::scenarios::{World, owner};
    use crate::ui::style::{ColorChoice, Painter};
    use chrono::{DateTime, Duration, Utc};

    /// A path no sane tool would be asked about, which a page that built markup from it would
    /// run.
    const HOSTILE: &str = "<img src=x onerror=alert(1)>";

    /// One response of the agent: its prompt size and the tools it asks for (id, name, input).
    fn request(id: &str, at: DateTime<Utc>, prompt: u64, tools: &[(&str, &str, Value)]) -> String {
        let content: Vec<Value> = tools
            .iter()
            .map(|(tid, name, input)| json!({"type": "tool_use", "id": tid, "name": name, "input": input}))
            .collect();
        json!({
            "type": "assistant", "timestamp": at.to_rfc3339(), "cwd": "/w/shop", "sessionId": "s1",
            "message": {"id": id, "model": "claude-sonnet-5-5", "content": content,
                "usage": {"input_tokens": prompt, "output_tokens": 100,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        })
        .to_string()
    }

    fn result(at: DateTime<Utc>, tool_id: &str, is_error: bool) -> String {
        json!({
            "type": "user", "timestamp": at.to_rfc3339(),
            "message": {"role": "user", "content": [
                {"tool_use_id": tool_id, "type": "tool_result", "content": "out", "is_error": is_error}]},
            "toolUseResult": {"stdout": "out"}
        })
        .to_string()
    }

    /// Four requests in the last four minutes, with prompts of 20k, 40k, 120k and 30k tokens
    /// (the last one a fall to under half of 120k: a compaction) and these tools:
    /// Read (ok, 1 s), Bash `cargo test` (failed, 10 s), an Edit of `HOSTILE` (ok, 2 s) and
    /// Bash `sleep 600`, which never got a result.
    fn world() -> World {
        let w = World::new(&["work"]);
        let now = Utc::now();
        let at = |secs: i64| now - Duration::seconds(secs);
        let path = |p: &str| json!({ "file_path": p });
        w.append(
            "work",
            "s1",
            &[
                owner("acc-a"),
                request(
                    "m1",
                    at(240),
                    20_000,
                    &[("t1", "Read", path("/w/shop/src/cart.rs"))],
                ),
                result(at(239), "t1", false),
                request(
                    "m2",
                    at(180),
                    40_000,
                    &[("t2", "Bash", json!({"command": "cargo test"}))],
                ),
                result(at(170), "t2", true),
                request("m3", at(60), 120_000, &[("t3", "Edit", path(HOSTILE))]),
                result(at(58), "t3", false),
                request(
                    "m4",
                    at(30),
                    30_000,
                    &[("t4", "Bash", json!({"command": "sleep 600"}))],
                ),
            ],
        );
        w
    }

    /// The context only for a hub: a made-up context over the world's seats, never real ones.
    fn hub(w: &World) -> Hub {
        let painter = Painter::new(ColorChoice::Never);
        let ctx = Ctx::demo(painter, w.seats.clone(), w.dir.path()).unwrap();
        Hub::new(ctx).unwrap()
    }

    fn ask(path: &str, key: Option<&str>) -> Request {
        Request {
            method: "GET".into(),
            path: path.into(),
            query: key
                .map(|k| ("key".to_string(), k.to_string()))
                .into_iter()
                .collect(),
            host: Some("127.0.0.1:7878".into()),
            fetch_site: None,
        }
    }

    fn key_of_main(hub: &Hub) -> String {
        let snap = hub.snapshot.as_ref().unwrap();
        assert_eq!(snap.agents.len(), 1, "one session, one main thread");
        snap.agents[0].key()
    }

    #[test]
    fn an_agents_detail_has_its_tools_newest_first_and_its_life_in_buckets() {
        let w = world();
        let mut hub = hub(&w);
        let key = key_of_main(&hub);
        let v = agent(&mut hub, &ask("/api/agent", Some(&key)))
            .ok()
            .unwrap();
        assert_eq!(v["key"], key.as_str());

        // Newest first. The session is not running, so the call with no result never finished.
        let tools = v["tools"].as_array().unwrap();
        let seen: Vec<(&str, &str, &str, Option<i64>)> = tools
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap(),
                    t["summary"].as_str().unwrap(),
                    t["status"].as_str().unwrap(),
                    t["took_ms"].as_i64(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("Bash", "sleep 600", "unfinished", None),
                ("Edit", HOSTILE, "ok", Some(2_000)),
                ("Bash", "cargo test", "failed", Some(10_000)),
                ("Read", "/w/shop/src/cart.rs", "ok", Some(1_000)),
            ]
        );

        // The life: four requests in 48 buckets, the context against the model's window.
        let life = &v["life"];
        assert_eq!(life["requests"], 4);
        let window = context_window("claude-sonnet-5-5") as f64;
        let ctx_at = |i: usize| life["context_pct"][i].as_f64().unwrap();
        let pct = |tokens: f64| (tokens / window * 1000.0).round() / 10.0;
        assert_eq!(life["cost"].as_array().unwrap().len(), 48);
        assert_eq!(ctx_at(0), pct(20_000.0));
        assert_eq!(
            ctx_at(47),
            pct(30_000.0),
            "the last request's context, at the end"
        );
        assert_eq!(life["compactions"].as_array().unwrap().len(), 1);
        // The same money as the live row and the dashboard's AGENT card.
        let live = live(&hub).ok().unwrap();
        let row = &live["agents"][0];
        let total = life["total_cost"].as_f64().unwrap();
        assert!(
            total > 0.0 && (total - row["cost"].as_f64().unwrap()).abs() < 1e-5,
            "{total}"
        );
        let bars: f64 = life["cost"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_f64)
            .sum();
        assert!(
            (bars - total).abs() < 1e-5,
            "the buckets add up to the total"
        );
    }

    #[test]
    fn a_tool_without_a_result_is_running_while_its_agent_is_at_work() {
        let w = world();
        let mut hub = hub(&w);
        let key = key_of_main(&hub);
        if let Some(snap) = hub.snapshot.as_mut() {
            snap.agents[0].state = State::Thinking;
        }
        let v = agent(&mut hub, &ask("/api/agent", Some(&key)))
            .ok()
            .unwrap();
        assert_eq!(v["tools"][0]["status"], "running");
        assert_eq!(v["tools"][1]["status"], "ok", "a result decides");
    }

    /// A hostile tool path is a string in a JSON body that is served as JSON and never
    /// sniffed as anything else; the page puts it in a text node (see the guards in
    /// `web::tests`).
    #[test]
    fn a_hostile_tool_summary_stays_a_plain_string_in_a_json_response() {
        let w = world();
        let mut hub = hub(&w);
        let key = key_of_main(&hub);
        let resp = super::super::answer(&mut hub, &ask("/api/agent", Some(&key)));
        assert_eq!(resp.status, 200);
        assert!(resp.content_type.starts_with("application/json"));
        let bytes = String::from_utf8(resp.to_bytes()).unwrap();
        assert!(bytes.contains("X-Content-Type-Options: nosniff"));
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["tools"][1]["summary"], HOSTILE);
    }

    #[test]
    fn mask_hides_what_the_tools_were_asked_but_not_which_tools_ran() {
        let w = world();
        let mut hub = hub(&w);
        hub.ctx.mask = true;
        let key = key_of_main(&hub);
        let v = agent(&mut hub, &ask("/api/agent", Some(&key)))
            .ok()
            .unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        assert!(tools.iter().all(|t| t["summary"] == ""));
        assert_eq!(tools[3]["name"], "Read");
        assert!(!v.to_string().contains("cart.rs") && !v.to_string().contains("onerror"));
    }

    #[test]
    fn a_missing_or_unknown_key_is_for_the_caller_to_fix() {
        let w = world();
        let mut hub = hub(&w);
        assert!(matches!(
            agent(&mut hub, &ask("/api/agent", None)),
            Err(ApiError::BadRequest(m)) if m.contains("key")
        ));
        // Gone from the list is not a server fault: a 404 the page takes quietly.
        assert!(matches!(
            agent(&mut hub, &ask("/api/agent", Some("0000/none/"))),
            Err(ApiError::NotFound(_))
        ));
        let mut code =
            |key: Option<&str>| super::super::answer(&mut hub, &ask("/api/agent", key)).status;
        assert_eq!(code(None), 400);
        assert_eq!(code(Some("0000/none/")), 404);
        // The key is compared, not interpreted: paths and SQL are just keys that match nothing.
        for odd in ["../../etc/passwd", "' OR 1=1 --", "%00", "*", ""] {
            assert!(matches!(code(Some(odd)), 400 | 404), "{odd}");
        }
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

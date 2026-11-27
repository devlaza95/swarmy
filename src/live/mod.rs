//! The live state of every agent: who is thinking, running a tool, waiting for the human,
//! waiting for its children, stalled or done. Built from the index (freshly refreshed), the
//! session registry and the process table.

// A new variant of a domain enum (a harness, a window kind, a dim) must be handled where it
// is matched, not fall into a catch-all (reviews A-H7, B-M24, B-L3).
#![deny(clippy::wildcard_enum_match_arm)]
// No result ignored, no string cut by bytes, and (where there are none) no silent numeric
// casts: the strict lints switched on, in production code, where they hold already (reviews
// A-H7, B-M24).
#![cfg_attr(not(test), deny(clippy::let_underscore_must_use))]
#![cfg_attr(not(test), deny(clippy::string_slice))]

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::claude::registry::LiveSession;
use crate::core::harness::Harness;
use crate::core::ids::{SeatId, SessionId};
use crate::core::models::Family;
use crate::core::tools::ToolClass;
use crate::index::live::{AgentKey, AgentRow, DoneAgent};
use crate::index::{Index, from_ms, ms};

mod checkout;
mod state;
pub use checkout::Checkout;
pub use state::{Options, State, StateInputs, agent_state};

#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub seat: SeatId,
    /// The seat's label, for display only.
    pub seat_label: String,
    pub harness: Harness,
    pub session_id: SessionId,
    /// Empty for the main thread.
    pub agent_id: String,
    pub parent: Option<String>,
    pub depth: u32,
    pub name: String,
    pub agent_type: Option<String>,
    pub project: String,
    /// The project's identity (`projects.key`), what panels join on; `None` for a dir the index
    /// has not resolved yet.
    #[serde(skip)]
    pub project_key: Option<crate::core::ids::ProjectKey>,
    /// The git checkout it works in (worktree, branch, live sessions sharing it).
    pub checkout: Option<Checkout>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub state: State,
    /// When the current state began (tool start, last activity…).
    pub since: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub cost: f64,
    pub requests: u64,
    /// True when the agent has requests, zero priced cost, and its latest model has no known price.
    pub unpriced: bool,
    /// Prompt size of the last request: how full the context is.
    pub context_tokens: u64,
    /// USD per minute over the last 15 minutes.
    pub spark: Vec<f64>,
    /// What the agent did in each of the last 30 minutes (oldest first), see `Lane`.
    pub lane: Vec<Lane>,
    pub pane: Option<crate::core::pane::Pane>,
    pub pid: Option<u32>,
    pub account: Option<String>,
    /// When the agent first appeared: the stable sort key of the tree.
    pub first_seen: DateTime<Utc>,
    /// Main threads only: the whole session (main thread and all its agents).
    pub session_cost: f64,
    /// Main threads only: session spend over the last hour.
    pub session_burn: f64,
    /// Main threads only: CPU % and resident bytes of the Claude Code process.
    pub cpu: f32,
    pub memory: u64,
    /// Time of the last request, to tell whether anything changed since the last snapshot.
    pub last_request_ts: Option<i64>,
}

impl Agent {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.seat, self.session_id, self.agent_id)
    }

    pub fn family(&self) -> Family {
        self.model
            .as_deref()
            .map(Family::of)
            .unwrap_or(Family::Other)
    }

    pub fn is_main(&self) -> bool {
        self.agent_id.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolLine {
    pub ts: DateTime<Utc>,
    pub name: String,
    pub summary: String,
    pub done: Option<DateTime<Utc>>,
    pub is_error: bool,
}

/// An agent by (seat, session id, agent id).
type AgentOf = (SeatId, SessionId, String);

/// Per-agent reads kept from one snapshot to the next: read again only when the index wrote
/// something of the agent's since (`Index::agent_seq`), and on every heavy tick (`build`'s
/// `prev` is `None` then), which also picks up a reprice or a re-attribution.
#[derive(Default)]
pub struct Reads {
    agents: HashMap<AgentOf, AgentReads>,
    /// A session's whole spend, with the session's write sequence and the price fingerprint
    /// it was read under.
    sessions: HashMap<(SeatId, SessionId), (u64, Option<String>, f64)>,
}

#[derive(Clone)]
struct AgentReads {
    seq: u64,
    last_req: crate::index::live::LastRequest,
    totals: (u64, f64),
    tool: Option<crate::index::live::OpenTool>,
    done_ts: Option<i64>,
    branch: Option<String>,
    /// USD per minute over the last 15 minutes, as of the read.
    spark: Vec<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub taken_at: Option<DateTime<Utc>>,
    /// Ordered as a forest: each main thread followed by its descendants.
    pub agents: Vec<Agent>,
    /// USD per clock minute, per model (`models::short_name`: `opus-5.5`, `gem-4-neon`):
    /// [`BURN_MINUTES`] of them from [`burn_start`], the last one the minute now in.
    pub burn: BTreeMap<String, Vec<f64>>,
    /// USD per model over the last 60 minutes, to the second: the rates beside the chart.
    pub burn_hour: BTreeMap<String, f64>,
    pub burn_per_hour: f64,
    pub today_cost: f64,
    pub errors_last_hour: Vec<(String, u64)>,
    /// Agents that finished in the last hours, newest first.
    pub recent_done: Vec<DoneRow>,
    /// Files edited by more than one agent in the last 30 minutes.
    pub conflicts: Vec<(String, u32)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoneRow {
    pub name: String,
    pub project: String,
    pub model: Option<String>,
    pub started: DateTime<Utc>,
    pub finished: DateTime<Utc>,
    pub cost: f64,
}

impl Snapshot {
    pub fn count(&self, f: impl Fn(&State) -> bool) -> usize {
        self.agents.iter().filter(|a| f(&a.state)).count()
    }

    /// Dollars per hour by model: each model's spend over the last 60 minutes (its part of
    /// [`Snapshot::burn_per_hour`], which is their sum), the largest first; models that spent
    /// nothing are left out.
    pub fn burn_by_model(&self) -> Vec<(&str, f64)> {
        let mut v: Vec<(&str, f64)> = self
            .burn_hour
            .iter()
            .map(|(m, r)| (m.as_str(), *r))
            .filter(|(_, r)| *r > 0.0)
            .collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
        v
    }
}

/// The BURN chart's bins: whole clock minutes, the first one cut by the hour's start, the last
/// one the minute now in, still filling.
pub const BURN_MINUTES: usize = 61;

/// The start of the BURN chart's first bin: the clock minute an hour before the one `now` is
/// in. The bins stay put while a minute lasts and step once when it turns (counted from `now`,
/// every second moved a few requests into the next bin and the bars jittered).
pub fn burn_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let minute = DateTime::from_timestamp(now.timestamp().div_euclid(60) * 60, 0).unwrap_or(now);
    minute - Duration::minutes(60)
}

/// How far `now` is into its clock minute, from 0 up to 1: how far the chart slid left since
/// its bins last stepped.
pub fn burn_offset(now: DateTime<Utc>) -> f64 {
    (now - burn_start(now) - Duration::minutes(60)).num_milliseconds() as f64 / 60_000.0
}

pub fn build(
    idx: &Index,
    live: &[LiveSession],
    procs: &crate::procs::ProcTable,
    opts: &Options,
    now: DateTime<Utc>,
    // Session totals and the recently-done list change slowly: when `prev` is given they are
    // carried over instead of recomputed.
    prev: Option<&Snapshot>,
    // Per-agent reads of earlier snapshots, reused while the agent has nothing new.
    reads: &mut Reads,
) -> anyhow::Result<Snapshot> {
    let recent = now - Duration::minutes(opts.stall_minutes.max(30));
    let live_keys: HashSet<(SeatId, SessionId)> = live
        .iter()
        .map(|l| (l.seat.clone(), l.session_id.clone()))
        .collect();

    let mut rows: Vec<AgentRow> = Vec::new();
    for row in idx.live_agent_rows(recent, now - Duration::hours(24), &live_keys)? {
        let is_live_main = row.agent_id.is_empty()
            && live_keys.contains(&(row.seat.clone(), row.session_id.clone()));
        let recent_enough = row.last_ts.is_some_and(|t| t >= ms(recent));
        if row.agent_id.is_empty() && !is_live_main && !recent_enough {
            continue;
        }
        rows.push(row);
    }
    // Live sessions that have no transcript yet still show up.
    let mut labels: Option<std::collections::HashMap<SeatId, String>> = None;
    for l in live {
        if !rows
            .iter()
            .any(|r| r.agent_id.is_empty() && r.seat == l.seat && r.session_id == l.session_id)
        {
            if labels.is_none() {
                labels = Some(idx.seat_labels()?.into_iter().collect());
            }
            let seat_label = labels
                .as_ref()
                .and_then(|m| m.get(&l.seat).cloned())
                .unwrap_or_else(|| l.seat.to_string());
            rows.push(AgentRow {
                seat: l.seat.clone(),
                seat_label,
                harness: l.harness,
                session_id: l.session_id.clone(),
                agent_id: String::new(),
                kind: "main".into(),
                agent_type: None,
                description: None,
                parent_agent_id: None,
                depth: Some(0),
                requested_model: None,
                effort: None,
                finished: true,
                last_ts: None,
                first_ts: Some(l.started_at.timestamp_millis()),
                title: l.name.clone(),
                cwd: Some(l.cwd.clone()),
            });
        }
    }
    let projects = idx.project_dirs()?;
    let spark_start = now - Duration::minutes(15);

    let mut agents = Vec::new();
    let mut next: HashMap<AgentOf, AgentReads> = HashMap::new();
    for r in &rows {
        let key = AgentKey {
            seat: &r.seat,
            session_id: &r.session_id,
            agent_id: &r.agent_id,
        };
        // Nothing new since the previous snapshot: its reads stand (heavy ticks read again).
        let seq = idx.agent_seq(key);
        let k = (r.seat.clone(), r.session_id.clone(), r.agent_id.clone());
        let kept = reads
            .agents
            .remove(&k)
            .filter(|o| prev.is_some() && o.seq == seq);
        let got = match kept {
            Some(o) => o,
            None => {
                let last_req = idx.last_request(key)?.unwrap_or_default();
                let tool = match last_req.ts {
                    Some(t) => idx.open_tool(key, t)?,
                    None => None,
                };
                let mut spark = vec![0.0; 15];
                for (i, c) in idx.agent_spend_per_minute(key, spark_start)? {
                    if (0..15).contains(&i) {
                        spark[i as usize] = c;
                    }
                }
                AgentReads {
                    seq,
                    totals: idx.agent_totals(key)?,
                    tool,
                    done_ts: idx.last_tool_done(key)?,
                    branch: idx.agent_branch(key)?,
                    last_req,
                    spark,
                }
            }
        };
        next.insert(k, got.clone());
        let AgentReads {
            last_req,
            totals: (requests, cost),
            tool,
            done_ts,
            spark,
            branch,
            ..
        } = got;
        let unpriced = requests > 0 && cost == 0.0 && last_req.priced == Some(0);
        let req_ts = last_req.ts;
        let last = [r.last_ts, req_ts, done_ts]
            .into_iter()
            .flatten()
            .max()
            .map(from_ms)
            .unwrap_or(now);
        let live_session = live
            .iter()
            .find(|l| l.seat == r.seat && l.session_id == r.session_id);
        let Some((state, since)) = agent_state(
            &StateInputs {
                is_main: r.agent_id.is_empty(),
                finished: r.finished,
                session: live_session,
                tool: tool.as_ref(),
                last,
            },
            opts,
            now,
        ) else {
            continue;
        };
        let name = if r.agent_id.is_empty() {
            live_session
                .and_then(|l| l.name.clone())
                .or_else(|| r.title.clone())
                .unwrap_or_else(|| r.session_id.short().to_string())
        } else {
            r.description.clone().unwrap_or_else(|| r.agent_id.clone())
        };
        let project_key = r
            .cwd
            .as_ref()
            .and_then(|c| projects.get(c))
            .map(|p| crate::core::ids::ProjectKey::from_stored(p.key.clone()));
        let project = r
            .cwd
            .as_ref()
            .and_then(|c| projects.get(c).map(|p| p.name.clone()))
            .unwrap_or_else(|| {
                r.cwd
                    .as_deref()
                    .map(|c| c.rsplit('/').next().unwrap_or(c).to_string())
                    .unwrap_or_default()
            });
        // The agent's own dir (a subagent may work in a worktree of its own), else the session's.
        let cwd = last_req.cwd.as_deref().or(r.cwd.as_deref());
        let checkout = Checkout::of(cwd, cwd.and_then(|c| projects.get(c)), branch);
        agents.push(Agent {
            seat: r.seat.clone(),
            seat_label: r.seat_label.clone(),
            harness: live_session.map_or(r.harness, |l| l.harness),
            session_id: r.session_id.clone(),
            agent_id: r.agent_id.clone(),
            parent: if r.agent_id.is_empty() {
                None
            } else {
                Some(r.parent_agent_id.clone().unwrap_or_default())
            },
            depth: r
                .depth
                .map(|d| d as u32)
                .unwrap_or(if r.agent_id.is_empty() { 0 } else { 1 }),
            name,
            agent_type: r
                .agent_type
                .clone()
                .or_else(|| (r.kind == "workflow").then(|| "workflow".to_string())),
            project,
            project_key,
            checkout,
            model: last_req.model.or_else(|| r.requested_model.clone()),
            effort: last_req.effort.or_else(|| r.effort.clone()),
            state,
            since,
            last_activity: last,
            cost,
            requests,
            unpriced,
            context_tokens: last_req.context_tokens.unwrap_or(0).max(0) as u64,
            spark,
            lane: Vec::new(),
            pane: live_session.and_then(|l| l.pane.clone()),
            pid: live_session.and_then(|l| (l.pid > 0).then_some(l.pid)),
            account: last_req.account,
            first_seen: r.first_ts.map(from_ms).unwrap_or(last),
            session_cost: 0.0,
            session_burn: 0.0,
            cpu: 0.0,
            memory: 0,
            last_request_ts: req_ts,
        });
    }

    reads.agents = next;

    // Live sessions that work in the same checkout (no worktree between them); only those
    // sessions' agents carry the count.
    let at_work = |a: &Agent| {
        live_keys.contains(&(a.seat.clone(), a.session_id.clone()))
            && !matches!(a.state, State::Done | State::Gone)
    };
    let shared =
        checkout::sessions_per_root(agents.iter().filter(|a| at_work(a)).filter_map(|a| {
            let c = a.checkout.as_ref()?;
            Some(((&a.seat, &a.session_id), c.root.as_str()))
        }));
    let counts: Vec<u32> = agents
        .iter()
        .map(|a| match (&a.checkout, at_work(a)) {
            (Some(c), true) => shared.get(&c.root).copied().unwrap_or(0),
            (Some(_), false) | (None, _) => 0,
        })
        .collect();
    for (a, n) in agents.iter_mut().zip(counts) {
        if let Some(c) = a.checkout.as_mut() {
            c.sessions = n;
        }
    }

    // Main threads waiting on their background agents (`state::waiting_on_agents`).
    let waiting = state::waiting_on_agents(
        &agents
            .iter()
            .map(|a| state::Member {
                main: a.is_main(),
                session: (a.seat.as_str(), a.session_id.as_str()),
                state: &a.state,
            })
            .collect::<Vec<_>>(),
    );
    for (a, n) in agents.iter_mut().zip(waiting) {
        if n > 0 {
            a.state = State::WaitingAgents(n);
        }
    }

    // Activity lanes: computed with the slow parts, carried over in between.
    match prev {
        Some(p) => {
            for a in agents.iter_mut() {
                if let Some(o) = p.agents.iter().find(|o| {
                    o.seat == a.seat && o.session_id == a.session_id && o.agent_id == a.agent_id
                }) {
                    a.lane = o.lane.clone();
                }
            }
        }
        None => {
            for a in agents.iter_mut() {
                a.lane = lane(idx, a, now).unwrap_or_default();
            }
        }
    }
    // Session totals for the main rows.
    if let Some(p) = prev {
        for a in agents.iter_mut().filter(|a| a.is_main()) {
            match p
                .agents
                .iter()
                .find(|o| o.is_main() && o.seat == a.seat && o.session_id == a.session_id)
            {
                Some(o) => {
                    a.session_cost = o.session_cost.max(a.cost);
                    a.session_burn = o.session_burn;
                }
                // A session first seen on a light tick: read once now (from the next tick on it
                // is carried over), not shown at $0 until the next heavy tick.
                None => {
                    let (total, hour) =
                        idx.session_spend(&a.seat, &a.session_id, now - Duration::hours(1))?;
                    a.session_cost = total.max(a.cost);
                    a.session_burn = hour;
                }
            }
            if let Some(n) = a.pid.and_then(|pid| procs.nodes.get(&pid)) {
                a.cpu = n.cpu;
                a.memory = n.memory;
            }
        }
    } else {
        // The last hour of every session in one read; a session's whole spend only when the
        // session was written or the prices changed since it was last read.
        let hour = idx.spend_by_session_since(now - Duration::hours(1))?;
        let fp = idx.meta("price_fp")?;
        let mut kept = std::mem::take(&mut reads.sessions);
        for a in agents.iter_mut().filter(|a| a.is_main()) {
            let k = (a.seat.clone(), a.session_id.clone());
            let seq = idx.session_seq(&a.seat, &a.session_id);
            let total = match kept.remove(&k).filter(|(s, f, _)| *s == seq && *f == fp) {
                Some((_, _, t)) => t,
                None => idx.session_total(&a.seat, &a.session_id)?,
            };
            reads.sessions.insert(k.clone(), (seq, fp.clone(), total));
            a.session_cost = total;
            a.session_burn = hour.get(&k).copied().unwrap_or(0.0);
            if let Some(n) = a.pid.and_then(|p| procs.nodes.get(&p)) {
                a.cpu = n.cpu;
                a.memory = n.memory;
            }
        }
    }
    let agents = order_forest(agents);
    let mut snap = Snapshot {
        taken_at: Some(now),
        agents,
        ..Default::default()
    };

    // Burn per clock minute by model, and each model's last 60 minutes.
    let burn = idx.spend_per_minute_by_model(burn_start(now), now - Duration::minutes(60))?;
    for (i, model, c) in burn.minutes {
        if (0..BURN_MINUTES as i64).contains(&i) {
            let name = crate::core::models::short_name(&model);
            snap.burn
                .entry(name)
                .or_insert_with(|| vec![0.0; BURN_MINUTES])[i as usize] += c;
        }
    }
    for (model, c) in burn.hour {
        *snap
            .burn_hour
            .entry(crate::core::models::short_name(&model))
            .or_default() += c;
    }
    snap.burn_per_hour = snap.burn_hour.values().sum();
    let midnight =
        crate::core::period::local_midnight(now.with_timezone(&chrono::Local).date_naive());
    snap.today_cost = idx.spend_since(midnight)?;
    // Recently finished agents (the history under the live tree).
    if let Some(p) = prev {
        snap.recent_done = p.recent_done.clone();
        snap.conflicts = p.conflicts.clone();
    } else {
        snap.conflicts = idx
            .edit_conflicts(now - Duration::minutes(30))
            .unwrap_or_default();
        for d in idx.recently_done(now - Duration::hours(6))? {
            let DoneAgent {
                name,
                cwd,
                first_ts: first,
                last_ts: last,
                cost,
                model,
            } = d;
            // A worktree is named after its project (`shop/fix-1`).
            let project = cwd
                .as_ref()
                .and_then(|c| projects.get(c))
                .map(|p| match Checkout::of(None, Some(p), None) {
                    Some(c) => c.label(&p.name),
                    None => p.name.clone(),
                })
                .unwrap_or_else(|| {
                    cwd.as_deref()
                        .map(|c| c.rsplit('/').next().unwrap_or(c).to_string())
                        .unwrap_or_default()
                });
            let finished = last.map(from_ms).unwrap_or(now);
            snap.recent_done.push(DoneRow {
                name,
                project,
                model,
                started: first.map(from_ms).unwrap_or(finished),
                finished,
                cost: cost.unwrap_or(0.0),
            });
        }
    }
    snap.errors_last_hour = idx.api_error_counts(now - Duration::minutes(60))?;
    Ok(snap)
}

/// What happened before the dashboard started, so the event log is not empty on launch. The
/// dashboard words it (`PastEvent::kind` carries the facts, not text).
#[derive(Debug, Clone, PartialEq)]
pub struct PastEvent {
    pub at: DateTime<Utc>,
    pub kind: PastKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PastKind {
    Spawned {
        name: String,
        /// Short model name, empty when unknown.
        model: String,
        effort: Option<String>,
    },
    Done {
        name: String,
        took: Option<Duration>,
        cost: f64,
    },
    ApiError {
        kind: String,
        /// First line of the error text.
        text: Option<String>,
    },
    Prompt {
        /// Session title, empty when it has none.
        title: String,
        chars: i64,
    },
}

/// Agents spawned and finished, API errors and the user's prompts since `since`, newest first.
pub fn past_events(
    idx: &Index,
    since: DateTime<Utc>,
    limit: usize,
) -> anyhow::Result<Vec<PastEvent>> {
    let since_ms = ms(since);
    let mut out = Vec::new();
    for e in idx.agent_events(since)? {
        let model = e
            .model
            .as_deref()
            .map(crate::core::models::short_name)
            .unwrap_or_default();
        if let Some(f) = e.first_ts.filter(|f| *f >= since_ms) {
            out.push(PastEvent {
                at: from_ms(f),
                kind: PastKind::Spawned {
                    name: e.name.clone(),
                    model,
                    effort: e.effort.clone(),
                },
            });
        }
        if let (true, Some(l)) = (e.finished, e.last_ts.filter(|l| *l >= since_ms)) {
            out.push(PastEvent {
                at: from_ms(l),
                kind: PastKind::Done {
                    name: e.name,
                    took: e.first_ts.map(|f| from_ms(l) - from_ms(f)),
                    cost: e.cost.unwrap_or(0.0),
                },
            });
        }
    }
    for (ts, kind, text) in idx.api_errors_since(since)? {
        out.push(PastEvent {
            at: from_ms(ts),
            kind: PastKind::ApiError {
                kind,
                text: text.map(|t| t.lines().next().unwrap_or("").to_string()),
            },
        });
    }
    for (ts, chars, title) in idx.prompts_since(since)? {
        out.push(PastEvent {
            at: from_ms(ts),
            kind: PastKind::Prompt { title, chars },
        });
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.at));
    out.truncate(limit);
    Ok(out)
}

/// A stable tree: running sessions before exited ones, each in the order it started, followed by
/// its agents in spawn order. States change every second; positions must not, or the selection
/// and the reader's eye jump around.
fn order_forest(mut agents: Vec<Agent>) -> Vec<Agent> {
    agents.sort_by(|a, b| {
        (a.state == State::Gone)
            .cmp(&(b.state == State::Gone))
            .then(a.first_seen.cmp(&b.first_seen))
            .then(a.agent_id.cmp(&b.agent_id))
    });
    let mains: Vec<Agent> = agents.iter().filter(|a| a.is_main()).cloned().collect();
    let mut used = HashSet::new();
    let mut out = Vec::new();
    fn push_children(
        all: &[Agent],
        seat: &SeatId,
        session: &SessionId,
        parent: &str,
        depth: u32,
        used: &mut HashSet<String>,
        out: &mut Vec<Agent>,
    ) {
        for a in all.iter().filter(|a| {
            !a.is_main()
                && a.seat == *seat
                && a.session_id == *session
                && a.parent.as_deref().unwrap_or("") == parent
        }) {
            if used.insert(a.key()) {
                let mut a = a.clone();
                a.depth = depth;
                let id = a.agent_id.clone();
                out.push(a);
                push_children(all, seat, session, &id, depth + 1, used, out);
            }
        }
    }
    for m in &mains {
        used.insert(m.key());
        out.push(m.clone());
        push_children(&agents, &m.seat, &m.session_id, "", 1, &mut used, &mut out);
    }
    // Agents whose main thread is not shown (a session no longer running).
    for a in agents {
        if used.insert(a.key()) {
            out.push(a);
        }
    }
    out
}

/// The last text the agent wrote (read from the tail of its transcript by its harness's reader).
pub fn last_message(idx: &Index, a: &Agent) -> Option<String> {
    let path = idx.transcript_path(key_of(a)).ok().flatten()?;
    let path = std::path::Path::new(&path);
    match a.harness {
        Harness::Agy => crate::agy::transcript::last_message(path),
        Harness::Codex => crate::codex::rollout::last_message(path),
        Harness::Claude => crate::claude::transcript::last_message(path),
    }
}

/// The latest tool calls of one agent, newest first.
/// What an agent did during one minute, most telling first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
pub enum Lane {
    #[default]
    Quiet,
    Thinking,
    Tool,
    /// Waiting for subagents (the `Agent` tool).
    Agents,
    /// Waiting for the user (a question or a plan).
    You,
}

pub const LANE_MINUTES: i64 = 30;

fn lane(idx: &Index, a: &Agent, now: DateTime<Utc>) -> anyhow::Result<Vec<Lane>> {
    let start = now - Duration::minutes(LANE_MINUTES);
    let mut out = vec![Lane::Quiet; LANE_MINUTES as usize];
    let slot = |t: i64| ((t - ms(start)) / 60_000).clamp(0, LANE_MINUTES - 1) as usize;
    for t in idx.agent_request_times(key_of(a), start)? {
        let i = slot(t);
        out[i] = out[i].max(Lane::Thinking);
    }
    for (t0, t1, name) in idx.agent_tool_spans(key_of(a), start, now)? {
        let kind = match ToolClass::of(&name) {
            ToolClass::AskUser | ToolClass::Plan => Lane::You,
            ToolClass::Agent => Lane::Agents,
            ToolClass::Edit | ToolClass::Read | ToolClass::Shell | ToolClass::Other => Lane::Tool,
        };
        for v in &mut out[slot(t0)..=slot(t1.max(t0))] {
            *v = (*v).max(kind);
        }
    }
    if matches!(a.state, State::NeedsYou(_)) {
        out[LANE_MINUTES as usize - 1] = Lane::You;
    }
    Ok(out)
}

/// One request of an agent: when, what it cost, how big the context was.
#[derive(Debug, Clone, Copy)]
pub struct LifePoint {
    pub ts: DateTime<Utc>,
    pub cost: f64,
    pub context: u64,
}

/// The agent's requests, oldest first (the last 2000).
pub fn life(idx: &Index, a: &Agent) -> anyhow::Result<Vec<LifePoint>> {
    let mut v: Vec<LifePoint> = idx
        .agent_requests(key_of(a), 2000)?
        .into_iter()
        .map(|r| LifePoint {
            ts: from_ms(r.ts),
            cost: r.cost,
            context: r.context.max(0) as u64,
        })
        .collect();
    v.reverse();
    Ok(v)
}

pub fn tool_timeline(idx: &Index, a: &Agent, limit: usize) -> anyhow::Result<Vec<ToolLine>> {
    Ok(idx
        .agent_tool_calls(key_of(a), limit)?
        .into_iter()
        .map(|t| ToolLine {
            ts: from_ms(t.ts),
            name: t.name,
            summary: t.summary.unwrap_or_default(),
            done: t.done_ts.map(from_ms),
            is_error: t.is_error,
        })
        .collect())
}

pub fn key_of(a: &Agent) -> AgentKey<'_> {
    AgentKey {
        seat: &a.seat,
        session_id: &a.session_id,
        agent_id: &a.agent_id,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_burn_per_model_adds_up_to_the_burn_per_hour() {
        // opus-5.5: $1.75 in the hour; gem-4-neon: $0.30; gem-4-hydrogen spent nothing.
        let mut snap = Snapshot::default();
        snap.burn_hour.insert("opus-5.5".into(), 1.75);
        snap.burn_hour.insert("gem-4-neon".into(), 0.3);
        snap.burn_hour.insert("gem-4-hydrogen".into(), 0.0);
        snap.burn_per_hour = 2.05;
        let rates = snap.burn_by_model();
        assert_eq!(rates, vec![("opus-5.5", 1.75), ("gem-4-neon", 0.3)]);
        let sum: f64 = rates.iter().map(|(_, r)| r).sum();
        assert!((sum - snap.burn_per_hour).abs() < 1e-9);
    }

    use super::*;

    fn agent(id: &str, parent: Option<&str>, state: State) -> Agent {
        Agent {
            seat: SeatId::from_stored("0123456789abcdef"),
            seat_label: "s".into(),
            harness: Harness::Claude,
            session_id: SessionId::new("x"),
            agent_id: id.into(),
            parent: parent.map(str::to_string),
            depth: 0,
            name: id.into(),
            agent_type: None,
            project: "p".into(),
            project_key: None,
            checkout: None,
            model: None,
            effort: None,
            state,
            since: Utc::now(),
            last_activity: Utc::now(),
            cost: 0.0,
            requests: 0,
            unpriced: false,
            context_tokens: 0,
            spark: vec![],
            pane: None,
            pid: None,
            account: None,
            first_seen: Utc::now(),
            session_cost: 0.0,
            session_burn: 0.0,
            cpu: 0.0,
            memory: 0,
            last_request_ts: None,
            lane: Vec::new(),
        }
    }

    /// The BURN chart's bins are clock minutes. They were counted from the current second, so
    /// every second a few requests went into the next bin and the bars jittered while the chart
    /// hardly moved. Now a request keeps its bin while the minute lasts, every bin steps one to
    /// the left when it turns, and the rates are still the last 60 minutes to the second.
    #[test]
    fn burn_bins_are_clock_minutes() {
        use crate::core::pricing::PriceBook;
        use crate::index::scenarios::{World, assistant, owner};
        // A whole minute in the past, so every request below is too.
        let base = burn_start(Utc::now()) + Duration::minutes(50);
        assert_eq!(burn_start(base), base - Duration::minutes(60));
        let at = |id: &str, t: DateTime<Utc>| {
            let t = t.to_rfc3339();
            assistant(id, &t, "claude-sonnet-5-5", "/w/shop", (20_000, 2_000), &[])
        };
        let w = World::new(&["work"]);
        // Three requests of the same size, so of the same cost c.
        let early = base - Duration::minutes(60) + Duration::seconds(30);
        let mid = base - Duration::minutes(30) + Duration::seconds(10);
        let late = base - Duration::minutes(2) + Duration::seconds(50);
        w.append(
            "work",
            "s1",
            &[
                owner("acc-a"),
                at("m1", early),
                at("m2", mid),
                at("m3", late),
            ],
        );
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        let procs = crate::procs::ProcTable::empty();
        let opts = Options::default();
        let snap = |now: DateTime<Utc>| {
            let s = build(&idx, &[], &procs, &opts, now, None, &mut Reads::default()).unwrap();
            assert_eq!(s.burn.len(), 1, "one model");
            let bins = s.burn.values().next().unwrap().clone();
            assert_eq!(bins.len(), BURN_MINUTES);
            (bins, s.burn_per_hour)
        };
        let lit =
            |bins: &[f64]| -> Vec<usize> { (0..bins.len()).filter(|i| bins[*i] > 0.0).collect() };
        let (a, rate_a) = snap(base + Duration::seconds(5));
        let (b, rate_b) = snap(base + Duration::seconds(45));
        let (c, _) = snap(base + Duration::seconds(60));
        // From base − 60 min: early (+30 s) in bin 0, mid (+30 min 10 s) in 30, late
        // (+58 min 50 s) in 58.
        assert_eq!(lit(&a), vec![0, 30, 58]);
        assert_eq!(a, b, "the bins stay put within the minute");
        // From base − 59 min: early is out of the chart, the others one bin to the left.
        assert_eq!(lit(&c), vec![29, 57]);
        assert_eq!(c[29], a[30]);
        // At :05 the hour reaches back to base − 60 min + 5 s and holds early (at +30 s); at :45
        // it no longer does: 3c, then 2c.
        let cost = a[30];
        assert!(cost > 0.0);
        assert!(
            (rate_a - 3.0 * cost).abs() < 1e-12,
            "{rate_a} vs 3 × {cost}"
        );
        assert!(
            (rate_b - 2.0 * cost).abs() < 1e-12,
            "{rate_b} vs 2 × {cost}"
        );
        assert!((burn_offset(base + Duration::seconds(45)) - 0.75).abs() < 1e-9);
        assert_eq!(burn_offset(base), 0.0);
    }

    /// Review L7: a session that shows up between two heavy ticks has its spend at once.
    #[test]
    fn a_session_first_seen_on_a_light_tick_has_its_spend() {
        use crate::core::pricing::PriceBook;
        use crate::index::scenarios::{World, assistant, owner};
        let w = World::new(&["work"]);
        let now = Utc::now();
        let line = |id: &str, ago: i64| {
            let t = (now - Duration::seconds(ago)).to_rfc3339();
            assistant(id, &t, "claude-sonnet-5-5", "/w/shop", (20_000, 2_000), &[])
        };
        let book = PriceBook::builtin();
        w.append("work", "s1", &[owner("acc-a"), line("m1", 90)]);
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &book);
        let procs = crate::procs::ProcTable::empty();
        let opts = Options::default();
        let mut reads = Reads::default();
        let heavy = build(&idx, &[], &procs, &opts, now, None, &mut reads).unwrap();
        w.append(
            "work",
            "s2",
            &[owner("acc-a"), line("m2", 30), line("m3", 20)],
        );
        w.refresh(&mut idx, &[], &book);
        let light = build(&idx, &[], &procs, &opts, now, Some(&heavy), &mut reads).unwrap();
        let main = |s: &Snapshot, sid: &str| {
            s.agents
                .iter()
                .find(|a| a.is_main() && a.session_id.as_str() == sid)
                .map(|a| (a.session_cost, a.session_burn))
                .unwrap()
        };
        let (cost, burn) = main(&light, "s2");
        let (total, _) = idx
            .session_spend(&w.id("work"), &SessionId::new("s2"), now)
            .unwrap();
        assert!(
            total > 0.0 && (cost - total).abs() < 1e-12,
            "{cost} vs {total}"
        );
        assert!((burn - total).abs() < 1e-12, "all of it in the last hour");
        assert_eq!(main(&light, "s1"), main(&heavy, "s1"), "carried over");
    }

    #[test]
    fn forest_order_nests_children_under_parents() {
        let v = vec![
            agent("b2", Some("b"), State::Thinking),
            agent("", None, State::Idle),
            agent("b", Some(""), State::Thinking),
            agent("c", Some(""), State::Done),
        ];
        let out = order_forest(v);
        let ids: Vec<(&str, u32)> = out.iter().map(|a| (a.agent_id.as_str(), a.depth)).collect();
        assert_eq!(ids, [("", 0), ("b", 1), ("b2", 2), ("c", 1)]);
    }
}

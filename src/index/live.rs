//! Typed reads for the live view (`crate::live`): the SQL over the index tables stays here, so the
//! schema has one owner. Times are epoch milliseconds, as stored.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

use super::{Index, ms, settled};
use crate::core::harness::Harness;
use crate::core::ids::{SeatId, SessionId};
use crate::core::period::Span;

/// One agent (or main thread, `agent_id` empty) with its session's title and directory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentRow {
    pub seat: SeatId,
    /// The seat's label, for display (`seats`).
    pub seat_label: String,
    pub harness: Harness,
    pub session_id: SessionId,
    pub agent_id: String,
    pub kind: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub parent_agent_id: Option<String>,
    pub depth: Option<i64>,
    pub requested_model: Option<String>,
    pub effort: Option<String>,
    pub finished: bool,
    pub last_ts: Option<i64>,
    pub first_ts: Option<i64>,
    pub title: Option<String>,
    pub cwd: Option<String>,
}

/// A request's time, model and cost (0 while unpriced, as `TOTAL` counts it).
type CostAt = (i64, String, f64);

/// Which agent a per-agent read is about.
/// What `Index::spend_per_minute_by_model` read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MinuteSpend {
    /// (minute index from the start, model, cost).
    pub minutes: Vec<(i64, String, f64)>,
    /// Each model's spend since the cutoff.
    pub hour: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Copy)]
pub struct AgentKey<'a> {
    pub seat: &'a SeatId,
    pub session_id: &'a SessionId,
    pub agent_id: &'a str,
}

/// The agent's latest request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LastRequest {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub ts: Option<i64>,
    /// Prompt size: input plus cache writes and reads.
    pub context_tokens: Option<i64>,
    pub account: Option<String>,
    /// `Some(1)` when priced, `Some(0)` when the model has no known price, `None` before pricing.
    pub priced: Option<i64>,
    /// The working directory of the request.
    pub cwd: Option<String>,
}

/// What `projects` says about a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDir {
    pub key: String,
    pub name: String,
    /// The linked worktree's root, when the dir is inside one.
    pub worktree: Option<String>,
}

/// A tool call that has not finished.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenTool {
    pub name: String,
    pub summary: Option<String>,
    pub ts: i64,
}

/// A finished agent, for the recently-done list.
#[derive(Debug, Clone, PartialEq)]
pub struct DoneAgent {
    pub name: String,
    pub cwd: Option<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub cost: Option<f64>,
    pub model: Option<String>,
}

/// An agent spawned or finished since some time, for the event log.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEventRow {
    pub name: String,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub finished: bool,
    /// The model of its first request.
    pub model: Option<String>,
    pub cost: Option<f64>,
    pub effort: Option<String>,
}

/// One request of an agent: time, cost (0 when unpriced) and prompt size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RequestPoint {
    pub ts: i64,
    pub cost: f64,
    pub context: i64,
}

/// One tool call of an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRow {
    pub ts: i64,
    pub name: String,
    pub summary: Option<String>,
    pub done_ts: Option<i64>,
    pub is_error: bool,
}

/// An agent's rolled-up part (`agent_rollup`), by the agent of `?1`, `?2`, `?3`: an agent
/// running for longer than the raw window, or a session resumed after weeks.
macro_rules! rolled {
    () => {
        "FROM agent_rollup WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3"
    };
}
/// The same, for the agent `a` of the outer query (as `x`).
macro_rules! rolled_of_a {
    () => {
        "FROM agent_rollup x WHERE x.seat = a.seat AND x.session_id = a.session_id AND x.agent_id = a.agent_id"
    };
}

impl Index {
    /// Agents active since `recent`, unfinished ones active since `unfinished_since`, and the
    /// main threads of the `live` sessions (seat, session id) however long they have been idle.
    /// Bounded in time: `agents` keeps every agent ever seen (a dimension table), and this runs
    /// every second.
    pub fn live_agent_rows(
        &self,
        recent: DateTime<Utc>,
        unfinished_since: DateTime<Utc>,
        live: &HashSet<(SeatId, SessionId)>,
    ) -> anyhow::Result<Vec<AgentRow>> {
        macro_rules! columns {
            () => {
                "SELECT a.seat, a.session_id, a.agent_id, a.kind, a.agent_type, a.description, a.parent_agent_id, a.depth,
                    a.requested_model, a.effort, a.finished, a.last_ts, COALESCE(s.title, s.ai_title), s.cwd, a.first_ts,
                    COALESCE((SELECT st.label FROM seats st WHERE st.seat = a.seat), a.seat),
                    COALESCE((SELECT st.harness FROM seats st WHERE st.seat = a.seat), 'claude')
             FROM agents a LEFT JOIN sessions s ON s.seat = a.seat AND s.session_id = a.session_id"
            };
        }
        let row = |r: &rusqlite::Row| {
            let h_raw: String = r.get(16)?;
            Ok(AgentRow {
                seat: r.get(0)?,
                session_id: r.get(1)?,
                agent_id: r.get(2)?,
                kind: r.get(3)?,
                agent_type: r.get(4)?,
                description: r.get(5)?,
                parent_agent_id: r.get(6)?,
                depth: r.get(7)?,
                requested_model: r.get(8)?,
                effort: r.get(9)?,
                finished: r.get::<_, i64>(10)? != 0,
                last_ts: r.get(11)?,
                title: r.get(12)?,
                cwd: r.get(13)?,
                first_ts: r.get(14)?,
                seat_label: r.get(15)?,
                harness: Harness::from_key(&h_raw).unwrap_or(Harness::Claude),
            })
        };
        let mut out: Vec<AgentRow> = self
            .conn
            .prepare_cached(concat!(
                columns!(),
                " WHERE a.last_ts >= MIN(?1, ?2) AND (a.last_ts >= ?1 OR a.finished = 0)"
            ))?
            .query_map(params![ms(recent), ms(unfinished_since)], row)?
            .collect::<Result<_, _>>()?;
        let mut main = self.conn.prepare_cached(concat!(
            columns!(),
            " WHERE a.seat = ?1 AND a.session_id = ?2 AND a.agent_id = ''"
        ))?;
        let mut sessions: Vec<&(SeatId, SessionId)> = live.iter().collect();
        sessions.sort();
        for (seat, session_id) in sessions {
            let listed = out
                .iter()
                .any(|r| r.agent_id.is_empty() && &r.seat == seat && &r.session_id == session_id);
            if !listed && let Some(r) = main.query_row(params![seat, session_id], row).optional()? {
                out.push(r);
            }
        }
        // Sorted here: an `ORDER BY` makes SQLite walk the primary key instead of the time index.
        out.sort_by(|x, y| {
            (&x.seat, &x.session_id, &x.agent_id).cmp(&(&y.seat, &y.session_id, &y.agent_id))
        });
        Ok(out)
    }

    /// The project (and worktree) of every working directory.
    pub fn project_dirs(&self) -> anyhow::Result<HashMap<String, ProjectDir>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT cwd, key, name, worktree FROM projects")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get(0)?,
                ProjectDir {
                    key: r.get(1)?,
                    name: r.get(2)?,
                    worktree: r.get(3)?,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The git branch the agent works on now (its latest `branches` row).
    pub fn agent_branch(&self, a: AgentKey) -> anyhow::Result<Option<String>> {
        let mut st = self.conn.prepare_cached(
            "SELECT branch FROM branches WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3
             ORDER BY from_ts DESC LIMIT 1",
        )?;
        Ok(st
            .query_row(params![a.seat, a.session_id, a.agent_id], |x| x.get(0))
            .optional()?)
    }

    pub fn last_request(&self, a: AgentKey) -> anyhow::Result<Option<LastRequest>> {
        let mut st = self.conn.prepare_cached(
            "SELECT model, effort, ts, input + cw5 + cw1h + cread, account_uuid, priced, cwd FROM requests
             WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3 AND model != '<synthetic>'
             ORDER BY ts DESC LIMIT 1",
        )?;
        Ok(st
            .query_row(params![a.seat, a.session_id, a.agent_id], |x| {
                Ok(LastRequest {
                    model: x.get(0)?,
                    effort: x.get(1)?,
                    ts: x.get(2)?,
                    context_tokens: x.get(3)?,
                    account: x.get(4)?,
                    priced: x.get(5)?,
                    cwd: x.get(6)?,
                })
            })
            .optional()?)
    }

    /// Requests and their total cost over the agent's lifetime (its rolled-up part included).
    pub fn agent_totals(&self, a: AgentKey) -> anyhow::Result<(u64, f64)> {
        let mut st = self.conn.prepare_cached(concat!(
            "SELECT COUNT(*) + COALESCE((SELECT requests ",
            rolled!(),
            "), 0), TOTAL(cost) + COALESCE((SELECT cost ",
            rolled!(),
            "), 0) FROM requests WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3"
        ))?;
        let (n, cost): (i64, f64) = st
            .query_row(params![a.seat, a.session_id, a.agent_id], |x| {
                Ok((x.get(0)?, x.get(1)?))
            })?;
        Ok((n.max(0) as u64, cost))
    }

    /// The newest tool call started at or after `since_ms` that has not finished.
    pub fn open_tool(&self, a: AgentKey, since_ms: i64) -> anyhow::Result<Option<OpenTool>> {
        let mut st = self.conn.prepare_cached(
            "SELECT name, summary, ts FROM tool_calls WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3 AND done_ts IS NULL AND ts >= ?4
             ORDER BY ts DESC LIMIT 1",
        )?;
        Ok(st
            .query_row(params![a.seat, a.session_id, a.agent_id, since_ms], |x| {
                Ok(OpenTool {
                    name: x.get(0)?,
                    summary: x.get(1)?,
                    ts: x.get(2)?,
                })
            })
            .optional()?)
    }

    /// When the agent's last tool call finished.
    pub fn last_tool_done(&self, a: AgentKey) -> anyhow::Result<Option<i64>> {
        let mut st = self.conn.prepare_cached(
            "SELECT MAX(done_ts) FROM tool_calls WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3",
        )?;
        Ok(st.query_row(params![a.seat, a.session_id, a.agent_id], |x| x.get(0))?)
    }

    /// Spend per minute since `start`: (minute index from `start`, cost).
    pub fn agent_spend_per_minute(
        &self,
        a: AgentKey,
        start: DateTime<Utc>,
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT (ts - ?4) / 60000, TOTAL(cost) FROM requests WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3 AND ts >= ?4 GROUP BY 1",
        )?;
        let rows = st.query_map(params![a.seat, a.session_id, a.agent_id, ms(start)], |x| {
            Ok((x.get(0)?, x.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Times of the agent's requests since `start`.
    pub fn agent_request_times(
        &self,
        a: AgentKey,
        start: DateTime<Utc>,
    ) -> anyhow::Result<Vec<i64>> {
        let mut st = self.conn.prepare_cached(
            "SELECT ts FROM requests WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3 AND ts >= ?4",
        )?;
        let rows = st.query_map(params![a.seat, a.session_id, a.agent_id, ms(start)], |r| {
            r.get(0)
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Tool calls running at some point since `start`: (started, finished or `now`, tool name).
    pub fn agent_tool_spans(
        &self,
        a: AgentKey,
        start: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Vec<(i64, i64, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT ts, COALESCE(done_ts, ?5), name FROM tool_calls WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3
             AND COALESCE(done_ts, ?5) >= ?4",
        )?;
        let rows = st.query_map(
            params![a.seat, a.session_id, a.agent_id, ms(start), ms(now)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A whole session's spend (its rolled-up part included), and its spend since `since`, a
    /// recent time (rolled-up spend is older than the raw window).
    pub fn session_spend(
        &self,
        seat: &SeatId,
        session_id: &SessionId,
        since: DateTime<Utc>,
    ) -> anyhow::Result<(f64, f64)> {
        let mut st = self.conn.prepare_cached(
            "SELECT TOTAL(cost) + (SELECT TOTAL(cost) FROM agent_rollup WHERE seat = ?1 AND session_id = ?2),
                    TOTAL(CASE WHEN ts >= ?3 THEN cost ELSE 0 END)
             FROM requests WHERE seat = ?1 AND session_id = ?2",
        )?;
        Ok(st.query_row(params![seat, session_id, ms(since)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?)
    }

    /// A whole session's spend, its rolled-up part included.
    pub fn session_total(&self, seat: &SeatId, session_id: &SessionId) -> anyhow::Result<f64> {
        let mut st = self.conn.prepare_cached(
            "SELECT TOTAL(cost) + (SELECT TOTAL(cost) FROM agent_rollup WHERE seat = ?1 AND session_id = ?2)
             FROM requests WHERE seat = ?1 AND session_id = ?2",
        )?;
        Ok(st.query_row(params![seat, session_id], |r| r.get(0))?)
    }

    /// Spend per session since `since`, a recent time (rolled-up spend is older than the raw
    /// window): one read of the requests since then for every session.
    pub fn spend_by_session_since(
        &self,
        since: DateTime<Utc>,
    ) -> anyhow::Result<HashMap<(SeatId, SessionId), f64>> {
        let mut st = self.conn.prepare_cached(
            "SELECT seat, session_id, TOTAL(cost) FROM requests WHERE ts >= ?1 GROUP BY 1, 2",
        )?;
        let rows = st.query_map(params![ms(since)], |r| {
            Ok(((r.get(0)?, r.get(1)?), r.get(2)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Spend per minute and model since `start`: (minute index from `start`, model, cost),
    /// and each model's spend since `hour_from` (the BURN chart's bins and its rates).
    /// Runs every second over the last hour: the settled rows of that hour are kept between
    /// calls (`settled`), only the last quarter hours are read again.
    pub fn spend_per_minute_by_model(
        &self,
        start: DateTime<Utc>,
        hour_from: DateTime<Utc>,
    ) -> anyhow::Result<MinuteSpend> {
        // The kept rows start an hour before the horizon, so they cover `start` until the
        // horizon moves.
        let from = settled::horizon(self.now()) - chrono::Duration::hours(1);
        let span = Span {
            start: from.min(start).min(hour_from),
            end: DateTime::<Utc>::MAX_UTC,
        };
        let read = |c: &rusqlite::Connection, s: Span| -> anyhow::Result<Rc<Vec<CostAt>>> {
            let mut st = c.prepare_cached(
                "SELECT ts, model, COALESCE(cost, 0.0) FROM requests WHERE ts >= ?1 AND ts < ?2",
            )?;
            let rows = st.query_map(params![ms(s.start), ms(s.end)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            Ok(Rc::new(rows.collect::<Result<_, _>>()?))
        };
        let (old, new) = self.settled_read(&format!("burn_rows|{}", ms(span.start)), span, read)?;
        let (t0, h0) = (ms(start), ms(hour_from));
        let mut by: HashMap<(i64, &str), f64> = HashMap::new();
        let mut hour: HashMap<&str, f64> = HashMap::new();
        for (ts, model, cost) in old.iter().flat_map(|o| o.iter()).chain(new.iter()) {
            if *ts >= t0 {
                *by.entry(((ts - t0) / 60_000, model.as_str())).or_default() += cost;
            }
            if *ts >= h0 {
                *hour.entry(model.as_str()).or_default() += cost;
            }
        }
        Ok(MinuteSpend {
            minutes: by
                .into_iter()
                .map(|((i, m), c)| (i, m.to_string(), c))
                .collect(),
            hour: hour.into_iter().map(|(m, c)| (m.to_string(), c)).collect(),
        })
    }

    /// Everything spent since `since` (its settled part kept between calls, `settled`).
    pub fn spend_since(&self, since: DateTime<Utc>) -> anyhow::Result<f64> {
        let span = Span {
            start: since,
            end: DateTime::<Utc>::MAX_UTC,
        };
        let (old, new) = self.settled_read("spend_since", span, |c, s| {
            Ok(
                c.prepare_cached("SELECT TOTAL(cost) FROM requests WHERE ts >= ?1 AND ts < ?2")?
                    .query_row(params![ms(s.start), ms(s.end)], |r| r.get::<_, f64>(0))?,
            )
        })?;
        Ok(old.map_or(new, |o| o + new))
    }

    /// Agents that finished since `since`, newest first (at most 60).
    pub fn recently_done(&self, since: DateTime<Utc>) -> anyhow::Result<Vec<DoneAgent>> {
        let mut st = self.conn.prepare_cached(concat!(
            "SELECT COALESCE(a.description, a.agent_id), s.cwd, a.first_ts, a.last_ts,
                    (SELECT TOTAL(cost) FROM requests r WHERE r.seat = a.seat AND r.session_id = a.session_id AND r.agent_id = a.agent_id)
                      + COALESCE((SELECT x.cost ", rolled_of_a!(), "), 0),
                    COALESCE((SELECT model FROM requests r WHERE r.seat = a.seat AND r.session_id = a.session_id AND r.agent_id = a.agent_id ORDER BY ts DESC LIMIT 1),
                             (SELECT x.last_model ", rolled_of_a!(), "))
             FROM agents a LEFT JOIN sessions s ON s.seat = a.seat AND s.session_id = a.session_id
             WHERE a.agent_id != '' AND a.finished = 1 AND a.last_ts >= ?1 ORDER BY a.last_ts DESC LIMIT 60"
        ))?;
        let rows = st.query_map(params![ms(since)], |r| {
            Ok(DoneAgent {
                name: r.get(0)?,
                cwd: r.get(1)?,
                first_ts: r.get(2)?,
                last_ts: r.get(3)?,
                cost: r.get(4)?,
                model: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// API errors since `since`, counted by kind.
    pub fn api_error_counts(&self, since: DateTime<Utc>) -> anyhow::Result<Vec<(String, u64)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT kind, COUNT(*) FROM api_errors WHERE ts >= ?1 GROUP BY 1")?;
        let rows = st.query_map(params![ms(since)], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Agents spawned, or finished, since `since`.
    pub fn agent_events(&self, since: DateTime<Utc>) -> anyhow::Result<Vec<AgentEventRow>> {
        let mut st = self.conn.prepare_cached(concat!(
            "SELECT COALESCE(a.description, a.agent_type, a.agent_id), a.first_ts, a.last_ts, a.finished,
                    COALESCE((SELECT x.first_model ", rolled_of_a!(), "),
                             (SELECT model FROM requests r WHERE r.seat = a.seat AND r.session_id = a.session_id AND r.agent_id = a.agent_id ORDER BY ts LIMIT 1)),
                    (SELECT TOTAL(cost) FROM requests r WHERE r.seat = a.seat AND r.session_id = a.session_id AND r.agent_id = a.agent_id)
                      + COALESCE((SELECT x.cost ", rolled_of_a!(), "), 0),
                    a.effort
             FROM agents a WHERE a.agent_id != '' AND (a.first_ts >= ?1 OR (a.finished = 1 AND a.last_ts >= ?1))"
        ))?;
        let rows = st.query_map(params![ms(since)], |r| {
            Ok(AgentEventRow {
                name: r.get(0)?,
                first_ts: r.get(1)?,
                last_ts: r.get(2)?,
                finished: r.get::<_, i64>(3)? == 1,
                model: r.get(4)?,
                cost: r.get(5)?,
                effort: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// API errors since `since`: (time, kind, text).
    pub fn api_errors_since(
        &self,
        since: DateTime<Utc>,
    ) -> anyhow::Result<Vec<(i64, String, Option<String>)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT ts, kind, text FROM api_errors WHERE ts >= ?1")?;
        let rows = st.query_map(params![ms(since)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The user's prompts since `since`: (time, characters, session title or empty).
    pub fn prompts_since(&self, since: DateTime<Utc>) -> anyhow::Result<Vec<(i64, i64, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT p.ts, p.chars, COALESCE(s.title, s.ai_title, '') FROM prompts p
             LEFT JOIN sessions s ON s.seat = p.seat AND s.session_id = p.session_id WHERE p.ts >= ?1",
        )?;
        let rows = st.query_map(params![ms(since)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The transcript file of an agent.
    pub fn transcript_path(&self, a: AgentKey) -> anyhow::Result<Option<String>> {
        let mut st = self.conn.prepare_cached(
            "SELECT path FROM files WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3",
        )?;
        Ok(st
            .query_row(params![a.seat, a.session_id, a.agent_id], |r| r.get(0))
            .optional()?)
    }

    /// The agent's latest `limit` requests, newest first.
    pub fn agent_requests(&self, a: AgentKey, limit: usize) -> anyhow::Result<Vec<RequestPoint>> {
        let mut st = self.conn.prepare_cached(
            "SELECT ts, COALESCE(cost, 0), input + cw5 + cw1h + cread FROM requests
             WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3 ORDER BY ts DESC LIMIT ?4",
        )?;
        let rows = st.query_map(
            params![a.seat, a.session_id, a.agent_id, limit as i64],
            |r| {
                Ok(RequestPoint {
                    ts: r.get(0)?,
                    cost: r.get(1)?,
                    context: r.get(2)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The agent's latest `limit` tool calls, newest first.
    pub fn agent_tool_calls(&self, a: AgentKey, limit: usize) -> anyhow::Result<Vec<ToolCallRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT ts, name, summary, done_ts, is_error FROM tool_calls WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3 ORDER BY ts DESC LIMIT ?4",
        )?;
        let rows = st.query_map(
            params![a.seat, a.session_id, a.agent_id, limit as i64],
            |r| {
                Ok(ToolCallRow {
                    ts: r.get(0)?,
                    name: r.get(1)?,
                    summary: r.get(2)?,
                    done_ts: r.get(3)?,
                    is_error: r.get::<_, i64>(4)? != 0,
                })
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

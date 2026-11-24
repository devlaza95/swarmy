//! Efficiency figures for a period: where agents spend their time, how long they wait for the
//! user, what the cache saves, which agent types cost what, how big the fleet gets.

use std::collections::{BTreeMap, HashSet};

use rusqlite::params;

use super::archive::{self, Plan};
use super::query::AGENT_TYPE;
use super::{Index, ms};
use crate::core::period::{BUCKET_MS, Span, bucket_start};
use crate::core::tools::ToolClass;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ToolStat {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
    /// Agent-hours spent inside the tool.
    pub hours: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TypeStat {
    pub name: String,
    pub agents: u64,
    pub cost: f64,
    /// Average lifetime, minutes.
    pub avg_minutes: f64,
    /// Subagents that never reported finishing (killed, stalled, still running).
    pub unfinished: u64,
}

/// Dollars by kind of token (`core::pricing::CostParts` summed); the total is the cost.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct TokenSplit {
    pub input: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub output: f64,
    /// Web searches.
    pub web: f64,
}

impl TokenSplit {
    pub fn total(&self) -> f64 {
        self.input + self.cache_write + self.cache_read + self.output + self.web
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Efficiency {
    pub tools: Vec<ToolStat>,
    /// Bash commands grouped by program (`cargo test`, `flutter`, …), by time.
    pub commands: Vec<ToolStat>,
    /// Questions to the user: count and hours the agents waited for the answer.
    pub questions: (u64, f64),
    /// Plans waiting for approval.
    pub plans: (u64, f64),
    pub prompts: u64,
    /// Lifetime of all agents that ran in the period, hours.
    pub agent_hours: f64,
    pub cost: f64,
    pub cache_hit: f64,
    /// What cache reads saved against paying the input rate.
    pub cache_saved: f64,
    pub split: TokenSplit,
    pub types: Vec<TypeStat>,
    /// Agents active in each of the last 24 hours (oldest first).
    pub fleet: Vec<u32>,
    pub rate_limits: u64,
    pub other_errors: u64,
    pub compactions: u64,
    pub files_edited: u64,
    pub edits: u64,
    /// Files edited by more than one agent within the same hour, last 24 hours.
    pub conflicts: Vec<(String, u32)>,
}

/// `'Edit', 'Write', …`: the names of a tool class, for an `IN (...)`.
fn tool_names(class: ToolClass) -> String {
    class
        .names()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `cd x && FOO=1 cargo test -p y | tail` → `cargo test`; `tools/agents/gate.sh` → `gate.sh`.
///
/// The compactor stores this key in `tool_rollup` (`compact.rs`): a later change to these rules
/// regroups only the raw window; rolled-up days keep the keys of the rules they were rolled under.
pub fn command_key(cmd: &str) -> String {
    const SKIP: &[&str] = &[
        "until", "while", "for", "if", "case", "test", "[", "[[", "wait", "read", "exit", "return",
        "cd", "export", "set", "source", ".", "mkdir", "echo", "sleep", "true", "unset", "trap",
        "local", "rm",
    ];
    const TWO: &[&str] = &[
        "cargo", "flutter", "dart", "npm", "npx", "pnpm", "yarn", "make", "git", "gh", "docker",
        "go", "pip", "uv", "node",
    ];
    let mut fallback = None;
    for seg in cmd.split(['\n', ';', '|', '&']) {
        let mut words = seg.split_whitespace().skip_while(|w| {
            w.contains('=') && !w.starts_with('-') && !w.starts_with('"')
                || matches!(
                    *w,
                    "env" | "time" | "do" | "then" | "else" | "{" | "(" | "!" | "exec" | "nice"
                )
        });
        let Some(mut first) = words.next() else {
            continue;
        };
        // Wrappers: `timeout 600 cargo test`, `xargs -0 grep`, `stdbuf -oL make`.
        while matches!(
            first.rsplit('/').next().unwrap_or(first),
            "timeout" | "xargs" | "stdbuf" | "nohup" | "sudo" | "command"
        ) {
            match words.find(|w| {
                !w.starts_with('-') && !w.chars().next().is_some_and(|c| c.is_ascii_digit())
            }) {
                Some(next) => first = next,
                None => break,
            }
        }
        let prog = first
            .rsplit('/')
            .next()
            .unwrap_or(first)
            .trim_matches(['(', ')', '"', '\'']);
        if prog.is_empty() || prog.starts_with('$') || matches!(prog, "done" | "fi" | "esac" | "}")
        {
            continue;
        }
        if SKIP.contains(&prog)
            || prog.starts_with("for")
            || prog.starts_with("while")
            || prog.starts_with("if")
        {
            fallback.get_or_insert_with(|| prog.to_string());
            continue;
        }
        if TWO.contains(&prog)
            && let Some(sub) = words.find(|w| !w.starts_with('-') && !w.starts_with('+'))
        {
            return format!("{prog} {sub}");
        }
        return prog.to_string();
    }
    fallback.unwrap_or_else(|| "?".into())
}

/// Calls, errors and busy milliseconds of a tool or command.
#[derive(Debug, Clone, Copy, Default)]
struct Busy {
    calls: u64,
    errors: u64,
    ms: i64,
}

impl Busy {
    fn add(&mut self, o: Busy) {
        self.calls += o.calls;
        self.errors += o.errors;
        self.ms += o.ms;
    }

    fn stat(self, name: String) -> ToolStat {
        ToolStat {
            name,
            calls: self.calls,
            errors: self.errors,
            hours: self.ms as f64 / 3.6e6,
        }
    }
}

/// (name, calls, errors, busy ms) of a `GROUP BY name` over `tool_calls`.
fn busy_row(r: &rusqlite::Row) -> rusqlite::Result<(String, Busy)> {
    Ok((
        r.get(0)?,
        Busy {
            calls: r.get::<_, i64>(1)? as u64,
            errors: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
            ms: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
        },
    ))
}

/// Distinct files edited and edits in the plan's span. Rolled-up edits keep a hash of each file
/// (`archive::file_hash`), so a span reaching them hashes the raw targets too.
fn files_edited(c: &rusqlite::Connection, p: Plan) -> anyhow::Result<(u64, u64)> {
    let names = tool_names(ToolClass::Edit);
    if !p.archive {
        let (files, edits): (i64, i64) = c.query_row(
            &format!(
                "SELECT COUNT(DISTINCT target), COUNT(*) FROM tool_calls
                 WHERE name IN ({names}) AND ts >= ?1 AND ts < ?2"
            ),
            params![p.start, p.end],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        return Ok((files as u64, edits as u64));
    }
    let (b0, b1) = p.buckets();
    let mut files: HashSet<i64> = HashSet::new();
    let mut st = c.prepare(&format!(
        "SELECT DISTINCT target FROM tool_calls
         WHERE name IN ({names}) AND ts >= ?1 AND ts < ?2 AND target IS NOT NULL"
    ))?;
    for t in st.query_map(params![p.start, p.end], |r| r.get::<_, String>(0))? {
        files.insert(archive::file_hash(&t?));
    }
    let mut st =
        c.prepare("SELECT DISTINCT file_h FROM edit_rollup WHERE bucket >= ?1 AND bucket < ?2")?;
    for h in st.query_map(params![b0, b1], |r| r.get::<_, i64>(0))? {
        files.insert(h?);
    }
    let edits: i64 = c.query_row(
        &format!(
            "SELECT (SELECT COUNT(*) FROM tool_calls WHERE name IN ({names}) AND ts >= ?1 AND ts < ?2)
                  + (SELECT COALESCE(SUM(calls), 0) FROM tool_rollup WHERE name IN ({names}) AND bucket >= ?3 AND bucket < ?4)"
        ),
        params![p.start, p.end, b0, b1],
        |r| r.get(0),
    )?;
    Ok((files.len() as u64, edits as u64))
}

impl Index {
    pub fn efficiency(&self, span: Span) -> anyhow::Result<Efficiency> {
        archive::read(&self.conn, span, false, self.now(), |c, p| {
            self.efficiency_in(c, p)
        })
    }

    /// [`Index::efficiency`] over one plan: raw rows, plus the rollups when the span reaches them.
    fn efficiency_in(&self, c: &rusqlite::Connection, p: Plan) -> anyhow::Result<Efficiency> {
        let (a, b) = (p.start, p.end);
        let (b0, b1) = p.buckets();
        let mut e = Efficiency::default();
        let t0 = std::time::Instant::now();
        let dbg = std::env::var_os("SWARMY_DEBUG").is_some();
        let mark = |n: &str| {
            if dbg {
                eprintln!("    eff {n} {:?}", t0.elapsed());
            }
        };

        // Tools by time; Bash also by program. Busy time is summed in ms and turned into hours
        // once, so raw rows and rollups add up exactly.
        let mut tools: BTreeMap<String, Busy> = BTreeMap::new();
        let mut st = c.prepare_cached(
            "SELECT name, COUNT(*), SUM(is_error), SUM(CASE WHEN done_ts >= ts THEN done_ts - ts ELSE 0 END)
             FROM tool_calls WHERE ts >= ?1 AND ts < ?2 GROUP BY name",
        )?;
        for row in st.query_map(params![a, b], busy_row)? {
            let (name, n) = row?;
            tools.entry(name).or_default().add(n);
        }
        let mut cmds: BTreeMap<String, Busy> = BTreeMap::new();
        if p.archive {
            let mut st = c.prepare_cached(
                "SELECT name, cmd, SUM(calls), SUM(errors), SUM(busy_ms) FROM tool_rollup
                 WHERE bucket >= ?1 AND bucket < ?2 GROUP BY name, cmd",
            )?;
            for row in st.query_map(params![b0, b1], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    Busy {
                        calls: r.get::<_, i64>(2)? as u64,
                        errors: r.get::<_, i64>(3)? as u64,
                        ms: r.get::<_, i64>(4)?,
                    },
                ))
            })? {
                let (name, cmd, n) = row?;
                // A shell command recorded at roll-up time is grouped by its program then.
                if ToolClass::of(&name) == ToolClass::Shell && !cmd.is_empty() {
                    cmds.entry(cmd).or_default().add(n);
                }
                tools.entry(name).or_default().add(n);
            }
        }
        for (name, n) in tools {
            let t = n.stat(name);
            match ToolClass::of(&t.name) {
                ToolClass::AskUser => {
                    e.questions = (e.questions.0 + t.calls, e.questions.1 + t.hours)
                }
                ToolClass::Plan => e.plans = (e.plans.0 + t.calls, e.plans.1 + t.hours),
                ToolClass::Edit
                | ToolClass::Read
                | ToolClass::Shell
                | ToolClass::Agent
                | ToolClass::Other => {}
            }
            e.tools.push(t);
        }
        e.tools
            .sort_by(|x, y| y.hours.total_cmp(&x.hours).then(y.calls.cmp(&x.calls)));

        mark("tools");
        let mut st = c.prepare_cached(&format!(
            "SELECT summary, done_ts - ts, is_error FROM tool_calls
             WHERE name IN ({}) AND ts >= ?1 AND ts < ?2 AND summary IS NOT NULL",
            tool_names(ToolClass::Shell)
        ))?;
        for row in st.query_map(params![a, b], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            let (cmd, dur, err) = row?;
            cmds.entry(command_key(&cmd)).or_default().add(Busy {
                calls: 1,
                errors: err as u64,
                ms: dur.unwrap_or(0).max(0),
            });
        }
        e.commands = cmds.into_iter().map(|(k, n)| n.stat(k)).collect();
        e.commands.sort_by(|x, y| y.hours.total_cmp(&x.hours));
        e.commands.truncate(12);

        mark("commands");
        let count = |r: &rusqlite::Row| r.get::<_, i64>(0);
        e.prompts = if p.archive {
            c.query_row(
                "SELECT (SELECT COUNT(*) FROM prompts WHERE ts >= ?1 AND ts < ?2)
                      + (SELECT COALESCE(SUM(prompts), 0) FROM prompt_rollup WHERE bucket >= ?3 AND bucket < ?4)",
                params![a, b, b0, b1],
                count,
            )?
        } else {
            c.query_row(
                "SELECT COUNT(*) FROM prompts WHERE ts >= ?1 AND ts < ?2",
                params![a, b],
                count,
            )?
        } as u64;

        // Tokens and dollars by kind: the parts stored with each request's cost (the same
        // formula, prices of each request's own day, fast mode and long context included), so
        // the split adds up to the cost.
        let (fresh, read) = c.query_row(
            &format!(
                "SELECT TOTAL(r.input + r.cw5 + r.cw1h), TOTAL(r.cread), TOTAL(r.cost), TOTAL(r.cost_input),
                        TOTAL(r.cost_cache_write), TOTAL(r.cost_cache_read), TOTAL(r.cost_output), TOTAL(r.cost_web),
                        TOTAL(r.cache_saved)
                 FROM {} r WHERE r.ts >= ?1 AND r.ts < ?2",
                p.source()
            ),
            params![a, b],
            |r| {
                e.cost = r.get(2)?;
                e.split = TokenSplit {
                    input: r.get(3)?,
                    cache_write: r.get(4)?,
                    cache_read: r.get(5)?,
                    output: r.get(6)?,
                    web: r.get(7)?,
                };
                e.cache_saved = r.get(8)?;
                Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?))
            },
        )?;
        e.cache_hit = if fresh + read > 0.0 {
            read / (fresh + read)
        } else {
            0.0
        };

        mark("tokens");
        // Agent types (main threads count as "main"), with each agent's spend in the span: its
        // raw requests by the (seat, session, agent, ts) index, plus its rollups grouped once
        // (a `GROUP BY` over the raw rows instead costs several times more).
        let (rolled, plus) = if p.archive {
            (
                format!(
                    "LEFT JOIN (SELECT seat, session_id, agent_id, TOTAL(cost) AS cost FROM usage_rollup
                       WHERE bucket >= {b0} AND bucket < {b1} GROUP BY 1, 2, 3) x
                     ON x.seat = a.seat AND x.session_id = a.session_id AND x.agent_id = a.agent_id"
                ),
                " + COALESCE(x.cost, 0)",
            )
        } else {
            (String::new(), "")
        };
        let mut st = c.prepare(&format!(
            "SELECT {AGENT_TYPE},
                    COUNT(*), SUM(CASE WHEN a.kind != 'main' AND a.finished = 0 THEN 1 ELSE 0 END),
                    AVG(a.last_ts - a.first_ts), SUM(a.last_ts - a.first_ts),
                    SUM((SELECT TOTAL(r.cost) FROM requests r WHERE r.seat = a.seat AND r.session_id = a.session_id
                         AND r.agent_id = a.agent_id AND r.ts >= ?1 AND r.ts < ?2){plus})
             FROM agents a {rolled} WHERE a.last_ts >= ?1 AND a.first_ts < ?2 GROUP BY 1"
        ))?;
        for row in st.query_map(params![a, b], |r| {
            Ok((
                TypeStat {
                    name: r.get(0)?,
                    agents: r.get::<_, i64>(1)? as u64,
                    unfinished: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
                    avg_minutes: r.get::<_, Option<f64>>(3)?.unwrap_or(0.0) / 60_000.0,
                    cost: r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                },
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        })? {
            let (t, life) = row?;
            if t.name != "main thread" {
                e.agent_hours += life.max(0) as f64 / 3.6e6;
            }
            e.types.push(t);
        }
        e.types.sort_by(|x, y| y.cost.total_cmp(&x.cost));

        mark("types");
        // The fleet over the last 24 hours (always raw).
        let now = self.now();
        let h0 = ms(now - chrono::Duration::hours(24)) / 3_600_000;
        e.fleet = vec![0; 25];
        let mut st = c.prepare_cached(
            "SELECT ts / 3600000, COUNT(DISTINCT seat || char(31) || session_id || char(31) || agent_id) FROM requests WHERE ts >= ?1 GROUP BY 1",
        )?;
        for row in st.query_map(params![h0 * 3_600_000], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })? {
            let (h, n) = row?;
            if let Some(slot) = e.fleet.get_mut((h - h0) as usize) {
                *slot = n as u32;
            }
        }

        let errors: Vec<(String, i64)> = {
            let kind_n = |r: &rusqlite::Row| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?));
            if p.archive {
                c.prepare_cached(
                    "SELECT kind, SUM(n) FROM (SELECT kind, 1 AS n FROM api_errors WHERE ts >= ?1 AND ts < ?2
                       UNION ALL SELECT kind, n FROM api_error_rollup WHERE bucket >= ?3 AND bucket < ?4) GROUP BY 1",
                )?
                .query_map(params![a, b, b0, b1], kind_n)?
                .collect::<Result<_, _>>()?
            } else {
                c.prepare_cached(
                    "SELECT kind, COUNT(*) FROM api_errors WHERE ts >= ?1 AND ts < ?2 GROUP BY 1",
                )?
                .query_map(params![a, b], kind_n)?
                .collect::<Result<_, _>>()?
            }
        };
        for (k, n) in errors {
            if k == "rate_limit" {
                e.rate_limits += n as u64;
            } else {
                e.other_errors += n as u64;
            }
        }
        e.compactions = c.query_row(
            "SELECT COALESCE(SUM(compactions), 0) FROM sessions WHERE last_ts >= ?1 AND first_ts < ?2",
            params![a, b],
            |r| r.get::<_, i64>(0),
        )? as u64;

        mark("fleet+errors");
        (e.files_edited, e.edits) = files_edited(c, p)?;
        mark("files");
        e.conflicts = self.edit_conflicts(now - chrono::Duration::hours(24))?;
        mark("conflicts");
        Ok(e)
    }

    /// Files edited by more than one agent within the same hour since `since`, most agents first.
    pub fn edit_conflicts(
        &self,
        since: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Vec<(String, u32)>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT target, MAX(n) FROM (
               SELECT target, ts / 3600000, COUNT(DISTINCT seat || char(31) || session_id || char(31) || agent_id) n FROM tool_calls
               WHERE name IN ({}) AND ts >= ?1 AND target IS NOT NULL GROUP BY 1, 2)
             WHERE n > 1 GROUP BY 1 ORDER BY 2 DESC LIMIT 20",
            tool_names(ToolClass::Edit)
        ))?;
        let rows = st.query_map(params![ms(since)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

impl Index {
    /// Spend per local day and model (for stacked daily bars).
    pub fn days_by_model(&self, span: Span) -> anyhow::Result<Vec<(String, String, f64)>> {
        archive::read(&self.conn, span, false, self.now(), |c, p| {
            let mut st = c.prepare(&format!(
                "SELECT r.ts / {BUCKET_MS}, r.model, TOTAL(r.cost) FROM {} r WHERE r.ts >= ?1 AND r.ts < ?2 GROUP BY 1, 2",
                p.source()
            ))?;
            let mut acc: BTreeMap<(String, String), f64> = Default::default();
            for row in st.query_map(params![p.start, p.end], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                ))
            })? {
                let (b, model, c) = row?;
                let day = bucket_start(b, &chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string();
                *acc.entry((day, model)).or_default() += c;
            }
            Ok(acc.into_iter().map(|((d, m), c)| (d, m, c)).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::command_key;

    #[test]
    fn commands_are_grouped_by_program() {
        assert_eq!(
            command_key("cd ~/x && cargo test -p foo --no-fail-fast 2>&1 | tail"),
            "cargo test"
        );
        assert_eq!(
            command_key("export TMPDIR=$HOME/tmp; RUST_LOG=1 cargo +nightly build"),
            "cargo build"
        );
        assert_eq!(
            command_key("tools/agents/cargo-heavy.sh tools/agents/gate.sh"),
            "cargo-heavy.sh"
        );
        assert_eq!(command_key("git -C repo status --short"), "git repo");
        assert_eq!(command_key("python3 - <<'EOF'"), "python3");
        assert_eq!(command_key("sleep 30"), "sleep");
        assert_eq!(command_key("timeout 600 cargo test -p x"), "cargo test");
        assert_eq!(
            command_key("until [ -s f ]; do sleep 5; done; cat f"),
            "cat"
        );
        assert_eq!(command_key("for i in 1 2; do grep -n x f; done"), "grep");
    }
}

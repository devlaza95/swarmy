//! The archive's read path, proved on a populated synthetic world: rolling raw rows up into the
//! archive tables (by SQL, as the compactor will) and deleting them changes no number on any
//! report, analytics panel, limit or live total. The world and the snapshot are shared with the
//! compactor's tests (`compact_scenarios`).

#[cfg(test)]
pub(crate) use tests::{NOW, diff, populate, readings, refresh, snapshot};

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{DateTime, Duration, NaiveDate, Utc};
    use rusqlite::params;
    use serde_json::{Value, json};

    use crate::app::context::Ctx;
    use crate::config::{Config, Dirs};
    use crate::core::period::{BUCKET_MS, Period, Span};
    use crate::core::pricing::PriceBook;
    use crate::core::tools::ToolClass;
    use crate::index::archive::{self, file_hash};
    use crate::index::insights::command_key;
    use crate::index::live::AgentKey;
    use crate::index::query::{Dim, Filter};
    use crate::index::scenarios::{World, account, at, owner};
    use crate::index::{Index, SeatInfo};
    use crate::procs::ProcTable;
    use crate::ui::style::{ColorChoice, Painter};

    /// A small deterministic generator (xorshift), so the world is the same on every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[self.below(xs.len() as u64) as usize]
        }
    }

    fn ts(t: DateTime<Utc>) -> String {
        t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    const MODELS: &[&str] = &[
        "claude-sonnet-5-5",
        "claude-sonnet-5-5",
        "claude-opus-5-5",
        "claude-haiku-5-5",
        "claude-mystery-1", // no price known: counted as unpriced
    ];
    const CWDS: &[&str] = &["/w/shop", "/w/shop", "/w/docs", "/w/api"];
    const COMMANDS: &[&str] = &[
        "cargo test -p x --no-fail-fast",
        "cd /w/shop && make build",
        "timeout 60 cargo build",
        "git status",
        "ls -la",
        "FOO=1 npm run lint",
    ];
    const FILES: &[&str] = &[
        "/w/shop/a.rs",
        "/w/shop/b.rs",
        "/w/docs/x.md",
        "/w/docs/n.ipynb",
    ];

    /// Lines of one transcript, and the clock of its agent.
    struct Writer<'a> {
        rng: &'a mut Rng,
        lines: Vec<String>,
        t: DateTime<Utc>,
        id: String,
        n: u32,
    }

    impl Writer<'_> {
        fn tick(&mut self) -> DateTime<Utc> {
            self.t += Duration::milliseconds(37_000 + self.rng.below(400_000) as i64);
            self.t
        }

        /// One response: model, effort, fast mode, requested model, tokens and tool calls (most
        /// of them finished later, some with an error, some still open).
        fn request(&mut self, cwd: &str) {
            let t = self.tick();
            self.n += 1;
            let msg = format!("{}-m{}", self.id, self.n);
            let model = self.rng.pick(MODELS);
            let effort = self.rng.pick(&["", "low", "high", "max", "-"]);
            let fast = model == "claude-opus-5-5" && self.rng.below(3) == 0;
            let requested = match self.rng.below(4) {
                0 => Some("claude-opus-5-5"),
                1 => Some(model),
                _ => None,
            };
            let big = self.rng.below(3) == 0;
            let cread = if big {
                150_000 + self.rng.below(120_000)
            } else {
                self.rng.below(40_000)
            };
            let mut content = Vec::new();
            let mut results = Vec::new();
            for k in 0..self.rng.below(3) {
                let id = format!("{msg}-t{k}");
                let (name, input) = match self.rng.below(6) {
                    0 | 1 => ("Bash", json!({"command": self.rng.pick(COMMANDS)})),
                    2 => (
                        self.rng.pick(&["Edit", "Write", "NotebookEdit"]),
                        json!({"file_path": self.rng.pick(FILES)}),
                    ),
                    3 => ("Read", json!({"file_path": self.rng.pick(FILES)})),
                    4 => (
                        self.rng.pick(&["AskUserQuestion", "ExitPlanMode"]),
                        json!({"question": "go on?"}),
                    ),
                    _ => ("Grep", json!({"pattern": "todo"})),
                };
                content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
                if self.rng.below(8) != 0 {
                    let done = t + Duration::milliseconds(500 + self.rng.below(900_000) as i64);
                    results.push(json!({"type": "user", "timestamp": ts(done), "sessionId": "s",
                        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id,
                            "content": "ok", "is_error": self.rng.below(6) == 0}]}}));
                }
            }
            let mut rec = json!({
                "type": "assistant", "timestamp": ts(t), "cwd": cwd, "sessionId": "s",
                "message": {"id": msg, "model": model, "content": [],
                    "usage": {"input_tokens": self.rng.below(6000), "output_tokens": self.rng.below(3000),
                        "cache_creation_input_tokens": 0, "cache_read_input_tokens": cread,
                        "cache_creation": {"ephemeral_5m_input_tokens": self.rng.below(2000),
                                           "ephemeral_1h_input_tokens": self.rng.below(2000)},
                        "output_tokens_details": {"thinking_tokens": self.rng.below(500)},
                        "server_tool_use": {"web_search_requests": self.rng.below(4) / 3},
                        "speed": if fast { "fast" } else { "standard" }}}
            });
            if !effort.is_empty() {
                rec["effort"] = json!(effort);
            }
            if let Some(r) = requested {
                rec["requestedModel"] = json!(r);
            }
            // As Claude Code writes a response: one record per content block, the same message
            // id and usage, each block at its own time (a text block, then each tool call two
            // seconds apart).
            let blocks = std::iter::once(json!({"type": "text", "text": "on it"})).chain(content);
            for (k, block) in blocks.enumerate() {
                rec["timestamp"] = json!(ts(t + Duration::seconds(2 * k as i64)));
                rec["message"]["content"] = json!([block]);
                self.lines.push(rec.to_string());
            }
            self.lines.extend(results.iter().map(Value::to_string));
        }

        fn prompt(&mut self) {
            let t = self.tick();
            let text = "x".repeat(5 + self.rng.below(200) as usize);
            self.lines.push(
                json!({"type": "user", "uuid": format!("{}-p{}", self.id, self.lines.len()), "timestamp": ts(t),
                       "message": {"role": "user", "content": text}})
                    .to_string(),
            );
        }

        fn api_error(&mut self) {
            let t = self.tick();
            let kind = self.rng.pick(&["rate_limit", "overloaded"]);
            let uuid = format!("{}-e{}", self.id, self.lines.len());
            self.lines.push(json!({"type": "assistant", "uuid": uuid, "isApiErrorMessage": true, "error": kind, "timestamp": ts(t),
                "message": {"id": "x", "model": "<synthetic>", "content": [{"type": "text", "text": "API Error"}]}})
            .to_string());
        }

        fn compact(&mut self) {
            let t = self.tick();
            self.lines.push(
                json!({"type": "system", "subtype": "compact_boundary", "timestamp": ts(t)})
                    .to_string(),
            );
        }
    }

    fn write(path: &PathBuf, lines: &[String], append: bool) {
        use std::io::Write as _;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(append)
            .write(true)
            .truncate(!append)
            .open(path)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    const DAY0: &str = "2026-09-21T00:00:00Z";
    const DAYS: i64 = 40;
    /// The end of the world: every span of the snapshot is resolved at this time.
    pub(crate) const NOW: &str = "2026-10-30T23:07:13Z";
    /// What is rolled up: everything before this (a UTC midnight, so a quarter hour).
    const CUT: &str = "2026-10-21T00:00:00Z";

    /// 40 days on three seats and two accounts (one seat swaps its login on day 19): a session a
    /// day with prompts, errors, compactions, a subagent, every fourth day a workflow agent, and
    /// one session resumed every week from the first day to the last.
    pub(crate) fn populate(w: &World) {
        let mut rng = Rng(0x5eed_1234_abcd);
        let day0 = at(DAY0);
        let seats = ["work", "home", "lab"];
        let owners = ["acc-a", "acc-b", "acc-a"];
        let dir = |seat: &str| {
            w.seats
                .iter()
                .find(|s| s.label == seat)
                .unwrap()
                .projects_dir()
                .join("-w-proj")
        };
        let mut long = Vec::new();
        for d in 0..DAYS {
            let i = (d % 3) as usize;
            let (seat, session) = (seats[i], format!("s{d:02}"));
            let start =
                day0 + Duration::days(d) + Duration::minutes(8 * 60 + rng.below(600) as i64);
            let cwd = rng.pick(CWDS);
            let mut m = Writer {
                rng: &mut rng,
                lines: vec![
                    owner(owners[i]),
                    json!({"type": "custom-title", "customTitle": format!("day {d}"), "sessionId": "s"})
                        .to_string(),
                ],
                t: start,
                id: session.clone(),
                n: 0,
            };
            for k in 0..6 {
                if k % 2 == 0 {
                    m.prompt();
                }
                m.request(cwd);
                if k == 3 && d % 2 == 0 {
                    m.api_error();
                }
                if k == 4 && d % 5 == 0 {
                    m.compact();
                }
            }
            m.lines.push(
                json!({"type": "cost-state", "totalCostUSD": 1.0 + d as f64, "modelUsage": {}})
                    .to_string(),
            );
            let main_lines = std::mem::take(&mut m.lines);
            write(
                &dir(seat).join(format!("{session}.jsonl")),
                &main_lines,
                false,
            );
            // A subagent, and every fourth day a workflow agent too.
            let mut agents = vec![(
                dir(seat)
                    .join(&session)
                    .join("subagents")
                    .join("agent-a1.jsonl"),
                rng.pick(&["reviewer", "general-purpose", "Explore"]),
            )];
            if d % 4 == 0 {
                agents.push((
                    dir(seat)
                        .join(&session)
                        .join("subagents/workflows/wf_1/agent-w1.jsonl"),
                    "builder",
                ));
            }
            for (k, (path, kind)) in agents.into_iter().enumerate() {
                let mut a = Writer {
                    rng: &mut rng,
                    lines: Vec::new(),
                    t: start + Duration::seconds(17 + k as i64),
                    id: format!("{session}-a{k}"),
                    n: 0,
                };
                for _ in 0..3 {
                    a.request(cwd);
                }
                if k == 0 && d % 3 == 0 {
                    a.api_error();
                }
                write(&path, &a.lines, false);
                std::fs::write(
                    path.with_extension("meta.json"),
                    json!({"agentType": kind, "description": format!("task {d}.{k}"),
                           "finished": d % 7 != 6, "spawnDepth": 1})
                    .to_string(),
                )
                .unwrap();
            }
            // The long session: resumed every week, from day 0 to day 38.
            if d % 6 == 0 || d == DAYS - 2 {
                let mut l = Writer {
                    rng: &mut rng,
                    lines: if d == 0 {
                        vec![owner("acc-a")]
                    } else {
                        Vec::new()
                    },
                    t: start + Duration::hours(3),
                    id: format!("long-{d}"),
                    n: 0,
                };
                l.prompt();
                l.request("/w/shop");
                l.request("/w/docs");
                long.extend(l.lines);
            }
        }
        write(&dir("work").join("long.jsonl"), &long, false);
    }

    /// The login history: `lab` and `work` had acc-a, `home` acc-b; `work` shows acc-b later
    /// (seen when refreshed, after every request here: the sessions' owners decide).
    pub(crate) fn refresh(w: &World, idx: &mut Index) {
        let book = PriceBook::builtin();
        let info = |work: &str, since: &str| {
            vec![
                SeatInfo {
                    seat: Default::default(),
                    label: "work".into(),
                    account: Some(account(work, since)),
                    switched_at: None,
                    utilization: None,
                },
                SeatInfo {
                    seat: Default::default(),
                    label: "home".into(),
                    account: Some(account("acc-b", "2026-09-01T00:00:00Z")),
                    switched_at: None,
                    utilization: None,
                },
                SeatInfo {
                    seat: Default::default(),
                    label: "lab".into(),
                    account: Some(account("acc-a", "2026-09-01T00:00:00Z")),
                    switched_at: None,
                    utilization: None,
                },
            ]
        };
        w.refresh(idx, &info("acc-a", "2026-09-01T00:00:00Z"), &book);
        w.refresh(idx, &info("acc-b", "2026-10-10T12:00:00Z"), &book);
    }

    /// Weekly readings of both accounts: windows reset on quarter hours; readings older than
    /// the shortest raw window are taken on quarter hours too (a span ending inside the archive
    /// counts its whole quarter hour), newer ones at any second.
    pub(crate) fn readings(idx: &Index) {
        let mut rows = Vec::new();
        for (acct, reset0, scale) in [
            ("acc-a", "2026-09-29T09:00:00Z", 1.0),
            ("acc-b", "2026-09-29T09:30:00Z", 0.7),
        ] {
            for k in 0..6 {
                let reset = at(reset0) + Duration::days(7 * k);
                let start = reset - Duration::days(7);
                for (j, off) in [
                    (1, Duration::hours(49)),
                    (2, Duration::hours(121) + Duration::minutes(45)),
                ] {
                    let mut fetched = start + off;
                    if fetched >= at(NOW) - Duration::days(archive::MIN_RAW_DAYS) {
                        fetched += Duration::milliseconds(1_234_567);
                    }
                    if fetched > at(NOW) {
                        continue;
                    }
                    rows.push((acct, fetched, scale * (10.0 * j as f64 + k as f64), reset));
                }
            }
        }
        for (acct, fetched, used, reset) in rows {
            idx.conn
                .execute(
                    "INSERT INTO readings VALUES (?1, 'seven_day', ?2, ?3, ?4)",
                    params![
                        acct,
                        fetched.timestamp_millis(),
                        used,
                        reset.timestamp_millis()
                    ],
                )
                .unwrap();
        }
    }

    fn ctx(w: &World) -> Ctx {
        Ctx {
            dirs: Dirs {
                data: w.dir.path().join("data"),
                config: w.dir.path().join("config"),
                home: w.dir.path().to_path_buf(),
            },
            config: Config::default(),
            seats: w.seats.clone(),
            seat_info: Vec::new(),
            prices: PriceBook::builtin(),
            procs: ProcTable::empty(),
            painter: Painter::new(ColorChoice::Never),
            mask: false,
            now: at(NOW),
            login_labels: Default::default(),
            login_tiers: Default::default(),
            problems: Vec::new(),
        }
    }

    fn v<T: serde::Serialize>(x: T) -> Value {
        serde_json::to_value(x).unwrap()
    }

    /// Every number a surface shows, as one comparable tree.
    pub(crate) fn snapshot(w: &World, idx: &Index) -> Value {
        let now = at(NOW);
        let d = |s: &str| s.parse::<NaiveDate>().unwrap();
        let periods = [
            Period::Today,
            Period::Yesterday,
            Period::Hours(24),
            Period::Days(7),
            Period::Days(14),
            Period::Days(30),
            Period::Days(90),
            Period::Month,
            Period::All,
            Period::Range(d("2026-09-25"), d("2026-10-05")),
            Period::Range(d("2026-10-18"), d("2026-10-25")),
            Period::Range(d("2026-10-20"), d("2026-10-20")),
        ];
        let dims = [
            Dim::Project,
            Dim::Worktree,
            Dim::Branch,
            Dim::Account,
            Dim::Seat,
            Dim::Harness,
            Dim::Model,
            Dim::Effort,
            Dim::AgentType,
            Dim::Session,
            Dim::Agent,
            Dim::Day,
            Dim::Hour,
            Dim::Weekday,
        ];
        let filters = [
            Filter::default(),
            Filter {
                account: Some("acc-a".into()),
                ..Default::default()
            },
            Filter {
                seat: Some(w.id("home")),
                ..Default::default()
            },
            Filter {
                project: Some("shop".into()),
                ..Default::default()
            },
            Filter {
                model: Some("opus".into()),
                ..Default::default()
            },
            Filter {
                harness: Some(crate::core::harness::Harness::Claude),
                ..Default::default()
            },
        ];
        let mut out = serde_json::Map::new();
        for p in &periods {
            let span = p.span(now, None);
            let mut by = serde_json::Map::new();
            for (i, f) in filters.iter().enumerate() {
                by.insert(format!("total/{i}"), v(idx.total(span, f).unwrap()));
                for dim in dims {
                    by.insert(
                        format!("{dim:?}/{i}"),
                        v(idx.spend_by(dim, span, f).unwrap()),
                    );
                }
                by.insert(
                    format!("subs/{i}"),
                    v(idx.model_substitutions(span, f).unwrap()),
                );
            }
            for (a, b) in [
                (Dim::Project, Dim::Model),
                (Dim::Weekday, Dim::Hour),
                (Dim::Model, Dim::Effort),
                (Dim::Account, Dim::Day),
                (Dim::AgentType, Dim::Seat),
            ] {
                by.insert(
                    format!("{a:?}x{b:?}"),
                    v(idx.matrix(a, b, span, &Filter::default()).unwrap()),
                );
            }
            by.insert("days_by_model".into(), v(idx.days_by_model(span).unwrap()));
            let mut e = v(idx.efficiency(span).unwrap());
            let e_obj = e.as_object_mut().unwrap();
            // Both read the last 24 hours of the real clock (raw only); compared elsewhere.
            e_obj.remove("fleet");
            e_obj.remove("conflicts");
            by.insert("efficiency".into(), e);
            by.insert("api_errors".into(), v(idx.api_errors(span).unwrap()));
            by.insert("crosscheck".into(), v(idx.cost_crosscheck(span).unwrap()));
            // Sparklines and the limit curve (hourly over a bounded span).
            if *p != Period::All {
                by.insert(
                    "series".into(),
                    v(idx.spend_series(span, 60, &Filter::default()).unwrap()),
                );
            }
            out.insert(p.label(), Value::Object(by));
        }
        // HISTORY and the dashboard's trends.
        let span = Period::Days(7).span(now, None);
        out.insert(
            "previous".into(),
            v(idx
                .spend_by(Dim::Project, span.previous(), &Filter::default())
                .unwrap()),
        );
        // Limits: the windows in force and the calibration over older windows.
        let reports = {
            let c = ctx(w);
            crate::limits::all_reports(&c.limits_inputs(idx).unwrap(), idx).unwrap()
        };
        let curves: Vec<Vec<crate::core::units::Pct>> = reports
            .iter()
            .flat_map(|r| r.windows.iter().map(|x| x.curve.clone()))
            .collect();
        out.insert("limits".into(), v(&reports));
        out.insert("limit_curves".into(), v(curves));
        // Live totals over every agent's and session's lifetime.
        let agents: Vec<(
            crate::core::ids::SeatId,
            crate::core::ids::SessionId,
            String,
        )> = {
            let mut st = idx
                .conn
                .prepare("SELECT seat, session_id, agent_id FROM agents ORDER BY 1, 2, 3")
                .unwrap();
            st.query_map([], |r| {
                Ok((
                    r.get::<_, crate::core::ids::SeatId>(0)?,
                    r.get::<_, crate::core::ids::SessionId>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
        };
        let mut live = serde_json::Map::new();
        for (seat, session, agent) in &agents {
            let key = AgentKey {
                seat,
                session_id: session,
                agent_id: agent,
            };
            live.insert(
                format!("{seat}/{session}/{agent}"),
                json!({
                    "totals": v(idx.agent_totals(key).unwrap()),
                    "session": v(idx.session_spend(seat, session, now - Duration::hours(1)).unwrap()),
                }),
            );
        }
        let epoch = DateTime::<Utc>::UNIX_EPOCH;
        let done: Vec<Value> = idx
            .recently_done(epoch)
            .unwrap()
            .into_iter()
            .map(|d| json!([d.name, d.cost, d.model, d.first_ts, d.last_ts]))
            .collect();
        let mut events: Vec<Value> = idx
            .agent_events(epoch)
            .unwrap()
            .into_iter()
            .map(|e| json!([e.name, e.cost, e.model, e.first_ts, e.last_ts, e.finished]))
            .collect();
        events.sort_by_key(|e| e.to_string());
        live.insert("recently_done".into(), Value::Array(done));
        live.insert("events".into(), Value::Array(events));
        out.insert("live".into(), Value::Object(live));
        Value::Object(out)
    }

    /// Differences between two snapshots: integers and text must be equal, floats agree within
    /// 1e-9 relative (sums over other groupings add in another order).
    pub(crate) fn diff(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
                let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
                if (x - y).abs() > 1e-9 * x.abs().max(y.abs()).max(1e-3) {
                    out.push(format!("{path}: {x} vs {y}"));
                }
            }
            (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
                for (i, (p, q)) in x.iter().zip(y).enumerate() {
                    diff(p, q, &format!("{path}[{i}]"), out);
                }
            }
            (Value::Object(x), Value::Object(y))
                if x.keys().collect::<Vec<_>>() == y.keys().collect::<Vec<_>>() =>
            {
                for (k, p) in x {
                    diff(p, &y[k], &format!("{path}.{k}"), out);
                }
            }
            _ if a == b => {}
            _ => out.push(format!(
                "{path}: {} vs {}",
                truncate(a.to_string()),
                truncate(b.to_string())
            )),
        }
    }

    fn truncate(s: String) -> String {
        s.chars().take(300).collect()
    }

    /// Rolls up every raw row before `cut` into the archive tables and deletes it, in one
    /// transaction, the way the compactor will: requests by the generated `INSERT … SELECT`
    /// (per model, with its long-context threshold), tool calls by command key, edited files by
    /// hash, prompts and errors as counts.
    fn roll_up(idx: &mut Index, cut: i64) {
        let book = PriceBook::builtin();
        let day = at(CUT).date_naive();
        let tx = idx.conn.transaction().unwrap();
        let models: Vec<String> = tx
            .prepare("SELECT DISTINCT model FROM requests WHERE ts < ?1")
            .unwrap()
            .query_map([cut], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let insert = archive::rollup_insert_sql("ts < :cut AND model = :model");
        for m in &models {
            let lc = book
                .lookup(m, day)
                .and_then(|e| e.rates.long_context.as_ref())
                .map_or(0, |l| l.threshold as i64);
            tx.execute(
                &insert,
                rusqlite::named_params! {":cut": cut, ":model": m, ":lc": lc},
            )
            .unwrap();
        }
        {
            let mut sel = tx
                .prepare("SELECT ts, name, summary, target, done_ts, is_error FROM tool_calls WHERE ts < ?1")
                .unwrap();
            let rows: Vec<ToolRow> = sel
                .query_map([cut], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for (t, name, summary, target, done, err) in rows {
                let class = ToolClass::of(&name);
                let cmd = match (&summary, class) {
                    (Some(s), ToolClass::Shell) => command_key(s),
                    _ => String::new(),
                };
                let busy = done.map_or(0, |d| (d - t).max(0));
                tx.execute(
                    "INSERT INTO tool_rollup VALUES (?1, ?2, ?3, 1, ?4, ?5)
                     ON CONFLICT(bucket, name, cmd) DO UPDATE SET calls = calls + 1,
                       errors = errors + excluded.errors, busy_ms = busy_ms + excluded.busy_ms",
                    params![t / BUCKET_MS, name, cmd, err, busy],
                )
                .unwrap();
                if let (ToolClass::Edit, Some(target)) = (class, &target) {
                    tx.execute(
                        "INSERT OR IGNORE INTO edit_rollup VALUES (?1, ?2)",
                        params![t / BUCKET_MS, file_hash(target)],
                    )
                    .unwrap();
                }
            }
        }
        tx.execute_batch(&format!(
            "INSERT INTO prompt_rollup SELECT ts / {BUCKET_MS}, COUNT(*), SUM(chars) FROM prompts WHERE ts < {cut} GROUP BY 1;
             INSERT INTO api_error_rollup SELECT ts / {BUCKET_MS}, kind, COUNT(*) FROM api_errors WHERE ts < {cut} GROUP BY 1, 2;
             {}
             DELETE FROM requests WHERE ts < {cut};
             DELETE FROM tool_calls WHERE ts < {cut};
             DELETE FROM prompts WHERE ts < {cut};
             DELETE FROM api_errors WHERE ts < {cut};
             UPDATE meta SET value = '{cut}' WHERE key = 'rolled_until';",
            archive::agent_rollup_sql("1")
        ))
        .unwrap();
        tx.commit().unwrap();
    }

    /// A raw tool call: (ts, name, summary, target, done_ts, is_error).
    type ToolRow = (
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        i64,
    );

    fn count(idx: &Index, sql: &str) -> i64 {
        idx.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn rolled_up_spend_reads_the_same_on_every_surface() {
        let w = World::new(&["work", "home", "lab"]);
        populate(&w);
        let path = w.dir.path().join("i.sqlite");
        let mut idx = Index::open(&path).unwrap();
        refresh(&w, &mut idx);
        readings(&idx);
        drop(idx);
        let idx = Index::open(&path).unwrap();
        let before = snapshot(&w, &idx);
        drop(idx);

        // The fixture covers what it should, before and after the cut.
        let mut idx = Index::open(&path).unwrap();
        let cut = at(CUT).timestamp_millis();
        let raw_old = count(
            &idx,
            &format!("SELECT COUNT(*) FROM requests WHERE ts < {cut}"),
        );
        assert!(raw_old > 250, "{raw_old} requests to roll up");
        assert!(
            count(
                &idx,
                &format!("SELECT COUNT(*) FROM requests WHERE ts >= {cut}")
            ) > 100
        );
        for (what, sql) in [
            ("unpriced", "SELECT COUNT(*) FROM requests WHERE priced = 0"),
            ("fast", "SELECT COUNT(*) FROM requests WHERE fast = 1"),
            (
                "substituted",
                "SELECT COUNT(*) FROM requests WHERE requested_model != model",
            ),
            (
                "long context",
                "SELECT COUNT(*) FROM requests WHERE model = 'claude-haiku-5-5' AND input + cw5 + cw1h + cread > 100000",
            ),
            (
                "two accounts",
                "SELECT COUNT(DISTINCT account_uuid) - 1 FROM requests",
            ),
            (
                "open tools",
                "SELECT COUNT(*) FROM tool_calls WHERE done_ts IS NULL",
            ),
            (
                "failed tools",
                "SELECT COUNT(*) FROM tool_calls WHERE is_error = 1",
            ),
            (
                "shell commands",
                "SELECT COUNT(*) FROM tool_calls WHERE name = 'Bash'",
            ),
            (
                "edits",
                "SELECT COUNT(*) FROM tool_calls WHERE name IN ('Edit', 'Write', 'NotebookEdit')",
            ),
            ("prompts", "SELECT COUNT(*) FROM prompts"),
            ("errors", "SELECT COUNT(DISTINCT kind) - 1 FROM api_errors"),
            (
                "workflow agents",
                "SELECT COUNT(*) FROM agents WHERE kind = 'workflow'",
            ),
        ] {
            assert!(count(&idx, sql) > 0, "the world has {what}");
        }
        roll_up(&mut idx, cut);
        assert_eq!(
            count(
                &idx,
                &format!("SELECT COUNT(*) FROM requests WHERE ts < {cut}")
            ),
            0
        );
        let rolled = count(&idx, "SELECT SUM(requests) FROM usage_rollup");
        assert_eq!(rolled, raw_old, "every old request is in the rollup once");
        assert!(count(&idx, "SELECT COUNT(*) FROM usage_rollup WHERE long_ctx = 1") > 0);
        assert!(count(&idx, "SELECT COUNT(*) FROM agent_rollup") > 0);
        drop(idx);

        // A fresh process (no memoized totals) reads the archive.
        let idx = Index::open(&path).unwrap();
        let all = Period::All.span(at(NOW), None);
        assert!(
            archive::plan(&idx.conn, all, false, idx.now())
                .unwrap()
                .archive
        );
        let after = snapshot(&w, &idx);
        let mut d = Vec::new();
        diff(&before, &after, "", &mut d);
        assert!(
            d.is_empty(),
            "{} differences, the first: {:#?}",
            d.len(),
            &d[..d.len().min(20)]
        );
        // Spans inside the raw window keep the plain raw read.
        let week = Period::Days(7).span(at(NOW), None);
        assert!(
            !archive::plan(&idx.conn, week, false, idx.now())
                .unwrap()
                .archive
        );
    }

    #[test]
    fn live_agent_rows_are_bounded_in_time_but_keep_idle_live_sessions() {
        // `agents` keeps every agent ever seen, and the live view reads it every second: only
        // recent agents, unfinished ones of the last day, and the main threads of running
        // sessions however long they have been idle.
        let w = World::new(&["work"]);
        let m = "claude-sonnet-5-5";
        let rec = |id: &str, t: &str| {
            crate::index::scenarios::assistant(id, t, m, "/w/shop", (10, 1), &[])
        };
        w.append("work", "busy", &[rec("b1", "2026-11-10T11:50:00Z")]);
        w.append("work", "idle", &[rec("i1", "2026-11-07T09:00:00Z")]);
        w.append("work", "gone", &[rec("g1", "2026-11-01T09:00:00Z")]);
        for (agent, t) in [
            ("fresh", "2026-11-10T02:00:00Z"),
            ("stale", "2026-11-08T02:00:00Z"),
        ] {
            w.write_subagent("work", "busy", agent, &[rec(&format!("{agent}-1"), t)]);
            let dir = w.seats[0].projects_dir().join("-w-proj/busy/subagents");
            std::fs::write(
                dir.join(format!("agent-{agent}.meta.json")),
                r#"{"agentType":"x","finished":false}"#,
            )
            .unwrap();
        }
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        let keys = |live: &[(&str, &str)]| -> Vec<String> {
            let live = live.iter().map(|(a, b)| (w.id(a), (*b).into())).collect();
            idx.live_agent_rows(
                at("2026-11-10T11:30:00Z"),
                at("2026-11-09T12:00:00Z"),
                &live,
            )
            .unwrap()
            .into_iter()
            .map(|r| format!("{}/{}", r.session_id, r.agent_id))
            .collect()
        };
        assert_eq!(keys(&[]), vec!["busy/", "busy/fresh"]);
        assert_eq!(
            keys(&[("work", "idle"), ("work", "busy"), ("work", "nowhere")]),
            vec!["busy/", "busy/fresh", "idle/"],
            "a running session idle for days still shows, once"
        );
    }

    #[test]
    #[should_panic(expected = "must start on a quarter hour")]
    fn a_span_into_the_archive_off_the_quarter_hour_is_caught() {
        let w = World::new(&["work"]);
        let mut idx = w.index("i.sqlite");
        idx.set_meta("rolled_until", &at(CUT).timestamp_millis().to_string())
            .unwrap();
        let span = Span {
            start: at("2026-10-01T10:07:00Z"),
            end: at(NOW),
        };
        // Limit calibration floors such a start instead.
        let p = archive::plan(&idx.conn, span, true, idx.now()).unwrap();
        assert!(p.archive && p.start == at("2026-10-01T10:00:00Z").timestamp_millis());
        let _ = idx.total(span, &Filter::default());
    }
}

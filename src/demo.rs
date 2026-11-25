//! `swarmy demo`: a made-up world of Claude Code config dirs, so the dashboard can be tried
//! (and recorded for the README) without anyone's real data.
//!
//! Two seats on two accounts, four projects, two weeks of history, and a background thread that
//! keeps a swarm of agents busy: tools start and finish, agents spawn and complete, one session
//! asks for permission now and then.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Duration, Utc};
use serde_json::json;

use crate::claude::seats::Seat;
use crate::procs::proc_start_ticks;

mod agy;
mod codex;

use agy::{AGY_LIVE, agy_planner, agy_seat, agy_transcript};
use codex::codex_seat;

fn rnd(a: u64, b: u64) -> u64 {
    crate::ui::hash(a, b, 0x5eed)
}

fn frac(a: u64, b: u64) -> f64 {
    (rnd(a, b) % 10_000) as f64 / 10_000.0
}

const PROJECTS: &[&str] = &["storefront", "payments-api", "ml-pipeline", "docs-site"];
const MODELS: &[(&str, &str)] = &[
    ("claude-opus-5-5", "high"),
    ("claude-fable-5-2", "xhigh"),
    ("claude-sonnet-5-5", "medium"),
    ("claude-haiku-5-5", "low"),
];

const TOOLS: &[(&str, &str, &str)] = &[
    ("Bash", "command", "cargo test --workspace --no-fail-fast"),
    ("Bash", "command", "npm run build && npm run lint"),
    ("Bash", "command", "pytest -q tests/test_checkout.py"),
    ("Bash", "command", "git diff --stat origin/main"),
    (
        "Edit",
        "file_path",
        "/home/dev/work/storefront/src/cart/pricing.rs",
    ),
    (
        "Edit",
        "file_path",
        "/home/dev/work/payments-api/handlers/refund.go",
    ),
    (
        "Read",
        "file_path",
        "/home/dev/work/ml-pipeline/train/config.yaml",
    ),
    ("Grep", "pattern", "fn apply_discount"),
    ("Glob", "pattern", "**/*.test.ts"),
    ("WebSearch", "query", "stripe idempotency keys retries"),
    (
        "Write",
        "file_path",
        "/home/dev/work/docs-site/content/api/webhooks.md",
    ),
];

const AGENT_NAMES: &[&str] = &[
    "Fix flaky checkout tests",
    "Refund flow: idempotency",
    "Profile feature pipeline",
    "Audit pricing rounding",
    "Write webhook docs",
    "Migrate cart to new API",
    "Review payment retries",
    "Speed up CI cache",
    "Port tokenizer to Rust",
    "Triage lint warnings",
];

/// How much the made-up world does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// The README's world: two seats, two weeks of light history, 5–7 agents at work.
    Demo,
    /// A busy 4K wall, the load of the battery budget (`tools/perf.sh`): three seats, ten days of
    /// about 12,600 requests a day in long sessions full of subagents, and three live sessions,
    /// each with a long history of its own, keeping about 18 agents at work.
    Wall,
    /// A Codex-only machine (issue #8): the Codex home at work, and a `~/.claude` that Claude
    /// Code left a `settings.json` in but was never signed into or used — an empty seat.
    CodexOnly,
}

pub struct Demo {
    pub root: PathBuf,
    pub seats: Vec<Seat>,
    stop: Arc<AtomicBool>,
}

impl Drop for Demo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct SeatDef {
    label: &'static str,
    account: &'static str,
    email: &'static str,
    utilization: f64,
    resets_in_h: i64,
}

/// The first two seats make the README's world; the wall adds the third.
const SEATS: &[SeatDef] = &[
    SeatDef {
        label: "personal",
        account: "acc-demo-personal",
        email: "alex@example.com",
        utilization: 71.0,
        resets_in_h: 77,
    },
    SeatDef {
        label: "work",
        account: "acc-demo-work",
        email: "alex@acme.example",
        utilization: 38.0,
        resets_in_h: 130,
    },
    SeatDef {
        label: "lab",
        account: "acc-demo-lab",
        email: "alex@lab.example",
        utilization: 54.0,
        resets_in_h: 40,
    },
];

fn usage(seed: u64, model: &str) -> serde_json::Value {
    let big = model.contains("opus") || model.contains("fable");
    let cr = 40_000 + rnd(seed, 1) % if big { 260_000 } else { 90_000 };
    let cw = 300 + rnd(seed, 2) % 6_000;
    let out = 80 + rnd(seed, 3) % if big { 2_600 } else { 900 };
    json!({"input_tokens": 2 + rnd(seed, 4) % 8, "cache_creation_input_tokens": cw, "cache_read_input_tokens": cr,
           "output_tokens": out, "output_tokens_details": {"thinking_tokens": out * (20 + rnd(seed, 5) % 40) / 100}, "cache_creation": {"ephemeral_1h_input_tokens": cw, "ephemeral_5m_input_tokens": 0},
           "speed": "standard"})
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn assistant(
    seed: u64,
    t: DateTime<Utc>,
    model: &str,
    effort: &str,
    cwd: &str,
    agent: Option<&str>,
    tool: usize,
) -> (serde_json::Value, String) {
    let (name, key, val) = TOOLS[tool % TOOLS.len()];
    // Ids must be unique across sessions and agents, or tool results land on the wrong call.
    let who = agent.unwrap_or("main");
    let tool_id = format!("toolu_demo{seed:x}_{who}_{}", t.timestamp_millis());
    let mut v = json!({
        "type": "assistant", "timestamp": ts(t), "cwd": cwd, "gitBranch": branch_of(cwd), "effort": effort, "requestedModel": model,
        "message": {"id": format!("msg_demo{seed:x}"), "model": model,
                    "content": [{"type": "tool_use", "id": tool_id, "name": name, "input": {key: val}}],
                    "stop_reason": "tool_use", "usage": usage(seed, model)}
    });
    if let Some(a) = agent {
        v["agentId"] = json!(a);
        v["isSidechain"] = json!(true);
    }
    (v, tool_id)
}

fn tool_result(t: DateTime<Utc>, tool_id: &str) -> serde_json::Value {
    json!({"type": "user", "timestamp": ts(t),
           "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": "ok"}]},
           "toolUseResult": {"stdout": "ok"}})
}

fn append(path: &Path, lines: &[serde_json::Value]) {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        for l in lines {
            let _ = writeln!(f, "{l}");
        }
    }
}

/// Claude Code's transcript dir of a checkout's project.
fn project_dir(seat_dir: &Path, checkout: &str) -> PathBuf {
    seat_dir
        .join("projects")
        .join(format!("-home-dev-work-{}", project_of(checkout)))
}

/// Where the demo's repositories are: `<root>/work`, set by [`start`].
static WORK: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
/// Checkouts made on disk already.
static MADE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A demo checkout is `project` (the main checkout), `project/.claude/worktrees/<name>` (Claude
/// Code's `--worktree`) or `project-wt/<name>` (a worktree next to the repository).
fn project_of(checkout: &str) -> &str {
    let first = checkout.split('/').next().unwrap_or(checkout);
    first.strip_suffix("-wt").unwrap_or(first)
}

/// The branch Claude Code would record in a checkout.
fn branch_of(cwd: &str) -> String {
    if let Some((_, name)) = cwd.split_once("/.claude/worktrees/") {
        return format!("worktree-{}", name.split('/').next().unwrap_or(name));
    }
    match cwd.split_once("-wt/") {
        Some((_, name)) => format!("fix/{}", name.split('/').next().unwrap_or(name)),
        None => "main".into(),
    }
}

/// The working directory of a checkout, made on disk the first time (a repository with a
/// `.git` dir; a linked worktree with the `.git` file git writes), so projects and worktrees
/// resolve as they do on a real machine.
fn cwd(checkout: &str) -> String {
    let work = match WORK.lock() {
        Ok(w) => w.clone(),
        Err(p) => p.into_inner().clone(),
    }
    .unwrap_or_else(|| PathBuf::from("/home/dev/work"));
    let dir = work.join(checkout);
    let mut made = match MADE.lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    if !made.iter().any(|m| m == checkout) {
        let repo = work.join(project_of(checkout));
        let _ = std::fs::create_dir_all(repo.join(".git"));
        if dir != repo {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let gitdir = repo.join(".git").join("worktrees").join(name);
            let _ = std::fs::create_dir_all(&gitdir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(".git"), format!("gitdir: {}\n", gitdir.display()));
        }
        made.push(checkout.to_string());
    }
    dir.to_string_lossy().into_owned()
}

/// Builds the demo world under `root` (wiped first) and starts the agents.
pub fn start(root: &Path, load: Load) -> anyhow::Result<Demo> {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root)?;
    match WORK.lock() {
        Ok(mut w) => *w = Some(root.join("work")),
        Err(p) => *p.into_inner() = Some(root.join("work")),
    }
    match MADE.lock() {
        Ok(mut m) => m.clear(),
        Err(p) => p.into_inner().clear(),
    }
    let now = Utc::now();
    if load == Load::CodexOnly {
        return codex_only(root, now);
    }
    let mut seats = Vec::new();
    let defs = match load {
        Load::Demo | Load::CodexOnly => &SEATS[..2],
        Load::Wall => SEATS,
    };
    for (si, s) in defs.iter().enumerate() {
        let dir = root.join(format!(".claude-{}", s.label));
        std::fs::create_dir_all(dir.join("projects"))?;
        std::fs::create_dir_all(dir.join("sessions"))?;
        let cj = json!({
            "oauthAccount": {"accountUuid": s.account, "emailAddress": s.email, "displayName": "Alex Demo",
                             "userRateLimitTier": "default_claude_max_20x"},
            "cachedUsageUtilization": {"fetchedAtMs": (now - Duration::minutes(4)).timestamp_millis(), "accountUuid": s.account,
                "utilization": {"seven_day": {"utilization": s.utilization, "resets_at": ts(now + Duration::hours(s.resets_in_h))},
                                "five_hour": {"utilization": 22.0 + si as f64 * 15.0, "resets_at": ts(now + Duration::minutes(130 + si as i64 * 60))}}}
        });
        std::fs::write(dir.join(".claude.json"), serde_json::to_string_pretty(&cj)?)?;
        std::fs::write(
            dir.join(".credentials.json"),
            json!({"claudeAiOauth": {"accessToken": format!("demo-access-{}", s.label), "refreshToken": format!("demo-refresh-{}", s.label),
                   "expiresAt": (now + Duration::hours(6)).timestamp_millis(), "refreshTokenExpiresAt": (now + Duration::days(25)).timestamp_millis()}})
            .to_string(),
        )?;
        match load {
            Load::Demo | Load::CodexOnly => history(&dir, s, si as u64, now),
            Load::Wall => wall_history(&dir, s, si as u64, now),
        }
        seats.push(Seat {
            found_by: vec!["demo"],
            ..Seat::new(s.label, dir.clone(), dir.join(".claude.json"), false)
        });
    }
    let seat_dirs: Vec<PathBuf> = seats.iter().map(|s| s.dir.clone()).collect();
    let agy = agy_seat(root, now)?;
    let agy_dir = agy.dir.clone();
    seats.push(agy);
    let (codex, codex_live) = codex_seat(root, now)?;
    let codex_dir = codex.dir.clone();
    seats.push(codex);
    let stop = Arc::new(AtomicBool::new(false));
    let st = stop.clone();
    // Named, so `tools/perf.sh` can leave the writer out of the dashboard's CPU time.
    std::thread::Builder::new()
        .name("demo-writer".into())
        .spawn(move || live(seat_dirs, agy_dir, (codex_dir, codex_live), st, load))?;
    Ok(Demo {
        root: root.to_path_buf(),
        seats,
        stop,
    })
}

/// [`Load::CodexOnly`]: an empty Claude Code seat (only a `settings.json`) and the Codex home,
/// whose running thread keeps working.
fn codex_only(root: &Path, now: DateTime<Utc>) -> anyhow::Result<Demo> {
    let claude = root.join(".claude");
    std::fs::create_dir_all(&claude)?;
    std::fs::write(claude.join("settings.json"), "{}")?;
    let (codex, rollout) = codex_seat(root, now)?;
    let codex_dir = codex.dir.clone();
    let seats = vec![
        Seat {
            found_by: vec!["demo"],
            ..Seat::new(
                "default",
                claude.clone(),
                claude.join(".claude.json"),
                false,
            )
        },
        codex,
    ];
    let stop = Arc::new(AtomicBool::new(false));
    let st = stop.clone();
    std::thread::Builder::new()
        .name("demo-writer".into())
        .spawn(move || {
            let mut thread = codex::Live::start(&codex_dir, rollout);
            let mut tick = 0;
            while !st.load(Ordering::Relaxed) {
                thread.step(Utc::now(), tick);
                tick += 1;
                std::thread::sleep(std::time::Duration::from_millis(700));
            }
        })?;
    Ok(Demo {
        root: root.to_path_buf(),
        seats,
        stop,
    })
}

/// Two weeks of finished sessions with subagents, busier on weekdays and in working hours.
fn history(dir: &Path, s: &SeatDef, si: u64, now: DateTime<Utc>) {
    for day in 0..15i64 {
        let weekday = (now - Duration::days(day))
            .format("%u")
            .to_string()
            .parse::<u64>()
            .unwrap_or(1);
        let load = if weekday >= 6 { 0.35 } else { 1.0 } * (0.6 + frac(si * 100 + day as u64, 9));
        for (pi, project) in PROJECTS.iter().enumerate() {
            if si == 1 && pi == 3 || si == 0 && pi == 2 {
                continue;
            }
            let seed0 = si * 10_000 + day as u64 * 100 + pi as u64;
            let sid = format!("{:08x}-demo-{}-{day}-{pi}", rnd(seed0, 7) as u32, s.label);
            let pdir = project_dir(dir, project);
            // Every third session works in a worktree of its own.
            let checkout = if (day as u64 + pi as u64 + si).is_multiple_of(3) {
                format!("{project}/.claude/worktrees/task-{day}")
            } else {
                project.to_string()
            };
            let start = if day == 0 {
                now - Duration::minutes(55 + (rnd(seed0, 8) % 120) as i64)
            } else {
                now - Duration::days(day) + Duration::hours(-14 + (rnd(seed0, 8) % 6) as i64)
            };
            let mut main = vec![
                json!({"type": "session-owner", "accountUuid": s.account, "sessionId": sid}),
                json!({"type": "custom-title", "customTitle": format!("{project} sprint"), "sessionId": sid}),
                json!({"type": "user", "timestamp": ts(start), "message": {"role": "user", "content": "Plan and run today's tasks"}}),
            ];
            let n_main = (20.0 * load) as u64 + 5;
            for i in 0..n_main {
                let seed = seed0 * 1000 + i;
                let t = start + Duration::minutes(i as i64 * 3);
                let (a, tid) = assistant(
                    seed,
                    t,
                    "claude-opus-5-5",
                    "high",
                    &cwd(&checkout),
                    None,
                    i as usize,
                );
                main.push(a);
                main.push(tool_result(t + Duration::seconds(20), &tid));
            }
            append(&pdir.join(format!("{sid}.jsonl")), &main);
            let n_agents = (4.0 * load) as u64 + 1;
            for k in 0..n_agents {
                let aid = format!("ad{:015x}", rnd(seed0, 50 + k) >> 4);
                let (model, effort) =
                    MODELS[(rnd(seed0, 60 + k) % if pi == 2 { 4 } else { 3 }) as usize];
                let sub = pdir.join(&sid).join("subagents");
                let meta = json!({"agentType": if k % 3 == 0 { "Explore" } else { "general-purpose" },
                                  "description": AGENT_NAMES[((seed0 + k) % AGENT_NAMES.len() as u64) as usize],
                                  "model": model, "effort": effort, "spawnDepth": 1, "finished": true});
                let _ = std::fs::create_dir_all(&sub);
                let _ =
                    std::fs::write(sub.join(format!("agent-{aid}.meta.json")), meta.to_string());
                let mut lines = Vec::new();
                let n = (30.0 * load) as u64 + 8;
                for i in 0..n {
                    let seed = seed0 * 100_000 + k * 1000 + i;
                    let t = start
                        + Duration::minutes(5 + k as i64 * 7)
                        + Duration::seconds(i as i64 * 40);
                    let (a, tid) = assistant(
                        seed,
                        t,
                        model,
                        effort,
                        &cwd(&checkout),
                        Some(&aid),
                        (k + i) as usize,
                    );
                    lines.push(a);
                    lines.push(tool_result(t + Duration::seconds(15), &tid));
                }
                append(&sub.join(format!("agent-{aid}.jsonl")), &lines);
            }
        }
    }
}

/// Dates a transcript written now to its last record, as Claude Code would have left it: the
/// dashboard's hot refresh looks at the files written in the last half hour.
fn written_at(path: &Path, t: DateTime<Utc>) {
    if let Ok(f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = f.set_modified(t.into());
    }
}

struct LiveAgent {
    path: PathBuf,
    meta: PathBuf,
    id: String,
    model: &'static str,
    effort: &'static str,
    project: &'static str,
    open_tool: Option<(String, DateTime<Utc>)>,
    steps_left: u64,
    seed: u64,
}

/// Ten days of a busy wall: per seat and day two long sessions, each a main thread of 300
/// requests and 20 subagents of 90.
fn wall_history(dir: &Path, s: &SeatDef, si: u64, now: DateTime<Utc>) {
    for day in 0..10i64 {
        for k in 0..2u64 {
            let pi = ((si + k) % PROJECTS.len() as u64) as usize;
            let project = PROJECTS[pi];
            let seed0 = 1_000_000_000_000
                + si * 100_000_000_000
                + day as u64 * 1_000_000_000
                + k * 100_000_000;
            let sid = format!("{:08x}-wall-{}-{day}-{k}", rnd(seed0, 7) as u32, s.label);
            let pdir = project_dir(dir, project);
            let checkout = if k == 1 && day % 2 == 0 {
                format!("{project}/.claude/worktrees/wall-{day}")
            } else {
                project.to_string()
            };
            let start =
                now - Duration::days(day) - Duration::hours(11) + Duration::minutes(k as i64 * 20);
            let mut main = vec![
                json!({"type": "session-owner", "accountUuid": s.account, "sessionId": sid}),
                json!({"type": "custom-title", "customTitle": format!("{project} wall {day}"), "sessionId": sid}),
            ];
            for i in 0..300u64 {
                let t = start + Duration::minutes(i as i64 * 2);
                let (a, tid) = assistant(
                    seed0 + i,
                    t,
                    "claude-opus-5-5",
                    "high",
                    &cwd(&checkout),
                    None,
                    i as usize,
                );
                main.push(a);
                main.push(tool_result(t + Duration::seconds(30), &tid));
            }
            let main_path = pdir.join(format!("{sid}.jsonl"));
            append(&main_path, &main);
            written_at(&main_path, start + Duration::minutes(600));
            for g in 0..20u64 {
                let aid = format!("aw{:015x}", rnd(seed0, 50 + g) >> 4);
                let (model, effort) = MODELS[(rnd(seed0, 60 + g) % 3) as usize];
                let sub = pdir.join(&sid).join("subagents");
                let _ = std::fs::create_dir_all(&sub);
                let meta = json!({"agentType": if g % 3 == 0 { "Explore" } else { "general-purpose" },
                                  "description": AGENT_NAMES[((seed0 + g) % AGENT_NAMES.len() as u64) as usize],
                                  "model": model, "effort": effort, "spawnDepth": 1, "finished": true});
                let _ =
                    std::fs::write(sub.join(format!("agent-{aid}.meta.json")), meta.to_string());
                let mut lines = Vec::new();
                for i in 0..90u64 {
                    let t =
                        start + Duration::minutes(g as i64 * 25) + Duration::seconds(i as i64 * 20);
                    let (a, tid) = assistant(
                        seed0 + 1_000_000 + g * 1000 + i,
                        t,
                        model,
                        effort,
                        &cwd(&checkout),
                        Some(&aid),
                        (g + i) as usize,
                    );
                    lines.push(a);
                    lines.push(tool_result(t + Duration::seconds(12), &tid));
                }
                let path = sub.join(format!("agent-{aid}.jsonl"));
                append(&path, &lines);
                written_at(&path, start + Duration::minutes(g as i64 * 25 + 30));
            }
        }
    }
}

/// Keeps the demo agents working until stopped.
fn live(
    seat_dirs: Vec<PathBuf>,
    agy_dir: PathBuf,
    (codex_dir, codex_rollout): (PathBuf, PathBuf),
    stop: Arc<AtomicBool>,
    load: Load,
) {
    let wall = load == Load::Wall;
    // The running agy conversation: its presence lock held as long as the demo runs.
    let _agy_presence = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(agy_dir.join("presence").join(format!("{AGY_LIVE}.lock")))
        .and_then(|f| f.lock().map(|()| f));
    let agy_live = agy_transcript(&agy_dir, AGY_LIVE);
    let mut agy_step: u64 = 100;
    // The running Codex thread: its writer lock held as long as the demo runs.
    let mut codex_thread = codex::Live::start(&codex_dir, codex_rollout);
    let pid = std::process::id();
    let start = proc_start_ticks(pid);
    let now = Utc::now();
    // (seat, checkout, session id, name): the storefront's main checkout has two live sessions
    // (⇄2 on screen), the others work in worktrees.
    let sessions: Vec<(usize, &str, &str, &str)> = [
        (
            0usize,
            "storefront",
            "a1b2c3d4-demo-live-storefront",
            "checkout-revamp",
        ),
        (
            1,
            "payments-api-wt/refund-retry",
            "e5f6a7b8-demo-live-payments",
            "refunds",
        ),
        (
            2,
            "ml-pipeline/.claude/worktrees/feature-store",
            "c9d0e1f2-demo-live-ml",
            "feature-store",
        ),
        (1, "storefront", "b3c4d5e6-demo-live-hotfix", "cart-hotfix"),
    ]
    .into_iter()
    .filter(|s| s.0 < seat_dirs.len())
    .collect();
    let sessions = &sessions[..];
    // Main threads with a little recent history, and the registry entries.
    // The registry is keyed by pid, so a second live session on one seat needs another live
    // process: the demo's parent (the shell) stands in for it.
    #[cfg(unix)]
    let other_pid = std::os::unix::process::parent_id();
    #[cfg(not(unix))]
    let other_pid = 0;
    let other_start = proc_start_ticks(other_pid);
    for (k, &(si, project, sid, name)) in sessions.iter().enumerate() {
        let dir = &seat_dirs[si];
        let second = sessions[..k].iter().any(|o| o.0 == si);
        let p = project_dir(dir, project).join(format!("{sid}.jsonl"));
        let mut lines = vec![
            json!({"type": "session-owner", "accountUuid": SEATS[si].account, "sessionId": sid}),
            json!({"type": "custom-title", "customTitle": name, "sessionId": sid}),
        ];
        // The wall's live sessions have run for a day already, like long-lived orchestrators.
        let (n, step) = if wall { (1500u64, 1) } else { (12, 3) };
        for i in 0..n {
            let t = now - Duration::minutes(4 + (n - i) as i64 * step);
            let (a, tid) = assistant(
                if wall {
                    500_000_000 + si as u64 * 10_000 + i
                } else {
                    9_000 + i + si as u64 * 50
                } + if second { 70_000_000 } else { 0 },
                t,
                "claude-opus-5-5",
                "high",
                &cwd(project),
                None,
                i as usize,
            );
            lines.push(a);
            lines.push(tool_result(t + Duration::seconds(30), &tid));
        }
        append(&p, &lines);
        if second {
            write_registry(
                dir,
                other_pid,
                other_start.as_deref(),
                sid,
                project,
                name,
                "idle",
            );
        } else {
            write_registry(dir, pid, start.as_deref(), sid, project, name, "busy");
        }
    }
    // A stalled agent from 40 minutes ago.
    {
        let sub = project_dir(&seat_dirs[0], "storefront")
            .join(sessions[0].2)
            .join("subagents");
        let _ = std::fs::create_dir_all(&sub);
        let _ = std::fs::write(
            sub.join("agent-adstalled0000001.meta.json"),
            json!({"agentType": "general-purpose", "description": "Bisect the cart regression", "model": "claude-opus-5-5", "effort": "high", "spawnDepth": 1, "finished": false}).to_string(),
        );
        let t = now - Duration::minutes(41);
        let (a, _) = assistant(
            77_777,
            t,
            "claude-opus-5-5",
            "high",
            &cwd("storefront"),
            Some("adstalled0000001"),
            0,
        );
        append(&sub.join("agent-adstalled0000001.jsonl"), &[a]);
    }
    let mut agents: Vec<LiveAgent> = Vec::new();
    let mut tick: u64 = 0;
    let mut spawned: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        let now = Utc::now();
        // Keep 5–7 agents running (17–19 on the wall).
        let want = if wall { 17 } else { 5 } + (tick / 7 % 3);
        while (agents.len() as u64) < want {
            spawned += 1;
            let (si, project, sid, _) = sessions[(spawned % sessions.len() as u64) as usize];
            let (model, effort) = MODELS[(spawned * 7 % 3) as usize];
            let id = format!("adlive{spawned:011x}");
            let sub = project_dir(&seat_dirs[si], project)
                .join(sid)
                .join("subagents");
            let _ = std::fs::create_dir_all(&sub);
            let meta = sub.join(format!("agent-{id}.meta.json"));
            let desc = AGENT_NAMES[(spawned as usize * 3) % AGENT_NAMES.len()];
            let _ = std::fs::write(&meta, json!({"agentType": if spawned.is_multiple_of(4) { "Explore" } else { "general-purpose" }, "description": desc, "model": model, "effort": effort, "spawnDepth": 1, "finished": false}).to_string());
            agents.push(LiveAgent {
                path: sub.join(format!("agent-{id}.jsonl")),
                meta,
                id,
                model,
                effort,
                project,
                open_tool: None,
                steps_left: if wall {
                    20 + spawned % 30
                } else {
                    6 + spawned % 9
                },
                seed: 1_000_000 + spawned * 1000,
            });
        }
        for a in agents.iter_mut() {
            a.seed += 1;
            match &a.open_tool {
                Some((tid, since))
                    if (now - *since).num_seconds() as u64
                        >= if wall { 4 + a.seed % 9 } else { 2 + a.seed % 5 } =>
                {
                    append(&a.path, &[tool_result(now, tid)]);
                    a.open_tool = None;
                    a.steps_left = a.steps_left.saturating_sub(1);
                }
                None if a.seed % 3 != 0 => {
                    let (v, tid) = assistant(
                        a.seed,
                        now,
                        a.model,
                        a.effort,
                        &cwd(a.project),
                        Some(&a.id),
                        (a.seed % 11) as usize,
                    );
                    append(&a.path, &[v]);
                    a.open_tool = Some((tid, now));
                }
                _ => {}
            }
        }
        // The wall's main threads answer now and then too.
        if wall && tick.is_multiple_of(14) {
            for (k, &(si, project, sid, _)) in sessions.iter().enumerate() {
                let (v, tid) = assistant(
                    2_000_000_000 + tick * 10 + k as u64,
                    now,
                    "claude-opus-5-5",
                    "high",
                    &cwd(project),
                    None,
                    tick as usize,
                );
                let p = project_dir(&seat_dirs[si], project).join(format!("{sid}.jsonl"));
                append(&p, &[v, tool_result(now, &tid)]);
            }
        }
        // The agy conversation takes a step now and then, a quick one on Hydrogen among them
        // (BURN tells the two Gemini 4 models apart).
        if tick.is_multiple_of(4) {
            let model = if tick.is_multiple_of(12) {
                "gemini-4-hydrogen"
            } else {
                "gemini-4-neon-high"
            };
            let (p, g) = agy_planner(3_000_000 + tick, agy_step, now, model, tick);
            append(&agy_live, &[p, g]);
            agy_step += 2;
        }
        codex_thread.step(now, tick);
        // Finished agents.
        agents.retain(|a| {
            if a.steps_left == 0 && a.open_tool.is_none() {
                if let Ok(text) = std::fs::read_to_string(&a.meta)
                    && let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text)
                {
                    v["finished"] = json!(true);
                    let _ = std::fs::write(&a.meta, v.to_string());
                }
                false
            } else {
                true
            }
        });
        // The payments session asks for permission every so often.
        let waiting = tick % 30 >= 22;
        let (si, project, sid, name) = sessions[1];
        write_registry(
            &seat_dirs[si],
            pid,
            start.as_deref(),
            sid,
            project,
            name,
            if waiting { "waiting" } else { "busy" },
        );
        tick += 1;
        std::thread::sleep(std::time::Duration::from_millis(700));
    }
}

fn write_registry(
    dir: &Path,
    pid: u32,
    start: Option<&str>,
    sid: &str,
    project: &str,
    name: &str,
    status: &str,
) {
    let mut v = json!({"pid": pid, "sessionId": sid, "cwd": cwd(project), "startedAt": Utc::now().timestamp_millis() - 3_600_000,
                       "version": "demo", "kind": "interactive", "name": name, "status": status,
                       "statusUpdatedAt": Utc::now().timestamp_millis()});
    if status == "waiting" {
        v["waitingFor"] = json!("permission: Bash git push");
    }
    if let Some(s) = start {
        v["procStart"] = json!(s);
    }
    // One file per session: the registry is keyed by pid, so use a per-session name.
    let _ = std::fs::write(
        dir.join("sessions").join(format!("{pid}.json")),
        v.to_string(),
    );
}

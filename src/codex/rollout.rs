//! Parsing of OpenAI Codex rollouts: `<home>/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`.
//!
//! Each line is `{"timestamp", "type", "payload"}` (verified on Codex 0.172):
//! - `session_meta`: the thread's id, `cwd`, `git` (branch), `cli_version`, and the account it
//!   runs under (`creator_user_id`, `creator_account_id`);
//! - `turn_context`: the model, `cwd` and effort of the turn that follows;
//! - `response_item`: messages, reasoning, tool calls (`function_call`, `custom_tool_call`,
//!   `local_shell_call`, `web_search_call`) and their outputs (`…_output`, by `call_id`);
//! - `event_msg`: `task_started` / `task_complete` (a turn), `token_count` (`info.total_token_usage`
//!   and `info.last_token_usage`, plus `rate_limits`), `item_completed` (a user message), …;
//! - `token_usage_record`: one response's `usage` with the thread's running total
//!   (`thread_token_usage`), written next to the `token_count` of the same response.
//!
//! Usage is per response: `input_tokens` includes `cached_input_tokens`, and `output_tokens`
//! includes `reasoning_output_tokens`. One response is named by the thread's running total of
//! tokens after it (`cdx:<thread>:<total>`), so its `token_usage_record` and its `token_count`
//! (and a `token_count` repeated with unchanged totals) are one request, never two.
//!
//! The model is not on the usage lines but on the `turn_context` before them, so reading from an
//! offset needs what came before: the state at the end of each read is kept per file, and a read
//! from an offset this process has no state for replays the file up to it once.
//!
//! Rollouts of the first Codex versions (2025) had no `type`/`payload` envelope, no timestamps
//! per line and no token counts: their lines are counted as `legacy` and read no further.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::core::tools::ToolClass;
use crate::core::usage::Usage;
use crate::model::event::{Chunk, Event, ParseStats, Request, ToolUse};

/// The model of a record that carries tool calls but no usage (see [`Ctx::flush_tools`]).
const NO_REQUEST_MODEL: &str = "<synthetic>";

/// The model of a response whose turn named none (no `turn_context` before it).
const UNKNOWN_MODEL: &str = "unknown";

/// How the index counts lines of the 2025 format.
pub const LEGACY: &str = "legacy";

/// The record types of a rollout swarmy knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    SessionMeta,
    TurnContext,
    ResponseItem,
    EventMsg,
    TokenUsage,
    /// The model's whole context (instructions, tools, environment): nothing to count.
    WorldState,
    Compacted,
}

const RECORDS: [(&str, Record); 7] = [
    ("session_meta", Record::SessionMeta),
    ("turn_context", Record::TurnContext),
    ("response_item", Record::ResponseItem),
    ("event_msg", Record::EventMsg),
    ("token_usage_record", Record::TokenUsage),
    ("world_state", Record::WorldState),
    ("compacted", Record::Compacted),
];

/// Record types swarmy recognises in Codex rollouts (for `doctor` format-drift checks).
pub const KNOWN_TYPES: [&str; 8] = [
    RECORDS[0].0,
    RECORDS[1].0,
    RECORDS[2].0,
    RECORDS[3].0,
    RECORDS[4].0,
    RECORDS[5].0,
    RECORDS[6].0,
    LEGACY,
];

/// The response items swarmy reads (others, such as reasoning, carry nothing it counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Message,
    FunctionCall,
    CustomToolCall,
    LocalShellCall,
    WebSearchCall,
    /// The output of a call, by `call_id`.
    Output,
}

const ITEMS: [(&str, Item); 8] = [
    ("message", Item::Message),
    ("function_call", Item::FunctionCall),
    ("custom_tool_call", Item::CustomToolCall),
    ("local_shell_call", Item::LocalShellCall),
    ("web_search_call", Item::WebSearchCall),
    ("function_call_output", Item::Output),
    ("custom_tool_call_output", Item::Output),
    ("local_shell_call_output", Item::Output),
];

/// The events swarmy reads (Codex writes many more: they are skipped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ev {
    TokenCount,
    TaskStarted,
    TaskComplete,
    TurnAborted,
    ThreadSettingsApplied,
    /// A prompt, as Codex before the item events wrote it.
    UserMessage,
    ItemCompleted,
    AgentMessage,
    Error,
}

const EVENTS: [(&str, Ev); 9] = [
    ("token_count", Ev::TokenCount),
    ("task_started", Ev::TaskStarted),
    ("task_complete", Ev::TaskComplete),
    ("turn_aborted", Ev::TurnAborted),
    ("thread_settings_applied", Ev::ThreadSettingsApplied),
    ("user_message", Ev::UserMessage),
    ("item_completed", Ev::ItemCompleted),
    ("agent_message", Ev::AgentMessage),
    ("error", Ev::Error),
];

fn lookup<T: Copy>(table: &[(&str, T)], key: Option<&str>) -> Option<T> {
    let key = key?;
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// What the lines before a point say that the lines after it need.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    model: Option<String>,
    effort: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    /// The turn runs on the priority tier (`service_tier` "priority" or "fast").
    fast: bool,
    /// Tool calls of the response whose usage has not been read yet.
    pending_tools: Vec<ToolUse>,
    /// Web searches of that response.
    pending_web: u64,
    /// The assistant's last text, for the request that follows and the AGENT panel.
    text: Option<String>,
    last_text: Option<String>,
    /// The last prompt (text and time): Codex may write one prompt as two events.
    last_prompt: Option<(String, DateTime<Utc>)>,
    /// The turn running since a `task_started`, if no `task_complete` / `turn_aborted` came yet.
    turn_started: Option<DateTime<Utc>>,
    /// The last turn event's time.
    turn_event: Option<DateTime<Utc>>,
}

impl Ctx {
    /// The assistant wrote `text`: the next request carries it, the AGENT panel shows it.
    fn said(&mut self, text: &str) {
        let t = truncate(text, 160);
        self.last_text = Some(t.clone());
        self.text = Some(t);
    }

    /// Tool calls left without a usage line (an aborted turn, an output before the usage): one
    /// record with no usage, so the calls still count and their results find them.
    fn flush_tools(&mut self, ts: DateTime<Utc>, out: &mut Vec<Event>) {
        if self.pending_tools.is_empty() {
            return;
        }
        let tools = std::mem::take(&mut self.pending_tools);
        out.push(Event::Request(Request {
            msg_id: format!("{}:tools", tools[0].id),
            request_id: None,
            ts,
            model: NO_REQUEST_MODEL.into(),
            requested_model: None,
            effort: None,
            fast: false,
            usage: Usage::default(),
            cwd: self.cwd.clone(),
            git_branch: self.branch.clone(),
            agent_id: None,
            tool_uses: tools,
            stop_reason: None,
            text: None,
            thinking: false,
        }));
    }
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn truncate(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut t: String = line.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// A response's usage in swarmy's terms, and the thread's running total after it.
///
/// `input_tokens` includes the cached part (and, assumed the same way, the cache-write part);
/// what is left is fresh input. `output_tokens` includes reasoning, kept apart as thinking.
fn usage_of(u: &Value) -> Usage {
    let input = u64_of(u, "input_tokens");
    let cached = u64_of(u, "cached_input_tokens");
    let written = u64_of(u, "cache_write_input_tokens");
    Usage {
        input: input.saturating_sub(cached).saturating_sub(written),
        cache_write_5m: written,
        cache_write_1h: 0,
        cache_read: cached,
        output: u64_of(u, "output_tokens"),
        thinking: u64_of(u, "reasoning_output_tokens"),
        web_search: 0,
    }
}

fn total_of(u: &Value) -> Option<u64> {
    u.get("total_tokens").and_then(Value::as_u64)
}

/// A shell call's command: `["bash", "-lc", "cargo test"]` → `cargo test`.
fn command_of(args: &Value) -> String {
    match args.get("command").or_else(|| args.get("cmd")) {
        Some(Value::Array(parts)) => {
            let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            if let [sh, flag, cmd] = parts.as_slice()
                && flag.starts_with('-')
                && sh.ends_with("sh")
            {
                (*cmd).into()
            } else {
                parts.join(" ")
            }
        }
        Some(Value::String(s)) => s.clone(),
        Some(_) | None => String::new(),
    }
}

/// The first file an `apply_patch` touches (`*** Update File: src/lib.rs`).
fn patch_target(patch: &str) -> Option<String> {
    patch.lines().find_map(|l| {
        ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
            .iter()
            .find_map(|p| l.strip_prefix(p))
            .map(|p| p.trim().to_string())
    })
}

/// One line describing a call, and the file it works on.
fn summarize_tool(name: &str, args: &Value, input: Option<&str>) -> (String, Option<String>) {
    let class = ToolClass::of(name);
    let path = str_of(args, "path")
        .or_else(|| str_of(args, "file_path"))
        .map(str::to_string)
        .or_else(|| {
            (name == "apply_patch")
                .then(|| {
                    input
                        .or_else(|| str_of(args, "input"))
                        .and_then(patch_target)
                })
                .flatten()
        });
    let summary = match class {
        ToolClass::Shell => truncate(&command_of(args), 120),
        ToolClass::Edit | ToolClass::Read => path.clone().unwrap_or_default(),
        ToolClass::Agent | ToolClass::AskUser | ToolClass::Plan | ToolClass::Other => match input {
            Some(text) => truncate(text, 80),
            None => args
                .as_object()
                .and_then(|m| {
                    m.values()
                        .find_map(|v| v.as_str().filter(|s| !s.is_empty() && s.len() < 200))
                })
                .map(|s| truncate(s, 80))
                .unwrap_or_default(),
        },
    };
    let target = class.has_target().then_some(path).flatten();
    (summary, target)
}

/// Whether a path a rollout names is relative, whatever system wrote it (`/w`, `C:\\w` and
/// `\\\\host` are not).
fn is_relative(p: &str) -> bool {
    let b = p.as_bytes();
    !(p.starts_with(['/', '\\']) || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()))
}

/// `rel` under `cwd`, with `cwd`'s separator throughout, so a file a patch names reads the same
/// as the same file named by another tool (an edit conflict is found by the path).
fn join_like(cwd: &str, rel: &str) -> String {
    let sep = if cwd.contains('\\') && !cwd.contains('/') {
        '\\'
    } else {
        '/'
    };
    let rel: String = rel
        .chars()
        .map(|c| if c == '/' || c == '\\' { sep } else { c })
        .collect();
    format!("{}{sep}{rel}", cwd.trim_end_matches(['/', '\\']))
}

/// A code-mode script (`const r = await tools.exec_command({cmd: "ls -la"}); …`): the tool it
/// calls (`exec` when it calls several kinds) and its commands, `; `-joined (else the tools).
/// `None` when it calls no tool.
fn code_mode(script: &str) -> Option<(String, String)> {
    let mut tools: Vec<String> = Vec::new();
    for (i, _) in script.match_indices("tools.") {
        let name: String = script[i + 6..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && !tools.contains(&name) {
            tools.push(name);
        }
    }
    if tools.is_empty() {
        return None;
    }
    let mut cmds: Vec<String> = Vec::new();
    for (i, _) in script.match_indices("cmd") {
        let rest = script[i + 3..].trim_start_matches(['"', '\'']);
        let Some(rest) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(q) = rest
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\'' | '`'))
        else {
            continue;
        };
        let mut cmd = String::new();
        let mut esc = false;
        for c in rest[1..].chars() {
            match (esc, c) {
                (true, c) => {
                    cmd.push(c);
                    esc = false;
                }
                (false, '\\') => esc = true,
                (false, c) if c == q => break,
                (false, c) => cmd.push(c),
            }
        }
        if !cmd.is_empty() {
            cmds.push(cmd);
        }
    }
    let name = match tools.as_slice() {
        [one] => one.clone(),
        [] | [_, _, ..] => "exec".to_string(),
    };
    let summary = if cmds.is_empty() {
        tools.join(", ")
    } else {
        cmds.join("; ")
    };
    Some((name, truncate(&summary, 120)))
}

/// Whether a tool's output says it failed: `{"success": false}`, or (older Codex) an output JSON
/// whose `metadata.exit_code` is not 0.
fn output_failed(out: Option<&Value>) -> bool {
    match out {
        Some(Value::Object(m)) => m.get("success").and_then(Value::as_bool) == Some(false),
        Some(Value::String(s)) if s.starts_with('{') => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| v.pointer("/metadata/exit_code").and_then(Value::as_i64))
            .is_some_and(|c| c != 0),
        Some(_) | None => false,
    }
}

/// The text of a message's content blocks (`input_text`, `output_text`, `text`).
fn content_text(content: Option<&Value>) -> Option<String> {
    let parts: Vec<&str> = content?
        .as_array()?
        .iter()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect();
    let t = parts.join("\n");
    (!t.trim().is_empty()).then_some(t)
}

/// Parses one line of the rollout of thread `sid`, with what the lines before it said.
pub fn parse_line(
    line: &str,
    sid: &str,
    ctx: &mut Ctx,
    out: &mut Vec<Event>,
    stats: &mut ParseStats,
) {
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }
    stats.lines += 1;
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        stats.malformed += 1;
        return;
    };
    let (Some(kind), Some(payload)) = (str_of(&v, "type"), v.get("payload")) else {
        *stats.by_type.entry(LEGACY.to_string()).or_default() += 1;
        return;
    };
    *stats.by_type.entry(kind.to_string()).or_default() += 1;
    let ts = parse_ts(str_of(&v, "timestamp"));
    // An unknown record type is counted (`stats.by_type`, `doctor`) and otherwise skipped.
    let Some(kind) = lookup(&RECORDS, Some(kind)) else {
        return;
    };
    match kind {
        Record::SessionMeta => {
            if let Some(c) = str_of(payload, "cwd") {
                ctx.cwd = Some(c.to_string());
            }
            if let Some(b) = payload.pointer("/git/branch").and_then(Value::as_str) {
                ctx.branch = (!b.is_empty()).then(|| b.to_string());
            }
            if let (Some(user), Some(acct)) = (
                str_of(payload, "creator_user_id"),
                str_of(payload, "creator_account_id"),
            ) {
                out.push(Event::Owner {
                    account: super::account::account_id(user, acct),
                });
            }
        }
        Record::TurnContext => {
            if let Some(m) = str_of(payload, "model") {
                ctx.model = Some(m.to_string());
            }
            if let Some(c) = str_of(payload, "cwd") {
                ctx.cwd = Some(c.to_string());
            }
            ctx.effort = str_of(payload, "effort")
                .or_else(|| {
                    payload
                        .pointer("/collaboration_mode/settings/reasoning_effort")
                        .and_then(Value::as_str)
                })
                .map(str::to_string);
            if let Some(t) = str_of(payload, "service_tier") {
                ctx.fast = matches!(t, "priority" | "fast");
            }
        }
        Record::ResponseItem => response_item(payload, ts, sid, ctx, out),
        Record::EventMsg => event_msg(payload, ts, sid, ctx, out),
        Record::TokenUsage => {
            if let (Some(ts), Some(u)) = (ts, payload.get("usage")) {
                let total = payload.get("thread_token_usage").and_then(total_of);
                request(usage_of(u), total, ts, sid, ctx, out);
            }
        }
        Record::Compacted => out.push(Event::Compact { ts }),
        Record::WorldState => {}
    }
}

/// A response's usage as a request, with the tool calls and searches read before it.
fn request(
    usage: Usage,
    total: Option<u64>,
    ts: DateTime<Utc>,
    sid: &str,
    ctx: &mut Ctx,
    out: &mut Vec<Event>,
) {
    if usage.is_empty() {
        return;
    }
    let total = total.unwrap_or_else(|| usage.total());
    let mut usage = usage;
    usage.web_search = std::mem::take(&mut ctx.pending_web);
    out.push(Event::Request(Request {
        msg_id: format!("cdx:{sid}:{total}"),
        request_id: None,
        ts,
        model: ctx.model.clone().unwrap_or_else(|| UNKNOWN_MODEL.into()),
        requested_model: None,
        effort: ctx.effort.clone(),
        fast: ctx.fast,
        thinking: usage.thinking > 0,
        usage,
        cwd: ctx.cwd.clone(),
        git_branch: ctx.branch.clone(),
        agent_id: None,
        tool_uses: std::mem::take(&mut ctx.pending_tools),
        stop_reason: None,
        text: ctx.text.take(),
    }));
}

fn response_item(
    p: &Value,
    ts: Option<DateTime<Utc>>,
    sid: &str,
    ctx: &mut Ctx,
    out: &mut Vec<Event>,
) {
    let call_id = str_of(p, "call_id").or_else(|| str_of(p, "id"));
    let Some(item) = lookup(&ITEMS, str_of(p, "type")) else {
        return;
    };
    match item {
        Item::Message => {
            if str_of(p, "role") == Some("assistant")
                && let Some(t) = content_text(p.get("content"))
            {
                ctx.said(&t);
            }
        }
        Item::FunctionCall | Item::CustomToolCall | Item::LocalShellCall => {
            let Some(call_id) = call_id else { return };
            let (name, args, input) = match item {
                Item::FunctionCall => (
                    str_of(p, "name").unwrap_or("function").to_string(),
                    str_of(p, "arguments")
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .unwrap_or(Value::Null),
                    None,
                ),
                Item::CustomToolCall => (
                    str_of(p, "name").unwrap_or("custom").to_string(),
                    Value::Null,
                    str_of(p, "input"),
                ),
                Item::LocalShellCall | Item::Message | Item::WebSearchCall | Item::Output => (
                    "local_shell".to_string(),
                    p.get("action").cloned().unwrap_or(Value::Null),
                    None,
                ),
            };
            // Code mode (Codex 0.172): one `exec` call runs a script that calls the tools; it is
            // named after them and summed up by their commands.
            let (name, code) = match (item, input.and_then(code_mode)) {
                (Item::CustomToolCall, Some((n, s))) if name == "exec" => (n, Some(s)),
                (_, Some(_) | None) => (name, None),
            };
            let (summary, target) = match code {
                Some(s) => (s, None),
                None => summarize_tool(&name, &args, input),
            };
            // `apply_patch` names files relative to the turn's directory.
            let target = match (target, ctx.cwd.as_deref()) {
                (Some(t), Some(cwd)) if is_relative(&t) => Some(join_like(cwd, &t)),
                (t, Some(_) | None) => t,
            };
            ctx.pending_tools.push(ToolUse {
                id: format!("cdx:{sid}:{call_id}"),
                name,
                summary,
                target,
            });
        }
        Item::WebSearchCall => ctx.pending_web += 1,
        Item::Output => {
            let Some(call_id) = call_id else { return };
            let id = format!("cdx:{sid}:{call_id}");
            // An output before the usage line of its call: write the call first, so the
            // result finds it.
            if ctx.pending_tools.iter().any(|t| t.id == id)
                && let Some(ts) = ts
            {
                ctx.flush_tools(ts, out);
            }
            out.push(Event::ToolResult {
                tool_use_id: id,
                ts,
                is_error: output_failed(p.get("output")),
            });
        }
    }
}

fn event_msg(p: &Value, ts: Option<DateTime<Utc>>, sid: &str, ctx: &mut Ctx, out: &mut Vec<Event>) {
    let Some(ev) = lookup(&EVENTS, str_of(p, "type")) else {
        return;
    };
    match ev {
        Ev::TokenCount => {
            if let (Some(ts), Some(last)) = (ts, p.pointer("/info/last_token_usage")) {
                let total = p.pointer("/info/total_token_usage").and_then(total_of);
                request(usage_of(last), total, ts, sid, ctx, out);
            }
        }
        Ev::TaskStarted => {
            ctx.turn_started = ts;
            ctx.turn_event = ts;
        }
        Ev::TaskComplete | Ev::TurnAborted => {
            if let Some(t) = ts {
                ctx.flush_tools(t, out);
            }
            ctx.turn_started = None;
            ctx.turn_event = ts;
            if ev == Ev::TaskComplete {
                out.push(Event::TurnEnd {
                    ts,
                    duration_ms: u64_of(p, "duration_ms"),
                    pending_background: 0,
                });
            }
        }
        Ev::ThreadSettingsApplied => {
            let s = p.get("thread_settings").unwrap_or(&Value::Null);
            if let Some(m) = str_of(s, "model") {
                ctx.model.get_or_insert_with(|| m.to_string());
            }
            if let Some(t) = str_of(s, "service_tier") {
                ctx.fast = matches!(t, "priority" | "fast");
            }
        }
        Ev::UserMessage => {
            if let Some(t) = str_of(p, "message") {
                prompt(t, ts, ctx, out);
            }
        }
        Ev::ItemCompleted => {
            let item = p.get("item").unwrap_or(&Value::Null);
            let text = content_text(item.get("content"));
            match (str_of(item, "type"), text) {
                (Some("UserMessage"), Some(t)) => prompt(&t, ts, ctx, out),
                (Some("AgentMessage"), Some(t)) => ctx.said(&t),
                (Some(_) | None, Some(_) | None) => {}
            }
        }
        Ev::AgentMessage => {
            if let Some(t) = str_of(p, "message") {
                ctx.said(t);
            }
        }
        Ev::Error => out.push(Event::ApiError {
            ts,
            kind: "codex_error".into(),
            text: str_of(p, "message")
                .map(|m| truncate(m, 200))
                .unwrap_or_default(),
            id: None,
        }),
    }
}

fn prompt(text: &str, ts: Option<DateTime<Utc>>, ctx: &mut Ctx, out: &mut Vec<Event>) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if let (Some((prev, at)), Some(t)) = (&ctx.last_prompt, ts)
        && prev == text
        && (t - *at).abs() < Duration::seconds(5)
    {
        return;
    }
    if let Some(t) = ts {
        ctx.last_prompt = Some((text.to_string(), t));
    }
    out.push(Event::Prompt {
        ts,
        text: truncate(text, 200),
        id: None,
    });
}

/// The state at the end of each file's last read, by path: (offset, file identity, state). A
/// file replaced at the same path and grown to the same offset is another file (review A-H6).
type States = Mutex<HashMap<PathBuf, (u64, u64, Ctx)>>;

fn file_id_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .map(|m| crate::fsutil::file_id(path, &m))
}

static STATES: OnceLock<States> = OnceLock::new();

/// Bounds the kept states (one per rollout read in this process).
const MAX_STATES: usize = 4096;

fn state_at(path: &Path, offset: u64, sid: &str) -> std::io::Result<Ctx> {
    if offset == 0 {
        return Ok(Ctx::default());
    }
    if let Ok(m) = STATES.get_or_init(Default::default).lock()
        && let Some((end, id, ctx)) = m.get(path)
        && *end == offset
        && file_id_of(path) == Some(*id)
    {
        return Ok(ctx.clone());
    }
    // Not read up to here by this process: the lines before say what the next ones need.
    let mut ctx = Ctx::default();
    let (mut sink, mut stats) = (Vec::new(), ParseStats::default());
    let f = File::open(path)?;
    let mut rd = BufReader::with_capacity(256 * 1024, f.take(offset));
    let mut buf = Vec::with_capacity(64 * 1024);
    loop {
        buf.clear();
        if rd.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        if let Ok(line) = std::str::from_utf8(&buf) {
            parse_line(line, sid, &mut ctx, &mut sink, &mut stats);
            sink.clear();
        }
    }
    Ok(ctx)
}

fn keep_state(path: &Path, end: u64, ctx: Ctx) {
    if let Ok(mut m) = STATES.get_or_init(Default::default).lock() {
        if m.len() >= MAX_STATES {
            m.clear();
        }
        if let Some(id) = file_id_of(path) {
            m.insert(path.to_path_buf(), (end, id, ctx));
        }
    }
}

/// Reads complete lines of the rollout of thread `sid` from `offset` to the end. A last line
/// without a newline is still being written and is left for the next read.
pub fn read_from(path: &Path, offset: u64, sid: &str) -> std::io::Result<Chunk> {
    let mut ctx = state_at(path, offset, sid)?;
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut rd = BufReader::with_capacity(256 * 1024, f);
    let mut chunk = Chunk {
        end: offset,
        ..Default::default()
    };
    let mut buf = Vec::with_capacity(64 * 1024);
    loop {
        buf.clear();
        let n = rd.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break;
        }
        chunk.end += n as u64;
        match std::str::from_utf8(&buf) {
            Ok(line) => parse_line(line, sid, &mut ctx, &mut chunk.events, &mut chunk.stats),
            Err(_) => chunk.stats.malformed += 1,
        }
    }
    keep_state(path, chunk.end, ctx);
    Ok(chunk)
}

/// What the end of a rollout says about its thread right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TailState {
    /// A turn runs: `task_started` without a `task_complete` / `turn_aborted` after it.
    pub busy: bool,
    /// The last turn event's time.
    pub since: Option<DateTime<Utc>>,
    /// The assistant's last text.
    pub text: Option<String>,
    /// The thread's working directory.
    pub cwd: Option<String>,
}

/// The thread's directory from the first line (`session_meta`), read up to 1 MiB.
fn head_cwd(path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    let mut line = Vec::new();
    BufReader::new(f.take(1024 * 1024))
        .read_until(b'\n', &mut line)
        .ok()?;
    let (mut ctx, mut sink, mut stats) = (Ctx::default(), Vec::new(), ParseStats::default());
    parse_line(
        &String::from_utf8_lossy(&line),
        "",
        &mut ctx,
        &mut sink,
        &mut stats,
    );
    ctx.cwd
}

/// Reads the last 256 KiB of a rollout (the whole file when smaller).
pub fn tail_state(path: &Path) -> Option<TailState> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(256 * 1024);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let (mut ctx, mut sink, mut stats) = (Ctx::default(), Vec::new(), ParseStats::default());
    for line in text.lines().skip(usize::from(start > 0)) {
        parse_line(line, "", &mut ctx, &mut sink, &mut stats);
        sink.clear();
    }
    let cwd = match ctx.cwd {
        Some(c) => Some(c),
        None if start > 0 => head_cwd(path),
        None => None,
    };
    Some(TailState {
        busy: ctx.turn_started.is_some(),
        since: ctx.turn_event,
        text: ctx.last_text,
        cwd,
    })
}

/// The assistant's last text, for the AGENT panel.
pub fn last_message(path: &Path) -> Option<String> {
    tail_state(path)?.text
}

#[cfg(test)]
mod tests;

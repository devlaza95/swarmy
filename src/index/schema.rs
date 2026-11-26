//! The index schema. Every table is in exactly one class:
//!
//! **Raw tables** hold what the transcripts say, row by row (requests, tool calls, prompts, …).
//! When `CACHE_VERSION` changes they are dropped and read again from the transcripts.
//!
//! **Dimension tables** label the raw rows and outlive them: sessions (titles, compactions),
//! agents (type, description, lifetime), the project of each working directory, the git branch
//! each agent worked on and Claude Code's
//! own cost totals. A `CACHE_VERSION` change keeps them, because Claude Code deletes transcripts
//! after `cleanupPeriodDays` (30 by default) and nothing else knows what an old agent was; files
//! read again merge into them (`COALESCE`, `MIN`, `MAX`, the larger total).
//!
//! **Archive tables** hold quarter-hour rollups of raw rows that are gone (`archive`): never
//! dropped; `ARCHIVE_VERSION` steps migrate them.
//!
//! **Fact tables** hold what swarmy observed over time and nothing else records: which account
//! each seat had logged in and since when (`seat_accounts`), the accounts seen (`accounts`), limit
//! readings (`readings`), the dates Claude Code's prices were first seen (`observed_prices`), and
//! the label and dir last seen for each seat id (`seats`; every `seat` column holds a seat id,
//! `index::seats`).
//! They are never dropped; `FACTS_VERSION` steps migrate them.
//!
//! The rules of the data are constraints in the schema, so every write path is checked, and the
//! lists of enum-like values are generated from the Rust enums (`WindowKind::ALL`,
//! `FileKind::TAGS`) so the two cannot drift.

use rusqlite::{Connection, OptionalExtension, params};

use crate::claude::files::FileKind;
use crate::core::harness::Harness;
use crate::core::limits::WindowKind;

/// Version of the raw tables; a change drops them and reads the transcripts again (never the
/// dimensions or the facts).
pub const CACHE_VERSION: &str = "7";

/// The first cache version whose dimension tables have today's shape: a database older than that
/// drops them with the raw tables (they are rebuilt from the transcripts, as before the classes
/// existed). A later change to a dimension table's shape is a step in `CACHE_STEPS`, never a
/// rebuild.
const DIMENSIONS_SINCE: u32 = 6;

/// Version of the fact tables; each step up has a migration in `migrate_facts`.
pub const FACTS_VERSION: u32 = 5;

/// Read from the transcripts row by row: dropped and read again when `CACHE_VERSION` changes.
pub const RAW_TABLES: &[&str] = &[
    "files",
    "requests",
    "tool_calls",
    "owners",
    "prompts",
    "api_errors",
    "compact_events",
    "record_types",
    "unreadable_files",
];

/// What labels the raw rows (sessions, agents, projects, branches, Claude Code's totals): kept across a
/// `CACHE_VERSION` change, merged into when a file is read again.
pub const DIMENSION_TABLES: &[&str] =
    &["sessions", "agents", "projects", "branches", "cost_states"];

/// Version of the archive tables (`archive`); each step up has a migration in
/// `migrate_archive`.
pub const ARCHIVE_VERSION: u32 = 2;

/// Rollups of raw rows that are gone (`archive`): migrated, never dropped.
pub const ARCHIVE_TABLES: &[&str] = &[
    "usage_rollup",
    "agent_rollup",
    "tool_rollup",
    "edit_rollup",
    "prompt_rollup",
    "api_error_rollup",
    "rollup_seats",
    "storage_samples",
];

/// Observed over time and written nowhere else: migrated, never dropped.
pub const FACT_TABLES: &[&str] = &[
    "meta",
    "seat_accounts",
    "accounts",
    "readings",
    "observed_prices",
    "seats",
];

/// Keys of `meta` that describe the raw tables and go with them (`projects_rev` describes
/// `projects`, a dimension, and stays).
pub const CACHE_META: &[&str] = &["price_fp"];

/// `'a', 'b', 'c'` for an `IN (...)` check.
fn in_list(keys: &[&str]) -> String {
    keys.iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn window_kinds() -> String {
    in_list(&WindowKind::ALL.map(WindowKind::key))
}

pub fn file_kinds() -> String {
    in_list(&FileKind::TAGS)
}

pub fn harness_kinds() -> String {
    in_list(&Harness::TAGS)
}

/// The parts of a request's cost (`core::pricing::CostParts`), stored next to `cost` so the
/// efficiency split is a `SUM` over the same rows as the total. Columns added after a cache
/// version shipped are added in place by `migrate` (no rebuild: transcripts may be gone).
pub const COST_PARTS: &[&str] = &[
    "cost_input",
    "cost_cache_write",
    "cost_cache_read",
    "cost_output",
    "cost_web",
    "cache_saved",
];

fn cost_part_column(c: &str) -> String {
    format!("{c} REAL CHECK ({c} IS NULL OR {c} >= 0)")
}

/// `cost = NULL, priced = NULL, cost_input = NULL, …`: a request waiting for its price.
pub fn unpriced_sql() -> String {
    let mut s = String::from("cost = NULL, priced = NULL");
    for c in COST_PARTS {
        s.push_str(&format!(", {c} = NULL"));
    }
    s
}

/// Every table that is not a fact: raw and dimension tables (all `IF NOT EXISTS`).
pub fn cache_sql() -> String {
    format!("{}{}", raw_sql(), dimension_sql())
}

pub fn raw_sql() -> String {
    let file_kinds = file_kinds();
    let cost_parts: String = COST_PARTS
        .iter()
        .map(|c| format!("  {},\n", cost_part_column(c)))
        .collect();
    format!(
        r#"
CREATE TABLE IF NOT EXISTS files (
  path TEXT PRIMARY KEY,
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ({file_kinds})),
  ino INTEGER NOT NULL DEFAULT 0 CHECK (ino >= 0),
  size INTEGER NOT NULL DEFAULT 0 CHECK (size >= 0),
  mtime INTEGER NOT NULL DEFAULT 0,
  offset INTEGER NOT NULL DEFAULT 0 CHECK (offset >= 0),
  last_ts INTEGER NOT NULL DEFAULT 0,
  account_uuid TEXT
);

CREATE TABLE IF NOT EXISTS requests (
  msg_id TEXT PRIMARY KEY,
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  account_uuid TEXT,
  ts INTEGER NOT NULL,
  model TEXT NOT NULL,
  requested_model TEXT,
  effort TEXT,
  fast INTEGER NOT NULL DEFAULT 0 CHECK (fast IN (0, 1)),
  cwd TEXT,
  input INTEGER NOT NULL DEFAULT 0 CHECK (input >= 0),
  cw5 INTEGER NOT NULL DEFAULT 0 CHECK (cw5 >= 0),
  cw1h INTEGER NOT NULL DEFAULT 0 CHECK (cw1h >= 0),
  cread INTEGER NOT NULL DEFAULT 0 CHECK (cread >= 0),
  output INTEGER NOT NULL DEFAULT 0 CHECK (output >= 0),
  thinking INTEGER NOT NULL DEFAULT 0 CHECK (thinking >= 0),
  web INTEGER NOT NULL DEFAULT 0 CHECK (web >= 0),
  tools INTEGER NOT NULL DEFAULT 0 CHECK (tools >= 0),
  -- USD at the prices of `meta.price_fp`; NULL while not priced, or when no price is known.
  cost REAL CHECK (cost IS NULL OR cost >= 0),
  -- NULL: not priced yet; 0: no price is known for the model; 1: priced.
  priced INTEGER CHECK (priced IN (0, 1)),
  -- `cost` by kind of token (`core::pricing::cost_parts`), set and cleared together with it.
{cost_parts}
  CHECK (CASE WHEN priced = 1 THEN cost IS NOT NULL ELSE cost IS NULL END)
);
CREATE INDEX IF NOT EXISTS requests_ts ON requests(ts);
CREATE INDEX IF NOT EXISTS requests_agent ON requests(seat, session_id, agent_id, ts);
CREATE INDEX IF NOT EXISTS requests_pending ON requests(msg_id) WHERE priced IS NULL;

CREATE TABLE IF NOT EXISTS tool_calls (
  id TEXT PRIMARY KEY,
  msg_id TEXT,
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  ts INTEGER NOT NULL,
  name TEXT NOT NULL,
  summary TEXT,
  target TEXT,
  done_ts INTEGER,
  is_error INTEGER NOT NULL DEFAULT 0 CHECK (is_error IN (0, 1))
);
CREATE INDEX IF NOT EXISTS tool_calls_ts ON tool_calls(ts);
CREATE INDEX IF NOT EXISTS tool_calls_agent ON tool_calls(seat, session_id, agent_id, ts);

-- The account of a session over time, from its `session-owner` records.
CREATE TABLE IF NOT EXISTS owners (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  ts INTEGER NOT NULL,
  account_uuid TEXT NOT NULL,
  PRIMARY KEY (seat, session_id, ts, account_uuid)
);

-- Rows with natural keys: reading the same bytes twice (a replaced file, two processes) adds
-- nothing.
CREATE TABLE IF NOT EXISTS prompts (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  ts INTEGER NOT NULL,
  chars INTEGER NOT NULL CHECK (chars >= 0),
  -- The record's own id (Claude Code's `uuid`): a fork or a moved config dir repeats it.
  record_id TEXT,
  PRIMARY KEY (seat, session_id, ts)
);
CREATE INDEX IF NOT EXISTS prompts_ts ON prompts(ts);

CREATE TABLE IF NOT EXISTS api_errors (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  ts INTEGER NOT NULL,
  kind TEXT NOT NULL,
  text TEXT,
  record_id TEXT,
  PRIMARY KEY (seat, session_id, agent_id, ts, kind)
);
CREATE INDEX IF NOT EXISTS api_errors_ts ON api_errors(ts);

-- Compactions of a session (main transcript); `sessions.compactions` is their count. Rows with
-- a negative `ts` are counts kept from before the times were stored.
CREATE TABLE IF NOT EXISTS compact_events (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  ts INTEGER NOT NULL,
  PRIMARY KEY (seat, session_id, ts)
);

-- Record types per transcript file (for `doctor`): a file read again from the start forgets its
-- counts first; an empty path holds counts kept from before files were told apart.
CREATE TABLE IF NOT EXISTS record_types (
  path TEXT NOT NULL,
  type TEXT NOT NULL,
  count INTEGER NOT NULL CHECK (count >= 0),
  PRIMARY KEY (path, type)
);

-- Transcripts the last refresh that tried could not read (left for the next one: their offset
-- did not move). A successful read removes the row. Shown by `doctor` and in ATTENTION.
CREATE TABLE IF NOT EXISTS unreadable_files (
  path TEXT PRIMARY KEY,
  error TEXT NOT NULL,
  since INTEGER NOT NULL
);
"#
    )
}

pub fn dimension_sql() -> String {
    let file_kinds = file_kinds();
    format!(
        r#"
CREATE TABLE IF NOT EXISTS sessions (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  title TEXT,
  ai_title TEXT,
  cwd TEXT,
  first_ts INTEGER,
  last_ts INTEGER,
  compactions INTEGER NOT NULL DEFAULT 0 CHECK (compactions >= 0),
  PRIMARY KEY (seat, session_id)
);

CREATE TABLE IF NOT EXISTS agents (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ({file_kinds})),
  workflow_run TEXT,
  agent_type TEXT,
  description TEXT,
  tool_use_id TEXT,
  parent_agent_id TEXT,
  depth INTEGER CHECK (depth IS NULL OR depth >= 0),
  requested_model TEXT,
  effort TEXT,
  finished INTEGER NOT NULL DEFAULT 0 CHECK (finished IN (0, 1)),
  first_ts INTEGER,
  last_ts INTEGER,
  PRIMARY KEY (seat, session_id, agent_id)
);

CREATE TABLE IF NOT EXISTS projects (
  cwd TEXT PRIMARY KEY,
  key TEXT NOT NULL,
  name TEXT NOT NULL,
  worktree TEXT
);

-- The git branch an agent worked on, from `from_ts` (epoch ms) until its next row: one row per
-- change of the transcripts' `gitBranch`, so it outlives the raw rows and labels the rollups.
CREATE TABLE IF NOT EXISTS branches (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  from_ts INTEGER NOT NULL,
  branch TEXT NOT NULL CHECK (branch != ''),
  PRIMARY KEY (seat, session_id, agent_id, from_ts)
);

CREATE TABLE IF NOT EXISTS cost_states (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  total_usd REAL NOT NULL CHECK (total_usd >= 0),
  models TEXT NOT NULL,
  PRIMARY KEY (seat, session_id)
);

"#
    )
}

pub fn facts_sql() -> String {
    let window_kinds = window_kinds();
    let harnesses = harness_kinds();
    format!(
        r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Which account each seat (config dir) had logged in, and since when: a change is dated when
-- the vault switched the seat, else when swarmy first sees the seat's `.claude.json` name
-- another account, never earlier. It fills in for requests whose session has no
-- `session-owner` record, and decides for sessions that began before the change (their owner
-- records go stale: `SEAT_ACCOUNT_AT`).
CREATE TABLE IF NOT EXISTS seat_accounts (
  seat TEXT NOT NULL,
  ts INTEGER NOT NULL,
  account_uuid TEXT NOT NULL,
  PRIMARY KEY (seat, ts)
);

CREATE TABLE IF NOT EXISTS accounts (
  account_uuid TEXT PRIMARY KEY,
  email TEXT,
  display_name TEXT,
  org_name TEXT,
  org_uuid TEXT,
  plan TEXT,
  tier TEXT,
  seat TEXT,
  seen_at INTEGER
);

CREATE TABLE IF NOT EXISTS readings (
  account_uuid TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ({window_kinds})),
  fetched_at INTEGER NOT NULL,
  utilization REAL NOT NULL CHECK (utilization >= 0),
  resets_at INTEGER NOT NULL,
  PRIMARY KEY (account_uuid, kind, fetched_at)
);

-- Each version of a model's price in Claude Code's cache, from the day it was first seen; a
-- request is priced with the version in force on its day.
CREATE TABLE IF NOT EXISTS observed_prices (
  model TEXT NOT NULL,
  seen_at INTEGER NOT NULL,
  seen_on TEXT NOT NULL CHECK (date(seen_on) IS seen_on),
  seat TEXT NOT NULL,
  rates TEXT NOT NULL CHECK (json_valid(rates)),
  PRIMARY KEY (model, seen_at)
);

-- What each seat id (`core::ids::SeatId`, from the config dir) was last called, for display: a
-- label changes nothing else. `dir` is NULL for a label an older swarmy stored in place of an id,
-- until a seat with that label adopts its rows (`index::seats`).
CREATE TABLE IF NOT EXISTS seats (
  seat TEXT PRIMARY KEY,
  label TEXT NOT NULL,
  dir TEXT,
  harness TEXT NOT NULL DEFAULT 'claude' CHECK (harness IN ({harnesses})),
  seen_at INTEGER NOT NULL
);
"#
    )
}

/// Indexes added after a cache version shipped; created on every open, cheaply.
pub const EXTRA_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS requests_account_ts ON requests(account_uuid, ts);
CREATE INDEX IF NOT EXISTS requests_no_account ON requests(seat, session_id) WHERE account_uuid IS NULL;
CREATE INDEX IF NOT EXISTS files_mtime ON files(mtime);
CREATE INDEX IF NOT EXISTS tool_calls_msg ON tool_calls(msg_id);
CREATE INDEX IF NOT EXISTS agents_last_ts ON agents(last_ts);
CREATE INDEX IF NOT EXISTS agents_session ON agents(session_id);
CREATE UNIQUE INDEX IF NOT EXISTS prompts_record ON prompts(record_id) WHERE record_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS api_errors_record ON api_errors(record_id) WHERE record_id IS NOT NULL;
"#;

pub(super) fn meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()
}

pub(super) fn set_meta(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
        .exists([name])
}

/// Brings a database to the current schema in one transaction: migrates the facts, rebuilds the
/// cache when its version changed. Returns whether the cache was rebuilt.
pub fn migrate(conn: &Connection) -> anyhow::Result<bool> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    // Immediate: two processes opening an old index migrate it one after the other.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    migrate_facts(&tx)?;
    migrate_archive(&tx)?;
    let version = meta(&tx, "schema_version")?;
    // A newer swarmy's cache is refused, like its facts and archive: rebuilding it would cost a
    // full re-read, and the newer binary would rebuild it back (review B-M11).
    if let Some(v) = version.as_deref().and_then(|v| v.parse::<u32>().ok())
        && v > CACHE_VERSION.parse::<u32>()?
    {
        return Err(crate::errors::IndexError::NewerVersion {
            what: "cache",
            version: v,
        }
        .into());
    }
    let rebuild = version.as_deref() != Some(CACHE_VERSION);
    let mut stashed = false;
    if rebuild {
        // Facts and dimensions are never dropped with the raw tables, whatever the lists say.
        debug_assert!(RAW_TABLES.iter().all(|t| !FACT_TABLES.contains(t)
            && !DIMENSION_TABLES.contains(t)
            && !ARCHIVE_TABLES.contains(t)));
        // Dimension tables of a database older than their current shape go too (rebuilt from
        // the transcripts, as before the classes existed).
        let old_dimensions = !matches!(
            version.as_deref().map(str::parse::<u32>),
            Some(Ok(v)) if v >= DIMENSIONS_SINCE
        );
        // What reading the transcripts again would not give back stays (`stash_unrereadable`).
        stashed = !old_dimensions && stash_unrereadable(&tx)?;
        let dropped = RAW_TABLES
            .iter()
            .chain(DIMENSION_TABLES.iter().filter(|_| old_dimensions));
        for t in dropped {
            tx.execute_batch(&format!("DROP TABLE IF EXISTS {t};"))?;
        }
        if old_dimensions {
            tx.execute("DELETE FROM meta WHERE key = 'projects_rev'", [])?;
        }
        for k in CACHE_META {
            tx.execute("DELETE FROM meta WHERE key = ?1", [k])?;
        }
        set_meta(&tx, "schema_version", CACHE_VERSION)?;
    }
    // Tables added without a version bump (all `IF NOT EXISTS`), then the steps that reshape
    // cache tables in place, then extra indexes.
    tx.execute_batch(&cache_sql())?;
    if stashed {
        restore_unrereadable(&tx)?;
    }
    let done: usize = if rebuild {
        CACHE_STEPS.len()
    } else {
        meta(&tx, "cache_step")?.map_or(Ok(0), |v| v.parse())?
    };
    if done > CACHE_STEPS.len() {
        // Steps this binary does not know ran already: writing a lower count would make the
        // newer one run its own again.
        return Err(crate::errors::IndexError::NewerVersion {
            what: "cache step",
            version: u32::try_from(done)?,
        }
        .into());
    }
    for step in CACHE_STEPS.iter().skip(done) {
        step(&tx)?;
        // Tables a step recreated get their indexes back.
        tx.execute_batch(&cache_sql())?;
    }
    set_meta(&tx, "cache_step", &CACHE_STEPS.len().to_string())?;
    tx.execute_batch(EXTRA_INDEXES)?;
    tx.commit()?;
    Ok(rebuild)
}

/// Raw tables whose rows a rebuild may not get back from the transcripts: (table, whether its rows
/// belong to an agent (else to a session's main transcript), whether the compactor rolls it up
/// by `ts`).
const KEPT_ON_REBUILD: &[(&str, bool, bool)] = &[
    ("requests", true, true),
    ("tool_calls", true, true),
    ("api_errors", true, true),
    ("prompts", false, true),
    ("owners", false, false),
    ("compact_events", false, false),
];

/// Before a rebuild drops the raw tables, copies into temp tables the rows that reading the
/// transcripts again would not give back (design §3.1 assumed none): rows of an agent whose
/// transcript is gone (a seat dir removed or moved, a retention shorter than the raw window),
/// and rows older than `rolled_until` that are still raw (old records found after a
/// compaction: the re-read skips them as rolled up). `restore_unrereadable` puts them back into
/// the new tables. Returns whether anything was stashed.
fn stash_unrereadable(tx: &Connection) -> anyhow::Result<bool> {
    let need = ["path", "seat", "session_id", "agent_id"];
    let files = columns(tx, "files")?;
    if !need.iter().all(|c| files.iter().any(|f| f == c)) {
        return Ok(false);
    }
    let rolled_until: i64 = match meta(tx, "rolled_until")? {
        Some(v) => v.parse()?,
        None => 0,
    };
    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.readable;
         CREATE TEMP TABLE readable (seat TEXT, session_id TEXT, agent_id TEXT,
           PRIMARY KEY (seat, session_id, agent_id));",
    )?;
    {
        let mut all = tx.prepare("SELECT path, seat, session_id, agent_id FROM files")?;
        let rows: Vec<(String, String, String, String)> = all
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        let mut add = tx.prepare("INSERT OR IGNORE INTO temp.readable VALUES (?1, ?2, ?3)")?;
        for (path, seat, session, agent) in rows {
            if std::path::Path::new(&path).exists() {
                add.execute(params![seat, session, agent])?;
            }
        }
    }
    for (t, per_agent, rolled) in KEPT_ON_REBUILD {
        if !table_exists(tx, t)? {
            continue;
        }
        let agent = if *per_agent { "x.agent_id" } else { "''" };
        let old = if *rolled {
            format!("x.ts < {rolled_until} OR ")
        } else {
            String::new()
        };
        tx.execute_batch(&format!(
            "DROP TABLE IF EXISTS temp.keep_{t};
             CREATE TEMP TABLE keep_{t} AS SELECT * FROM {t} x WHERE {old}NOT EXISTS (
               SELECT 1 FROM temp.readable r WHERE r.seat = x.seat AND r.session_id = x.session_id
                 AND r.agent_id = {agent});"
        ))?;
    }
    tx.execute_batch("DROP TABLE temp.readable;")?;
    Ok(true)
}

/// Puts the rows `stash_unrereadable` kept into the rebuilt tables (the columns both have; a
/// row the new rules refuse is left out) and drops the temp tables.
fn restore_unrereadable(tx: &Connection) -> anyhow::Result<()> {
    for (t, ..) in KEPT_ON_REBUILD {
        let kept: Vec<String> = tx
            .prepare("SELECT name FROM pragma_table_info(?1, 'temp')")?
            .query_map([format!("keep_{t}")], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if kept.is_empty() {
            continue;
        }
        let now = columns(tx, t)?;
        let cols: Vec<&String> = now.iter().filter(|c| kept.contains(c)).collect();
        let cols = cols
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let stashed: i64 =
            tx.query_row(&format!("SELECT COUNT(*) FROM temp.keep_{t}"), [], |r| {
                r.get(0)
            })?;
        let put = tx.execute(
            &format!("INSERT OR IGNORE INTO main.{t}({cols}) SELECT {cols} FROM temp.keep_{t}"),
            [],
        )?;
        tx.execute_batch(&format!("DROP TABLE temp.keep_{t};"))?;
        // The tables were empty: every row left out is one the new rules refused. Nothing read
        // is dropped silently (review B-M7): it is counted, and `doctor` says so.
        let refused = stashed - i64::try_from(put)?;
        if refused > 0 {
            let before: i64 = meta(tx, "rebuild_refused")?.map_or(Ok(0), |v| v.parse())?;
            set_meta(tx, "rebuild_refused", &(before + refused).to_string())?;
        }
    }
    Ok(())
}

/// Steps that bring the cache tables of an older database to the shape of `cache_sql` in place,
/// keeping their rows: a rebuild would lose requests whose transcripts Claude Code already
/// deleted. Each runs once (`meta.cache_step`), on databases older than the step; rebuilt raw
/// tables already have the latest shape. Steps only ever get appended. A step that reshapes a
/// dimension table must also run after a rebuild (dimension tables are kept by it).
type CacheStep = fn(&Connection) -> anyhow::Result<()>;
const CACHE_STEPS: &[CacheStep] = &[add_cost_parts, add_natural_keys, add_record_ids];

fn columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT name FROM pragma_table_info(?1)")?
        .query_map([table], |r| r.get(0))?
        .collect()
}

/// Recreates `table` from `cache_sql` and copies the rows `copy` selects from the old one
/// (`{old}` in it names the old table); rows the new keys refuse as duplicates are left out.
fn recreate(conn: &Connection, table: &str, copy: &str) -> anyhow::Result<()> {
    let old = format!("{table}_old");
    conn.execute_batch(&format!("ALTER TABLE {table} RENAME TO {old};"))?;
    // The old table keeps its index names; drop them so `cache_sql` creates them anew.
    let indexes: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL")?
        .query_map([&old], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for i in indexes {
        conn.execute_batch(&format!("DROP INDEX {i};"))?;
    }
    conn.execute_batch(&cache_sql())?;
    conn.execute_batch(&copy.replace("{old}", &old))?;
    conn.execute_batch(&format!("DROP TABLE {old};"))?;
    Ok(())
}

/// Step 1: the parts of each request's cost, next to `cost` (computed again with them).
fn add_cost_parts(conn: &Connection) -> anyhow::Result<()> {
    let have = columns(conn, "requests")?;
    let missing: Vec<&&str> = COST_PARTS
        .iter()
        .filter(|c| !have.iter().any(|h| h == **c))
        .collect();
    for c in &missing {
        conn.execute_batch(&format!(
            "ALTER TABLE requests ADD COLUMN {};",
            cost_part_column(c)
        ))?;
    }
    if !missing.is_empty() {
        conn.execute("DELETE FROM meta WHERE key = 'price_fp'", [])?;
    }
    Ok(())
}

/// Step 3: each prompt and API error keeps its record's own id, which a fork of the session or a
/// moved config dir repeats (review B-M8: keyed by seat and session, they counted again). Rows
/// read before have none, and keep their old key.
fn add_record_ids(conn: &Connection) -> anyhow::Result<()> {
    for t in ["prompts", "api_errors"] {
        if !columns(conn, t)?.iter().any(|c| c == "record_id") {
            conn.execute_batch(&format!("ALTER TABLE {t} ADD COLUMN record_id TEXT;"))?;
        }
    }
    Ok(())
}

/// Step 2: natural keys on prompts and API errors, compactions as events, record types per file.
fn add_natural_keys(conn: &Connection) -> anyhow::Result<()> {
    if !columns(conn, "record_types")?.iter().any(|c| c == "path") {
        recreate(
            conn,
            "record_types",
            "INSERT INTO record_types(path, type, count) SELECT '', type, count FROM {old};",
        )?;
    }
    recreate(
        conn,
        "prompts",
        "INSERT OR IGNORE INTO prompts(seat, session_id, ts, chars)
         SELECT seat, session_id, ts, chars FROM {old};",
    )?;
    recreate(
        conn,
        "api_errors",
        "INSERT OR IGNORE INTO api_errors(seat, session_id, agent_id, ts, kind, text)
         SELECT seat, session_id, agent_id, ts, kind, text FROM {old};",
    )?;
    // The counts so far, as events without a time (ts −1, −2, …): a session whose transcript is
    // read again from the start replaces them with the real ones.
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < (SELECT MAX(compactions) FROM sessions))
         INSERT OR IGNORE INTO compact_events(seat, session_id, ts)
         SELECT s.seat, s.session_id, -n.i FROM sessions s JOIN n ON n.i <= s.compactions;",
    )?;
    Ok(())
}

fn migrate_archive(conn: &Connection) -> anyhow::Result<()> {
    let version: u32 = match meta(conn, "archive_version")? {
        Some(v) => v.parse()?,
        None => 0,
    };
    if version > ARCHIVE_VERSION {
        return Err(crate::errors::IndexError::NewerVersion {
            what: "archive",
            version,
        }
        .into());
    }
    // 0 → 1: the rollup tables, empty; nothing is rolled up yet.
    let had_rollups = table_exists(conn, "usage_rollup")?;
    conn.execute_batch(&super::archive::archive_sql())?;
    if version < 1 {
        set_meta(conn, "rolled_until", "0")?;
    }
    // 1 → 2: how many requests of a rollup row had no price (and how many had no tokens), as
    // counts instead of one flag. Rows rolled up before keep what the flag said: all of the
    // row's requests unpriced, or none.
    if version < 2 && had_rollups {
        let have = columns(conn, "usage_rollup")?;
        for (col, fill) in [
            (
                "n_unpriced",
                "CASE WHEN priced = 0 THEN requests ELSE 0 END",
            ),
            (
                "n_empty",
                "CASE WHEN input + cw5 + cw1h + cread + output + web = 0 THEN requests ELSE 0 END",
            ),
        ] {
            if have.iter().any(|h| h == col) {
                continue;
            }
            let Some(sum) = super::archive::SUMS.iter().find(|s| s.col == col) else {
                anyhow::bail!("no rollup column {col}");
            };
            conn.execute_batch(&format!(
                "ALTER TABLE usage_rollup ADD COLUMN {col} {};
                 UPDATE usage_rollup SET {col} = {fill};",
                sum.ddl
            ))?;
        }
    }
    set_meta(conn, "archive_version", &ARCHIVE_VERSION.to_string())?;
    Ok(())
}

fn migrate_facts(conn: &Connection) -> anyhow::Result<()> {
    let version: u32 = match meta(conn, "facts_version")? {
        Some(v) => v.parse()?,
        None => 0,
    };
    if version > FACTS_VERSION {
        return Err(crate::errors::IndexError::NewerVersion {
            what: "facts",
            version,
        }
        .into());
    }
    // 0 → 1: the fact tables of cache versions ≤ 5 had no constraints. Copy them into the new
    // tables, leaving out rows the new rules refuse.
    let legacy = ["seat_accounts", "accounts", "readings"];
    let mut renamed = Vec::new();
    if version < 1 {
        for t in legacy {
            if table_exists(conn, t)? {
                conn.execute_batch(&format!("ALTER TABLE {t} RENAME TO {t}_v0;"))?;
                renamed.push(t);
            }
        }
    }
    conn.execute_batch(&facts_sql())?;
    for t in renamed {
        let copy = match t {
            "seat_accounts" => "INSERT INTO seat_accounts(seat, ts, account_uuid)
                 SELECT seat, ts, account_uuid FROM seat_accounts_v0"
                .to_string(),
            "accounts" => "INSERT INTO accounts(account_uuid, email, display_name, org_name, plan, tier, seat, seen_at)
                 SELECT account_uuid, email, display_name, org_name, plan, tier, seat, seen_at FROM accounts_v0"
                .to_string(),
            "readings" => format!(
                "INSERT INTO readings(account_uuid, kind, fetched_at, utilization, resets_at)
                 SELECT account_uuid, kind, fetched_at, utilization, resets_at FROM readings_v0
                 WHERE kind IN ({}) AND utilization >= 0",
                window_kinds()
            ),
            other => anyhow::bail!("no migration for table {other}"),
        };
        conn.execute_batch(&copy)?;
        conn.execute_batch(&format!("DROP TABLE {t}_v0;"))?;
    }
    // 1 → 2: seats are keyed by id; the labels stored so far wait in `seats` for their seat.
    if version < 2 {
        super::seats::list_legacy_labels(conn)?;
    }
    // 2 → 3: accounts carry `org_uuid` alongside the canonical `<account>/<org>` primary key.
    if version < 3 && !columns(conn, "accounts")?.iter().any(|c| c == "org_uuid") {
        conn.execute_batch("ALTER TABLE accounts ADD COLUMN org_uuid TEXT;")?;
    }
    // 3 → 4: seats record which harness owns them (`claude`, `agy`, …); existing seats are claude.
    if version < 4 && !columns(conn, "seats")?.iter().any(|c| c == "harness") {
        conn.execute_batch(&format!(
            "ALTER TABLE seats ADD COLUMN harness TEXT NOT NULL DEFAULT 'claude' CHECK (harness IN ({}));",
            harness_kinds()
        ))?;
    }
    // 4 → 5: the `CHECK` on `seats.harness` lists every harness (Codex came after it was made).
    // SQLite cannot change a CHECK: the table is made anew and its rows copied.
    if version < 5 && !seats_name_every_harness(conn)? {
        conn.execute_batch("ALTER TABLE seats RENAME TO seats_v4;")?;
        conn.execute_batch(&facts_sql())?;
        conn.execute_batch(
            "INSERT INTO seats(seat, label, dir, harness, seen_at)
               SELECT seat, label, dir, harness, seen_at FROM seats_v4;
             DROP TABLE seats_v4;",
        )?;
    }
    set_meta(conn, "facts_version", &FACTS_VERSION.to_string())?;
    Ok(())
}

/// Whether the `CHECK` of `seats.harness` allows every harness of `Harness::TAGS`.
fn seats_name_every_harness(conn: &Connection) -> rusqlite::Result<bool> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'seats'",
        [],
        |r| r.get(0),
    )?;
    Ok(Harness::TAGS
        .iter()
        .all(|t| sql.contains(&format!("'{t}'"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table names in `CREATE TABLE` statements.
    fn created(sql: &str) -> Vec<String> {
        sql.split("CREATE TABLE IF NOT EXISTS ")
            .skip(1)
            .map(|s| s.split([' ', '(']).next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn every_table_is_in_exactly_one_class() {
        let raw = created(&raw_sql());
        let dims = created(&dimension_sql());
        let archive = created(&super::super::archive::archive_sql());
        let facts = created(&facts_sql());
        assert_eq!(
            archive, ARCHIVE_TABLES,
            "archive tables and ARCHIVE_TABLES differ"
        );
        assert_eq!(raw, RAW_TABLES, "raw tables and RAW_TABLES differ");
        assert_eq!(
            dims, DIMENSION_TABLES,
            "dimension tables and DIMENSION_TABLES differ"
        );
        assert_eq!(facts, FACT_TABLES, "fact tables and FACT_TABLES differ");
        let mut all: Vec<&String> = raw
            .iter()
            .chain(&dims)
            .chain(&archive)
            .chain(&facts)
            .collect();
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n, "a table is in two classes");
        // And nothing else is created on open.
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        let mut on_disk: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        on_disk.sort();
        let mut want: Vec<String> = all.into_iter().cloned().collect();
        want.sort();
        assert_eq!(on_disk, want);
    }

    #[test]
    fn check_lists_come_from_the_enums() {
        // A new variant must be added to `ALL` too (this match stops compiling otherwise).
        let n = |k: WindowKind| match k {
            WindowKind::FiveHour
            | WindowKind::SevenDay
            | WindowKind::SevenDayOpus
            | WindowKind::SevenDaySonnet => 4,
        };
        assert_eq!(WindowKind::ALL.len(), n(WindowKind::FiveHour));
        for k in WindowKind::ALL {
            assert_eq!(WindowKind::from_key(k.key()), Some(k));
            assert!(window_kinds().contains(&format!("'{}'", k.key())));
        }
        assert_eq!(
            window_kinds(),
            "'five_hour', 'seven_day', 'seven_day_opus', 'seven_day_sonnet'"
        );
        for k in [
            FileKind::Main,
            FileKind::Subagent,
            FileKind::Workflow("w".into()),
        ] {
            assert!(file_kinds().contains(&format!("'{}'", k.tag())));
        }
    }

    #[test]
    fn the_schema_refuses_what_the_rules_forbid() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        let bad = [
            "INSERT INTO readings VALUES ('a', 'monthly', 1, 10, 2)",
            "INSERT INTO readings VALUES ('a', 'seven_day', 1, -1, 2)",
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model, input) VALUES ('m', 's', 'x', '', 1, 'm', -5)",
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model, fast) VALUES ('m', 's', 'x', '', 1, 'm', 2)",
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model, cost, priced) VALUES ('m', 's', 'x', '', 1, 'm', -1, 1)",
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model, cost, priced) VALUES ('m', 's', 'x', '', 1, 'm', NULL, 1)",
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model, cost) VALUES ('m', 's', 'x', '', 1, 'm', 0.5)",
            "INSERT INTO files(path, seat, session_id, agent_id, kind) VALUES ('p', 's', 'x', '', 'other')",
            "INSERT INTO agents(seat, session_id, agent_id, kind, finished) VALUES ('s', 'x', 'a', 'main', 3)",
            "INSERT INTO observed_prices VALUES ('m', 1, 'yesterday', 's', '{}')",
        ];
        for sql in bad {
            assert!(c.execute(sql, []).is_err(), "accepted: {sql}");
        }
        let good = [
            "INSERT INTO readings VALUES ('a', 'seven_day', 1, 104.5, 2)",
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model) VALUES ('m1', 's', 'x', '', 1, 'm')",
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model, cost, priced) VALUES ('m2', 's', 'x', '', 1, 'm', 0.5, 1)",
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model, priced) VALUES ('m3', 's', 'x', '', 1, 'm', 0)",
        ];
        for sql in good {
            c.execute(sql, []).unwrap();
        }
    }

    #[test]
    fn legacy_fact_tables_are_migrated_not_dropped() {
        // A version-5 database: fact tables without constraints, one reading the new rules refuse.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '5'), ('price_fp', 'old');
             CREATE TABLE seat_accounts (seat TEXT NOT NULL, ts INTEGER NOT NULL, account_uuid TEXT NOT NULL, PRIMARY KEY (seat, ts));
             INSERT INTO seat_accounts VALUES ('work', 100, 'acc-a'), ('work', 200, 'acc-b');
             CREATE TABLE accounts (account_uuid TEXT PRIMARY KEY, email TEXT, display_name TEXT, org_name TEXT, plan TEXT, tier TEXT, seat TEXT, seen_at INTEGER);
             INSERT INTO accounts VALUES ('acc-a', 'a@example.com', NULL, NULL, 'max', 't', 'work', 1);
             CREATE TABLE readings (account_uuid TEXT NOT NULL, kind TEXT NOT NULL, fetched_at INTEGER NOT NULL, utilization REAL NOT NULL, resets_at INTEGER NOT NULL, PRIMARY KEY (account_uuid, kind, fetched_at));
             INSERT INTO readings VALUES ('acc-a', 'seven_day', 1, 40, 9), ('acc-a', 'bogus', 2, 1, 9);
             CREATE TABLE requests (msg_id TEXT PRIMARY KEY, cost REAL NOT NULL DEFAULT -1);
             INSERT INTO requests VALUES ('m', -1);",
        )
        .unwrap();
        assert!(migrate(&c).unwrap(), "the cache is rebuilt");
        let n = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(n("SELECT COUNT(*) FROM seat_accounts"), 2);
        assert_eq!(n("SELECT COUNT(*) FROM accounts"), 1);
        assert_eq!(n("SELECT COUNT(*) FROM readings"), 1);
        assert_eq!(n("SELECT COUNT(*) FROM requests"), 0);
        assert_eq!(n("SELECT COUNT(*) FROM meta WHERE key = 'price_fp'"), 0);
        assert_eq!(
            meta(&c, "facts_version").unwrap().as_deref(),
            Some(FACTS_VERSION.to_string().as_str())
        );
        // And the migrated readings table carries the constraint now.
        assert!(
            c.execute("INSERT INTO readings VALUES ('a', 'x', 1, 1, 1)", [])
                .is_err()
        );
        // A second open changes nothing.
        assert!(!migrate(&c).unwrap());
        assert_eq!(n("SELECT COUNT(*) FROM seat_accounts"), 2);
    }

    #[test]
    fn facts_version_3_adds_harness_column_and_defaults_to_claude() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('facts_version', '3');
             CREATE TABLE seat_accounts (seat TEXT NOT NULL, ts INTEGER NOT NULL, account_uuid TEXT NOT NULL, PRIMARY KEY (seat, ts));
             CREATE TABLE accounts (account_uuid TEXT PRIMARY KEY, email TEXT, display_name TEXT, org_name TEXT, plan TEXT, tier TEXT, seen_at INTEGER);
             CREATE TABLE readings (account_uuid TEXT NOT NULL, kind TEXT NOT NULL, ts INTEGER NOT NULL, used REAL NOT NULL, resets_at INTEGER NOT NULL, PRIMARY KEY (account_uuid, kind, ts));
             CREATE TABLE observed_prices (model TEXT NOT NULL, seen_at INTEGER NOT NULL, since TEXT NOT NULL, seat TEXT NOT NULL, rates TEXT NOT NULL, PRIMARY KEY (model, since));
             CREATE TABLE seats (seat TEXT PRIMARY KEY, label TEXT NOT NULL, dir TEXT NOT NULL, seen_at INTEGER NOT NULL);
             INSERT INTO seats VALUES ('0123456789abcdef', 'work', '/d/work', 100);",
        )
        .unwrap();
        migrate(&c).unwrap();
        assert_eq!(
            meta(&c, "facts_version").unwrap().as_deref(),
            Some(FACTS_VERSION.to_string().as_str())
        );
        let h: String = c
            .query_row(
                "SELECT harness FROM seats WHERE seat = '0123456789abcdef'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(h, "claude");
        // CHECK constraint rejects unknown harness tags.
        assert!(
            c.execute(
                "INSERT INTO seats(seat, label, dir, seen_at, harness) VALUES ('fedcba9876543210', 'x', '/x', 1, 'unknown')",
                [],
            )
            .is_err()
        );
        // Second open is a no-op.
        assert!(!migrate(&c).unwrap());
    }

    #[test]
    fn facts_version_4_lets_seats_be_codex_and_keeps_them() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        // A version 4 index: its CHECK knew claude and agy only.
        c.execute_batch(
            "DROP TABLE seats;
             CREATE TABLE seats (seat TEXT PRIMARY KEY, label TEXT NOT NULL, dir TEXT,
               harness TEXT NOT NULL DEFAULT 'claude' CHECK (harness IN ('claude', 'agy')),
               seen_at INTEGER NOT NULL);
             INSERT INTO seats VALUES ('0123456789abcdef', 'work', '/d/work', 'agy', 100);",
        )
        .unwrap();
        set_meta(&c, "facts_version", "4").unwrap();
        migrate(&c).unwrap();
        let h: String = c
            .query_row(
                "SELECT harness FROM seats WHERE seat = '0123456789abcdef'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(h, "agy");
        c.execute(
            "INSERT INTO seats(seat, label, dir, seen_at, harness) VALUES ('fedcba9876543210', 'codex', '/c', 1, 'codex')",
            [],
        )
        .unwrap();
        assert!(!migrate(&c).unwrap());
    }

    #[test]
    fn a_newer_facts_version_is_refused() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        set_meta(&c, "facts_version", "99").unwrap();
        assert!(migrate(&c).is_err());
    }

    #[test]
    fn an_older_cache_is_reshaped_in_place() {
        // A cache of the current version written before the cost parts, the natural keys and
        // the compaction events existed: its rows stay (their transcripts may be gone).
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '{CACHE_VERSION}'), ('facts_version', '1'), ('price_fp', 'old');",
        ))
        .unwrap();
        c.execute_batch(&cache_sql()).unwrap();
        c.execute_batch(
            "DROP TABLE prompts; DROP TABLE api_errors; DROP TABLE record_types; DROP TABLE compact_events;
             CREATE TABLE prompts (seat TEXT NOT NULL, session_id TEXT NOT NULL, ts INTEGER NOT NULL, chars INTEGER NOT NULL CHECK (chars >= 0));
             CREATE INDEX prompts_ts ON prompts(ts);
             CREATE TABLE api_errors (seat TEXT NOT NULL, session_id TEXT NOT NULL, agent_id TEXT NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT);
             CREATE TABLE record_types (type TEXT PRIMARY KEY, count INTEGER NOT NULL CHECK (count >= 0));
             INSERT INTO prompts VALUES ('w', 's1', 5, 10), ('w', 's1', 5, 10), ('w', 's1', 6, 3);
             INSERT INTO api_errors VALUES ('w', 's1', '', 7, 'rate_limit', 'x'), ('w', 's1', '', 7, 'rate_limit', 'x');
             INSERT INTO record_types VALUES ('assistant', 40), ('user', 30);
             INSERT INTO sessions(seat, session_id, compactions) VALUES ('w', 's1', 3), ('w', 's2', 0);",
        )
        .unwrap();
        for col in COST_PARTS {
            c.execute_batch(&format!("ALTER TABLE requests DROP COLUMN {col};"))
                .unwrap();
        }
        c.execute_batch(
            "INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model, cost, priced)
               VALUES ('m', 's', 'x', '', 1, 'm', 0.5, 1);",
        )
        .unwrap();
        assert!(!migrate(&c).unwrap(), "no rebuild");
        let n = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(n("SELECT COUNT(*) FROM requests"), 1);
        assert_eq!(n("SELECT COUNT(*) FROM meta WHERE key = 'price_fp'"), 0);
        assert_eq!(n("SELECT COUNT(cost_input) FROM requests"), 0);
        assert!(
            c.execute("UPDATE requests SET cost_output = -1", [])
                .is_err()
        );
        // Duplicates collapse under the new keys, and the keys hold from now on.
        assert_eq!(n("SELECT COUNT(*) FROM prompts"), 2);
        assert_eq!(n("SELECT COUNT(*) FROM api_errors"), 1);
        assert!(
            c.execute("INSERT INTO prompts VALUES ('w', 's1', 6, 3)", [])
                .is_err()
        );
        assert_eq!(
            n("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'prompts_ts'"),
            1
        );
        // Legacy counts are kept: record types under an empty path, compactions as events.
        assert_eq!(n("SELECT SUM(count) FROM record_types WHERE path = ''"), 70);
        assert_eq!(
            n("SELECT COUNT(*) FROM compact_events WHERE session_id = 's1' AND ts < 0"),
            3
        );
        assert_eq!(
            n("SELECT COUNT(*) FROM compact_events WHERE session_id = 's2'"),
            0
        );
        // A second open runs no step again.
        assert!(!migrate(&c).unwrap());
        assert_eq!(n("SELECT COUNT(*) FROM compact_events"), 3);
        assert_eq!(n("SELECT COUNT(*) FROM prompts"), 2);
    }

    #[test]
    fn a_cache_version_bump_keeps_every_fact_table() {
        // Review H1: a schema bump must never drop what nothing else records. Every fact table
        // gets a row, the cache version changes, the rows are all still there.
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO seat_accounts VALUES ('work', 100, 'acc-a');
             INSERT INTO accounts(account_uuid, email) VALUES ('acc-a', 'a@example.com');
             INSERT INTO readings VALUES ('acc-a', 'seven_day', 1, 40, 9);
             INSERT INTO observed_prices VALUES ('m', 1, '2026-11-01', 'work', '{}');
             INSERT INTO seats(seat, label, dir, seen_at, harness) VALUES ('0123456789abcdef', 'work', '/d', 1, 'claude');
             INSERT INTO requests(msg_id, seat, session_id, agent_id, ts, model) VALUES ('m', 's', 'x', '', 1, 'm');
             UPDATE meta SET value = '0' WHERE key = 'schema_version';",
        )
        .unwrap();
        let n = |t: &str| {
            c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        let before: Vec<i64> = FACT_TABLES.iter().map(|t| n(t)).collect();
        assert!(migrate(&c).unwrap(), "the cache is rebuilt");
        assert_eq!(n("requests"), 0);
        for (t, b) in FACT_TABLES.iter().zip(before) {
            if *t != "meta" {
                assert_eq!(n(t), b, "{t} kept its rows");
                assert!(b > 0, "{t} had a row");
            }
        }
    }

    #[test]
    fn the_archive_refuses_bad_rows_and_outlives_every_cache_bump() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        assert_eq!(meta(&c, "rolled_until").unwrap().as_deref(), Some("0"));
        let row = |bucket: i64, first: i64, priced: &str, cost: &str| {
            format!(
                "INSERT INTO usage_rollup(bucket, seat, session_id, agent_id, model, fast, long_ctx, requests,
                   input, cw5, cw1h, cread, output, thinking, web, tools, cost, priced, first_ts, last_ts)
                 VALUES ({bucket}, 's', 'x', '', 'm', 0, 0, 3, 1, 0, 0, 0, 1, 0, 0, 0, {cost}, {priced}, {first}, {first})"
            )
        };
        let b = 1_000;
        let at = b * 900_000 + 5;
        for bad in [
            row(b, at, "NULL", "NULL"),       // pending rows are never rolled up
            row(b, at, "1", "NULL"),          // priced without a cost
            row(b, at, "0", "0.5"),           // a cost without a price
            row(b, at - 900_000, "1", "0.5"), // first_ts outside its bucket
            row(b, at, "1", "-1"),            // a negative cost
            "INSERT INTO tool_rollup VALUES (1, 'Bash', '', 2, 3, 0)".to_string(), // more errors than calls
            "INSERT INTO prompt_rollup VALUES (1, 0, 0)".to_string(),
            "INSERT INTO storage_samples VALUES ('today', 1, 1, 1, 1)".to_string(),
        ] {
            assert!(c.execute(&bad, []).is_err(), "accepted: {bad}");
        }
        c.execute(&row(b, at, "1", "0.5"), []).unwrap();
        c.execute(&row(b + 1, at + 900_000, "0", "NULL"), [])
            .unwrap();
        // A cache bump (and even one from before the dimensions' shape) keeps it.
        set_meta(&c, "schema_version", "1").unwrap();
        assert!(migrate(&c).unwrap());
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM usage_rollup", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        // A newer archive is refused.
        set_meta(&c, "archive_version", "99").unwrap();
        assert!(migrate(&c).is_err());
    }

    /// Review B-M8: an index from before step 3 gets the record ids in place, its rows kept.
    #[test]
    fn record_ids_are_added_in_place() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "DROP INDEX prompts_record; DROP INDEX api_errors_record;
             ALTER TABLE prompts DROP COLUMN record_id; ALTER TABLE api_errors DROP COLUMN record_id;
             INSERT INTO prompts(seat, session_id, ts, chars) VALUES ('work', 's1', 1, 10);",
        )
        .unwrap();
        set_meta(&c, "cache_step", "2").unwrap();
        assert!(!migrate(&c).unwrap(), "no rebuild");
        for t in ["prompts", "api_errors"] {
            assert!(
                columns(&c, t).unwrap().iter().any(|x| x == "record_id"),
                "{t}"
            );
        }
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM prompts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Review B-M11: an older swarmy refuses a newer cache (it used to drop and re-read it, and
    /// to rewind the step counter so the newer one ran its steps twice).
    #[test]
    fn a_newer_cache_is_refused_not_rebuilt() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        let newer = (CACHE_VERSION.parse::<u32>().unwrap() + 1).to_string();
        set_meta(&c, "schema_version", &newer).unwrap();
        let e = migrate(&c).unwrap_err();
        assert!(e.to_string().contains("newer swarmy"), "{e}");
        set_meta(&c, "schema_version", CACHE_VERSION).unwrap();
        set_meta(&c, "cache_step", &(CACHE_STEPS.len() + 1).to_string()).unwrap();
        assert!(migrate(&c).is_err());
        assert_eq!(
            meta(&c, "cache_step").unwrap(),
            Some((CACHE_STEPS.len() + 1).to_string()),
            "nothing rewound"
        );
    }

    #[test]
    fn archive_version_1_counts_its_unpriced_requests() {
        // Rollups written with the `priced` flag only: the counts are added, filled from it.
        let c = Connection::open_in_memory().unwrap();
        let v1: String = super::super::archive::archive_sql()
            .lines()
            .filter(|l| !l.contains("n_unpriced") && !l.contains("n_empty"))
            .collect::<Vec<_>>()
            .join("\n");
        c.execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('archive_version', '1'), ('rolled_until', '0');
             {v1}"
        ))
        .unwrap();
        let row = |bucket: i64, tokens: i64, priced: i64, cost: &str| {
            format!(
                "INSERT INTO usage_rollup(bucket, seat, session_id, agent_id, model, fast, long_ctx, requests,
                   input, cw5, cw1h, cread, output, thinking, web, tools, cost, priced, first_ts, last_ts)
                 VALUES ({bucket}, 's', 'x', '', 'm', 0, 0, 3, {tokens}, 0, 0, 0, 0, 0, 0, 0, {cost}, {priced},
                   {bucket} * 900000, {bucket} * 900000)"
            )
        };
        for sql in [row(1, 5, 0, "NULL"), row(2, 5, 1, "0.5"), row(3, 0, 1, "0")] {
            c.execute(&sql, []).unwrap();
        }
        migrate(&c).unwrap();
        let counts: Vec<(i64, i64)> = c
            .prepare("SELECT n_unpriced, n_empty FROM usage_rollup ORDER BY bucket")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(counts, [(3, 0), (0, 0), (0, 3)]);
        assert_eq!(meta(&c, "archive_version").unwrap().as_deref(), Some("2"));
        assert!(!migrate(&c).unwrap());
    }

    #[test]
    fn a_cache_version_bump_keeps_every_dimension_table() {
        // Agent types, session titles, projects and Claude Code's totals outlive the transcripts
        // (deleted after 30 days): a bump drops only the raw rows.
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        let fill = |c: &Connection, version: &str| {
            c.execute_batch(&format!(
                "INSERT OR IGNORE INTO sessions(seat, session_id, title) VALUES ('work', 's1', 'old work');
                 INSERT OR IGNORE INTO agents(seat, session_id, agent_id, kind, agent_type) VALUES ('work', 's1', 'a1', 'subagent', 'reviewer');
                 INSERT OR IGNORE INTO projects VALUES ('/w/shop', 'shop', 'shop', NULL);
                 INSERT OR IGNORE INTO branches VALUES ('work', 's1', 'a1', 1, 'main');
                 INSERT OR IGNORE INTO cost_states VALUES ('work', 's1', 1.5, '{{}}');
                 INSERT OR REPLACE INTO meta VALUES ('projects_rev', 'r'), ('price_fp', 'p'), ('schema_version', '{version}');
                 INSERT OR IGNORE INTO prompts(seat, session_id, ts, chars) VALUES ('work', 's1', 1, 10);"
            ))
            .unwrap();
        };
        let n = |c: &Connection, t: &str| -> i64 {
            c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        };
        let dims = |c: &Connection| DIMENSION_TABLES.iter().map(|t| n(c, t)).collect::<Vec<_>>();
        // Another (older) cache version of today's dimension shape: raw rows go, dimensions stay.
        let other = (CACHE_VERSION.parse::<u32>().unwrap() - 1).to_string();
        fill(&c, &other);
        assert!(migrate(&c).unwrap(), "the raw tables are rebuilt");
        assert_eq!(
            n(&c, "prompts"),
            1,
            "no transcript gives the prompt back: it stays"
        );
        assert_eq!(
            dims(&c),
            vec![1, 1, 1, 1, 1],
            "every dimension table kept its rows"
        );
        assert_eq!(
            c.query_row("SELECT agent_type FROM agents", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "reviewer"
        );
        assert_eq!(meta(&c, "projects_rev").unwrap().as_deref(), Some("r"));
        assert_eq!(
            meta(&c, "price_fp").unwrap(),
            None,
            "raw costs go with the raw rows"
        );
        // A version older than the dimensions' shape, or no number at all: they go too.
        for old in ["5", "x"] {
            fill(&c, old);
            assert!(migrate(&c).unwrap());
            assert_eq!(dims(&c), vec![0, 0, 0, 0, 0], "version {old}");
            assert_eq!(meta(&c, "projects_rev").unwrap(), None);
        }
    }
}

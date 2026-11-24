//! The archive: quarter-hour rollups of spend, tool calls, edits, prompts and API errors, kept
//! after the raw rows they sum are gone (`docs/design/storage.md`).
//!
//! The key and sum columns of `usage_rollup` are declared once, in [`KEYS`] and [`SUMS`]: the
//! table, the `INSERT … SELECT` that fills it from `requests` and the union that reads it back as
//! if it were `requests` are all generated from them, so they cannot drift.
//!
//! Reads go through [`plan`]: a span that starts at or after `meta.rolled_until` reads `requests`
//! alone, with exactly the SQL it ran before the archive existed. Only a span reaching further
//! back reads the union of raw rows and rollups. Raw rows and rollups never overlap (a compaction
//! inserts and deletes in one transaction), so the union needs no cutoff.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use super::ms;
use crate::core::period::{BUCKET_MS, Span};

/// The raw window is never shorter than this: the current weekly limit window (7 days) is always
/// raw, and a span starting less than this long ago never needs the archive (no I/O to decide).
/// The compactor's `keep_raw_days` must be at least this.
pub use crate::core::storage::MIN_RAW_DAYS;

/// A key column of `usage_rollup`.
pub struct Key {
    /// The column.
    pub col: &'static str,
    /// Its type and constraints.
    pub ddl: &'static str,
    /// Its value for a `requests` row. `:lc` is the long-context threshold of the row's model on
    /// its day (0: none), which the compactor binds per model and day.
    pub from_raw: &'static str,
    /// The `requests` column it stands for in the union, and the expression that gives it from a
    /// rollup row; `None` when it is no column of `requests`.
    pub as_raw: Option<(&'static str, &'static str)>,
}

/// The key of `usage_rollup`, in primary-key order: every dimension a report groups or filters
/// by, plus the long-context band (cost is not linear across it, so repricing needs it).
pub const KEYS: &[Key] = &[
    Key {
        col: "bucket",
        ddl: "INTEGER NOT NULL",
        from_raw: "ts / 900000",
        as_raw: Some(("ts", "bucket * 900000")),
    },
    Key {
        col: "seat",
        ddl: "TEXT NOT NULL",
        from_raw: "seat",
        as_raw: Some(("seat", "seat")),
    },
    Key {
        col: "session_id",
        ddl: "TEXT NOT NULL",
        from_raw: "session_id",
        as_raw: Some(("session_id", "session_id")),
    },
    Key {
        col: "agent_id",
        ddl: "TEXT NOT NULL",
        from_raw: "agent_id",
        as_raw: Some(("agent_id", "agent_id")),
    },
    // '' stands for NULL: primary-key columns cannot be NULL. The writer never stores ''.
    Key {
        col: "account_uuid",
        ddl: "TEXT NOT NULL DEFAULT ''",
        from_raw: "COALESCE(account_uuid, '')",
        as_raw: Some(("account_uuid", "NULLIF(account_uuid, '')")),
    },
    Key {
        col: "cwd",
        ddl: "TEXT NOT NULL DEFAULT ''",
        from_raw: "COALESCE(cwd, '')",
        as_raw: Some(("cwd", "NULLIF(cwd, '')")),
    },
    Key {
        col: "model",
        ddl: "TEXT NOT NULL",
        from_raw: "model",
        as_raw: Some(("model", "model")),
    },
    // Kept only when it differs from the model served (all `model_substitutions` needs).
    Key {
        col: "requested_model",
        ddl: "TEXT NOT NULL DEFAULT ''",
        from_raw: "CASE WHEN requested_model IS NULL OR requested_model = model THEN '' ELSE requested_model END",
        as_raw: Some(("requested_model", "NULLIF(requested_model, '')")),
    },
    Key {
        col: "effort",
        ddl: "TEXT NOT NULL DEFAULT ''",
        from_raw: "COALESCE(effort, '')",
        as_raw: Some(("effort", "NULLIF(effort, '')")),
    },
    Key {
        col: "fast",
        ddl: "INTEGER NOT NULL CHECK (fast IN (0, 1))",
        from_raw: "fast",
        as_raw: Some(("fast", "fast")),
    },
    Key {
        col: "long_ctx",
        ddl: "INTEGER NOT NULL CHECK (long_ctx IN (0, 1))",
        from_raw: "CASE WHEN :lc > 0 AND input + cw5 + cw1h + cread > :lc THEN 1 ELSE 0 END",
        as_raw: None,
    },
];

/// A column of `usage_rollup` that is not part of the key.
pub struct Sum {
    pub col: &'static str,
    pub ddl: &'static str,
    /// The aggregate over the `requests` rows of one key.
    pub from_raw: &'static str,
    /// How two rows of one key merge (`excluded.` is the incoming one).
    pub merge: &'static str,
    /// The column of the union it gives (`requests` columns, or the counts `n`, `n_unpriced`,
    /// `n_pending` that stand for `COUNT(*)` and the priced states), and how.
    pub as_raw: &'static [(&'static str, &'static str)],
}

macro_rules! token_sum {
    ($c:literal) => {
        Sum {
            col: $c,
            ddl: concat!("INTEGER NOT NULL CHECK (", $c, " >= 0)"),
            from_raw: concat!("SUM(", $c, ")"),
            merge: concat!($c, " + excluded.", $c),
            as_raw: &[($c, $c)],
        }
    };
}

macro_rules! cost_sum {
    ($c:literal) => {
        Sum {
            col: $c,
            ddl: concat!("REAL CHECK (", $c, " IS NULL OR ", $c, " >= 0)"),
            from_raw: concat!("SUM(", $c, ")"),
            merge: concat!($c, " + excluded.", $c),
            as_raw: &[($c, $c)],
        }
    };
}

/// The sums of `usage_rollup`. The cost and its parts are the exact sums of the rows' stored
/// values at `meta.price_fp` (NULL when no price is known: pending rows are never rolled up).
pub const SUMS: &[Sum] = &[
    Sum {
        col: "requests",
        ddl: "INTEGER NOT NULL CHECK (requests > 0)",
        from_raw: "COUNT(*)",
        merge: "requests + excluded.requests",
        as_raw: &[("n", "requests"), ("n_pending", "0")],
    },
    token_sum!("input"),
    token_sum!("cw5"),
    token_sum!("cw1h"),
    token_sum!("cread"),
    token_sum!("output"),
    token_sum!("thinking"),
    token_sum!("web"),
    token_sum!("tools"),
    cost_sum!("cost"),
    cost_sum!("cost_input"),
    cost_sum!("cost_cache_write"),
    cost_sum!("cost_cache_read"),
    cost_sum!("cost_output"),
    cost_sum!("cost_web"),
    cost_sum!("cache_saved"),
    // Whether any of the requests has a price (so `cost` is not NULL); how many have none is
    // `n_unpriced`.
    Sum {
        col: "priced",
        ddl: "INTEGER NOT NULL CHECK (priced IN (0, 1))",
        from_raw: "MAX(priced)",
        merge: "MAX(priced, excluded.priced)",
        as_raw: &[],
    },
    // Counted, never a flag: one key can hold requests with and without a price (a request
    // without tokens is priced at $0 whatever its model).
    Sum {
        col: "n_unpriced",
        ddl: "INTEGER NOT NULL DEFAULT 0 CHECK (n_unpriced >= 0)",
        from_raw: "SUM(priced = 0)",
        merge: "n_unpriced + excluded.n_unpriced",
        as_raw: &[("n_unpriced", "n_unpriced")],
    },
    // Requests without any token: priced at $0 even when their model has no price
    // (`PriceBook::cost_parts`), so a reprice from the sums knows how many stay priced.
    Sum {
        col: "n_empty",
        ddl: "INTEGER NOT NULL DEFAULT 0 CHECK (n_empty >= 0)",
        from_raw: "SUM(input + cw5 + cw1h + cread + output + web = 0)",
        merge: "n_empty + excluded.n_empty",
        as_raw: &[],
    },
    // The threshold `long_ctx` was taken against, so a later price version with another one can
    // be found.
    Sum {
        col: "lc_threshold",
        ddl: "INTEGER NOT NULL DEFAULT 0 CHECK (lc_threshold >= 0)",
        from_raw: ":lc",
        merge: "excluded.lc_threshold",
        as_raw: &[],
    },
    Sum {
        col: "first_ts",
        ddl: "INTEGER NOT NULL",
        from_raw: "MIN(ts)",
        merge: "MIN(first_ts, excluded.first_ts)",
        as_raw: &[],
    },
    Sum {
        col: "last_ts",
        ddl: "INTEGER NOT NULL",
        from_raw: "MAX(ts)",
        merge: "MAX(last_ts, excluded.last_ts)",
        as_raw: &[],
    },
];

/// The columns of the union, as `requests` columns (plus the counts): (column, raw expression,
/// rollup expression).
fn union_columns() -> Vec<(&'static str, &'static str, &'static str)> {
    let mut v: Vec<(&str, &str, &str)> = KEYS
        .iter()
        .filter_map(|k| k.as_raw.map(|(c, e)| (c, c, e)))
        .collect();
    for s in SUMS {
        for (c, e) in s.as_raw {
            let raw = match *c {
                "n" => "1",
                "n_unpriced" => "(priced = 0)",
                "n_pending" => "(priced IS NULL)",
                other => other,
            };
            v.push((c, raw, e));
        }
    }
    v
}

/// `usage_rollup` and the other archive tables.
pub fn archive_sql() -> String {
    let mut cols: Vec<String> = KEYS
        .iter()
        .map(|k| format!("  {} {}", k.col, k.ddl))
        .collect();
    cols.extend(SUMS.iter().map(|s| format!("  {} {}", s.col, s.ddl)));
    let pk: Vec<&str> = KEYS.iter().map(|k| k.col).collect();
    format!(
        r#"
-- One row per UTC quarter hour and key: the sums of the requests rolled up into it.
CREATE TABLE IF NOT EXISTS usage_rollup (
{cols},
  CHECK (CASE WHEN priced = 1 THEN cost IS NOT NULL ELSE cost IS NULL END),
  CHECK (first_ts / {BUCKET_MS} = bucket AND last_ts / {BUCKET_MS} = bucket AND first_ts <= last_ts),
  PRIMARY KEY ({pk})
) WITHOUT ROWID;

-- An agent's rolled-up part over its lifetime: what the live totals add to their raw sums.
-- Derived from usage_rollup (`agent_rollup_sql`).
CREATE TABLE IF NOT EXISTS agent_rollup (
  seat TEXT NOT NULL,
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  requests INTEGER NOT NULL CHECK (requests > 0),
  cost REAL NOT NULL CHECK (cost >= 0),
  unpriced INTEGER NOT NULL CHECK (unpriced >= 0),
  first_ts INTEGER NOT NULL,
  last_ts INTEGER NOT NULL,
  first_model TEXT NOT NULL,
  last_model TEXT NOT NULL,
  PRIMARY KEY (seat, session_id, agent_id)
) WITHOUT ROWID;

-- Tool calls per quarter hour, tool and command (`insights::command_key` of a shell command,
-- '' for other tools or a command not recorded).
CREATE TABLE IF NOT EXISTS tool_rollup (
  bucket INTEGER NOT NULL,
  name TEXT NOT NULL,
  cmd TEXT NOT NULL DEFAULT '',
  calls INTEGER NOT NULL CHECK (calls > 0),
  errors INTEGER NOT NULL CHECK (errors >= 0 AND errors <= calls),
  busy_ms INTEGER NOT NULL CHECK (busy_ms >= 0),
  PRIMARY KEY (bucket, name, cmd)
) WITHOUT ROWID;

-- Files edited per quarter hour, as `file_hash` of the target, for counting distinct files.
CREATE TABLE IF NOT EXISTS edit_rollup (
  bucket INTEGER NOT NULL,
  file_h INTEGER NOT NULL,
  PRIMARY KEY (bucket, file_h)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS prompt_rollup (
  bucket INTEGER PRIMARY KEY,
  prompts INTEGER NOT NULL CHECK (prompts > 0),
  chars INTEGER NOT NULL CHECK (chars >= 0)
);

CREATE TABLE IF NOT EXISTS api_error_rollup (
  bucket INTEGER NOT NULL,
  kind TEXT NOT NULL,
  n INTEGER NOT NULL CHECK (n > 0),
  PRIMARY KEY (bucket, kind)
) WITHOUT ROWID;

-- Config dirs (canonical path) whose transcripts were indexed when a compaction ran.
CREATE TABLE IF NOT EXISTS rollup_seats (
  seat_dir TEXT PRIMARY KEY,
  seat TEXT NOT NULL
);

-- One sample a day, for growth and projection. file_bytes: in use (the file less free pages).
CREATE TABLE IF NOT EXISTS storage_samples (
  day TEXT PRIMARY KEY CHECK (date(day) IS day),
  file_bytes INTEGER NOT NULL CHECK (file_bytes >= 0),
  raw_requests INTEGER NOT NULL CHECK (raw_requests >= 0),
  raw_tool_calls INTEGER NOT NULL CHECK (raw_tool_calls >= 0),
  rollup_rows INTEGER NOT NULL CHECK (rollup_rows >= 0)
);
"#,
        cols = cols.join(",\n"),
        pk = pk.join(", ")
    )
}

/// `INSERT INTO usage_rollup … SELECT … FROM requests WHERE {filter} GROUP BY <key>`, adding into
/// rows already there. `filter` names the rows (and binds `:lc`, see [`Key::from_raw`]); the
/// rows must be priced (`priced IS NOT NULL`).
pub fn rollup_insert_sql(filter: &str) -> String {
    let cols: Vec<&str> = KEYS
        .iter()
        .map(|k| k.col)
        .chain(SUMS.iter().map(|s| s.col))
        .collect();
    let exprs: Vec<&str> = KEYS
        .iter()
        .map(|k| k.from_raw)
        .chain(SUMS.iter().map(|s| s.from_raw))
        .collect();
    let group: Vec<String> = (1..=KEYS.len()).map(|i| i.to_string()).collect();
    let pk: Vec<&str> = KEYS.iter().map(|k| k.col).collect();
    let merge: Vec<String> = SUMS
        .iter()
        .map(|s| format!("{} = {}", s.col, s.merge))
        .collect();
    format!(
        "INSERT INTO usage_rollup({}) SELECT {} FROM requests WHERE {filter} GROUP BY {}
         ON CONFLICT({}) DO UPDATE SET {}",
        cols.join(", "),
        exprs.join(", "),
        group.join(", "),
        pk.join(", "),
        merge.join(", ")
    )
}

#[cfg(test)]
/// Rewrites the `agent_rollup` rows of the agents `filter` selects (over `usage_rollup u`) from
/// `usage_rollup`.
pub fn agent_rollup_sql(filter: &str) -> String {
    let model_at = |order: &str| {
        format!(
            "(SELECT x.model FROM usage_rollup x WHERE x.seat = u.seat AND x.session_id = u.session_id
               AND x.agent_id = u.agent_id ORDER BY x.{order} LIMIT 1)"
        )
    };
    format!(
        "DELETE FROM agent_rollup WHERE (seat, session_id, agent_id) IN (SELECT seat, session_id, agent_id FROM usage_rollup u WHERE {filter});
         INSERT INTO agent_rollup(seat, session_id, agent_id, requests, cost, unpriced, first_ts, last_ts, first_model, last_model)
         SELECT seat, session_id, agent_id, SUM(requests), TOTAL(cost), SUM(n_unpriced),
                MIN(first_ts), MAX(last_ts), {}, {}
         FROM usage_rollup u WHERE (seat, session_id, agent_id) IN (SELECT seat, session_id, agent_id FROM usage_rollup u WHERE {filter})
         GROUP BY seat, session_id, agent_id;",
        model_at("first_ts ASC"),
        model_at("last_ts DESC")
    )
}

/// What `edit_rollup` stores for an edited file: the first 8 bytes of its path's SHA-256.
pub fn file_hash(target: &str) -> i64 {
    let d = Sha256::digest(target.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    i64::from_be_bytes(b)
}

/// How a read over a span runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// The span in epoch ms (the start at 0 at the earliest).
    pub start: i64,
    pub end: i64,
    /// Whether the span reaches rolled-up spend.
    pub archive: bool,
}

impl Plan {
    /// Rollup buckets inside the plan's span: `bucket * BUCKET_MS` in `[start, end)`.
    pub fn buckets(&self) -> (i64, i64) {
        (
            self.start.div_euclid(BUCKET_MS) + i64::from(self.start.rem_euclid(BUCKET_MS) != 0),
            self.end.div_euclid(BUCKET_MS) + i64::from(self.end.rem_euclid(BUCKET_MS) != 0),
        )
    }

    /// `requests`, or the union of raw rows and rollups in the span, as a `FROM` source.
    pub fn source(&self) -> String {
        if !self.archive {
            return "requests".into();
        }
        let cols = union_columns();
        let raw: Vec<String> = cols
            .iter()
            .map(|(c, r, _)| {
                if c == r {
                    c.to_string()
                } else {
                    format!("{r} AS {c}")
                }
            })
            .collect();
        let rolled: Vec<&str> = cols.iter().map(|(_, _, e)| *e).collect();
        let (b0, b1) = self.buckets();
        format!(
            "(SELECT {} FROM requests WHERE ts >= {} AND ts < {}
              UNION ALL SELECT {} FROM usage_rollup WHERE bucket >= {b0} AND bucket < {b1})",
            raw.join(", "),
            self.start,
            self.end,
            rolled.join(", ")
        )
    }

    /// `COUNT(*)` over the plan's source (`r`).
    pub fn count(&self) -> &'static str {
        if self.archive { "SUM(r.n)" } else { "COUNT(*)" }
    }

    /// Request count, token sums, cost (NULL costs left out), unpriced and pending counts.
    pub fn sums(&self) -> &'static str {
        if self.archive {
            "SUM(r.n), SUM(r.input), SUM(r.cw5), SUM(r.cw1h), SUM(r.cread), SUM(r.output), SUM(r.thinking), SUM(r.web),
  TOTAL(r.cost), SUM(r.n_unpriced), SUM(r.n_pending)"
        } else {
            "COUNT(*), SUM(r.input), SUM(r.cw5), SUM(r.cw1h), SUM(r.cread), SUM(r.output), SUM(r.thinking), SUM(r.web),
  TOTAL(r.cost), SUM(r.priced = 0), SUM(r.priced IS NULL)"
        }
    }
}

/// Whether `span` starts inside the shortest raw window: then it reads raw rows only, decided
/// without touching the database.
pub fn inside_raw_window(span: Span, now: DateTime<Utc>) -> bool {
    span.start >= now - chrono::Duration::days(MIN_RAW_DAYS)
}

/// Every raw row older than this (epoch ms, a quarter-hour boundary) has been rolled up; 0 when
/// nothing has.
pub fn rolled_until(conn: &Connection) -> anyhow::Result<i64> {
    let v: Option<String> = conn
        .prepare_cached("SELECT value FROM meta WHERE key = 'rolled_until'")?
        .query_row([], |r| r.get(0))
        .optional()?;
    Ok(match v {
        Some(v) => v.parse()?,
        None => 0,
    })
}

/// How to read `span`, in the snapshot of `conn`. A span reaching rolled-up spend must start on a
/// quarter hour (a rollup cannot be split); `snap` floors it there instead (limit calibration of
/// an account idle for longer than the raw window, design §3.5). A span end inside the archive
/// counts the whole quarter hour it falls in.
pub fn plan(conn: &Connection, span: Span, snap: bool, now: DateTime<Utc>) -> anyhow::Result<Plan> {
    let mut start = ms(span.start.max(DateTime::<Utc>::UNIX_EPOCH));
    let end = ms(span.end);
    if inside_raw_window(span, now) {
        return Ok(Plan {
            start,
            end,
            archive: false,
        });
    }
    let archive = start < rolled_until(conn)?;
    if archive && snap {
        start -= start.rem_euclid(BUCKET_MS);
    }
    debug_assert!(
        !archive || start % BUCKET_MS == 0,
        "a span reaching rolled-up spend must start on a quarter hour: {start}"
    );
    Ok(Plan {
        start,
        end,
        archive,
    })
}

/// Runs `f` with the plan of `span`, in one read snapshot when deciding the plan reads the
/// database (so a compaction committed in between cannot move rows out from under the plan).
pub fn read<T>(
    conn: &Connection,
    span: Span,
    snap: bool,
    now: DateTime<Utc>,
    f: impl FnOnce(&Connection, Plan) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if inside_raw_window(span, now) || !conn.is_autocommit() {
        let p = plan(conn, span, snap, now)?;
        return f(conn, p);
    }
    let tx = conn.unchecked_transaction()?;
    let p = plan(&tx, span, snap, now)?;
    let v = f(&tx, p)?;
    tx.commit()?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema;

    #[test]
    fn the_union_has_every_column_once_and_the_raw_arm_comes_first() {
        let cols = union_columns();
        let names: Vec<&str> = cols.iter().map(|c| c.0).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "{names:?}");
        for c in [
            "ts",
            "seat",
            "session_id",
            "agent_id",
            "account_uuid",
            "cwd",
            "model",
            "requested_model",
            "effort",
            "fast",
            "cost",
            "n",
            "n_unpriced",
            "n_pending",
        ] {
            assert!(names.contains(&c), "{c}");
        }
        for c in schema::COST_PARTS {
            assert!(names.contains(c), "{c}");
            assert!(SUMS.iter().any(|s| s.col == *c), "{c} is summed");
        }
        let p = Plan {
            start: 0,
            end: BUCKET_MS * 4 + 1,
            archive: true,
        };
        let src = p.source();
        assert!(src.find("FROM requests").unwrap() < src.find("FROM usage_rollup").unwrap());
        assert!(src.contains("bucket >= 0 AND bucket < 5"), "{src}");
        let raw = Plan {
            archive: false,
            ..p
        };
        assert_eq!(raw.source(), "requests");
    }

    #[test]
    fn plan_buckets_are_those_starting_inside_the_span() {
        let p = |start, end| {
            Plan {
                start,
                end,
                archive: true,
            }
            .buckets()
        };
        assert_eq!(p(0, BUCKET_MS), (0, 1));
        assert_eq!(p(BUCKET_MS, 3 * BUCKET_MS + 1), (1, 4));
        assert_eq!(p(1, BUCKET_MS), (1, 1));
    }

    #[test]
    fn file_hashes_are_stable() {
        assert_eq!(file_hash("/w/a.rs"), file_hash("/w/a.rs"));
        assert_ne!(file_hash("/w/a.rs"), file_hash("/w/b.rs"));
        // sha256("") starts e3 b0 c4 42 98 fc 1c 14.
        assert_eq!(
            file_hash(""),
            i64::from_be_bytes([0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14])
        );
    }
}

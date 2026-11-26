//! End-to-end scenario tests for mixed Claude Code + Google Antigravity (`agy`) indices:
//! ingestion, unified projects, `Dim::Harness` / `Filter::harness` across raw and compacted
//! rollups, subagent parent-child hierarchy, unpriced preview models, and cross-harness edit
//! conflict detection. All paths and accounts are synthetic.

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::Path;

    use chrono::{DateTime, Duration, Utc};
    use rusqlite::Connection;

    use crate::claude::project::Resolver;
    use crate::claude::seats::Seat;
    use crate::core::harness::Harness;
    use crate::core::period::Span;
    use crate::core::pricing::PriceBook;
    use crate::index::compact::CompactOpts;
    use crate::index::live::AgentKey;
    use crate::index::query::{Dim, Filter};
    use crate::index::scenarios::{account, assistant, at, owner};
    use crate::index::{Index, SeatInfo};

    fn write_lines(path: &Path, lines: &[String]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn agy_planner(
        step: u64,
        ts: &str,
        model: &str,
        input: u64,
        cread: u64,
        output: u64,
        tools: &[(&str, serde_json::Value)],
    ) -> String {
        let calls: Vec<serde_json::Value> = tools
            .iter()
            .map(|(name, args)| {
                serde_json::json!({
                    "name": name,
                    "args": args,
                })
            })
            .collect();
        serde_json::json!({
            "step_index": step,
            "type": "PLANNER_RESPONSE",
            "created_at": ts,
            "model": model,
            "input_tokens": input,
            "cache_read_tokens": cread,
            "output_tokens": output,
            "tool_calls": calls,
        })
        .to_string()
    }

    fn agy_generic(step: u64, ts: &str, completed_at: &str) -> String {
        serde_json::json!({
            "step_index": step,
            "type": "GENERIC",
            "created_at": ts,
            "content": format!("Tool finished.\nCompleted At: {completed_at}"),
        })
        .to_string()
    }

    fn agy_user(step: u64, ts: &str, text: &str) -> String {
        serde_json::json!({
            "step_index": step,
            "type": "USER_INPUT",
            "created_at": ts,
            "content": text,
        })
        .to_string()
    }

    fn write_summaries_db(seat_dir: &Path, rows: &[(&str, &str, &str, &str, &str, i64, &str)]) {
        let db = seat_dir.join("conversation_summaries.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversation_summaries (
               conversation_id TEXT PRIMARY KEY,
               title TEXT NOT NULL DEFAULT '',
               workspace_uris TEXT NOT NULL DEFAULT '',
               status TEXT NOT NULL DEFAULT '',
               parent_conversation_id TEXT NOT NULL DEFAULT '',
               nesting_depth INTEGER NOT NULL DEFAULT 0,
               agent_name TEXT NOT NULL DEFAULT '',
               not_fully_idle NUMERIC NOT NULL DEFAULT 0,
               killed NUMERIC NOT NULL DEFAULT 0,
               last_modified_time TEXT NOT NULL DEFAULT ''
             );",
        )
        .unwrap();
        for (cid, title, uri, status, parent, depth, agent_name) in rows {
            conn.execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, '2026-10-01 12:00:00+00:00')",
                rusqlite::params![cid, title, uri, status, parent, depth, agent_name],
            )
            .unwrap();
        }
    }

    fn mixed_world() -> (tempfile::TempDir, Vec<Seat>, Vec<SeatInfo>, Index) {
        let dir = tempfile::tempdir().unwrap();
        let cc_dir = dir.path().join("claude-work");
        let agy_dir = dir.path().join("agy-main");
        std::fs::create_dir_all(&cc_dir).unwrap();
        std::fs::create_dir_all(&agy_dir).unwrap();

        let cc = Seat::new("work", cc_dir.clone(), cc_dir.join(".claude.json"), false);
        let agy = Seat::agy("agy", agy_dir.clone());

        // Claude seat: 1 request on /w/shop with claude-sonnet-5-5 ($2 input, $10 output per MTok)
        // 100k input ($0.20) + 10k output ($0.10) = $0.30.
        // Also edits /w/shop/src/lib.rs at a recent timestamp for conflict test.
        // Both recent edits share one UTC hour inside the last 24h (never straddling an hour mark).
        let hour_base =
            chrono::DateTime::from_timestamp((Utc::now().timestamp() / 3600 - 1) * 3600 + 60, 0)
                .unwrap();
        let cc_file = cc_dir.join("projects").join("p").join("cc-sess.jsonl");
        let recent_ts = hour_base.to_rfc3339();
        write_lines(
            &cc_file,
            &[
                owner("acc-cc"),
                assistant(
                    "cc-m1",
                    "2026-10-01T10:00:00Z",
                    "claude-sonnet-5-5",
                    "/w/shop",
                    (100_000, 10_000),
                    &[],
                ),
                serde_json::json!({
                    "type": "assistant",
                    "timestamp": recent_ts,
                    "cwd": "/w/shop",
                    "sessionId": "cc-sess",
                    "message": {
                        "id": "cc-m2",
                        "model": "claude-sonnet-5-5",
                        "content": [{
                            "type": "tool_use",
                            "id": "cc-t1",
                            "name": "Edit",
                            "input": {"file_path": "/w/shop/src/lib.rs"}
                        }],
                        "usage": {"input_tokens": 1000, "output_tokens": 100,
                                  "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
                    }
                })
                .to_string(),
            ],
        );

        // Agy seat:
        // - root conversation `c-root` on /w/shop:
        //   * 1 request on gemini-2.5-pro ($1.25 input, $0.125 cread, $10.00 output):
        //     100k input ($0.125) + 80k cread at $0.125/MTok ($0.010) + 20k output ($0.200) = $0.335
        //   * 1 recent edit_file on /w/shop/src/lib.rs (triggers cross-harness conflict!)
        // - child subagent `c-sub` (parent = c-root) on `gemini-4-neon-high`, unpriced on 2026-10-01
        //   (its price applies from 2026-11-18):
        //   * 1 request -> cost = 0, priced = 0, effort = "high", model = "gemini-4-neon"
        write_summaries_db(
            &agy_dir,
            &[
                (
                    "c-root",
                    "Refactor checkout",
                    r#"["file:///w/shop"]"#,
                    "CASCADE_RUN_STATUS_IDLE",
                    "",
                    0,
                    "",
                ),
                (
                    "c-sub",
                    "Codebase check",
                    r#"["file:///w/shop"]"#,
                    "CASCADE_RUN_STATUS_IDLE",
                    "c-root",
                    1,
                    "research",
                ),
            ],
        );

        let root_path = agy_dir
            .join("brain")
            .join("c-root")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let agy_recent_ts = (hour_base + Duration::seconds(30)).to_rfc3339();
        let agy_recent_done = (hour_base + Duration::seconds(45)).to_rfc3339();
        write_lines(
            &root_path,
            &[
                agy_user(0, "2026-10-01T10:05:00Z", "Refactor the checkout handler"),
                agy_planner(
                    1,
                    "2026-10-01T10:05:10Z",
                    "gemini-2.5-pro",
                    100_000,
                    80_000,
                    20_000,
                    &[(
                        "edit_file",
                        serde_json::json!({"Path": "/w/shop/src/lib.rs"}),
                    )],
                ),
                agy_generic(2, "2026-10-01T10:05:12Z", "2026-10-01T10:05:15Z"),
                agy_planner(
                    3,
                    &agy_recent_ts,
                    "gemini-2.5-pro",
                    1000,
                    0,
                    100,
                    &[(
                        "edit_file",
                        serde_json::json!({"Path": "/w/shop/src/lib.rs"}),
                    )],
                ),
                agy_generic(4, &agy_recent_ts, &agy_recent_done),
            ],
        );

        let sub_path = agy_dir
            .join("brain")
            .join("c-sub")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        write_lines(
            &sub_path,
            &[agy_planner(
                0,
                "2026-10-01T10:06:00Z",
                "gemini-4-neon-high",
                50_000,
                10_000,
                2_000,
                &[],
            )],
        );

        let info = vec![
            SeatInfo {
                seat: cc.id.clone(),
                label: cc.label.clone(),
                account: Some(account("acc-cc", "2026-10-01T00:00:00Z")),
                switched_at: None,
                utilization: None,
            },
            SeatInfo {
                seat: agy.id.clone(),
                label: agy.label.clone(),
                account: Some(account("agy:sub-1", "2026-10-01T00:00:00Z")),
                switched_at: None,
                utilization: None,
            },
        ];

        let mut idx = Index::open(&dir.path().join("index.sqlite")).unwrap();
        let seats = vec![cc, agy];
        let mut res = Resolver::new(Vec::new());
        idx.refresh(&seats, &info, &mut res, &PriceBook::builtin(), &mut |_| {})
            .unwrap();
        (dir, seats, info, idx)
    }

    #[test]
    fn mixed_claude_and_agy_unified_projects_harnesses_subagents_and_compaction() {
        let (_dir, seats, _info, mut idx) = mixed_world();
        let agy_seat = &seats[1];

        // 1. Historical day span (2026-10-01) isolates the hand-computed rows from the recent conflict rows.
        let oct1 = Span {
            start: at("2026-10-01T00:00:00Z"),
            end: at("2026-10-02T00:00:00Z"),
        };

        // Unified project spend: Claude ($0.30) + Agy ($0.335) = $0.635 under one project "shop".
        let by_proj = idx
            .spend_by(Dim::Project, oct1, &Filter::default())
            .unwrap();
        assert_eq!(by_proj.len(), 1);
        assert_eq!(by_proj[0].key, "/w/shop");
        assert_eq!(by_proj[0].label, "shop");
        assert!((by_proj[0].cost - 0.635).abs() < 1e-6, "{by_proj:?}");
        assert_eq!(by_proj[0].requests, 3);
        assert_eq!(by_proj[0].unpriced, 1);

        // Dim::Harness splits cleanly into "agy" ($0.335, 2 reqs, 1 unpriced) and "claude" ($0.30, 1 req).
        let by_harness_before = idx
            .spend_by(Dim::Harness, oct1, &Filter::default())
            .unwrap();
        assert_eq!(by_harness_before.len(), 2);
        let agy_row = by_harness_before.iter().find(|r| r.key == "agy").unwrap();
        let cc_row = by_harness_before
            .iter()
            .find(|r| r.key == "claude")
            .unwrap();
        assert!((agy_row.cost - 0.335).abs() < 1e-6, "{agy_row:?}");
        assert_eq!(agy_row.requests, 2);
        assert_eq!(agy_row.unpriced, 1);
        assert!((cc_row.cost - 0.30).abs() < 1e-6, "{cc_row:?}");
        assert_eq!(cc_row.requests, 1);
        assert_eq!(cc_row.unpriced, 0);

        // Filter::harness narrows totals cleanly.
        let f_agy = Filter {
            harness: Some(Harness::Agy),
            ..Default::default()
        };
        let agy_tot_before = idx.total(oct1, &f_agy).unwrap();
        assert!((agy_tot_before.cost - 0.335).abs() < 1e-6);
        assert_eq!(agy_tot_before.requests, 2);
        assert_eq!(agy_tot_before.unpriced, 1);

        // Subagent hierarchy: c-sub is indexed under session_id = "c-root", agent_id = "c-sub",
        // with model = "gemini-4-neon", effort = "high", priced = Some(0).
        let sub_key = AgentKey {
            seat: &agy_seat.id,
            session_id: &crate::core::ids::SessionId::new("c-root"),
            agent_id: "c-sub",
        };
        let last_sub = idx.last_request(sub_key).unwrap().unwrap();
        assert_eq!(last_sub.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(last_sub.effort.as_deref(), Some("high"));
        assert_eq!(last_sub.priced, Some(0));

        // Cross-harness file edit conflict: Claude (`Edit`) and Agy (`edit_file`) both edited
        // `/w/shop/src/lib.rs` in the last 30 minutes -> detected in efficiency().conflicts!
        let all_span = Span {
            start: DateTime::<Utc>::UNIX_EPOCH,
            end: Utc::now() + Duration::hours(1),
        };
        let eff = idx.efficiency(all_span).unwrap();
        assert!(
            eff.conflicts
                .iter()
                .any(|(path, n)| path == "/w/shop/src/lib.rs" && *n >= 2),
            "expected cross-harness edit conflict, got {:?}",
            eff.conflicts
        );

        // Compaction proof: compact everything before 2026-10-15 into rollups; Dim::Harness,
        // Dim::Project, and Filter::harness must return identical numbers post-compaction!
        let compact_now = at("2026-11-01T00:00:00Z");
        idx.compact(&CompactOpts {
            keep_raw_days: 14,
            now: compact_now,
            prices: &PriceBook::builtin(),
            seats: &seats,
            max_chunks: None,
        })
        .unwrap();

        let by_harness_after = idx
            .spend_by(Dim::Harness, oct1, &Filter::default())
            .unwrap();
        assert_eq!(by_harness_before.len(), by_harness_after.len());
        for (b, a) in by_harness_before.iter().zip(by_harness_after.iter()) {
            assert_eq!(b.key, a.key);
            assert!((b.cost - a.cost).abs() < 1e-9);
            assert_eq!(b.requests, a.requests);
            assert_eq!(b.unpriced, a.unpriced);
        }
        let agy_tot_after = idx.total(oct1, &f_agy).unwrap();
        assert!((agy_tot_before.cost - agy_tot_after.cost).abs() < 1e-9);
        assert_eq!(agy_tot_before.requests, agy_tot_after.requests);
        assert_eq!(agy_tot_before.unpriced, agy_tot_after.unpriced);
    }
}

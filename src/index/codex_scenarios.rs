//! End-to-end scenario tests for a Claude Code + OpenAI Codex index: rollouts read into requests
//! (each response once, though Codex writes its usage twice), one project for the repo both
//! worked on, `Dim::Harness` / `Filter::harness`, a rollout read again as it grows, thread names,
//! tool calls and cross-harness edit conflicts, before and after compaction. All paths, ids and
//! accounts are synthetic.

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use chrono::{DateTime, Duration, Utc};
    use serde_json::{Value, json};

    use crate::claude::project::Resolver;
    use crate::claude::seats::Seat;
    use crate::core::harness::Harness;
    use crate::core::period::Span;
    use crate::core::pricing::PriceBook;
    use crate::index::compact::CompactOpts;
    use crate::index::query::{Dim, Filter};
    use crate::index::scenarios::{account, assistant, at, owner};
    use crate::index::{Index, SeatInfo};

    const TID: &str = "0c0d0e0f-0000-4000-8000-00000000c0de";

    fn append(path: &Path, lines: &[String]) {
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

    fn line(ts: &str, kind: &str, payload: Value) -> String {
        json!({"timestamp": ts, "type": kind, "payload": payload}).to_string()
    }

    fn tokens(input: u64, cached: u64, output: u64, total: u64) -> Value {
        json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
               "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": total})
    }

    /// One response as Codex 0.172 writes it: its `token_usage_record`, then a `token_count`
    /// with the same numbers. `total` is the thread's running total after it.
    fn response(ts: &str, input: u64, cached: u64, output: u64, total: u64) -> [String; 2] {
        let u = tokens(input, cached, output, input + output);
        let t = tokens(0, 0, 0, total);
        [
            line(
                ts,
                "token_usage_record",
                json!({"thread_id": TID, "usage": u, "thread_token_usage": t}),
            ),
            line(
                ts,
                "event_msg",
                json!({"type": "token_count",
                       "info": {"total_token_usage": t, "last_token_usage": u}}),
            ),
        ]
    }

    fn world() -> (tempfile::TempDir, Vec<Seat>, Vec<SeatInfo>, Index, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let cc_dir = dir.path().join("claude-work");
        let cdx_dir = dir.path().join("codex-home");
        std::fs::create_dir_all(&cc_dir).unwrap();
        std::fs::create_dir_all(&cdx_dir).unwrap();
        let cc = Seat::new("work", cc_dir.clone(), cc_dir.join(".claude.json"), false);
        let cdx = Seat::codex("codex", cdx_dir.clone());

        // Both recent edits share one UTC hour inside the last 24h (never straddling an hour mark).
        let hour_base =
            DateTime::from_timestamp((Utc::now().timestamp() / 3600 - 1) * 3600 + 60, 0).unwrap();
        let recent = hour_base.to_rfc3339();
        let recent_codex = (hour_base + Duration::seconds(30)).to_rfc3339();

        // Claude seat: 1 request on /w/shop with claude-sonnet-5-5 ($2 input, $10 output per
        // MTok): 100k input ($0.20) + 10k output ($0.10) = $0.30; a recent Edit of src/lib.rs.
        append(
            &cc_dir.join("projects/p/cc-sess.jsonl"),
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
                json!({"type": "assistant", "timestamp": recent, "cwd": "/w/shop", "sessionId": "cc-sess",
                       "message": {"id": "cc-m2", "model": "claude-sonnet-5-5",
                           "content": [{"type": "tool_use", "id": "cc-t1", "name": "Edit",
                                        "input": {"file_path": "/w/shop/src/lib.rs"}}],
                           "usage": {"input_tokens": 1000, "output_tokens": 100,
                                     "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}})
                .to_string(),
            ],
        );

        // Codex seat: one thread on /w/shop with gpt-5.6-terra
        // ($2.00 input, $0.20 cached, $12.00 output per MTok):
        // - response 1: 120k input of which 100k cached, 5k output:
        //   20k × 2.00 = $0.040 + 100k × 0.20 = $0.020 + 5k × 12.00 = $0.060 -> $0.120
        //   (it calls `shell`, whose output comes after the usage);
        // - response 2: 130k input of which 120k cached, 2k output:
        //   10k × 2.00 = $0.020 + 120k × 0.20 = $0.024 + 2k × 12.00 = $0.024 -> $0.068.
        // Codex total $0.188 in 2 requests, though each usage is written twice.
        let day = cdx_dir.join("sessions/2026/10/01");
        let rollout = day.join(format!("rollout-2026-10-01T10-04-00-{TID}.jsonl"));
        let mut lines = vec![
            line(
                "2026-10-01T10:04:00Z",
                "session_meta",
                json!({"id": TID, "cwd": "/w/shop", "cli_version": "0.172.0",
                       "creator_user_id": "user-7", "creator_account_id": "ws-7",
                       "git": {"branch": "main"}}),
            ),
            line(
                "2026-10-01T10:04:01Z",
                "event_msg",
                json!({"type": "task_started"}),
            ),
            line(
                "2026-10-01T10:04:01Z",
                "turn_context",
                json!({"cwd": "/w/shop", "model": "gpt-5.6-terra", "effort": "medium"}),
            ),
            line(
                "2026-10-01T10:04:02Z",
                "event_msg",
                json!({"type": "item_completed", "item": {"type": "UserMessage",
                       "content": [{"type": "text", "text": "Make the API tests pass"}]}}),
            ),
            line(
                "2026-10-01T10:04:10Z",
                "response_item",
                json!({"type": "function_call", "name": "shell", "call_id": "call_a",
                       "arguments": "{\"command\":[\"bash\",\"-lc\",\"cargo test\"]}"}),
            ),
        ];
        lines.extend(response(
            "2026-10-01T10:04:10Z",
            120_000,
            100_000,
            5_000,
            125_000,
        ));
        lines.push(line(
            "2026-10-01T10:04:20Z",
            "response_item",
            json!({"type": "function_call_output", "call_id": "call_a", "output": "ok"}),
        ));
        lines.extend(response(
            "2026-10-01T10:04:30Z",
            130_000,
            120_000,
            2_000,
            257_000,
        ));
        lines.push(line(
            "2026-10-01T10:04:31Z",
            "event_msg",
            json!({"type": "task_complete", "duration_ms": 30000}),
        ));
        append(&rollout, &lines);
        std::fs::write(
            cdx_dir.join("session_index.jsonl"),
            format!("{{\"id\":\"{TID}\",\"thread_name\":\"API tests\",\"updated_at\":\"x\"}}\n"),
        )
        .unwrap();
        // A recent second thread edits the file Claude edited, with a relative patch path.
        let tid2 = "0c0d0e0f-0000-4000-8000-00000000c0df";
        let mut recent_lines = vec![
            line(
                &recent_codex,
                "turn_context",
                json!({"cwd": "/w/shop", "model": "gpt-5.6-terra"}),
            ),
            line(
                &recent_codex,
                "response_item",
                json!({"type": "custom_tool_call", "name": "apply_patch", "call_id": "call_p",
                       "input": "*** Begin Patch\n*** Update File: src/lib.rs\n*** End Patch"}),
            ),
        ];
        let u = tokens(1_000, 0, 100, 1_100);
        recent_lines.push(line(
            &recent_codex,
            "event_msg",
            json!({"type": "token_count", "info": {"total_token_usage": u, "last_token_usage": u}}),
        ));
        append(
            &cdx_dir.join(format!(
                "sessions/2026/11/24/rollout-2026-11-24T10-00-00-{tid2}.jsonl"
            )),
            &recent_lines,
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
                seat: cdx.id.clone(),
                label: cdx.label.clone(),
                account: None,
                switched_at: None,
                utilization: None,
            },
        ];
        let mut idx = Index::open(&dir.path().join("index.sqlite")).unwrap();
        let seats = vec![cc, cdx];
        let mut res = Resolver::new(Vec::new());
        idx.refresh(&seats, &info, &mut res, &PriceBook::builtin(), &mut |_| {})
            .unwrap();
        (dir, seats, info, idx, rollout)
    }

    fn oct1() -> Span {
        Span {
            start: at("2026-10-01T00:00:00Z"),
            end: at("2026-10-02T00:00:00Z"),
        }
    }

    /// Review B-M10: a hot refresh that does not know a file's seat (a harness disabled, a seat
    /// found only through another process's environment) leaves the file alone. It used to read
    /// a Codex rollout with Claude Code's parser and move the offset past the new spend.
    #[test]
    fn a_hot_refresh_leaves_files_of_an_unknown_seat_alone() {
        let (_dir, seats, info, mut idx, rollout) = world();
        let offset = |idx: &Index| {
            crate::index::scenarios::dump(
                idx,
                &format!(
                    "SELECT offset FROM files WHERE path = '{}'",
                    rollout.display()
                ),
            )
        };
        let before = offset(&idx);
        append(
            &rollout,
            &response("2026-10-01T10:05:00Z", 140_000, 130_000, 1_000, 398_000),
        );
        let claude_only = &seats[..1];
        let mut res = Resolver::new(Vec::new());
        idx.refresh_hot(claude_only, &[], &info, &mut res, &PriceBook::builtin())
            .unwrap();
        assert_eq!(offset(&idx), before);
        // The full refresh that knows the seat then reads the new response.
        idx.refresh(&seats, &info, &mut res, &PriceBook::builtin(), &mut |_| {})
            .unwrap();
        let codex_only = Filter {
            harness: Some(Harness::Codex),
            ..Default::default()
        };
        assert_eq!(idx.total(oct1(), &codex_only).unwrap().requests, 3);
    }

    #[test]
    fn codex_spend_is_counted_once_per_response_and_joins_the_project() {
        let (_dir, seats, info, mut idx, rollout) = world();
        let codex_only = Filter {
            harness: Some(Harness::Codex),
            ..Default::default()
        };
        let t = idx.total(oct1(), &codex_only).unwrap();
        assert_eq!(t.requests, 2, "{t:?}");
        assert!((t.cost - 0.188).abs() < 1e-9, "{t:?}");
        assert_eq!(t.unpriced, 0);

        // One project for the repo, whichever harness worked on it:
        // $0.30 (Claude) + $0.188 (Codex) = $0.488 in 3 requests.
        let by_proj = idx
            .spend_by(Dim::Project, oct1(), &Filter::default())
            .unwrap();
        assert_eq!(by_proj.len(), 1, "{by_proj:?}");
        assert_eq!(by_proj[0].label, "shop");
        assert!((by_proj[0].cost - 0.488).abs() < 1e-9, "{by_proj:?}");
        assert_eq!(by_proj[0].requests, 3);

        let by_harness = idx
            .spend_by(Dim::Harness, oct1(), &Filter::default())
            .unwrap();
        let row = |k: &str| by_harness.iter().find(|r| r.key == k).unwrap();
        assert!((row("codex").cost - 0.188).abs() < 1e-9, "{by_harness:?}");
        assert!((row("claude").cost - 0.30).abs() < 1e-9, "{by_harness:?}");

        let by_model = idx.spend_by(Dim::Model, oct1(), &codex_only).unwrap();
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].key, "gpt-5.6-terra");

        // The thread's name labels its session; its account is the one session_meta names.
        let by_session = idx.spend_by(Dim::Session, oct1(), &codex_only).unwrap();
        assert_eq!(by_session[0].label, "API tests", "{by_session:?}");
        let by_account = idx.spend_by(Dim::Account, oct1(), &codex_only).unwrap();
        assert_eq!(by_account[0].key, "cdx:user-7/ws-7", "{by_account:?}");

        // The rollout grows: only the new response is read, with the model of the turn before.
        // response 3: 140k input of which 130k cached, 1k output:
        // 10k × 2.00 = $0.020 + 130k × 0.20 = $0.026 + 1k × 12.00 = $0.012 -> $0.058.
        append(
            &rollout,
            &response("2026-10-01T10:05:00Z", 140_000, 130_000, 1_000, 398_000),
        );
        let mut res = Resolver::new(Vec::new());
        idx.refresh(&seats, &info, &mut res, &PriceBook::builtin(), &mut |_| {})
            .unwrap();
        let t = idx.total(oct1(), &codex_only).unwrap();
        assert_eq!(t.requests, 3);
        assert!((t.cost - 0.246).abs() < 1e-9, "{t:?}");

        // Claude's Edit and Codex's apply_patch of /w/shop/src/lib.rs within the hour: a conflict.
        let all = Span {
            start: DateTime::<Utc>::UNIX_EPOCH,
            end: Utc::now() + Duration::hours(1),
        };
        let eff = idx.efficiency(all).unwrap();
        assert!(
            eff.conflicts
                .iter()
                .any(|(path, n)| path == "/w/shop/src/lib.rs" && *n >= 2),
            "{:?}",
            eff.conflicts
        );
        assert!(
            eff.commands.iter().any(|c| c.name.starts_with("cargo")),
            "{:?}",
            eff.commands
        );

        // Compacted, the numbers stay the same.
        let before = idx
            .spend_by(Dim::Harness, oct1(), &Filter::default())
            .unwrap();
        idx.compact(&CompactOpts {
            keep_raw_days: 14,
            now: at("2026-11-01T00:00:00Z"),
            prices: &PriceBook::builtin(),
            seats: &seats,
            max_chunks: None,
        })
        .unwrap();
        let after = idx
            .spend_by(Dim::Harness, oct1(), &Filter::default())
            .unwrap();
        assert_eq!(before.len(), after.len());
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(b.key, a.key);
            assert!((b.cost - a.cost).abs() < 1e-9);
            assert_eq!(b.requests, a.requests);
        }
    }
}

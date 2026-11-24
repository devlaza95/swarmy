//! Scenario tests of organization-scoped account identity (`<accountUuid>/<organizationUuid>`):
//! two seats under the same `accountUuid` with different `organizationUuid` stay separate accounts
//! with independent limits and plans; legacy indexes with bare `accountUuid` migrate cleanly in
//! place for both single-org and multi-org setups.

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::app::context::Ctx;
    use crate::claude::config_json::Account;
    use crate::claude::project::Resolver;
    use crate::config::{Config, Dirs};
    use crate::core::ids::AccountId;
    use crate::core::pricing::PriceBook;
    use crate::index::SeatInfo;
    use crate::index::compact::CompactOpts;
    use crate::index::query::{Dim, Filter};
    use crate::index::scenarios::{World, assistant, at, dump, owner, weekly};
    use crate::limits::all_reports;
    use crate::procs::ProcTable;
    use crate::ui::style::{ColorChoice, Painter};

    const M: &str = "claude-sonnet-5-5";

    fn owner_org(account: &str, org: &str) -> String {
        serde_json::json!({
            "type": "session-owner",
            "accountUuid": account,
            "organizationUuid": org,
            "sessionId": "s"
        })
        .to_string()
    }

    fn org_account(
        uuid: &str,
        org: &str,
        org_name: &str,
        email: &str,
        org_tier: Option<&str>,
        user_tier: Option<&str>,
    ) -> Account {
        Account {
            account_uuid: uuid.into(),
            organization_uuid: Some(org.into()),
            organization_name: Some(org_name.into()),
            email: Some(email.into()),
            organization_rate_limit_tier: org_tier.map(str::to_string),
            user_rate_limit_tier: user_tier.map(str::to_string),
            profile_fetched_at: Some(at("2026-11-01T00:00:00Z")),
            ..Default::default()
        }
    }

    fn ctx_of(w: &World, seat_info: Vec<SeatInfo>, now: &str) -> Ctx {
        Ctx {
            dirs: Dirs {
                data: w.dir.path().join("data"),
                config: w.dir.path().join("config"),
                home: w.dir.path().to_path_buf(),
            },
            config: Config::default(),
            seats: w.seats.clone(),
            seat_info,
            prices: PriceBook::builtin(),
            procs: ProcTable::empty(),
            painter: Painter::new(ColorChoice::Never),
            mask: false,
            now: at(now),
            login_labels: Default::default(),
            login_tiers: Default::default(),
            problems: Vec::new(),
        }
    }

    #[test]
    fn same_account_uuid_with_different_organizations_stays_separate() {
        let w = World::new(&["default", "work"]);
        let req =
            |id: &str, ts: &str, usd: u64| assistant(id, ts, M, "/w/app", (usd * 500_000, 0), &[]);
        w.append(
            "default",
            "s-team",
            &[
                owner_org("acc-same", "org-team"),
                req("t1", "2026-11-18T10:00:00Z", 10),
                req("t2", "2026-11-19T10:00:00Z", 10),
            ],
        );
        w.append(
            "work",
            "s-max",
            &[
                owner_org("acc-same", "org-max"),
                req("m1", "2026-11-18T10:00:00Z", 40),
                req("m2", "2026-11-19T10:00:00Z", 40),
            ],
        );
        let team = org_account(
            "acc-same",
            "org-team",
            "Acme Corp",
            "dev@example.com",
            Some("default_raven"),
            Some("default_claude_max_5x"),
        );
        let max = org_account(
            "acc-same",
            "org-max",
            "Personal",
            "dev@example.com",
            None,
            Some("default_claude_max_5x"),
        );
        assert_eq!(team.plan_label().as_deref(), Some("team"));
        assert_eq!(max.plan_label().as_deref(), Some("max_5x"));

        let resets = "2026-11-24T00:00:00Z";
        let infos = vec![
            SeatInfo {
                seat: w.id("default"),
                label: "default".into(),
                account: Some(team),
                utilization: Some(weekly(
                    "acc-same/org-team",
                    "2026-11-20T00:00:00Z",
                    20.0,
                    resets,
                )),
            },
            SeatInfo {
                seat: w.id("work"),
                label: "work".into(),
                account: Some(max),
                utilization: Some(weekly(
                    "acc-same/org-max",
                    "2026-11-20T01:00:00Z",
                    80.0,
                    resets,
                )),
            },
        ];

        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &infos, &PriceBook::builtin());

        let ctx = ctx_of(&w, infos, "2026-11-20T02:00:00Z");
        let reports = all_reports(&ctx.limits_inputs(&idx).unwrap(), &idx).unwrap();
        assert_eq!(reports.len(), 2, "two distinct accounts: {reports:?}");

        let r_max = reports
            .iter()
            .find(|r| r.account.as_str() == "acc-same/org-max")
            .unwrap();
        let r_team = reports
            .iter()
            .find(|r| r.account.as_str() == "acc-same/org-team")
            .unwrap();

        assert_eq!(r_team.tier.as_deref(), Some("default_raven"));
        assert_eq!(r_max.tier.as_deref(), Some("default_claude_max_5x"));
        assert_eq!(r_team.windows[0].reading.utilization, 20.0);
        assert_eq!(r_max.windows[0].reading.utilization, 80.0);
        assert!((r_team.windows[0].cost_in_window.get() - 20.0).abs() < 1e-6);
        assert!((r_max.windows[0].cost_in_window.get() - 80.0).abs() < 1e-6);
        // Team and Max 5x are different tiers: neither borrows the other's $/pt reference.
        assert_eq!(r_team.windows[0].reference, None);
        assert_eq!(r_max.windows[0].reference, None);

        // `--account` resolves by org uuid, org name, or full id; shared user uuid or email is ambiguous.
        assert_eq!(
            ctx.resolve_account(&idx, "org-team").unwrap().as_str(),
            "acc-same/org-team"
        );
        assert_eq!(
            ctx.resolve_account(&idx, "Acme Corp").unwrap().as_str(),
            "acc-same/org-team"
        );
        assert_eq!(
            ctx.resolve_account(&idx, "acc-same/org-max")
                .unwrap()
                .as_str(),
            "acc-same/org-max"
        );
        for shared in ["acc-same", "dev@example.com"] {
            let e = ctx.resolve_account(&idx, shared).unwrap_err();
            assert_eq!(
                crate::errors::code_of(&e),
                Some("ambiguous-account"),
                "{shared}: {e}"
            );
        }
    }

    #[test]
    fn same_account_uuid_and_same_organization_share_one_account() {
        let w = World::new(&["default", "work"]);
        let a = || {
            org_account(
                "acc-same",
                "org-shared",
                "Shared",
                "dev@example.com",
                None,
                Some("default_claude_max_5x"),
            )
        };
        let resets = "2026-11-24T00:00:00Z";
        let infos = vec![
            SeatInfo {
                seat: w.id("default"),
                label: "default".into(),
                account: Some(a()),
                utilization: Some(weekly(
                    "acc-same/org-shared",
                    "2026-11-20T00:00:00Z",
                    30.0,
                    resets,
                )),
            },
            SeatInfo {
                seat: w.id("work"),
                label: "work".into(),
                account: Some(a()),
                utilization: Some(weekly(
                    "acc-same/org-shared",
                    "2026-11-20T01:00:00Z",
                    35.0,
                    resets,
                )),
            },
        ];
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &infos, &PriceBook::builtin());
        let ctx = ctx_of(&w, infos, "2026-11-20T02:00:00Z");
        let reports = all_reports(&ctx.limits_inputs(&idx).unwrap(), &idx).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].account.as_str(), "acc-same/org-shared");
        assert_eq!(reports[0].windows[0].reading.utilization, 35.0);
    }

    #[test]
    fn single_org_legacy_index_adopts_bare_account_in_place() {
        let w = World::new(&["work"]);
        let req =
            |id: &str, ts: &str, usd: u64| assistant(id, ts, M, "/w/app", (usd * 500_000, 0), &[]);
        // Old transcripts: older one will be compacted into `usage_rollup`, recent one stays raw.
        w.append(
            "work",
            "s-old",
            &[owner("acc-1"), req("old1", "2026-10-01T10:00:00Z", 5)],
        );
        w.append(
            "work",
            "s-now",
            &[owner("acc-1"), req("now1", "2026-11-18T10:00:00Z", 12)],
        );
        let legacy_seat = SeatInfo {
            seat: w.id("work"),
            label: "work".into(),
            account: Some(Account {
                account_uuid: "acc-1".into(),
                email: Some("a@example.com".into()),
                profile_fetched_at: Some(at("2026-10-01T00:00:00Z")),
                ..Default::default()
            }),
            utilization: Some(weekly(
                "acc-1",
                "2026-11-19T00:00:00Z",
                25.0,
                "2026-11-24T00:00:00Z",
            )),
        };
        let book = PriceBook::builtin();
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[legacy_seat], &book);
        idx.compact(&CompactOpts {
            keep_raw_days: 14,
            now: at("2026-11-20T00:00:00Z"),
            prices: &book,
            seats: &w.seats,
            max_chunks: None,
        })
        .unwrap();
        assert_eq!(
            dump(&idx, "SELECT DISTINCT account_uuid FROM usage_rollup"),
            ["acc-1"]
        );

        // Upgrade: seat now carries `organizationUuid`.
        let upgraded = SeatInfo {
            seat: w.id("work"),
            label: "work".into(),
            account: Some(org_account(
                "acc-1",
                "org-1",
                "Solo",
                "a@example.com",
                None,
                Some("default_claude_max_20x"),
            )),
            utilization: Some(weekly(
                "acc-1/org-1",
                "2026-11-20T00:00:00Z",
                30.0,
                "2026-11-24T00:00:00Z",
            )),
        };
        let mut resolver = Resolver::new(vec![]);
        let stats = idx
            .refresh(
                &w.seats,
                std::slice::from_ref(&upgraded),
                &mut resolver,
                &book,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            stats.files_read, 0,
            "single-org adoption needs zero transcript re-reads"
        );

        for sql in [
            "SELECT DISTINCT account_uuid FROM requests",
            "SELECT DISTINCT account_uuid FROM usage_rollup",
            "SELECT DISTINCT account_uuid FROM owners",
            "SELECT DISTINCT account_uuid FROM seat_accounts",
            "SELECT DISTINCT account_uuid FROM accounts",
            "SELECT DISTINCT account_uuid FROM readings",
        ] {
            assert_eq!(dump(&idx, sql), ["acc-1/org-1"], "{sql}");
        }
        // Both the pre-upgrade reading (25%) and the post-upgrade reading (30%) are preserved.
        assert_eq!(
            dump(
                &idx,
                "SELECT CAST(utilization AS INTEGER) FROM readings WHERE account_uuid = 'acc-1/org-1' ORDER BY fetched_at"
            ),
            ["25", "30"]
        );
        // Spend across archive + raw stays intact ($5 + $12 = $17).
        let span = crate::core::period::Span {
            start: at("2026-09-01T00:00:00Z"),
            end: at("2026-12-01T00:00:00Z"),
        };
        let rows = idx
            .spend_by(Dim::Account, span, &Filter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "acc-1/org-1");
        assert!((rows[0].cost - 17.0).abs() < 1e-6, "{}", rows[0].cost);
    }

    #[test]
    fn multi_org_legacy_index_separates_mixed_accounts_cleanly() {
        let w = World::new(&["default", "work"]);
        let req =
            |id: &str, ts: &str, usd: u64| assistant(id, ts, M, "/w/app", (usd * 500_000, 0), &[]);
        // Real transcripts ALREADY had organizationUuid in `session-owner`, even though the older
        // swarmy ignored it and indexed everything under bare `acc-1`.
        w.append(
            "default",
            "s-def",
            &[
                owner_org("acc-1", "org-team"),
                req("d1", "2026-11-18T10:00:00Z", 15),
            ],
        );
        w.append(
            "work",
            "s-wrk",
            &[
                owner_org("acc-1", "org-max"),
                req("w1", "2026-11-18T11:00:00Z", 45),
            ],
        );
        let book = PriceBook::builtin();
        let mut idx = w.index("i.sqlite");
        // Simulate an older swarmy that stored bare `acc-1` everywhere and interleaved readings.
        let p_def = w.seats[0]
            .dir
            .join("projects/-w-proj/s-def.jsonl")
            .to_string_lossy()
            .into_owned();
        let p_wrk = w.seats[1]
            .dir
            .join("projects/-w-proj/s-wrk.jsonl")
            .to_string_lossy()
            .into_owned();
        let s_def = w.id("default");
        let s_wrk = w.id("work");
        w.refresh(&mut idx, &[], &book);
        // Rewrite the indexed rows back to bare `acc-1` as an older swarmy would have written them.
        idx.conn
            .execute_batch(&format!(
                "UPDATE requests SET account_uuid = 'acc-1';
                 UPDATE owners SET account_uuid = 'acc-1';
                 UPDATE files SET account_uuid = 'acc-1' WHERE path IN ('{p_def}', '{p_wrk}');
                 INSERT INTO seat_accounts VALUES ('{s_def}', 1000, 'acc-1'), ('{s_wrk}', 1000, 'acc-1');
                 INSERT INTO accounts(account_uuid, email, seat, seen_at) VALUES ('acc-1', 'dev@example.com', '{s_wrk}', 1000);
                 INSERT INTO readings VALUES
                   ('acc-1', 'seven_day', 1795000000000, 12.0, 1795478400000),
                   ('acc-1', 'seven_day', 1795003600000, 44.0, 1795478400000);"
            ))
            .unwrap();
        // Force a fresh open so `adopted_accounts` starts unset, as on a real upgrade.
        drop(idx);
        let mut idx = w.index("i.sqlite");

        let resets = "2026-11-24T00:00:00Z";
        let infos = vec![
            SeatInfo {
                seat: s_def.clone(),
                label: "default".into(),
                account: Some(org_account(
                    "acc-1",
                    "org-team",
                    "Acme Corp",
                    "dev@example.com",
                    Some("default_raven"),
                    Some("default_claude_max_5x"),
                )),
                utilization: Some(weekly(
                    "acc-1/org-team",
                    "2026-11-20T00:00:00Z",
                    18.0,
                    resets,
                )),
            },
            SeatInfo {
                seat: s_wrk.clone(),
                label: "work".into(),
                account: Some(org_account(
                    "acc-1",
                    "org-max",
                    "Personal",
                    "dev@example.com",
                    None,
                    Some("default_claude_max_5x"),
                )),
                utilization: Some(weekly(
                    "acc-1/org-max",
                    "2026-11-20T00:00:00Z",
                    52.0,
                    resets,
                )),
            },
        ];
        w.refresh(&mut idx, &infos, &book);

        // No bare `acc-1` ghost account remains anywhere.
        assert_eq!(
            dump(&idx, "SELECT account_uuid FROM accounts ORDER BY 1"),
            ["acc-1/org-max", "acc-1/org-team"]
        );
        assert_eq!(
            dump(
                &idx,
                "SELECT account_uuid, CAST(utilization AS INTEGER) FROM readings ORDER BY 1"
            ),
            ["acc-1/org-max|52", "acc-1/org-team|18"]
        );
        assert_eq!(
            dump(&idx, "SELECT msg_id, account_uuid FROM requests ORDER BY 1"),
            ["d1|acc-1/org-team", "w1|acc-1/org-max"]
        );
        assert_eq!(
            dump(
                &idx,
                "SELECT session_id, account_uuid FROM owners ORDER BY 1"
            ),
            ["s-def|acc-1/org-team", "s-wrk|acc-1/org-max"]
        );

        let ctx = ctx_of(&w, infos, "2026-11-20T01:00:00Z");
        let reports = all_reports(&ctx.limits_inputs(&idx).unwrap(), &idx).unwrap();
        assert_eq!(reports.len(), 2, "no ghost 3rd account: {reports:?}");
        assert_eq!(
            reports
                .iter()
                .find(|r| r.account.as_str() == "acc-1/org-team")
                .unwrap()
                .windows[0]
                .cost_in_window
                .get(),
            15.0
        );
        assert_eq!(
            reports
                .iter()
                .find(|r| r.account.as_str() == "acc-1/org-max")
                .unwrap()
                .windows[0]
                .cost_in_window
                .get(),
            45.0
        );
        let _ = Utc::now();
    }

    fn work_seat(w: &World, org: &str) -> SeatInfo {
        SeatInfo {
            seat: w.id("work"),
            label: "work".into(),
            account: Some(org_account(
                "acc-1",
                org,
                "Acme Corp",
                "dev@example.com",
                None,
                None,
            )),
            utilization: None,
        }
    }

    /// Review A1/A2: the seat runs one organization now, the user's other one is parked in the
    /// vault. The old index holds only bare `acc-1`; the parked organization must not be folded
    /// into the visible one, and the owners' re-attribution survives until the re-read is done.
    #[test]
    fn a_parked_second_organization_is_not_folded_into_the_visible_one() {
        let w = World::new(&["work"]);
        let req =
            |id: &str, ts: &str, usd: u64| assistant(id, ts, M, "/w/app", (usd * 500_000, 0), &[]);
        w.append(
            "work",
            "s-old",
            &[
                owner_org("acc-1", "org-max"),
                req("m1", "2026-11-10T10:00:00Z", 30),
            ],
        );
        w.append(
            "work",
            "s-new",
            &[
                owner_org("acc-1", "org-team"),
                req("t1", "2026-11-18T10:00:00Z", 10),
            ],
        );
        let book = PriceBook::builtin();
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &book);
        idx.conn
            .execute_batch(
                "UPDATE requests SET account_uuid = 'acc-1';
                 UPDATE owners SET account_uuid = 'acc-1';
                 UPDATE files SET account_uuid = 'acc-1';",
            )
            .unwrap();
        drop(idx);
        let mut idx = w.index("i.sqlite");
        idx.set_known_accounts([AccountId::of("acc-1", Some("org-max"))]);
        let infos = vec![work_seat(&w, "org-team")];
        // The migration alone (as if the pass stopped right after it): the flag is on disk.
        assert!(idx.adopt_accounts(&infos).unwrap());
        assert_eq!(
            idx.meta(super::super::accounts::OWNERS_PENDING)
                .unwrap()
                .as_deref(),
            Some("1")
        );
        // Review B-H5: the transcripts are marked for a re-read (`offset > size`), so a pass that
        // re-reads none of them (a hot refresh) keeps the flag. SQLite evaluates every `SET`
        // with the old row: `size = size + 1, offset = size + 1` left them equal.
        let scheduled = dump(&idx, "SELECT COUNT(*) FROM files WHERE offset > size");
        assert_eq!(scheduled, ["2"]);
        super::super::accounts::finish_reattribution(&idx.conn).unwrap();
        assert_eq!(
            idx.meta(super::super::accounts::OWNERS_PENDING)
                .unwrap()
                .as_deref(),
            Some("1")
        );
        drop(idx);
        // The transcript grows before it is read again (a live session): it is still read from
        // the start, not resumed at the mark.
        w.append("work", "s-old", &[req("m2", "2026-11-10T11:00:00Z", 5)]);
        // A later run (fresh process, nothing in memory) still re-attributes, then clears it.
        let mut idx = w.index("i.sqlite");
        idx.set_known_accounts([AccountId::of("acc-1", Some("org-max"))]);
        w.refresh(&mut idx, &infos, &book);
        assert_eq!(
            dump(&idx, "SELECT msg_id, account_uuid FROM requests ORDER BY 1"),
            ["m1|acc-1/org-max", "m2|acc-1/org-max", "t1|acc-1/org-team"]
        );
        assert_eq!(
            dump(&idx, "SELECT COUNT(*) FROM files WHERE offset > size"),
            ["0"]
        );
        assert_eq!(
            idx.meta(super::super::accounts::OWNERS_PENDING).unwrap(),
            None
        );
    }

    /// Review A3: an older `session-owner` without `organizationUuid` takes the seat's
    /// organization (as the status line does), so spend never lands on a bare id.
    #[test]
    fn an_owner_without_organization_takes_the_seats() {
        let w = World::new(&["work"]);
        let bare_owner = serde_json::json!({
            "type": "session-owner", "accountUuid": "acc-1", "sessionId": "s"
        })
        .to_string();
        w.append(
            "work",
            "s-1",
            &[
                bare_owner,
                assistant(
                    "r1",
                    "2026-11-18T10:00:00Z",
                    M,
                    "/w/app",
                    (1_000_000, 0),
                    &[],
                ),
            ],
        );
        let book = PriceBook::builtin();
        let mut idx = w.index("i.sqlite");
        let infos = vec![work_seat(&w, "org-team")];
        w.refresh(&mut idx, &infos, &book);
        assert_eq!(
            dump(&idx, "SELECT account_uuid FROM owners"),
            ["acc-1/org-team"]
        );
        assert_eq!(
            dump(&idx, "SELECT msg_id, account_uuid FROM requests"),
            ["r1|acc-1/org-team"]
        );
    }
}

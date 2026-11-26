//! Scenario tests of seat identity (review H3): rows are keyed by the seat's id, so a new label
//! changes no attribution and no price, and an index an older swarmy keyed by labels moves to ids
//! in place without losing a row.

#[cfg(test)]
mod tests {
    use crate::claude::seats::Seat;
    use crate::core::pricing::PriceBook;
    use crate::index::query::{Dim, Filter};
    use crate::index::scenarios::{World, account, assistant, at, dump, owner};
    use crate::index::{Index, SeatInfo};

    const M: &str = "claude-sonnet-5-5";

    fn info(label: &str, acc: &str, since: &str) -> SeatInfo {
        SeatInfo {
            seat: Default::default(),
            label: label.into(),
            account: Some(account(acc, since)),
            switched_at: None,
            utilization: None,
        }
    }

    fn all() -> crate::core::period::Span {
        crate::core::period::Span {
            start: at("2026-01-01T00:00:00Z"),
            end: at("2026-12-01T00:00:00Z"),
        }
    }

    /// Every table that keys rows by seat, as text, for comparing two indexes.
    const KEYED: [&str; 7] = [
        "SELECT msg_id, seat, session_id, account_uuid FROM requests ORDER BY 1",
        "SELECT seat, session_id, first_ts FROM sessions ORDER BY 1, 2",
        "SELECT seat, session_id, agent_id FROM agents ORDER BY 1, 2, 3",
        "SELECT seat, ts, account_uuid FROM seat_accounts ORDER BY 1, 2",
        "SELECT seat, session_id, ts FROM owners ORDER BY 1, 2, 3",
        "SELECT path, seat FROM files ORDER BY 1",
        "SELECT account_uuid, seat FROM accounts ORDER BY 1",
    ];

    fn keyed(idx: &Index) -> Vec<Vec<String>> {
        KEYED.iter().map(|q| dump(idx, q)).collect()
    }

    fn session(w: &World, seat: &str, sid: &str, msgs: &[(&str, &str)]) {
        let mut lines = vec![owner("acc-a")];
        lines.extend(
            msgs.iter()
                .map(|(id, ts)| assistant(id, ts, M, "/w/shop", (1000, 0), &[])),
        );
        w.append(seat, sid, &lines);
    }

    #[test]
    fn a_new_label_changes_no_attribution_and_no_price() {
        let mut w = World::new(&["work"]);
        session(
            &w,
            "work",
            "s1",
            &[
                ("r1", "2026-11-10T10:00:00Z"),
                ("r2", "2026-11-10T12:00:00Z"),
            ],
        );
        // The seat reports acc-b; its session's owner records say acc-a, and they decide.
        let swap = info("work", "acc-b", "2026-11-10T11:00:00Z");
        let mut idx = w.index("i.sqlite");
        let mut book = PriceBook::builtin();
        book.merge_claude_code_cache(
            &w.id("work"),
            &serde_json::json!({"claude-new-9": {"inputTokens": 7, "outputTokens": 35}}),
        );
        w.refresh(&mut idx, &[swap], &book);
        let before = keyed(&idx);
        let fp = idx.meta("price_fp").unwrap();
        assert!(fp.is_some());
        let id = w.id("work");
        assert!(
            before[0].iter().all(|r| r.contains(id.as_str())),
            "{before:?}"
        );

        // The same dir gets another label (`label = "job"` in config.toml, or another dir named
        // like it found first).
        w.seats[0].label = "job".into();
        w.refresh(
            &mut idx,
            &[info("job", "acc-b", "2026-11-10T11:00:00Z")],
            &book,
        );
        assert_eq!(w.id("job"), id, "the id comes from the dir");
        assert_eq!(keyed(&idx), before, "no row moved");
        assert_eq!(idx.meta("price_fp").unwrap(), fp, "nothing was repriced");
        // New requests of the dir follow the same login history.
        w.append(
            "job",
            "s1",
            &[assistant(
                "r3",
                "2026-11-10T13:00:00Z",
                M,
                "/w/shop",
                (1000, 0),
                &[],
            )],
        );
        w.refresh(
            &mut idx,
            &[info("job", "acc-b", "2026-11-10T11:00:00Z")],
            &book,
        );
        assert_eq!(
            dump(&idx, "SELECT msg_id, account_uuid FROM requests ORDER BY 1"),
            ["r1|acc-a", "r2|acc-a", "r3|acc-a"]
        );
        assert_eq!(
            dump(&idx, "SELECT COUNT(*) FROM seat_accounts"),
            ["1"],
            "one login change, still"
        );
        // Reports show the current label; a filter by the seat's id finds every request.
        let rows = idx.spend_by(Dim::Seat, all(), &Filter::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].key.as_str(), rows[0].label.as_str()),
            (id.as_str(), "job")
        );
        let f = Filter {
            seat: Some(id.clone()),
            ..Default::default()
        };
        assert_eq!(idx.total(all(), &f).unwrap().requests, 3);
        // The fingerprint never depended on which seat carried Claude Code's prices.
        let mut other = PriceBook::builtin();
        other.merge_claude_code_cache(
            &"another-seat".into(),
            &serde_json::json!({"claude-new-9": {"inputTokens": 7, "outputTokens": 35}}),
        );
        assert_eq!(other.fingerprint(), book.fingerprint());
    }

    #[test]
    fn an_index_keyed_by_labels_moves_to_seat_ids_in_place() {
        let mut w = World::new(&["work", "home"]);
        session(
            &w,
            "work",
            "s1",
            &[
                ("r1", "2026-11-10T10:00:00Z"),
                ("r2", "2026-11-10T12:00:00Z"),
            ],
        );
        session(&w, "home", "s2", &[("r3", "2026-11-10T10:30:00Z")]);
        let infos = [
            info("work", "acc-b", "2026-11-10T11:00:00Z"),
            info("home", "acc-c", "2026-11-01T00:00:00Z"),
        ];
        // The reference: an index that always had ids.
        let mut reference = w.index("ref.sqlite");
        w.refresh(&mut reference, &infos, &PriceBook::builtin());
        let want = keyed(&reference);
        drop(reference);

        // The same index as an older swarmy left it: every `seat` holds the label, facts version
        // 1, no `seats` table; plus the history of a seat that is not around now ("old").
        let path = w.dir.path().join("old.sqlite");
        std::fs::copy(w.dir.path().join("ref.sqlite"), &path).unwrap();
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            for t in crate::index::seats::seat_tables(&c).unwrap() {
                for label in ["work", "home"] {
                    c.execute(
                        &format!("UPDATE {t} SET seat = ?1 WHERE seat = ?2"),
                        rusqlite::params![label, w.id(label)],
                    )
                    .unwrap();
                }
            }
            c.execute_batch(
                "DROP TABLE seats;
                 UPDATE meta SET value = '1' WHERE key = 'facts_version';
                 INSERT INTO seat_accounts VALUES ('old', 5, 'acc-z');
                 INSERT INTO sessions(seat, session_id, first_ts) VALUES ('old', 's9', 7);",
            )
            .unwrap();
        }
        let labelled =
            |idx: &Index| dump(idx, "SELECT seat, label, dir IS NULL FROM seats ORDER BY 1");
        let mut idx = Index::open(&path).unwrap();
        assert_eq!(
            labelled(&idx),
            ["home|home|1", "old|old|1", "work|work|1"],
            "the labels wait for their seats"
        );
        let counts = |idx: &Index| -> Vec<String> {
            [
                "requests",
                "sessions",
                "agents",
                "seat_accounts",
                "owners",
                "files",
                "accounts",
            ]
            .iter()
            .map(|t| dump(idx, &format!("SELECT COUNT(*) FROM {t}"))[0].clone())
            .collect()
        };
        let before = counts(&idx);

        // The first refresh with today's seats moves their rows to the ids; nothing is lost.
        w.refresh(&mut idx, &infos, &PriceBook::builtin());
        assert_eq!(counts(&idx), before, "no row lost");
        let got = keyed(&idx);
        for (i, (g, r)) in got.iter().zip(&want).enumerate() {
            let g: Vec<&String> = g.iter().filter(|x| !x.starts_with("old|")).collect();
            let r: Vec<&String> = r.iter().collect();
            assert_eq!(g, r, "{}", KEYED[i]);
        }
        assert_eq!(
            dump(
                &idx,
                "SELECT seat, session_id FROM sessions WHERE seat = 'old'"
            ),
            ["old|s9"],
            "a label no seat carries stays as it was"
        );
        let rows = idx.spend_by(Dim::Seat, all(), &Filter::default()).unwrap();
        let mut labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        labels.sort();
        assert_eq!(labels, ["home", "work"]);

        // The seat comes back (found through a running `claude`, say): its history follows it.
        let dir = w.dir.path().join("claude-old");
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        w.seats.push(Seat::new(
            "old",
            dir.clone(),
            dir.join(".claude.json"),
            false,
        ));
        w.refresh(&mut idx, &infos, &PriceBook::builtin());
        let old = w.id("old");
        assert_eq!(
            dump(
                &idx,
                "SELECT seat, ts, account_uuid FROM seat_accounts WHERE account_uuid = 'acc-z'"
            ),
            [format!("{old}|5|acc-z")]
        );
        assert_eq!(
            dump(&idx, "SELECT COUNT(*) FROM seats WHERE dir IS NULL"),
            ["0"],
            "every label found its seat"
        );
        assert_eq!(counts(&idx), before);
    }

    /// Issue #8: a seat counts as used once the index has read a transcript of it, and stays so
    /// after the transcript is gone (its sessions outlive it); a seat never read is not named.
    #[test]
    fn seats_with_transcripts_names_the_seats_ever_read() {
        let w = World::new(&["work", "empty"]);
        session(&w, "work", "s1", &[("r1", "2026-11-10T10:00:00Z")]);
        let mut idx = w.index("i.sqlite");
        let empty = SeatInfo {
            seat: Default::default(),
            label: "empty".into(),
            account: None,
            switched_at: None,
            utilization: None,
        };
        w.refresh(
            &mut idx,
            &[info("work", "acc-a", "2026-11-01T00:00:00Z"), empty],
            &PriceBook::builtin(),
        );
        let read = idx.seats_with_transcripts().unwrap();
        assert_eq!(read.into_iter().collect::<Vec<_>>(), [w.id("work")]);
        // Claude Code's retention removed the transcript; the compactor dropped its file row.
        idx.conn.execute("DELETE FROM files", []).unwrap();
        let read = idx.seats_with_transcripts().unwrap();
        assert_eq!(read.into_iter().collect::<Vec<_>>(), [w.id("work")]);
    }
}

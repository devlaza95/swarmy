//! What is known of each seat's use (`core::seat_use`), gathered once for every front end: the
//! dashboard, `swarmy accounts` and `swarmy doctor` judge empty seats by the same facts.

use std::collections::BTreeMap;

use super::context::Ctx;
use crate::claude::seats::{Seat, SeatId};
use crate::core::seat_use::SeatUse;
use crate::index::Index;
use crate::vault::SeatState;

impl Ctx {
    /// The use of each of `self.seats`, in their order: `states` is what the vault read of each
    /// seat's login, `live` counts a seat's running sessions.
    pub fn seat_uses(
        &self,
        idx: &Index,
        states: &BTreeMap<SeatId, SeatState>,
        live: impl Fn(&Seat) -> usize,
    ) -> anyhow::Result<Vec<SeatUse>> {
        let read = idx.seats_with_transcripts()?;
        Ok(self
            .seats
            .iter()
            .zip(&self.seat_info)
            .map(|(s, si)| {
                let state = states.get(&s.id);
                SeatUse {
                    login: state
                        .is_some_and(|st| st.fingerprint.is_some() || st.saved_as.is_some()),
                    account: si.account.is_some(),
                    live: live(s),
                    transcripts: read.contains(&s.id),
                    configured: s.configured(),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Dirs};
    use crate::core::pricing::PriceBook;
    use crate::index::SeatInfo;
    use crate::index::scenarios::{World, account, assistant, at, owner};
    use crate::procs::ProcTable;
    use crate::ui::style::{ColorChoice, Painter};

    fn info(label: &str, acc: Option<&str>) -> SeatInfo {
        SeatInfo {
            seat: Default::default(),
            label: label.into(),
            account: acc.map(|a| account(a, "2026-11-01T00:00:00Z")),
            switched_at: None,
            utilization: None,
        }
    }

    /// Issue #8: each fact keeps a seat in use; a seat with none of them is empty.
    #[test]
    fn every_front_end_judges_a_seat_by_the_same_facts() {
        let mut w = World::new(&["used", "empty", "listed", "running", "signed", "known"]);
        w.append(
            "used",
            "s1",
            &[
                owner("acc-a"),
                assistant(
                    "r1",
                    "2026-11-10T10:00:00Z",
                    "claude-sonnet-5-5",
                    "/w/shop",
                    (10, 1),
                    &[],
                ),
            ],
        );
        // `[[seat]]` of config.toml named it; nothing else is there.
        w.seats[2].found_by = vec!["config.toml"];
        let mut infos: Vec<SeatInfo> = w.seats.iter().map(|s| info(&s.label, None)).collect();
        infos[5] = info("known", Some("acc-k"));
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &infos, &PriceBook::builtin());
        for (i, s) in infos.iter_mut().zip(&w.seats) {
            i.seat = s.id.clone();
        }
        let ctx = Ctx {
            dirs: Dirs {
                data: w.dir.path().join("data"),
                config: w.dir.path().join("config"),
                home: w.dir.path().to_path_buf(),
            },
            config: Config::default(),
            seats: w.seats.clone(),
            seat_info: infos,
            prices: PriceBook::builtin(),
            procs: ProcTable::empty(),
            painter: Painter::new(ColorChoice::Never),
            mask: false,
            now: at("2026-11-20T00:00:00Z"),
            login_labels: Default::default(),
            login_tiers: Default::default(),
            problems: Vec::new(),
        };
        let mut states = BTreeMap::new();
        states.insert(
            w.id("signed"),
            SeatState {
                fingerprint: Some("f1".into()),
                ..Default::default()
            },
        );
        let running = w.id("running");
        let uses = ctx
            .seat_uses(&idx, &states, |s| usize::from(s.id == running))
            .unwrap();
        let empty: Vec<(&str, bool)> = w
            .seats
            .iter()
            .zip(&uses)
            .map(|(s, u)| (s.label.as_str(), u.is_empty()))
            .collect();
        assert_eq!(
            empty,
            [
                ("used", false),
                ("empty", true),
                ("listed", false),
                ("running", false),
                ("signed", false),
                ("known", false)
            ]
        );
    }
}

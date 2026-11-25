//! The rows of LIMITS · SEATS: sections per harness, the empty ones left out (`core::seat_use`),
//! and the selection kept on its row while the rows change.

use super::*;
use crate::core::seat_use::hides_harness;

impl App {
    /// Rows of the LIMITS · SEATS panel, one section per harness (in `Harness::ALL` order): its
    /// seats, then every saved login of that harness, in a seat or not, the most limit left
    /// first. A login fits only its harness's seats, so it is listed with them. A harness of
    /// [`App::hidden_harnesses`] is left out unless `.` shows it.
    pub fn limits_rows(&self) -> Vec<LimitsRow> {
        let hidden = if self.show_hidden {
            Vec::new()
        } else {
            self.hidden_harnesses()
        };
        let mut rows = Vec::new();
        for h in Harness::ALL {
            if hidden.iter().any(|(x, _)| *x == h) {
                continue;
            }
            rows.extend(
                self.seats
                    .iter()
                    .filter(|s| s.harness == h)
                    .map(|s| LimitsRow::Seat(s.label.clone())),
            );
            let mut logins: Vec<&crate::vault::Login> = self
                .vault
                .logins
                .iter()
                .filter(|l| l.harness == h)
                .collect();
            logins.sort_by(|a, b| self.login_pressure(a).total_cmp(&self.login_pressure(b)));
            rows.extend(
                logins
                    .into_iter()
                    .map(|l| LimitsRow::Login(l.id.to_string())),
            );
        }
        rows
    }

    /// Harnesses whose section is left out (whether `.` shows them now or not), with their
    /// seats: every seat empty and no saved login of the harness. A seat the analytics have not
    /// judged yet counts as in use, so nothing is hidden before the first pass.
    pub fn hidden_harnesses(&self) -> Vec<(Harness, Vec<&Seat>)> {
        Harness::ALL
            .into_iter()
            .filter_map(|h| {
                let seats: Vec<&Seat> = self.seats.iter().filter(|s| s.harness == h).collect();
                let empty: Vec<bool> = seats
                    .iter()
                    .map(|s| {
                        self.analytics
                            .seats
                            .iter()
                            .find(|r| r.id == s.id)
                            .is_some_and(|r| r.empty)
                    })
                    .collect();
                let saved = self.vault.logins.iter().filter(|l| l.harness == h).count();
                hides_harness(&empty, saved).then_some((h, seats))
            })
            .collect()
    }

    /// The harness a row of LIMITS · SEATS belongs to.
    pub fn row_harness(&self, r: &LimitsRow) -> Harness {
        match r {
            LimitsRow::Seat(label) => self
                .seats
                .iter()
                .find(|s| &s.label == label)
                .map_or(Harness::Claude, |s| s.harness),
            LimitsRow::Login(id) => self
                .vault
                .logins
                .iter()
                .find(|l| l.id.as_str() == id)
                .map_or(Harness::Claude, |l| l.harness),
        }
    }

    /// Whether LIMITS · SEATS shows one section per harness: when the rows shown hold seats or
    /// logins of more than one harness (a single-harness setup has no section headings).
    pub fn limits_sections(&self) -> bool {
        let rows = self.limits_rows();
        rows.iter()
            .map(|r| self.row_harness(r))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1
    }

    /// `.`: shows or hides the sections of empty seats.
    pub(super) fn toggle_hidden(&mut self) {
        if self.hidden_harnesses().is_empty() {
            self.toast("no empty seats to hide", Tone::Info);
            return;
        }
        self.keep_limits_row(|app| app.show_hidden = !app.show_hidden);
        self.toast(
            if self.show_hidden {
                "LIMITS · SEATS: empty seats shown (. hides them)"
            } else {
                "LIMITS · SEATS: empty seats hidden (. shows them)"
            },
            Tone::Info,
        );
    }

    /// Applies `change` and keeps the LIMITS · SEATS cursor on the row it was on (by key); when
    /// that row is gone, on the same place, within the rows left.
    pub(super) fn keep_limits_row(&mut self, change: impl FnOnce(&mut App)) {
        let before = self.limits_rows().get(self.cur(Panel::Limits)).cloned();
        change(self);
        let rows = self.limits_rows();
        let at = before
            .and_then(|b| rows.iter().position(|r| *r == b))
            .unwrap_or_else(|| self.cur(Panel::Limits).min(rows.len().saturating_sub(1)));
        self.set_cur(Panel::Limits, at);
    }
}

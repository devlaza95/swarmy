//! The vault, operated inside the LIMITS · SEATS panel: per harness, its seats, then every saved
//! login of that harness (one login may be in several seats). Enter on a seat opens a picker of
//! that harness's logins sorted by limit left (the best one first); Enter on a login picks a seat
//! for it. `S` puts the best login into the selected seat.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::app::{App, LimitsRow, Panel, Tone};
use crate::claude::seats::Seat;
use crate::core::harness::Harness;
use crate::vault::{Login, Vault};

/// Seats are named by label, saved logins by id (a label names one login per harness).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Capture(String),
    Rename(String),
    Add(Harness),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    Switch { seat: String, login: String },
    Remove(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    Input {
        title: String,
        value: String,
        action: InputAction,
    },
    Confirm {
        text: String,
        action: ConfirmAction,
    },
}

/// What a picker chooses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickFor {
    /// A login for this seat.
    Seat(String),
    /// A seat for this login.
    Login(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub what: PickFor,
    /// Login ids (for a seat) or seat labels (for a login), best first.
    pub items: Vec<String>,
    pub sel: usize,
}

/// Work that needs the real terminal (runs `claude` or `agy` for a new login).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum External {
    AddLogin(String, Harness),
}

#[derive(Debug, Default)]
pub struct VaultUi {
    pub logins: Vec<Login>,
    pub loaded: bool,
    pub picker: Option<Picker>,
    pub modal: Option<Modal>,
    pub external: Option<External>,
}

impl App {
    /// The seats whose logins the vault keeps.
    fn vault_seats(&self) -> Vec<Seat> {
        self.seats
            .iter()
            .filter(|s| s.harness.supports_vault())
            .cloned()
            .collect()
    }

    fn seat_named(&self, label: &str) -> Option<&Seat> {
        self.seats.iter().find(|s| s.label == label)
    }

    /// The saved login with this id.
    pub fn login_by_id(&self, id: &str) -> Option<&Login> {
        self.vault.logins.iter().find(|l| l.id.as_str() == id)
    }

    /// The label of the saved login `id` (the id itself when it is gone).
    pub fn login_label(&self, id: &str) -> String {
        self.login_by_id(id)
            .map_or_else(|| id.to_string(), |l| l.label.clone())
    }

    /// The id of the saved login `key` (an id, a label or an email) that fits `seat`'s harness.
    fn login_for_seat(&self, seat: &str, key: &str) -> Option<String> {
        let h = self.seat_named(seat).map(|s| s.harness);
        self.vault
            .logins
            .iter()
            .filter(|l| h.is_none_or(|h| l.harness == h))
            .find(|l| l.id.as_str() == key || l.label == key || l.email.as_deref() == Some(key))
            .map(|l| l.id.to_string())
    }

    pub fn vault_reload(&mut self) {
        let vault_seats = self.vault_seats();
        match Vault::open(&self.vault_dir).and_then(|mut v| {
            v.edit(|tx| tx.sync(&vault_seats))?;
            Ok(v.logins().to_vec())
        }) {
            Ok(l) => {
                self.vault.logins = l;
                self.vault.loaded = true;
            }
            Err(e) => self.toast(format!("vault: {e:#}"), Tone::Bad),
        }
    }

    /// Live sessions in the seat labelled `seat` (labels are unique within one run).
    fn live_in(&self, seat: &str) -> usize {
        let Some(id) = self
            .analytics
            .seats
            .iter()
            .find(|s| s.label == seat)
            .map(|s| &s.id)
        else {
            return 0;
        };
        self.live.iter().filter(|l| &l.seat == id).count()
    }

    /// Limit used by a login's account: the larger of the 5-hour and weekly estimates (unknown
    /// sorts last).
    pub fn login_pressure(&self, l: &Login) -> f64 {
        let (five, week, _) = self.limits_of(l.account_uuid.as_deref());
        match (five, week) {
            (None, None) => 1_000.0,
            (f, w) => f.unwrap_or(0.0).max(w.unwrap_or(0.0)),
        }
    }

    /// Logins that could go into `seat`: every saved login of its harness except the one it
    /// holds (a login in other seats too: one grant may serve several), the most limit left
    /// first.
    pub fn candidates_for(&self, seat: &str) -> Vec<String> {
        let Some(s) = self.seat_named(seat) else {
            return Vec::new();
        };
        let mut v: Vec<&Login> = self
            .vault
            .logins
            .iter()
            .filter(|l| l.harness == s.harness && !l.is_in(&s.id))
            .collect();
        v.sort_by(|a, b| self.login_pressure(a).total_cmp(&self.login_pressure(b)));
        v.into_iter().map(|l| l.id.to_string()).collect()
    }

    /// The login `S` puts into `seat`: the most limit left among the candidates, not of the
    /// account the seat already runs (another login of it shares the same limit).
    fn best_for(&self, seat: &str) -> Option<String> {
        let current = self
            .analytics
            .seats
            .iter()
            .find(|s| s.label == seat)
            .and_then(|s| s.account.clone());
        self.candidates_for(seat).into_iter().find(|id| {
            let acct = self.login_by_id(id).and_then(|l| l.account_uuid.clone());
            current.is_none() || acct != current
        })
    }

    /// Seats a login could go into: its harness's seats (those that hold it are marked by the
    /// picker).
    pub fn seats_for(&self, login: &str) -> Vec<String> {
        let h = self
            .login_by_id(login)
            .map_or(Harness::Claude, |l| l.harness);
        self.seats
            .iter()
            .filter(|s| s.harness == h && s.harness.supports_vault())
            .map(|s| s.label.clone())
            .collect()
    }

    /// Says why the vault cannot work with `h`'s logins here (Claude Code on macOS), before
    /// anything is tried; true when it cannot.
    fn vault_blocked(&mut self, h: Harness) -> bool {
        match crate::vault::unavailable(h) {
            Some(e) => {
                self.toast(e.to_string(), Tone::Warn);
                true
            }
            None => false,
        }
    }

    fn selected_row(&self) -> Option<LimitsRow> {
        self.limits_rows().get(self.cur(Panel::Limits)).cloned()
    }

    /// Asks for confirmation (always naming what is saved first), then switches. `login` is an
    /// id, or a label or email typed in `:switch`.
    pub fn request_switch(&mut self, seat: String, login: String) {
        self.vault.picker = None;
        let Some(login) = self.login_for_seat(&seat, &login) else {
            let h = self
                .seat_named(&seat)
                .map_or("this harness", |s| s.harness.display_name());
            self.toast(
                format!("no saved {h} login '{login}' for seat '{seat}'"),
                Tone::Warn,
            );
            return;
        };
        let label = self.login_label(&login);
        // Codex: nothing may run in this seat, nor in the one the login moves out of.
        if harness_of_seat(self, &seat) == Some(Harness::Codex) {
            let holders: Vec<String> = self
                .login_by_id(&login)
                .map(|l| {
                    self.seats
                        .iter()
                        .filter(|s| l.is_in(&s.id) && s.label != seat)
                        .map(|s| s.label.clone())
                        .collect()
                })
                .unwrap_or_default();
            if let Some(busy) = std::iter::once(seat.clone())
                .chain(holders)
                .find(|s| self.live_in(s) > 0)
            {
                self.toast(
                    format!(
                        "Codex is running in seat '{busy}': quit it first (it would write its old login back; OpenAI refresh tokens are single-use)"
                    ),
                    Tone::Warn,
                );
                return;
            }
        }
        let live = self.live_in(&seat);
        let current = self.analytics.seats.iter().find(|s| s.label == seat);
        let keeps = match current {
            Some(s) if s.login.is_some() => format!(
                "Its current login '{}' stays in the vault.",
                s.login.clone().unwrap_or_default()
            ),
            Some(s) if s.state.fingerprint.is_some() => {
                format!("Its current login is not saved yet: it is saved first, as '{seat}'.")
            }
            _ => String::new(),
        };
        let harness = self
            .seat_named(&seat)
            .map_or(Harness::Claude, |s| s.harness);
        let plural = if live == 1 { "" } else { "s" };
        let running = match (live, harness) {
            (0, _) => String::new(),
            (_, Harness::Claude) => format!(
                "\n\nClaude Code runs there ({live} session{plural}): it picks up the new login on its next request; restart it to refresh the account it shows."
            ),
            (_, Harness::Agy) => format!(
                "\n\nagy runs there ({live} session{plural}): it keeps its current login until restarted (it does not write the old one back)."
            ),
            // Refused before the dialog (above): a running Codex would write its old login back.
            (_, Harness::Codex) => String::new(),
        };
        let others: Vec<String> = self
            .login_by_id(&login)
            .map(|l| {
                l.mounted
                    .iter()
                    .filter_map(|id| self.seats.iter().find(|s| &s.id == id))
                    .filter(|s| s.label != seat)
                    .map(|s| s.label.clone())
                    .collect()
            })
            .unwrap_or_default();
        let shared = match (others.is_empty(), harness.shares_logins()) {
            (true, _) => String::new(),
            (false, true) => format!(
                "\n\n'{label}' stays in seat {} too: both seats use one login (and one limit).",
                others.join(", ")
            ),
            // A Codex login moves: its refresh token is single-use.
            (false, false) => format!(
                "\n\n'{label}' moves out of seat {} (a {} login can be in one seat only): that seat takes this seat's login, or none.",
                others.join(", "),
                harness.display_name()
            ),
        };
        self.vault.modal = Some(Modal::Confirm {
            text: format!(
                "Put the saved login '{label}' into seat '{seat}'?\n\n{keeps}{shared}{running}\n\n[y] switch   [n] cancel"
            ),
            action: ConfirmAction::Switch { seat, login },
        });
    }

    fn do_switch(&mut self, seat: &str, login: &str) {
        let vault_seats = self.vault_seats();
        let res = Vault::open(&self.vault_dir).and_then(|mut v| {
            v.edit(|tx| {
                tx.sync(&vault_seats)?;
                let s = vault_seats
                    .iter()
                    .find(|s| s.label == seat)
                    .ok_or_else(|| crate::errors::LookupError::NoSeat {
                        name: seat.to_string(),
                        known: Vec::new(),
                    })?;
                tx.switch_moving(s, login, &vault_seats)
            })
        });
        match res {
            Ok(r) if r.unchanged => self.toast(
                format!("seat '{}' already uses '{}'", r.seat, r.to),
                Tone::Info,
            ),
            Ok(r) => {
                if let Some(saved) = &r.auto_saved {
                    self.log_event(
                        "⚿",
                        format!("seat {}'s previous login saved as '{saved}'", r.seat),
                        Tone::Good,
                    );
                }
                self.log_event(
                    "⚿",
                    format!(
                        "seat {} → {} (was {})",
                        r.seat,
                        r.to,
                        r.from.as_deref().unwrap_or("-")
                    ),
                    Tone::Accent,
                );
                self.alerts
                    .insert("vault".into(), std::time::Instant::now());
                let also = if r.shared_with.is_empty() {
                    String::new()
                } else {
                    format!(" (also in {})", r.shared_with.join(", "))
                };
                for (o, now) in &r.moved {
                    self.log_event(
                        "⚿",
                        format!(
                            "seat {o} → {} ('{}' moved out: one seat only)",
                            now.as_deref().unwrap_or("no login"),
                            r.to
                        ),
                        Tone::Accent,
                    );
                }
                self.toast(
                    format!("seat '{}' now uses '{}'{also}", r.seat, r.to),
                    Tone::Good,
                );
            }
            Err(e) => self.toast(format!("{e:#}"), Tone::Bad),
        }
        self.vault_reload();
        self.request_analytics();
    }

    fn do_capture(&mut self, seat: &str, label: &str) {
        let label = label.trim();
        let vault_seats = self.vault_seats();
        let res = Vault::open(&self.vault_dir).and_then(|mut v| {
            v.edit(|tx| {
                tx.sync(&vault_seats)?;
                let s = vault_seats
                    .iter()
                    .find(|s| s.label == seat)
                    .ok_or_else(|| crate::errors::LookupError::NoSeat {
                        name: seat.to_string(),
                        known: Vec::new(),
                    })?;
                tx.capture(s, (!label.is_empty()).then_some(label))?
                    .ok_or_else(|| anyhow::anyhow!("seat '{seat}' has no login"))
            })
        });
        match res {
            Ok(_) => {
                self.log_event("⚿", format!("saved the login of seat {seat}"), Tone::Accent);
                self.toast(format!("saved the login of seat '{seat}'"), Tone::Good);
            }
            Err(e) => self.toast(format!("{e:#}"), Tone::Bad),
        }
        self.vault_reload();
        self.request_analytics();
    }

    /// Keys of the LIMITS · SEATS panel and of its dialogs. Returns true when the key was used.
    pub fn vault_key(&mut self, k: KeyEvent) -> bool {
        if !self.vault.loaded {
            self.vault_reload();
        }
        if let Some(m) = self.vault.modal.clone() {
            self.modal_key(m, k);
            return true;
        }
        if let Some(mut p) = self.vault.picker.clone() {
            match k.code {
                KeyCode::Esc => self.vault.picker = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    p.sel = (p.sel + 1).min(p.items.len().saturating_sub(1));
                    self.vault.picker = Some(p);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    p.sel = p.sel.saturating_sub(1);
                    self.vault.picker = Some(p);
                }
                KeyCode::Enter => {
                    if let Some(item) = p.items.get(p.sel).cloned() {
                        match p.what {
                            PickFor::Seat(seat) => self.request_switch(seat, item),
                            PickFor::Login(login) => self.request_switch(item, login),
                        }
                    }
                }
                _ => {}
            }
            return true;
        }
        let rows = self.limits_rows();
        let cur = self.cur(Panel::Limits).min(rows.len().saturating_sub(1));
        // Keys that read or write a seat's login, refused up front where the vault cannot.
        let row_h = self.selected_row().map(|r| self.row_harness(&r));
        let touches_login = matches!(
            k.code,
            KeyCode::Enter | KeyCode::Char('S') | KeyCode::Char('c')
        );
        if touches_login
            && let Some(h) = row_h
            && self.vault_blocked(h)
        {
            return true;
        }
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor
                    .insert(Panel::Limits, (cur + 1).min(rows.len().saturating_sub(1)));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor.insert(Panel::Limits, cur.saturating_sub(1));
            }
            KeyCode::Enter => match self.selected_row() {
                Some(LimitsRow::Seat(seat)) => {
                    let items = self.candidates_for(&seat);
                    if items.is_empty() {
                        self.toast(
                            "no other saved login for this seat's harness: c saves its login, a adds one",
                            Tone::Warn,
                        );
                    } else {
                        self.vault.picker = Some(Picker {
                            what: PickFor::Seat(seat),
                            items,
                            sel: 0,
                        });
                    }
                }
                Some(LimitsRow::Login(login)) => {
                    let items = self.seats_for(&login);
                    if items.is_empty() {
                        self.toast("no seat of this login's harness", Tone::Warn);
                    } else {
                        self.vault.picker = Some(Picker {
                            what: PickFor::Login(login),
                            items,
                            sel: 0,
                        });
                    }
                }
                None => {}
            },
            KeyCode::Char('S') => match self.selected_row() {
                Some(LimitsRow::Seat(seat)) => match self.best_for(&seat) {
                    Some(best) => self.request_switch(seat, best),
                    None => {
                        self.toast("no saved login of another account to switch to", Tone::Warn)
                    }
                },
                Some(LimitsRow::Login(_)) | None => {}
            },
            KeyCode::Char('c') => match self.selected_row() {
                Some(LimitsRow::Seat(s)) => {
                    self.vault.modal = Some(Modal::Input {
                        title: format!("Save the login of seat '{s}' as"),
                        value: s.clone(),
                        action: InputAction::Capture(s),
                    });
                }
                Some(LimitsRow::Login(_)) | None => {
                    self.toast("select a seat to save its login", Tone::Info);
                }
            },
            KeyCode::Char('a') => {
                // A login for the harness of the selected row.
                let h = self
                    .selected_row()
                    .map_or(Harness::Claude, |r| self.row_harness(&r));
                if self.vault_blocked(h) {
                    return true;
                }
                let (program, hint) = crate::vault::new_login_recipe(h);
                self.vault.modal = Some(Modal::Input {
                    title: format!(
                        "Label for the new {} login (`{program}` opens: {hint})",
                        h.display_name()
                    ),
                    value: String::new(),
                    action: InputAction::Add(h),
                });
            }
            KeyCode::Char('r') => {
                let id = match self.selected_row() {
                    Some(LimitsRow::Login(id)) => Some(id),
                    Some(LimitsRow::Seat(s)) => self
                        .analytics
                        .seats
                        .iter()
                        .find(|x| x.label == s)
                        .and_then(|x| x.login.clone())
                        .and_then(|l| self.login_for_seat(&s, &l)),
                    None => None,
                };
                if let Some(id) = id {
                    let l = self.login_label(&id);
                    self.vault.modal = Some(Modal::Input {
                        title: format!("Rename the saved login '{l}' to"),
                        value: l,
                        action: InputAction::Rename(id),
                    });
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(LimitsRow::Login(id)) = self.selected_row() {
                    let label = self.login_label(&id);
                    self.vault.modal = Some(Modal::Confirm {
                        text: format!(
                            "Forget the saved login '{label}'?\n\nIts tokens are deleted from the vault.\n\n[y] forget   [n] cancel"
                        ),
                        action: ConfirmAction::Remove(id),
                    });
                } else {
                    self.toast(
                        "select a saved login (under SAVED LOGIN) to forget it",
                        Tone::Info,
                    );
                }
            }
            KeyCode::Char('R') => {
                self.vault_reload();
                self.request_analytics();
                self.toast("re-reading seats and logins…", Tone::Info);
            }
            _ => return false,
        }
        true
    }

    fn modal_key(&mut self, m: Modal, k: KeyEvent) {
        match m {
            Modal::Input {
                title,
                mut value,
                action,
            } => match k.code {
                KeyCode::Esc => self.vault.modal = None,
                KeyCode::Enter => {
                    self.vault.modal = None;
                    match action {
                        InputAction::Capture(seat) => self.do_capture(&seat, &value),
                        InputAction::Rename(old) => {
                            let res = Vault::open(&self.vault_dir)
                                .and_then(|mut v| v.edit(|tx| tx.rename(&old, value.trim())));
                            match res {
                                Ok(()) => {
                                    self.toast(format!("renamed to '{}'", value.trim()), Tone::Good)
                                }
                                Err(e) => self.toast(format!("{e:#}"), Tone::Bad),
                            }
                            self.vault_reload();
                            self.request_analytics();
                        }
                        InputAction::Add(h) => {
                            if value.trim().is_empty() {
                                self.toast("a label is needed", Tone::Warn);
                            } else {
                                self.vault.external =
                                    Some(External::AddLogin(value.trim().to_string(), h));
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    value.pop();
                    self.vault.modal = Some(Modal::Input {
                        title,
                        value,
                        action,
                    });
                }
                KeyCode::Char(c) if !c.is_control() => {
                    value.push(c);
                    self.vault.modal = Some(Modal::Input {
                        title,
                        value,
                        action,
                    });
                }
                _ => {}
            },
            Modal::Confirm { action, .. } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.vault.modal = None;
                    match action {
                        ConfirmAction::Switch { seat, login } => self.do_switch(&seat, &login),
                        ConfirmAction::Remove(login) => {
                            let res = Vault::open(&self.vault_dir)
                                .and_then(|mut v| v.edit(|tx| tx.remove(&login, true)));
                            match res {
                                Ok(l) => self.toast(format!("forgot '{}'", l.label), Tone::Good),
                                Err(e) => self.toast(format!("{e:#}"), Tone::Bad),
                            }
                            self.vault_reload();
                            self.request_analytics();
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.vault.modal = None,
                _ => {}
            },
        }
    }

    /// Runs the harness (`claude`, `agy`; a harness the vault keeps) in a scratch seat to sign in, then saves the login.
    /// Called with the dashboard suspended (the terminal belongs to the harness meanwhile).
    pub fn run_add_login(&mut self, label: &str, harness: Harness) {
        let scratch = crate::vault::new_login_scratch(&self.vault_dir, chrono::Utc::now());
        let (program, hint) = crate::vault::new_login_recipe(harness);
        println!(
            "\nswarmy: starting {} in a scratch seat for '{label}'. {hint}\n",
            harness.display_name()
        );
        let res = (|| -> anyhow::Result<crate::vault::NewLogin> {
            let mut v = Vault::open(&self.vault_dir)?;
            v.check_new_label(label, harness)?;
            std::fs::create_dir_all(&scratch)?;
            let (mut cmd, seat) = crate::vault::new_login(harness, label, &scratch);
            if let Err(e) = cmd.status() {
                if let Err(rm) = std::fs::remove_dir_all(&scratch) {
                    crate::diag::log("vault", &format!("{}: {rm}", scratch.display()));
                }
                anyhow::bail!("cannot start `{program}`: {e}");
            }
            crate::vault::save_new_login(&mut v, label, &scratch, &seat)
        })();
        match res {
            Ok(saved) => {
                if let Some(msg) = saved.left_behind {
                    crate::diag::log("vault", &msg);
                    self.log_event("⚿", msg, Tone::Bad);
                }
                self.log_event("⚿", format!("new login saved: {label}"), Tone::Accent);
                self.toast(format!("saved '{label}'"), Tone::Good);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                // A kept scratch dir holds a live login: the event log keeps where.
                self.log_event("⚿", msg.clone(), Tone::Bad);
                self.toast(msg, Tone::Bad);
            }
        }
        self.vault_reload();
        self.request_analytics();
    }
}

/// The harness of the seat labelled `seat`.
fn harness_of_seat(app: &App, seat: &str) -> Option<Harness> {
    app.seats
        .iter()
        .find(|s| s.label == seat)
        .map(|s| s.harness)
}

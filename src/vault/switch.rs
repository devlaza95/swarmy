//! Switching which saved login a seat uses: the vault's one write into a harness's config dir
//! (Claude Code: `.credentials.json` plus `oauthAccount` in `.claude.json`; Antigravity: its
//! `antigravity-oauth-token`).

use std::path::PathBuf;

use super::{Login, SEAT_BACKUPS, VaultError, VaultTx, read_seat, store};
use crate::claude::config_json;
use crate::claude::credentials;
use crate::claude::seats::{Seat, SeatId};
use crate::core::harness::Harness;
use crate::core::ids::LoginId;

/// How many times a switch starts again because Claude Code rewrote the seat's credentials
/// between swarmy saving them and replacing them.
const SWITCH_ATTEMPTS: usize = 5;

impl VaultTx<'_> {
    /// Writes `login_key` into `seat`, first saving whatever the seat holds. The login may be in
    /// other seats too and stays there: one grant can serve several seats (both refresh the
    /// access token; Claude Code and agy keep the refresh token).
    ///
    /// All or nothing for the seat: the seat's login is saved first (on disk before anything in
    /// the seat changes), both seat files are backed up, then `.credentials.json` is replaced
    /// only while it still holds what was saved (a token Claude Code refreshed meanwhile is saved
    /// again first, never overwritten), then `oauthAccount` is merged into `.claude.json`. When
    /// that last write fails, the credentials go back as they were; if even that is not possible
    /// (Claude Code wrote them since), the vault records the login the seat holds now, so the
    /// vault and the seat never disagree about which grant is where (and [`VaultTx::sync`]
    /// repairs the same after a crash between the two writes).
    pub fn switch(&mut self, seat: &Seat, login_key: &str) -> anyhow::Result<SwitchReport> {
        let target = self.target_for(seat, login_key)?;
        self.switch_in(seat, &target.id, false)
    }

    /// The login `login_key` names for `seat`: a label names one login per harness, the seat's
    /// first, else whatever it names (a login of another harness is refused by the switch).
    fn target_for(&self, seat: &Seat, login_key: &str) -> anyhow::Result<Login> {
        match self.index.find_in(login_key, Some(seat.harness)) {
            Ok(l) => Ok(l.clone()),
            Err(VaultError::NoLogin(_)) => Ok(self.find(login_key).cloned()?),
            Err(e) => Err(e.into()),
        }
    }

    /// [`switch`](Self::switch), where a login that cannot be in two seats (Codex) and is in
    /// another of `seats` moves instead of being refused: that seat takes the login `seat` held
    /// (the two swap), or is left without one when `seat` held none (its copy stays saved). The
    /// other seat's copy is saved first: it is the newest (Codex refreshes in place). Nothing may
    /// run in any of the seats: checked here, under the vault lock, right before the writes.
    ///
    /// The order keeps every single-use token in one seat at most, whichever step fails (reviews
    /// A-H2, B-M1): the seats the login moves out of are emptied first (each saved from the very
    /// text that is removed), then the login goes into `seat` (whose own login is saved), then
    /// that login goes into the first seat emptied. A failure after a seat was emptied is a
    /// [`VaultError::SwitchIncomplete`] that says which seat is empty; the vault records it.
    pub fn switch_moving(
        &mut self,
        seat: &Seat,
        login_key: &str,
        seats: &[Seat],
    ) -> anyhow::Result<SwitchReport> {
        let target = self.target_for(seat, login_key)?;
        let others: Vec<Seat> = seats
            .iter()
            .filter(|s| s.id != seat.id && target.is_in(&s.id))
            .cloned()
            .collect();
        let all_at_hand = target
            .mounted
            .iter()
            .all(|m| *m == seat.id || others.iter().any(|o| &o.id == m));
        if seat.harness.shares_logins() || others.is_empty() || !all_at_hand {
            return self.switch(seat, login_key);
        }
        for s in std::iter::once(seat).chain(&others) {
            ensure_idle(s)?;
        }
        // The seat's own login, saved first: it goes to the other seat.
        self.sync(std::slice::from_ref(seat))?;
        let (prev, saved_now) = match read_seat(seat)?.secret {
            Some(sec) => {
                let fp = sec.meta().fingerprint;
                let known = self.index.logins.iter().any(|l| l.meta.fingerprint == fp);
                (self.capture(seat, None)?, !known)
            }
            None => (None, false),
        };
        let mut emptied: Vec<&Seat> = Vec::new();
        for o in &others {
            if let Err(e) = self.clear_seat(o) {
                return Err(self.move_incomplete(seat, &emptied, e));
            }
            emptied.push(o);
        }
        let mut rep = match self.switch_in(seat, &target.id, true) {
            Ok(r) => r,
            Err(e) => return Err(self.move_incomplete(seat, &emptied, e)),
        };
        if saved_now {
            rep.auto_saved = prev.as_ref().and_then(|p| self.index.label_of(p));
        }
        for (i, o) in others.iter().enumerate() {
            match prev.as_ref().filter(|p| **p != target.id && i == 0) {
                Some(p) => {
                    if let Err(e) = self.switch_in(o, p, true) {
                        return Err(self.move_incomplete(seat, &emptied[i..], e));
                    }
                    rep.moved.push((o.label.clone(), self.index.label_of(p)));
                }
                None => rep.moved.push((o.label.clone(), None)),
            }
        }
        rep.shared_with.clear();
        Ok(rep)
    }

    /// A move that stopped after `emptied` lost their login: the vault is saved as the seats
    /// are now (`edit` would drop it with the error), and the error says which seats are empty.
    fn move_incomplete(
        &mut self,
        seat: &Seat,
        emptied: &[&Seat],
        err: anyhow::Error,
    ) -> anyhow::Error {
        if emptied.is_empty() {
            return err;
        }
        let saved = self.save();
        let names: Vec<&str> = emptied.iter().map(|s| s.label.as_str()).collect();
        let mut what = format!(
            "{err:#}; seat {} is left without a login (its login is saved in the vault)",
            names.join(", ")
        );
        if let Err(e) = saved {
            what.push_str(&format!("; the vault was not saved ({e:#})"));
        }
        self.journal(&format!("move to seat={} incomplete: {what}", seat.label));
        VaultError::SwitchIncomplete {
            seat: seat.label.clone(),
            what,
        }
        .into()
    }

    /// Takes the login out of `seat` (after a backup). Its newest copy is saved first, read from
    /// the very text that is then removed: a refresh after that read makes the removal refuse
    /// and start again, so no token is deleted unsaved (review B-M2).
    fn clear_seat(&mut self, seat: &Seat) -> anyhow::Result<()> {
        let path = seat.credentials_file();
        for _ in 0..SWITCH_ATTEMPTS {
            let text = store::read_optional(&path)?;
            let now = read_seat(seat)?;
            if store::read_optional(&path)? != text {
                continue;
            }
            self.sync_seat(seat, &now);
            self.save()?;
            let bdir = self.seat_backups(seat)?;
            let name = match path.file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => "login".to_string(),
            };
            store::backup(&bdir, &[(name.as_str(), text.as_deref())], SEAT_BACKUPS)?;
            #[cfg(test)]
            super::tests::hooks::before_seat_write();
            if !store::replace_if_unchanged(&path, text.as_deref(), None)? {
                continue;
            }
            for l in &mut self.index.logins {
                l.mounted.retain(|m| *m != seat.id);
            }
            self.index.seats.remove(&seat.id);
            self.journal(&format!("clear seat={}", seat.label));
            return Ok(());
        }
        Err(VaultError::SeatBusy(path).into())
    }

    /// The switch itself, of a login named by its id: the user's words (a label, an email) are
    /// resolved before (`target_for`), never inside (review B-M5).
    fn switch_in(
        &mut self,
        seat: &Seat,
        login: &LoginId,
        moving: bool,
    ) -> anyhow::Result<SwitchReport> {
        let target = self
            .index
            .logins
            .iter()
            .find(|l| l.id == *login)
            .cloned()
            .ok_or_else(|| VaultError::NoLogin(login.to_string()))?;
        if target.harness != seat.harness {
            return Err(VaultError::WrongHarness {
                login: target.label,
                login_harness: target.harness.display_name().into(),
                seat: seat.label.clone(),
                seat_harness: seat.harness.display_name().into(),
            }
            .into());
        }
        let shared_with: Vec<String> = target
            .mounted
            .iter()
            .filter(|m| **m != seat.id)
            .map(|m| self.index.seat_name(m))
            .collect();
        if !seat.harness.shares_logins() && !shared_with.is_empty() && !moving {
            return Err(VaultError::OneSeatOnly {
                login: target.label,
                harness: seat.harness.display_name().into(),
                seats: shared_with.join(", "),
            }
            .into());
        }
        let secret = self
            .secret(&target.id)
            .ok_or_else(|| VaultError::MissingSecret(target.label.clone()))?;
        ensure_idle(seat)?;
        let mut auto_saved: Option<String> = None;
        for _ in 0..SWITCH_ATTEMPTS {
            // 1. Save what the seat holds now, on disk before the seat is overwritten.
            self.sync(std::slice::from_ref(seat))?;
            let cred_text = store::read_optional(&seat.credentials_file())?;
            let cj_text = match seat.harness {
                Harness::Claude => store::read_optional(&seat.claude_json)?,
                Harness::Agy | Harness::Codex => None,
            };
            let before = read_seat(seat)?;
            if store::read_optional(&seat.credentials_file())? != cred_text {
                // Claude Code refreshed the token while it was read: read it again.
                continue;
            }
            let known_before = before.secret.as_ref().is_some_and(|sec| {
                self.index
                    .logins
                    .iter()
                    .any(|l| l.meta.fingerprint == sec.meta().fingerprint)
            });
            let previous = match &before.secret {
                Some(sec) => Some(self.save_seat_login(seat, &before, sec, None)?),
                None => None,
            };
            if auto_saved.is_none() && !known_before {
                auto_saved = previous.as_ref().and_then(|id| self.index.label_of(id));
            }
            if previous.as_ref() == Some(&target.id) {
                return Ok(SwitchReport {
                    auto_saved: None,
                    seat: seat.label.clone(),
                    from: Some(target.label.clone()),
                    to: target.label,
                    backup: None,
                    unchanged: true,
                    shared_with,
                    moved: Vec::new(),
                });
            }
            self.save()?;

            // 2. Back up both files, kept per seat.
            let bdir = self.seat_backups(seat)?;
            match seat.harness {
                Harness::Claude => store::backup(
                    &bdir,
                    &[
                        ("credentials.json", cred_text.as_deref()),
                        ("claude.json", cj_text.as_deref()),
                    ],
                    SEAT_BACKUPS,
                )?,
                Harness::Agy => store::backup(
                    &bdir,
                    &[(crate::agy::credentials::TOKEN_FILE, cred_text.as_deref())],
                    SEAT_BACKUPS,
                )?,
                Harness::Codex => store::backup(
                    &bdir,
                    &[(crate::codex::credentials::AUTH_FILE, cred_text.as_deref())],
                    SEAT_BACKUPS,
                )?,
            }

            #[cfg(test)]
            super::tests::hooks::before_seat_write();
            // 3. Write the new login, keeping mcpOAuth and every other key, only over the very
            //    text whose login step 1 saved.
            let new_creds = match seat.harness {
                Harness::Claude => credentials::with_login(cred_text.as_deref(), &secret)?,
                Harness::Agy => crate::agy::credentials::with_login(&secret)?,
                Harness::Codex => crate::codex::credentials::with_login(&secret)?,
            };
            if !store::replace_if_unchanged(
                &seat.credentials_file(),
                cred_text.as_deref(),
                Some(new_creds.as_bytes()),
            )? {
                continue;
            }
            // 4. The account shown by Claude Code: only `oauthAccount` changes, merged into the
            //    file as it is right now (Claude Code rewrites it every few seconds).
            if seat.harness == Harness::Claude
                && let Some(acct) = &target.oauth_account
                && let Err(e) = store::rewrite_merging(&seat.claude_json, |t| {
                    config_json::with_oauth_account(t, Some(acct))
                })
            {
                return Err(self.undo_credentials(
                    seat,
                    &target,
                    cred_text.as_deref(),
                    &new_creds,
                    e,
                ));
            }

            // 5. Bookkeeping.
            let from_label = previous.as_ref().and_then(|id| self.index.label_of(id));
            self.mount(&target.id, &seat.id);
            let target_profile_at = target
                .oauth_account
                .as_ref()
                .and_then(config_json::Account::from_value)
                .and_then(|a| a.profile_fetched_at);
            // Without a saved profile, `.claude.json` still names the previous account: the
            // seat's account is unknown until Claude Code reads the new login's profile.
            let now = self.now;
            // An agy token file names its own account.
            let known_profile = target.account_uuid.is_some()
                && (matches!(seat.harness, Harness::Agy | Harness::Codex)
                    || target.oauth_account.is_some());
            self.index.seats.insert(
                seat.id.clone(),
                super::logins::SeatSeen {
                    label: seat.label.clone(),
                    fingerprint: target.meta.fingerprint.clone(),
                    account_uuid: target.account_uuid.clone().filter(|_| known_profile),
                    profile_at: target_profile_at,
                    unverified_since: (!known_profile).then_some(now),
                    switched_at: Some(now),
                },
            );
            self.journal(&format!(
                "switch seat={} from={} to={}",
                seat.label,
                from_label.as_deref().unwrap_or("-"),
                target.label
            ));
            return Ok(SwitchReport {
                auto_saved,
                seat: seat.label.clone(),
                from: from_label,
                to: target.label,
                backup: Some(bdir),
                unchanged: false,
                shared_with,
                moved: Vec::new(),
            });
        }
        Err(VaultError::SeatBusy(seat.credentials_file()).into())
    }

    /// Records `id` as the login in `seat` (a seat holds one login; the login may be in other
    /// seats as well).
    pub(super) fn mount(&mut self, id: &LoginId, seat: &SeatId) {
        for l in &mut self.index.logins {
            if l.id != *id {
                l.mounted.retain(|m| m != seat);
            }
        }
        if let Some(l) = self.index.find_mut(id)
            && !l.is_in(seat)
        {
            l.mounted.push(seat.clone());
        }
    }

    /// A switch whose `.claude.json` write failed (`err`) after the credentials were written:
    /// puts the credentials back as they were, unless Claude Code has written them since. Then
    /// the seat really runs the new login, and the vault says so (saved right away: the change
    /// itself fails, so `edit` would drop it).
    fn undo_credentials(
        &mut self,
        seat: &Seat,
        target: &Login,
        before: Option<&str>,
        written: &str,
        err: anyhow::Error,
    ) -> anyhow::Error {
        let restored = store::replace_if_unchanged(
            &seat.credentials_file(),
            Some(written),
            before.map(str::as_bytes),
        );
        if let Ok(true) = restored {
            self.journal(&format!(
                "switch seat={} to={} failed, seat restored: {err:#}",
                seat.label, target.label
            ));
            return err.context(format!(
                "the switch of seat '{}' failed; the seat is as it was",
                seat.label
            ));
        }
        self.mount(&target.id, &seat.id);
        let saved = self.save();
        self.journal(&format!(
            "switch seat={} to={} half done (credentials written, .claude.json not): {err:#}",
            seat.label, target.label
        ));
        let what = if let Err(e) = restored {
            format!(".claude.json not written ({err:#}), credentials not restored ({e:#})")
        } else if let Err(e) = saved {
            format!(".claude.json not written ({err:#}), vault not saved ({e:#})")
        } else {
            format!(
                ".claude.json not written ({err:#}); Claude Code refreshed the new login meanwhile"
            )
        };
        VaultError::SwitchIncomplete {
            seat: seat.label.clone(),
            what,
        }
        .into()
    }
}

/// Refuses a switch into a Codex seat where Codex runs: it keeps the login it loaded and would
/// write a refresh of it back, spending the single-use refresh token the vault holds. Looked at
/// by the vault itself, under its lock, right before the writes; the callers' own looks are
/// hints only (reviews A-H3, B-M3: they looked when a dialog opened, and saw threads only).
fn ensure_idle(seat: &Seat) -> Result<(), VaultError> {
    if seat.harness != Harness::Codex {
        return Ok(());
    }
    let (threads, procs) = codex_running(seat);
    if threads == 0 && procs == 0 {
        return Ok(());
    }
    Err(VaultError::HarnessRunning {
        seat: seat.label.clone(),
        harness: seat.harness.display_name().into(),
        what: if threads > 0 {
            format!("{threads} open thread(s)")
        } else {
            format!("{procs} process(es)")
        },
    })
}

/// Open threads (writer locks held) and `codex` processes whose home is `seat`.
fn codex_running(seat: &Seat) -> (usize, usize) {
    #[cfg(test)]
    if let Some(n) = super::tests::hooks::codex_running(seat) {
        return n;
    }
    let threads = crate::codex::live::open_threads_once(&seat.dir).len();
    let procs = crate::procs::ProcTable::snapshot()
        .codex
        .iter()
        .filter(|p| {
            p.home
                .as_deref()
                .is_some_and(|h| crate::fsutil::canonical(h).is_ok_and(|h| h == seat.dir))
        })
        .count();
    (threads, procs)
}

#[derive(Debug)]
pub struct SwitchReport {
    /// The seat held a login that was not in the vault; it was saved under this label.
    pub auto_saved: Option<String>,
    pub seat: String,
    pub from: Option<String>,
    pub to: String,
    pub backup: Option<PathBuf>,
    pub unchanged: bool,
    /// Other seats that hold the same login.
    pub shared_with: Vec<String>,
    /// Seats a login that cannot be shared moved out of, and the login each holds now (the one
    /// the switched seat held; none when it held none).
    pub moved: Vec<(String, Option<String>)>,
}

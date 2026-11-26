//! What a seat holds, as the vault sees it: comparing a seat with what was seen there before
//! (`observe`), keeping the saved copy of a mounted login current (`sync_seat`), and telling a
//! rotated refresh token of the same grant from another login (`is_rotation`).

use super::logins::SeatSeen;
use super::{Login, SeatLogin, SeatState, VaultTx, read_seat};
use crate::claude::credentials::{LoginMeta, Secret};
use crate::claude::seats::{Seat, SeatId};
use crate::core::harness::Harness;
use crate::core::ids::LoginId;

/// Refresh tokens are issued for 30 days: a token issued after the profile was read means a new
/// login the profile does not describe yet.
const REFRESH_LIFETIME_DAYS: i64 = 30;

impl VaultTx<'_> {
    /// Compares a seat with what was seen there before. A new refresh token with no newer
    /// profile means a new login (or a restored backup) whose account is not known yet.
    pub(super) fn observe(&mut self, seat: &Seat, s: &SeatLogin) -> SeatState {
        let Some(sec) = &s.secret else {
            self.index.seats.remove(&seat.id);
            return SeatState::default();
        };
        let meta = sec.meta();
        let fp = meta.fingerprint.clone();
        let profile_at = s.account.as_ref().and_then(|a| a.profile_fetched_at);
        let account = s.account.as_ref().map(|a| a.id().to_string());
        let prev = self.index.seats.get(&seat.id).cloned();
        // A Claude Code grant that names its account, the one the profile names.
        let named = meta.account.is_some() && meta.account == account;
        let unverified_since = match &prev {
            // An agy token file and Codex's auth.json name their own account, and so may a
            // Claude Code grant: nothing to confirm.
            _ if matches!(seat.harness, Harness::Agy | Harness::Codex) || named => None,
            // First sight: a refresh token issued after the profile was read is a newer login;
            // a file written after the profile with an access token already expired at that time
            // was copied in (Claude Code only writes fresh tokens).
            None => {
                let issued = meta
                    .refresh_expires_at
                    .map(|t| t - chrono::Duration::days(REFRESH_LIFETIME_DAYS));
                let copied_in = match (s.written_at, meta.access_expires_at, profile_at) {
                    (Some(w), Some(exp), Some(p))
                        if w > p && exp + chrono::Duration::minutes(5) < w =>
                    {
                        Some(w)
                    }
                    _ => None,
                };
                match (issued, profile_at) {
                    (Some(i), Some(p)) if i > p + chrono::Duration::minutes(10) => Some(i),
                    _ => copied_in,
                }
            }
            Some(p) if p.fingerprint != fp => {
                // The login the vault knows is in this seat (it wrote it there, or followed its
                // rotation) and the profile names its account: nothing to confirm.
                let known = self.index.logins.iter().any(|l| {
                    l.is_in(&seat.id)
                        && l.meta.fingerprint == fp
                        && l.account_uuid.is_some()
                        && l.account_uuid == account
                });
                if profile_at > p.profile_at || known {
                    None
                } else {
                    Some(self.now)
                }
            }
            Some(p) => p.unverified_since.filter(|_| profile_at <= p.profile_at),
        };
        // The switch stays this seat's while it holds the login the vault put there, rotated or
        // not (`sync_seat` follows a rotation before this runs); another login ends it.
        let followed = self
            .index
            .logins
            .iter()
            .any(|l| l.is_in(&seat.id) && l.meta.fingerprint == fp);
        let switched_at = prev
            .as_ref()
            .filter(|p| p.fingerprint == fp || followed)
            .and_then(|p| p.switched_at);
        let seen = SeatSeen {
            label: seat.label.clone(),
            fingerprint: fp.clone(),
            account_uuid: account.clone(),
            profile_at: if unverified_since.is_some() {
                prev.as_ref().and_then(|p| p.profile_at).or(profile_at)
            } else {
                profile_at
            },
            unverified_since,
            switched_at,
        };
        if prev.as_ref() != Some(&seen) {
            self.index.seats.insert(seat.id.clone(), seen);
        }
        let saved_as = self
            .index
            .logins
            .iter()
            .find(|l| l.harness == seat.harness && l.meta.fingerprint == fp)
            .map(|l| l.label.clone());
        SeatState {
            fingerprint: Some(fp),
            account_uuid: if unverified_since.is_some() {
                None
            } else {
                account
            },
            unverified_since,
            switched_at,
            saved_as,
        }
    }

    /// The state of every seat (also brings the vault up to date, see `sync`).
    pub fn seat_states(
        &mut self,
        seats: &[Seat],
    ) -> anyhow::Result<std::collections::BTreeMap<SeatId, SeatState>> {
        self.index.adopt_seats(seats);
        let mut out = std::collections::BTreeMap::new();
        for seat in seats {
            // A seat that cannot be read has no state to show; its failure is not the vault's,
            // and the saved copy stays as it is until the seat can be read again.
            match read_seat(seat) {
                Ok(s) => {
                    self.sync_seat(seat, &s);
                    out.insert(seat.id.clone(), self.observe(seat, &s));
                }
                Err(e) => crate::diag::log("vault", &format!("seat {}: {e:#}", seat.label)),
            }
        }
        Ok(out)
    }

    /// Keeps the vault copy of every mounted login current with its seat (see `sync_seat`).
    pub fn sync(&mut self, seats: &[Seat]) -> anyhow::Result<()> {
        self.index.adopt_seats(seats);
        for seat in seats {
            match read_seat(seat) {
                Ok(s) => self.sync_seat(seat, &s),
                Err(e) => crate::diag::log("vault", &format!("seat {}: {e:#}", seat.label)),
            }
        }
        Ok(())
    }

    /// Brings what the vault records about one seat in line with what the seat holds.
    ///
    /// A login is one OAuth grant, named by the fingerprint of its refresh token. Claude Code
    /// (verified on 2.1.332) rotates the refresh token on most refreshes of the access token
    /// and keeps the grant's `refreshTokenExpiresAt` (recomputed, so it moves by a second or
    /// so). The grant is followed explicitly ([`is_rotation`]): same account, same plan, a fresh
    /// access token written by Claude Code; the saved login takes the new tokens and stays
    /// mounted. Any other new refresh token is another login (a `/login`, a restored backup):
    /// the saved login is left untouched and no longer counts as mounted there, so its tokens
    /// are never overwritten by someone else's.
    ///
    /// A saved login that a missed rotation left mounted nowhere is taken back by the seat that
    /// holds its grant (`orphan_of`): the same grant lifetime, the same account, and no other
    /// candidate.
    ///
    /// A seat that holds a saved login the vault does not record there (a switch that stopped
    /// between its two writes, a seat file restored by hand, the same login copied into a second
    /// seat) gets it recorded, whatever other seats hold it too.
    ///
    /// One grant in several seats: each seat refreshes its own access token, and the saved copy
    /// takes whichever is newer (`keep_current`). Should one seat rotate the refresh token, the
    /// login follows that seat; the others then hold an older token the vault no longer knows
    /// ("not saved", never a rotation: [`is_rotation`] never moves back), which the next
    /// switch saves first like any other.
    pub(super) fn sync_seat(&mut self, seat: &Seat, s: &SeatLogin) {
        let mounted = self
            .index
            .logins
            .iter()
            .find(|l| l.is_in(&seat.id))
            .cloned();
        let Some(sec) = &s.secret else {
            if let Some(l) = mounted.and_then(|m| self.index.find_mut(&m.id)) {
                l.mounted.retain(|m| *m != seat.id);
            }
            return;
        };
        let meta = sec.meta();
        // A grant the vault holds already is that login, never a rotation of another one: a
        // switch that died after writing the credentials (`.claude.json` still naming the old
        // account) looks like a rotation otherwise, and would overwrite the old login's tokens
        // with the other grant (review B-H1).
        let saved = self
            .index
            .logins
            .iter()
            .find(|l| l.harness == seat.harness && l.meta.fingerprint == meta.fingerprint)
            .map(|l| (l.id.clone(), l.label.clone()));
        match &mounted {
            Some(m) if m.meta.fingerprint == meta.fingerprint => {
                // Same grant, maybe a fresher access token: keep the copy current.
                self.keep_current(&m.id, sec);
                return;
            }
            Some(m) if saved.is_none() && is_rotation(m, &self.saved_meta(m), &meta, s) => {
                self.keep_current(&m.id, sec);
                self.adopt_profile(&m.id, s);
                self.journal(&format!(
                    "seat={}: the refresh token of '{}' was rotated; the saved login follows it",
                    seat.label, m.label
                ));
                return;
            }
            Some(m) => {
                if let Some(l) = self.index.find_mut(&m.id) {
                    l.mounted.retain(|x| *x != seat.id);
                }
                self.journal(&format!(
                    "seat={} has a new login; '{}' kept unchanged",
                    seat.label, m.label
                ));
            }
            None => {}
        }
        if saved.is_none()
            && let Some((id, label)) = self.orphan_of(seat, &meta, s)
        {
            self.mount(&id, &seat.id);
            self.keep_current(&id, sec);
            self.adopt_profile(&id, s);
            self.journal(&format!(
                "seat={}: the refresh token of '{label}' was rotated while no seat held it; \
                 the saved login follows it",
                seat.label
            ));
            return;
        }
        if let Some((id, label)) = saved {
            self.mount(&id, &seat.id);
            self.keep_current(&id, sec);
            self.journal(&format!(
                "seat={} holds '{label}': recorded as mounted there",
                seat.label
            ));
        }
    }

    /// What the saved secret of `l` says about itself: newer than `l.meta` when that was written
    /// by an older swarmy (which did not read the account a grant names).
    fn saved_meta(&self, l: &Login) -> LoginMeta {
        self.secret(&l.id)
            .map(|x| x.meta())
            .unwrap_or_else(|| l.meta.clone())
    }

    /// The one saved login, mounted nowhere, whose grant the seat now holds rotated: a rotation
    /// the vault did not follow (an older swarmy, or none running) left it unmounted. Only
    /// Claude Code grants, which keep their lifetime across a rotation: a login whose grant
    /// expires at another time is another sign-in, even of the same account.
    fn orphan_of(&self, seat: &Seat, new: &LoginMeta, s: &SeatLogin) -> Option<(LoginId, String)> {
        if seat.harness != Harness::Claude {
            return None;
        }
        let new_exp = new.refresh_expires_at?;
        let mut found = self.index.logins.iter().filter(|l| {
            if l.harness != seat.harness || !l.mounted.is_empty() || l.mounted_label.is_some() {
                return false;
            }
            let old = self.saved_meta(l);
            old.refresh_expires_at
                .is_some_and(|e| (e - new_exp).abs() <= expiry_slack())
                && is_rotation(l, &old, new, s)
        });
        let l = found.next()?;
        found
            .next()
            .is_none()
            .then(|| (l.id.clone(), l.label.clone()))
    }

    /// A login saved while its seat's profile was not confirmed has no account; once a rotation
    /// of it is followed and the seat's profile names the account the grant names, it takes the
    /// profile.
    fn adopt_profile(&mut self, id: &LoginId, s: &SeatLogin) {
        let Some(sec) = &s.secret else { return };
        let named = sec.meta().account;
        let Some(a) = s
            .account
            .as_ref()
            .filter(|a| named.is_some() && named.as_deref() == Some(a.id().as_str()))
        else {
            return;
        };
        if let Some(l) = self.index.find_mut(id)
            && l.account_uuid.is_none()
        {
            l.account_uuid = named;
            l.email = a.email.clone().or(l.email.take());
            l.plan = a.effective_tier().or(l.plan.take());
            if l.oauth_account.is_none() {
                l.oauth_account = s.oauth_account.clone();
            }
            l.updated_at = self.now;
        }
    }

    /// The saved copy of `id` takes the seat's tokens when they are newer: a rotated grant, or an
    /// access token that expires later. Two seats sharing a grant each refresh their own access
    /// token; taking any differing copy would rewrite the vault (and its backups) back and forth.
    fn keep_current(&mut self, id: &LoginId, sec: &Secret) {
        let saved = self.secret(id);
        if saved.as_ref() == Some(sec) {
            return;
        }
        let newer = match saved.map(|x| x.meta()) {
            None => true,
            Some(old) => {
                let new = sec.meta();
                old.fingerprint != new.fingerprint
                    || match (old.access_expires_at, new.access_expires_at) {
                        (Some(a), Some(b)) => b > a,
                        // An expiry not known on one side: the seat's copy, as before.
                        (None, _) | (Some(_), None) => true,
                    }
            }
        };
        if newer {
            self.set_secret(id, sec);
            if let Some(l) = self.index.find_mut(id) {
                l.meta = sec.meta();
                l.updated_at = self.now;
            }
        }
    }
}

/// A fresh access token is valid for hours; one that expires within this margin of being
/// written was not just issued by Claude Code (a copied or restored file).
const FRESH_ACCESS_MINUTES: i64 = 30;

/// Claude Code writes `refreshTokenExpiresAt` as its own clock plus the seconds the server says
/// are left, so the same grant's expiry moves by a second or so with each refresh (seen: 1.2 s
/// earlier). Within this margin two expiries are the same.
fn expiry_slack() -> chrono::Duration {
    chrono::Duration::minutes(5)
}

/// Whether the seat's new refresh token is `mounted`'s own grant (whose saved secret says
/// `old`), rotated by Claude Code (see `VaultTx::sync_seat`), rather than another login. All of
/// it must hold:
/// - the same account: the one each grant names itself when both do, else the login's account
///   and the one the new grant names or, failing that, the seat's profile;
/// - the plan is the same (subscription and rate-limit tier, where both are known);
/// - the access token was fresh when the file was written (Claude Code writes new tokens only;
///   a backup copied in carries an old one);
/// - the grant does not expire before the saved one (a rotation never shortens it; see
///   [`expiry_slack`]);
/// - it is not older than the saved copy: its access token does not expire earlier (a refresh
///   issues the next one; Codex's expiry has whole seconds). With one grant in two seats, the
///   seat that did not refresh still holds the token from before, which passes every other
///   test; taken for a rotation, it sent the saved login back to that token, and the next look
///   at the other seat forward again, on every look.
fn is_rotation(mounted: &Login, old: &LoginMeta, new: &LoginMeta, s: &SeatLogin) -> bool {
    let same_account = if let (Some(a), Some(b)) = (&old.account, &new.account) {
        a == b
    } else {
        let new_account = new
            .account
            .clone()
            .or_else(|| s.account.as_ref().map(|a| a.id().to_string()));
        let old_account = mounted.account_uuid.clone().or_else(|| old.account.clone());
        old_account.is_some() && new_account == old_account
    };
    let same = |a: &Option<String>, b: &Option<String>| a.is_none() || b.is_none() || a == b;
    let same_plan = same(&old.subscription_type, &new.subscription_type)
        && same(&old.rate_limit_tier, &new.rate_limit_tier);
    let fresh = s
        .written_at
        .zip(new.access_expires_at)
        .is_some_and(|(w, exp)| exp > w + chrono::Duration::minutes(FRESH_ACCESS_MINUTES));
    let not_shorter = old
        .refresh_expires_at
        .zip(new.refresh_expires_at)
        .is_none_or(|(old, new)| new + expiry_slack() >= old);
    let forward = old
        .access_expires_at
        .zip(new.access_expires_at)
        .is_none_or(|(old, new)| new >= old);
    same_account && same_plan && fresh && not_shorter && forward
}

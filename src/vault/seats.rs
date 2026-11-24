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
        let unverified_since = match &prev {
            // An agy token file and Codex's auth.json name their own account: nothing to confirm.
            _ if matches!(seat.harness, Harness::Agy | Harness::Codex) => None,
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
        let switched_at = prev
            .as_ref()
            .filter(|p| p.fingerprint == fp)
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
    /// (verified on 2.1.329) refreshes the access token every few hours and keeps the refresh
    /// token, so the fingerprint stays and the saved copy simply takes the new access token.
    /// Should a refresh ever rotate the refresh token too, the grant is followed explicitly
    /// ([`is_rotation`]): same account, same plan, a fresh access token written by Claude Code;
    /// the saved login takes the new tokens and stays mounted. Any other new refresh token is
    /// another login (a `/login`, a restored backup): the saved login is left untouched and no
    /// longer counts as mounted there, so its tokens are never overwritten by someone else's.
    ///
    /// A seat that holds a saved login the vault does not record there (a switch that stopped
    /// between its two writes, a seat file restored by hand, the same login copied into a second
    /// seat) gets it recorded, whatever other seats hold it too.
    ///
    /// One grant in several seats: each seat refreshes its own access token, and the saved copy
    /// takes whichever is newer (`keep_current`). Should one seat rotate the refresh token, the
    /// login follows that seat; the others then hold a token the vault no longer knows ("not
    /// saved"), which the next switch saves first like any other.
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
            Some(m) if saved.is_none() && is_rotation(m, &meta, s) => {
                self.keep_current(&m.id, sec);
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
        if let Some((id, label)) = saved {
            self.mount(&id, &seat.id);
            self.keep_current(&id, sec);
            self.journal(&format!(
                "seat={} holds '{label}': recorded as mounted there",
                seat.label
            ));
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

/// Whether the seat's new refresh token is the mounted login's own grant, rotated by Claude
/// Code (see `VaultTx::sync_seat`), rather than another login. All of it must hold:
/// - the profile in the seat names the mounted login's account;
/// - the plan is the same (subscription and rate-limit tier, where both are known);
/// - the access token was fresh when the file was written (Claude Code writes new tokens only;
///   a backup copied in carries an old one);
/// - the grant does not expire before the saved one (a rotation never shortens it).
fn is_rotation(mounted: &Login, new: &LoginMeta, s: &SeatLogin) -> bool {
    let same_account = mounted.account_uuid.is_some()
        && s.account.as_ref().map(|a| a.id().to_string()) == mounted.account_uuid;
    let same = |a: &Option<String>, b: &Option<String>| a.is_none() || b.is_none() || a == b;
    let same_plan = same(&mounted.meta.subscription_type, &new.subscription_type)
        && same(&mounted.meta.rate_limit_tier, &new.rate_limit_tier);
    let fresh = s
        .written_at
        .zip(new.access_expires_at)
        .is_some_and(|(w, exp)| exp > w + chrono::Duration::minutes(FRESH_ACCESS_MINUTES));
    let not_shorter = mounted
        .meta
        .refresh_expires_at
        .zip(new.refresh_expires_at)
        .is_none_or(|(old, new)| new >= old);
    same_account && same_plan && fresh && not_shorter
}

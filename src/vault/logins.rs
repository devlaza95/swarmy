//! `vault/logins.json`: the saved logins and what was last seen in each seat, its format
//! versions, and the rules on labels.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Login, store};
use crate::claude::seats::{Seat, SeatId};
use crate::core::harness::Harness;
use crate::core::ids::LoginId;
use crate::errors::VaultError;

/// The format of `logins.json`: 4 gives each login its `harness` and a list of seats in
/// `mounted` (one login in several seats); 3 keys `account_uuid` by canonical `AccountId`
/// (`<accountUuid>/<organizationUuid>` when an org is known); 2 named seats by `SeatId`; 1 (no
/// `v`) by seat label. Older files read as they are (`Login::mounted` takes one seat or many).
const INDEX_VERSION: u32 = 4;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Index {
    #[serde(default = "version_1")]
    v: u32,
    pub(super) logins: Vec<Login>,
    /// What was last seen in each seat, to tell a token refresh from a new login.
    #[serde(default)]
    pub(super) seats: std::collections::BTreeMap<SeatId, SeatSeen>,
    /// Entries of `seats` an older swarmy keyed by a seat label, not matched to a seat yet.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    legacy_seats: std::collections::BTreeMap<String, SeatSeen>,
}

fn version_1() -> u32 {
    1
}

impl Default for Index {
    fn default() -> Self {
        Index {
            v: INDEX_VERSION,
            logins: Vec::new(),
            seats: Default::default(),
            legacy_seats: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct SeatSeen {
    /// The seat's label when it was last seen, for messages only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(super) label: String,
    pub(super) fingerprint: String,
    pub(super) account_uuid: Option<String>,
    pub(super) profile_at: Option<DateTime<Utc>>,
    /// The refresh token changed and Claude Code has not re-read the profile since.
    pub(super) unverified_since: Option<DateTime<Utc>>,
    /// When the vault last switched this seat to another login: a cached utilization reading
    /// older than this without its own `organizationUuid` may belong to the previous org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) switched_at: Option<DateTime<Utc>>,
}

pub(super) fn read_index(dir: &Path) -> anyhow::Result<(Index, Option<String>)> {
    let text = store::read_optional(&dir.join("logins.json"))?;
    let index = match &text {
        Some(t) => {
            let mut i: Index = serde_json::from_str(t)
                .map_err(|e| anyhow::Error::new(e).context(VaultError::Damaged("logins.json")))?;
            i.upgrade()?;
            i
        }
        None => Index::default(),
    };
    Ok((index, text))
}

impl Index {
    /// Reads an older format in place (nothing is lost): version 1 stored seat labels where
    /// version 2 stores seat ids, so they wait in `mounted_label` and `legacy_seats` until
    /// [`Index::adopt_seats`] matches them to a seat.
    pub(super) fn upgrade(&mut self) -> anyhow::Result<()> {
        if self.v > INDEX_VERSION {
            return Err(VaultError::NewerFormat(self.v).into());
        }
        if self.v < 2 {
            for l in &mut self.logins {
                if let Some(label) = std::mem::take(&mut l.mounted).into_iter().next() {
                    l.mounted_label = Some(label.as_str().to_string());
                }
            }
            for (label, mut seen) in std::mem::take(&mut self.seats) {
                seen.label = label.as_str().to_string();
                self.legacy_seats.insert(label.as_str().to_string(), seen);
            }
        }
        if self.v < 3 {
            // Upgrade bare `accountUuid` to `<accountUuid>/<organizationUuid>` from the saved
            // `oauthAccount` profile, and refresh the derived plan tier with org precedence.
            for l in &mut self.logins {
                if let Some(raw) = &l.oauth_account
                    && let Some(mut acct) = crate::claude::config_json::Account::from_value(raw)
                {
                    acct.apply_credentials(&l.meta);
                    let id = acct.id();
                    if id.has_org() || l.account_uuid.is_none() {
                        l.account_uuid = Some(id.to_string());
                    }
                    if let Some(tier) = acct.effective_tier() {
                        l.plan = Some(tier);
                    }
                }
            }
        }
        self.v = INDEX_VERSION;
        Ok(())
    }

    /// Matches what an older swarmy stored under a seat label to the seat that carries that
    /// label now. A label no current seat carries keeps waiting.
    pub(super) fn adopt_seats(&mut self, seats: &[Seat]) {
        for seat in seats {
            if let Some(mut seen) = self.legacy_seats.remove(&seat.label) {
                seen.label = seat.label.clone();
                self.seats.entry(seat.id.clone()).or_insert(seen);
            }
            for l in &mut self.logins {
                if l.mounted_label.as_deref() == Some(seat.label.as_str()) {
                    if !l.is_in(&seat.id) {
                        l.mounted.push(seat.id.clone());
                    }
                    l.mounted_label = None;
                }
            }
        }
    }

    /// How messages name a seat id: its last label, else the id.
    pub(super) fn seat_name(&self, id: &SeatId) -> String {
        self.seats
            .get(id)
            .map(|s| s.label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| id.to_string())
    }

    /// The login `key` names: its label, its id or its email. A key that names two logins
    /// (one's label is another's email, two logins of one email, one label in two harnesses)
    /// is an error that lists them, never a guess (review L3).
    /// An id for a new login: `l` and the first 10 hex digits of its grant's fingerprint, with
    /// `-2`, `-3`… when taken. A login keeps the id of its first grant when its refresh token
    /// rotates, so that grant coming back (an old file imported) would otherwise get the same
    /// id, overwrite the live secret, and be forgotten together with it (review A-H1).
    pub(super) fn new_login_id(&self, fingerprint: &str) -> LoginId {
        let base = format!("l{}", fingerprint.chars().take(10).collect::<String>());
        let taken = |id: &str| self.logins.iter().any(|l| l.id.as_str() == id);
        let mut id = base.clone();
        let mut n = 2u32;
        while taken(&id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        LoginId::from_stored(id)
    }

    /// What every saved vault holds (reviews A-H1, B-H1, B-M1, B-M5): login ids are unique, a
    /// grant (harness and fingerprint) is saved once, every login has its secret, a seat holds
    /// one login of its harness, and a Codex login (single-use refresh tokens) is in one seat at
    /// most. The violations, worded for the journal; empty when it all holds.
    pub(super) fn violations(&self, has_secret: impl Fn(&LoginId) -> bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        let mut grants = std::collections::BTreeSet::new();
        let mut seats = std::collections::BTreeMap::new();
        for l in &self.logins {
            if !ids.insert(&l.id) {
                out.push(format!("two logins have id '{}'", l.id));
            }
            if !grants.insert((l.harness, l.meta.fingerprint.as_str())) {
                out.push(format!(
                    "the grant of '{}' ({}) is saved twice",
                    l.label, l.meta.fingerprint
                ));
            }
            if !has_secret(&l.id) {
                out.push(format!("login '{}' has no secret", l.label));
            }
            if !l.harness.shares_logins() && l.mounted.len() > 1 {
                out.push(format!(
                    "{} login '{}' is in {} seats",
                    l.harness,
                    l.label,
                    l.mounted.len()
                ));
            }
            for seat in &l.mounted {
                if let Some(other) = seats.insert((l.harness, seat), l.label.as_str()) {
                    out.push(format!(
                        "seat {} holds both '{other}' and '{}'",
                        self.seat_name(seat),
                        l.label
                    ));
                }
            }
        }
        out
    }

    pub(super) fn find(&self, key: &str) -> Result<&Login, VaultError> {
        self.find_in(key, None)
    }

    /// [`find`](Self::find) among the logins of `harness` only, when given: a label names one
    /// login per harness.
    pub(super) fn find_in(
        &self,
        key: &str,
        harness: Option<Harness>,
    ) -> Result<&Login, VaultError> {
        let hits: Vec<&Login> = self
            .logins
            .iter()
            .filter(|l| harness.is_none_or(|h| l.harness == h))
            .filter(|l| l.label == key || l.id.as_str() == key || l.email.as_deref() == Some(key))
            .collect();
        match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(VaultError::NoLogin(key.to_string())),
            many => Err(VaultError::AmbiguousLogin {
                key: key.to_string(),
                logins: many
                    .iter()
                    .map(|l| format!("{} ({})", l.label, l.harness.display_name()))
                    .collect(),
            }),
        }
    }

    pub(super) fn find_mut(&mut self, id: &LoginId) -> Option<&mut Login> {
        self.logins.iter_mut().find(|l| l.id == *id)
    }

    pub(super) fn label_of(&self, id: &LoginId) -> Option<String> {
        self.logins
            .iter()
            .find(|l| l.id == *id)
            .map(|l| l.label.clone())
    }

    /// The one place that decides whether `label` may name the login `id` (`None`: a new one) of
    /// `harness`. Labels are unique per harness: `sam` may name a Claude Code login and an
    /// Antigravity one.
    pub(super) fn check_label(
        &self,
        label: &str,
        id: Option<&LoginId>,
        harness: Harness,
    ) -> anyhow::Result<()> {
        if label.trim().is_empty() {
            return Err(VaultError::LabelEmpty.into());
        }
        // A label may not be another login's label in the harness, nor any other login's id or
        // email: lookups take all three, and an id that names two logins broke the vault's own
        // lookups by id (review B-M5).
        if self.logins.iter().any(|l| {
            Some(&l.id) != id
                && ((l.harness == harness && l.label == label)
                    || l.id.as_str() == label
                    || l.email.as_deref() == Some(label))
        }) {
            return Err(VaultError::LabelTaken {
                label: label.to_string(),
                harness: harness.display_name().to_string(),
            }
            .into());
        }
        Ok(())
    }

    /// `base`, or `base-2`, `base-3`… whichever no login of `harness` has.
    pub(super) fn unique_label(&self, base: &str, harness: Harness) -> String {
        let taken = |l: &str| {
            self.logins
                .iter()
                .any(|x| x.harness == harness && x.label == l)
        };
        let mut label = base.to_string();
        let mut n = 2;
        while taken(&label) {
            label = format!("{base}-{n}");
            n += 1;
        }
        label
    }

    /// Repairs labels duplicated by older versions: later duplicates get a unique suffix.
    pub(super) fn dedupe_labels(&mut self) -> Vec<(String, String)> {
        let mut renamed = Vec::new();
        for i in 0..self.logins.len() {
            let label = self.logins[i].label.clone();
            let h = self.logins[i].harness;
            if self.logins[..i]
                .iter()
                .any(|l| l.harness == h && l.label == label)
            {
                let new = self.unique_label(&label, h);
                self.logins[i].label = new.clone();
                renamed.push((label, new));
            }
        }
        renamed
    }
}

//! Identities: a seat, a session, an account. Each is a zero-cost newtype over the text Claude
//! Code or swarmy stores, so a seat id cannot be passed where a session id or a display label is
//! expected (reviews H3, H7, M10).

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The identity of a seat (a Claude Code config dir): the first 16 hex digits of the SHA-256 of
/// its canonical path. It is what the index, the vault, the status-line files and the fleet
/// summary store; a seat's label is for display only, so renaming a seat (a `label` in
/// `config.toml`, another dir of the same name found first) changes no attribution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SeatId(String);

impl SeatId {
    /// Hex digits kept of the digest.
    pub const LEN: usize = 16;

    /// The id of a config dir from its canonical path (the caller canonicalizes: `core` does no
    /// I/O). The status line names its reading files the same way.
    pub fn of_canonical_dir(dir: &Path) -> SeatId {
        let h = Sha256::digest(dir.to_string_lossy().as_bytes());
        let mut s = hex::encode(h);
        s.truncate(Self::LEN);
        SeatId(s)
    }

    /// An id read back from where swarmy stored it (the index, the vault, the fleet summary).
    /// Older stores held a seat's label here instead; [`SeatId::looks_like_id`] tells them apart.
    pub fn from_stored(s: impl Into<String>) -> SeatId {
        SeatId(s.into())
    }

    /// Whether `s` has the shape of a seat id (16 lowercase hex digits), as opposed to a label
    /// stored by an older swarmy.
    pub fn looks_like_id(s: &str) -> bool {
        s.len() == Self::LEN
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Tests name seats with short made-up ids.
#[cfg(test)]
impl From<&str> for SeatId {
    fn from(s: &str) -> SeatId {
        SeatId(s.to_string())
    }
}

// Same `Eq`, `Ord` and `Hash` as the text: maps keyed by id can be read with a `&str`.
impl std::borrow::Borrow<str> for SeatId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SeatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A project's identity: its key in the index (`projects.key`: the git root, or a dir outside any
/// repo). Two repositories with the same dir name are two projects; a project's name is for
/// display only, so joining on it mixed their numbers (review B-H3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct ProjectKey(String);

impl ProjectKey {
    /// A key as the index stores it (a query's `Row::key` for `Dim::Project`).
    pub fn from_stored(s: impl Into<String>) -> ProjectKey {
        ProjectKey(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A saved login's id in the vault: `l` and the first hex digits of its first grant's
/// fingerprint (`-2`, `-3`… when taken). Made only by the vault (`logins::Index::new_login_id`);
/// what a user types (a label, an id, an email) is resolved to one at the edge, so a label can
/// never be taken for an id inside the vault (review B-M5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct LoginId(String);

impl LoginId {
    /// An id as the vault stores it (`logins.json`, `secrets.json` keys).
    pub fn from_stored(s: impl Into<String>) -> LoginId {
        LoginId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for LoginId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LoginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Claude Code session id (a uuid in practice, but read from files that may hold anything).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(s: impl Into<String>) -> SessionId {
        SessionId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The first 8 characters (fewer when the id is shorter), for display. Never panics, also
    /// on an empty id or one with multi-byte characters.
    pub fn short(&self) -> &str {
        short(&self.0)
    }
}

impl std::borrow::Borrow<str> for SessionId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> SessionId {
        SessionId(s.to_string())
    }
}

impl PartialEq<str> for SessionId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SessionId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// An Anthropic account in an organization: `<accountUuid>/<organizationUuid>` when the
/// organization is known, else `<accountUuid>` (legacy transcripts/rows without an org). One
/// user (`accountUuid`) can belong to several organizations (e.g. personal Max and a company
/// Team) with separate plans and rate limits, so the pair is the true identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct AccountId(String);

impl AccountId {
    /// Canonical identity of an account in an organization: `<account>/<org>` when the
    /// organization is known, else `<account>`.
    pub fn of(account_uuid: &str, org_uuid: Option<&str>) -> AccountId {
        match org_uuid.filter(|o| !o.is_empty()) {
            Some(org) => AccountId(format!("{account_uuid}/{org}")),
            None => AccountId(account_uuid.to_string()),
        }
    }

    pub fn new(s: impl Into<String>) -> AccountId {
        AccountId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The user part (`accountUuid`), before any `/`.
    pub fn user_uuid(&self) -> &str {
        self.0.split_once('/').map_or(self.0.as_str(), |(u, _)| u)
    }

    /// The organization part (`organizationUuid`), when present.
    pub fn org_uuid(&self) -> Option<&str> {
        self.0
            .split_once('/')
            .map(|(_, o)| o)
            .filter(|o| !o.is_empty())
    }

    /// Whether this id carries an organization uuid (as opposed to a legacy bare `accountUuid`).
    pub fn has_org(&self) -> bool {
        self.org_uuid().is_some()
    }

    /// A bare id (an older `session-owner` record without an organization) completed with the
    /// organization it must be in: the seat's own account when it is the same user, else the one
    /// known organization of that user. Two known organizations and no matching seat leave it
    /// bare: a guess would book one org's spend to the other.
    pub fn completed(self, seat: Option<&AccountId>, known: &[AccountId]) -> AccountId {
        if self.has_org() {
            return self;
        }
        if let Some(s) = seat.filter(|s| s.has_org() && s.user_uuid() == self.as_str()) {
            return s.clone();
        }
        let mut same = known
            .iter()
            .filter(|k| k.has_org() && k.user_uuid() == self.as_str());
        match (same.next(), same.next()) {
            (Some(one), None) => one.clone(),
            (None, _) | (Some(_), Some(_)) => self,
        }
    }

    /// The first 8 characters, for a name of last resort (`acct-1a2b3c4d`).
    pub fn short(&self) -> &str {
        short(&self.0)
    }
}

/// Short display fallback for an account id: `1a2b3c4d/9e8f7a6b` when an organization uuid is
/// present, else `1a2b3c4d`.
pub fn short_account(id: &str) -> String {
    if let Some((u, o)) = id.split_once('/')
        && !o.is_empty()
    {
        format!("{}/{}", short(u), short(o))
    } else {
        short(id).to_string()
    }
}

#[cfg(test)]
impl From<&str> for AccountId {
    fn from(s: &str) -> AccountId {
        AccountId(s.to_string())
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The first 8 characters of `s` (by character, so a multi-byte one is never cut).
pub fn short(s: &str) -> &str {
    match s.char_indices().nth(8) {
        Some((i, _)) => s.get(..i).unwrap_or(s),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids_never_panic() {
        assert_eq!(SessionId::new("").short(), "");
        assert_eq!(SessionId::new("s1").short(), "s1");
        assert_eq!(SessionId::new("12345678").short(), "12345678");
        assert_eq!(
            SessionId::new("0f9e1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b").short(),
            "0f9e1a2b"
        );
        // Multi-byte characters are kept whole: byte slicing at 8 would split "é".
        assert_eq!(SessionId::new("abcdefgé-xyz").short(), "abcdefgé");
        assert_eq!(SessionId::new("ééééééééé").short(), "éééééééé");
        assert_eq!(AccountId::new("ab").short(), "ab");
    }

    #[test]
    fn seat_ids_come_from_the_path_only() {
        let a = SeatId::of_canonical_dir(Path::new("/h/.claude-work"));
        assert_eq!(a, SeatId::of_canonical_dir(Path::new("/h/.claude-work")));
        assert_ne!(a, SeatId::of_canonical_dir(Path::new("/h/.claude-home")));
        assert_eq!(a.as_str().len(), SeatId::LEN);
        assert!(SeatId::looks_like_id(a.as_str()));
        assert!(!SeatId::looks_like_id("work"));
        assert!(!SeatId::looks_like_id("0123456789ABCDEF"));
        // The same digest the status line has always named its files with.
        let full = hex::encode(Sha256::digest(b"/h/.claude-work"));
        assert_eq!(a.as_str(), &full[..16]);
    }

    #[test]
    fn account_ids_carry_user_and_organization() {
        let bare = AccountId::of("acc-123456789", None);
        assert_eq!(bare.as_str(), "acc-123456789");
        assert_eq!(bare.user_uuid(), "acc-123456789");
        assert_eq!(bare.org_uuid(), None);
        assert!(!bare.has_org());
        assert_eq!(short_account(bare.as_str()), "acc-1234");

        let empty_org = AccountId::of("acc-123456789", Some(""));
        assert_eq!(empty_org, bare);

        let scoped = AccountId::of("acc-123456789", Some("org-abcdef0123"));
        assert_eq!(scoped.as_str(), "acc-123456789/org-abcdef0123");
        assert_eq!(scoped.user_uuid(), "acc-123456789");
        assert_eq!(scoped.org_uuid(), Some("org-abcdef0123"));
        assert!(scoped.has_org());
        assert_eq!(short_account(scoped.as_str()), "acc-1234/org-abcd");
        assert_ne!(
            scoped,
            AccountId::of("acc-123456789", Some("org-other")),
            "two organizations of the same user are distinct accounts"
        );
    }

    #[test]
    fn a_bare_owner_is_completed_only_when_the_organization_is_certain() {
        let bare = || AccountId::new("u1");
        let o1 = AccountId::of("u1", Some("o1"));
        let o2 = AccountId::of("u1", Some("o2"));
        let other = AccountId::of("u2", Some("o9"));
        // The seat runs the same user: its organization.
        assert_eq!(bare().completed(Some(&o2), std::slice::from_ref(&o1)), o2);
        // Another user in the seat: the one known organization of this user.
        assert_eq!(
            bare().completed(Some(&other), std::slice::from_ref(&o1)),
            o1
        );
        // Two known organizations and no matching seat: stays bare.
        assert_eq!(
            bare().completed(Some(&other), &[o1.clone(), o2.clone()]),
            bare()
        );
        assert_eq!(bare().completed(None, &[]), bare());
        // A scoped id is never changed.
        assert_eq!(
            o1.clone().completed(Some(&o2), std::slice::from_ref(&o2)),
            o1
        );
    }
}

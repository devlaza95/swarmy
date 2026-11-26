//! The login vault: saved OAuth logins of every harness (Claude Code, Antigravity), and switching
//! which one a seat uses.
//!
//! A *login* is the secret object of a seat's credentials (Claude Code's `claudeAiOauth`, agy's
//! whole `antigravity-oauth-token`; an OAuth grant) plus facts about it, named by the fingerprint
//! of its refresh token. It belongs to one harness and only goes into seats of that harness. A
//! *seat* is a harness's config dir; one login may be in several seats at once (`mounted`).
//! Claude Code refreshes the access token every few hours and, on most refreshes, rotates the
//! refresh token too (verified on 2.1.332), so a saved copy goes stale while the login is in
//! use: the vault tracks which seat each login is mounted in and, before every change, takes the
//! seat's current tokens back first. A rotated refresh token is followed explicitly
//! (`VaultTx::sync_seat`), so the login keeps its identity.
//!
//! Secrets live in `vault/secrets.json` with mode 0600 — the same protection Claude Code gives the
//! very same tokens in `.credentials.json` on Linux and Windows.
//!
//! Reading needs no lock: every file is replaced by an atomic rename, so a reader sees a whole
//! old or a whole new file. Every change goes through [`Vault::edit`], which holds the vault's
//! OS lock, re-reads the files under it, and saves them through a [`VaultTx`].

// A new variant of a domain enum (a harness, a window kind, a dim) must be handled where it
// is matched, not fall into a catch-all (reviews A-H7, B-M24, B-L3).
#![deny(clippy::wildcard_enum_match_arm)]
// No result ignored, no string cut by bytes, and (where there are none) no silent numeric
// casts: the strict lints switched on, in production code, where they hold already (reviews
// A-H7, B-M24).
#![cfg_attr(not(test), deny(clippy::let_underscore_must_use))]
#![cfg_attr(not(test), deny(clippy::string_slice))]
#![cfg_attr(not(test), deny(clippy::cast_possible_truncation))]
#![cfg_attr(not(test), deny(clippy::cast_possible_wrap))]
#![cfg_attr(not(test), deny(clippy::cast_sign_loss))]

mod logins;
mod seats;
mod store;
mod switch;

mod new_login;
use logins::{Index, read_index};
pub use new_login::{
    NewLogin, new_login, new_login_recipe, new_login_scratch, parse_import, save_new_login,
};

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claude::config_json;
use crate::claude::credentials::{self, LoginMeta, Secret};
use crate::claude::seats::{Seat, SeatId};
use crate::core::harness::Harness;
use crate::core::ids::LoginId;
pub use crate::errors::VaultError;
use store::{Secrets, atomic_write};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Login {
    pub id: crate::core::ids::LoginId,
    pub label: String,
    /// The harness whose seats this login fits (logins saved before harnesses: Claude Code).
    #[serde(default)]
    pub harness: Harness,
    pub account_uuid: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub meta: LoginMeta,
    /// `oauthAccount` from the seat's `.claude.json` at capture time, restored on a switch.
    pub oauth_account: Option<Value>,
    pub captured_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The seats this login is currently written into (one grant may serve several seats).
    /// `logins.json` before v4 held one seat or `null`; both still read.
    #[serde(default, deserialize_with = "one_or_many")]
    pub mounted: Vec<SeatId>,
    /// A seat label an older swarmy stored in `mounted`, not matched to a seat yet
    /// (`Index::adopt_seats`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mounted_label: Option<String>,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<SeatId>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<SeatId>),
        One(Option<SeatId>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::Many(v) => v,
        OneOrMany::One(o) => o.into_iter().collect(),
    })
}

impl Login {
    pub fn is_in(&self, seat: &SeatId) -> bool {
        self.mounted.contains(seat)
    }
}

/// What a seat holds, as the vault sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeatState {
    pub fingerprint: Option<String>,
    /// The account from Claude Code's profile (`<accountUuid>/<organizationUuid>` when known),
    /// when it is known to belong to the current login.
    pub account_uuid: Option<String>,
    /// The login changed after the profile was read: the account behind it is not known yet.
    pub unverified_since: Option<DateTime<Utc>>,
    /// When the vault last switched this seat's login.
    pub switched_at: Option<DateTime<Utc>>,
    /// The saved login that holds exactly these tokens.
    pub saved_as: Option<String>,
}

/// How long a change waits for another swarmy (process or thread) to finish its own.
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Copies kept of each of the vault's own files, and of each seat file per seat.
const VAULT_BACKUPS: usize = 30;
/// How long a grant taken out of the vault is kept in `retired.json`.
const RETIRED_DAYS: i64 = 90;
const SEAT_BACKUPS: usize = 20;
/// A read-only view of the vault, as of the last `open` or `edit`.
pub struct Vault {
    dir: PathBuf,
    index: Index,
}

/// What a seat holds right now.
#[derive(Debug, Clone)]
pub struct SeatLogin {
    pub secret: Option<Secret>,
    /// When the credentials file was last written.
    pub written_at: Option<DateTime<Utc>>,
    pub oauth_account: Option<Value>,
    pub account: Option<config_json::Account>,
}

/// Why the vault cannot keep `harness`'s logins on this machine, when that is known up front
/// (Claude Code on macOS keeps them in the Keychain), so the dashboard and the commands say so
/// before anything is tried instead of after a login was made.
pub fn unavailable(harness: Harness) -> Option<VaultError> {
    unavailable_on(harness, std::env::consts::OS)
}

/// Whether `harness` keeps its login in the macOS Keychain here (so a seat that shows none is not
/// logged out).
pub fn in_keychain(harness: Harness) -> bool {
    matches!(unavailable(harness), Some(VaultError::Keychain))
}

fn unavailable_on(harness: Harness, os: &str) -> Option<VaultError> {
    match (harness, os) {
        (Harness::Claude, "macos") => Some(VaultError::Keychain),
        (Harness::Claude | Harness::Agy | Harness::Codex, _) => None,
    }
}

#[cfg(test)]
#[test]
fn claude_logins_are_unavailable_on_macos_only() {
    assert!(matches!(
        unavailable_on(Harness::Claude, "macos"),
        Some(VaultError::Keychain)
    ));
    assert!(unavailable_on(Harness::Claude, "linux").is_none());
    assert!(unavailable_on(Harness::Claude, "windows").is_none());
    assert!(unavailable_on(Harness::Agy, "macos").is_none());
    assert!(unavailable_on(Harness::Codex, "linux").is_none());
}

pub fn read_seat(seat: &Seat) -> anyhow::Result<SeatLogin> {
    let path = seat.credentials_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match seat.harness {
                Harness::Claude if cfg!(target_os = "macos") => {
                    return Err(VaultError::Keychain.into());
                }
                Harness::Agy if !cfg!(target_os = "linux") => {
                    return Err(VaultError::Keyring(seat.harness.display_name().into()).into());
                }
                Harness::Claude | Harness::Agy | Harness::Codex => {}
            }
            None
        }
        Err(e) => {
            return Err(e).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    let written_at = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(DateTime::<Utc>::from);
    match seat.harness {
        Harness::Codex => {
            let secret = text
                .as_deref()
                .map(crate::codex::credentials::parse_login)
                .transpose()
                .with_context(|| format!("{} is damaged", path.display()))?
                .flatten();
            // `auth.json` names its own account (the id token's claims).
            let account = secret
                .as_ref()
                .and_then(|s| crate::codex::account::account_of(s.expose()));
            Ok(SeatLogin {
                secret,
                written_at,
                oauth_account: None,
                account,
            })
        }
        Harness::Claude => {
            let secret = text
                .as_deref()
                .map(credentials::parse_login)
                .transpose()?
                .flatten();
            // A damaged `.claude.json` fails the read: saving a login without its account would
            // label it wrongly later.
            let cj = config_json::read_seat_file(&seat.claude_json)?;
            let mut account = cj.account;
            if let (Some(a), Some(sec)) = (account.as_mut(), secret.as_ref()) {
                a.apply_credentials(&sec.meta());
            }
            Ok(SeatLogin {
                secret,
                written_at,
                oauth_account: cj.oauth_account_raw,
                account,
            })
        }
        Harness::Agy => {
            let secret = text
                .as_deref()
                .map(crate::agy::credentials::parse_login)
                .transpose()
                .with_context(|| format!("{} is damaged", path.display()))?
                .flatten();
            // The token file names its own account: there is no separate profile.
            let account = secret
                .as_ref()
                .and_then(|s| crate::agy::account::account_of(s.expose()));
            Ok(SeatLogin {
                secret,
                written_at,
                oauth_account: None,
                account,
            })
        }
    }
}

/// The index as a reader sees it: labels an older version duplicated are told apart the way the
/// next change will save them (`VaultTx::begin`), so a label never names two logins.
fn read_view(dir: &Path) -> anyhow::Result<Index> {
    let (mut index, _) = read_index(dir)?;
    index.dedupe_labels();
    Ok(index)
}

/// One line in `vault/journal.log`, the human-readable history of changes. The change itself
/// is already saved; a journal that cannot be written is reported on the debug channel.
fn journal(dir: &Path, line: &str) {
    use std::io::Write;
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("journal.log"))
        .and_then(|mut f| writeln!(f, "{} {line}", Utc::now().to_rfc3339()));
    if let Err(e) = written {
        crate::diag::log("vault", &format!("journal.log: {e}"));
    }
}

/// A seat label as a directory name.
fn safe_name(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if n.trim_matches('.').is_empty() {
        format!("_{n}")
    } else {
        n
    }
}

impl Vault {
    /// Reads the vault without locking it.
    pub fn open(dir: &Path) -> anyhow::Result<Vault> {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        store::restrict_dir(dir)?;
        let index = read_view(dir)?;
        Ok(Vault {
            dir: dir.to_path_buf(),
            index,
        })
    }

    pub fn logins(&self) -> &[Login] {
        &self.index.logins
    }

    /// Whether `label` can name a new login of `harness`: checked before its harness is started,
    /// so a sign-in is not made for nothing (the save checks again under the lock).
    pub fn check_new_label(&self, label: &str, harness: Harness) -> anyhow::Result<()> {
        self.index.check_label(label, None, harness)
    }

    /// The login a label, an id or an email names (an error when it names none or several).
    pub fn find(&self, key: &str) -> Result<&Login, VaultError> {
        self.index.find(key)
    }

    /// [`find`](Self::find) among the logins of `harness` (a label names one login per harness).
    pub fn find_in(&self, key: &str, harness: Option<Harness>) -> Result<&Login, VaultError> {
        self.index.find_in(key, harness)
    }

    pub fn store_name(&self) -> &'static str {
        store::STORE_NAME
    }

    /// The only way to change the vault: takes the exclusive lock, re-reads the files under it
    /// (never a copy read before), runs `f`, saves what changed (after a backup of each changed
    /// file) and releases the lock. When `f` fails, nothing it staged is saved. Afterwards `self`
    /// shows the vault as saved.
    pub fn edit<T>(
        &mut self,
        f: impl FnOnce(&mut VaultTx<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.edit_within(LOCK_TIMEOUT, f)
    }

    /// [`Vault::edit`] when no other swarmy is changing the vault right now, else `None` at once
    /// (nothing waits, nothing is written): for bookkeeping a reader may do on the side.
    pub fn try_edit<T>(
        &mut self,
        f: impl FnOnce(&mut VaultTx<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        match self.edit_within(std::time::Duration::ZERO, f) {
            Ok(v) => Ok(Some(v)),
            Err(e)
                if e.downcast_ref::<VaultError>()
                    .is_some_and(|v| matches!(v, VaultError::Locked(_))) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// What every seat holds, as the vault sees it, without the lock and without writing
    /// anything: for read paths (`Ctx::load`, reports, `doctor`), which must never wait for or
    /// fail on another swarmy's change. The bookkeeping this would do (copies kept current,
    /// rotations followed, first-seen times) is saved only by changes: `swarmy accounts`, a
    /// switch, and the dashboard's [`Vault::try_edit`].
    pub fn seat_states(
        &self,
        seats: &[Seat],
    ) -> anyhow::Result<std::collections::BTreeMap<SeatId, SeatState>> {
        VaultTx::detached(&self.dir)?.seat_states(seats)
    }

    fn edit_within<T>(
        &mut self,
        timeout: std::time::Duration,
        f: impl FnOnce(&mut VaultTx<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let lock = store::Lock::acquire(&self.dir.join("lock"), timeout)?;
        let mut tx = VaultTx::begin(&self.dir, &lock)?;
        let out = f(&mut tx).and_then(|v| tx.save().map(|()| v));
        match &out {
            Ok(_) => self.index = tx.index,
            Err(_) => {
                if let Ok(index) = read_view(&self.dir) {
                    self.index = index;
                }
            }
        }
        out
    }
}

/// A change in progress: the vault as read under the lock, plus what is on disk, so that saving
/// writes only what changed. Only [`Vault::edit`] makes one.
pub struct VaultTx<'a> {
    dir: PathBuf,
    index: Index,
    secrets: Secrets,
    disk_logins: Option<String>,
    disk_secrets: Secrets,
    disk_secrets_text: Option<String>,
    /// `None` only for a read view (`VaultTx::detached`), which cannot save.
    lock: Option<&'a store::Lock>,
    /// The one instant of this change: what it records is dated by it (review A-M6).
    now: DateTime<Utc>,
}

impl<'a> VaultTx<'a> {
    fn begin(dir: &Path, lock: &'a store::Lock) -> anyhow::Result<VaultTx<'a>> {
        let (mut index, disk_logins) = read_index(dir)?;
        let secrets_path = dir.join("secrets.json");
        let disk_secrets_text = store::read_optional(&secrets_path)?;
        let disk_secrets: Secrets = match &disk_secrets_text {
            Some(t) => serde_json::from_str(t)
                .map_err(|e| anyhow::Error::new(e).context(VaultError::Damaged("secrets.json")))?,
            None => Secrets::new(),
        };
        for (old, new) in index.dedupe_labels() {
            journal(dir, &format!("duplicate label '{old}' renamed to '{new}'"));
        }
        Ok(VaultTx {
            dir: dir.to_path_buf(),
            index,
            secrets: disk_secrets.clone(),
            disk_logins,
            disk_secrets,
            disk_secrets_text,
            lock: Some(lock),
            now: Utc::now(),
        })
    }

    /// The vault as it is on disk, without the lock and without its secrets: the state for a
    /// read view ([`Vault::seat_states`]). It never saves.
    fn detached(dir: &Path) -> anyhow::Result<VaultTx<'static>> {
        let (mut index, disk_logins) = read_index(dir)?;
        index.dedupe_labels();
        Ok(VaultTx {
            dir: dir.to_path_buf(),
            index,
            secrets: Secrets::new(),
            disk_logins,
            disk_secrets: Secrets::new(),
            disk_secrets_text: None,
            lock: None,
            now: Utc::now(),
        })
    }

    /// Writes what changed so far. New secrets go to disk before the index that lists them, and
    /// removed ones after the index that no longer does, so a reader never finds a listed login
    /// without its secret. Each changed file is backed up first.
    fn save(&mut self) -> anyhow::Result<()> {
        if self.lock.is_none() {
            anyhow::bail!("a read view of the vault cannot be saved");
        }
        let logins_text = serde_json::to_string_pretty(&self.index)?;
        let logins_changed = self.disk_logins.as_deref() != Some(logins_text.as_str());
        let secrets_changed = self.secrets != self.disk_secrets;
        if !logins_changed && !secrets_changed {
            return Ok(());
        }
        // Tests (debug builds) refuse a vault that breaks an invariant; a release build says so
        // in the journal and saves anyway, so an old damaged vault never locks its owner out.
        let broken = self.index.violations(|id| self.secrets.contains_key(id));
        if !broken.is_empty() {
            let msg = format!("vault invariants broken: {}", broken.join("; "));
            journal(&self.dir, &msg);
            crate::diag::log("vault", &msg);
            if cfg!(debug_assertions) {
                anyhow::bail!(msg);
            }
        }
        store::backup(
            &self.dir.join("backups").join("vault"),
            &[
                (
                    "logins.json",
                    self.disk_logins.as_deref().filter(|_| logins_changed),
                ),
                (
                    "secrets.json",
                    self.disk_secrets_text
                        .as_deref()
                        .filter(|_| secrets_changed),
                ),
            ],
            VAULT_BACKUPS,
        )?;
        let secrets_path = self.dir.join("secrets.json");
        if secrets_changed {
            self.retire_replaced_grants(self.now)?;
        }
        if secrets_changed {
            let mut grown = self.disk_secrets.clone();
            grown.extend(self.secrets.clone());
            if grown != self.disk_secrets {
                let text = serde_json::to_string(&grown)?;
                atomic_write(&secrets_path, text.as_bytes())?;
                self.disk_secrets = grown;
                self.disk_secrets_text = Some(text);
            }
        }
        if logins_changed {
            atomic_write(&self.dir.join("logins.json"), logins_text.as_bytes())?;
            self.disk_logins = Some(logins_text);
        }
        if self.secrets != self.disk_secrets {
            let text = serde_json::to_string(&self.secrets)?;
            atomic_write(&secrets_path, text.as_bytes())?;
            self.disk_secrets = self.secrets.clone();
            self.disk_secrets_text = Some(text);
        }
        Ok(())
    }

    pub fn find(&self, key: &str) -> Result<&Login, VaultError> {
        self.index.find(key)
    }

    fn secret(&self, id: &LoginId) -> Option<Secret> {
        let harness = self
            .index
            .logins
            .iter()
            .find(|l| l.id == *id)
            .map_or(Harness::Claude, |l| l.harness);
        self.secrets
            .get(id)
            .cloned()
            .map(|v| Secret::of(harness, v))
    }

    /// Keeps every grant this save takes out of `secrets.json` (a login forgotten, or its copy
    /// replaced by another grant) in `retired.json` for [`RETIRED_DAYS`]: the backup ring turns
    /// over in about a day of access-token refreshes, so a grant overwritten by mistake could be
    /// recovered for only that long (review B-M28). A refreshed access token of the same grant
    /// is not kept.
    fn retire_replaced_grants(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        let fp = |id: &LoginId, v: &Value| {
            let harness = self
                .index
                .logins
                .iter()
                .find(|l| l.id == *id)
                .map_or(Harness::Claude, |l| l.harness);
            Secret::of(harness, v.clone()).meta().fingerprint
        };
        let gone: Vec<(&LoginId, &Value)> = self
            .disk_secrets
            .iter()
            .filter(|(id, old)| match self.secrets.get(*id) {
                None => true,
                Some(new) => fp(id, old) != fp(id, new),
            })
            .collect();
        let path = self.dir.join("retired.json");
        let mut kept: serde_json::Map<String, Value> = match store::read_optional(&path)? {
            None => serde_json::Map::new(),
            Some(t) => match serde_json::from_str(&t) {
                Ok(m) => m,
                // Never written over: what it holds may be the only copy of a grant.
                Err(e) => {
                    crate::diag::log(
                        "vault",
                        &format!("retired.json is damaged, left as is: {e}"),
                    );
                    return Ok(());
                }
            },
        };
        let before = kept.len();
        let cutoff = (now - chrono::Duration::days(RETIRED_DAYS)).timestamp_millis();
        kept.retain(|_, v| {
            v.get("at")
                .and_then(Value::as_i64)
                .is_some_and(|t| t >= cutoff)
        });
        let pruned = kept.len() != before;
        for (id, value) in &gone {
            let label = self.index.label_of(id);
            kept.insert(
                format!("{id}@{}", now.timestamp_millis()),
                serde_json::json!({"at": now.timestamp_millis(), "login": label, "secret": value}),
            );
        }
        if gone.is_empty() && !pruned {
            return Ok(());
        }
        atomic_write(&path, serde_json::to_string(&kept)?.as_bytes())
    }

    fn set_secret(&mut self, id: &LoginId, secret: &Secret) {
        self.secrets.insert(id.clone(), secret.expose().clone());
    }

    fn journal(&self, line: &str) {
        // A read view changes nothing, so it has nothing to tell.
        if self.lock.is_some() {
            journal(&self.dir, line);
        }
    }

    /// `backups/seats/<seat id>`: kept per seat whatever its label. Backups an older swarmy kept
    /// under the seat's label move there first.
    fn seat_backups(&self, seat: &Seat) -> anyhow::Result<PathBuf> {
        let root = self.dir.join("backups").join("seats");
        let dir = root.join(seat.id.as_str());
        let old = root.join(safe_name(&seat.label));
        if !dir.exists() && old.is_dir() {
            crate::fsutil::move_into_place(&old, &dir)
                .with_context(|| format!("cannot move {} to {}", old.display(), dir.display()))?;
        }
        Ok(dir)
    }

    /// Stores a login. When `mounted_in` names the seat it came from, the entry already mounted
    /// there is updated (its tokens are newer); otherwise an entry with the same
    /// grant is updated, or a new one is created. A given `label` must not name another login.
    pub fn put(
        &mut self,
        secret: &Secret,
        oauth_account: Option<Value>,
        account: Option<&config_json::Account>,
        label: Option<&str>,
        mounted_in: Option<&SeatId>,
    ) -> anyhow::Result<LoginId> {
        let choice = match label {
            Some(l) => Label::Given(l),
            None => Label::Default(
                account
                    .and_then(|a| {
                        a.email
                            .as_ref()
                            .map(|e| e.split('@').next().unwrap_or(e).to_string())
                    })
                    .unwrap_or_else(|| "login".into()),
            ),
        };
        self.put_labelled(secret, oauth_account, account, choice, mounted_in)
    }

    fn put_labelled(
        &mut self,
        secret: &Secret,
        oauth_account: Option<Value>,
        account: Option<&config_json::Account>,
        label: Label<'_>,
        mounted_in: Option<&SeatId>,
    ) -> anyhow::Result<LoginId> {
        let meta = secret.meta();
        let now = self.now;
        let account_with_meta: Option<config_json::Account> = account.map(|a| {
            let mut a = a.clone();
            a.apply_credentials(&meta);
            a
        });
        let account = account_with_meta.as_ref();
        let account_id = account.map(|a| a.id().to_string());
        let logins = &self.index.logins;
        // The grant decides first: a grant the vault holds is that login, whichever is mounted
        // in the seat (review B-L6); else the seat's login takes it (a refresh followed).
        let existing = logins
            .iter()
            .find(|l| l.harness == secret.harness() && l.meta.fingerprint == meta.fingerprint)
            .or_else(|| {
                mounted_in
                    .and_then(|seat| {
                        logins
                            .iter()
                            .find(|l| l.is_in(seat) && l.harness == secret.harness())
                    })
                    .filter(|l| {
                        account_id.is_none()
                            || l.account_uuid.is_none()
                            || l.account_uuid == account_id
                    })
            })
            .map(|l| l.id.clone());
        // An existing login keeps its label unless one is given; a new one gets a free one.
        let new_label = match (&label, &existing) {
            (Label::Given(l), id) => {
                self.index.check_label(l, id.as_ref(), secret.harness())?;
                Some(l.to_string())
            }
            (Label::Default(_), Some(_)) => None,
            (Label::Default(base), None) => Some(self.index.unique_label(base, secret.harness())),
        };
        let id = match existing {
            Some(id) => {
                let l = self
                    .index
                    .find_mut(&id)
                    .context("the vault index changed under the lock")?;
                l.meta = meta;
                l.updated_at = now;
                if oauth_account.is_some() {
                    l.oauth_account = oauth_account;
                }
                if let Some(a) = account {
                    l.account_uuid = Some(a.id().to_string());
                    l.email = a.email.clone().or(l.email.take());
                    l.plan = a.effective_tier().or(l.plan.take());
                }
                if let Some(lb) = new_label {
                    l.label = lb;
                }
                id
            }
            None => {
                let id = self.index.new_login_id(&meta.fingerprint);
                self.index.logins.push(Login {
                    id: id.clone(),
                    label: new_label.context("a new login without a label")?,
                    harness: secret.harness(),
                    account_uuid: account_id,
                    email: account.and_then(|a| a.email.clone()),
                    plan: account.and_then(|a| a.effective_tier()),
                    meta,
                    oauth_account,
                    captured_at: now,
                    updated_at: now,
                    mounted: Vec::new(),
                    mounted_label: None,
                });
                id
            }
        };
        if let Some(seat) = mounted_in {
            self.mount(&id, seat);
        }
        self.set_secret(&id, secret);
        Ok(id)
    }

    /// Takes the seat's current login into the vault.
    pub fn capture(&mut self, seat: &Seat, label: Option<&str>) -> anyhow::Result<Option<LoginId>> {
        self.sync(std::slice::from_ref(seat))?;
        let s = read_seat(seat)?;
        let Some(secret) = s.secret.clone() else {
            return Ok(None);
        };
        let id = self.save_seat_login(seat, &s, &secret, label)?;
        self.journal(&format!(
            "capture seat={} login={}",
            seat.label,
            self.index.label_of(&id).as_deref().unwrap_or("?")
        ));
        Ok(Some(id))
    }

    /// Saves the login a seat holds; a new one is named after the seat unless a label is given.
    /// When the account behind it is not confirmed yet, it is saved without one, never under the
    /// stale profile's name.
    fn save_seat_login(
        &mut self,
        seat: &Seat,
        s: &SeatLogin,
        secret: &Secret,
        label: Option<&str>,
    ) -> anyhow::Result<LoginId> {
        let state = self.observe(seat, s);
        let choice = match label {
            Some(l) => Label::Given(l),
            None => Label::Default(seat.label.clone()),
        };
        if state.unverified_since.is_some() {
            return self.put_labelled(secret, None, None, choice, Some(&seat.id));
        }
        self.put_labelled(
            secret,
            s.oauth_account.clone(),
            s.account.as_ref(),
            choice,
            Some(&seat.id),
        )
    }

    pub fn rename(&mut self, key: &str, new_label: &str) -> anyhow::Result<()> {
        let (id, harness) = self.find(key).map(|l| (l.id.clone(), l.harness))?;
        self.index.check_label(new_label, Some(&id), harness)?;
        if let Some(l) = self.index.find_mut(&id) {
            l.label = new_label.to_string();
        }
        Ok(())
    }

    /// Forgets a login. Its secret leaves the disk only after the index no longer lists it.
    pub fn remove(&mut self, key: &str, force: bool) -> anyhow::Result<Login> {
        let l = self.find(key).cloned()?;
        let names: Vec<String> = l
            .mounted
            .iter()
            .map(|m| self.index.seat_name(m))
            .chain(l.mounted_label.clone())
            .collect();
        let in_seat = (!names.is_empty()).then(|| names.join(", "));
        if let Some(seat) = in_seat
            && !force
        {
            return Err(VaultError::InUse {
                login: l.label,
                seat,
            }
            .into());
        }
        self.index.logins.retain(|x| x.id != l.id);
        self.secrets.remove(&l.id);
        self.journal(&format!("remove login={}", l.label));
        Ok(l)
    }
}

/// How a stored login is named.
enum Label<'a> {
    /// Chosen by the user: refused when another login has it.
    Given(&'a str),
    /// A base for a new login's label (made unique); an existing login keeps its own.
    Default(String),
}

#[cfg(test)]
mod tests;

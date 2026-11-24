//! The catalogue of failures a user sees (review L3): each has a stable code, so a message can
//! be looked up, scripted against and tested, and callers can react to a kind of failure (the
//! dashboard tells "the vault is busy" from "the vault is damaged"). Errors are made where the
//! failure is found (`vault`, `config`, `cli::setup`, `index`, `app`) and travel inside
//! `anyhow::Error` to the edges, which print `swarmy: <message> [<code>]` ([`code_of`]).

use std::path::PathBuf;

/// The saved logins (`src/vault`).
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("another swarmy is changing the vault (still busy after {0:?})")]
    Locked(std::time::Duration),
    #[error("vault/{0} is damaged")]
    Damaged(&'static str),
    #[error("vault/logins.json was written by a newer swarmy (format {0}); update swarmy")]
    NewerFormat(u32),
    #[error("no login '{0}' in the vault (see `swarmy accounts`)")]
    NoLogin(String),
    #[error("'{key}' names several logins: {}; use the label", .logins.join(", "))]
    AmbiguousLogin { key: String, logins: Vec<String> },
    #[error("a {harness} login labelled '{label}' already exists")]
    LabelTaken { label: String, harness: String },
    #[error("a label is needed")]
    LabelEmpty,
    #[error("login '{login}' is a {login_harness} login; seat '{seat}' runs {seat_harness}")]
    WrongHarness {
        login: String,
        login_harness: String,
        seat: String,
        seat_harness: String,
    },
    #[error(
        "login '{login}' is in use in seat '{seat}'; pass --force to forget it anyway (the seat keeps working)"
    )]
    InUse { login: String, seat: String },
    #[error("the secret of '{0}' is missing from the vault")]
    MissingSecret(String),
    #[error(
        "on macOS Claude Code keeps its login in the Keychain, so swarmy cannot save or switch Claude Code logins there yet (the dashboard, limits and reports work as usual)"
    )]
    Keychain,
    #[error("{0} keeps its login in the OS keyring here; the vault cannot read it")]
    Keyring(String),
    #[error("the vault does not keep {0} logins")]
    Unsupported(String),
    #[error(
        "'{login}' is in seat {seats} already: a {harness} login can be in one seat only (its refresh token is single-use, so two seats would sign each other out); put another login into that seat first"
    )]
    OneSeatOnly {
        login: String,
        harness: String,
        seats: String,
    },
    #[error(
        "{} kept changing while swarmy tried to write it (Claude Code is writing it); nothing was overwritten, try again",
        .0.display()
    )]
    SeatBusy(PathBuf),
    #[error(
        "the switch of seat '{seat}' stopped half way: {what}. The vault records what the seat holds now; run the switch again"
    )]
    SwitchIncomplete { seat: String, what: String },
    #[error(
        "seat '{seat}' holds another login now, one the vault has not saved; nothing moved. Save it first (`c` in the dashboard, `swarmy accounts capture {seat}`), then move again"
    )]
    SeatChanged { seat: String },
    #[error(
        "{harness} is running in seat '{seat}' ({what}). It keeps the login it loaded, and a refresh of it would undo the switch (its refresh tokens are single-use). Quit it there first"
    )]
    HarnessRunning {
        seat: String,
        harness: String,
        what: String,
    },
}

/// `config.toml`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("cannot read {path}: {source}")]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A seat's `settings.json` (Claude Code's) that cannot be read: its transcript retention
    /// (`cleanupPeriodDays`) is unknown, never taken for the default silently.
    #[error(
        "{path} is damaged ({message}): transcript retention unknown, Claude Code's default assumed"
    )]
    SeatSettings { path: PathBuf, message: String },
}

/// `swarmy setup` (Claude Code's `settings.json`).
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("{0} is not valid JSON; fix or remove it first (nothing was changed)")]
    SettingsDamaged(PathBuf),
}

/// The index (`src/index`).
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("the index was written by a newer swarmy ({what} version {version}); update swarmy")]
    NewerVersion { what: &'static str, version: u32 },
    /// Another process held the index's write lock past the busy timeout (a compaction chunk, a
    /// `VACUUM`, a second dashboard's refresh).
    #[error("another swarmy is writing the index (a compaction or VACUUM?); try again ({0})")]
    Busy(String),
    #[error("the index is damaged ({0}); move it aside and swarmy builds a new one")]
    Damaged(String),
    #[error("no space left on the disk of the index ({0})")]
    NoSpace(String),
}

/// What a user named on the command line (`--account`, `--seat`).
#[derive(Debug, thiserror::Error)]
pub enum LookupError {
    #[error(
        "no account matches '{0}' (give an account uuid, an email or a saved login's label; `swarmy accounts` lists them)"
    )]
    NoAccount(String),
    #[error("'{key}' matches several accounts: {}; give the uuid", .candidates.join(", "))]
    AmbiguousAccount {
        key: String,
        candidates: Vec<String>,
    },
    #[error("no seat '{name}' (seats: {})", .known.join(", "))]
    NoSeat { name: String, known: Vec<String> },
}

/// A stable code per failure, for messages, scripts and tests.
pub trait Coded {
    fn code(&self) -> &'static str;
}

impl Coded for VaultError {
    fn code(&self) -> &'static str {
        match self {
            VaultError::Locked(_) => "vault-locked",
            VaultError::Damaged(_) => "vault-damaged",
            VaultError::NewerFormat(_) => "vault-newer",
            VaultError::NoLogin(_) => "vault-no-login",
            VaultError::AmbiguousLogin { .. } => "vault-ambiguous-login",
            VaultError::LabelTaken { .. } => "vault-label-taken",
            VaultError::LabelEmpty => "vault-label-empty",
            VaultError::WrongHarness { .. } => "vault-wrong-harness",
            VaultError::InUse { .. } => "vault-in-use",
            VaultError::MissingSecret(_) => "vault-missing-secret",
            VaultError::Keychain => "vault-keychain",
            VaultError::Keyring(_) => "vault-keyring",
            VaultError::Unsupported(_) => "vault-unsupported",
            VaultError::OneSeatOnly { .. } => "vault-one-seat-only",
            VaultError::SeatBusy(_) => "vault-seat-busy",
            VaultError::SwitchIncomplete { .. } => "vault-switch-incomplete",
            VaultError::HarnessRunning { .. } => "vault-harness-running",
            VaultError::SeatChanged { .. } => "vault-seat-changed",
        }
    }
}

impl Coded for ConfigError {
    fn code(&self) -> &'static str {
        match self {
            ConfigError::Invalid { .. } => "config-invalid",
            ConfigError::Unreadable { .. } => "config-unreadable",
            ConfigError::SeatSettings { .. } => "config-seat-settings",
        }
    }
}

impl Coded for SetupError {
    fn code(&self) -> &'static str {
        match self {
            SetupError::SettingsDamaged(_) => "setup-settings-damaged",
        }
    }
}

impl Coded for IndexError {
    fn code(&self) -> &'static str {
        match self {
            IndexError::NewerVersion { .. } => "index-newer",
            IndexError::Busy(_) => "index-busy",
            IndexError::Damaged(_) => "index-damaged",
            IndexError::NoSpace(_) => "index-no-space",
        }
    }
}

impl Coded for LookupError {
    fn code(&self) -> &'static str {
        match self {
            LookupError::NoAccount(_) => "no-account",
            LookupError::AmbiguousAccount { .. } => "ambiguous-account",
            LookupError::NoSeat { .. } => "no-seat",
        }
    }
}

/// The code of the first catalogued error in `e`'s chain.
pub fn code_of(e: &anyhow::Error) -> Option<&'static str> {
    e.chain().find_map(|c| {
        if let Some(x) = c.downcast_ref::<VaultError>() {
            Some(x.code())
        } else if let Some(x) = c.downcast_ref::<ConfigError>() {
            Some(x.code())
        } else if let Some(x) = c.downcast_ref::<SetupError>() {
            Some(x.code())
        } else if let Some(x) = c.downcast_ref::<IndexError>() {
            Some(x.code())
        } else {
            c.downcast_ref::<LookupError>().map(Coded::code)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_survive_context() {
        let e = anyhow::Error::from(VaultError::Locked(std::time::Duration::from_secs(30)))
            .context("cannot save the login");
        assert_eq!(code_of(&e), Some("vault-locked"));
        let e = anyhow::anyhow!("plain");
        assert_eq!(code_of(&e), None);
        let e: anyhow::Error = LookupError::AmbiguousAccount {
            key: "work".into(),
            candidates: vec!["a (u1)".into(), "b (u2)".into()],
        }
        .into();
        assert_eq!(code_of(&e), Some("ambiguous-account"));
        assert_eq!(
            e.to_string(),
            "'work' matches several accounts: a (u1), b (u2); give the uuid"
        );
    }

    /// Codes are part of the interface: each is unique.
    #[test]
    fn codes_are_unique() {
        let all = [
            VaultError::Locked(std::time::Duration::ZERO).code(),
            VaultError::Damaged("x").code(),
            VaultError::NewerFormat(3).code(),
            VaultError::NoLogin(String::new()).code(),
            VaultError::AmbiguousLogin {
                key: String::new(),
                logins: vec![],
            }
            .code(),
            VaultError::LabelTaken {
                label: String::new(),
                harness: String::new(),
            }
            .code(),
            VaultError::LabelEmpty.code(),
            VaultError::WrongHarness {
                login: String::new(),
                login_harness: String::new(),
                seat: String::new(),
                seat_harness: String::new(),
            }
            .code(),
            VaultError::InUse {
                login: String::new(),
                seat: String::new(),
            }
            .code(),
            VaultError::MissingSecret(String::new()).code(),
            VaultError::Keychain.code(),
            VaultError::Keyring(String::new()).code(),
            VaultError::OneSeatOnly {
                login: String::new(),
                harness: String::new(),
                seats: String::new(),
            }
            .code(),
            VaultError::SeatBusy(PathBuf::new()).code(),
            VaultError::SwitchIncomplete {
                seat: String::new(),
                what: String::new(),
            }
            .code(),
            VaultError::HarnessRunning {
                seat: String::new(),
                harness: String::new(),
                what: String::new(),
            }
            .code(),
            VaultError::Unsupported(String::new()).code(),
            VaultError::SeatChanged {
                seat: String::new(),
            }
            .code(),
            ConfigError::Invalid {
                path: PathBuf::new(),
                message: String::new(),
            }
            .code(),
            ConfigError::Unreadable {
                path: PathBuf::new(),
                source: std::io::Error::other("x"),
            }
            .code(),
            ConfigError::SeatSettings {
                path: PathBuf::new(),
                message: String::new(),
            }
            .code(),
            SetupError::SettingsDamaged(PathBuf::new()).code(),
            IndexError::NewerVersion {
                what: "facts",
                version: 9,
            }
            .code(),
            IndexError::Busy(String::new()).code(),
            IndexError::Damaged(String::new()).code(),
            IndexError::NoSpace(String::new()).code(),
            LookupError::NoAccount(String::new()).code(),
            LookupError::AmbiguousAccount {
                key: String::new(),
                candidates: vec![],
            }
            .code(),
            LookupError::NoSeat {
                name: String::new(),
                known: vec![],
            }
            .code(),
        ];
        let set: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(set.len(), all.len());
    }
}

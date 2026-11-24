//! The vault's files: the exclusive lock, the secrets file, safe writes and bounded backups.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::errors::VaultError;
use crate::fsutil::{self, Mode};

/// Secrets by login id: the verbatim `claudeAiOauth` objects.
pub type Secrets = BTreeMap<crate::core::ids::LoginId, Value>;

/// Where secrets are kept: a JSON file readable only by the owner.
/// How the secrets are kept, for `accounts` and `doctor`.
pub const STORE_NAME: &str = if cfg!(windows) {
    "file (your profile's permissions)"
} else {
    "file (0600)"
};

/// Reads a file that may not exist yet.
pub fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Writes through a unique temp file, fsyncs it and the directory, mode 0600 on Unix
/// (`fsutil::atomic_write`, the one implementation).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    fsutil::atomic_write(path, bytes, Mode::Private)
        .with_context(|| format!("cannot write {}", path.display()))
}

/// What identifies one version of a file: another writer's rename or rewrite changes it.
#[derive(PartialEq)]
struct Stamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    ino: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: m.modified().ok(),
        len: m.len(),
        #[cfg(unix)]
        ino: std::os::unix::fs::MetadataExt::ino(&m),
    })
}

/// How many times a write into a seat file starts again because Claude Code changed the file
/// meanwhile (or was half way through writing it), before the write gives up.
const ATTEMPTS: usize = 8;
/// The pause before reading a file again that did not parse: Claude Code may be mid-write.
const RETRY_PAUSE: Duration = Duration::from_millis(15);

/// Rewrites a file that another program (Claude Code) also rewrites: reads it, applies `change`
/// to the text, writes the result to a temp file and renames it over the target only if the file
/// has not changed since it was read; otherwise starts again from the newer text. A text that
/// `change` refuses (a torn read while Claude Code writes) is read again after a short pause.
/// After [`ATTEMPTS`] it fails ([`VaultError::SeatBusy`], or `change`'s own error when the file
/// never parsed) instead of writing over a version it has not seen. Returns false (and writes
/// nothing) when the file does not exist.
///
/// The file is another program's: it keeps its mode, and a symlink to it (a dotfile manager's)
/// stays a link, the file it points at being the one replaced (review B-L7).
pub fn rewrite_merging(
    path: &Path,
    change: impl Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    let resolved = std::fs::canonicalize(path);
    let path: &Path = match &resolved {
        Ok(p) => p,
        Err(_) => path,
    };
    let mut refused = None;
    for _ in 0..ATTEMPTS {
        let before = stamp(path);
        let Some(text) = read_optional(path)? else {
            return Ok(false);
        };
        let new = match change(&text) {
            Ok(t) => t,
            Err(e) => {
                refused = Some(e);
                std::thread::sleep(RETRY_PAUSE);
                continue;
            }
        };
        refused = None;
        let staged = fsutil::stage(path, new.as_bytes(), Mode::KeepExisting)
            .with_context(|| format!("cannot write next to {}", path.display()))?;
        // The stamp alone is not enough: on Windows a quick replace can keep the same time and
        // size, so the text itself must still be what was read.
        if stamp(path) == before && read_optional(path)?.as_deref() == Some(text.as_str()) {
            staged
                .commit(path)
                .with_context(|| format!("cannot replace {}", path.display()))?;
            return Ok(true);
        }
        // Dropped: the temp file is removed, and the next attempt starts from the newer text.
        drop(staged);
    }
    Err(match refused {
        Some(e) => e.context(format!(
            "{} did not parse in {ATTEMPTS} reads",
            path.display()
        )),
        None => VaultError::SeatBusy(path.to_path_buf()).into(),
    })
}

/// Replaces `path` with `bytes` only while it still holds exactly `expected` (`None`: no file):
/// the check and the rename are a few microseconds apart, and a change in between (another
/// writer's rename) is caught by the file's stamp. Returns false, writing nothing, when the file
/// is not what the caller read: the caller starts again from what is there now. With `bytes`
/// `None` the file is removed instead.
pub fn replace_if_unchanged(
    path: &Path,
    expected: Option<&str>,
    bytes: Option<&[u8]>,
) -> anyhow::Result<bool> {
    let staged = match bytes {
        Some(b) => Some(
            fsutil::stage(path, b, Mode::Private)
                .with_context(|| format!("cannot write next to {}", path.display()))?,
        ),
        None => None,
    };
    let before = stamp(path);
    if read_optional(path)?.as_deref() != expected || stamp(path) != before {
        return Ok(false);
    }
    match staged {
        Some(s) => s
            .commit(path)
            .with_context(|| format!("cannot replace {}", path.display()))?,
        None => match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot remove {}", path.display())),
        },
    }
    Ok(true)
}

pub fn restrict_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = dir;
    Ok(())
}

/// A backup stamp, sortable as text, free in `dir` for every `{stamp}-{kind}` name.
fn unique_stamp(dir: &Path, kinds: &[&str]) -> String {
    let mut t: DateTime<Utc> = Utc::now();
    loop {
        let s = t.format("%Y%m%dT%H%M%S%.6f").to_string();
        if kinds.iter().all(|k| !dir.join(format!("{s}-{k}")).exists()) {
            return s;
        }
        t += chrono::Duration::microseconds(1);
    }
}

/// Keeps the newest `keep` files of one kind (`*-{kind}`) in `dir`.
fn prune_kind(dir: &Path, kind: &str, keep: usize) -> anyhow::Result<()> {
    let suffix = format!("-{kind}");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&suffix))
        })
        .collect();
    files.sort();
    if files.len() > keep {
        for f in &files[..files.len() - keep] {
            std::fs::remove_file(f).with_context(|| format!("cannot remove {}", f.display()))?;
        }
    }
    Ok(())
}

/// Saves copies of several files under one stamp in `dir` (`{stamp}-{kind}`; `None` texts are
/// skipped), then keeps the newest `keep` of each kind. Names never collide, even within the same
/// second; callers hold the vault lock, so no other swarmy writes there meanwhile.
pub fn backup(dir: &Path, files: &[(&str, Option<&str>)], keep: usize) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let kinds: Vec<&str> = files.iter().map(|(k, _)| *k).collect();
    let stamp = unique_stamp(dir, &kinds);
    for (kind, text) in files {
        if let Some(t) = text {
            atomic_write(&dir.join(format!("{stamp}-{kind}")), t.as_bytes())?;
        }
    }
    for kind in kinds {
        prune_kind(dir, kind, keep)?;
    }
    Ok(())
}

/// An exclusive OS lock on `vault/lock` (`flock` on Unix). The kernel releases it when the file
/// is closed or the process dies, so there is no stale lock to take over. Locks belong to the open
/// file, so two threads of one process exclude each other too.
pub struct Lock {
    _file: std::fs::File,
}

impl Lock {
    /// Waits up to `timeout` for the lock (zero: tries once).
    pub fn acquire(path: &Path, timeout: Duration) -> anyhow::Result<Lock> {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(crate::errors::VaultError::Locked(timeout).into());
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("cannot lock {}", path.display()));
                }
            }
        }
    }
}

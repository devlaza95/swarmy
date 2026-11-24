//! The one way swarmy replaces a file: [`atomic_write`] (review L5/M8). The vault, `swarmy setup`
//! (Claude Code's `settings.json`), the fleet summary and the status-line files all go through
//! it, so every write has the same guarantee: a crash leaves the old file or the new one, never
//! half of one, and two writers (two threads, two processes) never share a temp file.

pub mod locks;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The permissions of the new file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 0600 on Unix: the vault, swarmy's own state, Claude Code's credentials.
    Private,
    /// The mode of the file being replaced (0644 under the usual umask for a new one): files
    /// that belong to someone else, such as Claude Code's `settings.json`.
    KeepExisting,
}

/// Temp-file names are unique per process, per thread and per call.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `.<name>.swarmy-<pid>-<thread>-<seq>.tmp` next to `path`.
fn temp_path(path: &Path) -> std::io::Result<PathBuf> {
    let invalid = |what: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{what}: {}", path.display()),
        )
    };
    let dir = path
        .parent()
        .ok_or_else(|| invalid("path without a parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("path without a file name"))?
        .to_string_lossy();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    // `ThreadId`'s Debug is `ThreadId(N)`; keep the digits.
    let thread: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    Ok(dir.join(format!(
        ".{name}.swarmy-{}-{thread}-{seq}.tmp",
        std::process::id()
    )))
}

/// A temp file written and fsynced next to its target, not yet visible under the target's name.
/// Dropped without [`Staged::commit`], it is removed.
#[derive(Debug)]
pub struct Staged {
    tmp: PathBuf,
    committed: bool,
}

/// Writes `bytes` to a new temp file next to `path` and fsyncs it.
pub fn stage(path: &Path, bytes: &[u8], mode: Mode) -> std::io::Result<Staged> {
    let tmp = temp_path(path)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if mode == Mode::Private {
            opts.mode(0o600);
        }
    }
    let staged = Staged {
        tmp,
        committed: false,
    };
    let mut f = opts.open(&staged.tmp)?;
    if mode == Mode::KeepExisting
        && let Ok(m) = std::fs::metadata(path)
    {
        f.set_permissions(m.permissions())?;
    }
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(staged)
}

impl Staged {
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.tmp
    }

    /// Renames the temp file over `path` and fsyncs the directory, so the new name survives a
    /// crash too.
    pub fn commit(mut self, path: &Path) -> std::io::Result<()> {
        rename_over(&self.tmp, path)?;
        self.committed = true;
        sync_dir(path)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            // Nothing to do when it is already gone; any other failure leaves a stray temp file
            // that names its writer.
            if let Err(e) = std::fs::remove_file(&self.tmp)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                crate::diag::log(
                    "fsutil",
                    &format!("cannot remove {}: {e}", self.tmp.display()),
                );
            }
        }
    }
}

/// Renames `from` over `to`. Windows refuses to replace a file while another writer (thread or
/// process) is replacing it or holds it open without delete sharing (access denied, sharing
/// violation); those moments are short, so the rename is tried again for a while.
fn rename_over(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const ERROR_SHARING_VIOLATION: i32 = 32;
        let start = std::time::Instant::now();
        loop {
            match std::fs::rename(from, to) {
                Err(e)
                    if (e.kind() == std::io::ErrorKind::PermissionDenied
                        || e.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
                        && start.elapsed() < std::time::Duration::from_secs(2) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                other => return other,
            }
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(from, to)
}

/// Fsyncs the directory holding `path` (Unix; elsewhere the rename is all there is).
fn sync_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Writes through a unique temp file in the same directory, fsyncs it, renames it over `path`
/// and fsyncs the directory. Creates the parent directory when it is missing.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: Mode) -> std::io::Result<()> {
    if let Some(d) = path.parent()
        && !d.as_os_str().is_empty()
    {
        std::fs::create_dir_all(d)?;
    }
    stage(path, bytes, mode)?.commit(path)
}

/// Moves a file or directory to `to` (same file system), refusing to replace anything there:
/// swarmy's own data moving to a new place (the vault's per-seat backups, keyed by seat id now).
pub fn move_into_place(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists", to.display()),
        ));
    }
    std::fs::rename(from, to)?;
    sync_dir(to)
}

/// `std::fs::canonicalize` without Windows' verbatim prefix (`\\?\C:\…` → `C:\…`), so paths
/// shown to people and hashed into seat ids look the same as the ones they typed.
/// The last `max` bytes of a file as text, from the first whole line: the cut may fall inside a
/// line or inside a UTF-8 character (review B-M21: `read_to_string` failed on a cut inside
/// "é" or "—", and the AGENT card showed no last message), so a partial first line is dropped
/// and any invalid byte read as U+FFFD.
pub fn tail_lines(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let start = f.metadata()?.len().saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(if start > 0 {
        text.split_once('\n')
            .map_or("", |(_, rest)| rest)
            .to_string()
    } else {
        text.into_owned()
    })
}

pub fn canonical(p: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let c = std::fs::canonicalize(p)?;
    if cfg!(windows) {
        let s = c.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return Ok(std::path::PathBuf::from(rest));
        }
    }
    Ok(c)
}

/// The identity of a file (its inode on Unix): a file replaced at the same path has another.
#[cfg(unix)]
pub fn file_id(_path: &Path, m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.ino()
}

/// Windows has no inode in `Metadata`: the file index of an open handle (mixed with the
/// volume serial) tells a replaced file from one appended to, as the inode does on Unix.
#[cfg(windows)]
pub fn file_id(path: &Path, _m: &std::fs::Metadata) -> u64 {
    winapi_util::Handle::from_path_any(path)
        .and_then(|h| winapi_util::file::information(&h))
        .map_or(0, |i| {
            i.file_index() ^ i.volume_serial_number().rotate_left(48)
        })
}

#[cfg(not(any(unix, windows)))]
pub fn file_id(_path: &Path, _m: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tail_cut_inside_a_character_is_read() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("t.jsonl");
        // 21 bytes; "é" is bytes 6 and 7, so a 14-byte window starts inside it.
        std::fs::write(&p, "first é line\nsecond\n").unwrap();
        assert_eq!(tail_lines(&p, 14).unwrap(), "second\n");
        assert_eq!(tail_lines(&p, 9).unwrap(), "second\n");
        assert_eq!(tail_lines(&p, 1000).unwrap(), "first é line\nsecond\n");
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn replaces_the_file_and_leaves_no_temp() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub/state.json");
        atomic_write(&p, b"one", Mode::Private).unwrap();
        atomic_write(&p, b"two", Mode::Private).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert!(leftovers(&t.path().join("sub")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn modes() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let private = t.path().join("secret.json");
        atomic_write(&private, b"x", Mode::Private).unwrap();
        assert_eq!(mode(&private), 0o600);
        // Someone else's file keeps its mode.
        let theirs = t.path().join("settings.json");
        std::fs::write(&theirs, "{}").unwrap();
        std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o640)).unwrap();
        atomic_write(&theirs, b"{\"a\":1}", Mode::KeepExisting).unwrap();
        assert_eq!(mode(&theirs), 0o640);
    }

    #[test]
    fn temp_names_are_unique_across_threads() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("fleet.json");
        let names: Vec<PathBuf> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8).map(|_| s.spawn(|| temp_path(&p).unwrap())).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        // Concurrent writers: every write lands whole, the last rename wins, no temp is left.
        std::thread::scope(|s| {
            for i in 0..8 {
                let p = &p;
                s.spawn(move || {
                    for _ in 0..20 {
                        atomic_write(p, format!("writer {i}").as_bytes(), Mode::Private).unwrap();
                    }
                });
            }
        });
        assert!(std::fs::read_to_string(&p).unwrap().starts_with("writer "));
        assert!(leftovers(t.path()).is_empty());
    }

    #[test]
    fn a_staged_file_dropped_is_removed() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x.json");
        let s = stage(&p, b"x", Mode::Private).unwrap();
        assert!(s.path().exists());
        drop(s);
        assert!(leftovers(t.path()).is_empty());
        assert!(!p.exists());
    }
}

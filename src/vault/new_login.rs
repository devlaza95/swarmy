//! New logins: signing in in a scratch seat and saving what it made (`accounts add`, `a` in the
//! dashboard), and reading a credentials file of any harness (`accounts import`).

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};

use super::{SeatLogin, Vault, read_seat};
use crate::claude::config_json;
use crate::claude::credentials::{self, Secret};
use crate::claude::seats::Seat;
use crate::core::harness::Harness;

/// A login read from a credentials file of any harness the vault keeps (`accounts import`):
/// Claude Code's `.credentials.json` (or `{"claudeAiOauth": …}`), agy's
/// `antigravity-oauth-token`, Codex's `auth.json`. An agy or Codex file names its own account,
/// which comes with it (review A-M4: Codex files were refused, and agy's account dropped).
pub fn parse_import(text: &str) -> anyhow::Result<(Secret, Option<config_json::Account>)> {
    if let Some(s) = credentials::parse_login(text)? {
        return Ok((s, None));
    }
    if let Some(s) = crate::agy::credentials::parse_login(text)? {
        let account = crate::agy::account::account_of(s.expose());
        return Ok((s, account));
    }
    if let Some(s) = crate::codex::credentials::parse_login(text)? {
        let account = crate::codex::account::account_of(s.expose());
        return Ok((s, account));
    }
    anyhow::bail!("no Claude Code, Antigravity or Codex login in that file")
}

/// What to run for a new login of `h`, and what to do in it.
pub fn new_login_recipe(h: Harness) -> (&'static str, &'static str) {
    match h {
        Harness::Claude => ("claude", "Run /login, choose the account, then /exit."),
        Harness::Agy => ("agy", "Sign in with Google when agy asks, then quit agy."),
        Harness::Codex => (
            "codex",
            "Sign in in the browser Codex opens; it returns by itself.",
        ),
    }
}

/// The process that makes a new login of `h` in the scratch dir `scratch`, and the seat the
/// login lands in. Claude Code takes its config dir from `CLAUDE_CONFIG_DIR`; the agy CLI keys its
/// dir on `HOME` (`$HOME/.gemini/antigravity-cli`; verified: it ignores
/// `ANTIGRAVITY_APP_DATA_DIR` for its token and conversations), so it runs with `HOME=scratch`.
pub fn new_login(h: Harness, label: &str, scratch: &Path) -> (std::process::Command, Seat) {
    let (program, _) = new_login_recipe(h);
    let mut cmd = std::process::Command::new(program);
    let seat = match h {
        Harness::Claude => {
            cmd.env("CLAUDE_CONFIG_DIR", scratch);
            Seat::new(
                format!("scratch-{label}"),
                scratch.to_path_buf(),
                scratch.join(".claude.json"),
                false,
            )
        }
        Harness::Agy => {
            cmd.env("HOME", scratch);
            Seat::agy(
                format!("scratch-{label}"),
                scratch.join(".gemini").join("antigravity-cli"),
            )
        }
        Harness::Codex => {
            cmd.env("CODEX_HOME", scratch).arg("login");
            Seat::codex(format!("scratch-{label}"), scratch.to_path_buf())
        }
    };
    (cmd, seat)
}

/// A scratch dir for a new login, under the vault: named by the time, this process and a
/// counter, so two logins added in the same second never share one.
pub fn new_login_scratch(vault_dir: &Path, now: DateTime<Utc>) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    vault_dir.join(format!(
        "new-{}-{}-{n}",
        now.format("%Y%m%d%H%M%S"),
        std::process::id()
    ))
}

/// What [`save_new_login`] saved.
#[derive(Debug)]
pub struct NewLogin {
    pub fingerprint: String,
    /// The scratch dir could not be removed (it holds the login): what to tell the user.
    pub left_behind: Option<String>,
}

/// After the harness of a new login (`new_login`) exited: saves the login it made in `seat`
/// (inside `scratch`) under `label`, then removes `scratch`. When the login cannot be saved
/// (the label was taken meanwhile, the vault is locked or damaged), the scratch dir is kept, as
/// it holds the only copy of a new grant, and the error says where (review B-M4).
pub fn save_new_login(
    vault: &mut Vault,
    label: &str,
    scratch: &Path,
    seat: &Seat,
) -> anyhow::Result<NewLogin> {
    let s = read_seat(seat);
    let made = matches!(
        &s,
        Ok(SeatLogin {
            secret: Some(_),
            ..
        })
    );
    let res = (|| -> anyhow::Result<String> {
        let s = s?;
        let secret = s.secret.context("no login was made in the scratch seat")?;
        vault.edit(|tx| {
            tx.put(
                &secret,
                s.oauth_account,
                s.account.as_ref(),
                Some(label),
                None,
            )
        })?;
        Ok(secret.meta().fingerprint)
    })();
    match res {
        Ok(fingerprint) => Ok(NewLogin {
            fingerprint,
            left_behind: match std::fs::remove_dir_all(scratch) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Some(format!(
                    "cannot remove {} (it holds a login): {e}",
                    scratch.display()
                )),
                Ok(()) | Err(_) => None,
            },
        }),
        Err(e) if made => Err(e.context(format!(
            "the new login was not saved; it is kept in {}",
            scratch.display()
        ))),
        Err(e) => {
            // No login was made: nothing in the dir is worth keeping.
            if let Err(rm) = std::fs::remove_dir_all(scratch) {
                crate::diag::log("vault", &format!("{}: {rm}", scratch.display()));
            }
            Err(e)
        }
    }
}

//! `swarmy setup`: a short wizard for a new machine.
//!
//! 1. Lists the Claude Code config dirs ("seats") it found.
//! 2. Offers the swarmy status line in each seat's `settings.json`: Claude Code then shows week and
//!    5-hour use under the prompt, and swarmy gets exact limit readings after every response. A
//!    status line the user already has is kept and shown first (`swarmy statusline --then …`).
//!    The old file is backed up next to it; `swarmy setup --remove` puts things back.
//! 3. Offers to save the logins the seats hold in the vault, so they can be switched later.
//!
//! The only files it writes in a config dir are `settings.json` (and its backup), and only after a
//! yes (or `--yes`).

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Args;
use serde_json::{Map, Value, json};

use crate::app::context::Ctx;
use crate::claude::seats::Seat;
use crate::core::harness::Harness;
use crate::ui::style::Tone;

#[derive(Args, Debug, Clone)]
pub struct SetupArgs {
    /// Answer yes to every question.
    #[arg(long, short)]
    pub yes: bool,
    /// Take the swarmy status line out of every seat (restoring the one it wrapped).
    #[arg(long)]
    pub remove: bool,
    /// Do not offer to save logins in the vault.
    #[arg(long)]
    pub no_vault: bool,
}

/// What a seat's `settings.json` has as its status line.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLine {
    None,
    Swarmy { wraps: Option<String> },
    Other(String),
}

pub fn settings_file(seat: &Seat) -> PathBuf {
    seat.dir.join("settings.json")
}

fn read_settings(path: &Path) -> anyhow::Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(Map::new()),
        Ok(t) => match serde_json::from_str::<Value>(&t) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) | Err(_) => {
                Err(crate::errors::SetupError::SettingsDamaged(path.to_path_buf()).into())
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn status_line_of(settings: &Map<String, Value>) -> StatusLine {
    let Some(cmd) = settings
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
    else {
        return StatusLine::None;
    };
    match cmd.find(" statusline") {
        Some(_) if cmd.contains("swarmy") => StatusLine::Swarmy {
            wraps: wrapped(cmd),
        },
        _ => StatusLine::Other(cmd.to_string()),
    }
}

pub fn seat_status_line(seat: &Seat) -> StatusLine {
    read_settings(&settings_file(seat))
        .map(|s| status_line_of(&s))
        .unwrap_or(StatusLine::None)
}

/// POSIX single quotes: `it's` → `'it'\''s'`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command a `--then '…'` argument wraps, undoing `sh_quote`.
fn wrapped(cmd: &str) -> Option<String> {
    let rest = cmd.split_once("--then ")?.1.trim();
    let inner = rest.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace(r"'\''", "'"))
}

/// The status-line command for this binary, wrapping `previous` when there is one.
pub fn command_for(exe: &Path, previous: Option<&str>) -> String {
    let exe = exe.to_string_lossy();
    let exe = if exe.contains(' ') {
        format!("\"{exe}\"")
    } else {
        exe.into_owned()
    };
    match previous {
        Some(p) => format!("{exe} statusline --then {}", sh_quote(p)),
        None => format!("{exe} statusline"),
    }
}

/// The new `settings.json` content, or `None` when nothing changes.
pub fn with_status_line(
    mut settings: Map<String, Value>,
    exe: &Path,
    remove: bool,
    refresh: u32,
) -> Option<Map<String, Value>> {
    let current_refresh = settings
        .get("statusLine")
        .and_then(|s| s.get("refreshInterval"))
        .and_then(Value::as_u64);
    match (status_line_of(&settings), remove) {
        // Ours already: only the refresh timer may need adding.
        (StatusLine::Swarmy { .. }, false) => {
            if refresh == 0 || current_refresh.is_some() {
                return None;
            }
            settings["statusLine"]["refreshInterval"] = json!(refresh);
            Some(settings)
        }
        (StatusLine::None | StatusLine::Other(_), true) => None,
        (StatusLine::None, false) => {
            let mut sl = json!({"type": "command", "command": command_for(exe, None)});
            if refresh > 0 {
                // Idle sessions re-run it on this timer, so "needs you" elsewhere shows up.
                sl["refreshInterval"] = json!(refresh);
            }
            settings.insert("statusLine".into(), sl);
            Some(settings)
        }
        (StatusLine::Other(prev), false) => {
            let mut sl = settings
                .get("statusLine")
                .cloned()
                .unwrap_or_else(|| json!({}));
            sl["command"] = Value::String(command_for(exe, Some(&prev)));
            if refresh > 0 && sl.get("refreshInterval").is_none() {
                sl["refreshInterval"] = json!(refresh);
            }
            settings.insert("statusLine".into(), sl);
            Some(settings)
        }
        (StatusLine::Swarmy { wraps }, true) => {
            match wraps {
                Some(prev) => {
                    let mut sl = settings
                        .get("statusLine")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    sl["command"] = Value::String(prev);
                    if let Some(o) = sl.as_object_mut() {
                        o.remove("refreshInterval");
                    }
                    settings.insert("statusLine".into(), sl);
                }
                None => {
                    settings.remove("statusLine");
                }
            }
            Some(settings)
        }
    }
}

/// Backs up the old file (when there is one) and writes the new one atomically.
fn write_settings(path: &Path, settings: &Map<String, Value>) -> anyhow::Result<Option<PathBuf>> {
    let backup = if path.exists() {
        // Never over an older backup: two runs within a second must both keep theirs.
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut b = path.with_extension(format!("json.swarmy-{stamp}.bak"));
        let mut n = 1;
        while b.exists() {
            b = path.with_extension(format!("json.swarmy-{stamp}-{n}.bak"));
            n += 1;
        }
        std::fs::copy(path, &b).with_context(|| format!("cannot back up {}", path.display()))?;
        Some(b)
    } else {
        None
    };
    let mut text = serde_json::to_string_pretty(&Value::Object(settings.clone()))?;
    text.push('\n');
    // Claude Code's file: it keeps its mode.
    crate::fsutil::atomic_write(path, text.as_bytes(), crate::fsutil::Mode::KeepExisting)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(backup)
}

fn ask(question: &str, yes: bool) -> bool {
    if yes {
        println!("{question} [Y/n] y");
        return true;
    }
    print!("{question} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    !matches!(line.trim().to_lowercase().as_str(), "n" | "no")
}

pub fn run(ctx: &Ctx, a: &SetupArgs) -> anyhow::Result<()> {
    let p = &ctx.painter;
    if !a.yes && !std::io::stdin().is_terminal() {
        anyhow::bail!("setup asks questions: run it in a terminal, or with --yes");
    }
    let home = &ctx.dirs.home;
    let show = |path: &Path| match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    };
    println!("{}", p.paint("swarmy setup", Tone::Header));
    let claude_seats: Vec<&Seat> = ctx
        .seats
        .iter()
        .filter(|s| s.harness.supports_statusline())
        .collect();
    // The status line is Claude Code's; a machine with only agy or Codex seats still gets the
    // saved-logins step below (review A-L2).
    'statusline: {
        if claude_seats.is_empty() {
            println!("No Claude Code config dir found: no status line to install.");
            break 'statusline;
        }
        println!("\nClaude Code config dirs (seats) found:");
        for s in &claude_seats {
            let state = match seat_status_line(s) {
                StatusLine::None => p.paint("no status line", Tone::Dim),
                StatusLine::Swarmy { .. } => p.paint("swarmy status line ✓", Tone::Good),
                StatusLine::Other(_) => p.paint("has its own status line", Tone::Dim),
            };
            println!("  {:<12} {:<34} {state}", s.label, show(&s.dir));
        }
        let exe = std::env::current_exe().context("cannot find the swarmy binary")?;
        let exe = crate::fsutil::canonical(&exe).unwrap_or(exe);

        // 1. The status line.
        let todo: Vec<&Seat> = claude_seats
            .iter()
            .copied()
            .filter(|s| match read_settings(&settings_file(s)) {
                Ok(settings) => with_status_line(
                    settings,
                    &exe,
                    a.remove,
                    ctx.config.statusline.refresh_seconds,
                )
                .is_some(),
                // Never taken for an empty file: that seat is left out, and says why.
                Err(e) => {
                    println!("  {} {e:#}", p.paint("skipped:", Tone::Warn));
                    false
                }
            })
            .collect();
        println!();
        if todo.is_empty() {
            println!(
                "{}",
                if a.remove {
                    "No seat has the swarmy status line."
                } else {
                    "Every seat already has the swarmy status line."
                }
            );
        } else {
            let q = if a.remove {
                format!("Take the swarmy status line out of {} seat(s)?", todo.len())
            } else {
                println!(
                    "The swarmy status line shows week and 5-hour use under Claude Code's prompt, and"
                );
                println!(
                    "gives swarmy the exact limits after every response (no network, nothing else changes)."
                );
                println!("A status line you already have stays and is shown first.");
                let fresh = todo
                    .iter()
                    .filter(|s| !matches!(seat_status_line(s), StatusLine::Swarmy { .. }))
                    .count();
                if fresh == 0 {
                    format!(
                        "Update the swarmy status line in {} seat(s) (refresh every {} s)?",
                        todo.len(),
                        ctx.config.statusline.refresh_seconds
                    )
                } else {
                    format!("Add it to {} seat(s)?", todo.len())
                }
            };
            if ask(&q, a.yes) {
                for s in todo {
                    let path = settings_file(s);
                    let settings = read_settings(&path)?;
                    if let Some(new) = with_status_line(
                        settings,
                        &exe,
                        a.remove,
                        ctx.config.statusline.refresh_seconds,
                    ) {
                        let backup = write_settings(&path, &new)?;
                        println!(
                            "  {} {}{}",
                            p.paint("✓", Tone::Good),
                            show(&path),
                            backup
                                .map(|b| format!(" (backup: {})", show(&b)))
                                .unwrap_or_default()
                        );
                    }
                }
                if !a.remove {
                    println!("  Claude Code shows it with the next response (or after a restart).");
                }
            }
        }
    }

    // 2. Saved logins.
    // Claude Code on macOS keeps its login in the Keychain: say so instead of offering nothing.
    if !a.remove
        && !a.no_vault
        && ctx.seats.iter().any(|s| s.harness == Harness::Claude)
        && let Some(e) = crate::vault::unavailable(Harness::Claude)
    {
        println!(
            "{}",
            ctx.painter.paint(&format!("Saved logins: {e}."), Tone::Dim)
        );
    }
    if !a.remove && !a.no_vault {
        let mut v = crate::vault::Vault::open(&ctx.dirs.vault_dir())?;
        let vault_seats: Vec<Seat> = ctx
            .seats
            .iter()
            .filter(|s| s.harness.supports_vault())
            .cloned()
            .collect();
        let states = v.edit(|tx| tx.seat_states(&vault_seats))?;
        let unsaved: Vec<&Seat> = ctx
            .seats
            .iter()
            .filter(|s| {
                s.harness.supports_vault()
                    && states
                        .get(&s.id)
                        .is_some_and(|st| st.fingerprint.is_some() && st.saved_as.is_none())
            })
            .collect();
        if !unsaved.is_empty() {
            println!();
            println!(
                "Saving a seat's login lets you put another account into it when a limit runs out,"
            );
            println!(
                "and switch back later (dashboard: panel 2, Enter; or `swarmy accounts switch`)."
            );
            for s in unsaved {
                if ask(
                    &format!("Save the login of seat '{}' in the vault?", s.label),
                    a.yes,
                ) {
                    match v.edit(|tx| tx.capture(s, None))? {
                        Some(id) => println!(
                            "  {} saved as '{}'",
                            p.paint("✓", Tone::Good),
                            v.logins()
                                .iter()
                                .find(|l| l.id == id)
                                .map(|l| l.label.as_str())
                                .unwrap_or("?")
                        ),
                        None => println!("  no login in this seat"),
                    }
                }
            }
        }
    }
    println!(
        "\nDone. Run {} for the dashboard, {} for keys and a legend.",
        p.paint("swarmy", Tone::Accent),
        p.paint("?", Tone::Accent)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn adds_wraps_and_removes_the_status_line() {
        let exe = Path::new("/opt/bin/swarmy");
        // None → ours; other keys stay.
        let s = with_status_line(obj(json!({"model": "opus"})), exe, false, 0).unwrap();
        assert_eq!(s["statusLine"]["command"], "/opt/bin/swarmy statusline");
        assert_eq!(s["model"], "opus");
        assert_eq!(
            with_status_line(s.clone(), exe, false, 0),
            None,
            "already there"
        );
        // The refresh timer is added to ours when asked for.
        let timed = with_status_line(s.clone(), exe, false, 10).unwrap();
        assert_eq!(timed["statusLine"]["refreshInterval"], 10);

        // Someone else's → wrapped, with quotes kept intact, and restored on removal.
        let theirs = r#"bash -c 'echo it'"'"'s me'"#;
        let s = obj(json!({"statusLine": {"type": "command", "command": theirs, "padding": 1}}));
        let w = with_status_line(s, exe, false, 0).unwrap();
        let cmd = w["statusLine"]["command"].as_str().unwrap();
        assert!(
            cmd.starts_with("/opt/bin/swarmy statusline --then '"),
            "{cmd}"
        );
        assert_eq!(w["statusLine"]["padding"], 1);
        assert_eq!(
            status_line_of(&w),
            StatusLine::Swarmy {
                wraps: Some(theirs.to_string())
            }
        );
        let back = with_status_line(w, exe, true, 0).unwrap();
        assert_eq!(back["statusLine"]["command"], theirs);

        // Ours alone → the key goes away.
        let mine = with_status_line(obj(json!({})), exe, false, 10).unwrap();
        assert!(
            !with_status_line(mine, exe, true, 10)
                .unwrap()
                .contains_key("statusLine")
        );
    }
}

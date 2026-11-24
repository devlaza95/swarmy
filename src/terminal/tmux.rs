//! Switching the user's tmux client to the pane where an agent runs.
//!
//! Accepts both Claude Code's `session:@window.%pane` format and bare pane IDs (`%24`, from
//! `TMUX_PANE` in a process environment).

/// Outcome of a jump, for the caller to report.
pub enum Jump {
    Done,
    NotInTmux { hint: String },
    Failed,
}

/// Resolves a bare pane id (`%24`) into `session:@window.%pane` using tmux.
fn resolve_pane(pane: &str) -> Option<String> {
    if !pane.starts_with('%') {
        return Some(pane.to_string());
    }
    let out = std::process::Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            pane,
            "#{session_name}:#{window_id}.#{pane_id}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn switch_to(pane: &str) -> Jump {
    if std::env::var_os("TMUX").is_none() {
        let hint = if pane.starts_with('%') {
            format!(r"tmux attach \; select-pane -t {pane}")
        } else {
            let (session, rest) = pane.split_once(':').unwrap_or(("", pane));
            let (_, pane_id) = rest.split_once('.').unwrap_or((rest, ""));
            format!(r"tmux attach -t {session} \; select-pane -t {pane_id}")
        };
        return Jump::NotInTmux { hint };
    }
    let Some(full) = resolve_pane(pane) else {
        return Jump::Failed;
    };
    let (session, rest) = full.split_once(':').unwrap_or(("", &full));
    let (window, pane_id) = rest.split_once('.').unwrap_or((rest, ""));
    let ok = std::process::Command::new("tmux")
        .args([
            "select-window",
            "-t",
            window,
            ";",
            "select-pane",
            "-t",
            pane_id,
            ";",
            "switch-client",
            "-t",
            session,
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok { Jump::Done } else { Jump::Failed }
}

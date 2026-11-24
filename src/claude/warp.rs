//! Focusing the Warp session a Claude Code runs in (macOS, via Warp's `warp://session/<id>` URL).
//!
//! Warp exports `WARP_TERMINAL_SESSION_UUID` (and `WARP_FOCUS_URL`, the same id as a URL) into
//! every shell it starts; opening that URL brings Warp forward on the session's tab and pane.

use std::process::Command;

use super::pane::{JUMP_TIMEOUT, RunOutcome, run_with_timeout};

#[derive(Debug, PartialEq, Eq)]
pub enum FocusError {
    Unreachable(String),
    Failed(String),
}

/// The URL that focuses a session, or `None` when the id is not one Warp writes (hex, maybe with
/// dashes), so nothing else can end up in the URL.
pub fn focus_url(session_id: &str) -> Option<String> {
    crate::core::pane::is_warp_session_id(session_id)
        .then(|| format!("warp://session/{session_id}"))
}

fn classify_output(success: bool, stderr: &str) -> Result<(), FocusError> {
    if success {
        return Ok(());
    }
    let first = stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("open failed");
    Err(FocusError::Failed(first.trim().to_string()))
}

/// Opens the session's URL. `open` cannot tell whether the session is still there: for a closed
/// one Warp only comes to the front.
pub fn focus(session_id: &str) -> Result<(), FocusError> {
    if !cfg!(target_os = "macos") {
        return Err(FocusError::Unreachable(
            "Warp jump is only available on macOS".into(),
        ));
    }
    let Some(url) = focus_url(session_id) else {
        return Err(FocusError::Failed(format!(
            "not a Warp session id: {session_id}"
        )));
    };
    let mut cmd = Command::new("open");
    cmd.arg(url);
    match run_with_timeout(&mut cmd, JUMP_TIMEOUT) {
        Ok(RunOutcome::Exited(status, _stdout, stderr)) => {
            classify_output(status.success(), &stderr)
        }
        Ok(RunOutcome::TimedOut) => Err(FocusError::Failed("timed out waiting for Warp".into())),
        Err(e) => Err(FocusError::Failed(format!("cannot run open: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_focus_url_only_from_a_session_id() {
        assert_eq!(
            focus_url("0123456789abcdef0123456789abcdef").as_deref(),
            Some("warp://session/0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            focus_url("11223344-5566-7788-99aa-bbccddeeff00").as_deref(),
            Some("warp://session/11223344-5566-7788-99aa-bbccddeeff00")
        );
        assert_eq!(focus_url(""), None);
        assert_eq!(focus_url("abc/../x"), None);
        assert_eq!(focus_url("abcd?x=1"), None);
    }

    #[test]
    fn classifies_open_outcomes() {
        assert_eq!(classify_output(true, ""), Ok(()));
        assert_eq!(
            classify_output(false, "LSOpenURLsWithRole() failed with error -10814\n"),
            Err(FocusError::Failed(
                "LSOpenURLsWithRole() failed with error -10814".into()
            ))
        );
        assert_eq!(
            classify_output(false, ""),
            Err(FocusError::Failed("open failed".into()))
        );
    }
}

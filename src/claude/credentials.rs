//! The OAuth login of a seat: `claudeAiOauth` in `<config dir>/.credentials.json` (Linux and
//! Windows) or, on macOS, a Keychain item. Token values never leave this module except through
//! `Secret`, which does not print them.

use serde_json::Value;

pub use crate::core::login::{LoginMeta, Secret};

/// Extracts `claudeAiOauth` from the text of a credentials file or of a backup holding just
/// `{"claudeAiOauth": {...}}`.
pub fn parse_login(text: &str) -> anyhow::Result<Option<Secret>> {
    let v: Value = serde_json::from_str(text)?;
    Ok(v.get("claudeAiOauth")
        .filter(|x| x.is_object())
        .cloned()
        .map(Secret::new)
        .filter(Secret::has_token))
}

/// Puts `login` into the credentials file text, keeping every other key (`mcpOAuth` tokens belong
/// to the config dir, not to the account, and must survive a switch).
pub fn with_login(text: Option<&str>, login: &Secret) -> anyhow::Result<String> {
    let mut m: serde_json::Map<String, Value> = match text {
        Some(t) if !t.trim().is_empty() => serde_json::from_str(t)?,
        _ => serde_json::Map::new(),
    };
    m.insert("claudeAiOauth".into(), login.expose().clone());
    Ok(serde_json::to_string(&m)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDS: &str = r#"{"mcpOAuth":{"plugin:x|1":{"accessToken":"m"}},
      "claudeAiOauth":{"accessToken":"sk-a","refreshToken":"sk-r","expiresAt":1795377583375,
      "refreshTokenExpiresAt":1797969583375,"scopes":["user:inference"],"subscriptionType":"max",
      "rateLimitTier":"default_claude_max_20x"}}"#;

    #[test]
    fn parses_meta_without_exposing_tokens() {
        let s = parse_login(CREDS).unwrap().unwrap();
        let m = s.meta();
        assert_eq!(m.fingerprint, crate::core::login::fingerprint("sk-r"));
        assert_eq!(m.fingerprint.len(), 16);
        assert_eq!(m.subscription_type.as_deref(), Some("max"));
        assert!(m.refresh_expires_at.is_some());
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("sk-a") && !dbg.contains("sk-r"), "{dbg}");
    }

    #[test]
    fn replacing_the_login_keeps_mcp_tokens() {
        let other = Secret::new(serde_json::json!({"accessToken": "sk-b", "refreshToken": "sk-q"}));
        let out = with_login(Some(CREDS), &other).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "sk-q");
        assert_eq!(v["mcpOAuth"]["plugin:x|1"]["accessToken"], "m");
        let fresh = with_login(None, &other).unwrap();
        assert!(fresh.contains("sk-q"));
    }

    #[test]
    fn backup_files_hold_just_the_login() {
        assert!(
            parse_login(r#"{"claudeAiOauth":{"refreshToken":"x"}}"#)
                .unwrap()
                .is_some()
        );
        assert!(parse_login(r#"{"mcpOAuth":{}}"#).unwrap().is_none());
    }
}

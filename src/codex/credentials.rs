//! The login of a Codex home: the whole `auth.json` (`{"auth_mode", "OPENAI_API_KEY", "tokens":
//! {id_token, access_token, refresh_token, account_id, …}, "last_refresh"}`), the one file the
//! vault writes into a Codex seat.
//!
//! OpenAI's refresh tokens are single-use: a refresh returns a new one and a used one is refused
//! afterwards (Codex says "your refresh token was already used. Please log out and sign in
//! again", verified in its binary, 0.172). So one login lives in one seat only (two seats would
//! sign each other out), and the vault follows each refresh of the seat it is in
//! (`vault::seats::is_rotation`).

use serde_json::Value;

use crate::core::harness::Harness;
use crate::core::login::Secret;

/// The login file inside a Codex home.
pub const AUTH_FILE: &str = super::account::AUTH_FILE;

/// The login in the text of an `auth.json` (or a backup of one): the whole object, when it holds
/// ChatGPT tokens or an API key.
pub fn parse_login(text: &str) -> anyhow::Result<Option<Secret>> {
    let v: Value = serde_json::from_str(text)?;
    let tokens = v.get("tokens").is_some_and(Value::is_object);
    let key = v
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|k| !k.is_empty());
    Ok((tokens || key)
        .then(|| Secret::of(Harness::Codex, v))
        .filter(Secret::has_token))
}

/// The text of `auth.json` for `login`: the file is the login, so it is replaced whole.
pub fn with_login(login: &Secret) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(login.expose())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_json_is_the_login() {
        let text = r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":"a.b.c","access_token":"x.y.z","refresh_token":"rt-1","account_id":"ws-1"},"last_refresh":"2026-11-24T10:00:00Z"}"#;
        let s = parse_login(text).unwrap().unwrap();
        assert_eq!(s.harness(), Harness::Codex);
        let back: Value = serde_json::from_str(&with_login(&s).unwrap()).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(text).unwrap());
        assert!(
            parse_login(r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null}"#)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_login(r#"{"OPENAI_API_KEY":"sk-test"}"#)
                .unwrap()
                .is_some()
        );
    }
}

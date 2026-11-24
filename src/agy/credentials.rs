//! The login of an Antigravity seat: the whole `antigravity-oauth-token` file
//! (`{"token": {access_token, refresh_token, expiry, …}, "auth_method", "id_token_email"}`), the
//! one file the vault writes into an agy seat. On Linux Antigravity keeps it as a file; elsewhere
//! it may use the OS keyring (`jetski-standalone-oauth-token`), which the vault cannot read.
//!
//! Antigravity re-reads the file and skips saving a refreshed token when "stored token changed
//! externally" (verified in its binary), so a switch is not undone by a running agy; that agy
//! keeps the login it loaded until it restarts.

use serde_json::Value;

use crate::core::harness::Harness;
use crate::core::login::Secret;

/// The token file's name inside the seat dir.
pub const TOKEN_FILE: &str = "antigravity-oauth-token";

/// The login in the text of a token file (or of a backup of one): the whole object, when it
/// holds a token.
pub fn parse_login(text: &str) -> anyhow::Result<Option<Secret>> {
    let v: Value = serde_json::from_str(text)?;
    Ok(v.get("token")
        .is_some_and(Value::is_object)
        .then(|| Secret::of(Harness::Agy, v))
        .filter(Secret::has_token))
}

/// The text of the token file for `login`: the file is the login, so it is replaced whole.
pub fn with_login(login: &Secret) -> anyhow::Result<String> {
    Ok(serde_json::to_string(login.expose())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_file_is_the_login() {
        let text = r#"{"token":{"access_token":"ya-a","refresh_token":"ya-r","expiry":"2026-11-24T10:00:00Z"},"auth_method":"consumer","id_token_email":"sam@example.com"}"#;
        let s = parse_login(text).unwrap().unwrap();
        assert_eq!(s.harness(), Harness::Agy);
        let back: Value = serde_json::from_str(&with_login(&s).unwrap()).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(text).unwrap());
        let acct = crate::agy::account::account_of(s.expose()).unwrap();
        assert_eq!(acct.account_uuid, "agy:sam@example.com");
        assert!(
            parse_login(r#"{"auth_method":"consumer"}"#)
                .unwrap()
                .is_none()
        );
    }
}

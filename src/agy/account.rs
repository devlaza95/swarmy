//! Read-only non-secret account identity for an Antigravity (`agy`) seat.
//!
//! Reads `~/.gemini/antigravity-cli/antigravity-oauth-token` and decodes ONLY the non-secret
//! `id_token` JWT payload (`sub`, `email`, `name`) and `auth_method` in memory. Raw access or
//! refresh tokens are never stored, logged or returned.

use std::path::Path;

use serde::Deserialize;

use crate::model::account::Account;

#[derive(Deserialize, Default)]
#[serde(default)]
struct OAuthTokenFile {
    auth_method: Option<String>,
    id_token: Option<String>,
    id_token_email: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IdTokenClaims {
    sub: Option<String>,
    email: Option<String>,
    name: Option<String>,
}

/// Reads the non-secret Google account identity from `seat_dir/antigravity-oauth-token`.
/// Missing files return `Ok(None)`; damaged JSON returns an error for `doctor`/`ATTENTION`.
pub fn read_account(seat_dir: &Path) -> Result<Option<Account>, String> {
    let path = seat_dir.join("antigravity-oauth-token");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{} is damaged: {e}", path.display()))?;
    Ok(account_of(&v))
}

/// The account a token file (or a saved copy of one) names: the non-secret `id_token` claims
/// and `id_token_email`, never the tokens.
pub fn account_of(v: &serde_json::Value) -> Option<Account> {
    let parsed = OAuthTokenFile::deserialize(v).ok()?;
    let claims = parsed
        .id_token
        .as_deref()
        .and_then(crate::core::jwt::payload)
        .and_then(|bytes| serde_json::from_slice::<IdTokenClaims>(&bytes).ok())
        .unwrap_or_default();
    let email = claims
        .email
        .filter(|s| !s.is_empty())
        .or_else(|| parsed.id_token_email.filter(|s| !s.is_empty()));
    // The email names the account (it is what the token file always carries); the subject only
    // when there is no email, so a newer agy writing an `id_token` does not split the account.
    let key = email
        .as_deref()
        .map(str::to_lowercase)
        .or_else(|| claims.sub.filter(|s| !s.is_empty()))?;
    let plan = parsed.auth_method.filter(|s| !s.is_empty());
    Some(Account {
        // Namespaced so Google subjects never collide with Anthropic UUIDs.
        account_uuid: format!("agy:{key}"),
        email,
        display_name: claims.name.filter(|s| !s.is_empty()),
        plan_display_name: plan,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_non_secret_claims_from_synthetic_token() {
        let t = tempfile::tempdir().unwrap();
        let header = crate::core::jwt::b64url(br#"{"alg":"RS256"}"#);
        let payload = crate::core::jwt::b64url(
            br#"{"sub":"1000000000001","email":"alex@example.com","name":"Alex Example"}"#,
        );
        let jwt = format!("{header}.{payload}.sig");
        std::fs::write(
            t.path().join("antigravity-oauth-token"),
            serde_json::json!({
                "token": {"access_token": "secret-do-not-read"},
                "auth_method": "consumer",
                "id_token": jwt
            })
            .to_string(),
        )
        .unwrap();
        let acct = read_account(t.path()).unwrap().unwrap();
        assert_eq!(acct.account_uuid, "agy:alex@example.com");
        assert_eq!(acct.email.as_deref(), Some("alex@example.com"));
        assert_eq!(acct.display_name.as_deref(), Some("Alex Example"));
        assert_eq!(acct.plan_display_name.as_deref(), Some("consumer"));
    }

    #[test]
    fn reads_top_level_id_token_email_without_jwt() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(
            t.path().join("antigravity-oauth-token"),
            serde_json::json!({
                "token": {"access_token": "secret-do-not-read"},
                "auth_method": "consumer",
                "id_token_email": "sam@example.com"
            })
            .to_string(),
        )
        .unwrap();
        let acct = read_account(t.path()).unwrap().unwrap();
        assert_eq!(acct.account_uuid, "agy:sam@example.com");
        assert_eq!(acct.email.as_deref(), Some("sam@example.com"));
        assert_eq!(acct.plan_display_name.as_deref(), Some("consumer"));
    }

    #[test]
    fn missing_file_is_none_and_damaged_is_err() {
        let t = tempfile::tempdir().unwrap();
        assert!(read_account(t.path()).unwrap().is_none());
        std::fs::write(t.path().join("antigravity-oauth-token"), "{broken").unwrap();
        assert!(read_account(t.path()).is_err());
    }
}

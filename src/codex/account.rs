//! Read-only non-secret account identity of a Codex home.
//!
//! `auth.json` holds `auth_mode`, `OPENAI_API_KEY` (API-key logins) and, for a ChatGPT login,
//! `tokens` (`id_token`, `access_token`, `refresh_token`, `account_id`) and `last_refresh`. Only the
//! `id_token`'s payload is decoded, in memory: `email`, `name` and its
//! `https://api.openai.com/auth` claims (`chatgpt_user_id`, `chatgpt_account_id`,
//! `chatgpt_plan_type`). No token is stored, logged or returned here.
//!
//! An account is a user in a ChatGPT workspace (one user can be in a personal and a team one, each
//! with its own plan and limits): `cdx:<user id>/<workspace id>`, the same pair a rollout's
//! `session_meta` names its thread's account by (`creator_user_id`, `creator_account_id`).

use std::path::Path;

use serde_json::Value;

use crate::core::ids::AccountId;
use crate::model::account::Account;

/// The login file inside a Codex home.
pub const AUTH_FILE: &str = "auth.json";

/// The account of a Codex user in a ChatGPT workspace.
pub fn account_id(user_id: &str, workspace_id: &str) -> AccountId {
    AccountId::of(&format!("cdx:{user_id}"), Some(workspace_id))
}

/// Reads the account of a Codex home's `auth.json`. A missing file, or an API-key login (no
/// ChatGPT account), is `Ok(None)`; a damaged file is an error for `doctor` / ATTENTION.
pub fn read_account(home: &Path) -> Result<Option<Account>, String> {
    let path = home.join(AUTH_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let v: Value =
        serde_json::from_str(&text).map_err(|e| format!("{} is damaged: {e}", path.display()))?;
    Ok(account_of(&v))
}

/// The account an `auth.json` (or a saved copy of one) names: the `id_token`'s non-secret claims,
/// never the tokens.
pub fn account_of(v: &Value) -> Option<Account> {
    let tokens = v.get("tokens")?;
    let claims: Value = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(crate::core::jwt::payload)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let auth = claims
        .get("https://api.openai.com/auth")
        .unwrap_or(&Value::Null);
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let user = s(auth, "chatgpt_user_id").or_else(|| s(auth, "user_id"))?;
    let workspace = s(auth, "chatgpt_account_id").or_else(|| s(tokens, "account_id"))?;
    let id = account_id(&user, &workspace);
    Some(Account {
        account_uuid: id.user_uuid().to_string(),
        organization_uuid: Some(workspace),
        email: s(&claims, "email"),
        display_name: s(&claims, "name"),
        plan_display_name: s(auth, "chatgpt_plan_type"),
        subscription_type: s(auth, "chatgpt_plan_type"),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::jwt::b64url;

    fn auth_json(claims: &str) -> String {
        let jwt = format!(
            "{}.{}.sig",
            b64url(br#"{"alg":"RS256"}"#),
            b64url(claims.as_bytes())
        );
        serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt,
                "access_token": "secret-access",
                "refresh_token": "secret-refresh",
                "account_id": "ws-from-tokens",
            },
            "last_refresh": "2026-11-24T10:00:00Z",
        })
        .to_string()
    }

    #[test]
    fn names_the_user_in_the_workspace_from_the_id_token() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(
            t.path().join(AUTH_FILE),
            auth_json(
                r#"{"email":"robin@example.com","name":"Robin Example","https://api.openai.com/auth":{"chatgpt_user_id":"user-1","chatgpt_account_id":"ws-1","chatgpt_plan_type":"plus"}}"#,
            ),
        )
        .unwrap();
        let a = read_account(t.path()).unwrap().unwrap();
        assert_eq!(a.id(), account_id("user-1", "ws-1"));
        assert_eq!(a.id().as_str(), "cdx:user-1/ws-1");
        assert_eq!(a.email.as_deref(), Some("robin@example.com"));
        assert_eq!(a.display_name.as_deref(), Some("Robin Example"));
        assert_eq!(a.plan_display_name.as_deref(), Some("plus"));
        let text = format!("{a:?}");
        assert!(!text.contains("secret"), "no token leaks into the account");
    }

    #[test]
    fn the_workspace_falls_back_to_the_tokens_account_id() {
        let v: Value = serde_json::from_str(&auth_json(
            r#"{"https://api.openai.com/auth":{"user_id":"user-2"}}"#,
        ))
        .unwrap();
        assert_eq!(
            account_of(&v).unwrap().id().as_str(),
            "cdx:user-2/ws-from-tokens"
        );
    }

    #[test]
    fn an_api_key_login_has_no_account_and_damage_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        assert!(read_account(t.path()).unwrap().is_none());
        std::fs::write(
            t.path().join(AUTH_FILE),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-secret"}"#,
        )
        .unwrap();
        assert!(read_account(t.path()).unwrap().is_none());
        std::fs::write(t.path().join(AUTH_FILE), "{broken").unwrap();
        assert!(read_account(t.path()).is_err());
    }
}

//! A login (an OAuth grant) as the vault keeps it, whatever harness it belongs to: the verbatim
//! secret object, and the non-secret facts read from it. Pure: the files are read and written by
//! each harness's reader (`claude::credentials`, `agy::credentials`).
//!
//! - Claude Code: the `claudeAiOauth` object of `.credentials.json` (`refreshToken`,
//!   `expiresAt` in ms, `refreshTokenExpiresAt`, `subscriptionType`, `rateLimitTier`, `scopes`).
//! - Antigravity: the whole `antigravity-oauth-token` file (`token.refresh_token`,
//!   `token.expiry` as RFC 3339, `auth_method`, `id_token_email`).
//! - Codex: the whole `auth.json` (`tokens.refresh_token`, `tokens.access_token` whose JWT `exp`
//!   is its expiry, `tokens.id_token` whose claims name the plan; or `OPENAI_API_KEY`).

use std::fmt;

use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::harness::Harness;

/// The verbatim secret object of a login, tagged with the harness whose format it is in. Debug
/// shows only the fingerprint; the token strings are wiped when it is dropped.
#[derive(Clone, PartialEq)]
pub struct Secret {
    value: Value,
    harness: Harness,
}

impl Secret {
    /// A Claude Code login (`claudeAiOauth`).
    pub fn new(v: Value) -> Self {
        Secret::of(Harness::Claude, v)
    }

    pub fn of(harness: Harness, value: Value) -> Self {
        Secret { value, harness }
    }

    pub fn expose(&self) -> &Value {
        &self.value
    }

    pub fn harness(&self) -> Harness {
        self.harness
    }

    pub fn meta(&self) -> LoginMeta {
        LoginMeta::of(self.harness, &self.value)
    }

    /// Whether it holds a token at all: a file without one names no grant, and is no login
    /// (review B-M5: two tokenless secrets got the id `lnone` and one replaced the other).
    pub fn has_token(&self) -> bool {
        self.meta().fingerprint != NO_TOKEN
    }
}

/// The fingerprint of a secret without any token ([`Secret::has_token`]).
pub const NO_TOKEN: &str = "none";

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({} {})", self.harness, self.meta().fingerprint)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Best effort: overwrite token strings (nested ones too) before the memory is freed.
        wipe_strings(&mut self.value);
    }
}

fn wipe_strings(v: &mut Value) {
    match v {
        Value::String(s) => wipe(std::mem::take(s)),
        Value::Object(m) => m.values_mut().for_each(wipe_strings),
        Value::Array(a) => a.iter_mut().for_each(wipe_strings),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Zeroes the heap buffer of a string before freeing it.
fn wipe(s: String) {
    let mut b = s.into_bytes();
    b.iter_mut().for_each(|x| *x = 0);
    std::hint::black_box(&b);
}

/// Non-secret facts about a login.
#[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct LoginMeta {
    /// First 8 bytes of SHA-256 of the refresh token, hex: identifies the grant.
    pub fingerprint: String,
    /// Claude Code: `subscriptionType`; Antigravity: `auth_method` (`consumer`, …).
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub access_expires_at: Option<DateTime<Utc>>,
    pub refresh_expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
}

impl LoginMeta {
    pub fn of(harness: Harness, v: &Value) -> LoginMeta {
        match harness {
            Harness::Claude => claude_meta(v),
            Harness::Agy => agy_meta(v),
            Harness::Codex => codex_meta(v),
        }
    }
}

fn claude_meta(v: &Value) -> LoginMeta {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let t = |k: &str| {
        v.get(k)
            .and_then(Value::as_i64)
            .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
    };
    let token = v
        .get("refreshToken")
        .and_then(Value::as_str)
        .or_else(|| v.get("accessToken").and_then(Value::as_str));
    LoginMeta {
        fingerprint: token.map(fingerprint).unwrap_or_else(|| NO_TOKEN.into()),
        subscription_type: s("subscriptionType"),
        rate_limit_tier: s("rateLimitTier"),
        access_expires_at: t("expiresAt"),
        refresh_expires_at: t("refreshTokenExpiresAt"),
        scopes: v
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn agy_meta(v: &Value) -> LoginMeta {
    let token = v.get("token");
    let s = |k: &str| {
        token
            .and_then(|t| t.get(k))
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty())
    };
    LoginMeta {
        fingerprint: s("refresh_token")
            .or_else(|| s("access_token"))
            .map(fingerprint)
            .unwrap_or_else(|| NO_TOKEN.into()),
        subscription_type: v
            .get("auth_method")
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty())
            .map(str::to_string),
        rate_limit_tier: None,
        access_expires_at: s("expiry")
            .and_then(|e| DateTime::parse_from_rfc3339(e).ok())
            .map(|t| t.with_timezone(&Utc)),
        // Google refresh tokens have no fixed lifetime.
        refresh_expires_at: None,
        scopes: Vec::new(),
    }
}

fn codex_meta(v: &Value) -> LoginMeta {
    let tokens = v.get("tokens");
    let s = |k: &str| {
        tokens
            .and_then(|t| t.get(k))
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty())
    };
    let claims = |jwt: Option<&str>| -> Value {
        jwt.and_then(super::jwt::payload)
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null)
    };
    let id = claims(s("id_token"));
    let access = claims(s("access_token"));
    let api_key = v
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty());
    LoginMeta {
        fingerprint: s("refresh_token")
            .or_else(|| s("access_token"))
            .or(api_key)
            .map(fingerprint)
            .unwrap_or_else(|| NO_TOKEN.into()),
        subscription_type: id
            .get("https://api.openai.com/auth")
            .and_then(|a| a.get("chatgpt_plan_type"))
            .and_then(Value::as_str)
            .map(str::to_string),
        rate_limit_tier: None,
        access_expires_at: access
            .get("exp")
            .and_then(Value::as_i64)
            .and_then(|t| Utc.timestamp_opt(t, 0).single()),
        refresh_expires_at: None,
        scopes: Vec::new(),
    }
}

pub fn fingerprint(token: &str) -> String {
    let d = Sha256::digest(token.as_bytes());
    hex::encode(&d[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_meta_names_the_grant_without_exposing_it() {
        let s = Secret::of(
            Harness::Agy,
            serde_json::json!({
                "token": {"access_token": "ya-a", "refresh_token": "ya-r",
                          "expiry": "2026-11-24T10:00:00Z", "expires_in": 3599},
                "auth_method": "consumer",
                "id_token_email": "sam@example.com"
            }),
        );
        let m = s.meta();
        assert_eq!(m.fingerprint, fingerprint("ya-r"));
        assert_eq!(m.subscription_type.as_deref(), Some("consumer"));
        assert_eq!(
            m.access_expires_at.map(|t| t.to_rfc3339()).as_deref(),
            Some("2026-11-24T10:00:00+00:00")
        );
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("ya-a") && !dbg.contains("ya-r"), "{dbg}");
    }

    #[test]
    fn codex_meta_reads_the_plan_and_the_access_expiry_from_the_tokens_claims() {
        use crate::core::jwt::b64url;
        let jwt = |claims: &str| format!("{}.{}.sig", b64url(b"{}"), b64url(claims.as_bytes()));
        let s = Secret::of(
            Harness::Codex,
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": jwt(r#"{"https://api.openai.com/auth":{"chatgpt_plan_type":"plus"}}"#),
                    "access_token": jwt(r#"{"exp":1795600000}"#),
                    "refresh_token": "rt-secret",
                }
            }),
        );
        let m = s.meta();
        assert_eq!(m.fingerprint, fingerprint("rt-secret"));
        assert_eq!(m.subscription_type.as_deref(), Some("plus"));
        assert_eq!(
            m.access_expires_at.map(|t| t.timestamp()),
            Some(1_795_600_000)
        );
    }
}

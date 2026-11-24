//! The parts of Claude Code's `.claude.json` this tool reads.

pub use crate::model::account::Account;
use std::path::Path;

use anyhow::Context as _;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claude::credentials::LoginMeta;
use crate::core::ids::AccountId;
use crate::core::limits::{Reading, WindowKind};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Raw {
    oauth_account: Option<Value>,
    cached_usage_utilization: Option<Value>,
    additional_model_costs_cache: Option<Value>,
}

fn contains_any_ci(s: &str, needles: &[&str]) -> bool {
    let low = s.to_ascii_lowercase();
    needles.iter().any(|n| low.contains(n))
}

const ORG_PLAN_WORDS: &[&str] = &["team", "enterprise", "raven"];

impl Account {
    pub(crate) fn from_value(v: &Value) -> Option<Account> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        };
        Some(Account {
            account_uuid: s("accountUuid")?,
            organization_uuid: s("organizationUuid"),
            organization_name: s("organizationName"),
            email: s("emailAddress"),
            display_name: s("displayName").or_else(|| s("fullName")),
            plan_display_name: s("planDisplayName"),
            seat_tier: s("seatTier"),
            organization_rate_limit_tier: s("organizationRateLimitTier"),
            user_rate_limit_tier: s("userRateLimitTier"),
            billing_type: s("billingType"),
            subscription_type: s("subscriptionType"),
            cred_rate_limit_tier: None,
            profile_fetched_at: v
                .get("profileFetchedAt")
                .and_then(Value::as_i64)
                .and_then(|ms| Utc.timestamp_millis_opt(ms).single()),
        })
    }

    /// Canonical account identity: `<accountUuid>/<organizationUuid>` when the organization is
    /// known, else `<accountUuid>`.
    pub fn id(&self) -> AccountId {
        AccountId::of(&self.account_uuid, self.organization_uuid.as_deref())
    }

    /// Enriches the profile with non-secret metadata from `.credentials.json` (scoped to this
    /// seat's active OAuth grant).
    pub fn apply_credentials(&mut self, meta: &LoginMeta) {
        if let Some(sub) = &meta.subscription_type
            && !sub.is_empty()
        {
            self.subscription_type = Some(sub.clone());
        }
        if let Some(tier) = &meta.rate_limit_tier
            && !tier.is_empty()
        {
            self.cred_rate_limit_tier = Some(tier.clone());
        }
    }

    /// Whether this profile belongs to a Team or Enterprise organization rather than an
    /// individual Pro/Max subscription.
    pub fn is_org_plan(&self) -> bool {
        if self
            .subscription_type
            .as_deref()
            .is_some_and(|s| contains_any_ci(s, ORG_PLAN_WORDS))
        {
            return true;
        }
        if self.seat_tier.is_some() {
            return true;
        }
        if self
            .plan_display_name
            .as_deref()
            .is_some_and(|s| contains_any_ci(s, ORG_PLAN_WORDS))
        {
            return true;
        }
        if self
            .organization_rate_limit_tier
            .as_deref()
            .is_some_and(|s| contains_any_ci(s, ORG_PLAN_WORDS))
        {
            return true;
        }
        if self.billing_type.as_deref().is_some_and(|b| {
            let low = b.to_ascii_lowercase();
            low == "stripe_subscription_contract"
                || low == "invoice"
                || ORG_PLAN_WORDS.iter().any(|w| low.contains(w))
        }) {
            return true;
        }
        false
    }

    /// The tier key that identifies this account's plan bucket (for cross-account `$/pt`
    /// references in `limits`, `accounts.tier`, and `vault::Login.plan`).
    ///
    /// Organization plans (Team/Enterprise) never fall back to bare `userRateLimitTier`, which is
    /// user-scoped and may reflect a separate personal Max subscription.
    pub fn effective_tier(&self) -> Option<String> {
        if self.is_org_plan() {
            return self.org_effective_tier();
        }
        let org = self.organization_rate_limit_tier.as_deref();
        let cred = self.cred_rate_limit_tier.as_deref();
        let user = self.user_rate_limit_tier.as_deref();
        let raw = org
            .filter(|t| *t != "default_claude_ai")
            .or(cred)
            .or(user)
            .or(org)
            .or(self.plan_display_name.as_deref())
            .or(self.subscription_type.as_deref())?;
        if raw == "default_claude_ai"
            && self
                .subscription_type
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("pro"))
        {
            return Some("default_claude_pro".to_string());
        }
        Some(raw.to_string())
    }

    fn org_effective_tier(&self) -> Option<String> {
        let sub = self
            .subscription_type
            .as_deref()
            .filter(|s| contains_any_ci(s, ORG_PLAN_WORDS))
            .map(str::to_ascii_lowercase);
        let org = self.organization_rate_limit_tier.as_deref();
        let cred = self.cred_rate_limit_tier.as_deref();
        let seat = self.seat_tier.as_deref();
        let plan_low = self
            .plan_display_name
            .as_deref()
            .map(str::to_ascii_lowercase);

        let base = if let Some(o) = org {
            if !contains_any_ci(o, ORG_PLAN_WORDS) {
                let prefix = sub.as_deref().unwrap_or("team");
                if o == "default_claude_ai" {
                    prefix.to_string()
                } else {
                    format!("{prefix}:{o}")
                }
            } else {
                o.to_string()
            }
        } else if let Some(c) = cred {
            if !contains_any_ci(c, ORG_PLAN_WORDS) {
                let prefix = sub.as_deref().unwrap_or("team");
                if c == "default_claude_ai" {
                    prefix.to_string()
                } else {
                    format!("{prefix}:{c}")
                }
            } else {
                c.to_string()
            }
        } else if let Some(s) = sub {
            s
        } else if let Some(st) = seat {
            st.to_string()
        } else if let Some(p) = plan_low {
            p
        } else {
            let b = self.billing_type.as_deref()?;
            if b.eq_ignore_ascii_case("invoice") {
                "enterprise".to_string()
            } else {
                "team".to_string()
            }
        };

        if let Some(st) = seat
            && base != st
            && !base.contains(st)
        {
            return Some(format!("{base}/{st}"));
        }
        Some(base)
    }

    /// Human-readable plan label for display (`doctor`, `limits`).
    pub fn plan_label(&self) -> Option<String> {
        if let Some(p) = &self.plan_display_name {
            if let Some(st) = &self.seat_tier {
                let clean = clean_seat_tier(st);
                if !p.to_ascii_lowercase().contains(&clean.to_ascii_lowercase()) {
                    return Some(format!("{p} ({clean})"));
                }
            }
            return Some(p.clone());
        }
        self.effective_tier().map(|t| format_tier_label(&t))
    }
}

fn clean_seat_tier(s: &str) -> String {
    s.strip_prefix("seat_tier_claude_")
        .or_else(|| s.strip_prefix("seat_tier_"))
        .unwrap_or(s)
        .to_string()
}

/// Formats a stored tier key (`default_claude_max_5x`, `default_raven`, `team:default_claude_max_5x`,
/// `default_raven/seat_tier_claude_team_premium`, …) for human display.
pub fn format_tier_label(tier: &str) -> String {
    if let Some((left, right)) = tier.split_once('/') {
        let l = format_tier_label(left);
        let r = clean_seat_tier(right);
        if r.to_ascii_lowercase().contains(&l.to_ascii_lowercase()) {
            return r;
        }
        return format!("{l} ({r})");
    }
    if let Some((prefix, rest)) = tier.split_once(':') {
        let r = format_tier_label(rest);
        if r == "ai" || r.is_empty() {
            return prefix.to_string();
        }
        if r.to_ascii_lowercase()
            .contains(&prefix.to_ascii_lowercase())
        {
            return r;
        }
        return format!("{prefix} ({r})");
    }
    if tier == "default_raven" {
        return "team".to_string();
    }
    if tier == "default_raven_enterprise" {
        return "enterprise".to_string();
    }
    if let Some(rest) = tier.strip_prefix("default_raven_") {
        return format!("team_{rest}");
    }
    if tier.starts_with("seat_tier_") {
        return clean_seat_tier(tier);
    }
    tier.replace("default_claude_", "")
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Utilization {
    /// Canonical account id: `<accountUuid>/<organizationUuid>` when known, else `<accountUuid>`.
    pub account_id: AccountId,
    /// True when `cachedUsageUtilization` carried no `organizationUuid` of its own and inherited
    /// the seat's `oauthAccount.organizationUuid`.
    #[serde(skip)]
    pub org_from_profile: bool,
    pub fetched_at: DateTime<Utc>,
    pub readings: Vec<Reading>,
    /// Extra (pay-as-you-go) usage, when enabled for the account.
    pub extra_usage_enabled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeJson {
    pub account: Option<Account>,
    /// The raw `oauthAccount` object, copied verbatim by the vault on a switch.
    pub oauth_account_raw: Option<Value>,
    pub utilization: Option<Utilization>,
    pub model_costs: Option<Value>,
}

/// A seat's `.claude.json`: empty when the seat has none yet (a fresh config dir), an error
/// when it is there but cannot be read or parsed, so a damaged file is never taken for a seat
/// without an account (first review H5).
pub fn read_seat_file(path: &Path) -> anyhow::Result<ClaudeJson> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("{} is damaged", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ClaudeJson::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn parse(text: &str) -> anyhow::Result<ClaudeJson> {
    let raw: Raw = serde_json::from_str(text)?;
    let account = raw.oauth_account.as_ref().and_then(Account::from_value);
    let utilization = raw
        .cached_usage_utilization
        .as_ref()
        .and_then(|v| parse_utilization(v, account.as_ref()));
    Ok(ClaudeJson {
        account,
        oauth_account_raw: raw.oauth_account,
        utilization,
        model_costs: raw.additional_model_costs_cache,
    })
}

fn parse_utilization(v: &Value, profile: Option<&Account>) -> Option<Utilization> {
    let account_uuid = v.get("accountUuid")?.as_str()?;
    let explicit_org = v
        .get("organizationUuid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let (org_uuid, org_from_profile) = match explicit_org {
        Some(org) => (Some(org), false),
        None => {
            if let Some(p) = profile.filter(|p| p.account_uuid == account_uuid)
                && p.organization_uuid.is_some()
            {
                (p.organization_uuid.as_deref(), true)
            } else {
                (None, false)
            }
        }
    };
    let account_id = AccountId::of(account_uuid, org_uuid);
    let fetched_at = Utc
        .timestamp_millis_opt(v.get("fetchedAtMs")?.as_i64()?)
        .single()?;
    let windows = v.get("utilization")?.as_object()?;
    let mut readings = Vec::new();
    for (k, w) in windows {
        let Some(kind) = WindowKind::from_key(k) else {
            continue;
        };
        let (Some(u), Some(r)) = (
            w.get("utilization").and_then(Value::as_f64),
            w.get("resets_at").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Ok(resets_at) = DateTime::parse_from_rfc3339(r) else {
            continue;
        };
        readings.extend(Reading::parse(
            kind,
            u,
            resets_at.with_timezone(&Utc),
            fetched_at,
        ));
    }
    readings.sort_by_key(|r| r.kind);
    let extra_usage_enabled = windows
        .get("extra_usage")
        .and_then(|e| e.get("is_enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(Utilization {
        account_id,
        org_from_profile,
        fetched_at,
        readings,
        extra_usage_enabled,
    })
}

/// Writes `oauthAccount` into a `.claude.json`, keeping every other key and the key order.
pub fn with_oauth_account(text: &str, account: Option<&Value>) -> anyhow::Result<String> {
    let mut v: serde_json::Map<String, Value> = serde_json::from_str(text)?;
    match account {
        Some(a) => {
            v.insert("oauthAccount".into(), a.clone());
        }
        None => {
            v.remove("oauthAccount");
        }
    }
    Ok(serde_json::to_string_pretty(&v)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "numStartups": 3,
      "oauthAccount": {"accountUuid": "acc-1", "emailAddress": "a@example.com", "organizationUuid": "org-1",
                       "displayName": "Ann", "billingType": "stripe_subscription", "userRateLimitTier": "default_claude_max_20x"},
      "cachedUsageUtilization": {"fetchedAtMs": 1795348787083, "accountUuid": "acc-1",
        "utilization": {"five_hour": null,
                        "seven_day": {"utilization": 65, "resets_at": "2026-11-25T14:59:59.983904+00:00"},
                        "tangelo": null, "extra_usage": {"is_enabled": false}}},
      "additionalModelCostsCache": {"claude-opus-5-5": {"inputTokens": 4, "outputTokens": 20}}
    }"#;

    #[test]
    fn reads_account_utilization_and_costs() {
        let c = parse(SAMPLE).unwrap();
        let a = c.account.unwrap();
        assert_eq!(a.account_uuid, "acc-1");
        assert_eq!(a.email.as_deref(), Some("a@example.com"));
        let u = c.utilization.unwrap();
        assert_eq!(
            u.readings.len(),
            1,
            "null windows and unknown keys are skipped"
        );
        assert_eq!(u.readings[0].kind, WindowKind::SevenDay);
        assert_eq!(u.readings[0].utilization, 65.0);
        assert_eq!(
            u.readings[0].resets_at.to_rfc3339(),
            "2026-11-25T14:59:59.983904+00:00"
        );
        // The window is named by its reset to the minute (sources differ by jitter).
        assert_eq!(
            u.readings[0].window().start.to_rfc3339(),
            "2026-11-18T15:00:00+00:00"
        );
        assert!(c.model_costs.is_some());
    }

    #[test]
    fn replaces_only_oauth_account() {
        let new = serde_json::json!({"accountUuid": "acc-2"});
        let out = with_oauth_account(SAMPLE, Some(&new)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["oauthAccount"]["accountUuid"], "acc-2");
        assert_eq!(v["numStartups"], 3);
        assert_eq!(v["cachedUsageUtilization"]["accountUuid"], "acc-1");
        // Key order is kept (serde_json `preserve_order`).
        assert!(
            out.trim_start().starts_with("{\n  \"numStartups\""),
            "{out}"
        );
    }

    #[test]
    fn account_id_and_utilization_scope_to_organization() {
        let c = parse(SAMPLE).unwrap();
        let a = c.account.unwrap();
        assert_eq!(a.id().as_str(), "acc-1/org-1");
        let u = c.utilization.unwrap();
        assert_eq!(u.account_id.as_str(), "acc-1/org-1");
        assert!(u.org_from_profile);

        // Explicit organizationUuid on cachedUsageUtilization wins over oauthAccount's.
        let explicit = r#"{
          "oauthAccount": {"accountUuid": "acc-1", "organizationUuid": "org-new"},
          "cachedUsageUtilization": {"fetchedAtMs": 1795348787083, "accountUuid": "acc-1", "organizationUuid": "org-old",
            "utilization": {"seven_day": {"utilization": 10, "resets_at": "2026-11-25T14:59:59Z"}}}
        }"#;
        let u = parse(explicit).unwrap().utilization.unwrap();
        assert_eq!(u.account_id.as_str(), "acc-1/org-old");
        assert!(!u.org_from_profile);
    }

    #[test]
    fn team_and_enterprise_plans_are_not_overridden_by_user_rate_limit_tier() {
        // 1. Team org with organizationRateLimitTier = default_raven and stale userRateLimitTier = max_5x.
        let team_raven = Account::from_value(&serde_json::json!({
            "accountUuid": "acc-1", "organizationUuid": "org-team",
            "organizationRateLimitTier": "default_raven",
            "userRateLimitTier": "default_claude_max_5x"
        }))
        .unwrap();
        assert!(team_raven.is_org_plan());
        assert_eq!(
            team_raven.effective_tier().as_deref(),
            Some("default_raven")
        );
        assert_eq!(team_raven.plan_label().as_deref(), Some("team"));

        // 2. Team org with seatTier and userRateLimitTier = max_5x.
        let team_seat = Account::from_value(&serde_json::json!({
            "accountUuid": "acc-1", "organizationUuid": "org-team",
            "seatTier": "seat_tier_claude_team_premium",
            "userRateLimitTier": "default_claude_max_5x"
        }))
        .unwrap();
        assert!(team_seat.is_org_plan());
        assert_eq!(
            team_seat.effective_tier().as_deref(),
            Some("seat_tier_claude_team_premium")
        );
        assert_eq!(team_seat.plan_label().as_deref(), Some("team_premium"));

        // 3. Team org with planDisplayName = Team and userRateLimitTier = max_5x.
        let team_display = Account::from_value(&serde_json::json!({
            "accountUuid": "acc-1", "organizationUuid": "org-team",
            "planDisplayName": "Team",
            "userRateLimitTier": "default_claude_max_5x"
        }))
        .unwrap();
        assert!(team_display.is_org_plan());
        assert_eq!(team_display.effective_tier().as_deref(), Some("team"));
        assert_eq!(team_display.plan_label().as_deref(), Some("Team"));

        // 4. Team org enriched from .credentials.json (subscriptionType = team, rateLimitTier = max_5x):
        // distinct bucket from personal default_claude_max_5x.
        let mut team_cred = Account::from_value(&serde_json::json!({
            "accountUuid": "acc-1", "organizationUuid": "org-team",
            "userRateLimitTier": "default_claude_max_5x"
        }))
        .unwrap();
        team_cred.apply_credentials(&LoginMeta {
            subscription_type: Some("team".into()),
            rate_limit_tier: Some("default_claude_max_5x".into()),
            ..Default::default()
        });
        assert!(team_cred.is_org_plan());
        assert_eq!(
            team_cred.effective_tier().as_deref(),
            Some("team:default_claude_max_5x")
        );
        assert_eq!(team_cred.plan_label().as_deref(), Some("team (max_5x)"));

        // 5. Personal Max 5x and Pro.
        let personal_max = Account::from_value(&serde_json::json!({
            "accountUuid": "acc-1", "organizationUuid": "org-personal",
            "billingType": "stripe_subscription",
            "userRateLimitTier": "default_claude_max_5x"
        }))
        .unwrap();
        assert!(!personal_max.is_org_plan());
        assert_eq!(
            personal_max.effective_tier().as_deref(),
            Some("default_claude_max_5x")
        );
        assert_eq!(personal_max.plan_label().as_deref(), Some("max_5x"));
        assert_ne!(team_cred.effective_tier(), personal_max.effective_tier());

        let mut personal_pro = Account::from_value(&serde_json::json!({
            "accountUuid": "acc-2", "organizationUuid": "org-pro",
            "userRateLimitTier": "default_claude_ai"
        }))
        .unwrap();
        personal_pro.apply_credentials(&LoginMeta {
            subscription_type: Some("pro".into()),
            rate_limit_tier: Some("default_claude_ai".into()),
            ..Default::default()
        });
        assert_eq!(
            personal_pro.effective_tier().as_deref(),
            Some("default_claude_pro")
        );
        assert_eq!(personal_pro.plan_label().as_deref(), Some("pro"));
    }
}

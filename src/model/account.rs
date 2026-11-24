//! The account behind a login, as any harness names it: Claude Code's profile, the account
//! agy's token or Codex's `auth.json` names. How Claude Code's profile reads into it, and its
//! plan rules, stay with Claude Code's reader (`claude::config_json`).

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Account {
    pub account_uuid: String,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
    pub email: Option<String>,
    pub display_name: Option<String>,
    /// `planDisplayName` from `oauthAccount`.
    pub plan_display_name: Option<String>,
    /// `seatTier` from `oauthAccount` (set on Team/Enterprise org seats).
    pub seat_tier: Option<String>,
    /// `organizationRateLimitTier` from `oauthAccount`.
    pub organization_rate_limit_tier: Option<String>,
    /// `userRateLimitTier` from `oauthAccount` (user-scoped; may reflect a personal plan even
    /// when the active organization is Team/Enterprise).
    pub user_rate_limit_tier: Option<String>,
    pub billing_type: Option<String>,
    /// `subscriptionType` from `oauthAccount` or `.credentials.json` (`pro`, `max`, `team`,
    /// `enterprise`).
    pub subscription_type: Option<String>,
    /// `rateLimitTier` from `.credentials.json` (scoped to this seat's OAuth grant).
    pub cred_rate_limit_tier: Option<String>,
    /// When Claude Code last fetched this profile; a login newer than this is not yet confirmed.
    pub profile_fetched_at: Option<DateTime<Utc>>,
}

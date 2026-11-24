//! The status line's limit readings: what `swarmy statusline` keeps of a call in
//! `<data>/statusline/<seat id>.json`, and how the dashboard merges such a reading with the one
//! Claude Code caches. A published language between the status line (writer) and the dashboard
//! (reader), like `fleet.json` the other way.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::input::{Input, Window};
use crate::claude::config_json::Utilization;
use crate::core::ids::{AccountId, SeatId};
use crate::core::limits::{Reading, WindowKind};

/// The format of a reading file. 3: `account` carries `<accountUuid>/<organizationUuid>` when an
/// organization is known; older versions (1 without account, 2 with bare `accountUuid`) are
/// ignored until the next response writes 3.
pub const VERSION: u32 = 3;

/// What `swarmy statusline` keeps of one call: a limit reading of one account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveLimits {
    pub v: u32,
    /// When these numbers were true: the time of the response they came with (the last one in
    /// the session's transcript), else when the status line first saw them. A repeat of the same
    /// numbers never moves it forward.
    pub at: DateTime<Utc>,
    /// The account the numbers belong to: the owner of the session's last response
    /// (`session-owner`), else the seat's `oauthAccount` when the line ran. Required: a reading
    /// without its account cannot be booked to anyone.
    pub account: AccountId,
    /// The config dir of the Claude Code that ran it.
    pub config_dir: String,
    pub session_id: Option<String>,
    /// (used %, resets at) per window.
    pub five_hour: Option<(f64, DateTime<Utc>)>,
    pub seven_day: Option<(f64, DateTime<Utc>)>,
}

impl LiveLimits {
    /// The same account and the same numbers: nothing new.
    fn repeats(&self, other: &LiveLimits) -> bool {
        self.account == other.account
            && self.five_hour == other.five_hour
            && self.seven_day == other.seven_day
    }
}

/// The file a seat's readings go to: named by its seat id (a hash of its config dir), so
/// `swarmy statusline` needs nothing but `CLAUDE_CONFIG_DIR` to find it.
pub fn file_for(data_dir: &Path, config_dir: &Path) -> PathBuf {
    file_of(data_dir, &crate::claude::seats::seat_id(config_dir))
}

/// The status-line file of a seat id.
pub fn file_of(data_dir: &Path, seat: &SeatId) -> PathBuf {
    data_dir.join("statusline").join(format!("{seat}.json"))
}

/// The windows `rate_limits` carries, when it carries any.
pub fn windows(input: &Input) -> Option<Windows> {
    let rl = input.rate_limits.as_ref()?;
    let five_hour = rl.five_hour.as_ref().and_then(Window::pair);
    let seven_day = rl.seven_day.as_ref().and_then(Window::pair);
    if five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    Some((five_hour, seven_day))
}

/// (five hours, seven days): (used %, resets at) each.
pub type Windows = (Option<(f64, DateTime<Utc>)>, Option<(f64, DateTime<Utc>)>);

/// Whether these windows say the same as the stored reading: then there is nothing to write,
/// and nothing more to read (the account and the time) to find that out.
pub fn same_numbers(old: Option<&LiveLimits>, w: &Windows) -> bool {
    old.is_some_and(|o| o.five_hour == w.0 && o.seven_day == w.1)
}

pub fn live_from(
    input: &Input,
    config_dir: &Path,
    account: AccountId,
    at: DateTime<Utc>,
) -> Option<LiveLimits> {
    let (five_hour, seven_day) = windows(input)?;
    Some(LiveLimits {
        v: VERSION,
        at,
        account,
        config_dir: config_dir.to_string_lossy().into_owned(),
        session_id: input.session_id.clone(),
        five_hour,
        seven_day,
    })
}

/// What to write over the stored reading `old`, if anything (review M5): a repeat keeps the
/// time its numbers were first seen (nothing is written), and a reading older than the stored
/// one never goes over it, so an idle session's stale numbers cannot pass for fresh ones or
/// flip the file back and forth with a busy session's.
pub fn next(old: Option<&LiveLimits>, new: LiveLimits) -> Option<LiveLimits> {
    match old {
        Some(o) if o.repeats(&new) || o.at > new.at => None,
        Some(_) | None => Some(new),
    }
}

/// Writes atomically (`fsutil::atomic_write`); errors are ignored by the caller, a status line
/// must never fail.
pub fn write(path: &Path, live: &LiveLimits) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(live)?;
    crate::fsutil::atomic_write(path, &bytes, crate::fsutil::Mode::Private)
}

/// The stored reading; one in another format (an older swarmy's, without its account) is none.
pub fn read(path: &Path) -> Option<LiveLimits> {
    serde_json::from_slice::<LiveLimits>(&std::fs::read(path).ok()?)
        .ok()
        .filter(|l| l.v == VERSION)
}

/// The utilization of the seat's `account`: the status-line reading when it is that account's
/// and newer than Claude Code's cached one. A reading of another account (the seat was switched
/// since, or the session still ran the old login) is not this seat's now and is left out.
pub fn merge(
    cached: Option<Utilization>,
    account: &AccountId,
    live: Option<LiveLimits>,
) -> Option<Utilization> {
    let Some(live) = live.filter(|l| &l.account == account) else {
        return cached;
    };
    if cached.as_ref().is_some_and(|c| c.fetched_at >= live.at) {
        return cached;
    }
    let mut readings: Vec<Reading> = Vec::new();
    for (kind, v) in [
        (WindowKind::FiveHour, live.five_hour),
        (WindowKind::SevenDay, live.seven_day),
    ] {
        if let Some((utilization, resets_at)) = v {
            readings.extend(Reading::parse(kind, utilization, resets_at, live.at));
        }
    }
    // Windows the status line does not carry (per-model weekly limits) keep the cached values.
    let cached = cached.filter(|c| &c.account_id == account);
    if let Some(c) = cached.as_ref() {
        readings.extend(
            c.readings
                .iter()
                .filter(|r| !matches!(r.kind, WindowKind::FiveHour | WindowKind::SevenDay))
                .copied(),
        );
    }
    Some(Utilization {
        account_id: account.clone(),
        org_from_profile: false,
        fetched_at: live.at,
        readings,
        extra_usage_enabled: cached.is_some_and(|c| c.extra_usage_enabled),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::input::parse_input;
    use chrono::TimeZone;

    const INPUT: &str = r#"{"session_id":"s1","model":{"display_name":"Opus"},"cost":{"total_cost_usd":4.2},
        "context_window":{"used_percentage":42.0},
        "rate_limits":{"five_hour":{"used_percentage":12.5,"resets_at":1795400000},
                       "seven_day":{"used_percentage":3,"resets_at":1795960000}}}"#;

    fn acc(s: &str) -> AccountId {
        AccountId::new(s)
    }

    #[test]
    fn readings_come_from_rate_limits_and_win_when_newer() {
        let input = parse_input(INPUT);
        let at = Utc.timestamp_opt(1_795_380_000, 0).unwrap();
        let live = live_from(&input, Path::new("/x/.claude"), acc("acc"), at).unwrap();
        assert_eq!(live.seven_day.unwrap().0, 3.0);
        assert_eq!(live.five_hour.unwrap().1.timestamp(), 1_795_400_000);

        // Older cached reading (95%) of the same account: the live one replaces it.
        let cached = Utilization {
            account_id: acc("acc"),
            org_from_profile: false,
            fetched_at: at - chrono::Duration::hours(1),
            readings: vec![Reading {
                kind: WindowKind::SevenDay,
                utilization: 95.0,
                resets_at: at,
                fetched_at: at,
            }],
            extra_usage_enabled: false,
        };
        let m = merge(Some(cached.clone()), &acc("acc"), Some(live.clone())).unwrap();
        let week = m
            .readings
            .iter()
            .find(|r| r.kind == WindowKind::SevenDay)
            .unwrap();
        assert_eq!((week.utilization, m.fetched_at), (3.0, at));

        // A newer cached reading stays.
        let newer = Utilization {
            fetched_at: at + chrono::Duration::minutes(1),
            ..cached.clone()
        };
        assert_eq!(
            merge(Some(newer.clone()), &acc("acc"), Some(live.clone())),
            Some(newer)
        );
    }

    /// Review H2: the seat ran acc-a when the status line wrote 80%, and was switched to acc-b
    /// since; Claude Code's cached reading is older. The reading is acc-a's: it is never booked
    /// to acc-b, whatever the seat holds now.
    #[test]
    fn a_reading_of_another_account_is_not_the_seats() {
        let at = Utc.timestamp_opt(1_795_380_000, 0).unwrap();
        let input = parse_input(INPUT);
        let a = live_from(&input, Path::new("/x"), acc("acc-a"), at).unwrap();
        let cached_b = Utilization {
            account_id: acc("acc-b"),
            org_from_profile: false,
            fetched_at: at - chrono::Duration::hours(3),
            readings: vec![],
            extra_usage_enabled: true,
        };
        assert_eq!(
            merge(Some(cached_b.clone()), &acc("acc-b"), Some(a.clone())),
            Some(cached_b)
        );
        assert_eq!(merge(None, &acc("acc-b"), Some(a)), None);
    }

    /// Review H2: a file an older swarmy wrote has no account (and no `v`): it is not read.
    #[test]
    fn a_reading_without_its_account_is_ignored() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("r.json");
        std::fs::write(
            &p,
            r#"{"at":"2026-11-22T10:00:00Z","config_dir":"/x","session_id":null,
                "five_hour":[12.0,"2026-11-22T14:00:00Z"],"seven_day":null}"#,
        )
        .unwrap();
        assert_eq!(read(&p), None);
        let at = Utc.timestamp_opt(1_795_380_000, 0).unwrap();
        let live = live_from(&parse_input(INPUT), Path::new("/x"), acc("acc-a"), at).unwrap();
        write(&p, &live).unwrap();
        assert_eq!(read(&p), Some(live));
    }

    /// Review M5: a repeat never moves the time forward, and an older reading (an idle
    /// session's stale numbers) never goes over a newer one (a busy session's), so two sessions
    /// on one seat leave the newest reading whatever order they run in.
    #[test]
    fn a_reading_keeps_the_time_its_numbers_were_first_true() {
        let t0 = Utc.timestamp_opt(1_795_380_000, 0).unwrap();
        let reading = |pct: f64, at: DateTime<Utc>| LiveLimits {
            v: VERSION,
            at,
            account: acc("acc-a"),
            config_dir: "/x".into(),
            session_id: None,
            five_hour: None,
            seven_day: Some((pct, t0 + chrono::Duration::days(3))),
        };
        let first = reading(40.0, t0);
        // The same 40% an hour later (an idle session on `refreshInterval`): nothing to write.
        assert_eq!(
            next(Some(&first), reading(40.0, t0 + chrono::Duration::hours(1))),
            None
        );
        // New numbers: written, with their own time.
        let busy = reading(55.0, t0 + chrono::Duration::hours(2));
        assert_eq!(next(Some(&first), busy.clone()), Some(busy.clone()));
        // The idle session runs again with its 40% from the response at t0: older, not written.
        assert_eq!(next(Some(&busy), reading(40.0, t0)), None);
        // In the other order the newest also wins.
        assert_eq!(next(Some(&reading(40.0, t0)), busy.clone()), Some(busy));
        assert_eq!(next(None, first.clone()), Some(first));
    }
}

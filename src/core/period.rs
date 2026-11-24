//! Reporting periods.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};

#[derive(Debug, Clone, PartialEq)]
pub enum Period {
    Today,
    Yesterday,
    /// The last N hours.
    Hours(i64),
    /// The last N days, counted back from now.
    Days(i64),
    /// The current calendar month.
    Month,
    /// The current weekly-limit window of an account (resolved by the caller; 7 days otherwise).
    LimitWeek,
    All,
    Range(NaiveDate, NaiveDate),
}

impl std::str::FromStr for Period {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        // At most a century back: anything longer is a typo, and would overflow the date maths.
        let num = |t: &str, max: i64| t.parse::<i64>().ok().filter(|n| (1..=max).contains(n));
        Ok(match s.as_str() {
            "today" => Period::Today,
            "yesterday" => Period::Yesterday,
            "week" | "limit" | "limit-week" => Period::LimitWeek,
            "month" => Period::Month,
            "all" => Period::All,
            _ => {
                if let Some(n) = s.strip_suffix('d').and_then(|t| num(t, MAX_DAYS)) {
                    Period::Days(n)
                } else if let Some(n) = s.strip_suffix('h').and_then(|t| num(t, MAX_DAYS * 24)) {
                    Period::Hours(n)
                } else if let Some((a, b)) = s.split_once("..") {
                    let (a, b) = (parse_day(a)?, parse_day(b)?);
                    if a > b {
                        return Err(format!("range '{s}' ends before it starts"));
                    }
                    Period::Range(a, b)
                } else if let Ok(d) = parse_day(&s) {
                    Period::Range(d, d)
                } else {
                    return Err(format!(
                        "unknown period '{s}': use today, yesterday, week, month, all, 24h, 7d, 30d, YYYY-MM-DD or FROM..TO"
                    ));
                }
            }
        })
    }
}

/// The longest period `Nd` / `Nh` accepted: a century.
const MAX_DAYS: i64 = 36_500;

/// A day `YYYY-MM-DD` within years 1..=9999 (chrono parses far wider years, whose next day or
/// local midnight would overflow).
fn parse_day(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .ok()
        .filter(|d| (1..=9999).contains(&d.year()))
        .ok_or_else(|| format!("bad date '{s}', expected YYYY-MM-DD"))
}

/// Spend is summed per UTC quarter hour and regrouped into local days and hours afterwards (far
/// cheaper than SQLite's `localtime` on every row). Every UTC offset in use is a whole number of
/// quarter hours (+05:30, +05:45, −03:30, +12:45), so a quarter hour never straddles a local
/// midnight or hour, whatever the zone and across DST changes.
pub const BUCKET_MS: i64 = 15 * 60 * 1000;

/// The start of bucket `bucket` (a count of [`BUCKET_MS`] since the epoch) in the zone `tz`.
pub fn bucket_start<Tz: TimeZone>(bucket: i64, tz: &Tz) -> DateTime<Tz> {
    let t = Utc
        .timestamp_millis_opt(bucket.saturating_mul(BUCKET_MS))
        .single()
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    t.with_timezone(tz)
}

/// A half-open UTC interval `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Span {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// `t` floored to its UTC quarter hour ([`BUCKET_MS`]).
pub fn quarter_floor(t: DateTime<Utc>) -> DateTime<Utc> {
    let ms = t.timestamp_millis();
    Utc.timestamp_millis_opt(ms - ms.rem_euclid(BUCKET_MS))
        .single()
        .unwrap_or(t)
}

impl Span {
    /// The span of the same length right before this one, for trends ("vs previous 7d"). Its
    /// start is floored to the quarter hour like every rolling start (see [`Period::span`]).
    pub fn previous(self) -> Span {
        Span {
            start: quarter_floor(self.start - (self.end - self.start)),
            end: self.start,
        }
    }
}

/// The UTC instant of the local midnight that starts day `d` (a quarter hour in every zone).
/// The one rule for "today" (review B-M20: the dashboard's own copies fell back to `now` on a
/// day whose midnight does not exist, and showed today's spend as about $0).
pub fn local_midnight(d: NaiveDate) -> DateTime<Utc> {
    local_midnight_in(d, &Local)
}

/// [`local_midnight`] in zone `tz`. Where DST starts at midnight (Chile, Cuba, Paraguay) the
/// day starts at the first local time that exists.
pub fn local_midnight_in<Tz: TimeZone>(d: NaiveDate, tz: &Tz) -> DateTime<Utc> {
    let naive = d.and_time(chrono::NaiveTime::MIN);
    (0..=4)
        .find_map(|q| {
            tz.from_local_datetime(&(naive + Duration::minutes(15 * q)))
                .earliest()
        })
        .map_or_else(|| naive.and_utc(), |t| t.with_timezone(&Utc))
}

impl Period {
    /// Resolves to a UTC span in the local calendar. `limit_week` is the current weekly window
    /// when the caller knows it.
    ///
    /// Rolling periods (`Nd`, `Nh`) start on the UTC quarter hour at or before `now − N`: "last
    /// 30d" is "since the quarter hour 30 days ago". Spend older than the raw window is kept in
    /// quarter-hour rollups, which cannot be split, so a span that starts on a quarter hour gives
    /// the same number whether its far end is raw or rolled up. Calendar periods start at a local
    /// midnight, which is a quarter hour in every zone. The limit week is the window in force and
    /// always lies inside the raw window.
    pub fn span(&self, now: DateTime<Utc>, limit_week: Option<Span>) -> Span {
        let today = now.with_timezone(&Local).date_naive();
        let end = now + Duration::seconds(1);
        match self {
            Period::Today => Span {
                start: local_midnight(today),
                end,
            },
            Period::Yesterday => Span {
                start: local_midnight(today - Duration::days(1)),
                end: local_midnight(today),
            },
            Period::Hours(h) => Span {
                start: quarter_floor(now - Duration::hours(*h)),
                end,
            },
            Period::Days(d) => Span {
                start: quarter_floor(now - Duration::days(*d)),
                end,
            },
            Period::Month => Span {
                start: local_midnight(today - chrono::Duration::days(i64::from(today.day0()))),
                end,
            },
            Period::LimitWeek => limit_week.unwrap_or(Span {
                start: now - Duration::days(7),
                end,
            }),
            Period::All => Span {
                start: DateTime::<Utc>::MIN_UTC,
                end,
            },
            Period::Range(a, b) => Span {
                start: local_midnight(*a),
                end: local_midnight(*b + Duration::days(1)),
            },
        }
    }

    pub fn label(&self) -> String {
        match self {
            Period::Today => "today".into(),
            Period::Yesterday => "yesterday".into(),
            Period::Hours(h) => format!("last {h}h"),
            Period::Days(d) => format!("last {d}d"),
            Period::Month => "this month".into(),
            Period::LimitWeek => "limit week".into(),
            Period::All => "all time".into(),
            Period::Range(a, b) if a == b => a.to_string(),
            Period::Range(a, b) => format!("{a}..{b}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_midnight_in_a_fixed_zone() {
        let kyiv = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        assert_eq!(
            local_midnight_in(d, &kyiv).to_rfc3339(),
            "2026-11-23T22:00:00+00:00"
        );
    }

    #[test]
    fn parses() {
        assert_eq!("7d".parse::<Period>().unwrap(), Period::Days(7));
        assert_eq!("24h".parse::<Period>().unwrap(), Period::Hours(24));
        assert_eq!("week".parse::<Period>().unwrap(), Period::LimitWeek);
        let d = |m, dd| NaiveDate::from_ymd_opt(2026, m, dd).unwrap();
        assert_eq!(
            "2026-11-01..2026-11-22".parse::<Period>().unwrap(),
            Period::Range(d(11, 1), d(11, 22))
        );
        assert_eq!(
            "2026-11-20".parse::<Period>().unwrap(),
            Period::Range(d(11, 20), d(11, 20))
        );
        assert!("fortnight".parse::<Period>().is_err());
        assert!("0d".parse::<Period>().is_err());
        assert!("-3d".parse::<Period>().is_err());
    }

    #[test]
    fn reversed_ranges_and_absurd_values_are_refused_without_panicking() {
        let err = "2026-11-22..2026-11-01".parse::<Period>().unwrap_err();
        assert!(err.contains("ends before it starts"), "{err}");
        // Values that used to panic in chrono's date maths.
        for bad in [
            "99999999999d",
            "99999999999h",
            "36501d",
            "876001h",
            "+262143-12-31",
            "0000-01-01..2026-01-01",
        ] {
            assert!(bad.parse::<Period>().is_err(), "accepted: {bad}");
        }
        // The largest accepted values resolve to a span without panicking.
        let now = Utc::now();
        for ok in ["36500d", "876000h", "0001-01-01..9999-12-31"] {
            let p: Period = ok.parse().unwrap();
            let s = p.span(now, None);
            assert!(s.start < s.end, "{ok}");
        }
    }

    #[test]
    fn quarter_hour_buckets_keep_local_days_in_odd_offset_zones() {
        use chrono::FixedOffset;
        // Local 00:10 on 11-22 and local 23:50 on 11-21 in zones with 30- and 45-minute offsets:
        // the bucket of each lands on its own local day and hour.
        for (offset_min, label) in [(330, "+05:30"), (345, "+05:45"), (-210, "-03:30")] {
            let tz = FixedOffset::east_opt(offset_min * 60).unwrap();
            for (local, day, hour) in [
                ("2026-11-22T00:10:00", "2026-11-22", "00"),
                ("2026-11-21T23:50:00", "2026-11-21", "23"),
                ("2026-11-22T00:00:00", "2026-11-22", "00"),
            ] {
                let t = DateTime::parse_from_rfc3339(&format!("{local}{label}")).unwrap();
                let bucket = t.timestamp_millis().div_euclid(BUCKET_MS);
                let start = bucket_start(bucket, &tz);
                assert_eq!(start.format("%Y-%m-%d").to_string(), day, "{local}{label}");
                assert_eq!(start.format("%H").to_string(), hour, "{local}{label}");
            }
        }
        // With UTC hours (the old buckets) the first half hour of a +05:30 day went to the day
        // before: 00:10 local is 18:40 UTC, whose hour starts at 23:30 local the previous day.
        let tz = FixedOffset::east_opt(330 * 60).unwrap();
        let t = DateTime::parse_from_rfc3339("2026-11-22T00:10:00+05:30").unwrap();
        let utc_hour = t.timestamp_millis() / 3_600_000;
        let old = Utc
            .timestamp_millis_opt(utc_hour * 3_600_000)
            .unwrap()
            .with_timezone(&tz);
        assert_eq!(old.format("%Y-%m-%d").to_string(), "2026-11-21");
    }

    #[test]
    fn rolling_periods_start_on_a_quarter_hour() {
        let now = DateTime::parse_from_rfc3339("2026-11-22T10:37:12.345Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let end = now + Duration::seconds(1);
        // 10:37:12.345 minus 7 days is 11-15 10:37:12.345, floored to 10:30.
        assert_eq!(
            Period::Days(7).span(now, None),
            Span {
                start: t("2026-11-15T10:30:00Z"),
                end
            }
        );
        assert_eq!(
            Period::Hours(24).span(now, None).start,
            t("2026-11-21T10:30:00Z")
        );
        assert_eq!(
            Period::Hours(1).span(now, None).start,
            t("2026-11-22T09:30:00Z")
        );
        // Already on a quarter hour: unchanged.
        let on = t("2026-11-22T10:45:00Z");
        assert_eq!(
            Period::Days(30).span(on, None).start,
            t("2026-10-23T10:45:00Z")
        );
        // The rolling figure moves in quarter-hour steps: one bucket later, one bucket on.
        let a = Period::Days(7)
            .span(t("2026-11-22T10:44:59.999Z"), None)
            .start;
        let b = Period::Days(7).span(t("2026-11-22T10:45:00Z"), None).start;
        assert_eq!(b - a, Duration::minutes(15));
        // Negative times floor down, not towards zero.
        assert_eq!(
            quarter_floor(t("1969-12-31T23:59:00Z")),
            t("1969-12-31T23:45:00Z")
        );
        // The limit week is the window in force, never floored.
        let week = Span {
            start: t("2026-11-18T09:07:00Z"),
            end,
        };
        assert_eq!(Period::LimitWeek.span(now, Some(week)), week);
        // Calendar periods start at local midnights, which are quarter hours in every zone.
        for p in [Period::Today, Period::Yesterday, Period::Month] {
            assert_eq!(p.span(now, None).start.timestamp_millis() % BUCKET_MS, 0);
        }
    }

    #[test]
    fn the_previous_period_has_the_same_length_and_starts_on_a_quarter_hour() {
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let now = t("2026-11-22T10:37:12Z");
        let span = Period::Days(7).span(now, None);
        let prev = span.previous();
        assert_eq!(prev.end, span.start);
        // The span is 7 days, 7 minutes and 13 seconds long (10:30 to 10:37:13); the previous
        // one starts that much earlier, floored: 11-08 10:22:47 → 10:15.
        assert_eq!(prev.start, t("2026-11-08T10:15:00Z"));
        assert_eq!(prev.start.timestamp_millis() % BUCKET_MS, 0);
        assert!(prev.end - prev.start >= span.end - span.start);
        assert!(prev.end - prev.start < span.end - span.start + Duration::minutes(15));
    }

    #[test]
    fn range_is_inclusive_of_the_last_day() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let s = Period::Range(d, d).span(Utc::now(), None);
        assert_eq!((s.end - s.start).num_hours(), 24);
    }
}

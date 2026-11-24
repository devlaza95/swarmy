//! How long raw detail is kept and how big the index grows: the rules of bounded storage
//! (`docs/design/storage.md`), as pure functions of numbers the index and the seats report.

/// The raw window is never shorter than this: the current weekly limit window (7 days) is always
/// read from raw rows.
pub const MIN_RAW_DAYS: i64 = 8;

/// Days of per-request detail kept when `[storage] keep_raw_days` is not set.
pub const DEFAULT_KEEP_RAW_DAYS: i64 = 14;

/// Claude Code deletes transcripts after this many days unless a seat's `settings.json` sets
/// `cleanupPeriodDays` (verified in 2.1.329: "default: 30. Minimum 1").
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

/// `doctor` and SYSTEM warn when the 12-month projection goes over this many MB.
pub const DEFAULT_BUDGET_MB: u64 = 1024;

/// Bytes per row with its indexes, measured on a real index (design §1 and §6).
pub const RAW_REQUEST_BYTES: u64 = 481;
pub const RAW_TOOL_CALL_BYTES: u64 = 432;
pub const USAGE_ROLLUP_BYTES: u64 = 245;
pub const TOOL_ROLLUP_BYTES: u64 = 40;
pub const EDIT_ROLLUP_BYTES: u64 = 18;
/// What the archive grows by per request once its day is rolled up (estimated in §6 from the
/// measured month: a rollup row per 24 requests, tool rollups, edited files, agent rows).
pub const ARCHIVE_BYTES_PER_REQUEST: u64 = 18;

/// The raw window in effect: `explicit` (`[storage] keep_raw_days` or `compact --keep-days`),
/// else the default. It must keep the weekly window raw (at least [`MIN_RAW_DAYS`]) and end two
/// days inside the shortest transcript retention of the seats, so a cache rebuild finds every
/// raw day still on disk; the default shrinks to fit a short retention, an explicit value that
/// does not fit is refused, with the limit named. `retention` is (seat, days).
pub fn keep_raw_days(explicit: Option<i64>, retention: &[(String, u32)]) -> Result<i64, String> {
    let shortest = retention.iter().min_by_key(|(_, d)| *d);
    let most = shortest.map_or(i64::MAX, |(_, d)| (i64::from(*d) - 2).max(MIN_RAW_DAYS));
    match explicit {
        None => Ok(DEFAULT_KEEP_RAW_DAYS.min(most)),
        Some(k) if k < MIN_RAW_DAYS => Err(format!(
            "keep_raw_days = {k}: at least {MIN_RAW_DAYS} (the current weekly limit window must stay raw)"
        )),
        Some(k) if k > most => {
            let (seat, days) = shortest.map_or(("?", 0), |(s, d)| (s.as_str(), *d));
            Err(format!(
                "keep_raw_days = {k}: at most {most} (seat {seat} keeps transcripts {days} days, cleanupPeriodDays; a cache rebuild must find every raw day on disk)"
            ))
        }
        Some(k) => Ok(k),
    }
}

/// One daily sample of the index (`storage_samples`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Days since the epoch (local calendar).
    pub day: i64,
    /// Bytes in use: the file less its free pages.
    pub file_bytes: u64,
    pub raw_requests: u64,
    pub raw_tool_calls: u64,
}

impl Sample {
    /// The file less what its raw rows take: the archive and the tables kept forever.
    fn kept_bytes(&self) -> f64 {
        self.file_bytes as f64 - raw_bytes(self.raw_requests, self.raw_tool_calls) as f64
    }
}

/// What raw requests and tool calls take, with their indexes.
pub fn raw_bytes(requests: u64, tool_calls: u64) -> u64 {
    requests * RAW_REQUEST_BYTES + tool_calls * RAW_TOOL_CALL_BYTES
}

/// The index now and where it is going.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct Outlook {
    pub file_bytes: u64,
    /// Bytes of the raw window (estimated from rows).
    pub raw_bytes: u64,
    /// The rest of what is in use: rollups and the tables kept forever (free pages are not).
    pub kept_bytes: u64,
    /// Growth of the kept part per day, in bytes.
    pub per_day: f64,
    /// Whether `per_day` is the slope of at least a week of daily samples (else estimated from
    /// the requests a day and [`ARCHIVE_BYTES_PER_REQUEST`]).
    pub from_samples: bool,
    /// The raw window once full (constant from then on) plus the kept part a year from now: the
    /// bytes in use then (free pages are reused or, with incremental auto-vacuum, given back).
    pub in_12_months: u64,
}

/// Samples needed before the growth is their slope.
const MIN_SAMPLES: usize = 7;
/// The slope is taken over at most this many of the newest samples.
const SLOPE_SAMPLES: usize = 14;

/// The outlook from the index's size now: its file and the free pages in it, its raw rows
/// (spanning `raw_days`, the days between the oldest raw row and now), the raw window it keeps,
/// and its daily samples (oldest first).
pub fn outlook(
    file_bytes: u64,
    free_bytes: u64,
    raw_requests: u64,
    raw_tool_calls: u64,
    raw_days: f64,
    keep_raw_days: i64,
    samples: &[Sample],
) -> Outlook {
    let raw = raw_bytes(raw_requests, raw_tool_calls);
    let kept = file_bytes.saturating_sub(free_bytes).saturating_sub(raw);
    let days = raw_days.max(1.0);
    // The raw window once it holds `keep_raw_days` days at today's pace.
    let raw_full = bytes(raw as f64 / days * keep_raw_days as f64);
    let recent = &samples[samples.len().saturating_sub(SLOPE_SAMPLES)..];
    let slope = (recent.len() >= MIN_SAMPLES).then(|| {
        let n = recent.len() as f64;
        let mx = recent.iter().map(|s| s.day as f64).sum::<f64>() / n;
        let my = recent.iter().map(|s| s.kept_bytes()).sum::<f64>() / n;
        let sxx: f64 = recent.iter().map(|s| (s.day as f64 - mx).powi(2)).sum();
        let sxy: f64 = recent
            .iter()
            .map(|s| (s.day as f64 - mx) * (s.kept_bytes() - my))
            .sum();
        if sxx > 0.0 { (sxy / sxx).max(0.0) } else { 0.0 }
    });
    let per_day = slope.unwrap_or(raw_requests as f64 / days * ARCHIVE_BYTES_PER_REQUEST as f64);
    Outlook {
        file_bytes,
        raw_bytes: raw,
        kept_bytes: kept,
        per_day,
        from_samples: slope.is_some(),
        in_12_months: raw_full + kept + bytes(per_day * 365.0),
    }
}

/// A projected byte count: an estimate, so a negative or non-finite one is 0 and one past
/// `u64` is `u64::MAX`.
fn bytes(x: f64) -> u64 {
    if !(x.is_finite() && x > 0.0) {
        return 0;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "finite and above zero, checked above; `as` saturates past u64::MAX"
    )]
    let b = x as u64;
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seats(days: &[u32]) -> Vec<(String, u32)> {
        days.iter()
            .enumerate()
            .map(|(i, d)| (format!("s{i}"), *d))
            .collect()
    }

    #[test]
    fn the_raw_window_fits_the_weekly_window_and_the_retention() {
        // Claude Code's default 30 days: 14 by default, up to 28.
        assert_eq!(keep_raw_days(None, &seats(&[30, 90])), Ok(14));
        assert_eq!(keep_raw_days(Some(28), &seats(&[30, 90])), Ok(28));
        let e = keep_raw_days(Some(29), &seats(&[90, 30])).unwrap_err();
        assert!(
            e.contains("at most 28") && e.contains("seat s1 keeps transcripts 30 days"),
            "{e}"
        );
        let e = keep_raw_days(Some(7), &seats(&[30])).unwrap_err();
        assert!(e.contains("at least 8"), "{e}");
        // A short retention shrinks the default, never below the weekly window.
        assert_eq!(keep_raw_days(None, &seats(&[12])), Ok(10));
        assert_eq!(keep_raw_days(None, &seats(&[5])), Ok(8));
        assert_eq!(keep_raw_days(Some(8), &seats(&[5])), Ok(8));
        // No seats: nothing to fit.
        assert_eq!(keep_raw_days(None, &[]), Ok(14));
        assert_eq!(keep_raw_days(Some(60), &[]), Ok(60));
    }

    #[test]
    fn the_outlook_is_the_full_raw_window_plus_a_year_of_growth() {
        // No samples yet: 31 days of raw rows, 9,300 requests and 9,700 tool calls a day.
        // Raw: 288,300 × 481 + 300,700 × 432 = 138,672,300 + 129,902,400 = 268,574,700 B.
        // Kept: 280,000,000 − 268,574,700 = 11,425,300 B. Full window of 14 days:
        // 268,574,700 / 31 × 14 = 121,291,800 B. Growth: 9,300 × 18 = 167,400 B a day, a year
        // 61,101,000 B. In 12 months: 121,291,800 + 11,425,300 + 61,101,000 = 193,818,100 B.
        let o = outlook(280_000_000, 0, 288_300, 300_700, 31.0, 14, &[]);
        assert_eq!(o.raw_bytes, 268_574_700);
        assert_eq!(o.kept_bytes, 11_425_300);
        assert!(!o.from_samples);
        assert!((o.per_day - 167_400.0).abs() < 1e-6);
        assert_eq!(o.in_12_months, 193_818_100);
        // A week of samples whose kept part grows 400,000 B a day: the slope is that.
        let samples: Vec<Sample> = (0..8)
            .map(|d| Sample {
                day: 20_000 + d,
                file_bytes: 100_000_000 + 400_000 * d as u64 + raw_bytes(1000, 1000),
                raw_requests: 1000,
                raw_tool_calls: 1000,
            })
            .collect();
        let o = outlook(103_000_000, 0, 1000, 1000, 14.0, 14, &samples);
        assert!(o.from_samples);
        assert!((o.per_day - 400_000.0).abs() < 1e-3, "{o:?}");
        // Raw 913,000 B over 14 days of 14: the window is full. Kept 102,087,000 B; a year at
        // 400,000 B a day is 146,000,000 B.
        assert_eq!(o.in_12_months, 913_000 + 102_087_000 + 146_000_000);
        // Free pages (a compaction without auto-vacuum leaves them) are no part of what is kept:
        // 280,000,000 − 10,000,000 − 268,574,700 = 1,425,300 B; in 12 months 121,291,800 +
        // 1,425,300 + 61,101,000 = 183,818,100 B.
        let o = outlook(280_000_000, 10_000_000, 288_300, 300_700, 31.0, 14, &[]);
        assert_eq!(o.kept_bytes, 1_425_300);
        assert_eq!(o.in_12_months, 183_818_100);
    }
}

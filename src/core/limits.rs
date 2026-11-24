//! Weekly / five-hour limit windows and their attribution to projects.
//!
//! Anthropic reports only a utilization percentage per window. How the percentage is metered is
//! not published, so the attribution assumes it is proportional to API-equivalent cost; every
//! derived number is an estimate and is shown with "≈".

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::ids::ProjectKey;
use super::period::Span;
use super::units::{Pct, PctPerHour, Usd, UsdPerPct};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum WindowKind {
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
}

impl WindowKind {
    /// Every kind; the index's `CHECK` on `readings.kind` is generated from it.
    pub const ALL: [WindowKind; 4] = [
        WindowKind::FiveHour,
        WindowKind::SevenDay,
        WindowKind::SevenDayOpus,
        WindowKind::SevenDaySonnet,
    ];

    pub fn from_key(k: &str) -> Option<Self> {
        Some(match k {
            "five_hour" => WindowKind::FiveHour,
            "seven_day" => WindowKind::SevenDay,
            "seven_day_opus" => WindowKind::SevenDayOpus,
            "seven_day_sonnet" => WindowKind::SevenDaySonnet,
            _ => return None,
        })
    }

    pub fn key(self) -> &'static str {
        match self {
            WindowKind::FiveHour => "five_hour",
            WindowKind::SevenDay => "seven_day",
            WindowKind::SevenDayOpus => "seven_day_opus",
            WindowKind::SevenDaySonnet => "seven_day_sonnet",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            WindowKind::FiveHour => "5-hour",
            WindowKind::SevenDay => "weekly",
            WindowKind::SevenDayOpus => "weekly opus",
            WindowKind::SevenDaySonnet => "weekly sonnet",
        }
    }

    pub fn length(self) -> Duration {
        match self {
            WindowKind::FiveHour => Duration::hours(5),
            WindowKind::SevenDay | WindowKind::SevenDayOpus | WindowKind::SevenDaySonnet => {
                Duration::days(7)
            }
        }
    }
}

/// One utilization reading as Claude Code cached it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Reading {
    pub kind: WindowKind,
    /// Percent, 0..=100 (can exceed 100 with extra usage).
    pub utilization: f64,
    pub resets_at: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
}

impl Reading {
    /// A reading as a source reports it, kept only when it can be one: a finite percent not
    /// below 0, and a reset no further than one window (and an hour) after the fetch, which
    /// catches seconds read as milliseconds (review B-M12: nothing checked it, and a bad number
    /// became an estimate).
    pub fn parse(
        kind: WindowKind,
        utilization: f64,
        resets_at: DateTime<Utc>,
        fetched_at: DateTime<Utc>,
    ) -> Option<Reading> {
        let sane = utilization.is_finite()
            && utilization >= 0.0
            && resets_at <= fetched_at + kind.length() + Duration::hours(1);
        sane.then_some(Reading {
            kind,
            utilization,
            resets_at,
            fetched_at,
        })
    }

    pub fn window(&self) -> Span {
        let end = self.window_end();
        Span {
            start: end - self.kind.length(),
            end,
        }
    }

    /// When the window resets, to the nearest minute: sources report the same reset with
    /// sub-second jitter (`14:59:59.56` from `.claude.json`, `15:00:00` from the status line), and
    /// two readings of one window must name the same window.
    pub fn window_end(&self) -> DateTime<Utc> {
        let ms = self.resets_at.timestamp_millis();
        let minute = 60_000;
        let rounded = (ms + minute / 2).div_euclid(minute) * minute;
        DateTime::<Utc>::from_timestamp_millis(rounded).unwrap_or(self.resets_at)
    }

    /// The reading belongs to the same window as `other` (same kind, same reset).
    pub fn same_window(&self, other: &Reading) -> bool {
        self.kind == other.kind && self.window_end() == other.window_end()
    }

    /// The window has rolled over since the reading was taken.
    pub fn is_reset(&self, now: DateTime<Utc>) -> bool {
        now >= self.window_end()
    }
}

/// How full a limit window is: one set of thresholds for every front end (dashboard colours,
/// CLI tones, status-line ANSI codes, alerts). Each front end maps a level to its own colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Level {
    /// Under 60%.
    Ok,
    /// From 60% to under 85%.
    Warn,
    /// From 85% to under 100%.
    Bad,
    /// 100% or more: "limit reached".
    Reached,
}

impl Level {
    pub const WARN_AT: f64 = 60.0;
    pub const BAD_AT: f64 = 85.0;
    pub const REACHED_AT: f64 = 100.0;

    pub fn of(pct: f64) -> Level {
        if pct >= Self::REACHED_AT {
            Level::Reached
        } else if pct >= Self::BAD_AT {
            Level::Bad
        } else if pct >= Self::WARN_AT {
            Level::Warn
        } else {
            Level::Ok
        }
    }
}

/// The window `r` belongs to, rolled forward to the one that contains `now` when the reading is
/// from an earlier window (the window keeps its length and cadence). The nominal window: the one
/// in force, with resets by hand, is [`window_of`].
pub fn current_window(r: &Reading, now: DateTime<Utc>) -> Span {
    let mut w = r.window();
    while w.end <= now {
        w = Span {
            start: w.end,
            end: w.end + r.kind.length(),
        };
    }
    w
}

/// A reset happened between two analyses of the same kind of window: its (effective) start moved
/// forward, either because the window rolled over or because the limit was reset by hand (see
/// [`effective_start`]).
pub fn was_reset(before: Span, after: Span) -> bool {
    after.start > before.start
}

/// A reading must fall by more than this many points below the one before it to mark a reset.
pub const RESET_FALL: f64 = 2.0;

/// How long sources of one account may disagree: two seats logged into the same account, the
/// status line and `.claude.json`, each report what they last fetched, so a reading can lag a few
/// minutes behind another one.
pub const SOURCES_DISAGREE_FOR: Duration = Duration::minutes(15);

/// The effective start of `cur`'s window, and the moment of a reset done by hand.
///
/// Claude Code can reset a 5-hour (and, sometimes, the weekly) limit early. Utilization then falls
/// to near zero inside the window, so spend before the reset must not count. A fall of more than
/// [`RESET_FALL`] points between consecutive readings of the same window marks a reset; it
/// happened between the last high and the first low reading, and the last high one's time is
/// taken as the start (a slight over-count when readings are far apart).
///
/// Readings come from several sources that lag behind each other, so a fall is a reset only when
/// it holds: the low reading is not a value the account already had just before the high one (a
/// lagging source), and the readings soon after it are not mostly back up at the high level.
/// Several readings of one moment count as the highest.
pub fn effective_start(
    history: &[Reading],
    cur: &Reading,
) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
    let nominal = cur.window().start;
    let mut hist: Vec<Reading> = history
        .iter()
        .filter(|h| h.same_window(cur) && h.fetched_at <= cur.fetched_at)
        .copied()
        .collect();
    hist.push(*cur);
    hist.sort_by(|a, b| {
        a.fetched_at
            .cmp(&b.fetched_at)
            .then(b.utilization.total_cmp(&a.utilization))
    });
    // One reading per moment, the highest (sorted first).
    hist.dedup_by_key(|h| h.fetched_at);
    let mut start = nominal;
    let mut reset = None;
    for j in 1..hist.len() {
        let (a, b) = (hist[j - 1], hist[j]);
        if b.utilization + RESET_FALL >= a.utilization || a.fetched_at <= start {
            continue;
        }
        let lagging = hist[..j - 1]
            .iter()
            .rev()
            .take_while(|c| c.fetched_at >= a.fetched_at - SOURCES_DISAGREE_FOR)
            .any(|c| c.utilization <= b.utilization + RESET_FALL);
        let (back_up, low): (Vec<&Reading>, Vec<&Reading>) = hist[j + 1..]
            .iter()
            .take_while(|c| c.fetched_at <= b.fetched_at + SOURCES_DISAGREE_FOR)
            .partition(|c| c.utilization + RESET_FALL >= a.utilization);
        let undone = back_up.len() > low.len();
        if !lagging && !undone {
            start = a.fetched_at;
            reset = Some(a.fetched_at);
        }
    }
    (start, reset)
}

/// What one project spent in a window.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSpend {
    /// What joins (the panels, other windows).
    pub project: ProjectKey,
    /// What is shown.
    pub name: String,
    pub cost: Usd,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Share {
    pub project: ProjectKey,
    pub name: String,
    pub cost: Usd,
    /// Share of the window's spend, 0..=1.
    pub share: f64,
    /// ≈ points of the limit this key consumed.
    pub limit_points: Pct,
}

/// Splits `utilization` points between projects in proportion to their cost.
pub fn attribute(utilization: Pct, costs: &[ProjectSpend]) -> Vec<Share> {
    let total: Usd = costs.iter().map(|p| p.cost.max(Usd::ZERO)).sum();
    let mut out: Vec<Share> = costs
        .iter()
        .map(|p| {
            let c = &p.cost;
            let share = if total > Usd::ZERO {
                c.max(Usd::ZERO) / total
            } else {
                0.0
            };
            Share {
                project: p.project.clone(),
                name: p.name.clone(),
                cost: *c,
                share,
                limit_points: utilization * share,
            }
        })
        .collect();
    out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    out
}

/// USD of API-equivalent spend per 1 point of utilization, from one reading.
pub fn usd_per_point(utilization: Pct, cost_until_reading: Usd) -> Option<UsdPerPct> {
    UsdPerPct::measured(cost_until_reading, utilization)
}

/// Median of several `usd_per_point` estimates (one per stored reading).
pub fn calibrate(samples: &[UsdPerPct]) -> Option<UsdPerPct> {
    let mut v: Vec<f64> = samples
        .iter()
        .filter(|x| x.is_usable())
        .map(|x| x.get())
        .collect();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let median = match n {
        0 => return None,
        _ if n % 2 == 1 => v.get(n / 2).copied(),
        _ => Some((v.get(n / 2 - 1)? + v.get(n / 2)?) / 2.0),
    };
    median.map(UsdPerPct::new)
}

/// Utilization now: the last reading plus what was spent since, converted at `usd_per_point`.
/// The spend never takes it past 100%: beyond, the next reading tells (a rate measured too low
/// would otherwise show thousands of percent, review B-H2).
pub fn estimate_now(reading: Pct, spent_since: Usd, usd_per_point: Option<UsdPerPct>) -> Pct {
    match usd_per_point {
        Some(r) if r.is_usable() => {
            let room = (Pct::new(Level::REACHED_AT) - reading).max(Pct::ZERO);
            reading + (spent_since / r).max(Pct::ZERO).min(room)
        }
        Some(_) | None => reading,
    }
}

/// Time until the limit reaches 100% at the given pace. Checked arithmetic: a pace of almost
/// nothing gives no time rather than a duration past chrono's range (review B-L1).
pub fn eta(current: Pct, pace: PctPerHour) -> Option<Duration> {
    if Level::of(current.get()) == Level::Reached {
        return Some(Duration::zero());
    }
    let hours = pace.hours_for(Pct::new(Level::REACHED_AT) - current)?;
    let secs = (hours * 3600.0).round();
    // An f64 past i64's range would saturate silently; beyond about 30 000 years is no time.
    if !(0.0..1e12).contains(&secs) {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a whole number of seconds in 0..1e12, checked above"
    )]
    let secs = secs as i64;
    Duration::try_seconds(secs)
}

/// When the window runs out at the current pace: the [`eta`], kept only when it comes before the
/// window resets (after the reset the pace no longer matters).
pub fn runs_out(
    current: Pct,
    pace: Option<PctPerHour>,
    now: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Option<Duration> {
    pace.and_then(|p| eta(current, p))
        .filter(|d| now.checked_add_signed(*d).is_some_and(|t| t < window_end))
}

/// Where the window ends up at the current pace: utilization at its reset, uncapped (over 100
/// means it runs out first). `None` without a positive pace.
pub fn projection_at_reset(
    current: Pct,
    pace: Option<PctPerHour>,
    now: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Option<Pct> {
    pace.filter(|p| p.get() > 0.0).map(|p| {
        let left = (window_end - now).num_seconds().max(0) as f64 / 3600.0;
        current + p.over(left)
    })
}

// ───────────────────────── estimating one window ─────────────────────────
//
// The rules below are pure: `crate::limits` queries the index for their inputs
// (`WindowInputs`) and turns the result into a report.

/// Burn rate is measured over the last this many minutes.
pub const BURN_MINUTES: i64 = 60;

/// Another account of the same plan tier serves as the reference ratio only when its USD per
/// point beats this account's own by more than this factor (20%): within that, the two differ by
/// noise, not by hidden usage.
pub const REFERENCE_MARGIN: f64 = 1.2;

/// At most this many recent readings (with distinct values) calibrate an account's ratio.
const CALIBRATION_READINGS: usize = 12;

/// Readings closer than this (points) count as one value when calibrating.
const CALIBRATION_STEP: f64 = 0.5;

/// Where a reading's window stands now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowPlace {
    /// The window that contains now; its start is moved to a reset by hand.
    pub window: Span,
    /// When the limit was reset by hand inside this window.
    pub manual_reset: Option<DateTime<Utc>>,
    /// The window has rolled over since the reading (a new window without a reading yet).
    pub reset_passed: bool,
}

/// The window in force for `r` now — the one definition every surface uses (`swarmy limits`, the
/// dashboard, `report --period week`): rolled forward when it reset since the reading, else its
/// nominal window with the start moved to a reset by hand found in `history` (readings of the same
/// kind).
pub fn window_of(r: &Reading, history: &[Reading], now: DateTime<Utc>) -> WindowPlace {
    let reset_passed = r.is_reset(now);
    let mut window = current_window(r, now);
    let mut manual_reset = None;
    if !reset_passed {
        let (start, reset) = effective_start(history, r);
        window.start = start;
        manual_reset = reset;
    }
    WindowPlace {
        window,
        manual_reset,
        reset_passed,
    }
}

/// What the estimate counts from: the reading and its time, or zero from the window start when the
/// window rolled over since the reading.
pub fn counting_from(r: &Reading, place: &WindowPlace) -> (f64, DateTime<Utc>) {
    if place.reset_passed {
        (0.0, place.window.start)
    } else {
        (r.utilization, r.fetched_at)
    }
}

/// The readings that calibrate an account's USD per point, each with the span whose spend it
/// reflects (from its effective window start to the reading): the most recent readings with
/// distinct values, newest first. `history` holds the stored readings of `r`'s kind, oldest first;
/// `r` is added when not stored yet.
pub fn calibration_points(history: &[Reading], r: &Reading) -> Vec<(Span, f64)> {
    let mut history = history.to_vec();
    if !history.iter().any(|h| h.fetched_at == r.fetched_at) {
        history.push(*r);
    }
    let mut recent: Vec<Reading> = Vec::new();
    for h in history.iter().rev() {
        if recent
            .last()
            .is_none_or(|l: &Reading| (l.utilization - h.utilization).abs() >= CALIBRATION_STEP)
        {
            recent.push(*h);
        }
        if recent.len() >= CALIBRATION_READINGS {
            break;
        }
    }
    recent
        .into_iter()
        .map(|h| {
            let (start, _) = effective_start(&history, &h);
            (
                Span {
                    start,
                    end: h.fetched_at,
                },
                h.utilization,
            )
        })
        .collect()
}

/// The account's own USD per point: the median over `(utilization, spend until it)` pairs.
pub fn own_ratio(points: &[(Pct, Usd)]) -> Option<UsdPerPct> {
    let samples: Vec<UsdPerPct> = points
        .iter()
        .filter_map(|(u, c)| usd_per_point(*u, *c))
        .collect();
    calibrate(&samples)
}

/// A plan tier within its harness: the key cross-account references are compared under. A
/// Codex `team` plan is not Claude Team (review B-M12: tiers were bare names of any harness).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlanTier {
    pub harness: crate::core::harness::Harness,
    pub tier: String,
}

impl PlanTier {
    /// The tier of an account, its harness read from the account id (`agy:…`, `cdx:…`, else
    /// Claude Code's).
    pub fn of(account: &str, tier: impl Into<String>) -> PlanTier {
        use crate::core::harness::Harness;
        let harness = if account.starts_with("agy:") {
            Harness::Agy
        } else if account.starts_with("cdx:") {
            Harness::Codex
        } else {
            Harness::Claude
        };
        PlanTier {
            harness,
            tier: tier.into(),
        }
    }
}

/// One account's own USD per point for a window kind, measured at its reading (`at`).
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    pub account: String,
    pub tier: PlanTier,
    pub kind: WindowKind,
    pub usd_per_point: UsdPerPct,
    pub at: DateTime<Utc>,
}

/// The reference of a (tier, window kind): the account with the highest own USD per point,
/// so the least hidden usage. A calibration older than one window no longer speaks for its
/// tier: a login parked weeks ago, calibrated in an unusual week, would set the rate for every
/// active account of the tier (review B-M13; the rule lived in the query layer, without it).
pub fn pick_references(
    c: &[Calibration],
    now: DateTime<Utc>,
) -> std::collections::HashMap<(PlanTier, WindowKind), Calibration> {
    let mut refs: std::collections::HashMap<(PlanTier, WindowKind), Calibration> =
        std::collections::HashMap::new();
    for x in c.iter().filter(|x| now - x.at <= x.kind.length()) {
        let e = refs
            .entry((x.tier.clone(), x.kind))
            .or_insert_with(|| x.clone());
        if x.usd_per_point > e.usd_per_point {
            *e = x.clone();
        }
    }
    refs
}

/// Whether a tier reference ratio replaces the account's own (see [`REFERENCE_MARGIN`]).
pub fn reference_wins(own: Option<UsdPerPct>, reference: UsdPerPct) -> bool {
    own.is_none_or(|o| reference > o * REFERENCE_MARGIN)
}

/// The step of a window's curve: about 170 points whatever its length, at least a minute.
pub fn curve_step(window: Span) -> Duration {
    Duration::minutes(((window.end - window.start).num_minutes() / 168).max(1))
}

/// Everything the estimate of one window needs, as queried from the index.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInputs {
    pub reading: Reading,
    pub place: WindowPlace,
    /// USD per point measured on this account alone ([`own_ratio`]).
    pub own_usd_per_point: Option<UsdPerPct>,
    /// Spend since the point [`counting_from`] names.
    pub spent_since: Usd,
    /// Spend from the window start until now.
    pub cost_in_window: Usd,
    /// Spend over the last [`BURN_MINUTES`].
    pub cost_burn: Usd,
    /// Spend in the window per project.
    pub by_project: Vec<ProjectSpend>,
    /// Spend per [`curve_step`] from the window start (empty when the window rolled over).
    pub series: Vec<Usd>,
    pub series_step: Duration,
}

/// The estimate of one window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowEstimate {
    /// USD per point used: the reference's when given, else the account's own.
    pub usd_per_point: Option<UsdPerPct>,
    pub estimate_now: Pct,
    /// ≈ points explained by spend visible on this machine.
    pub visible_points: Option<Pct>,
    /// ≈ points from claude.ai, the desktop app, other computers…
    pub outside_points: Option<Pct>,
    /// Dollars per hour.
    pub burn_usd_per_hour: Usd,
    /// Points per hour.
    pub points_per_hour: Option<PctPerHour>,
    /// When it runs out at this pace, if before the reset ([`runs_out`]).
    pub eta: Option<Duration>,
    /// The same as an instant, fixed when estimated: `now + eta` added again on every later tick
    /// would drift by up to a limits pass (review L2).
    pub runs_out_at: Option<DateTime<Utc>>,
    pub shares: Vec<Share>,
    /// ≈ API-equivalent dollars of a full window.
    pub window_usd: Option<Usd>,
    /// Cumulative spend through the window, scaled to end at `estimate_now`.
    pub curve: Vec<Pct>,
}

/// Estimates one window. `reference` is the tier reference ratio already chosen with
/// [`reference_wins`].
pub fn window_estimate(
    i: &WindowInputs,
    reference: Option<UsdPerPct>,
    now: DateTime<Utc>,
) -> WindowEstimate {
    let usd_per_point = reference.or(i.own_usd_per_point);
    let (base, _) = counting_from(&i.reading, &i.place);
    let estimate_now = estimate_now(Pct::new(base), i.spent_since, usd_per_point);
    // What the spend seen here explains can not exceed the estimate; the rest is elsewhere.
    let visible_points = usd_per_point.map(|u| (i.cost_in_window / u).min(estimate_now));
    let outside_points = visible_points.map(|v| (estimate_now - v).max(Pct::ZERO));
    let burn_usd_per_hour = i.cost_burn * 60.0 / BURN_MINUTES as f64;
    let points_per_hour = usd_per_point.map(|u| PctPerHour::of_burn(burn_usd_per_hour, u));
    let eta = runs_out(estimate_now, points_per_hour, now, i.place.window.end);
    let shares = attribute(visible_points.unwrap_or(estimate_now), &i.by_project);
    let total: Usd = i.series.iter().sum();
    // Points per dollar of the curve: it ends at the estimate.
    let scale = if total > Usd::ZERO {
        estimate_now.get() / total.get()
    } else {
        0.0
    };
    let mut acc = Usd::ZERO;
    let curve = i
        .series
        .iter()
        .map(|c| {
            acc += *c;
            Pct::new(acc.get() * scale)
        })
        .collect();
    WindowEstimate {
        usd_per_point,
        estimate_now,
        visible_points,
        outside_points,
        burn_usd_per_hour,
        points_per_hour,
        eta,
        runs_out_at: eta.map(|d| now + d),
        shares,
        window_usd: usd_per_point.map(|u| Pct::new(100.0) * u),
        curve,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review B-M12: a reading that cannot be one is refused where it comes in.
    #[test]
    fn a_reading_is_checked_where_it_comes_in() {
        let at = ts("2026-11-20T12:00:00Z");
        let reset = ts("2026-11-24T00:00:00Z");
        assert!(Reading::parse(WindowKind::SevenDay, 42.0, reset, at).is_some());
        assert!(Reading::parse(WindowKind::SevenDay, f64::NAN, reset, at).is_none());
        assert!(Reading::parse(WindowKind::SevenDay, -1.0, reset, at).is_none());
        // Seconds read as milliseconds: a reset thousands of years away.
        let far = DateTime::from_timestamp(reset.timestamp() * 1000, 0)
            .unwrap_or(reset + Duration::days(9999));
        assert!(Reading::parse(WindowKind::SevenDay, 42.0, far, at).is_none());
        // Extra usage reads past 100%.
        assert!(Reading::parse(WindowKind::FiveHour, 104.0, at + Duration::hours(3), at).is_some());
    }

    #[test]
    fn attribution_by_hand() {
        // 77% used; shop $810, blog $120, other $70 (total $1000).
        // shop: 0.81 → 62.37 points; blog: 0.12 → 9.24; other: 0.07 → 5.39.
        let s = attribute(
            Pct::new(77.0),
            &[
                spend("other", 70.0),
                spend("shop", 810.0),
                spend("blog", 120.0),
            ],
        );
        assert_eq!(s[0].name, "shop");
        assert!((s[0].share - 0.81).abs() < 1e-12);
        assert!((s[0].limit_points.get() - 62.37).abs() < 1e-9);
        assert!((s[1].limit_points.get() - 9.24).abs() < 1e-9);
        assert!((s[2].limit_points.get() - 5.39).abs() < 1e-9);
        let total: f64 = s.iter().map(|x| x.limit_points.get()).sum();
        assert!((total - 77.0).abs() < 1e-9);
    }

    #[test]
    fn zero_spend_attributes_nothing() {
        let s = attribute(Pct::new(10.0), &[spend("a", 0.0)]);
        assert_eq!(s[0].limit_points, Pct::ZERO);
    }

    #[test]
    fn calibration_and_estimate() {
        // $1000 spent for 50% → $20 per point. $100 more since then → +5 points.
        let r = usd_per_point(Pct::new(50.0), Usd::new(1000.0)).unwrap();
        assert_eq!(r, UsdPerPct::new(20.0));
        assert_eq!(
            estimate_now(Pct::new(50.0), Usd::new(100.0), Some(r)),
            Pct::new(55.0)
        );
        let rates = |v: &[f64]| v.iter().map(|x| UsdPerPct::new(*x)).collect::<Vec<_>>();
        assert_eq!(
            calibrate(&rates(&[30.0, 10.0, 20.0])),
            Some(UsdPerPct::new(20.0))
        );
        assert_eq!(calibrate(&rates(&[10.0, 20.0])), Some(UsdPerPct::new(15.0)));
        assert_eq!(calibrate(&[]), None);
        // Below 1% the ratio is too noisy to trust.
        assert_eq!(usd_per_point(Pct::new(0.5), Usd::new(10.0)), None);
    }

    /// Review B-H2: an account used mostly on claude.ai reads 40% with $0.004 spent here. That
    /// gives no rate, and $2 more leaves the estimate at the reading; a rate that is too low
    /// still never takes it past 100%.
    #[test]
    fn little_visible_spend_never_makes_the_estimate_explode() {
        let own = own_ratio(&[(Pct::new(40.0), Usd::new(0.004))]);
        assert_eq!(own, None);
        assert_eq!(
            estimate_now(Pct::new(40.0), Usd::new(2.0), own),
            Pct::new(40.0)
        );
        let tiny = Some(UsdPerPct::new(0.0001));
        assert_eq!(
            estimate_now(Pct::new(40.0), Usd::new(2.0), tiny),
            Pct::new(100.0)
        );
        // A reading past 100% (extra usage) stays what it is.
        assert_eq!(
            estimate_now(Pct::new(104.0), Usd::new(2.0), tiny),
            Pct::new(104.0)
        );
        for spent in [0.0, 0.5, 3.0, 1e9] {
            for r in [1e-12, 0.01, 5.0, 1e12] {
                let e = estimate_now(Pct::new(30.0), Usd::new(spent), Some(UsdPerPct::new(r)));
                assert!(e.get().is_finite() && e >= Pct::new(30.0) && e <= Pct::new(100.0));
            }
        }
    }

    #[test]
    fn manual_reset_moves_the_window_start() {
        let t = |h: i64| {
            DateTime::parse_from_rfc3339("2026-11-20T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
                + Duration::hours(h)
        };
        let r = |h: i64, u: f64| Reading {
            kind: WindowKind::SevenDay,
            utilization: u,
            resets_at: t(100),
            fetched_at: t(h),
        };
        let hist = [r(10, 40.0), r(20, 70.0), r(22, 3.0), r(30, 12.0)];
        let (start, reset) = effective_start(&hist, &hist[3]);
        assert_eq!(start, t(20), "the reading before the fall");
        assert_eq!(reset, Some(t(20)));
        // Before the reset the nominal window applies.
        assert_eq!(
            effective_start(&hist, &hist[1]),
            (t(100) - Duration::days(7), None)
        );
        // A normal roll-over is not a reset.
        let next = Reading {
            kind: WindowKind::SevenDay,
            utilization: 1.0,
            resets_at: t(268),
            fetched_at: t(101),
        };
        let (s2, r2) = effective_start(&[hist[3], next], &next);
        assert_eq!((s2, r2), (t(100), None));
    }

    #[test]
    fn eta_by_hand() {
        // 70% now, 3 points per hour → 10 hours.
        assert_eq!(eta(pct(70.0), pace(3.0)), Some(Duration::hours(10)));
        assert_eq!(eta(pct(100.0), pace(0.0)), Some(Duration::zero()));
        assert_eq!(eta(pct(50.0), pace(0.0)), None);
        // Review B-L1: paces this small used to build a duration past chrono's range.
        for p in [f64::MIN_POSITIVE, 1e-8, 2e-8, 1e-6] {
            assert!(eta(pct(50.0), pace(p)).is_none_or(|d| d > Duration::days(365)));
            let (now, end) = (ts("2026-11-20T12:00:00Z"), ts("2026-11-21T00:00:00Z"));
            assert_eq!(runs_out(pct(50.0), Some(pace(p)), now, end), None);
        }
    }

    #[test]
    fn levels_at_their_boundaries() {
        assert_eq!(Level::of(0.0), Level::Ok);
        assert_eq!(Level::of(59.99), Level::Ok);
        assert_eq!(Level::of(60.0), Level::Warn);
        assert_eq!(Level::of(84.99), Level::Warn);
        assert_eq!(Level::of(85.0), Level::Bad);
        assert_eq!(Level::of(99.99), Level::Bad);
        assert_eq!(Level::of(100.0), Level::Reached);
        assert_eq!(
            Level::of(130.0),
            Level::Reached,
            "extra usage goes past 100"
        );
        assert!(Level::Reached > Level::Bad && Level::Bad > Level::Warn && Level::Warn > Level::Ok);
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn current_window_rolls_forward() {
        // A weekly reading that resets on Mon 11-23 00:00: its window is 11-16 → 11-23.
        let r = Reading {
            kind: WindowKind::SevenDay,
            utilization: 40.0,
            resets_at: ts("2026-11-23T00:00:00Z"),
            fetched_at: ts("2026-11-20T00:00:00Z"),
        };
        let w = current_window(&r, ts("2026-11-22T12:00:00Z"));
        assert_eq!(
            (w.start, w.end),
            (ts("2026-11-16T00:00:00Z"), ts("2026-11-23T00:00:00Z"))
        );
        // At the reset itself the next window starts (end is exclusive).
        let w = current_window(&r, ts("2026-11-23T00:00:00Z"));
        assert_eq!(
            (w.start, w.end),
            (ts("2026-11-23T00:00:00Z"), ts("2026-11-30T00:00:00Z"))
        );
        // Two windows later: 11-30 → 12-07.
        let w = current_window(&r, ts("2026-12-01T09:00:00Z"));
        assert_eq!(
            (w.start, w.end),
            (ts("2026-11-30T00:00:00Z"), ts("2026-12-07T00:00:00Z"))
        );
        // A 5-hour window rolls by 5 hours: reset 10:00, now 16:30 → 15:00 → 20:00.
        let five = Reading {
            kind: WindowKind::FiveHour,
            resets_at: ts("2026-11-20T10:00:00Z"),
            ..r
        };
        let w = current_window(&five, ts("2026-11-20T16:30:00Z"));
        assert_eq!(
            (w.start, w.end),
            (ts("2026-11-20T15:00:00Z"), ts("2026-11-20T20:00:00Z"))
        );
    }

    #[test]
    fn reset_is_a_start_moving_forward() {
        let span = |a: &str, b: &str| Span {
            start: ts(a),
            end: ts(b),
        };
        let week = span("2026-11-16T00:00:00Z", "2026-11-23T00:00:00Z");
        assert!(!was_reset(week, week));
        // Rolled over.
        assert!(was_reset(
            week,
            span("2026-11-23T00:00:00Z", "2026-11-30T00:00:00Z")
        ));
        // Reset by hand on 11-19 06:00: same end, later start.
        assert!(was_reset(
            week,
            span("2026-11-19T06:00:00Z", "2026-11-23T00:00:00Z")
        ));
    }

    #[test]
    fn runs_out_and_projection_by_hand() {
        let now = ts("2026-11-20T12:00:00Z");
        let end = ts("2026-11-21T00:00:00Z"); // 12 h left
        // 70% at 2 points/h: runs out in 15 h, after the reset → no runs-out; at the reset
        // 70 + 2 × 12 = 94%.
        assert_eq!(runs_out(pct(70.0), Some(pace(2.0)), now, end), None);
        assert_eq!(
            projection_at_reset(pct(70.0), Some(pace(2.0)), now, end),
            Some(pct(94.0))
        );
        // 70% at 5 points/h: (100 − 70) / 5 = 6 h → before the reset; projection 70 + 60 = 130.
        assert_eq!(
            runs_out(pct(70.0), Some(pace(5.0)), now, end),
            Some(Duration::hours(6))
        );
        assert_eq!(
            projection_at_reset(pct(70.0), Some(pace(5.0)), now, end),
            Some(pct(130.0))
        );
        // No pace: no projection, and runs out only when already reached.
        assert_eq!(
            projection_at_reset(pct(70.0), Some(pace(0.0)), now, end),
            None
        );
        assert_eq!(projection_at_reset(pct(70.0), None, now, end), None);
        assert_eq!(runs_out(pct(70.0), None, now, end), None);
        assert_eq!(
            runs_out(pct(100.0), Some(pace(0.0)), now, end),
            Some(Duration::zero())
        );
    }

    // ── estimating one window ──

    fn hour(h: i64) -> DateTime<Utc> {
        ts("2026-11-20T00:00:00Z") + Duration::hours(h)
    }

    /// A weekly reading that resets at hour 100.
    fn wk(h: i64, used: f64) -> Reading {
        Reading {
            kind: WindowKind::SevenDay,
            utilization: used,
            resets_at: hour(100),
            fetched_at: hour(h),
        }
    }

    #[test]
    fn reference_wins_only_beyond_twenty_percent() {
        // No own ratio: any reference is better than nothing.
        assert!(reference_wins(None, UsdPerPct::new(0.3)));
        // Own $1.00 per point: a reference has to exceed $1.20.
        assert!(reference_wins(
            Some(UsdPerPct::new(1.0)),
            UsdPerPct::new(1.3)
        ));
        assert!(
            !reference_wins(Some(UsdPerPct::new(1.0)), UsdPerPct::new(1.2)),
            "exactly 20% more is noise"
        );
        assert!(!reference_wins(
            Some(UsdPerPct::new(1.0)),
            UsdPerPct::new(1.1)
        ));
        assert!(!reference_wins(
            Some(UsdPerPct::new(1.0)),
            UsdPerPct::new(0.5)
        ));
    }

    #[test]
    fn window_of_a_reading() {
        let hist = [wk(10, 40.0), wk(20, 70.0), wk(22, 3.0)];
        // Inside the window, after a reset by hand between hours 20 and 22: counts from 20.
        let p = window_of(&hist[2], &hist, hour(50));
        assert_eq!(p.window.start, hour(20));
        assert_eq!(p.window.end, hour(100));
        assert_eq!(p.manual_reset, Some(hour(20)));
        assert!(!p.reset_passed);
        assert_eq!(counting_from(&hist[2], &p), (3.0, hour(22)));
        // After the reset at hour 100: the next window, counting from zero at its start.
        let p = window_of(&hist[2], &hist, hour(120));
        assert!(p.reset_passed);
        assert_eq!(p.manual_reset, None);
        assert_eq!((p.window.start, p.window.end), (hour(100), hour(268)));
        assert_eq!(counting_from(&hist[2], &p), (0.0, hour(100)));
    }

    #[test]
    fn calibration_points_and_own_ratio_by_hand() {
        // Stored 10.0, 10.3, 11.0; the current 12.0 is not stored yet. Newest first: 12.0, 11.0,
        // 10.3; 10.0 is within half a point of 10.3 and is skipped. Each span runs from the
        // window start (hour 100 − 168 = hour −68) to its reading.
        let hist = [wk(1, 10.0), wk(2, 10.3), wk(3, 11.0)];
        let pts = calibration_points(&hist, &wk(4, 12.0));
        let start = hour(-68);
        let want: Vec<(Span, f64)> = [(4, 12.0), (3, 11.0), (2, 10.3)]
            .iter()
            .map(|&(h, u)| {
                (
                    Span {
                        start,
                        end: hour(h),
                    },
                    u,
                )
            })
            .collect();
        assert_eq!(pts, want);
        // At most 12 readings: 20 readings one point apart give the 12 newest.
        let many: Vec<Reading> = (1..=20).map(|h| wk(h, h as f64)).collect();
        let pts = calibration_points(&many, &many[19]);
        assert_eq!(pts.len(), 12);
        assert_eq!(pts[11].1, 9.0);
        // 10% after $4 → $0.40/pt; 30% after $14 → $14/30; 0.5% is too little to trust.
        // Median of the two: (12/30 + 14/30) / 2 = 13/30.
        let r = own_ratio(&[
            (Pct::new(10.0), Usd::new(4.0)),
            (Pct::new(30.0), Usd::new(14.0)),
            (Pct::new(0.5), Usd::new(10.0)),
        ])
        .unwrap();
        assert!((r.get() - 13.0 / 30.0).abs() < 1e-12);
        assert_eq!(own_ratio(&[]), None);
    }

    #[test]
    fn curve_step_keeps_about_170_points() {
        let span = |h: i64| Span {
            start: hour(0),
            end: hour(h),
        };
        // A week: 10 080 min / 168 = 60 min. Five hours: 300 / 168 = 1 min.
        assert_eq!(curve_step(span(168)), Duration::minutes(60));
        assert_eq!(curve_step(span(5)), Duration::minutes(1));
    }

    fn spend(dir: &str, usd: f64) -> ProjectSpend {
        ProjectSpend {
            project: ProjectKey::from_stored(format!("/w/{dir}")),
            name: dir.into(),
            cost: Usd::new(usd),
        }
    }

    /// 30% read at hour 10; the window ends at hour 100 and now is hour 76 (24 h left).
    fn inputs() -> WindowInputs {
        let r = wk(10, 30.0);
        WindowInputs {
            reading: r,
            place: window_of(&r, &[r], hour(76)),
            own_usd_per_point: Some(UsdPerPct::new(0.5)),
            spent_since: Usd::new(5.0),
            cost_in_window: Usd::new(15.0),
            cost_burn: Usd::new(2.0),
            by_project: vec![spend("b", 5.0), spend("a", 10.0)],
            series: [5.0, 0.0, 10.0].map(Usd::new).to_vec(),
            series_step: Duration::hours(1),
        }
    }

    /// Review B-M13: a login parked weeks ago, calibrated at $40 a point, no longer sets the
    /// tier's rate for an active account at $20; within one window it still does.
    #[test]
    fn a_stale_calibration_is_no_reference() {
        let now = ts("2026-11-20T12:00:00Z");
        let cal = |account: &str, usd: f64, at: &str| Calibration {
            account: account.into(),
            tier: PlanTier::of(account, "max"),
            kind: WindowKind::SevenDay,
            usd_per_point: UsdPerPct::new(usd),
            at: ts(at),
        };
        let parked = cal("parked", 40.0, "2026-10-25T00:00:00Z");
        let active = cal("active", 20.0, "2026-11-20T10:00:00Z");
        let refs = pick_references(&[parked.clone(), active.clone()], now);
        assert_eq!(
            refs[&(PlanTier::of("", "max"), WindowKind::SevenDay)].account,
            "active"
        );
        let recent = cal("recent", 30.0, "2026-11-15T00:00:00Z");
        let refs = pick_references(&[parked, active, recent], now);
        assert_eq!(
            refs[&(PlanTier::of("", "max"), WindowKind::SevenDay)].account,
            "recent"
        );
    }

    /// Review B-M12: a Codex `team` plan and Claude Team are two tiers.
    #[test]
    fn tiers_of_two_harnesses_never_share_a_reference() {
        let now = ts("2026-11-20T12:00:00Z");
        let cal = |account: &str, usd: f64| Calibration {
            account: account.into(),
            tier: PlanTier::of(account, "team"),
            kind: WindowKind::SevenDay,
            usd_per_point: UsdPerPct::new(usd),
            at: now,
        };
        let refs = pick_references(&[cal("acc-1/org", 20.0), cal("cdx:u/ws", 90.0)], now);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs[&(PlanTier::of("acc-1/org", "team"), WindowKind::SevenDay)].account,
            "acc-1/org"
        );
    }

    fn pct(v: f64) -> Pct {
        Pct::new(v)
    }

    fn pace(v: f64) -> PctPerHour {
        PctPerHour::new(v)
    }

    fn near(a: impl Into<f64>, b: f64) -> bool {
        (a.into() - b).abs() < 1e-9
    }

    #[test]
    fn window_estimate_with_its_own_ratio() {
        let e = window_estimate(&inputs(), None, hour(76));
        // $0.50 per point: 30 + $5 / 0.5 = 40%.
        assert_eq!(e.usd_per_point, Some(UsdPerPct::new(0.5)));
        assert!(near(e.estimate_now, 40.0));
        // $15 in the window explain 30 points; the other 10 come from elsewhere.
        assert!(near(e.visible_points.unwrap(), 30.0));
        assert!(near(e.outside_points.unwrap(), 10.0));
        // $2 in the last hour = 4 points/h: (100 − 40) / 4 = 15 h, before the reset in 24 h.
        assert!(near(e.burn_usd_per_hour, 2.0));
        assert!(near(e.points_per_hour.unwrap(), 4.0));
        assert_eq!(e.eta, Some(Duration::hours(15)));
        assert_eq!(e.runs_out_at, Some(hour(76) + Duration::hours(15)));
        // The visible 30 points split $10 : $5 → a 20, b 10 (largest first).
        assert_eq!(e.shares[0].name, "a");
        assert_eq!(e.shares[0].project, ProjectKey::from_stored("/w/a"));
        assert!(near(e.shares[0].limit_points, 20.0));
        assert!(near(e.shares[1].limit_points, 10.0));
        // A full window ≈ 100 × $0.50 = $50.
        assert!(near(e.window_usd.unwrap(), 50.0));
        // Cumulative 5, 5, 15 scaled by 40/15: 13.33, 13.33, 40.
        assert!(near(e.curve[0], 40.0 / 3.0));
        assert!(near(e.curve[1], 40.0 / 3.0));
        assert!(near(e.curve[2], 40.0));
    }

    #[test]
    fn window_estimate_with_a_tier_reference() {
        // The reference $1.00 per point replaces the own $0.50: 30 + 5 / 1 = 35%; the $15 seen
        // here explain 15 points, 20 come from elsewhere; 2 points/h → (100 − 35) / 2 = 32.5 h,
        // after the reset in 24 h, so no runs-out time.
        let e = window_estimate(&inputs(), Some(UsdPerPct::new(1.0)), hour(76));
        assert_eq!(e.usd_per_point, Some(UsdPerPct::new(1.0)));
        assert!(near(e.estimate_now, 35.0));
        assert!(near(e.visible_points.unwrap(), 15.0));
        assert!(near(e.outside_points.unwrap(), 20.0));
        assert!(near(e.points_per_hour.unwrap(), 2.0));
        assert_eq!(e.eta, None);
        assert!(near(e.window_usd.unwrap(), 100.0));
    }

    #[test]
    fn window_estimate_after_the_reset() {
        // Rolled over at hour 100; now hour 120. Counting from 0 at hour 100: $6 at $2/pt = 3%.
        let r = wk(10, 30.0);
        let i = WindowInputs {
            place: window_of(&r, &[r], hour(120)),
            own_usd_per_point: Some(UsdPerPct::new(2.0)),
            spent_since: Usd::new(6.0),
            cost_in_window: Usd::new(6.0),
            series: Vec::new(),
            ..inputs()
        };
        let e = window_estimate(&i, None, hour(120));
        assert!(near(e.estimate_now, 3.0));
        assert!(near(e.visible_points.unwrap(), 3.0));
        assert!(near(e.outside_points.unwrap(), 0.0));
        assert!(e.curve.is_empty());
    }

    #[test]
    fn window_estimate_with_nothing_known() {
        // No ratio and nothing spent: the reading itself, nothing derived.
        let i = WindowInputs {
            own_usd_per_point: None,
            spent_since: Usd::new(0.0),
            cost_in_window: Usd::new(0.0),
            cost_burn: Usd::new(0.0),
            by_project: Vec::new(),
            series: Vec::new(),
            ..inputs()
        };
        let e = window_estimate(&i, None, hour(76));
        assert!(near(e.estimate_now, 30.0));
        assert_eq!(
            (e.usd_per_point, e.visible_points, e.outside_points),
            (None, None, None)
        );
        assert_eq!((e.points_per_hour, e.eta, e.window_usd), (None, None, None));
        assert!(e.shares.is_empty() && e.curve.is_empty());
        assert!(near(e.burn_usd_per_hour, 0.0));
    }

    /// A weekly reading fetched at `fetched` (UTC, on 2026-11-22 unless the text has a date)
    /// whose window resets at `resets`.
    fn seen(fetched: &str, used: f64, resets: &str) -> Reading {
        let full = |t: &str| {
            if t.contains('T') {
                ts(&format!("{t}Z"))
            } else {
                ts(&format!("2026-11-22T{t}Z"))
            }
        };
        Reading {
            kind: WindowKind::SevenDay,
            utilization: used,
            resets_at: full(resets),
            fetched_at: full(fetched),
        }
    }

    #[test]
    fn readings_of_one_window_agree_despite_jitter_in_the_reset_time() {
        // `.claude.json` and the status line report the same reset a fraction of a second apart.
        let a = seen("10:00:00", 40.0, "2026-11-25T14:59:59.56");
        let b = seen("10:05:00", 41.0, "2026-11-25T15:00:00.13");
        assert!(a.same_window(&b));
        assert_eq!(a.window(), b.window());
        assert_eq!(a.window_end(), ts("2026-11-25T15:00:00Z"));
        // So a later reading of the same window is no reset (the dashboard's notice).
        assert!(!was_reset(a.window(), b.window()));
        let now = ts("2026-11-22T12:00:00Z");
        assert!(!was_reset(
            window_of(&a, &[a], now).window,
            window_of(&b, &[a, b], now).window
        ));
    }

    #[test]
    fn a_reset_by_hand_seen_through_lagging_sources() {
        // Seen on real data (numbers kept, times in UTC): the weekly limit reached 95% at 20:44,
        // was reset by hand, read 0% at 21:04, then climbed 1, 2, … 13% (status-line readings
        // every few minutes). Meanwhile another source of the same account kept reporting the
        // 2% it had fetched earlier, every ten seconds, between the status line's 4% and 5%;
        // resets differ by sub-second jitter between sources. The fall from 5% to that stale 2%
        // at 21:55 was taken as the reset ("reset by hand ≈ 21:55").
        let r = |f: &str, u: f64| seen(f, u, "2026-11-25T17:00:00");
        let mut hist = vec![
            seen("2026-11-20T15:44:49", 63.0, "2026-11-25T17:00:00.314"),
            seen("13:59:47", 65.0, "2026-11-25T16:59:59.983"),
            seen("16:12:33", 65.0, "2026-11-25T17:00:00.491"),
            seen("17:05:16", 66.0, "2026-11-25T16:59:59.596"),
            seen("18:07:01", 75.0, "2026-11-25T17:00:00.304"),
            seen("18:45:53", 82.0, "2026-11-25T16:59:59.936"),
            seen("20:44:58", 95.0, "2026-11-25T17:00:00.137"),
            seen("21:04:04", 0.0, "2026-11-25T16:59:59.562"),
            seen("21:05:31", 0.0, "2026-11-25T17:00:00.485"),
            r("21:11:14", 1.0),
            r("21:15:49", 2.0),
            r("21:27:43", 3.0),
        ];
        // 21:47–21:55: the lagging 2% every ten seconds, the status line's 4% now and then,
        // two readings of one moment at 21:47:23, then 5% at 21:55:24 and the stale 2% after it.
        for m in 47..=55 {
            for sec in [3, 13, 23, 33, 43, 53] {
                hist.push(r(&format!("21:{m}:{sec:02}"), 2.0));
            }
        }
        for f in ["21:47:06", "21:47:23", "21:48:13", "21:49:46", "21:54:52"] {
            hist.push(r(f, 4.0));
        }
        hist.push(r("21:55:24", 5.0));
        for (f, u) in [
            ("22:00:25", 5.0),
            ("22:11:28", 6.0),
            ("22:18:25", 7.0),
            ("22:27:12", 8.0),
            ("22:42:06", 10.0),
            ("23:00:28", 12.0),
            ("23:10:33", 13.0),
        ] {
            hist.push(r(f, u));
        }
        hist.sort_by_key(|h| h.fetched_at);
        let last_high = ts("2026-11-22T20:44:58Z");
        let first_low = ts("2026-11-22T21:04:04Z");
        // With every reading so far, and at each moment since the reset: the reset lies between
        // the last high and the first low reading (the high one's time is the start).
        let cur = *hist.last().unwrap();
        assert_eq!(effective_start(&hist, &cur), (last_high, Some(last_high)));
        for h in hist.iter().filter(|h| h.fetched_at >= first_low) {
            let (start, reset) = effective_start(&hist, h);
            assert_eq!(
                (start, reset),
                (last_high, Some(last_high)),
                "at {}",
                h.fetched_at
            );
            assert!(start < first_low);
        }
        // Before the reset: the nominal window.
        let before = hist.iter().find(|h| h.fetched_at == last_high).unwrap();
        assert_eq!(effective_start(&hist, before).0, ts("2026-11-18T17:00:00Z"));
    }

    #[test]
    fn a_fall_that_does_not_hold_is_no_reset_and_a_dense_one_is() {
        let r = |f: &str, u: f64| seen(f, u, "2026-11-25T17:00:00");
        // A real reset with readings a minute apart: 95% at 21:03, 0% at 21:04.
        let hist = [
            r("21:00:00", 94.0),
            r("21:03:00", 95.0),
            r("21:04:00", 0.0),
            r("21:10:00", 1.0),
        ];
        let t = ts("2026-11-22T21:03:00Z");
        assert_eq!(effective_start(&hist, &hist[3]), (t, Some(t)));
        // After the reset, a lagging source repeats the old 95%: the reset stays where it was.
        let mut late = hist.to_vec();
        late.push(r("21:12:00", 95.0));
        late.push(r("21:14:00", 2.0));
        assert_eq!(effective_start(&late, &late[5]), (t, Some(t)));
        // A source lagging behind a rising account (30% → 40%, then the stale 30%, then 41%):
        // no reset, both when the stale reading is the newest and after the next one.
        let rising = [
            r("10:00:00", 30.0),
            r("10:05:00", 40.0),
            r("10:06:00", 30.0),
            r("10:09:00", 41.0),
        ];
        let nominal = ts("2026-11-18T17:00:00Z");
        assert_eq!(effective_start(&rising, &rising[2]), (nominal, None));
        assert_eq!(effective_start(&rising, &rising[3]), (nominal, None));
        // A fall undone right after (the low source first seen after the high one).
        let undone = [
            r("10:00:00", 50.0),
            r("10:01:00", 10.0),
            r("10:04:00", 50.0),
        ];
        assert_eq!(effective_start(&undone, &undone[2]), (nominal, None));
    }
}

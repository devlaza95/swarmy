//! Units of the domain as zero-cost newtypes (first review M6, second review M9): dollars, points
//! of a limit, and the rate between them. Mixing them up is a compile error: `Usd + Pct` does not
//! exist, and converting spend into points needs a [`UsdPerPct`] by name. Money is an estimate
//! shown with ≈, never a ledger, so `f64` inside is fine; the value is unit safety.
//!
//! ```compile_fail
//! use swarmy::core::units::{Pct, Usd};
//! let _ = Usd::new(1.0) + Pct::new(1.0);
//! ```

use std::ops::{Add, AddAssign, Div, Mul, Sub};

use serde::{Deserialize, Serialize};

/// API-equivalent US dollars.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct Usd(f64);

/// Points of a limit's utilization, in percent (0..=100, more with extra usage).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct Pct(f64);

/// Dollars of API-equivalent spend per point of a limit: how spend turns into utilization.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct UsdPerPct(f64);

/// A pace of utilization: points of a limit per hour. Its own type, so a percent and a pace
/// cannot change places (review B-M12: `eta(current, points_per_hour)` took two `f64`).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct PctPerHour(f64);

macro_rules! scalar {
    ($t:ident) => {
        impl $t {
            pub const ZERO: $t = $t(0.0);

            pub const fn new(v: f64) -> $t {
                $t(v)
            }

            /// The number, for display and storage.
            pub const fn get(self) -> f64 {
                self.0
            }

            pub fn max(self, o: $t) -> $t {
                $t(self.0.max(o.0))
            }

            pub fn min(self, o: $t) -> $t {
                $t(self.0.min(o.0))
            }

            pub fn total_cmp(&self, o: &$t) -> std::cmp::Ordering {
                self.0.total_cmp(&o.0)
            }
        }

        /// The bare number, with the formatter's precision (`{:.1}`).
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }

        /// For display (`ui::fmt::money`, `ui::fmt::pct`) and storage.
        impl From<$t> for f64 {
            fn from(x: $t) -> f64 {
                x.0
            }
        }

        impl Add for $t {
            type Output = $t;
            fn add(self, o: $t) -> $t {
                $t(self.0 + o.0)
            }
        }

        impl Sub for $t {
            type Output = $t;
            fn sub(self, o: $t) -> $t {
                $t(self.0 - o.0)
            }
        }

        impl AddAssign for $t {
            fn add_assign(&mut self, o: $t) {
                self.0 += o.0;
            }
        }

        impl Mul<f64> for $t {
            type Output = $t;
            fn mul(self, k: f64) -> $t {
                $t(self.0 * k)
            }
        }

        impl Div<f64> for $t {
            type Output = $t;
            fn div(self, k: f64) -> $t {
                $t(self.0 / k)
            }
        }

        /// The ratio of two amounts of the same unit.
        impl Div for $t {
            type Output = f64;
            fn div(self, o: $t) -> f64 {
                self.0 / o.0
            }
        }

        impl std::iter::Sum for $t {
            fn sum<I: Iterator<Item = $t>>(it: I) -> $t {
                $t(it.map(|x| x.0).sum())
            }
        }

        impl<'a> std::iter::Sum<&'a $t> for $t {
            fn sum<I: Iterator<Item = &'a $t>>(it: I) -> $t {
                $t(it.map(|x| x.0).sum())
            }
        }
    };
}

scalar!(Usd);
scalar!(Pct);

impl UsdPerPct {
    pub const fn new(v: f64) -> UsdPerPct {
        UsdPerPct(v)
    }

    pub const fn get(self) -> f64 {
        self.0
    }

    /// Visible spend below which one reading says nothing about the rate: an account used
    /// mostly elsewhere (claude.ai) reads 40% with cents spent here, and a rate of cents per
    /// point turned the next dollars into thousands of percent (review B-H2).
    pub const MIN_CALIBRATION_SPEND: Usd = Usd::new(1.0);

    /// The rate from spend that moved the limit by `points` (`None` below one point or below
    /// [`UsdPerPct::MIN_CALIBRATION_SPEND`]: too little to tell).
    pub fn measured(spend: Usd, points: Pct) -> Option<UsdPerPct> {
        (points.0 >= 1.0 && spend.0 >= Self::MIN_CALIBRATION_SPEND.0)
            .then(|| UsdPerPct(spend.0 / points.0))
    }

    /// Whether it can convert at all (finite and above zero).
    pub fn is_usable(self) -> bool {
        self.0.is_finite() && self.0 > 0.0
    }
}

impl PctPerHour {
    pub const fn new(v: f64) -> PctPerHour {
        PctPerHour(v)
    }

    pub const fn get(self) -> f64 {
        self.0
    }

    /// The pace of a burn rate, converted at `rate`.
    pub fn of_burn(usd_per_hour: Usd, rate: UsdPerPct) -> PctPerHour {
        PctPerHour(usd_per_hour.0 / rate.0)
    }

    /// Hours until `points` more are used at this pace (`None` at no pace to speak of).
    pub fn hours_for(self, points: Pct) -> Option<f64> {
        (self.0.is_finite() && self.0 > 1e-9).then(|| points.0.max(0.0) / self.0)
    }

    /// Points used over `hours` at this pace.
    pub fn over(self, hours: f64) -> Pct {
        Pct(self.0 * hours)
    }
}

impl std::fmt::Display for PctPerHour {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl From<PctPerHour> for f64 {
    fn from(v: PctPerHour) -> f64 {
        v.0
    }
}

impl std::fmt::Display for UsdPerPct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// For display (`ui::fmt::money`).
impl From<UsdPerPct> for f64 {
    fn from(x: UsdPerPct) -> f64 {
        x.0
    }
}

impl Mul<f64> for UsdPerPct {
    type Output = UsdPerPct;
    fn mul(self, k: f64) -> UsdPerPct {
        UsdPerPct(self.0 * k)
    }
}

/// Spend at a rate is points of the limit.
impl Div<UsdPerPct> for Usd {
    type Output = Pct;
    fn div(self, r: UsdPerPct) -> Pct {
        Pct(self.0 / r.0)
    }
}

/// Points of the limit at a rate are spend.
impl Mul<UsdPerPct> for Pct {
    type Output = Usd;
    fn mul(self, r: UsdPerPct) -> Usd {
        Usd(self.0 * r.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_go_through_the_rate() {
        // $30 moved the limit by 10 points: $3 a point; $6 more is 2 points; a full window
        // (100 points) is $300.
        let r = UsdPerPct::measured(Usd::new(30.0), Pct::new(10.0)).unwrap();
        assert_eq!(r.get(), 3.0);
        assert_eq!(Usd::new(6.0) / r, Pct::new(2.0));
        assert_eq!(Pct::new(100.0) * r, Usd::new(300.0));
        assert_eq!(UsdPerPct::measured(Usd::new(30.0), Pct::new(0.5)), None);
        assert_eq!(UsdPerPct::measured(Usd::ZERO, Pct::new(10.0)), None);
        assert_eq!(UsdPerPct::measured(Usd::new(0.004), Pct::new(40.0)), None);
        let total: Usd = [Usd::new(1.5), Usd::new(2.5)].iter().sum();
        assert_eq!(total, Usd::new(4.0));
        assert_eq!(Usd::new(1.0) / Usd::new(4.0), 0.25);
    }
}

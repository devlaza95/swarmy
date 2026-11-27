//! Data views every front end shares: what `swarmy report` and `swarmy limits` show, computed
//! once here (masking included) so a command, the web view and any future front end cannot
//! disagree about a number or leak a name the others hide.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::context::Ctx;
use crate::core::ids::ProjectKey;
use crate::core::period::Period;
use crate::index::Index;
use crate::index::query::{Dim, Filter, Row};
use crate::limits::{AccountReport, all_reports};

#[derive(Debug, Serialize)]
pub struct PeriodInfo {
    pub label: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// Spend grouped by one dimension over one period. Serialises to the JSON `swarmy report
/// --format json` prints.
#[derive(Debug, Serialize)]
pub struct SpendReport {
    pub period: PeriodInfo,
    pub by: Dim,
    pub rows: Vec<Row>,
    pub total: Row,
}

/// Masks what looks like a path in the rows of a path-keyed grouping: a project key or
/// checkout's dir, a branch key (`<dir> ⎇ <branch>`), and a label that fell back to a cwd.
/// `show` is `Ctx::show_path` (masked, it keeps the last component).
pub fn mask_paths(dim: Dim, rows: &mut [Row], show: impl Fn(&str) -> String) {
    if !matches!(dim, Dim::Project | Dim::Worktree | Dim::Branch) {
        return;
    }
    let path_like = |s: &str| {
        s.starts_with(['/', '~', '\\']) || s.as_bytes().get(1..3) == Some(b":\\".as_slice())
    };
    let mask = |s: &str| {
        if let Some((dir, branch)) = s.split_once(" ⎇ ").filter(|(d, _)| path_like(d)) {
            format!("{} ⎇ {branch}", show(dir))
        } else if path_like(s) {
            show(s)
        } else {
            s.to_string()
        }
    };
    for r in rows {
        r.key = mask(&r.key);
        r.label = mask(&r.label);
    }
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

impl Ctx {
    /// Every account's limit analysis, with names and paths masked when `--mask` is on. The
    /// analysis itself is unmasked (`crate::limits`); what is shown goes through here.
    pub fn limit_reports(&self, idx: &Index) -> anyhow::Result<Vec<AccountReport>> {
        let mut reports = all_reports(&self.limits_inputs(idx)?, idx)?;
        if self.mask {
            // Shares name projects by their dir: the last component only.
            for r in &mut reports {
                r.name = self.shown_account_name(r.account.as_str());
                for w in &mut r.windows {
                    w.reference = w
                        .reference_id
                        .as_deref()
                        .map(|a| self.shown_account_name(a));
                    for s in &mut w.shares {
                        s.project = ProjectKey::from_stored(self.show_path(s.project.as_str()));
                        s.name = self.show_path(&s.name);
                    }
                }
            }
        }
        Ok(reports)
    }

    /// Spend by `by` over `period` for what `filter` lets through: labels resolved (accounts,
    /// weekdays), paths masked when `--mask` is on.
    pub fn spend_report(
        &self,
        idx: &Index,
        by: Dim,
        period: &Period,
        filter: &Filter,
    ) -> anyhow::Result<SpendReport> {
        let span = period.span(self.now, self.limit_week(idx, filter.account.as_ref())?);
        let mut rows = idx.spend_by(by, span, filter)?;
        let total = idx.total(span, filter)?;
        if by == Dim::Account {
            for r in &mut rows {
                r.label = self.shown_account_name(&r.key);
            }
        }
        if self.mask {
            mask_paths(by, &mut rows, |p| self.show_path(p));
        }
        if by == Dim::Weekday {
            for r in &mut rows {
                r.label = r
                    .key
                    .parse::<usize>()
                    .ok()
                    .and_then(|d| WEEKDAYS.get(d))
                    .unwrap_or(&"?")
                    .to_string();
            }
        }
        Ok(SpendReport {
            period: PeriodInfo {
                label: period.label(),
                start: span.start,
                end: span.end,
            },
            by,
            rows,
            total,
        })
    }
}

//! PROJECTS: spend per project in the last hour, today and in the period (▲▼ vs the previous
//! one); Enter filters SWARM to its agents.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{dim, faint, prev_label, trend_w};
use crate::core::limits::WindowKind;
use crate::live::State;
use crate::tui::app::{App, Panel};
use crate::tui::view::kit::*;
use crate::ui::fmt;

pub fn projects(f: &mut Frame, app: &mut App, area: Rect) {
    let period = app
        .analytics
        .period
        .as_ref()
        .map(|p| p.label())
        .unwrap_or_default();
    let inner = frame(
        f,
        app,
        Panel::Projects,
        area,
        &if period == "all time" {
            format!("{period} · Enter shows its agents")
        } else {
            format!(
                "{period} · ▲▼ vs {} · Enter shows its agents",
                prev_label(&period)
            )
        },
        false,
    );
    let app: &App = app;
    let th = app.theme;
    let width = inner.width as usize;
    let (live_w, hour_w, today_w, per_w, share_w, pts_w) =
        (5usize, 9usize, 10usize, 10usize, 6usize, 10usize);
    let name_w = 18usize.min(width / 4).max(10);
    // The share of a project's spend made in linked git worktrees, once any was.
    let wt_w = if app.analytics.projects.iter().any(|p| p.in_worktrees > 0.0) {
        7usize
    } else {
        0
    };
    // Which harnesses worked on each project (`cc+agy`), with more than one.
    let har_of = |p: &crate::tui::ProjectRow| -> String {
        p.by_harness
            .iter()
            .filter_map(|(k, _)| crate::core::harness::Harness::from_key(k))
            .map(|h| h.short())
            .collect::<Vec<_>>()
            .join("+")
    };
    let har_w = app
        .analytics
        .projects
        .iter()
        .map(|p| w(&har_of(p)))
        .max()
        .filter(|n| *n > 0)
        .map_or(0, |n| n.max(3) + 1);
    let fixed = name_w + har_w + live_w + hour_w + today_w + per_w + 6 + share_w + wt_w + pts_w + 7;
    let bar_w = width.saturating_sub(fixed).min(40);
    let total: f64 = app
        .analytics
        .projects
        .iter()
        .map(|p| p.period)
        .sum::<f64>()
        .max(1e-9);
    let max = app
        .analytics
        .projects
        .iter()
        .map(|p| p.period)
        .fold(0.0, f64::max)
        .max(1e-9);
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "{}{} {} {} {} {} {} {}{} {}",
            fit("PROJECT", name_w),
            if har_w > 0 {
                fit(" HAR", har_w)
            } else {
                String::new()
            },
            fit_right("LIVE", live_w),
            fit_right("$/HOUR", hour_w),
            fit_right("TODAY", today_w),
            fit_right(&format!("{}      ", period.to_uppercase()), per_w + 6),
            fit("", bar_w),
            fit_right("SHARE", share_w),
            if wt_w > 0 {
                format!(" {}", fit_right("IN WT", wt_w - 1))
            } else {
                String::new()
            },
            fit_right("% OF WEEK", pts_w)
        ),
        faint(app),
    ))];
    let cur = app.cur(Panel::Projects);
    let focused = app.focus == Panel::Projects;
    let rows = inner.height.saturating_sub(1) as usize;
    let offset = if cur >= rows { cur + 1 - rows } else { 0 };
    for (i, p) in app
        .analytics
        .projects
        .iter()
        .enumerate()
        .skip(offset)
        .take(rows)
    {
        let live = app
            .snapshot
            .agents
            .iter()
            .filter(|a| {
                a.project_key.as_ref() == Some(&p.key)
                    && (a.state.is_active() || matches!(a.state, State::NeedsYou(_)))
            })
            .count();
        let pts: f64 = app
            .analytics
            .limits
            .iter()
            .filter_map(|r| {
                r.windows
                    .iter()
                    .find(|w| w.reading.kind == WindowKind::SevenDay)
            })
            .flat_map(|w| {
                w.shares
                    .iter()
                    .filter(|s| s.project == p.key)
                    .map(|s| s.limit_points.get())
            })
            .sum();
        let name = if app.mask {
            crate::ui::table::truncate(&p.name, 12)
        } else {
            p.name.clone()
        };
        let mut sp = vec![Span::styled(
            fit(&name, name_w),
            th.bold(if p.hour > 0.0 { th.fg } else { th.dim }),
        )];
        if har_w > 0 {
            // A fact about where the work ran, not a model: plain text, no colour.
            sp.push(Span::styled(
                format!(" {}", fit(&har_of(p), har_w - 1)),
                Style::new().fg(th.dim),
            ));
        }
        sp.extend([
            Span::styled(
                format!(
                    " {}",
                    fit_right(
                        &if live > 0 {
                            format!("●{live}")
                        } else {
                            "·".into()
                        },
                        live_w
                    )
                ),
                Style::new().fg(if live > 0 { th.good } else { th.faint }),
            ),
            Span::styled(
                format!(
                    " {}",
                    fit_right(
                        &if p.hour > 0.0 {
                            fmt::usd(p.hour)
                        } else {
                            "·".into()
                        },
                        hour_w
                    )
                ),
                Style::new().fg(if p.hour > 0.0 { th.accent } else { th.faint }),
            ),
            Span::styled(
                format!(" {}", fit_right(&fmt::usd(p.today), today_w)),
                Style::new().fg(th.fg),
            ),
            Span::styled(
                format!(" {}", fit_right(&fmt::usd(p.period), per_w)),
                th.bold(th.fg),
            ),
            trend_w(app, p.period, p.prev, 6),
            Span::raw(" "),
        ]);
        sp.extend(gauge(p.period / max, bar_w, th.fade(th.accent, 0.25), &th));
        sp.push(Span::styled(
            format!(
                " {}",
                fit_right(&fmt::pct(p.period / total * 100.0), share_w)
            ),
            dim(app),
        ));
        if wt_w > 0 {
            let wt = if p.in_worktrees > 0.0 && p.period > 0.0 {
                fmt::pct(p.in_worktrees / p.period * 100.0)
            } else {
                "·".into()
            };
            sp.push(Span::styled(
                format!(" {}", fit_right(&wt, wt_w - 1)),
                Style::new().fg(if p.in_worktrees > 0.0 {
                    th.dim
                } else {
                    th.faint
                }),
            ));
        }
        sp.push(Span::styled(
            format!(
                " {}",
                fit_right(
                    &if pts > 0.05 {
                        format!("≈{pts:.1}%")
                    } else {
                        String::new()
                    },
                    pts_w
                )
            ),
            dim(app),
        ));
        let mut line = Line::from(sp);
        if focused && i == cur {
            line = line.style(Style::new().bg(th.sel));
        }
        lines.push(line);
    }
    if app.analytics.projects.is_empty() {
        lines.push(Line::from(Span::styled(
            "no spend in this period",
            dim(app),
        )));
    }
    put(f, inner, lines);
}

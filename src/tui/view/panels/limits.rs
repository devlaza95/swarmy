//! LIMITS · SEATS: every seat and saved login with its limits, where the week went, and the
//! limit chart.

use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CLine};

use super::{dim, faint};
use crate::core::harness::Harness;
use crate::core::limits::{Level, WindowKind};
use crate::core::units::Pct;
use crate::tui::app::{App, LimitsRow, Panel};
use crate::tui::view::kit::*;
use crate::ui::fmt;

pub fn limits_seats(f: &mut Frame, app: &mut App, area: Rect, detail: bool) {
    let hot = app
        .alerts
        .get("vault")
        .is_some_and(|t| t.elapsed().as_secs_f32() < 2.0)
        && blink(app);
    let n_acc = app.analytics.limits.len();
    let inner = frame(
        f,
        app,
        Panel::Limits,
        area,
        &format!("{n_acc} accounts · Enter switch · S best"),
        hot,
    );
    let app: &App = app;
    let th = app.theme;
    let rows = app.limits_rows();
    let width = inner.width as usize;
    // Narrow panels drop the account column (the seat and login usually say enough), and the
    // seat's dir unless the panel is wide.
    let acct_w = if inner.width < 120 { 0usize } else { 18 };
    let dir_w = if inner.width >= 150 { 22usize } else { 0 };
    let (mark_w, name_w, login_w, reset_w) = (2usize, 16usize, 17usize, 9usize);
    let fixed = mark_w
        + name_w
        + if dir_w > 0 { dir_w + 1 } else { 0 }
        + 1
        + login_w
        + if acct_w > 0 { acct_w + 1 } else { 0 }
        + 5
        + 1
        + 5
        + 1
        + reset_w
        + 1;
    let info_min = 22;
    let has_five = app.analytics.limits.iter().any(|r| {
        r.windows
            .iter()
            .any(|w| w.reading.kind == WindowKind::FiveHour)
    });
    let fixed = if has_five { fixed } else { fixed - 6 };
    let gauges = if has_five { 2 } else { 1 };
    let g = ((width.saturating_sub(fixed + info_min)) / gauges).clamp(6, 24);
    let info_w = width.saturating_sub(fixed + gauges * g);
    // Seats and saved logins are two tables per harness, each under its own column names: a
    // seat is where the harness runs (a config dir), a saved login is what the vault keeps.
    let head_w = name_w + if dir_w > 0 { dir_w + 1 } else { 0 };
    let header = |first: &str, dir: &str, second: &str, quota: bool| {
        let first = if dir_w > 0 && !dir.is_empty() {
            format!("{} {}", fit(first, name_w), fit(dir, dir_w))
        } else {
            first.to_string()
        };
        let tail = if quota {
            format!(
                "{}{} {} STATUS",
                if has_five {
                    format!("{} ", fit("5-HOUR", g + 5))
                } else {
                    String::new()
                },
                fit("WEEK USED", g + 5),
                fit("RESETS IN", reset_w),
            )
        } else {
            "SPEND · PLAN".to_string()
        };
        Line::from(Span::styled(
            format!(
                "{}{} {} {}{}",
                " ".repeat(mark_w),
                fit(&first, head_w),
                fit(second, login_w),
                if acct_w > 0 {
                    format!("{} ", fit("ACCOUNT", acct_w))
                } else {
                    String::new()
                },
                tail
            ),
            faint(app),
        ))
    };
    let mut lines: Vec<Line> = Vec::new();
    let cur = app.cur(Panel::Limits);
    let focused = app.focus == Panel::Limits;
    let now = Utc::now();
    let sections = app.limits_sections();
    let mut section: Option<Harness> = None;
    let mut first_login = true;
    // Gauges and the reset column, where a harness without a local quota shows its spend.
    let quota_w = gauges * (g + 5) + usize::from(has_five) + 1 + reset_w;
    for (i, r) in rows.iter().enumerate() {
        let sel = focused && i == cur;
        let h = app.row_harness(r);
        if section != Some(h) {
            section = Some(h);
            first_login = true;
            if sections {
                lines.push(section_head(app, h, &rows));
            }
            if let LimitsRow::Seat(_) = r {
                lines.push(header("SEAT", "DIR", "LOGIN IN IT", h.supports_limits()));
            }
        }
        if let LimitsRow::Login(_) = r
            && first_login
        {
            first_login = false;
            lines.push(header("SAVED LOGIN", "", "IN SEATS", h.supports_limits()));
        }
        let mut sp: Vec<Span> = vec![Span::styled(
            if sel { "▸ " } else { "  " },
            th.bold(th.accent),
        )];
        let (account, quiet) = match r {
            LimitsRow::Seat(label) => {
                let s = app.analytics.seats.iter().find(|x| &x.label == label);
                let state = s.map(|x| x.state.clone()).unwrap_or_default();
                let login = s.and_then(|x| x.login.clone());
                sp.push(Span::styled(fit(label, name_w), th.bold(th.fg)));
                if dir_w > 0 {
                    sp.push(Span::styled(
                        format!(" {}", fit(&seat_dir(app, label), dir_w)),
                        Style::new().fg(th.fade(th.dir, 0.3)),
                    ));
                }
                sp.push(match (&login, &state.fingerprint) {
                    (Some(l), _) => Span::styled(
                        format!(" {}", fit(&format!("⚿ {}", mask_name(app, l)), login_w)),
                        Style::new().fg(th.good),
                    ),
                    (None, Some(_)) => Span::styled(
                        format!(" {}", fit("not saved · c", login_w)),
                        th.bold(th.warn),
                    ),
                    // Not readable here (Claude Code on macOS: the Keychain), not logged out;
                    // a harness whose logins the vault does not keep: nothing to say.
                    (None, None) if crate::vault::unavailable(h).is_some() => Span::styled(
                        format!(
                            " {}",
                            fit(
                                if crate::vault::in_keychain(h) {
                                    "in Keychain"
                                } else {
                                    "·"
                                },
                                login_w
                            )
                        ),
                        dim(app),
                    ),
                    (None, None) => {
                        Span::styled(format!(" {}", fit("(logged out)", login_w)), dim(app))
                    }
                });
                let acct = s.and_then(|x| x.account.clone());
                let name = match (&state.unverified_since, &acct) {
                    (Some(t), _) => {
                        format!("? new login {}", t.with_timezone(&Local).format("%d %b"))
                    }
                    (None, Some(a)) => mask_name(app, &account_name(app, a)),
                    (None, None) if state.fingerprint.is_some() => "(unknown)".into(),
                    _ => "(logged out)".into(),
                };
                if acct_w > 0 {
                    sp.push(Span::styled(
                        format!(" {}", fit(&name, acct_w)),
                        Style::new().fg(if state.unverified_since.is_some() {
                            th.warn
                        } else {
                            th.fg
                        }),
                    ));
                } else if state.unverified_since.is_some() {
                    sp.push(Span::styled("?", Style::new().fg(th.warn)));
                }
                (acct, false)
            }
            LimitsRow::Login(id) => {
                let l = app.vault.logins.iter().find(|x| x.id.as_str() == id);
                let label = l.map(|x| x.label.clone()).unwrap_or_default();
                // The seats that hold it, or free.
                let seats: Vec<String> = l
                    .map(|x| {
                        x.mounted
                            .iter()
                            .filter_map(|id| app.analytics.seats.iter().find(|s| &s.id == id))
                            .map(|s| s.label.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                let (where_, wc) = if seats.is_empty() {
                    ("free".to_string(), th.faint)
                } else {
                    (seats.join(", "), th.good)
                };
                sp.push(Span::styled(
                    fit(&format!("⚿ {}", mask_name(app, &label)), head_w),
                    th.bold(th.fg),
                ));
                sp.push(Span::styled(
                    format!(" {}", fit(&where_, login_w)),
                    Style::new().fg(wc),
                ));
                let acct = l.and_then(|x| x.account_uuid.clone());
                let name = acct
                    .as_deref()
                    .map(|a| account_name(app, a))
                    .or_else(|| l.and_then(|x| x.email.clone()))
                    .unwrap_or_else(|| "(account unknown)".into());
                if acct_w > 0 {
                    sp.push(Span::styled(
                        format!(" {}", fit(&mask_name(app, &name), acct_w)),
                        dim(app),
                    ));
                }
                (acct, true)
            }
        };
        let (five, week, reset) = app.limits_of(account.as_deref());
        if h.supports_limits() {
            let shown: Vec<Option<f64>> = if has_five {
                vec![five, week]
            } else {
                vec![week]
            };
            for v in shown {
                sp.push(Span::raw(" "));
                match v {
                    Some(p) => {
                        let c = th.utilization(p);
                        let c = if quiet {
                            th.fade(c, 0.35)
                        } else if Level::of(p) >= Level::Bad {
                            pulse(app, c, 1.0)
                        } else {
                            c
                        };
                        sp.extend(gauge(p / 100.0, g, c, &th));
                        sp.push(Span::styled(fit_right(&fmt::pct(p), 5), th.bold(c)));
                    }
                    None => sp.push(Span::styled(fit("  unknown", g + 5), faint(app))),
                }
            }
            sp.push(Span::styled(
                format!(
                    " {}",
                    fit(
                        &reset.map(|r| fmt::duration(r - now)).unwrap_or_default(),
                        reset_w
                    )
                ),
                dim(app),
            ));
        } else {
            // No local quota: what the seat spent instead (a login: its plan).
            let text = match r {
                LimitsRow::Seat(label) => app
                    .analytics
                    .seats
                    .iter()
                    .find(|x| &x.label == label)
                    .map(|x| {
                        format!(
                            "today {} · 7d {} · {} req today",
                            fmt::usd(x.today_cost),
                            fmt::usd(x.week_cost),
                            fmt::count(x.today_requests)
                        )
                    })
                    .unwrap_or_default(),
                LimitsRow::Login(id) => app
                    .vault
                    .logins
                    .iter()
                    .find(|x| x.id.as_str() == id)
                    .and_then(|x| x.plan.clone().or_else(|| x.meta.subscription_type.clone()))
                    .unwrap_or_default(),
            };
            sp.push(Span::styled(
                format!(" {}", fit(&text, quota_w.saturating_sub(1))),
                if quiet { faint(app) } else { dim(app) },
            ));
        }
        // Status: what to do or expect.
        let report = account.as_deref().and_then(|a| {
            app.analytics
                .limits
                .iter()
                .find(|x| x.account.as_str() == a)
        });
        let weekly = report.and_then(|r| {
            r.windows
                .iter()
                .find(|w| w.reading.kind == WindowKind::SevenDay)
        });
        let status: (String, Color) = match r {
            LimitsRow::Seat(label) => {
                let live = app
                    .analytics
                    .seats
                    .iter()
                    .find(|x| &x.label == label)
                    .map(|x| x.live)
                    .unwrap_or(0);
                let eta = weekly.and_then(|w| {
                    if w.level() == Level::Reached {
                        Some("limit reached".to_string())
                    } else {
                        w.eta.map(|d| {
                            format!(
                                "runs out in {} ({})",
                                fmt::duration(d),
                                (now + d).with_timezone(&Local).format("%a %H:%M")
                            )
                        })
                    }
                });
                let burn = weekly
                    .map(|w| format!("{}/h", fmt::usd(w.burn_usd_per_hour)))
                    .unwrap_or_default();
                let live_s = if live > 0 {
                    format!("●{live} live")
                } else {
                    String::new()
                };
                // Whole parts only: drop the rate before cutting a number in half.
                let mut parts: Vec<String> = Vec::new();
                for p in [eta.clone().unwrap_or_default(), live_s, burn] {
                    let used: usize = parts.iter().map(|x| w(x) + 3).sum();
                    if !p.is_empty() && used + w(&p) < info_w {
                        parts.push(p);
                    }
                }
                let colour = if eta.is_some() {
                    th.warn
                } else if live > 0 && !h.supports_limits() {
                    th.good
                } else {
                    th.dim
                };
                (parts.join(" · "), colour)
            }
            LimitsRow::Login(id) => {
                let l = app.vault.logins.iter().find(|x| x.id.as_str() == id);
                let read = weekly
                    .map(|w| {
                        format!(
                            "limit read {} ago",
                            fmt::duration(now - w.reading.fetched_at)
                        )
                    })
                    .unwrap_or_default();
                match l.and_then(|x| x.meta.refresh_expires_at) {
                    Some(t) if (t - now).num_days() < 7 => (
                        format!("login expires in {} · {read}", fmt::duration(t - now)),
                        th.warn,
                    ),
                    Some(t) => (
                        format!("login valid {} · {read}", fmt::duration(t - now)),
                        th.dim,
                    ),
                    None => (read, th.dim),
                }
            }
        };
        sp.push(Span::styled(
            format!(" {}", fit(&status.0, info_w)),
            Style::new().fg(status.1),
        ));
        let mut line = Line::from(sp);
        if sel {
            line = line.style(Style::new().bg(th.sel));
        }
        lines.push(line);
        // After a harness's seats, where its logins cannot be kept here: why, instead of an
        // empty saved-logins table.
        let next_is_seat_here = rows
            .get(i + 1)
            .is_some_and(|n| matches!(n, LimitsRow::Seat(_)) && app.row_harness(n) == h);
        if matches!(r, LimitsRow::Seat(_))
            && !next_is_seat_here
            && let Some(e) = crate::vault::unavailable(h)
        {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    fit(&format!("saved logins: {e}"), width.saturating_sub(2))
                ),
                faint(app),
            )));
        }
    }
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", fit(no_rows_hint(app), width.saturating_sub(2))),
            dim(app),
        )));
    }
    lines.extend(hidden_note(app, width));
    if focused && (lines.len() as u16) < inner.height {
        lines.push(Line::from(Span::styled(
            "  Enter: seat → pick a saved login · saved login → pick seats · S best · c save the seat's login · a add · r rename · d forget",
            faint(app),
        )));
    }
    // Below the rows: the week as a curve (left) and where it went (right), when there is room.
    let used = lines.len() as u16;
    put(f, inner, lines);
    let rest = Rect {
        y: inner.y + used + 1,
        height: inner.height.saturating_sub(used + 1),
        ..inner
    };
    if rest.height >= 5 {
        let (chart, side) = if rest.width >= 110 {
            let c = hsplit(rest, &[58, 42]);
            (c[0], Some(c[1]))
        } else {
            (rest, None)
        };
        limit_curve(f, app, chart);
        if let Some(side) = side {
            let side = Rect {
                x: side.x + 2,
                width: side.width.saturating_sub(2),
                ..side
            };
            put(
                f,
                side,
                limit_shares(app, side.width as usize, side.height as usize, detail),
            );
        }
    }
}

/// What an empty panel says: where seats come from.
fn no_rows_hint(app: &App) -> &'static str {
    if app.hidden_harnesses().is_empty() {
        "no seats found — run claude, agy or codex once, or add [[seat]] to config.toml"
    } else {
        "no seats in use — run claude, agy or codex once, or add [[seat]] to config.toml"
    }
}

/// The line under the rows that names the harnesses left out because all their seats are empty
/// (`App::hidden_harnesses`), and the key that shows them.
fn hidden_note(app: &App, width: usize) -> Option<Line<'static>> {
    let hidden = app.hidden_harnesses();
    if hidden.is_empty() {
        return None;
    }
    let (lead, key) = if app.show_hidden {
        ("shown", ". hides")
    } else {
        ("hidden", ". shows")
    };
    let text = |dirs: bool| {
        let parts: Vec<String> = hidden
            .iter()
            .map(|(h, seats)| {
                let n = seats.len();
                let mut p = format!(
                    "{} — {n} empty seat{}",
                    h.display_name(),
                    if n == 1 { "" } else { "s" }
                );
                if dirs {
                    let d: Vec<String> = seats.iter().map(|s| seat_dir(app, &s.label)).collect();
                    p.push_str(&format!(" ({})", d.join(", ")));
                }
                p
            })
            .collect();
        format!("{lead}: {} · {key}", parts.join(" · "))
    };
    let room = width.saturating_sub(2);
    let full = text(true);
    let shown = if w(&full) <= room { full } else { text(false) };
    Some(Line::from(Span::styled(
        format!("  {}", fit(&shown, room)),
        dim(app),
    )))
}

/// A harness's section heading: its name on a plate, then what it holds.
fn section_head(app: &App, h: Harness, rows: &[LimitsRow]) -> Line<'static> {
    let th = app.theme;
    let of = |want_seat: bool| {
        rows.iter()
            .filter(|r| app.row_harness(r) == h && matches!(r, LimitsRow::Seat(_)) == want_seat)
            .count()
    };
    let (seats, logins) = (of(true), of(false));
    let mut what = format!(
        " · {seats} seat{} · {logins} saved login{}",
        if seats == 1 { "" } else { "s" },
        if logins == 1 { "" } else { "s" }
    );
    if !h.supports_limits() {
        what.push_str(" · no local quota: spend shown");
    }
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!(" {} ", h.display_name().to_uppercase()),
            th.heading(),
        ),
        Span::styled(what, faint(app)),
    ])
}

/// How an account is named on screen: the limit report's name (Anthropic accounts), else the
/// email an agy account is keyed by, else the email a seat's login names (Codex), else a short
/// id.
fn account_name(app: &App, id: &str) -> String {
    if app
        .analytics
        .limits
        .iter()
        .any(|r| r.account.as_str() == id)
    {
        return app.account_label(id);
    }
    if let Some(email) = id.strip_prefix("agy:") {
        return email.to_string();
    }
    app.analytics
        .seats
        .iter()
        .find(|s| s.account.as_deref() == Some(id))
        .and_then(|s| s.email.clone())
        .unwrap_or_else(|| app.account_label(id))
}

/// Where each account's week went: projects with their share of the used limit.
fn limit_shares(app: &App, width: usize, height: usize, _detail: bool) -> Vec<Line<'static>> {
    let th = app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, rep) in app.analytics.limits.iter().enumerate() {
        let Some(win) = rep
            .windows
            .iter()
            .find(|w| w.reading.kind == WindowKind::SevenDay)
        else {
            continue;
        };
        if lines.len() + 2 > height {
            break;
        }
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let mut head = vec![
            Span::styled("■ ", Style::new().fg(account_color(&th, i))),
            Span::styled(mask_name(app, &rep.name), th.bold(th.fg)),
            Span::styled(
                format!(
                    " · week used {} · 1% ≈ {}",
                    fmt::pct(win.estimate_now),
                    win.usd_per_point
                        .map(fmt::money)
                        .unwrap_or_else(|| "?".into())
                ),
                dim(app),
            ),
        ];
        if let Some(t) = win.manual_reset {
            head.push(Span::styled(
                format!(
                    " · reset by hand ≈{}",
                    t.with_timezone(&Local).format("%a %H:%M")
                ),
                Style::new().fg(th.accent),
            ));
        }
        if let Some(o) = win.outside_points.filter(|o| *o >= Pct::new(2.0)) {
            head.push(Span::styled(
                format!(" · ≈{o:.0}% used elsewhere (claude.ai, other machines)"),
                Style::new().fg(th.warn),
            ));
        }
        lines.push(Line::from(head));
        let max = win.shares.first().map(|s| s.share).unwrap_or(0.0).max(1e-9);
        let name_w = 16usize.min(width / 4);
        let bw = width.saturating_sub(name_w + 30).clamp(6, 40);
        for s in win.shares.iter() {
            if lines.len() >= height {
                break;
            }
            let mut sp = vec![Span::styled(
                format!("  {} ", fit(&s.name, name_w)),
                Style::new().fg(th.fg),
            )];
            sp.extend(gauge(
                s.share / max,
                bw,
                th.fade(account_color(&th, i), 0.3),
                &th,
            ));
            sp.push(Span::styled(
                format!(" {:>8} ≈{:.1}% of week", fmt::usd(s.cost), s.limit_points),
                dim(app),
            ));
            lines.push(Line::from(sp));
        }
    }
    lines
}

/// One account in the limit chart: colour, the curve, the readings, the projection
/// (from x, from y, to x, to y).
type CurveSeries = (
    Color,
    Vec<(f64, f64)>,
    Vec<(f64, f64)>,
    Option<(f64, f64, f64, f64)>,
);

/// Colours of accounts in the limit chart and its legend.
pub fn account_color(th: &crate::tui::theme::Theme, i: usize) -> Color {
    [th.accent, Color::Rgb(255, 121, 198), th.good, th.fable][i % 4]
}

/// The limit window as a curve per account: estimated utilization from the window start to now
/// (solid), the readings Claude Code fetched (dots), the projection to the reset at the current
/// pace (dotted), the 100% line and "now".
fn limit_curve(f: &mut Frame, app: &App, area: Rect) {
    use chrono::Duration;
    use ratatui::widgets::canvas::Points;
    let th = app.theme;
    let now = Utc::now();
    let wins: Vec<(usize, &crate::limits::WindowReport)> = app
        .analytics
        .limits
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            r.windows
                .iter()
                .find(|w| w.reading.kind == WindowKind::SevenDay && !w.curve.is_empty())
                .map(|w| (i, w))
        })
        .collect();
    if wins.is_empty() || area.height < 4 || area.width < 30 {
        return;
    }
    let [chart, legend] = [
        Rect {
            height: area.height - 1,
            ..area
        },
        Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        },
    ];
    let x0 = wins
        .iter()
        .map(|(_, w)| w.window.start)
        .min()
        .unwrap_or(now);
    let x1 = wins.iter().map(|(_, w)| w.window.end).max().unwrap_or(now);
    let hours = |t: chrono::DateTime<Utc>| (t - x0).num_seconds() as f64 / 3600.0;
    let span = hours(x1).max(1.0);
    // Projections: the current pace until the reset.
    let proj: Vec<Option<(f64, f64)>> = wins
        .iter()
        .map(|(_, w)| {
            w.projection_at_reset(now)
                .map(|at_reset| (hours(w.window.end), at_reset.get()))
        })
        .collect();
    let peak = wins
        .iter()
        .map(|(_, w)| w.estimate_now.get())
        .chain(proj.iter().flatten().map(|p| p.1.min(100.0)))
        .fold(100.0f64, f64::max)
        .min(400.0);
    let ymax = peak * 1.08;
    let (bad, faint_c, track) = (th.bad, th.faint, th.track);
    let series: Vec<CurveSeries> = wins
        .iter()
        .zip(&proj)
        .map(|((i, w), p)| {
            let step = w.curve_step.num_seconds() as f64 / 3600.0;
            let start = hours(w.window.start);
            let mut pts = vec![(start, 0.0)];
            pts.extend(
                w.curve
                    .iter()
                    .enumerate()
                    .map(|(k, v)| ((start + (k as f64 + 1.0) * step).min(hours(now)), v.get())),
            );
            let marks = w.marks.iter().map(|(t, u)| (hours(*t), *u)).collect();
            let pr = p.map(|(xe, ye)| (hours(now), w.estimate_now.get(), xe, ye));
            (account_color(&th, *i), pts, marks, pr)
        })
        .collect();
    let xn = hours(now);
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, span])
        .y_bounds([0.0, ymax])
        .paint(move |ctx| {
            // Day boundaries, then 100% and now.
            let mut d = (x0 + Duration::days(1)).with_timezone(&Local).date_naive();
            while let Some(t) = d
                .and_hms_opt(0, 0, 0)
                .and_then(|t| t.and_local_timezone(Local).single())
            {
                let x = hours(t.with_timezone(&Utc));
                if x >= span {
                    break;
                }
                ctx.draw(&CLine::new(x, 0.0, x, ymax * 0.04, track));
                ctx.print(
                    x,
                    0.0,
                    Line::styled(d.format(" %a").to_string(), Style::new().fg(faint_c)),
                );
                d = d.succ_opt().unwrap_or(d);
            }
            let mut x = 0.0;
            while x < span {
                ctx.draw(&CLine::new(
                    x,
                    100.0,
                    (x + span / 120.0).min(span),
                    100.0,
                    bad,
                ));
                x += span / 60.0;
            }
            ctx.draw(&CLine::new(xn, 0.0, xn, ymax, faint_c));
            for (k, (c, pts, marks, pr)) in series.iter().enumerate() {
                let k = &k;
                for w in pts.windows(2) {
                    ctx.draw(&CLine::new(w[0].0, w[0].1, w[1].0, w[1].1, *c));
                }
                if let Some((xa, ya, xe, ye)) = pr.filter(|p| p.1 < 100.0) {
                    // Dotted, and only up to the 100% line: where it crosses is the news.
                    let stop = if ye > 100.0 {
                        (100.0 - ya) / (ye - ya)
                    } else {
                        1.0
                    };
                    let n = 40;
                    for k in (0..n).step_by(2) {
                        let (t0, t1) = (k as f64 / n as f64, ((k + 1) as f64 / n as f64).min(stop));
                        if t0 >= stop {
                            break;
                        }
                        ctx.draw(&CLine::new(
                            xa + (xe - xa) * t0,
                            ya + (ye - ya) * t0,
                            xa + (xe - xa) * t1,
                            ya + (ye - ya) * t1,
                            *c,
                        ));
                    }
                    if stop < 1.0 {
                        let xc = xa + (xe - xa) * stop;
                        let t = x0 + Duration::seconds((xc * 3600.0) as i64);
                        ctx.print(
                            xc,
                            100.0 - ymax * 0.09 * (*k as f64 + 1.0),
                            Line::styled(
                                format!("⊘{}", t.with_timezone(&Local).format("%a %H:%M")),
                                Style::new().fg(*c),
                            ),
                        );
                    }
                }
                ctx.draw(&Points {
                    coords: marks,
                    color: Color::White,
                });
            }
        });
    f.render_widget(canvas, chart);
    // Legend: per account where it is and where it is heading.
    let mut spans = vec![
        Span::styled("100% ", Style::new().fg(th.bad)),
        Span::styled("╌ ", Style::new().fg(th.bad)),
    ];
    for ((i, w), p) in wins.iter().zip(&proj) {
        let name = mask_name(app, &app.analytics.limits[*i].name);
        let ahead = match (w.eta, p) {
            _ if w.level() == Level::Reached => " · limit reached".to_string(),
            (Some(d), _) => format!(
                " → 100% {}",
                (now + d).with_timezone(&Local).format("%a %H:%M")
            ),
            (None, Some((_, ye))) => format!(" → ≈{} at reset", fmt::pct(*ye)),
            _ => String::new(),
        };
        spans.push(Span::styled("■ ", Style::new().fg(account_color(&th, *i))));
        spans.push(Span::styled(
            format!("{name} {}{ahead}   ", fmt::pct(w.estimate_now)),
            dim(app),
        ));
    }
    let range = format!(
        "{} … {} · dots: readings",
        x0.with_timezone(&Local).format("%a %d"),
        x1.with_timezone(&Local).format("%a %d")
    );
    let used: usize = spans.iter().map(|s| w(&s.content)).sum();
    if used + w(&range) <= legend.width as usize {
        spans.push(Span::styled(range, faint(app)));
    }
    put(f, legend, vec![Line::from(spans)]);
}

/// A seat's dir as shown: `~/…`, only its name when masking.
fn seat_dir(app: &App, label: &str) -> String {
    let Some(s) = app.seats.iter().find(|s| s.label == label) else {
        return String::new();
    };
    let name = s
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if app.mask {
        return name;
    }
    // The demo's made-up seats live in its data dir; shown where real ones would be.
    if s.found_by.contains(&"demo") {
        return format!("~/{name}");
    }
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    match home.and_then(|h| s.dir.strip_prefix(h).ok().map(|r| r.to_path_buf())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => s.dir.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::seats::Seat;
    use crate::tui::SeatRow;
    use crate::tui::theme::Theme;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    /// A seat of `h` in a made-up home.
    fn seat(h: Harness, label: &str, dir: &str) -> Seat {
        let dir = PathBuf::from("/home/alice").join(dir);
        match h {
            Harness::Claude => Seat::new(label, dir.clone(), dir.join(".claude.json"), false),
            Harness::Agy => Seat::agy(label, dir),
            Harness::Codex => Seat::codex(label, dir),
        }
    }

    /// What the analytics pass says of a seat: `empty` as `core::seat_use` judged it.
    fn row(s: &Seat, empty: bool) -> SeatRow {
        SeatRow {
            id: s.id.clone(),
            label: s.label.clone(),
            harness: s.harness,
            account: (!empty).then(|| "cdx:user/ws".to_string()),
            login: None,
            email: None,
            live: usize::from(!empty),
            today_cost: 0.0,
            today_requests: 0,
            week_cost: 0.0,
            state: Default::default(),
            empty,
        }
    }

    fn login(h: Harness, id: &str) -> crate::vault::Login {
        serde_json::from_value(serde_json::json!({
            "id": id, "label": id, "harness": h, "account_uuid": null, "email": null, "plan": null,
            "meta": {"fingerprint": id, "subscription_type": null, "rate_limit_tier": null,
                     "access_expires_at": null, "refresh_expires_at": null, "scopes": []},
            "oauth_account": null, "captured_at": "2026-11-01T00:00:00Z",
            "updated_at": "2026-11-01T00:00:00Z", "mounted": []
        }))
        .unwrap()
    }

    /// A Codex-only machine: `~/.claude` found (a marker file) but empty, `~/.codex` in use.
    fn codex_only(t: &tempfile::TempDir) -> App {
        let claude = seat(Harness::Claude, "default", ".claude");
        let codex = seat(Harness::Codex, "codex", ".codex");
        let mut app = App::new(
            Theme::named(None),
            false,
            true,
            vec![claude.clone(), codex.clone()],
            t.path().into(),
        );
        app.boot = None;
        app.analytics.seats = vec![row(&claude, true), row(&codex, false)];
        app
    }

    fn draw(app: &mut App, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| limits_seats(f, app, f.area(), false))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    #[test]
    fn a_codex_only_machine_shows_one_harness_and_names_the_hidden_seat() {
        let t = tempfile::tempdir().unwrap();
        let mut app = codex_only(&t);
        assert_eq!(app.limits_rows(), [LimitsRow::Seat("codex".into())]);
        assert!(!app.limits_sections(), "a single-harness panel");
        let text = draw(&mut app, 200, 20);
        assert!(!text.contains("CLAUDE CODE"), "{text}");
        assert!(!text.contains("default"), "{text}");
        assert!(text.contains("codex"), "{text}");
        // The dir as shown (outside the real home: in full, with the OS's separator).
        let dir = PathBuf::from("/home/alice").join(".claude");
        assert!(
            text.contains(&format!(
                "hidden: Claude Code — 1 empty seat ({}) · . shows",
                dir.display()
            )),
            "{text}"
        );
        // `.` shows it, with its section; `.` again hides it.
        press(&mut app, '.');
        assert_eq!(
            app.limits_rows(),
            [
                LimitsRow::Seat("default".into()),
                LimitsRow::Seat("codex".into())
            ]
        );
        assert!(app.limits_sections());
        let text = draw(&mut app, 200, 20);
        assert!(
            text.contains("CLAUDE CODE") && text.contains("default"),
            "{text}"
        );
        assert!(text.contains("shown: Claude Code — 1 empty seat"), "{text}");
        assert!(text.contains(". hides"), "{text}");
        press(&mut app, '.');
        assert_eq!(app.limits_rows(), [LimitsRow::Seat("codex".into())]);
    }

    #[test]
    fn a_masked_hidden_line_names_the_dir_only() {
        let t = tempfile::tempdir().unwrap();
        let mut app = codex_only(&t);
        app.mask = true;
        let text = draw(&mut app, 200, 20);
        assert!(text.contains("1 empty seat (.claude)"), "{text}");
        assert!(!text.contains("alice"), "{text}");
    }

    #[test]
    fn a_saved_login_of_the_harness_keeps_its_empty_seat_shown() {
        let t = tempfile::tempdir().unwrap();
        let mut app = codex_only(&t);
        app.vault.logins = vec![login(Harness::Claude, "spare")];
        assert_eq!(
            app.limits_rows(),
            [
                LimitsRow::Seat("default".into()),
                LimitsRow::Login("spare".into()),
                LimitsRow::Seat("codex".into())
            ]
        );
        let text = draw(&mut app, 200, 20);
        assert!(text.contains("CLAUDE CODE"), "{text}");
        assert!(!text.contains("hidden:"), "{text}");
        // A Codex login keeps nothing of Claude Code shown.
        app.vault.logins = vec![login(Harness::Codex, "cx")];
        assert!(
            !app.limits_rows()
                .contains(&LimitsRow::Seat("default".into()))
        );
    }

    #[test]
    fn nothing_is_hidden_before_the_seats_are_judged() {
        let t = tempfile::tempdir().unwrap();
        let mut app = codex_only(&t);
        app.analytics.seats.clear();
        assert_eq!(app.limits_rows().len(), 2);
        assert!(app.hidden_harnesses().is_empty());
    }

    #[test]
    fn the_selection_stays_on_its_row_when_empty_seats_are_hidden() {
        let t = tempfile::tempdir().unwrap();
        let mut app = codex_only(&t);
        let judged = app.analytics.seats.clone();
        // Before the first pass both seats show; the user selects the Codex seat.
        app.analytics.seats.clear();
        app.focus = Panel::Limits;
        app.cursor.insert(Panel::Limits, 1);
        let a = crate::tui::Analytics {
            seats: judged,
            ..Default::default()
        };
        app.on_msg(crate::tui::Msg::Analytics(Box::new(a)));
        assert_eq!(app.limits_rows(), [LimitsRow::Seat("codex".into())]);
        assert_eq!(app.cur(Panel::Limits), 0, "still on the Codex seat");
        press(&mut app, '.');
        assert_eq!(
            app.cur(Panel::Limits),
            1,
            "the Codex seat, below the shown one"
        );
    }

    #[test]
    fn with_no_seat_at_all_the_panel_says_where_seats_come_from() {
        let t = tempfile::tempdir().unwrap();
        let mut app = App::new(Theme::named(None), false, true, Vec::new(), t.path().into());
        app.boot = None;
        let text = draw(&mut app, 200, 20);
        assert!(
            text.contains(
                "no seats found — run claude, agy or codex once, or add [[seat]] to config.toml"
            ),
            "{text}"
        );
        // Every seat empty: the hint, and what was left out.
        let claude = seat(Harness::Claude, "default", ".claude");
        let mut app = App::new(
            Theme::named(None),
            false,
            true,
            vec![claude.clone()],
            t.path().into(),
        );
        app.boot = None;
        app.analytics.seats = vec![row(&claude, true)];
        let text = draw(&mut app, 200, 20);
        assert!(text.contains("no seats in use"), "{text}");
        assert!(
            text.contains("hidden: Claude Code — 1 empty seat"),
            "{text}"
        );
    }
}

//! EFFICIENCY: where time and money go (tools, commands, waits, cache, agent types…).

use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{dim, faint};
use crate::core::limits::WindowKind;
use crate::core::tools::ToolClass;
use crate::tui::app::{App, Panel};
use crate::tui::view::kit::*;
use crate::ui::fmt;

/// `245h`, `3.6h`, `12m`, `40s`.
fn hrs(h: f64) -> String {
    if h >= 100.0 {
        format!("{h:.0}h")
    } else if h >= 1.0 {
        format!("{h:.1}h")
    } else if h * 60.0 >= 1.0 {
        format!("{:.0}m", h * 60.0)
    } else {
        format!("{:.0}s", h * 3600.0)
    }
}

fn section_head(app: &App, title: &str, note: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {title} "), app.theme.heading()),
        Span::styled(
            if note.is_empty() {
                note
            } else {
                format!(" · {note}")
            },
            faint(app),
        ),
    ])
}

fn time_widths(cw: usize) -> (usize, usize) {
    let name_w = 16.min(cw / 3);
    (name_w, cw.saturating_sub(name_w + 26).clamp(4, 30))
}

fn time_header(app: &App, first: &str, cw: usize) -> Line<'static> {
    let (name_w, bar_w) = time_widths(cw);
    Line::from(Span::styled(
        format!(
            "{} {} {:>6} {:>7} {:>5}",
            fit(first, name_w),
            " ".repeat(bar_w),
            "TIME",
            "CALLS",
            "ERR"
        ),
        faint(app),
    ))
}

/// Rows of `name bar value…` for tool-like stats.
fn time_rows(
    app: &App,
    rows: &[crate::index::insights::ToolStat],
    n: usize,
    cw: usize,
) -> Vec<Line<'static>> {
    let th = app.theme;
    let max = rows.iter().map(|t| t.hours).fold(0.0, f64::max).max(1e-9);
    let (name_w, bar_w) = time_widths(cw);
    rows.iter()
        .filter(|t| t.hours > 0.0 || t.calls > 0)
        .take(n)
        .map(|t| {
            let err = if t.calls > 0 {
                t.errors as f64 / t.calls as f64 * 100.0
            } else {
                0.0
            };
            let mut sp = vec![Span::styled(
                format!("{} ", fit(&t.name, name_w)),
                Style::new().fg(th.fg),
            )];
            sp.extend(gauge(t.hours / max, bar_w, th.accent, &th));
            sp.push(Span::styled(
                format!(" {:>6}", hrs(t.hours)),
                th.bold(th.fg),
            ));
            sp.push(Span::styled(
                format!(" {:>7}", fmt::count(t.calls)),
                dim(app),
            ));
            sp.push(Span::styled(
                format!(
                    " {:>5}",
                    if err >= 0.05 {
                        fmt::pct(err)
                    } else {
                        "0".to_string()
                    }
                ),
                Style::new().fg(if err >= 5.0 { th.bad } else { th.faint }),
            ));
            Line::from(sp)
        })
        .collect()
}

fn efficiency_sections(app: &App, cw: usize) -> Vec<Vec<Line<'static>>> {
    let th = app.theme;
    let Some(e) = &app.analytics.efficiency else {
        return vec![vec![Line::from(Span::styled("counting…", faint(app)))]];
    };
    let mut out = Vec::new();

    // Where agent time goes.
    let tools: Vec<_> = e
        .tools
        .iter()
        .filter(|t| !ToolClass::of(&t.name).waits_on_user())
        .cloned()
        .collect();
    let total_h: f64 = tools.iter().map(|t| t.hours).sum();
    let mut s = vec![section_head(
        app,
        "WHERE AGENT TIME GOES",
        format!("{} in tools", hrs(total_h)),
    )];
    s.push(time_header(app, "TOOL", cw));
    s.extend(time_rows(app, &tools, 7, cw));
    out.push(s);

    let mut s = vec![section_head(
        app,
        "SLOWEST COMMANDS",
        "Bash, by time".into(),
    )];
    s.push(time_header(app, "COMMAND", cw));
    s.extend(time_rows(app, &e.commands, 7, cw));
    out.push(s);

    // Waiting on the user, and what a prompt buys.
    let mut s = vec![section_head(app, "WAITING ON YOU", String::new())];
    for (label, (n, h)) in [("questions", e.questions), ("plan approvals", e.plans)] {
        if n == 0 {
            continue;
        }
        s.push(Line::from(vec![
            Span::styled(fit(label, 15), Style::new().fg(th.fg)),
            Span::styled(format!("{:>5}", fmt::count(n)), th.bold(th.fg)),
            Span::styled(format!(" · avg {:>4}", hrs(h / n as f64)), dim(app)),
            Span::styled(
                format!(" · {} blocked", hrs(h)),
                Style::new().fg(if h >= 1.0 { th.warn } else { th.dim }),
            ),
        ]));
    }
    if e.prompts > 0 {
        s.push(Line::from(vec![
            Span::styled(fit("your prompts", 15), Style::new().fg(th.fg)),
            Span::styled(format!("{:>5}", fmt::count(e.prompts)), th.bold(th.fg)),
            Span::styled(
                format!(" → {} +", fmt::usd(e.cost / e.prompts as f64)),
                dim(app),
            ),
            Span::styled(
                format!(" {} agent work each", hrs(e.agent_hours / e.prompts as f64)),
                dim(app),
            ),
        ]));
    }
    if s.len() == 1 {
        s.push(Line::from(Span::styled(
            "nothing waited for you",
            faint(app),
        )));
    }
    out.push(s);

    // Cache and what the money buys.
    let sp = e.split;
    let tot = sp.total().max(1e-9);
    let mut s = vec![section_head(
        app,
        "CACHE & TOKENS",
        format!(
            "hit {} · saved ≈{} (vs none)",
            fmt::pct(e.cache_hit * 100.0),
            fmt::usd(e.cache_saved)
        ),
    )];
    let mut parts = vec![
        ("input", sp.input, th.sonnet),
        ("cache write", sp.cache_write, th.warn),
        ("cache read", sp.cache_read, th.haiku),
        ("output", sp.output, th.fable),
    ];
    if sp.web > 0.0 {
        parts.push(("web search", sp.web, th.other));
    }
    let bw = cw.saturating_sub(1).max(10);
    let mut bar = Vec::new();
    let mut used = 0usize;
    for (k, (_, v, c)) in parts.iter().enumerate() {
        let n = if k == parts.len() - 1 {
            bw - used.min(bw)
        } else {
            ((v / tot) * bw as f64).round() as usize
        };
        let n = n.min(bw - used.min(bw));
        used += n;
        bar.push(Span::styled("█".repeat(n), Style::new().fg(*c)));
    }
    s.push(Line::from(bar));
    for chunk in parts.chunks(2) {
        let mut l = Vec::new();
        for (name, v, c) in chunk {
            l.push(Span::styled("■ ", Style::new().fg(*c)));
            l.push(Span::styled(
                fit(
                    &format!("{name} {} {}", fmt::usd(*v), fmt::pct(v / tot * 100.0)),
                    cw / 2 - 2,
                ),
                dim(app),
            ));
        }
        s.push(Line::from(l));
    }
    out.push(s);

    // Agent types.
    let mut s = vec![section_head(
        app,
        "AGENT TYPES",
        "cost · lifetime · unfinished".into(),
    )];
    let name_w = 16.min(cw / 3);
    s.push(Line::from(Span::styled(
        format!(
            "{} {:>5} {:>9} {:>8} {:>7} {:>5}",
            fit("TYPE", name_w),
            "N",
            "COST",
            "AVG $",
            "AVG T",
            "UNF"
        ),
        faint(app),
    )));
    for t in e.types.iter().take(7) {
        s.push(Line::from(vec![
            Span::styled(format!("{} ", fit(&t.name, name_w)), Style::new().fg(th.fg)),
            Span::styled(format!("{:>5}", fmt::count(t.agents)), dim(app)),
            Span::styled(format!(" {:>9}", fmt::usd(t.cost)), th.bold(th.fg)),
            Span::styled(
                format!(" {:>8}", fmt::usd(t.cost / t.agents.max(1) as f64)),
                dim(app),
            ),
            Span::styled(format!(" {:>7}", hrs(t.avg_minutes / 60.0)), dim(app)),
            Span::styled(
                format!(
                    " {:>5}",
                    if t.unfinished > 0 {
                        t.unfinished.to_string()
                    } else {
                        String::new()
                    }
                ),
                Style::new().fg(th.warn),
            ),
        ]));
    }
    out.push(s);

    // The fleet and its troubles.
    let peak = e.fleet.iter().copied().max().unwrap_or(0);
    let peak_at = e.fleet.iter().rposition(|n| *n == peak).unwrap_or(0);
    let peak_time = (Utc::now() - chrono::Duration::hours(24 - peak_at as i64))
        .with_timezone(&Local)
        .format("%H:00");
    let mut s = vec![section_head(
        app,
        "FLEET · 24 h",
        format!("peak {peak} agents at {peak_time}"),
    )];
    let vals: Vec<f64> = e.fleet.iter().map(|n| *n as f64).collect();
    let reps = (cw.saturating_sub(2) / vals.len().max(1)).clamp(1, 3);
    let wide: Vec<f64> = vals
        .iter()
        .flat_map(|v| std::iter::repeat_n(*v, reps))
        .collect();
    s.push(Line::from(spark(&wide, th.accent)));
    s.push(Line::from(Span::styled(
        fit(
            &format!("{:<w$}now", "−24 h", w = (wide.len()).saturating_sub(3)),
            cw,
        ),
        faint(app),
    )));
    let mut l = vec![
        Span::styled(
            format!("rate limits {}", e.rate_limits),
            Style::new().fg(if e.rate_limits > 0 { th.bad } else { th.dim }),
        ),
        Span::styled(format!(" · API errors {}", e.other_errors), dim(app)),
        Span::styled(format!(" · compactions {}", e.compactions), dim(app)),
    ];
    if e.rate_limits == 0 {
        l[0] = Span::styled("rate limits 0", dim(app));
    }
    s.push(Line::from(l));
    s.push(Line::from(vec![
        Span::styled(
            format!("files edited {}", fmt::count(e.files_edited)),
            dim(app),
        ),
        Span::styled(format!(" ({} edits)", fmt::count(e.edits)), faint(app)),
        Span::styled(
            format!(
                " · {} conflict{}",
                e.conflicts.len(),
                if e.conflicts.len() == 1 { "" } else { "s" }
            ),
            Style::new().fg(if e.conflicts.is_empty() {
                th.dim
            } else {
                th.warn
            }),
        ),
    ]));
    for (file, n) in e.conflicts.iter().take(3) {
        let name = if app.mask {
            "•••".to_string()
        } else {
            file.rsplit('/')
                .take(2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("/")
        };
        s.push(Line::from(Span::styled(
            format!(
                "  ⚠ {} · {n} agents in one hour",
                fit(&name, cw.saturating_sub(24))
            ),
            Style::new().fg(th.warn),
        )));
    }
    out.push(s);

    // What the subscription is worth, when its price is configured.
    if !app.analytics.subscriptions.is_empty() {
        let mut s = vec![section_head(
            app,
            "SUBSCRIPTION VALUE",
            "API-equivalent per week".into(),
        )];
        for rep in &app.analytics.limits {
            let Some(price) = app
                .analytics
                .subscriptions
                .iter()
                // A key may be a bare `accountUuid` (written before organizations counted).
                .find(|(k, _)| {
                    *k == rep.name || *k == rep.account.as_str() || *k == rep.account.user_uuid()
                })
                .map(|x| x.1)
            else {
                continue;
            };
            let Some(win) = rep
                .windows
                .iter()
                .find(|w| w.reading.kind == WindowKind::SevenDay)
            else {
                continue;
            };
            let week_price = price * 12.0 / 52.0;
            s.push(Line::from(vec![
                Span::styled(fit(&mask_name(app, &rep.name), 18), Style::new().fg(th.fg)),
                Span::styled(
                    format!(
                        " {} vs {}/wk",
                        fmt::usd(win.cost_in_window),
                        fmt::usd(week_price)
                    ),
                    dim(app),
                ),
                Span::styled(
                    format!("  ×{:.1}", win.cost_in_window.get() / week_price.max(1e-9)),
                    th.bold(th.good),
                ),
            ]));
        }
        if s.len() > 1 {
            out.push(s);
        }
    }
    out
}

pub fn efficiency(f: &mut Frame, app: &mut App, area: Rect) {
    let period = app
        .analytics
        .period
        .as_ref()
        .map(|p| p.label())
        .unwrap_or_default();
    // Lay the sections out in columns, page by page.
    let probe = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let ncols = ((probe.width as usize + 2) / 42).max(1);
    let cw = (probe.width as usize / ncols).saturating_sub(2).max(20);
    let height = probe.height as usize;
    let sections = efficiency_sections(app, cw);
    let ncols = ncols.min(sections.len()).max(1);
    // Each section goes to the shortest column it fits in; a new page when none has room.
    let mut pages: Vec<Vec<Vec<Line<'static>>>> = vec![vec![Vec::new(); ncols]];
    for mut sec in sections {
        sec.truncate(height);
        let Some(page) = pages.last_mut() else {
            break;
        };
        let fits = |c: &Vec<Line>| c.len() + sec.len() + usize::from(!c.is_empty()) <= height;
        match (0..ncols)
            .filter(|i| fits(&page[*i]))
            .min_by_key(|i| page[*i].len())
        {
            Some(i) => {
                if !page[i].is_empty() {
                    page[i].push(Line::from(""));
                }
                page[i].extend(sec);
            }
            None => {
                let mut p = vec![Vec::new(); ncols];
                p[0] = sec;
                pages.push(p);
            }
        }
    }
    let n = pages.len();
    let page_i = app.cur(Panel::Efficiency).min(n - 1);
    let suffix = if n > 1 {
        format!("{period} · page {}/{n} · ↑↓", page_i + 1)
    } else {
        period
    };
    let inner = frame(f, app, Panel::Efficiency, area, &suffix, false);
    let cols = hsplit(inner, &vec![1; ncols]);
    for (rect, col) in cols.iter().zip(pages.swap_remove(page_i)) {
        put(
            f,
            Rect {
                x: rect.x + 1,
                width: rect.width.saturating_sub(2),
                ..*rect
            },
            col,
        );
    }
}

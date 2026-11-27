//! HISTORY: daily bars by model and the weekday × hour heat map.

use chrono::{Datelike, Local};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{faint, trend};
use crate::core::models::Family;
use crate::tui::app::{App, Panel};
use crate::tui::view::kit::*;
use crate::ui::fmt;

pub fn history(f: &mut Frame, app: &mut App, area: Rect) {
    let inner = frame(
        f,
        app,
        Panel::History,
        area,
        "14 days · weekday × hour",
        false,
    );
    let app: &App = app;
    let th = app.theme;
    if inner.height < 6 {
        return;
    }
    let by_day: std::collections::HashMap<&str, f64> = app
        .analytics
        .days
        .iter()
        .map(|d| (d.key.as_str(), d.cost))
        .collect();
    let today = Local::now().date_naive();
    let days: Vec<(String, f64)> = (0..14)
        .rev()
        .map(|i| {
            let k = (today - chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string();
            let c = by_day.get(k.as_str()).copied().unwrap_or(0.0);
            (k, c)
        })
        .collect();
    let heat_h: u16 = 10;
    // Bars, labels and the selected day's line.
    let bars_h = inner.height.saturating_sub(heat_h + 1).max(5);
    let col = ((inner.width as usize) / 14).max(2);
    let h = bars_h.saturating_sub(3) as usize;
    let max = days.iter().map(|d| d.1).fold(0.0, f64::max).max(1e-9);
    // Per day: (family, cost) from the bottom up, biggest first.
    let fams = [
        Family::Opus,
        Family::Fable,
        Family::Sonnet,
        Family::Haiku,
        Family::Gemini,
        Family::Gpt,
        Family::Other,
    ];
    let mut stacks: std::collections::HashMap<&str, Vec<(Family, f64)>> = Default::default();
    for (d, m, c) in &app.analytics.days_model {
        let fam = match Family::of(m) {
            Family::Mythos => Family::Fable,
            x => x,
        };
        let e = stacks.entry(d.as_str()).or_default();
        match e.iter_mut().find(|x| x.0 == fam) {
            Some(x) => x.1 += c,
            None => e.push((fam, *c)),
        }
    }
    for v in stacks.values_mut() {
        v.sort_by_key(|x| fams.iter().position(|f| *f == x.0).unwrap_or(9));
    }
    let sel = 13usize.saturating_sub(app.cur(Panel::History).min(13));
    let focused = app.focus == Panel::History;
    let today_k = today.format("%Y-%m-%d").to_string();
    let weekend = |k: &str| {
        chrono::NaiveDate::parse_from_str(k, "%Y-%m-%d")
            .map(|d| matches!(d.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun))
            .unwrap_or(false)
    };
    let mut lines = Vec::new();
    for r in (0..h).rev() {
        let mut spans = Vec::new();
        for (i, (k, c)) in days.iter().enumerate() {
            let level = c / max * h as f64;
            let ch = if level >= r as f64 + 1.0 {
                '█'
            } else if level > r as f64 {
                VBARS[(((level - r as f64) * 8.0) as usize).clamp(0, 7)]
            } else {
                ' '
            };
            // The family that fills the middle of this cell.
            let mid = (r as f64 + 0.5).min(level) / h as f64 * max;
            let mut acc = 0.0;
            let mut fam = Family::Other;
            for (f, v) in stacks.get(k.as_str()).map(|v| v.as_slice()).unwrap_or(&[]) {
                fam = *f;
                acc += v;
                if acc >= mid {
                    break;
                }
            }
            let mut color = th.family(fam);
            if *k == today_k {
                color = pulse(app, color, 0.5);
            } else if focused && i != sel {
                color = th.fade(color, 0.45);
            } else if weekend(k) {
                color = th.fade(color, 0.3);
            }
            spans.push(Span::styled(
                format!("{} ", ch.to_string().repeat(col - 1)),
                Style::new().fg(color),
            ));
        }
        lines.push(Line::from(spans));
    }
    let labels: Vec<Span> = days
        .iter()
        .enumerate()
        .map(|(i, (k, _))| {
            let d = fit(k.get(8..10).unwrap_or(""), col);
            if i == sel && focused {
                Span::styled(d, Style::new().fg(th.bg).bg(th.accent))
            } else if *k == today_k {
                Span::styled(d, th.bold(th.accent))
            } else if weekend(k) {
                Span::styled(d, Style::new().fg(th.warn).add_modifier(Modifier::DIM))
            } else {
                Span::styled(d, faint(app))
            }
        })
        .collect();
    lines.push(Line::from(labels));
    // The selected day (today unless the panel is focused and moved).
    let (k, c) = &days[sel];
    let date = chrono::NaiveDate::parse_from_str(k, "%Y-%m-%d").ok();
    let mut sp = vec![
        Span::styled(
            date.map(|d| d.format("%a %d %b").to_string())
                .unwrap_or_default(),
            th.bold(th.fg),
        ),
        Span::styled(format!(" {}", fmt::usd(*c)), th.bold(th.fg)),
    ];
    if sel > 0 {
        sp.push(trend(app, *c, days[sel - 1].1));
        sp.push(Span::styled(" vs day before", faint(app)));
    }
    for (fam, v) in stacks.get(k.as_str()).map(|v| v.as_slice()).unwrap_or(&[]) {
        if *v / c.max(1e-9) >= 0.005 {
            sp.push(Span::styled(
                format!("  ■ {} {}", fam.name(), fmt::pct(v / c.max(1e-9) * 100.0)),
                Style::new().fg(th.family(*fam)),
            ));
        }
    }
    lines.push(Line::from(sp));
    lines.push(Line::from(Span::styled(
        format!(
            "max {}/day · 14 days {}{}",
            fmt::usd(max),
            fmt::usd(days.iter().map(|d| d.1).sum::<f64>()),
            if focused { " · ←→ pick a day" } else { "" }
        ),
        faint(app),
    )));
    put(
        f,
        Rect {
            height: bars_h + 1,
            ..inner
        },
        lines,
    );

    // Weekday × hour heatmap.
    let heat = Rect {
        y: inner.y + bars_h + 2,
        height: inner.height.saturating_sub(bars_h + 2),
        ..inner
    };
    let mut grid = [[0.0f64; 24]; 7];
    for (wd, hr, c) in &app.analytics.heat {
        if let (Ok(d), Ok(h)) = (wd.parse::<usize>(), hr.parse::<usize>())
            && d < 7
            && h < 24
        {
            grid[d][h] += c;
        }
    }
    let hmax = grid.iter().flatten().copied().fold(0.0, f64::max).max(1e-9);
    let cw = (((heat.width as usize).saturating_sub(4)) / 24).max(1);
    let names = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let mut lines = Vec::new();
    // A strip per weekday: its hours touch, and the upper half block leaves a gap under each
    // strip. With a gap between the hours and none between the days, the cells lined up as
    // columns and read as days.
    for d in [1, 2, 3, 4, 5, 6, 0] {
        let mut spans = vec![Span::styled(format!("{} ", names[d]), faint(app))];
        for hh in grid[d] {
            let k = (hh / hmax) as f32;
            let c = if hh <= 0.0 {
                th.track
            } else {
                th.fade(th.accent, 0.85 - k * 0.85)
            };
            spans.push(Span::styled(HEAT_CELL.repeat(cw), Style::new().fg(c)));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(
        format!("    {}", hour_ticks(cw)),
        faint(app),
    )));
    // Colour key of the heat map.
    let mut key = vec![Span::styled("    less ", faint(app))];
    for k in [0.85f32, 0.6, 0.35, 0.0] {
        key.push(Span::styled(
            HEAT_CELL,
            Style::new().fg(th.fade(th.accent, k)),
        ));
    }
    key.push(Span::styled(
        " more · spend per hour, last 4 weeks, local time",
        faint(app),
    ));
    lines.push(Line::from(key));
    put(f, heat, lines);
}

/// One hour of a weekday's strip in the heat map.
const HEAT_CELL: &str = "▀";

/// The hours under the heat map's strips, `cw` columns an hour: every third hour when the
/// labels fit, else every sixth, each label at its hour's first column.
fn hour_ticks(cw: usize) -> String {
    let step = if cw >= 2 { 3 } else { 6 };
    let mut out = String::new();
    for hh in (0..24).step_by(step) {
        let at = hh * cw;
        if out.chars().count() < at {
            out.push_str(&" ".repeat(at - out.chars().count()));
        } else if !out.is_empty() {
            continue;
        }
        out.push_str(&hh.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;
    use ratatui::buffer::Buffer;

    /// The panel alone with spend in every hour of every weekday.
    fn draw(w: u16, h: u16) -> Buffer {
        let t = tempfile::tempdir().unwrap();
        let mut app = App::new(Theme::named(None), false, true, Vec::new(), t.path().into());
        app.boot = None;
        app.analytics.heat = (0..7)
            .flat_map(|d| (0..24).map(move |h| (d.to_string(), h.to_string(), 1.0 + h as f64)))
            .collect();
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| history(f, &mut app, f.area())).unwrap();
        term.backend().buffer().clone()
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// A weekday's row of the heat map is one unbroken strip of half blocks: the cells had a
    /// space after each hour and full blocks that touched the rows above and below, so the
    /// hours lined up as columns and read as days.
    #[test]
    fn the_heat_map_is_a_strip_per_weekday() {
        for (w, h) in [(60, 30), (110, 30), (34, 30)] {
            let buf = draw(w, h);
            let rows: Vec<String> = (0..h).map(|y| row(&buf, y)).collect();
            let strips: Vec<&String> = ["Mon ", "Tue ", "Sun "]
                .iter()
                .map(|d| rows.iter().find(|r| r.contains(d)).expect(d))
                .collect();
            let cw = ((w as usize - 2 - 4) / 24).max(1);
            for r in strips {
                let cells = r.split_once(' ').unwrap().1;
                assert!(
                    cells.starts_with(&HEAT_CELL.repeat(24 * cw)),
                    "{w}x{h}: 24 hours of {cw} touching cells: {r:?}"
                );
                assert!(!r.contains('█') && !r.contains('■'), "{r:?}");
            }
        }
    }

    /// The hours under the strips: every third one when there is room, each at its column.
    #[test]
    fn hour_ticks_sit_under_their_hours() {
        assert_eq!(
            hour_ticks(2),
            "0     3     6     9     12    15    18    21"
        );
        assert_eq!(hour_ticks(1), "0     6     12    18");
        let t = hour_ticks(4);
        assert_eq!(t.find("12"), Some(12 * 4));
        assert_eq!(t.find("21"), Some(21 * 4));
    }
}

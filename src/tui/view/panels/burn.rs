//! BURN: the spend rate in big digits ($ per hour), today and the period, and $ per minute over
//! the last 60 minutes.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CLine};

use super::{dim, faint};
use crate::tui::app::{App, Panel};
use crate::tui::bigdigits;
use crate::tui::view::kit::*;
use crate::ui::fmt;

pub fn burn(f: &mut Frame, app: &mut App, area: Rect) {
    let inner = frame(f, app, Panel::Burn, area, "$ per minute · 60 min", false);
    let app: &App = app;
    let th = app.theme;
    if inner.height < 3 {
        return;
    }
    // Headline figures on the left when there is room.
    // Big digits carry the number only; the unit is written under it.
    let b = app.snapshot.burn_per_hour;
    let rate = if b >= 100.0 {
        format!("{b:.0}")
    } else if b >= 10.0 {
        format!("{b:.1}")
    } else {
        format!("{b:.2}")
    };
    // The rate per model family over the last 60 minutes (the families' sums are the total):
    // what burns the money now. The total itself is in the header bar; here it is the headline
    // only on a big panel.
    let rates = app.snapshot.burn_by_family();
    let rate_of = |r: f64| format!("{}/h", fmt::usd(r));
    // Today by harness, when more than one runs here (`cc $98 · agy $22`).
    let split = if app.multi_harness() {
        crate::core::harness::Harness::ALL
            .iter()
            .filter_map(|h| {
                let seats: Vec<_> = app
                    .analytics
                    .seats
                    .iter()
                    .filter(|s| s.harness == *h)
                    .collect();
                (!seats.is_empty()).then(|| {
                    let c: f64 = seats.iter().map(|s| s.today_cost).sum();
                    format!("{} {}", h.short(), fmt::usd(c))
                })
            })
            .collect::<Vec<_>>()
            .join(" · ")
    } else {
        String::new()
    };
    // Spend today, not a rate: said so under the rates per hour.
    let split = if split.is_empty() {
        split
    } else {
        format!("today {split}")
    };
    let name_w = rates.iter().map(|(n, _)| w(n)).max().unwrap_or(0);
    let model_lines: Vec<String> = rates
        .iter()
        .map(|(n, r)| format!("{n:<name_w$}  {}", rate_of(*r)))
        .collect();
    // The left column fits the big number and its lines, so the chart never covers them.
    let labels_w = model_lines
        .iter()
        .map(|l| w(l))
        .max()
        .unwrap_or(0)
        .max(w(&split))
        .max(10);
    let big_w = (bigdigits::width(&rate).max(labels_w) + 2) as u16;
    let accent = if app.snapshot.burn_per_hour > 0.0 {
        th.accent
    } else {
        th.dim
    };
    let chart_area = if inner.width >= big_w + 30 && inner.height >= 6 {
        let mut lines: Vec<Line> = bigdigits::render(&rate)
            .into_iter()
            .map(|r| Line::from(Span::styled(r, th.bold(accent))))
            .collect();
        lines.push(Line::from(Span::styled("$ per hour", dim(app))));
        lines.push(Line::from(""));
        // As many families as the column holds, the largest first; the harness split last.
        let room =
            (inner.height as usize).saturating_sub(lines.len() + usize::from(!split.is_empty()));
        for ((n, _), l) in rates.iter().zip(&model_lines).take(room) {
            lines.push(Line::from(Span::styled(
                l.clone(),
                Style::new().fg(th.family_name(n)),
            )));
        }
        if !split.is_empty() {
            lines.push(Line::from(Span::styled(split.clone(), faint(app))));
        }
        put(
            f,
            Rect {
                width: big_w,
                ..inner
            },
            lines,
        );
        Rect {
            x: inner.x + big_w,
            width: inner.width - big_w,
            ..inner
        }
    } else {
        // A small panel: the chart takes it, with the rates per family on one line above it
        // (as many as fit, the largest first).
        let mut spans: Vec<Span> = Vec::new();
        let mut left = inner.width as usize;
        for (n, r) in &rates {
            let piece = format!(
                "{}{n} {}",
                if spans.is_empty() { "" } else { " · " },
                rate_of(*r)
            );
            if w(&piece) > left {
                break;
            }
            left -= w(&piece);
            spans.push(Span::styled(piece, th.bold(th.family_name(n))));
        }
        if spans.is_empty() {
            spans.push(Span::styled("nothing spent in the last hour", faint(app)));
        }
        put(f, Rect { height: 1, ..inner }, vec![Line::from(spans)]);
        Rect {
            y: inner.y + 1,
            height: inner.height - 1,
            ..inner
        }
    };
    if chart_area.height < 2 {
        return;
    }
    let [chart, legend] = [
        Rect {
            height: chart_area.height - 1,
            ..chart_area
        },
        Rect {
            y: chart_area.y + chart_area.height - 1,
            height: 1,
            ..chart_area
        },
    ];
    let fams = [
        "fable", "mythos", "opus", "sonnet", "haiku", "gemini", "gpt", "other",
    ];
    let burn = &app.snapshot.burn;
    let mut totals = vec![0.0f64; 60];
    for s in burn.values() {
        for (i, v) in s.iter().enumerate() {
            totals[i] += v;
        }
    }
    let max = totals.iter().copied().fold(0.0, f64::max).max(0.01) * 1.1;
    let series: Vec<(Color, Vec<f64>)> = fams
        .iter()
        .filter_map(|n| burn.get(*n).map(|v| (th.family_name(n), v.clone())))
        .collect();
    let track = th.track;
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, 60.0])
        .y_bounds([0.0, max])
        .background_color(th.bg)
        .paint(move |ctx| {
            for i in 0..60 {
                let mut y0 = 0.0;
                for (c, v) in &series {
                    let y1 = y0 + v[i];
                    if v[i] > 0.0 {
                        for dx in [0.0, 0.33, 0.66] {
                            ctx.draw(&CLine::new(i as f64 + dx, y0, i as f64 + dx, y1, *c));
                        }
                    }
                    y0 = y1;
                }
            }
            ctx.draw(&CLine::new(0.0, 0.0, 60.0, 0.0, track));
        });
    f.render_widget(canvas, chart);
    let mut spans = Vec::new();
    for n in ["fable", "opus", "sonnet", "haiku", "gemini", "gpt"] {
        if burn.contains_key(n) {
            spans.push(Span::styled(
                format!("■ {n}  "),
                Style::new().fg(th.family_name(n)),
            ));
        }
    }
    spans.push(Span::styled(
        format!("peak {}/min · −60 min … now", fmt::usd(max / 1.1)),
        faint(app),
    ));
    put(f, legend, vec![Line::from(spans)]);
}

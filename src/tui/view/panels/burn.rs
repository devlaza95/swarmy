//! BURN: the spend rate in big digits ($ per hour), the rate of each model, and $ per minute
//! over the last 60 minutes stacked by model.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CLine};

use super::{dim, faint};
use crate::core::models::Family;
use crate::tui::app::{App, Panel};
use crate::tui::bigdigits;
use crate::tui::theme::Theme;
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
    // The rate of each model over the last 60 minutes (their sum is the total): what burns the
    // money now, down to the model (`gem-4-neon` apart from `gem-4-hydrogen`). The total itself
    // is in the header bar; here it is the headline only on a big panel.
    let rates = app.snapshot.burn_by_model();
    let series = stacked(&app.snapshot.burn, &th);
    let colour_of = |n: &str| {
        series
            .iter()
            .find(|s| s.name == n)
            .map_or(th.family_name(n), |s| s.colour)
    };
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
                Style::new().fg(colour_of(n)),
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
        // A small panel: the chart takes it, with the rates per model in lines above it, the
        // largest first, as many lines as leave the chart four rows and its legend.
        let pieces: Vec<(String, Color)> = rates
            .iter()
            .map(|(n, r)| (format!("{n} {}", rate_of(*r)), colour_of(n)))
            .collect();
        let most = (inner.height as usize).saturating_sub(6).max(1);
        let (rows, hidden) = pack(&pieces, inner.width as usize, most);
        let mut lines: Vec<Line> = rows
            .into_iter()
            .map(|row| {
                let mut spans: Vec<Span> = Vec::new();
                for (text, c) in row {
                    if !spans.is_empty() {
                        spans.push(Span::styled(" · ", faint(app)));
                    }
                    spans.push(Span::styled(text, th.bold(c)));
                }
                Line::from(spans)
            })
            .collect();
        if hidden > 0
            && let Some(last) = lines.last_mut()
        {
            last.spans
                .push(Span::styled(format!(" +{hidden}"), faint(app)));
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "nothing spent in the last hour",
                faint(app),
            )));
        }
        let used = lines.len() as u16;
        put(
            f,
            Rect {
                height: used,
                ..inner
            },
            lines,
        );
        Rect {
            y: inner.y + used,
            height: inner.height - used,
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
    let burn = &app.snapshot.burn;
    let max = peak_minute(burn).max(0.01) * 1.1;
    let legend_of: Vec<(String, Color)> =
        series.iter().map(|s| (s.name.clone(), s.colour)).collect();
    let track = th.track;
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, 60.0])
        .y_bounds([0.0, max])
        .background_color(th.bg)
        .paint(move |ctx| {
            // The baseline first: a braille cell takes the colour of what was drawn in it last,
            // so a baseline drawn over the bars would grey their lowest cell.
            ctx.draw(&CLine::new(0.0, 0.0, 60.0, 0.0, track));
            for i in 0..60 {
                let mut y0 = 0.0;
                for s in &series {
                    let v = s.minutes[i];
                    let y1 = y0 + v;
                    if v > 0.0 {
                        for dx in [0.0, 0.33, 0.66] {
                            ctx.draw(&CLine::new(i as f64 + dx, y0, i as f64 + dx, y1, s.colour));
                        }
                    }
                    y0 = y1;
                }
            }
        });
    f.render_widget(canvas, chart);
    // The models of the chart in the order of the stack (bottom first), as many as fit before
    // the peak and the time axis, which always show.
    let peak = format!("peak {}/min · −60 min … now", fmt::usd(max / 1.1));
    let mut left = (legend.width as usize).saturating_sub(w(&peak));
    let mut spans: Vec<Span> = Vec::new();
    for (n, c) in legend_of {
        let piece = format!("■ {n}  ");
        if w(&piece) > left {
            break;
        }
        left -= w(&piece);
        spans.push(Span::styled(piece, Style::new().fg(c)));
    }
    spans.push(Span::styled(peak, faint(app)));
    put(f, legend, vec![Line::from(spans)]);
}

/// One model's part of the chart.
struct Series {
    name: String,
    colour: Color,
    minutes: Vec<f64>,
}

/// The models that spent in the last hour as the chart stacks them, bottom first: by family
/// (fable, opus, sonnet, haiku, gemini, gpt, other), then by name; each family's models in its
/// colour and its shades (`Theme::model_shade`), in that order.
fn stacked(burn: &BTreeMap<String, Vec<f64>>, th: &Theme) -> Vec<Series> {
    let mut names: Vec<(Family, &String)> = burn
        .iter()
        .filter(|(_, v)| v.iter().any(|c| *c > 0.0))
        .map(|(n, _)| (Family::of(n), n))
        .collect();
    names.sort();
    let mut out: Vec<Series> = Vec::new();
    let mut k = 0;
    for (i, (fam, n)) in names.iter().enumerate() {
        k = if i > 0 && names[i - 1].0 == *fam {
            k + 1
        } else {
            0
        };
        out.push(Series {
            name: (*n).clone(),
            colour: th.model_shade(th.family(*fam), k),
            minutes: burn[*n].clone(),
        });
    }
    out
}

/// Pieces (`opus-5.5 $29.74/h`) into at most `most` lines of `width` columns, in their order,
/// joined by ` · `, with room left on the last line for ` +N` when some do not fit; and N.
fn pack(
    pieces: &[(String, Color)],
    width: usize,
    most: usize,
) -> (Vec<Vec<(String, Color)>>, usize) {
    const SEP: usize = 3; // " · "
    let mut rows: Vec<Vec<(String, Color)>> = Vec::new();
    let mut used = 0;
    for (i, (text, c)) in pieces.iter().enumerate() {
        // The last line keeps room for " +N" while pieces remain after this one.
        let more = pieces.len() - i - 1;
        let tail = |row: usize| {
            if row + 1 == most && more > 0 {
                2 + more.to_string().len()
            } else {
                0
            }
        };
        let n = rows.len();
        if let Some(last) = rows.last_mut()
            && used + SEP + w(text) + tail(n - 1) <= width
        {
            used += SEP + w(text);
            last.push((text.clone(), *c));
        } else if rows.len() < most && w(text) + tail(rows.len()) <= width {
            rows.push(vec![(text.clone(), *c)]);
            used = w(text);
        } else if rows.is_empty() {
            // Wider than the panel: cut when drawn, never left out.
            rows.push(vec![(text.clone(), *c)]);
            used = w(text);
        } else {
            return (rows, pieces.len() - i);
        }
    }
    (rows, 0)
}

/// The largest minute of all models together: the top of the chart.
fn peak_minute(burn: &BTreeMap<String, Vec<f64>>) -> f64 {
    (0..60)
        .map(|i| burn.values().filter_map(|v| v.get(i)).sum::<f64>())
        .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;
    use ratatui::buffer::Buffer;

    /// $ per minute: `at` lists (minute, dollars), the rest spent nothing.
    fn minutes(at: &[(usize, f64)]) -> Vec<f64> {
        let mut m = vec![0.0; 60];
        for (i, c) in at {
            m[*i] = *c;
        }
        m
    }

    /// The BURN panel alone, drawn into a `w`×`h` buffer from these families' minutes.
    fn draw(w: u16, h: u16, burn: &[(&str, Vec<f64>)]) -> (Buffer, Theme) {
        let t = tempfile::tempdir().unwrap();
        let mut app = App::new(Theme::named(None), false, true, Vec::new(), t.path().into());
        app.boot = None;
        app.snapshot.burn = burn
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect();
        app.snapshot.burn_per_hour = burn.iter().flat_map(|(_, v)| v).sum();
        let th = app.theme;
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::burn(f, &mut app, f.area())).unwrap();
        (term.backend().buffer().clone(), th)
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn text(buf: &Buffer) -> String {
        (0..buf.area.height).map(|y| row(buf, y) + "\n").collect()
    }

    /// The chart's lowest row: the one above the legend (`peak …`).
    fn bottom_row(buf: &Buffer) -> u16 {
        (0..buf.area.height)
            .find(|y| row(buf, *y).contains("peak "))
            .expect("a legend")
            - 1
    }

    /// The braille cells of a row with something drawn, as their colours.
    fn dots(buf: &Buffer, y: u16) -> Vec<Color> {
        (0..buf.area.width)
            .map(|x| &buf[(x, y)])
            .filter(|c| {
                c.symbol()
                    .chars()
                    .any(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch))
            })
            .map(|c| c.fg)
            .collect()
    }

    fn chart_colours(buf: &Buffer) -> Vec<Color> {
        (0..bottom_row(buf) + 1)
            .flat_map(|y| dots(buf, y))
            .collect()
    }

    /// The baseline was drawn over the bars, so the lowest cell of every bar took its grey.
    #[test]
    fn the_bars_keep_their_colour_down_to_the_baseline() {
        for (w, h) in [(120, 20), (40, 8)] {
            let every = minutes(&(0..60).map(|i| (i, 1.0)).collect::<Vec<_>>());
            let (buf, th) = draw(w, h, &[("fable-5.2", every.clone()), ("opus-5.5", every)]);
            let low = dots(&buf, bottom_row(&buf));
            assert!(!low.is_empty(), "{w}x{h}:\n{}", text(&buf));
            assert!(
                low.iter().all(|c| *c == th.fable),
                "{w}x{h}: the stack's lowest model fills the lowest row: {low:?}"
            );
            let all = chart_colours(&buf);
            assert!(all.contains(&th.opus), "{w}x{h}: opus is stacked on top");
            assert!(!all.contains(&th.track), "{w}x{h}: no baseline under bars");
        }
    }

    /// Where no minute spent, the baseline is there.
    #[test]
    fn the_baseline_shows_where_nothing_was_spent() {
        let (buf, th) = draw(120, 20, &[("opus-5.5", minutes(&[(59, 2.0)]))]);
        let low = dots(&buf, bottom_row(&buf));
        assert!(low.contains(&th.track), "the baseline: {low:?}");
        assert!(low.contains(&th.opus), "the last minute's bar");
    }

    /// Every model that spent has its family's colour in the chart and its entry in the legend:
    /// Codex's `gpt` once took `other`'s colour, and the legend left `mythos` and `other` out.
    #[test]
    fn every_model_is_in_the_chart_and_the_legend() {
        let one = minutes(&[(30, 1.0), (31, 1.0)]);
        let models = [
            "opus-5.5",
            "gem-4-neon",
            "gpt-6-luna",
            "mythos-1",
            "kimi-k3",
        ];
        let burn: Vec<(&str, Vec<f64>)> = models.iter().map(|n| (*n, one.clone())).collect();
        let (buf, th) = draw(200, 40, &burn);
        let legend = row(&buf, bottom_row(&buf) + 1);
        for n in models {
            assert!(legend.contains(&format!("■ {n} ")), "{n} in {legend}");
        }
        assert!(legend.contains("peak $5.00/min"), "{legend}");
        let all = chart_colours(&buf);
        for c in [th.opus, th.gemini, th.gpt, th.fable, th.other] {
            assert!(all.contains(&c), "{c:?} in the chart");
        }
        // A model that spent nothing in the hour has no entry.
        let (buf, _) = draw(160, 40, &[("opus-5.5", one), ("sonnet-5", minutes(&[]))]);
        assert!(!text(&buf).contains("sonnet"));
    }

    /// Two models of one family (agy's Neon and Hydrogen) are told apart: each has its line and
    /// its shade of the family's colour, in the chart and in the lines.
    #[test]
    fn models_of_one_family_are_told_apart() {
        // gem-4-neon $0.20 × 60 = $12.00/h, gem-4-hydrogen $0.05 × 60 = $3.00/h.
        let neon = minutes(&(0..60).map(|i| (i, 0.2)).collect::<Vec<_>>());
        let hydrogen = minutes(&(0..60).map(|i| (i, 0.05)).collect::<Vec<_>>());
        let (buf, th) = draw(
            140,
            30,
            &[("gem-4-neon", neon), ("gem-4-hydrogen", hydrogen)],
        );
        let s = text(&buf);
        assert!(s.contains("gem-4-neon      $12.00/h"), "{s}");
        assert!(s.contains("gem-4-hydrogen  $3.00/h"), "{s}");
        // By name: hydrogen is the family's first, in its colour; neon in the first shade.
        let shade = th.model_shade(th.gemini, 1);
        assert_ne!(shade, th.gemini);
        let all = chart_colours(&buf);
        assert!(all.contains(&th.gemini) && all.contains(&shade), "{all:?}");
        let line_fg = |needle: &str| {
            let y = (0..buf.area.height)
                .find(|y| row(&buf, *y).contains(needle))
                .unwrap();
            let x = row(&buf, y).find(needle).unwrap();
            let x = row(&buf, y)[..x].chars().count() as u16;
            buf[(x, y)].fg
        };
        assert_eq!(line_fg("gem-4-hydrogen  $"), th.gemini);
        assert_eq!(line_fg("gem-4-neon      $"), shade);
    }

    /// The rates of a small panel wrap into the lines it has, and say how many are left out.
    #[test]
    fn rates_wrap_into_the_lines_a_small_panel_has() {
        let c = Color::Reset;
        let p = |t: &str| (t.to_string(), c);
        // Widths 18, 17, 16: with " · " the first two take 38 columns.
        let pieces = [
            p("fable-5.2 $39.43/h"),
            p("opus-5.5 $29.74/h"),
            p("gem-4-neon $0.5/h"),
        ];
        let texts = |rows: &Vec<Vec<(String, Color)>>| -> Vec<Vec<String>> {
            rows.iter()
                .map(|r| r.iter().map(|(t, _)| t.clone()).collect())
                .collect()
        };
        let (rows, hidden) = pack(&pieces, 80, 3);
        assert_eq!(
            texts(&rows),
            vec![vec![
                pieces[0].0.clone(),
                pieces[1].0.clone(),
                pieces[2].0.clone()
            ]]
        );
        assert_eq!(hidden, 0);
        let (rows, hidden) = pack(&pieces, 38, 3);
        assert_eq!(texts(&rows).len(), 2);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(hidden, 0);
        // One line of 38: the first two fit, but " +1" must fit too, so the second goes.
        let (rows, hidden) = pack(&pieces, 38, 1);
        assert_eq!(texts(&rows), vec![vec![pieces[0].0.clone()]]);
        assert_eq!(hidden, 2);
        // A piece wider than the panel is still shown (cut when drawn).
        let (rows, hidden) = pack(&pieces, 10, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(hidden, 2);
    }

    /// The small panel in the dashboard's column layout (≈ 38 columns, 12 rows): every model of
    /// the hour is named above the chart.
    #[test]
    fn a_small_panel_names_every_model() {
        let one = minutes(&[(30, 1.0)]);
        let models = [
            "fable-5.2",
            "opus-5.5",
            "gem-4-neon",
            "gem-4-hydrogen",
            "gpt-6-luna",
        ];
        let burn: Vec<(&str, Vec<f64>)> = models.iter().map(|n| (*n, one.clone())).collect();
        let (buf, _) = draw(40, 14, &burn);
        let s = text(&buf);
        for n in models {
            assert!(s.contains(n), "{n} in\n{s}");
        }
        assert!(s.contains("peak "), "the chart is still there:\n{s}");
    }

    /// Shades step away from the family's colour, lighter and darker in turn, and stay apart.
    #[test]
    fn shades_of_a_family_differ() {
        for th in [Theme::named(None)] {
            let c = th.opus;
            let shades: Vec<Color> = (0..5).map(|k| th.model_shade(c, k)).collect();
            assert_eq!(shades[0], c);
            for i in 0..shades.len() {
                for j in i + 1..shades.len() {
                    assert_ne!(shades[i], shades[j], "{shades:?}");
                }
            }
        }
        // The 16-colour theme alternates a colour with its twin.
        let th = Theme::named(None);
        assert_eq!(th.model_shade(Color::Magenta, 1), Color::LightMagenta);
        assert_eq!(th.model_shade(Color::Magenta, 2), Color::Magenta);
    }

    /// The rate of each model is its hour's spend, and they add up to the big number:
    /// fable-5.2 $0.50 × 60 min = $30.00/h, opus-5.5 $0.25 × 60 = $15.00/h, together $45.00/h.
    #[test]
    fn the_rates_per_model_add_up_to_the_headline() {
        let fable = minutes(&(0..60).map(|i| (i, 0.5)).collect::<Vec<_>>());
        let opus = minutes(&(0..60).map(|i| (i, 0.25)).collect::<Vec<_>>());
        let burn = [("fable-5.2", fable), ("opus-5.5", opus)];
        let (big, _) = draw(140, 24, &burn);
        let s = text(&big);
        assert!(s.contains("$ per hour"), "{s}");
        assert!(s.contains("fable-5.2  $30.00/h"), "{s}");
        assert!(s.contains("opus-5.5   $15.00/h"), "{s}");
        let (small, _) = draw(50, 8, &burn);
        let s = text(&small);
        assert!(s.contains("fable-5.2 $30.00/h · opus-5.5 $15.00/h"), "{s}");
    }
}

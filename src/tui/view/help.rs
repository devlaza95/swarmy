//! The help overlay: keys of the focused panel and everywhere, the legend, and the glossary.
//!
//! Tab turns the pages (`HelpPage`; keys and legend share one on a wide screen). The keys and
//! legend pages are lines built from the app; the glossary page lays out `ui::glossary::TERMS`
//! by group, scrolls with its selection and opens the selected term's story. Nothing on a page
//! animates or reads the clock, so a frame of the overlay is the same until a key arrives.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::kit::*;
use super::overlays::{centered, dialog_line};
use super::panel_keys;
use crate::tui::app::{App, GlossaryAt, HelpPage, Shape};
use crate::tui::theme::Theme;
use crate::ui::glossary::{Group, TERMS};
use crate::ui::table::{truncate, wrap};

/// From this width the keys and the legend sit side by side on one page.
const WIDE: u16 = 170;
/// The glossary page's width on a wide screen: past this its lines are too long to read.
const GLOSSARY_WIDE: u16 = 112;
/// Rows the open term's story is given on top of the list (the longest takes 7 at the standard
/// width: a test keeps it so). The dialog is as tall as the list and that, whatever term is open,
/// so it does not move when the selection does; a shorter screen scrolls.
const STORY_ROWS: usize = 8;

pub(super) fn help(f: &mut Frame, app: &mut App, area: Rect) {
    let wide = area.width >= WIDE;
    // Tab needs to know whether the legend is a page of its own (`App::help_wide`).
    app.help_wide = wide;
    if wide && app.help_page == HelpPage::Legend {
        app.help_page = HelpPage::Keys;
    }
    match app.help_page {
        HelpPage::Glossary => glossary_page(f, app, area),
        HelpPage::Keys | HelpPage::Legend => keys_page(f, app, area, wide),
    }
}

/// The dialog of a page: its title names the pages (the open one on a plate), its bottom border
/// says how to turn them.
fn page_block(app: &App, wide: bool, room: usize) -> Block<'static> {
    let th = &app.theme;
    let mut title = vec![Span::raw(" ")];
    for page in HelpPage::cycle(wide) {
        let name = if wide && *page == HelpPage::Keys {
            "KEYS · LEGEND"
        } else {
            page.title()
        };
        let style = if *page == app.help_page {
            th.bold(th.bg).bg(th.accent)
        } else {
            Style::new().fg(th.dim)
        };
        title.push(Span::styled(format!(" {name} "), style));
    }
    if app.help_page == HelpPage::Glossary {
        title.push(Span::styled(
            format!(" {}/{}", app.glossary.term + 1, TERMS.len()),
            Style::new().fg(th.faint),
        ));
    }
    title.push(Span::raw(" "));
    dialog_line(app, Line::from(title)).title_bottom(hint_line(app.help_page, th, room))
}

/// A key and what it does.
type Hint = &'static [(&'static str, &'static str)];

/// Under the keys and legend pages, longest wording first.
const PAGE_HINTS: &[Hint] = &[
    &[
        ("Tab →", "next page"),
        ("Shift-Tab ←", "back"),
        ("Esc", "close"),
    ],
    &[("Tab", "next page"), ("Esc", "close")],
    &[("Tab", "pages")],
];

/// Under the glossary page: its own keys first, then shorter and shorter.
const GLOSSARY_HINTS: &[Hint] = &[
    &[
        ("↑↓ j k", "term"),
        ("PgUp PgDn", "8 terms"),
        ("Home End", "ends"),
        ("Tab →", "next page"),
        ("Shift-Tab ←", "back"),
        ("Esc", "close"),
    ],
    &[("↑↓", "term"), ("Tab", "next page"), ("Esc", "close")],
    &[("↑↓", "term"), ("Tab", "page")],
    &[("Tab", "pages")],
];

/// The width of a hint written out: `key text · key text`.
fn hint_width(h: Hint) -> usize {
    let words: usize = h.iter().map(|(k, d)| w(k) + 1 + w(d)).sum();
    words + 3 * h.len().saturating_sub(1)
}

/// The longest wording of the page's hints that fits `room` cells (the shortest otherwise).
fn hint_line(page: HelpPage, th: &Theme, room: usize) -> Line<'static> {
    let all = match page {
        HelpPage::Glossary => GLOSSARY_HINTS,
        HelpPage::Keys | HelpPage::Legend => PAGE_HINTS,
    };
    let pick = all
        .iter()
        .find(|h| hint_width(h) + 2 <= room)
        .or(all.last())
        .copied()
        .unwrap_or_default();
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, d)) in pick.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(th.faint)));
        }
        spans.push(Span::styled(*k, th.bold(th.accent)));
        spans.push(Span::styled(format!(" {d}"), Style::new().fg(th.dim)));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// The keys of the focused panel and of the dashboard, and the vault's.
fn keys_lines(app: &App) -> Vec<Line<'static>> {
    let th = &app.theme;
    let tabs = app.shape == Shape::Tabs;
    let mut rows: Vec<(String, String)> =
        vec![(format!("— {} —", app.focus.title()), String::new())];
    for part in panel_keys(app.focus).split(" · ") {
        if let Some((k, d)) = part.split_once(' ') {
            rows.push((k.to_string(), d.to_string()));
        }
    }
    rows.push(("— everywhere —".into(), String::new()));
    let focus_keys = if tabs {
        (
            "1-5",
            "switch tab (OPS · AGENT · LIMITS · SPEND · SYSTEM); Tab moves between its panels",
        )
    } else {
        (
            "1-9, 0",
            "focus the panel with that number (in its title); again = zoom; 0 = SYSTEM",
        )
    };
    for (k, d) in [
        (
            "Tab · Shift-Tab",
            "next / previous panel (the focused one has a bright border)",
        ),
        focus_keys,
        (
            "z · Esc",
            "zoom the focused panel · back (closes dialogs, zoom, filter)",
        ),
        (
            "n · N",
            "next / previous thing that needs attention (selects the agent)",
        ),
        (
            "/",
            "filter agents: words, p:project m:model s:seat h:harness state:needs",
        ),
        (
            ":",
            "switch <seat> <login> · capture <seat> · period 7d · layout wall|column|tabs",
        ),
        ("t · g", "period (today · 7d · 30d · all) · spend grouping"),
        (
            "J",
            "jump to the selected agent's Claude Code (tmux, zellij, kitty, wezterm, iTerm2, Warp; outside: `swarmy jump`)",
        ),
        ("v", "SWARM: activity lanes of 30 min ↔ spend of 15 min"),
        ("L", "layout: auto · wall · column · tabs"),
        ("m · p · T", "mask names · pause animations · theme"),
        (
            ".",
            "LIMITS · SEATS: show / hide the empty seats its hidden: line names",
        ),
        (
            "mouse",
            "click focuses a panel / selects an agent; wheel scrolls",
        ),
        (
            "help: Tab",
            "next page: keys · legend · glossary (Shift-Tab: back); on the glossary ↑↓ pick a term",
        ),
        (
            "? · q",
            if tabs {
                "legend, then close · quit"
            } else {
                "close help · quit"
            },
        ),
    ] {
        rows.push((k.into(), d.into()));
    }
    rows.push(("— vault (panel 2) —".into(), String::new()));
    for (k, d) in [
        (
            "Enter",
            "on a seat: pick a saved login to put in · on a saved login: pick a seat",
        ),
        (
            "S",
            "put the saved login with the most limit left into the seat",
        ),
        (
            "c · a",
            "save the seat's current login · add a new login (runs claude /login)",
        ),
        (
            "r · d · R",
            "rename · forget a saved login · re-read logins from disk",
        ),
        (
            "",
            "a switch saves the seat's current login first and backs up both files;",
        ),
        ("", "running sessions keep their old login until restarted"),
    ] {
        rows.push((k.into(), d.into()));
    }
    rows.into_iter()
        .map(|(k, d)| {
            if d.is_empty() {
                Line::from(Span::styled(k, th.bold(th.dim)))
            } else {
                Line::from(vec![
                    Span::styled(fit(&k, 16), th.bold(th.accent)),
                    Span::styled(d, Style::new().fg(th.fg)),
                ])
            }
        })
        .collect()
}

/// The keys and legend pages: both side by side on a wide screen, else one at a time.
fn keys_page(f: &mut Frame, app: &App, area: Rect, wide: bool) {
    let keys = keys_lines(app);
    let legend = legend_lines(app);
    let height = if wide {
        keys.len().max(legend.len())
    } else if app.help_page == HelpPage::Keys {
        keys.len()
    } else {
        legend.len()
    };
    let width = if wide {
        area.width.saturating_sub(6).min(186)
    } else {
        104
    };
    let r = centered(area, width, height as u16 + 2);
    f.render_widget(Clear, r);
    let block = page_block(app, wide, r.width.saturating_sub(2) as usize);
    let inner = block.inner(r);
    f.render_widget(block, r);
    if wide {
        let c = hsplit(inner, &[50, 50]);
        f.render_widget(Paragraph::new(keys), c[0]);
        f.render_widget(
            Paragraph::new(legend),
            Rect {
                x: c[1].x + 2,
                width: c[1].width.saturating_sub(2),
                ..c[1]
            },
        );
    } else if app.help_page == HelpPage::Keys {
        f.render_widget(Paragraph::new(keys), inner);
    } else {
        f.render_widget(Paragraph::new(legend), inner);
    }
}

/// One line of the glossary page, before it is styled.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// A group's title, on a plate.
    Heading(Group),
    /// A term (an index into [`TERMS`]): its name and one-line meaning.
    Term(usize),
    /// A line of the selected term's story, under its name.
    Story(String),
    /// Its other names and the terms to read next, dimmer.
    Note(String),
}

/// Cells before a story line, and before the names: the selection marker's.
const MARK_W: usize = 2;

/// `s` cut to `max` cells with an ellipsis; nothing at all when there is no room (`truncate`
/// would still give the ellipsis).
fn cut(s: &str, max: usize) -> String {
    if max == 0 {
        String::new()
    } else {
        truncate(s, max)
    }
}

/// The glossary page laid out for one width.
struct Layout {
    rows: Vec<Row>,
    /// The selected term's own row and the lines of its story under it.
    selected: Range<usize>,
    /// Cells of the name column, gap included.
    name_w: usize,
}

/// Every term under its group, the selected one followed by its story. Only that story is
/// wrapped: the other terms take one line each, cut to the width.
fn layout(width: usize, selected: usize) -> Layout {
    let longest = TERMS.iter().map(|t| w(t.name)).max().unwrap_or(0);
    // The longest name and a gap, but never more than half of a narrow page (8 cells at least):
    // a name tells the term, its one-line meaning is cut first. Room is left for the marker.
    let name_w = (longest + 2)
        .min((width / 2).max(8))
        .min(width.saturating_sub(MARK_W));
    let text_w = width.saturating_sub(MARK_W + 1).max(1);
    let mut rows = Vec::with_capacity(TERMS.len() + Group::ALL.len() + 12);
    let mut chosen = 0..0;
    let mut group = None;
    for (i, t) in TERMS.iter().enumerate() {
        if group != Some(t.group) {
            group = Some(t.group);
            rows.push(Row::Heading(t.group));
        }
        let top = rows.len();
        rows.push(Row::Term(i));
        if i == selected {
            rows.extend(wrap(t.long, text_w, text_w).into_iter().map(Row::Story));
            let mut notes = Vec::new();
            if !t.aliases.is_empty() {
                notes.push(format!("also called: {}", t.aliases.join(", ")));
            }
            if !t.see.is_empty() {
                notes.push(format!("see also: {}", t.see.join(" · ")));
            }
            for note in notes {
                rows.extend(wrap(&note, text_w, text_w).into_iter().map(Row::Note));
            }
            chosen = top..rows.len();
        }
    }
    Layout {
        rows,
        selected: chosen,
        name_w,
    }
}

/// The first row to show so the selected term and its story are in view: the story's end when
/// it fits below, the term's own row (and its group's title) when the story is taller than the
/// page, and no empty rows below the last one.
fn scroll_to_show(lay: &Layout, scroll: usize, height: usize) -> usize {
    let Range { start, end } = lay.selected.clone();
    let titled = start
        .checked_sub(1)
        .and_then(|i| lay.rows.get(i))
        .is_some_and(|r| matches!(r, Row::Heading(_)));
    let top = if titled { start - 1 } else { start };
    let mut first = scroll;
    if end > first + height {
        first = end - height;
    }
    if top < first {
        first = top;
    }
    first.min(lay.rows.len().saturating_sub(height))
}

fn row_line(th: &Theme, lay: &Layout, row: &Row, selected: usize, width: usize) -> Line<'static> {
    let text_w = width.saturating_sub(MARK_W);
    let indent = || Span::raw(" ".repeat(MARK_W));
    match row {
        Row::Heading(g) => Line::from(Span::styled(
            cut(&format!(" {} ", g.title()), width),
            th.heading(),
        )),
        Row::Term(i) => {
            let t = &TERMS[*i];
            let on = *i == selected;
            let name = cut(t.name, lay.name_w.saturating_sub(1));
            let gap = " ".repeat(lay.name_w.saturating_sub(w(&name)));
            let name_style = if on {
                th.bold(th.bg).bg(th.accent)
            } else {
                th.bold(th.fg)
            };
            Line::from(vec![
                Span::styled(if on { "▸ " } else { "  " }, th.bold(th.accent)),
                Span::styled(name, name_style),
                Span::raw(gap),
                Span::styled(
                    cut(t.short, text_w.saturating_sub(lay.name_w)),
                    Style::new().fg(th.fg),
                ),
            ])
        }
        Row::Story(t) => Line::from(vec![
            indent(),
            Span::styled(cut(t, text_w), Style::new().fg(th.fg)),
        ]),
        Row::Note(t) => Line::from(vec![
            indent(),
            Span::styled(cut(t, text_w), Style::new().fg(th.dim)),
        ]),
    }
}

/// The lines of the glossary page for an area `width` × `height`, and the position drawing
/// ended up at (`scroll` moved to keep the selected term in view).
fn glossary_view(
    th: &Theme,
    width: usize,
    height: usize,
    at: GlossaryAt,
) -> (Vec<Line<'static>>, GlossaryAt) {
    let term = at.term.min(TERMS.len().saturating_sub(1));
    let lay = layout(width, term);
    let scroll = scroll_to_show(&lay, at.scroll, height);
    // A page narrower than its own marker shows nothing (the dialog is a frame and a hint).
    let shown = if width < MARK_W { 0 } else { height };
    let lines = lay
        .rows
        .iter()
        .skip(scroll)
        .take(shown)
        .map(|r| row_line(th, &lay, r, term, width))
        .collect();
    (lines, GlossaryAt { term, scroll })
}

/// The glossary dialog on a tall terminal: every term, the group titles, a story and the frame.
fn glossary_height() -> u16 {
    (TERMS.len() + Group::ALL.len() + STORY_ROWS + 2) as u16
}

/// The glossary page: the terms by group, the selected one open. It reads nothing that moves:
/// the same keys give the same frame, so drawing it asks the clock for nothing.
fn glossary_page(f: &mut Frame, app: &mut App, area: Rect) {
    let width = if app.help_wide {
        GLOSSARY_WIDE.min(area.width.saturating_sub(6))
    } else {
        104
    };
    let height = area.height.saturating_sub(2).clamp(8, glossary_height());
    let r = centered(area, width, height);
    f.render_widget(Clear, r);
    let block = page_block(app, app.help_wide, r.width.saturating_sub(2) as usize);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let (lines, at) = glossary_view(
        &app.theme,
        inner.width as usize,
        inner.height as usize,
        app.glossary,
    );
    app.glossary = at;
    f.render_widget(Paragraph::new(lines), inner);
}

/// What the glyphs, numbers, colours and words on the screen mean.
fn legend_lines(app: &App) -> Vec<Line<'static>> {
    let th = &app.theme;
    let head = |t: &str| Line::from(Span::styled(format!("— {t} —"), th.bold(th.dim)));
    let row = |g: &str, c: ratatui::style::Color, d: &str| {
        Line::from(vec![
            Span::styled(fit(g, 16), th.bold(c)),
            Span::styled(d.to_string(), Style::new().fg(th.fg)),
        ])
    };
    vec![
        head("agent states"),
        row("●", th.good, "working: running a tool"),
        row(
            "⠋ ◌",
            th.thinking,
            "thinking: waiting for the model's answer",
        ),
        row("◐", th.accent, "waiting for its own agents"),
        row(
            "▲",
            th.warn,
            "needs you: a question, a permission, a plan to approve",
        ),
        row("✖", th.bad, "stalled: no activity for a while"),
        row("○ ✓ ·", th.dim, "idle · done · exited"),
        row(
            "▃ ▅ ▆ █",
            th.accent,
            "SWARM lanes, a minute each: thinking · tool · agents · you",
        ),
        row(
            "▀▀▀▀",
            th.accent,
            "HISTORY heat map: a strip per weekday, an hour per cell, brighter = more spend",
        ),
        head("alerts and events"),
        row("⊘", th.warn, "a weekly or 5-hour limit is close or reached"),
        row(
            "⚠",
            th.warn,
            "two agents edited the same file within the hour",
        ),
        row(
            "⚿",
            th.accent,
            "logins: unsaved, unknown, expiring (the vault)",
        ),
        row(
            "→ work, lab",
            th.dim,
            "LIMITS · SEATS: the seats a saved login is in (one login may serve several); free: in none",
        ),
        row(
            "CLAUDE CODE",
            th.fg,
            "LIMITS · SEATS sections per harness: its seats and logins (a login fits only its harness)",
        ),
        row(
            "hidden:",
            th.dim,
            "LIMITS · SEATS: a harness left out, its seats all empty (no login, no transcripts, nothing live, not in [[seat]]) and no saved login; . shows it",
        ),
        row(
            "+ ✓ ✎ ◆ !",
            th.dim,
            "spawned · finished · your prompt · online · API error",
        ),
        head("numbers"),
        row(
            "$",
            th.fg,
            "API-equivalent cost at list prices (not what a plan bills)",
        ),
        row(
            "Σ",
            th.fg,
            "the whole session: main thread and all its agents",
        ),
        row(
            "week used",
            th.fg,
            "share of the account's weekly limit; 1% ≈ $X: API dollars per 1%",
        ),
        row(
            "≈",
            th.fg,
            "an estimate between Claude Code's limit readings",
        ),
        row(
            "▲ ▼",
            th.warn,
            "change against the previous period (HISTORY: the day before)",
        ),
        row(
            "lo md hi xh mx",
            th.fg,
            "effort: low · medium · high · xhigh · max",
        ),
        row(
            "CONTEXT",
            th.fg,
            "share of the model's context window in use",
        ),
        row(
            "IN STATE",
            th.fg,
            "how long the agent has been in its current state",
        ),
        row(
            "index · 12 mo ≈",
            th.fg,
            "swarmy's own database · growth a day · size in a year (warning colour: over budget)",
        ),
        row(
            "—",
            th.dim,
            "unpriced model: no list price known yet (add [[price]] in config.toml)",
        ),
        row(
            "today · 7d",
            th.dim,
            "a seat whose harness has no local quota (Antigravity, Codex): its spend instead of limit gauges",
        ),
        row(
            "cc · agy · cdx",
            th.dim,
            "harness (SWARM, PROJECTS and MODELS HAR, BURN today split; with more than one): Claude Code · Antigravity · Codex",
        ),
        row(
            "shop/fix-1",
            th.dir,
            "a project's linked git worktree (no suffix: the main checkout); g groups by checkout",
        ),
        row(
            "⎇ branch",
            th.faint,
            "the git branch (left out for main/master, and when the worktree's name says it)",
        ),
        row(
            "⇄2",
            th.warn,
            "live sessions working in the same checkout: their edits can collide (a worktree each avoids it)",
        ),
        row(
            "IN WT",
            th.dim,
            "PROJECTS: share of the project's spend made in linked worktrees",
        ),
        head("colours"),
        Line::from(vec![
            Span::styled(fit("limit", 16), th.bold(th.fg)),
            Span::styled("below 60% ", Style::new().fg(th.good)),
            Span::styled("60–85% ", Style::new().fg(th.warn)),
            Span::styled("above 85%", Style::new().fg(th.bad)),
        ]),
        Line::from(vec![
            Span::styled(fit("models", 16), th.bold(th.fg)),
            Span::styled("fable ", Style::new().fg(th.fable)),
            Span::styled("opus ", Style::new().fg(th.opus)),
            Span::styled("sonnet ", Style::new().fg(th.sonnet)),
            Span::styled("haiku ", Style::new().fg(th.haiku)),
            Span::styled("gemini ", Style::new().fg(th.gemini)),
            Span::styled("gpt", Style::new().fg(th.gpt)),
            Span::styled(" (the same in every panel)", Style::new().fg(th.dim)),
        ]),
        Line::from(vec![
            Span::styled(fit("", 16), th.bold(th.fg)),
            Span::styled("gem-4-hydrogen ", Style::new().fg(th.gemini)),
            Span::styled("gem-4-neon", Style::new().fg(th.model_shade(th.gemini, 1))),
            Span::styled(
                " BURN: two models of one family in shades of its colour",
                Style::new().fg(th.dim),
            ),
        ]),
        row("project", th.dir, "project and directory names"),
        row(
            "● ○ ECO",
            th.good,
            "header: data fresh / stale · on battery, refreshes less often",
        ),
        row(
            "│ now",
            th.faint,
            "the limit chart: the vertical line is now, ⊘ where the projection hits 100%",
        ),
        head("words"),
        row(
            "seat",
            th.fg,
            "a harness config/data dir (~/.claude, ~/.claude-work, ~/.gemini/antigravity-cli, ~/.codex…)",
        ),
        row(
            "login",
            th.fg,
            "an OAuth sign-in; a saved login (⚿) lives in the vault and fits its own client's seats",
        ),
        row(
            "account",
            th.fg,
            "the account behind a login; Claude Code limits are per account",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    /// Words the panels put on screen have their legend line (CLAUDE.md: keep them in sync).
    #[test]
    fn the_legend_explains_the_storage_line() {
        let t = tempfile::tempdir().unwrap();
        let app = App::new(Theme::named(None), false, true, Vec::new(), t.path().into());
        let text: Vec<String> = legend_lines(&app)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let line = text.iter().find(|l| l.starts_with("index · 12 mo ≈"));
        assert!(
            line.is_some_and(|l| l.contains("size in a year") && l.contains("over budget")),
            "{text:#?}"
        );
    }

    fn app() -> App {
        let t = tempfile::tempdir().unwrap();
        let mut app = App::new(Theme::named(None), false, true, Vec::new(), t.path().into());
        app.boot = None;
        app
    }

    /// The overlay on a `width` × `height` screen, as rows of text (the cells' symbols).
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        term.draw(|f| {
            let area = f.area();
            help(f, app, area);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn glossary_app(term: usize) -> App {
        let mut a = app();
        a.help = true;
        a.help_page = HelpPage::Glossary;
        a.glossary.term = term;
        a
    }

    fn index_of(name: &str) -> usize {
        TERMS.iter().position(|t| t.name == name).unwrap()
    }

    #[test]
    fn the_glossary_page_lists_every_group_and_opens_the_selected_term() {
        let mut a = glossary_app(index_of("thinking"));
        let rows = screen(&mut a, 130, 60).join("\n");
        for g in Group::ALL {
            assert!(rows.contains(g.title()), "group {} is listed", g.title());
        }
        for t in TERMS {
            assert!(rows.contains(t.name), "term {} is listed", t.name);
        }
        // Its story is open, with the other names and the terms to read next.
        assert!(rows.contains("The agent sent a request"), "{rows}");
        assert!(rows.contains("also called: ⠋, ◌"), "{rows}");
        assert!(
            rows.contains("see also: effort · context · working"),
            "{rows}"
        );
        // The terms around it are closed: one line each, none of their stories.
        assert!(!rows.contains("The agent asked for a tool"), "{rows}");
        assert!(rows.contains("▸ thinking"), "{rows}");
        // Moving the selection closes it and opens the next.
        a.glossary.term = index_of("stalled");
        let rows = screen(&mut a, 130, 60).join("\n");
        assert!(rows.contains("no transcript activity") || rows.contains("has not written"));
        assert!(!rows.contains("The agent sent a request"), "{rows}");
    }

    /// Every term can be reached on a screen far too small for the list, and when it is, its
    /// name, the start of its story and (for the first of a group) the group's title are on it.
    #[test]
    fn every_term_scrolls_into_view_on_a_small_screen() {
        let mut a = glossary_app(0);
        for i in 0..TERMS.len() {
            a.glossary.term = i;
            let rows = screen(&mut a, 100, 16).join("\n");
            let t = &TERMS[i];
            assert!(
                rows.contains(&format!("▸ {}", t.name)),
                "{}: {rows}",
                t.name
            );
            let first_words: Vec<&str> = t.long.split_whitespace().take(3).collect();
            assert!(
                rows.contains(&first_words.join(" ")),
                "{}: the story starts on screen\n{rows}",
                t.name
            );
            if i == 0 || TERMS[i - 1].group != t.group {
                assert!(rows.contains(t.group.title()), "{}: {rows}", t.name);
            }
        }
    }

    /// Wherever the window was, the selected term ends up in it (with its group's title and the
    /// whole story when they fit), and the window moves only when it has to.
    #[test]
    fn scrolling_shows_the_selected_term_and_moves_only_when_it_must() {
        for width in [30, 60, 100] {
            for height in [4, 8, 14, 30, 60] {
                for term in 0..TERMS.len() {
                    let lay = layout(width, term);
                    let Range { start, end } = lay.selected.clone();
                    let titled = matches!(lay.rows[start - 1], Row::Heading(_));
                    let top = if titled { start - 1 } else { start };
                    let max = lay.rows.len().saturating_sub(height);
                    let ctx = format!("{width}x{height} term {term}");
                    for from in [0, 3, max / 2, max, max + 5] {
                        let s = scroll_to_show(&lay, from, height);
                        assert!(s <= max, "{ctx}: from {from} to {s}, at most {max}");
                        assert!(
                            (s..s + height).contains(&start),
                            "{ctx}: the term's row {start} is not in {s}..{}",
                            s + height
                        );
                        if end - top <= height {
                            assert!(
                                s <= top && end <= s + height,
                                "{ctx}: the whole block {top}..{end} fits, window {s}"
                            );
                        }
                        // Already in view (and not past the end of the list): stay.
                        if from <= max && from <= top && end <= from + height {
                            assert_eq!(s, from, "{ctx}: moved without need");
                        }
                    }
                }
            }
        }
    }

    /// On a terminal tall enough the whole list and any one story are on screen at once, at the
    /// standard width and on a wide screen: no term needs a scroll, and the dialog is no taller
    /// than that (no empty rows under the list).
    #[test]
    fn a_tall_terminal_shows_the_list_and_any_story_at_once() {
        for inner in [104 - 2, usize::from(GLOSSARY_WIDE) - 2] {
            let tallest = (0..TERMS.len())
                .map(|i| layout(inner, i).rows.len())
                .max()
                .unwrap();
            assert!(
                tallest <= usize::from(glossary_height()) - 2,
                "{inner} wide: a story is taller than its room ({tallest} rows)"
            );
            assert!(
                tallest + 2 >= usize::from(glossary_height()) - 2,
                "{inner} wide: the dialog has more room than its tallest page ({tallest} rows)"
            );
        }
    }

    /// A narrow page cuts the one-line meanings before the names.
    #[test]
    fn a_narrow_page_keeps_the_names_whole() {
        let th = Theme::named(None);
        let (lines, _) = glossary_view(&th, 58, 60, GlossaryAt::default());
        let rows = text(&lines).join("\n");
        for t in TERMS {
            assert!(rows.contains(t.name), "{} is cut:\n{rows}", t.name);
        }
        // ... and the meanings that do not fit end in an ellipsis.
        assert!(
            rows.contains("running a tool (a command, an edit…"),
            "{rows}"
        );
    }

    #[test]
    fn the_end_of_the_list_scrolls_and_home_comes_back_to_the_top() {
        let th = Theme::named(None);
        let (lines, at) = glossary_view(&th, 100, 14, GlossaryAt::default());
        assert_eq!(at.scroll, 0);
        assert!(text(&lines)[0].contains(Group::States.title()));
        let last = GlossaryAt::default().moved(100_000);
        let (lines, down) = glossary_view(&th, 100, 14, last);
        assert!(
            down.scroll > 0,
            "the end of the list needs a scroll: {down:?}"
        );
        assert!(text(&lines).join("\n").contains("▸ mask"));
        let (lines, home) = glossary_view(&th, 100, 14, GlossaryAt { term: 0, ..down });
        assert_eq!(home.scroll, 0);
        assert!(text(&lines)[0].contains(Group::States.title()));
    }

    /// The drawing writes where it scrolled to back, so the next key starts from what was seen.
    #[test]
    fn drawing_remembers_the_scroll() {
        let mut a = glossary_app(TERMS.len() - 1);
        assert_eq!(a.glossary.scroll, 0);
        screen(&mut a, 100, 16);
        assert!(a.glossary.scroll > 0);
        // A term past the end (the list shrank) is brought back inside it.
        a.glossary.term = 10_000;
        screen(&mut a, 100, 16);
        assert_eq!(a.glossary.term, TERMS.len() - 1);
    }

    /// Fitting narrow widths: whatever the width, no line is wider than the page, nothing
    /// panics, and the selected term and the way to the next page are still there.
    #[test]
    fn every_line_fits_whatever_the_width() {
        let th = Theme::named(None);
        for width in 1..=220usize {
            for term in [0, index_of("api-equivalent cost"), TERMS.len() - 1] {
                let at = GlossaryAt { term, scroll: 0 };
                let (lines, _) = glossary_view(&th, width, 30, at);
                for l in text(&lines) {
                    assert!(w(&l) <= width, "width {width}: {l:?} is {} wide", w(&l));
                }
            }
        }
    }

    #[test]
    fn the_page_fits_the_screen_at_narrow_and_short_sizes() {
        let sizes = [
            (12, 6),
            (20, 10),
            (30, 12),
            (40, 24),
            (60, 24),
            (80, 30),
            (103, 40),
            (104, 40),
            (169, 50),
            (170, 50),
            (220, 90),
        ];
        for (width, height) in sizes {
            for page in [HelpPage::Keys, HelpPage::Legend, HelpPage::Glossary] {
                let mut a = glossary_app(index_of("login"));
                a.help_page = page;
                // `screen` is the whole terminal: a cell outside it would panic in the buffer.
                let rows = screen(&mut a, width, height);
                assert_eq!(rows.len(), height as usize);
                assert!(
                    rows.iter().all(|r| w(r) == width as usize),
                    "{width}x{height} {page:?}"
                );
            }
        }
        // From 20 columns the dialog's title and footer name the page and the way on.
        for width in 20..=120u16 {
            let mut a = glossary_app(0);
            let rows = screen(&mut a, width, 24);
            let (top, bottom) = (
                rows.iter().find(|r| r.contains('┏')).unwrap(),
                rows.iter().rfind(|r| r.contains('┗')).unwrap(),
            );
            assert!(top.contains("KEYS") || width < 30, "{width}: {top}");
            assert!(bottom.contains("Tab"), "{width}: {bottom}");
        }
    }

    #[test]
    fn the_hint_is_the_longest_wording_that_fits() {
        let th = Theme::named(None);
        for page in [HelpPage::Keys, HelpPage::Glossary] {
            let mut last = 0;
            for room in 0..200 {
                let line = hint_line(page, &th, room);
                let width = line.width();
                // Wider rooms never take a shorter wording; it fits unless even the
                // shortest one does not.
                assert!(width >= last, "{page:?} {room}: {width} < {last}");
                last = width;
                // (the shortest wording is `Tab pages`, with a space on each side)
                assert!(width <= room.max(11), "{page:?} {room}: {width} wide");
            }
        }
        let full: String = text(&[hint_line(HelpPage::Glossary, &th, 200)])[0].clone();
        for want in ["↑↓ j k term", "PgUp PgDn", "Tab → next page", "Esc close"] {
            assert!(full.contains(want), "{full}");
        }
        let short: String = text(&[hint_line(HelpPage::Glossary, &th, 14)])[0].clone();
        assert!(short.contains("Tab pages"), "{short}");
    }

    #[test]
    fn the_title_names_the_pages_and_the_footer_how_to_turn_them() {
        let mut a = glossary_app(4);
        let rows = screen(&mut a, 130, 40);
        let top = rows.iter().find(|r| r.contains('┏')).unwrap();
        assert!(top.contains("KEYS"), "{top}");
        assert!(top.contains("LEGEND"), "{top}");
        assert!(top.contains("GLOSSARY"), "{top}");
        assert!(top.contains("5/"), "the selected term's place: {top}");
        let bottom = rows.iter().rfind(|r| r.contains('┗')).unwrap();
        assert!(bottom.contains("Tab"), "{bottom}");
        // On a wide screen keys and legend are one page in the title.
        let rows = screen(&mut a, 200, 60);
        let top = rows.iter().find(|r| r.contains('┏')).unwrap();
        assert!(
            top.contains("KEYS · LEGEND") && top.contains("GLOSSARY"),
            "{top}"
        );
        assert!(a.help_wide);
    }

    /// Drawing the keys page on a wide screen settles `help_wide` for Tab, and turns a legend
    /// page left over from a narrow screen into the keys.
    #[test]
    fn drawing_tells_tab_how_the_pages_are_laid_out() {
        let mut a = app();
        a.help = true;
        a.help_page = HelpPage::Legend;
        screen(&mut a, 200, 60);
        assert!(a.help_wide);
        assert_eq!(a.help_page, HelpPage::Keys);
        screen(&mut a, 120, 60);
        assert!(!a.help_wide);
    }

    #[test]
    fn the_keys_page_lists_the_help_keys() {
        let mut a = app();
        a.help = true;
        let rows = screen(&mut a, 120, 60).join("\n");
        assert!(rows.contains("help: Tab"), "{rows}");
        assert!(rows.contains("glossary"), "{rows}");
    }

    /// A page of the help asks nothing of the animation clock, so the dashboard's frame rate
    /// and its kept panels stay what they were without the overlay.
    #[test]
    fn the_overlay_does_not_read_the_clock_or_disturb_kept_panels() {
        let mut a = app();
        a.help = true;
        for page in [HelpPage::Keys, HelpPage::Legend, HelpPage::Glossary] {
            a.help_page = page;
            a.anim_used.set(false);
            screen(&mut a, 140, 50);
            assert!(!a.anim_used.get(), "{page:?} read the animation clock");
        }
        // The same keys give the same frame.
        a.help_page = HelpPage::Glossary;
        assert_eq!(screen(&mut a, 140, 50), screen(&mut a, 140, 50));

        // Panels kept between frames stay kept under the open help.
        let mut b = app();
        b.cache_panels = true;
        b.paused = true;
        let draw = |b: &mut App| {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 60)).unwrap();
            term.draw(|f| crate::tui::view::draw(f, b)).unwrap();
        };
        draw(&mut b);
        let kept = b.shots.len();
        assert!(kept > 0, "panels are kept without animation");
        b.help = true;
        b.help_page = HelpPage::Glossary;
        draw(&mut b);
        draw(&mut b);
        assert_eq!(
            b.shots.len(),
            kept,
            "the overlay left the kept panels alone"
        );
    }
}

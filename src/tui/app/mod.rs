//! Dashboard state and input.
//!
//! The screen is a set of panels. On a large terminal they are all visible at once (the "wall");
//! on a medium one some are stacked in a column; on a small one they are grouped into tabs. One
//! panel has the keyboard focus (Tab, or its number); `z` zooms it to the whole screen.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Local, Utc};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::Color;

mod keys;
mod limits;

use super::alerts;
pub use super::alerts::{Attn, AttnTarget, Tone};
use super::theme::Theme;
use super::{Analytics, Msg, SystemView};
use crate::claude::registry::LiveSession;
use crate::claude::seats::Seat;
use crate::core::harness::Harness;
use crate::core::limits::WindowKind;
use crate::core::period::Period;
use crate::index::Progress;
use crate::index::query::Dim;
use crate::live::{Agent, Snapshot, ToolLine};
use crate::ui::fmt;
use crate::vault::{Vault, VaultTx};

/// Every panel; the number is the key that focuses it and appears in its title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Panel {
    Swarm,
    Limits,
    Burn,
    Attention,
    Agent,
    Projects,
    Models,
    History,
    Efficiency,
    System,
}

impl Panel {
    pub const ALL: [Panel; 10] = [
        Panel::Swarm,
        Panel::Limits,
        Panel::Burn,
        Panel::Attention,
        Panel::Agent,
        Panel::Projects,
        Panel::Models,
        Panel::History,
        Panel::Efficiency,
        Panel::System,
    ];

    pub fn number(self) -> u8 {
        match self {
            Panel::Swarm => 1,
            Panel::Limits => 2,
            Panel::Burn => 3,
            Panel::Attention => 4,
            Panel::Agent => 5,
            Panel::Projects => 6,
            Panel::Models => 7,
            Panel::History => 8,
            Panel::Efficiency => 9,
            Panel::System => 0,
        }
    }

    pub fn from_number(n: u8) -> Panel {
        Panel::ALL
            .into_iter()
            .find(|p| p.number() == n)
            .unwrap_or(Panel::Swarm)
    }

    pub fn title(self) -> &'static str {
        match self {
            Panel::Swarm => "SWARM",
            Panel::Limits => "LIMITS · SEATS",
            Panel::Burn => "BURN",
            Panel::Attention => "ATTENTION",
            Panel::Agent => "AGENT",
            Panel::Projects => "PROJECTS",
            Panel::Models => "MODELS",
            Panel::History => "HISTORY",
            Panel::Efficiency => "EFFICIENCY",
            Panel::System => "SYSTEM",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Auto,
    Wall,
    Column,
    Tabs,
}

/// The arrangement actually drawn, chosen from the terminal size (or forced with `L`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Wall,
    Column,
    Tabs,
}

impl Shape {
    pub fn pick(mode: LayoutMode, area: Rect) -> Shape {
        match mode {
            LayoutMode::Wall => Shape::Wall,
            LayoutMode::Column => Shape::Column,
            LayoutMode::Tabs => Shape::Tabs,
            // Everything on one screen as soon as panels can be readable; they shrink, scroll and
            // zoom rather than hide.
            LayoutMode::Auto if area.width >= 180 && area.height >= 45 => Shape::Wall,
            LayoutMode::Auto if area.width >= 110 && area.height >= 40 => Shape::Column,
            LayoutMode::Auto => Shape::Tabs,
        }
    }
}

/// Screens of the tabs arrangement (small terminals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Ops,
    Agent,
    Limits,
    Spend,
    System,
}

pub const TABS: [(Tab, &str, Panel); 5] = [
    (Tab::Ops, "OPS", Panel::Swarm),
    (Tab::Agent, "AGENT", Panel::Agent),
    (Tab::Limits, "LIMITS", Panel::Limits),
    (Tab::Spend, "SPEND", Panel::Projects),
    (Tab::System, "SYSTEM", Panel::System),
];

#[derive(Debug, Clone)]
pub struct EventLine {
    pub at: DateTime<Utc>,
    pub glyph: &'static str,
    pub text: String,
    pub tone: Tone,
    pub born: Instant,
}

pub struct Boot {
    pub started: Instant,
    pub progress: Progress,
    pub ready: Option<(usize, usize, usize)>,
    pub ready_at: Option<Instant>,
    pub skip: bool,
}

/// A panel's cells as last drawn, with what decides whether they are still right.
pub struct Shot {
    pub key: ShotKey,
    pub cells: Vec<ratatui::buffer::Cell>,
    /// Cells that pulse or spin, patched on every frame.
    pub marks: Vec<Mark>,
    /// The blink's phase the panel was drawn in, when it blinks: drawn again when it flips.
    pub blink: Option<bool>,
    /// The mouse rects the panel registered.
    pub rects: Vec<(Panel, Rect)>,
}

/// A cell of a kept panel that moves: its place and what moves in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mark {
    pub x: u16,
    pub y: u16,
    pub what: Moves,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Moves {
    /// The cell's foreground or background pulses around this colour at this rate.
    Fg(ratatui::style::Color, f64),
    Bg(ratatui::style::Color, f64),
    /// The cell shows the thinking spinner.
    Spinner,
}

/// A drawn panel stays right while its place, the app's state (`App::rev`) and the clock's
/// second are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShotKey {
    pub area: Rect,
    pub big: bool,
    pub rev: u64,
    pub second: i64,
    pub eco: bool,
}

/// One row of the LIMITS · SEATS panel (section headings are drawn between them, never
/// selected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitsRow {
    /// A seat, by label.
    Seat(String),
    /// A saved login, by id (a label names one login per harness).
    Login(String),
}

pub struct App {
    pub theme: Theme,
    pub mask: bool,
    /// SWARM shows activity lanes (true) or the spend sparkline (false); `v` toggles.
    pub lanes: bool,
    pub paused: bool,
    pub eco: bool,
    pub bell: bool,
    pub layout: LayoutMode,
    pub shape: Shape,
    pub tab: Tab,
    pub focus: Panel,
    pub zoom: Option<Panel>,
    pub help: bool,
    /// Help page on small screens: 0 keys, 1 legend (both side by side on wide ones).
    pub help_page: u8,
    /// The ATTENTION item `n`/`N` went to last; the first press starts at the top.
    attn_jumped: Option<usize>,
    pub boot: Option<Boot>,
    pub started: Instant,
    pub snapshot: Snapshot,
    pub live: Vec<LiveSession>,
    pub system: SystemView,
    pub analytics: Analytics,
    pub timeline: (String, Vec<ToolLine>, Option<String>),
    /// The selected agent's requests (key, points).
    pub life: (String, Vec<crate::live::LifePoint>),
    pub events: VecDeque<EventLine>,
    prev_states: alerts::Seen,
    /// The selected agent, by key: rows move, the selection does not.
    pub selected_key: Option<String>,
    /// Per-panel cursor (lists) or scroll offset (text).
    pub cursor: HashMap<Panel, usize>,
    pub period: Period,
    pub group: Dim,
    pub command: Option<String>,
    pub filter: Option<String>,
    pub filter_edit: bool,
    pub toast: Option<(String, Tone, Instant)>,
    pub error: Option<String>,
    pub alerts: HashMap<String, Instant>,
    /// Where each panel was drawn last frame, for the mouse.
    pub rects: Vec<(Panel, Rect)>,
    /// Bumped by every message and input event: a panel drawn at the same `rev` (and second)
    /// draws the same cells, unless it animates (`view::draw_panel`).
    pub rev: u64,
    /// Panels drawn without animation, kept and pasted until something changes (the live
    /// terminal only: `cache_panels`).
    pub shots: HashMap<Panel, Shot>,
    pub cache_panels: bool,
    /// Set when drawing asked for the animation clock (`t`, `animating`).
    pub anim_used: std::cell::Cell<bool>,
    /// While a panel that may be kept is drawn, pulses and spinners leave marks instead of a
    /// colour or a frame of the clock (`view::kit::pulse`), patched in on every frame.
    pub marking: std::cell::Cell<bool>,
    /// Set when a panel drawn while marking blinked (`view::kit::blink`).
    pub blink_used: std::cell::Cell<bool>,
    /// The pulses (colour, Hz) marked while drawing the current panel.
    pub pulses: std::cell::RefCell<Vec<(ratatui::style::Color, f64)>>,
    /// The animation clock stopped at this time (tests).
    pub t_fixed: Option<f64>,
    analytics_request: bool,
    /// `.`: LIMITS · SEATS shows the sections of empty seats too.
    pub show_hidden: bool,
    pub(super) seats: Vec<Seat>,
    pub(super) vault_dir: PathBuf,
    pub vault: super::vault_ui::VaultUi,
}

impl App {
    pub fn new(
        theme: Theme,
        mask: bool,
        no_boot: bool,
        seats: Vec<Seat>,
        vault_dir: PathBuf,
    ) -> App {
        let skip = no_boot || !theme.animate;
        App {
            theme,
            mask,
            lanes: true,
            paused: false,
            eco: false,
            bell: true,
            layout: LayoutMode::Auto,
            shape: Shape::Wall,
            tab: Tab::Ops,
            focus: Panel::Swarm,
            zoom: None,
            help: false,
            help_page: 0,
            attn_jumped: None,
            boot: Some(Boot {
                started: Instant::now(),
                progress: Progress::default(),
                ready: None,
                ready_at: None,
                skip,
            }),
            started: Instant::now(),
            snapshot: Snapshot::default(),
            live: Vec::new(),
            system: SystemView::default(),
            analytics: Analytics::default(),
            timeline: (String::new(), Vec::new(), None),
            life: (String::new(), Vec::new()),
            events: VecDeque::new(),
            prev_states: HashMap::new(),
            selected_key: None,
            cursor: HashMap::new(),
            period: Period::Days(7),
            group: Dim::Project,
            command: None,
            filter: None,
            filter_edit: false,
            toast: None,
            error: None,
            alerts: HashMap::new(),
            rects: Vec::new(),
            rev: 0,
            shots: HashMap::new(),
            cache_panels: false,
            anim_used: std::cell::Cell::new(false),
            marking: std::cell::Cell::new(false),
            blink_used: std::cell::Cell::new(false),
            pulses: Default::default(),
            t_fixed: None,
            analytics_request: false,
            show_hidden: false,
            seats,
            vault_dir,
            vault: Default::default(),
        }
    }

    pub fn animating(&self) -> bool {
        let on = self.anim_on();
        if on {
            self.anim_used.set(true);
        }
        on
    }

    /// Whether things move, without counting as a use of the clock (`animating` does).
    pub fn anim_on(&self) -> bool {
        !self.paused && self.theme.animate
    }

    pub fn t(&self) -> f64 {
        self.anim_used.set(true);
        self.clock_now()
    }

    /// The animation clock, without counting as a use of it.
    pub fn clock_now(&self) -> f64 {
        self.t_fixed
            .unwrap_or_else(|| self.started.elapsed().as_secs_f64())
    }

    pub fn request_analytics(&mut self) {
        self.analytics_request = true;
    }

    pub fn take_analytics_request(&mut self) -> bool {
        std::mem::take(&mut self.analytics_request)
    }

    pub fn cur(&self, p: Panel) -> usize {
        self.cursor.get(&p).copied().unwrap_or(0)
    }

    fn set_cur(&mut self, p: Panel, v: usize) {
        self.cursor.insert(p, v);
    }

    // ───────────── agents ─────────────

    /// Filter chips: `p:project m:model s:seat h:harness state:needs`, anything else matches names.
    pub fn visible_agents(&self) -> Vec<&Agent> {
        let f = self.filter.as_deref().unwrap_or("").to_lowercase();
        let terms: Vec<&str> = f.split_whitespace().collect();
        self.snapshot
            .agents
            .iter()
            .filter(|a| {
                terms.iter().all(|t| match t.split_once(':') {
                    Some(("p", v)) => a.project.to_lowercase().contains(v),
                    Some(("m", v)) => a.model.as_deref().unwrap_or("").to_lowercase().contains(v),
                    Some(("s", v)) => a.seat_label.to_lowercase().contains(v),
                    Some(("h", v)) => a.harness.key().contains(v) || a.harness.short().contains(v),
                    Some(("state", v)) => format!("{:?}", a.state).to_lowercase().contains(v),
                    // Not a chip: a word matched against names.
                    Some((_, _)) | None => {
                        a.name.to_lowercase().contains(t) || a.project.to_lowercase().contains(t)
                    }
                })
            })
            .collect()
    }

    /// True when more than one harness is present in the seats or live snapshot (so harness badges
    /// add signal without cluttering Claude-only setups).
    pub fn multi_harness(&self) -> bool {
        let mut first = None;
        for h in self
            .seats
            .iter()
            .map(|s| s.harness)
            .chain(self.snapshot.agents.iter().map(|a| a.harness))
        {
            match first {
                None => first = Some(h),
                Some(f) if f != h => return true,
                Some(_) => {}
            }
        }
        false
    }

    pub fn selected_index(&self) -> usize {
        let v = self.visible_agents();
        match &self.selected_key {
            Some(k) => v.iter().position(|a| a.key() == *k).unwrap_or(0),
            None => 0,
        }
    }

    pub fn selected_agent(&self) -> Option<&Agent> {
        let v = self.visible_agents();
        v.get(self.selected_index()).copied()
    }

    fn move_agent(&mut self, delta: isize) {
        let keys: Vec<String> = self.visible_agents().iter().map(|a| a.key()).collect();
        if keys.is_empty() {
            return;
        }
        let i = self.selected_index() as isize + delta;
        let i = i.clamp(0, keys.len() as isize - 1) as usize;
        self.selected_key = Some(keys[i].clone());
        self.cursor.insert(Panel::Agent, 0);
    }

    pub fn select_agent(&mut self, key: &str) {
        self.selected_key = Some(key.to_string());
        self.filter = None;
        self.cursor.insert(Panel::Agent, 0);
    }

    // ───────────── accounts ─────────────

    /// The 5-hour and weekly estimate of an account, with the weekly reset.
    pub fn limits_of(
        &self,
        account: Option<&str>,
    ) -> (Option<f64>, Option<f64>, Option<DateTime<Utc>>) {
        let Some(a) = account else {
            return (None, None, None);
        };
        let Some(rep) = self
            .analytics
            .limits
            .iter()
            .find(|r| r.account.as_str() == a)
        else {
            return (None, None, None);
        };
        let five = rep
            .windows
            .iter()
            .find(|w| w.reading.kind == WindowKind::FiveHour)
            .map(|w| w.estimate_now.get());
        let week = rep
            .windows
            .iter()
            .find(|w| w.reading.kind == WindowKind::SevenDay);
        (
            five,
            week.map(|w| w.estimate_now.get()),
            week.map(|w| w.window.end),
        )
    }

    pub fn account_label(&self, uuid: &str) -> String {
        self.analytics
            .limits
            .iter()
            .find(|r| r.account.as_str() == uuid)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| format!("acct-{}", crate::core::ids::short_account(uuid)))
    }

    // ───────────── attention ─────────────

    /// What needs a look now (`alerts::attention` over what the dashboard holds).
    pub fn attention(&self) -> Vec<Attn> {
        alerts::attention(&alerts::Scene {
            snapshot: &self.snapshot,
            limits: &self.analytics.limits,
            seats: &self.analytics.seats,
            logins: &self.vault.logins,
            problems: &self.analytics.problems,
            mask: self.mask,
            now: Utc::now(),
        })
    }

    fn jump_attention(&mut self, delta: isize) {
        let items = self.attention();
        if items.is_empty() {
            self.toast("nothing needs attention", Tone::Good);
            return;
        }
        let i = next_attention(
            self.attn_jumped,
            self.cur(Panel::Attention),
            delta,
            items.len(),
        );
        self.attn_jumped = Some(i);
        self.set_cur(Panel::Attention, i);
        self.act_on_attention(&items[i]);
    }

    fn act_on_attention(&mut self, a: &Attn) {
        match &a.target {
            AttnTarget::Agent(k) => {
                self.select_agent(k);
                self.focus_panel(Panel::Swarm);
            }
            AttnTarget::Limits => self.focus_panel(Panel::Limits),
            AttnTarget::None => {}
        }
    }

    // ───────────── messages ─────────────

    pub(super) fn log_event(&mut self, glyph: &'static str, text: String, tone: Tone) {
        self.events.push_front(EventLine {
            at: Utc::now(),
            glyph,
            text,
            tone,
            born: Instant::now(),
        });
        self.events.truncate(300);
    }

    fn ring(&self) {
        if self.bell {
            print!("\x07");
        }
    }

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Progress(p) => {
                if let Some(b) = &mut self.boot {
                    b.progress = p;
                }
            }
            Msg::Ready {
                seats,
                accounts,
                files,
            } => {
                if let Some(b) = &mut self.boot {
                    b.ready = Some((seats, accounts, files));
                    b.ready_at = Some(Instant::now());
                }
                self.log_event(
                    "◆",
                    format!(
                        "online · {seats} seats{} · {} transcripts",
                        if accounts > 0 {
                            format!(" · {accounts} accounts")
                        } else {
                            String::new()
                        },
                        fmt::count(files as u64)
                    ),
                    Tone::Accent,
                );
            }
            Msg::Snapshot(s, live, sys) => {
                self.diff_events(&s);
                self.snapshot = *s;
                self.live = live;
                self.system = *sys;
                if self.selected_key.is_none()
                    || self
                        .selected_agent()
                        .is_none_or(|a| Some(a.key()) != self.selected_key)
                {
                    // Keep the selection when its agent is still listed, otherwise take the first.
                    let keep = self
                        .selected_key
                        .as_ref()
                        .is_some_and(|k| self.visible_agents().iter().any(|a| a.key() == *k));
                    if !keep {
                        self.selected_key = self.visible_agents().first().map(|a| a.key());
                    }
                }
            }
            Msg::Analytics(a) => {
                self.limit_alerts(&a);
                // Seats judged empty and logins reordered: the selection stays on its row.
                self.keep_limits_row(|app| {
                    if app.vault.modal.is_none() && app.vault.picker.is_none() {
                        app.vault.logins = a.logins.clone();
                        app.vault.loaded = true;
                    }
                    app.analytics = *a;
                });
            }
            Msg::Timeline(k, t, last, life) => {
                self.life = (k.clone(), life);
                self.timeline = (k, t, last);
            }
            Msg::Past(p) => {
                // Old news: no highlight.
                let born = Instant::now()
                    .checked_sub(std::time::Duration::from_secs(600))
                    .unwrap_or_else(Instant::now);
                for e in p {
                    let alerts::Notice {
                        glyph, text, tone, ..
                    } = alerts::past_notice(e.kind);
                    self.events.push_back(EventLine {
                        at: e.at,
                        glyph,
                        text,
                        tone,
                        born,
                    });
                }
                self.events
                    .make_contiguous()
                    .sort_by_key(|e| std::cmp::Reverse(e.at));
                self.events.truncate(300);
            }
            Msg::Error(e) => self.error = Some(e),
        }
    }

    /// Logs the notices in order, highlights what they name, rings once if asked.
    fn announce(&mut self, ev: alerts::Events) {
        for n in ev.notices {
            self.log_event(n.glyph, n.text, n.tone);
            if let Some(k) = n.flash {
                self.alerts.insert(k, Instant::now());
            }
        }
        if ev.ring {
            self.ring();
        }
    }

    fn diff_events(&mut self, s: &Snapshot) {
        let (ev, seen) = alerts::agent_events(&self.prev_states, &self.snapshot, s);
        self.announce(ev);
        self.prev_states = seen;
    }

    fn limit_alerts(&mut self, a: &Analytics) {
        let ev = alerts::limit_events(&self.analytics.limits, &a.limits);
        self.announce(ev);
    }

    pub fn toast(&mut self, msg: impl Into<String>, tone: Tone) {
        self.toast = Some((msg.into(), tone, Instant::now()));
    }

    pub fn tone_color(&self, t: Tone) -> Color {
        match t {
            Tone::Info => self.theme.dim,
            Tone::Good => self.theme.good,
            Tone::Warn => self.theme.warn,
            Tone::Bad => self.theme.bad,
            Tone::Accent => self.theme.accent,
        }
    }

    // ───────────── focus ─────────────

    /// Panels on screen in the current arrangement, in Tab order.
    pub fn visible_panels(&self) -> Vec<Panel> {
        if let Some(z) = self.zoom {
            return vec![z];
        }
        match self.shape {
            Shape::Wall => vec![
                Panel::Swarm,
                Panel::Agent,
                Panel::Limits,
                Panel::Attention,
                Panel::Burn,
                Panel::Projects,
                Panel::Models,
                Panel::History,
                Panel::Efficiency,
            ],
            Shape::Column => vec![
                Panel::Swarm,
                Panel::Agent,
                Panel::Limits,
                Panel::Attention,
                Panel::Burn,
                Panel::Projects,
                Panel::Efficiency,
                Panel::History,
                Panel::Models,
            ],
            Shape::Tabs => match self.tab {
                Tab::Ops => vec![Panel::Swarm, Panel::Limits, Panel::Attention],
                Tab::Agent => vec![Panel::Swarm, Panel::Agent],
                Tab::Limits => vec![Panel::Limits],
                Tab::Spend => vec![
                    Panel::Projects,
                    Panel::Efficiency,
                    Panel::Models,
                    Panel::History,
                ],
                Tab::System => vec![Panel::System],
            },
        }
    }

    /// Focus a panel; if it is not on screen, zoom it (or switch to the tab that has it).
    pub fn focus_panel(&mut self, p: Panel) {
        self.focus = p;
        if self.zoom.is_some() && self.zoom != Some(p) {
            self.zoom = None;
        }
        if self.shape == Shape::Tabs {
            if !self.visible_panels().contains(&p) {
                self.tab = match p {
                    Panel::Agent => Tab::Agent,
                    Panel::Limits | Panel::Burn => Tab::Limits,
                    Panel::Projects | Panel::Models | Panel::History => Tab::Spend,
                    Panel::System => Tab::System,
                    _ => Tab::Ops,
                };
            }
            if !self.visible_panels().contains(&p) {
                self.zoom = Some(p);
            }
        } else if !self.visible_panels().contains(&p) {
            self.zoom = Some(p);
        }
        if p == Panel::Limits && !self.vault.loaded {
            self.vault_reload();
        }
    }

    fn cycle_focus(&mut self, delta: isize) {
        let v = self.visible_panels();
        let i = v.iter().position(|p| *p == self.focus).unwrap_or(0) as isize;
        self.focus = v[(i + delta).rem_euclid(v.len() as isize) as usize];
    }

    pub fn clock(&self) -> String {
        Local::now().format("%H:%M:%S").to_string()
    }
}

/// Where `n` (delta 1) or `N` (delta -1) goes: the first press of a walk starts at the top (or
/// the bottom for `N`); after that it moves on from the item it went to, wrapping around. A
/// cursor moved by hand since then starts a new walk.
fn next_attention(jumped: Option<usize>, cur: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if let Some(j) = jumped.filter(|j| *j == cur && *j < len) {
        let len = isize::try_from(len).unwrap_or(isize::MAX);
        let j = isize::try_from(j).unwrap_or(0);
        return usize::try_from((j + delta).rem_euclid(len)).unwrap_or(0);
    }
    if delta < 0 { len - 1 } else { 0 }
}

#[cfg(test)]
mod attention_walk_tests {
    use super::next_attention;

    #[test]
    fn the_first_press_goes_to_the_top_then_walks_and_wraps() {
        assert_eq!(
            next_attention(None, 0, 1, 3),
            0,
            "first n: the most urgent item"
        );
        assert_eq!(next_attention(Some(0), 0, 1, 3), 1);
        assert_eq!(next_attention(Some(2), 2, 1, 3), 0, "wraps");
        assert_eq!(next_attention(None, 0, -1, 3), 2, "first N: the last item");
        assert_eq!(
            next_attention(Some(1), 2, 1, 3),
            0,
            "cursor moved by hand: start again"
        );
        assert_eq!(next_attention(Some(5), 5, 1, 3), 0, "the list shrank");
    }
}

//! The live dashboard.
//!
//! A worker thread (`worker.rs`) owns the index: every tick it reads what running sessions
//! wrote and builds the live snapshot; heavier parts and the analytics (`analytics.rs`) run on
//! the cadences declared in `jobs.rs`. The UI thread only draws and handles keys, so animations
//! stay smooth while transcripts are being read.

mod alerts;
mod analytics;
mod app;
mod bigdigits;
mod boot;
mod jobs;
mod theme;
mod vault_ui;
mod view;
mod worker;

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;

use crate::app::context::Ctx;
use crate::claude::registry::LiveSession;
use crate::core::period::Period;
use crate::index::Progress;
use crate::index::query::{Dim, Row};
use crate::limits::AccountReport;
use crate::live::{self, Snapshot, ToolLine};
use crate::vault::Login;

pub use app::App;

/// `SWARMY_DEBUG=1` prints timings to stderr (read once).
fn debug() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var_os("SWARMY_DEBUG").is_some())
}

/// Heavier, slower-changing numbers for the analytics screens.
#[derive(Debug, Clone, Default)]
pub struct Analytics {
    pub period: Option<Period>,
    pub group: Option<Dim>,
    pub groups: Vec<Row>,
    pub by_model: Vec<Row>,
    pub total: Option<Row>,
    pub days: Vec<Row>,
    /// Day × model spend for the daily bars (14 days).
    pub days_model: Vec<(String, String, f64)>,
    pub heat: Vec<(String, String, f64)>,
    /// Where time and money go (the EFFICIENCY panel).
    pub efficiency: Option<crate::index::insights::Efficiency>,
    /// Spend of the period before this one, for ▲▼ trends (none for "all").
    pub prev_total: Option<f64>,
    pub prev_models: Vec<(String, f64)>,
    /// Which harnesses ran each model in the period (model, harness key); empty when only one
    /// harness has seats (MODELS shows the column only then).
    pub model_harness: Vec<(String, String)>,
    /// When the slow parts were computed (efficiency, the previous period).
    pub efficiency_at: Option<Instant>,
    pub prev_at: Option<Instant>,
    /// Monthly subscription prices from the config, by account label or uuid.
    pub subscriptions: Vec<(String, f64)>,
    pub effort: Vec<(String, String, f64)>,
    pub substitutions: Vec<(String, String, u64)>,
    pub project_model: Vec<(String, String, f64)>,
    /// Per project: spend in the last hour, today and in the period.
    pub projects: Vec<ProjectRow>,
    pub limits: Vec<AccountReport>,
    pub logins: Vec<Login>,
    pub seats: Vec<SeatRow>,
    /// What could not be read (`Ctx::problems`, the vault): shown in ATTENTION.
    pub problems: Vec<String>,
    pub transcripts_bytes: Vec<(String, u64)>,
    pub index_bytes: u64,
    /// The index's size, growth and 12-month projection (SYSTEM), and the budget it is held to.
    pub storage: Option<crate::core::storage::Outlook>,
    pub storage_budget: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectRow {
    pub key: crate::core::ids::ProjectKey,
    pub name: String,
    pub hour: f64,
    pub today: f64,
    pub period: f64,
    pub requests: u64,
    /// Spend in the previous period of the same length.
    pub prev: f64,
    /// The part of `period` spent in linked git worktrees.
    pub in_worktrees: f64,
    /// `period` per harness key (with more than one harness), the largest first: one project
    /// worked on from Claude Code, Antigravity, ….
    pub by_harness: Vec<(String, f64)>,
}

#[derive(Debug, Clone)]
pub struct SeatRow {
    pub id: crate::core::ids::SeatId,
    /// For display; unique within one run.
    pub label: String,
    pub harness: crate::core::harness::Harness,
    /// The confirmed account (None while a new login is unconfirmed).
    pub account: Option<String>,
    pub login: Option<String>,
    /// The account's email, naming an account no limit report or login names (Codex);
    /// `--mask` hides it when drawn.
    pub email: Option<String>,
    pub live: usize,
    pub today_cost: f64,
    pub today_requests: u64,
    /// Spend of the last 7 days (shown for seats whose harness has no local quota).
    pub week_cost: f64,
    pub state: crate::vault::SeatState,
}

/// (seat, session name, pid, cpu %, bytes, top children (cmd, cpu, bytes))
pub type Instance = (String, String, u32, f32, u64, Vec<(String, f32, u64)>);

#[derive(Debug, Clone, Default)]
pub struct SystemView {
    pub instances: Vec<Instance>,
    pub mem_total: u64,
    pub mem_available: u64,
    pub disk_free: Option<u64>,
}

pub enum Msg {
    Progress(Progress),
    Ready {
        seats: usize,
        accounts: usize,
        files: usize,
    },
    Snapshot(Box<Snapshot>, Vec<LiveSession>, Box<SystemView>),
    Analytics(Box<Analytics>),
    /// Tool calls and the last thing the agent said.
    Timeline(String, Vec<ToolLine>, Option<String>, Vec<live::LifePoint>),
    /// What happened before launch, sent once.
    Past(Vec<live::PastEvent>),
    Error(String),
}

/// What the UI asks of the worker.
#[derive(Debug, Clone, Default)]
pub struct Wants {
    pub period: Option<Period>,
    pub group: Option<Dim>,
    pub agent: Option<live::Agent>,
    pub refresh_analytics: bool,
    /// The SYSTEM screen is open: keep the full process table fresh.
    pub system: bool,
}

pub fn run(ctx: Ctx, no_boot: bool, force_eco: bool) -> anyhow::Result<()> {
    let (tx, rx) = channel::<Msg>();
    let wants = Arc::new(Mutex::new(Wants {
        period: Some(Period::Days(7)),
        group: Some(Dim::Project),
        ..Default::default()
    }));
    let w2 = wants.clone();
    let mask = ctx.mask;
    let theme = theme::Theme::named(ctx.config.theme);
    let seats = ctx.seats.clone();
    let vault_dir = ctx.dirs.vault_dir();
    let eco = Arc::new(std::sync::atomic::AtomicBool::new(
        force_eco || crate::power::on_battery(),
    ));
    let eco2 = eco.clone();
    std::thread::spawn(move || worker::worker(ctx, tx, w2, eco2, force_eco));

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let mut app = App::new(theme, mask, no_boot, seats, vault_dir);
    app.cache_panels = true;
    let res = ui_loop(&mut terminal, &mut app, &rx, &wants, &eco);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn ui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &Receiver<Msg>,
    wants: &Arc<Mutex<Wants>>,
    eco: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<()> {
    loop {
        let started = Instant::now();
        app.eco = eco.load(std::sync::atomic::Ordering::Relaxed);
        let frame_time = if app.boot.is_some() {
            jobs::FRAME_BOOT
        } else if !app.animating() || app.eco {
            jobs::FRAME_CALM
        } else {
            jobs::FRAME_ANIMATED
        };
        while let Ok(m) = rx.try_recv() {
            app.on_msg(m);
            app.rev += 1;
        }
        let td = Instant::now();
        terminal.draw(|f| view::draw(f, app))?;
        if debug() {
            eprintln!("draw {:?}", td.elapsed());
        }
        let timeout = frame_time
            .saturating_sub(started.elapsed())
            .max(jobs::MIN_POLL);

        if event::poll(timeout)? {
            app.rev += 1;
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if app.on_key(k) {
                        return Ok(());
                    }
                    if let Some(vault_ui::External::AddLogin(label, harness)) =
                        app.vault.external.take()
                    {
                        // Hand the terminal to the harness for its sign-in, then take it back.
                        let _ = execute!(std::io::stdout(), DisableMouseCapture);
                        ratatui::restore();
                        app.run_add_login(&label, harness);
                        *terminal = ratatui::init();
                        let _ = execute!(std::io::stdout(), EnableMouseCapture);
                        terminal.clear()?;
                    }
                }
                Event::Mouse(m) => app.on_mouse(m),
                Event::Resize(..) => {}
                _ => {}
            }
        }
        sync_wants(app, wants);
    }
}

/// The UI's wishes, shared with the worker. They are plain values that are always whole, so a
/// thread that panicked while holding the lock leaves nothing half-written: the lock is taken
/// back instead of failing.
pub(crate) fn lock(w: &Mutex<Wants>) -> std::sync::MutexGuard<'_, Wants> {
    w.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Tells the worker what the UI shows: the selected agent (for its timeline), the period…
fn sync_wants(app: &mut App, wants: &Arc<Mutex<Wants>>) {
    let mut w = lock(wants);
    w.period = Some(app.period.clone());
    w.group = Some(app.group);
    let sel = app.selected_agent().cloned();
    if w.agent.as_ref().map(|a| a.key()) != sel.as_ref().map(|a| a.key()) {
        w.agent = sel;
    }
    w.system = app.zoom == Some(app::Panel::System) || app.focus == app::Panel::System;
    if app.take_analytics_request() {
        w.refresh_analytics = true;
    }
}

/// Records the dashboard headlessly: `frames` frames at 10 per second into `out` (JSON lines,
/// one frame per line, rows of `[text, fg, bg, bold]` runs), pressing `keys` at given frames.
pub fn record(
    ctx: Ctx,
    out: &std::path::Path,
    width: u16,
    height: u16,
    frames: usize,
    keys: &[(usize, char)],
) -> anyhow::Result<()> {
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Color;
    let (tx, rx) = channel::<Msg>();
    let wants = Arc::new(Mutex::new(Wants {
        period: Some(Period::Days(7)),
        group: Some(Dim::Project),
        ..Default::default()
    }));
    let w2 = wants.clone();
    let theme = theme::Theme::named(ctx.config.theme);
    let (seats, vault_dir) = (ctx.seats.clone(), ctx.dirs.vault_dir());
    let eco = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::spawn(move || worker::worker(ctx, tx, w2, eco, false));
    let mut term = ratatui::Terminal::new(TestBackend::new(width, height))?;
    let mut app = App::new(theme, false, false, seats, vault_dir);
    let col = |c: Color| match c {
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        _ => None,
    };
    let mut file = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut written = 0;
    let mut frame = 0;
    // Wait for the data before the clock of the recording starts (boot runs meanwhile).
    while written < frames {
        let t = Instant::now();
        while let Ok(m) = rx.try_recv() {
            app.on_msg(m);
        }
        let live = app.boot.is_none();
        if live {
            for (_, k) in keys.iter().filter(|(f, _)| *f == frame) {
                let code = match k {
                    '\n' => KeyCode::Enter,
                    '\x1b' => KeyCode::Esc,
                    c => KeyCode::Char(*c),
                };
                app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
            }
        }
        term.draw(|f| view::draw(f, &mut app))?;
        sync_wants(&mut app, &wants);
        let boot_rec = app.boot.is_some() && std::env::var_os("SWARMY_RECORD_BOOT").is_some();
        if live && !app.analytics.limits.is_empty() || boot_rec {
            let buf = term.backend().buffer();
            let mut rows = Vec::new();
            for y in 0..buf.area.height {
                let mut runs: Vec<(String, Option<String>, Option<String>, bool)> = Vec::new();
                for x in 0..buf.area.width {
                    let c = &buf[(x, y)];
                    let key = (
                        col(c.fg),
                        col(c.bg),
                        c.modifier.contains(ratatui::style::Modifier::BOLD),
                    );
                    match runs.last_mut() {
                        Some(r) if (r.1.clone(), r.2.clone(), r.3) == key => {
                            r.0.push_str(c.symbol())
                        }
                        _ => runs.push((c.symbol().to_string(), key.0, key.1, key.2)),
                    }
                }
                rows.push(runs);
            }
            use std::io::Write as _;
            writeln!(file, "{}", serde_json::to_string(&rows)?)?;
            written += 1;
            frame += 1;
        }
        std::thread::sleep(
            (if app.boot.is_some() {
                jobs::FRAME_BOOT
            } else {
                jobs::FRAME_RECORD
            })
            .saturating_sub(t.elapsed()),
        );
    }
    Ok(())
}

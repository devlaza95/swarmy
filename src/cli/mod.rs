//! Command-line interface.

pub mod accounts;
pub mod compact;
pub mod doctor;
pub mod limits;
pub mod models;
pub mod prices;
pub mod report;
pub mod setup;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::core::period::Period;
use crate::index::query::{Dim, Filter};
use crate::ui::style::{ColorChoice, Painter};

#[derive(Parser, Debug)]
#[command(
    name = "swarmy",
    version,
    about = "Real-time analytics dashboard for coding agents (Claude Code, Antigravity, Codex): live swarm, history, API-equivalent spend, weekly limits, login vault",
    long_about = None,
    after_help = "Run without a command for the live dashboard. Data never leaves this machine."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// When to use colours.
    #[arg(long, value_enum, global = true, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,
    /// Hide emails, names and full paths (for screenshots).
    #[arg(long, global = true)]
    pub mask: bool,
    /// Skip the boot animation of the dashboard.
    #[arg(long, global = true)]
    pub no_boot: bool,
    /// Low-power dashboard: fewer frames and refreshes (automatic on battery).
    #[arg(long, global = true)]
    pub eco: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// API-equivalent spend grouped by project, account, model, agent, day…
    Report(ReportArgs),
    /// Usage statistics per model: cost, cache hits, thinking, effort mix, substitutions.
    Models(PeriodFilterArgs),
    /// Weekly / 5-hour limits per account: where they went and when they run out.
    Limits {
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
        /// Projects to list per window.
        #[arg(long, default_value_t = 8)]
        top: usize,
    },
    /// The login vault: save, switch and list OAuth logins across seats.
    Accounts(accounts::AccountsArgs),
    /// What was found on this machine and whether it is readable.
    Doctor,
    /// Roll per-request detail older than the raw window up into the archive and free the space.
    Compact(compact::CompactArgs),
    /// Optional: add the swarmy status line to Claude Code (exact limits) and save logins.
    Setup(setup::SetupArgs),
    /// Jump to the Claude Code that has waited for you the longest (tmux, zellij, kitty, wezterm, iTerm2, Warp).
    Jump {
        /// Only list who waits, do not switch.
        #[arg(long)]
        list: bool,
    },
    /// Used by Claude Code's status line: records the limits it passes and prints a short line.
    Statusline {
        /// A status-line command to run first (the one you had before), with the same input.
        #[arg(long)]
        then: Option<String>,
    },
    /// The price in force for every model and its source.
    Prices {
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
    },
    /// Try the dashboard on a made-up world of agents (no real data is read).
    Demo {
        /// Record N frames headlessly into this file instead of opening the dashboard.
        #[arg(long, hide = true)]
        record: Option<std::path::PathBuf>,
        #[arg(long, hide = true, default_value_t = 100)]
        frames: usize,
        #[arg(long, hide = true, default_value = "140x40")]
        size: String,
        /// Keys to press while recording: `frame:key,frame:key` (key `enter`, `esc` or a character).
        #[arg(long, hide = true, default_value = "")]
        keys: String,
        /// The load of a busy 4K wall (three seats, ten days of history, about 18 agents at
        /// work), for measuring the battery budget (`tools/perf.sh`).
        #[arg(long, hide = true)]
        wall: bool,
    },
    /// Print shell completions.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print the man page (roff).
    Man,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Table,
    Json,
    Csv,
}

#[derive(Args, Debug, Clone)]
pub struct FilterArgs {
    /// Only this account: its uuid, its email or a saved login's label (resolved once; a name
    /// that fits several accounts is an error that lists them).
    #[arg(long)]
    pub account: Option<String>,
    /// Only this seat (its label, or its id from `swarmy doctor`).
    #[arg(long)]
    pub seat: Option<String>,
    /// Only this harness (`claude`, `agy`, `codex`).
    #[arg(long, value_enum)]
    pub harness: Option<crate::core::harness::Harness>,
    /// Only this project (name or path).
    #[arg(long)]
    pub project: Option<String>,
    /// Only models whose id contains this.
    #[arg(long)]
    pub model: Option<String>,
    /// Only work in linked git worktrees (`worktrees`) or outside them (`main`).
    #[arg(long, value_enum)]
    pub checkout: Option<CheckoutArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CheckoutArg {
    /// Linked git worktrees.
    Worktrees,
    /// Main checkouts (and dirs outside any repository).
    Main,
}

impl FilterArgs {
    /// The filter with what was typed resolved once, here at the edge: a seat label to its id,
    /// an account uuid, email or login label to the account (review H7).
    pub fn to_filter(
        &self,
        ctx: &crate::app::context::Ctx,
        idx: &crate::index::Index,
    ) -> anyhow::Result<Filter> {
        let seat = match &self.seat {
            Some(s) => Some(ctx.resolve_seat(s)?.id.clone()),
            None => None,
        };
        let account = match &self.account {
            Some(a) => Some(ctx.resolve_account(idx, a)?),
            None => None,
        };
        Ok(Filter {
            account,
            seat,
            harness: self.harness,
            project: self.project.clone(),
            model: self.model.clone(),
            in_worktree: self.checkout.map(|c| c == CheckoutArg::Worktrees),
        })
    }
}

#[derive(Args, Debug, Clone)]
pub struct PeriodFilterArgs {
    /// today, yesterday, week (current limit window), month, all, 24h, 7d, YYYY-MM-DD, FROM..TO
    #[arg(short, long, default_value = "week")]
    pub period: Period,
    #[command(flatten)]
    pub filter: FilterArgs,
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,
}

#[derive(Args, Debug, Clone)]
pub struct ReportArgs {
    /// Grouping.
    #[arg(short, long, value_enum, default_value_t = Dim::Project)]
    pub by: Dim,
    /// today, yesterday, week (current limit window), month, all, 24h, 7d, YYYY-MM-DD, FROM..TO
    #[arg(short, long, default_value = "week")]
    pub period: Period,
    #[command(flatten)]
    pub filter: FilterArgs,
    /// Rows to show (time groupings show all).
    #[arg(long, default_value_t = 15)]
    pub top: usize,
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,
}

pub fn run(cli: Cli) -> anyhow::Result<()> {
    let painter = Painter::new(cli.color);
    match cli.command {
        Some(Command::Completions { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "swarmy", &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Jump { list }) => jump(list),
        Some(Command::Statusline { then }) => {
            // Runs after every model response: no index, no scanning, just record and print.
            let dirs = crate::config::Dirs::resolve()?;
            // A status line must never fail: a bad config.toml falls back to the defaults, and
            // says so on the debug channel (`swarmy doctor` reports it too).
            let config = crate::config::Config::load(&dirs).unwrap_or_else(|e| {
                crate::diag::log("config", &format!("{e:#}"));
                crate::config::Config::default()
            });
            crate::statusline::run(&dirs, &config.statusline, then.as_deref())
        }
        Some(Command::Man) => {
            clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
            Ok(())
        }
        Some(Command::Demo {
            record,
            frames,
            size,
            keys,
            wall,
        }) => {
            let dirs = crate::config::Dirs::resolve()?;
            let load = if wall {
                crate::demo::Load::Wall
            } else {
                crate::demo::Load::Demo
            };
            let demo = crate::demo::start(&dirs.data.join("demo"), load)?;
            let mut ctx = crate::app::context::Ctx::demo(painter, demo.seats.clone(), &demo.root)?;
            ctx.mask = cli.mask;
            // Saved logins, so the VAULT screen has something to show.
            {
                let mut v = crate::vault::Vault::open(&ctx.dirs.vault_dir())?;
                for s in ctx.seats.iter().filter(|s| s.harness.supports_vault()) {
                    v.edit(|tx| tx.capture(s, Some(&format!("alex-{}", s.label))))?;
                }
                let spare = crate::claude::credentials::Secret::new(serde_json::json!({
                    "accessToken": "demo-access-spare", "refreshToken": "demo-refresh-spare",
                    "refreshTokenExpiresAt": (chrono::Utc::now() + chrono::Duration::days(4)).timestamp_millis()
                }));
                let acct = crate::claude::config_json::Account {
                    account_uuid: "acc-demo-spare".into(),
                    email: Some("alex+spare@example.com".into()),
                    ..Default::default()
                };
                v.edit(|tx| tx.put(&spare, Some(serde_json::json!({"accountUuid": "acc-demo-spare", "emailAddress": "alex+spare@example.com"})), Some(&acct), Some("alex-spare"), None))?;
                // A second Google login for the agy seat.
                let team = crate::claude::credentials::Secret::of(
                    crate::core::harness::Harness::Agy,
                    serde_json::json!({"token": {"access_token": "demo-agy-access-team", "refresh_token": "demo-agy-refresh-team",
                                                 "expiry": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()},
                                       "auth_method": "consumer", "id_token_email": "alex.team@example.com"}),
                );
                let team_acct = crate::agy::account::account_of(team.expose());
                v.edit(|tx| tx.put(&team, None, team_acct.as_ref(), Some("alex-g-team"), None))?;
                // On the wall, one login serves two seats.
                if wall && let Some(lab) = ctx.seats.iter().find(|s| s.label == "lab") {
                    v.edit(|tx| tx.switch(lab, "alex-work"))?;
                }
            }
            // Account names come from the saved logins just made.
            ctx.check_unconfirmed();
            match record {
                Some(out) => {
                    let (w, h) = size
                        .split_once('x')
                        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                        .unwrap_or((140, 40));
                    let keys: Vec<(usize, char)> = keys
                        .split(',')
                        .filter_map(|p| {
                            let (f, k) = p.split_once(':')?;
                            let c = match k {
                                "enter" => '\n',
                                "esc" => '\x1b',
                                k => k.chars().next()?,
                            };
                            Some((f.parse().ok()?, c))
                        })
                        .collect();
                    crate::tui::record(ctx, &out, w, h, frames, &keys)
                }
                None => crate::tui::run(ctx, cli.no_boot, cli.eco),
            }
        }
        command => {
            let ctx = crate::app::context::Ctx::load(painter, cli.mask)?;
            match command {
                Some(Command::Report(a)) => report::run(&ctx, &a),
                Some(Command::Models(a)) => models::run(&ctx, &a),
                Some(Command::Limits { format, top }) => limits::run(&ctx, format, top),
                Some(Command::Accounts(a)) => accounts::run(&ctx, &a),
                Some(Command::Doctor) => doctor::run(&ctx),
                Some(Command::Compact(a)) => compact::run(&ctx, &a),
                Some(Command::Prices { format }) => prices::run(&ctx, format),
                None => {
                    use std::io::IsTerminal;
                    if std::io::stdout().is_terminal() {
                        crate::tui::run(ctx, cli.no_boot, cli.eco)
                    } else {
                        limits::run(&ctx, Format::Table, 8)
                    }
                }
                Some(Command::Setup(a)) => setup::run(&ctx, &a),
                Some(
                    Command::Completions { .. }
                    | Command::Man
                    | Command::Demo { .. }
                    | Command::Statusline { .. }
                    | Command::Jump { .. },
                ) => {
                    unreachable!()
                }
            }
        }
    }
}

/// `swarmy jump`: the oldest session that waits for the user, from the dashboard's summary when
/// it runs, else straight from the session registries.
fn jump(list: bool) -> anyhow::Result<()> {
    let dirs = crate::config::Dirs::resolve()?;
    let config = crate::config::Config::load(&dirs)?;
    let now = chrono::Utc::now();
    use crate::core::pane::Pane;
    // (name, what, pane)
    let waiting: Vec<(String, String, Option<Pane>)> = match crate::fleet::read_fresh(
        &crate::fleet::path(&dirs.data),
        config.statusline.fresh_seconds,
        now,
    ) {
        Some(f) => f
            .needs_you
            .into_iter()
            .map(|n| (n.name, n.what, n.pane))
            .collect(),
        None => {
            let mut seat_dirs = vec![dirs.home.join(".claude")];
            if let Ok(rd) = std::fs::read_dir(&dirs.home) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with(".claude-") {
                        seat_dirs.push(e.path());
                    }
                }
            }
            let mut found: Vec<(String, String, Option<crate::core::pane::Pane>)> = seat_dirs
                .iter()
                .flat_map(|d| crate::claude::registry::waiting_sessions(&d.join("sessions")))
                .map(|w| (w.label(), w.what.clone(), w.pane.clone()))
                .collect();
            // Every Antigravity seat the dashboard would find, short of running processes.
            let agy_seats =
                crate::agy::discovery::discover(&crate::agy::discovery::AgyCandidates {
                    home: &dirs.home,
                    env_dirs: Vec::new(),
                    configured: config
                        .seat_entries(&dirs.home)
                        .into_iter()
                        .filter(|(p, _, h)| {
                            *h == Some(crate::core::harness::Harness::Agy)
                                || (h.is_none() && crate::agy::discovery::looks_like_agy_dir(p))
                        })
                        .map(|(p, l, _)| (p, l))
                        .collect(),
                });
            for seat in &agy_seats {
                let agy_dir = &seat.dir;
                let locked = crate::agy::live::locked_conversations_once(agy_dir);
                if locked.is_empty() {
                    continue;
                }
                let mut cache = crate::agy::live::AgyLiveCache::default();
                cache.resolve_procs(agy_dir, &locked);
                let metas = crate::index::agy_summaries::read_all(agy_dir);
                for cid in &locked {
                    if let Some(m) = metas.get(cid)
                        && m.status == crate::agy::meta::RunStatus::WaitingForUser
                    {
                        let name = m
                            .title
                            .clone()
                            .unwrap_or_else(|| crate::core::ids::short(cid).to_string());
                        let pane = cache.proc_of(cid).and_then(|p| p.pane.clone());
                        found.push((name, "input needed".into(), pane));
                    }
                }
            }
            found
        }
    };
    if waiting.is_empty() {
        println!("nothing waits for you");
        return Ok(());
    }
    if list {
        for (name, what, pane) in &waiting {
            println!(
                "▲ {name}: {what}{}",
                pane.as_ref()
                    .map(|p| format!("  ({p})"))
                    .unwrap_or_default()
            );
        }
        return Ok(());
    }
    let Some((name, what, Some(pane))) = waiting.iter().find(|w| w.2.is_some()).cloned() else {
        anyhow::bail!(
            "{} session(s) wait for you, none in a supported terminal (tmux, zellij, kitty, wezterm, iTerm2, Warp)",
            waiting.len()
        );
    };
    match crate::claude::pane::jump(&pane) {
        crate::claude::pane::Jump::Done => {
            println!("→ {name}: {what}");
            Ok(())
        }
        crate::claude::pane::Jump::Unreachable(why) | crate::claude::pane::Jump::Failed(why) => {
            anyhow::bail!(why)
        }
    }
}

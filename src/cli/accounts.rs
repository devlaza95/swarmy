//! `swarmy accounts`: the login vault.

use std::path::PathBuf;

use anyhow::{Context, bail};
use chrono::Local;
use clap::{Args, Subcommand};

use crate::claude::registry;
use crate::claude::seats::Seat;
use crate::core::harness::Harness;
use crate::core::limits::WindowKind;
use crate::core::seat_use::{SeatUse, hides_harness};
use crate::limits::{AccountReport, all_reports};
use crate::ui::fmt;
use crate::ui::mask;
use crate::ui::style::Tone;
use crate::ui::table::{Align, Cell, Table};
use crate::vault::{self, Vault, VaultError};

use crate::app::context::Ctx;

#[derive(Args, Debug, Clone)]
pub struct AccountsArgs {
    #[command(subcommand)]
    pub cmd: Option<AccountsCmd>,
    /// List every seat, the empty ones too (no login, no transcripts) that are left out.
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum AccountsCmd {
    /// Seats, saved logins, and how much limit each has left.
    List {
        /// Every seat, the empty ones too (no login, no transcripts) that are left out.
        #[arg(long)]
        all: bool,
    },
    /// Save the login a seat uses now.
    Capture {
        seat: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Import a backup holding `{"claudeAiOauth": {...}}` (or a whole credentials file).
    Import {
        file: PathBuf,
        #[arg(long)]
        label: String,
    },
    /// Put a saved login into a seat (the seat's current login is saved first).
    Switch {
        seat: String,
        login: String,
        /// Switch even though the seat's harness is running in it.
        #[arg(long)]
        force: bool,
    },
    /// Log into a new account in a scratch seat (runs `claude` or `agy` there) and save it.
    Add {
        #[arg(long)]
        label: String,
        /// The harness the login is for.
        #[arg(long, value_enum, default_value_t = crate::core::harness::Harness::Claude)]
        harness: crate::core::harness::Harness,
    },
    /// Rename a saved login (labels are unique per harness).
    Rename {
        login: String,
        new_label: String,
        /// The harness of the login, when its label names logins of two harnesses.
        #[arg(long, value_enum)]
        harness: Option<Harness>,
    },
    /// Forget a saved login.
    Rm {
        login: String,
        #[arg(long)]
        force: bool,
        /// The harness of the login, when its label names logins of two harnesses.
        #[arg(long, value_enum)]
        harness: Option<Harness>,
    },
}

fn vault_seats(ctx: &Ctx) -> Vec<Seat> {
    ctx.seats
        .iter()
        .filter(|s| s.harness.supports_vault())
        .cloned()
        .collect()
}

fn seat<'a>(ctx: &'a Ctx, key: &str) -> anyhow::Result<&'a Seat> {
    let found = ctx.resolve_seat(key)?;
    if let Some(e) = vault::unavailable(found.harness) {
        return Err(e.into());
    }
    if !found.harness.supports_vault() {
        return Err(VaultError::Unsupported(found.harness.display_name().into()).into());
    }
    Ok(found)
}

/// Claude Code processes using this seat right now.
fn live_in(ctx: &Ctx, s: &Seat) -> usize {
    match s.harness {
        crate::core::harness::Harness::Agy => {
            return crate::agy::live::locked_conversations_once(&s.dir).len();
        }
        crate::core::harness::Harness::Codex => {
            return crate::codex::live::open_threads_once(&s.dir).len();
        }
        crate::core::harness::Harness::Claude => {}
    }
    let registered = registry::live_sessions(&s.id, &s.sessions_dir(), &ctx.procs).len();
    let by_env = ctx
        .procs
        .claude
        .iter()
        .filter(|p| match &p.config_dir {
            Some(d) => crate::fsutil::canonical(d)
                .map(|d| d == s.dir)
                .unwrap_or(false),
            None => s.is_default,
        })
        .count();
    registered.max(by_env)
}

/// The weekly window of an account as the limit analysis sees it (the same estimate as
/// `swarmy limits`, the dashboard and the status line): (used ≈ now, resets at, the window rolled
/// over since the last reading).
fn weekly(
    reports: &[AccountReport],
    account: Option<&str>,
) -> Option<(f64, chrono::DateTime<chrono::Utc>, bool)> {
    let account = account?;
    let w = reports
        .iter()
        .find(|r| r.account.as_str() == account)?
        .windows
        .iter()
        .find(|w| w.reading.kind == WindowKind::SevenDay)?;
    Some((w.estimate_now.get(), w.window.end, w.reset_passed))
}

pub fn run(ctx: &Ctx, a: &AccountsArgs) -> anyhow::Result<()> {
    let mut v = Vault::open(&ctx.dirs.vault_dir())?;
    let seats = vault_seats(ctx);
    v.edit(|tx| tx.sync(&seats))?;
    let p = &ctx.painter;
    match a.cmd.clone().unwrap_or(AccountsCmd::List { all: false }) {
        AccountsCmd::List { all } => list(ctx, &mut v, a.all || all),
        AccountsCmd::Capture { seat: key, label } => {
            let s = seat(ctx, &key)?;
            match v.edit(|tx| tx.capture(s, label.as_deref()))? {
                Some(id) => {
                    let l = v.find(id.as_str())?;
                    println!(
                        "{} saved the login of seat '{}' as '{}' ({})",
                        p.paint("[ OK ]", Tone::Good),
                        s.label,
                        l.label,
                        l.meta.fingerprint
                    );
                }
                None => println!(
                    "{} seat '{}' has no login",
                    p.paint("[WARN]", Tone::Warn),
                    s.label
                ),
            }
            Ok(())
        }
        AccountsCmd::Import { file, label } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            let (secret, account) = vault::parse_import(&text)?;
            v.edit(|tx| tx.put(&secret, None, account.as_ref(), Some(&label), None))?;
            println!(
                "{} imported '{label}' ({})",
                p.paint("[ OK ]", Tone::Good),
                secret.meta().fingerprint
            );
            if account.is_none() {
                println!(
                    "{}",
                    p.paint(
                        "  The account behind it is learnt the first time it is switched into a seat.",
                        Tone::Dim
                    )
                );
            }
            Ok(())
        }
        AccountsCmd::Switch {
            seat: key,
            login,
            force,
        } => {
            let s = seat(ctx, &key)?;
            let live = live_in(ctx, s);
            // A running Codex refreshes the login it loaded and may write it back over the new
            // one; that refresh also spends the refresh token the vault saved. Never forced, in
            // this seat or in the one a Codex login moves out of.
            if s.harness == crate::core::harness::Harness::Codex {
                // An unknown login is refused by the switch itself, with its own message.
                let holders: Vec<&Seat> = match v.find_in(&login, Some(s.harness)) {
                    Ok(l) => ctx
                        .seats
                        .iter()
                        .filter(|o| l.is_in(&o.id) && o.id != s.id)
                        .collect(),
                    Err(_) => Vec::new(),
                };
                for o in std::iter::once(s).chain(holders) {
                    let n = live_in(ctx, o);
                    if n > 0 {
                        bail!(
                            "Codex is running in seat '{}' ({n} thread(s)). It keeps the login it loaded, and a refresh \
                             of it would undo the switch (OpenAI refresh tokens are single-use). Quit Codex there first.",
                            o.label
                        );
                    }
                }
            }
            if live > 0 && !force {
                let how = match s.harness {
                    crate::core::harness::Harness::Claude => {
                        "Running sessions re-read the credentials file when it changes and continue under the new login, \
                         but their account display stays stale until restart."
                    }
                    crate::core::harness::Harness::Agy => {
                        "A running agy keeps the login it loaded until it restarts (it does not write its old token back)."
                    }
                    // Refused above, whatever --force says.
                    crate::core::harness::Harness::Codex => "",
                };
                bail!(
                    "{} is running in seat '{}' ({live} session(s)). {how} Close them first, or pass --force.",
                    s.harness.display_name(),
                    s.label
                );
            }
            let seats = vault_seats(ctx);
            let rep = v.edit(|tx| tx.switch_moving(s, &login, &seats))?;
            if rep.unchanged {
                println!(
                    "{} seat '{}' already uses '{}'",
                    p.paint("[ OK ]", Tone::Good),
                    rep.seat,
                    rep.to
                );
                return Ok(());
            }
            println!(
                "{} seat '{}': {} → {}",
                p.paint("[ OK ]", Tone::Good),
                rep.seat,
                rep.from.as_deref().unwrap_or("(nothing)"),
                p.paint(&rep.to, Tone::Bold)
            );
            if !rep.shared_with.is_empty() {
                println!(
                    "{}",
                    p.paint(
                        &format!(
                            "  '{}' is also in seat {}",
                            rep.to,
                            rep.shared_with.join(", ")
                        ),
                        Tone::Dim
                    )
                );
            }
            for (o, now) in &rep.moved {
                println!(
                    "{}",
                    p.paint(
                        &format!(
                            "  '{}' moved out of seat '{o}' (one seat only), which now has {}",
                            rep.to,
                            now.as_deref()
                                .map_or("no login".to_string(), |l| format!("'{l}'"))
                        ),
                        Tone::Accent
                    )
                );
            }
            if let Some(saved) = &rep.auto_saved {
                println!("{}", p.paint(&format!("  the seat's previous login was not in the vault; saved it as '{saved}'"), Tone::Accent));
            }
            if let Some(b) = rep.backup {
                println!(
                    "{}",
                    p.paint(
                        &format!("  backups in {}", ctx.show_path(&b.to_string_lossy())),
                        Tone::Dim
                    )
                );
            }
            if live > 0 {
                println!("{}", p.paint("  running sessions pick up the new login on their next request; restart them to refresh the account shown", Tone::Warn));
            }
            Ok(())
        }
        AccountsCmd::Add { label, harness } => {
            // Said before the sign-in, not after it.
            if let Some(e) = vault::unavailable(harness) {
                return Err(e.into());
            }
            v.check_new_label(&label, harness)?;
            let scratch = vault::new_login_scratch(&ctx.dirs.vault_dir(), ctx.now);
            std::fs::create_dir_all(&scratch)?;
            let (program, hint) = vault::new_login_recipe(harness);
            println!(
                "Starting {} in a scratch seat. {hint}",
                harness.display_name()
            );
            let (mut cmd, tmp) = vault::new_login(harness, &label, &scratch);
            match cmd.status() {
                Ok(status) if !status.success() => eprintln!("{program} exited with {status}"),
                Ok(_) => {}
                Err(e) => {
                    if let Err(rm) = std::fs::remove_dir_all(&scratch) {
                        eprintln!("cannot remove {}: {rm}", scratch.display());
                    }
                    return Err(e)
                        .with_context(|| format!("cannot start `{program}` (is it on PATH?)"));
                }
            }
            let saved = vault::save_new_login(&mut v, &label, &scratch, &tmp)?;
            println!(
                "{} saved '{label}' ({})",
                p.paint("[ OK ]", Tone::Good),
                saved.fingerprint
            );
            if let Some(msg) = saved.left_behind {
                eprintln!("{msg}");
            }
            Ok(())
        }
        AccountsCmd::Rename {
            login,
            new_label,
            harness,
        } => {
            let id = v.find_in(&login, harness)?.id.clone();
            v.edit(|tx| tx.rename(id.as_str(), &new_label))?;
            println!("{} renamed to '{new_label}'", p.paint("[ OK ]", Tone::Good));
            Ok(())
        }
        AccountsCmd::Rm {
            login,
            force,
            harness,
        } => {
            let id = v.find_in(&login, harness)?.id.clone();
            let l = v.edit(|tx| tx.remove(id.as_str(), force))?;
            println!("{} forgot '{}'", p.paint("[ OK ]", Tone::Good), l.label);
            Ok(())
        }
    }
}

fn list(ctx: &Ctx, v: &mut Vault, all: bool) -> anyhow::Result<()> {
    let vault_seats = vault_seats(ctx);
    let states = v.edit(|tx| tx.seat_states(&vault_seats))?;
    let (idx, _) = ctx.index()?;
    let uses = ctx.seat_uses(&idx, &states, |s| live_in(ctx, s))?;
    // Harnesses left out because all their seats are empty: (harness, their dirs).
    let mut hidden: Vec<(Harness, Vec<String>)> = Vec::new();
    let reports = all_reports(&ctx.limits_inputs(&idx)?, &idx)?;
    let p = &ctx.painter;
    let week_cell = |acct: Option<&str>| match weekly(&reports, acct) {
        Some((u, end, reset)) => (
            Cell::tone(
                format!(
                    "{} {}{}",
                    fmt::gauge(u / 100.0, 10),
                    fmt::pct(u),
                    if reset { " (reset)" } else { "" }
                ),
                Tone::of_utilization(u),
            ),
            Cell::tone(
                end.with_timezone(&Local).format("%a %H:%M").to_string(),
                Tone::Dim,
            ),
            Some(u),
        ),
        None => (Cell::tone("-", Tone::Dim), Cell::new(""), None),
    };
    // The saved login with the most limit left that no seat uses: the one to switch to.
    let mut best: Option<(f64, String, Harness)> = None;
    let mut first = true;
    for h in Harness::ALL {
        let seats: Vec<(&Seat, &crate::index::SeatInfo, SeatUse)> = ctx
            .seats
            .iter()
            .zip(&ctx.seat_info)
            .zip(&uses)
            .filter(|((s, _), _)| s.harness == h && s.harness.supports_vault())
            .map(|((s, si), u)| (s, si, *u))
            .collect();
        let logins: Vec<&vault::Login> = v.logins().iter().filter(|l| l.harness == h).collect();
        if seats.is_empty() && logins.is_empty() {
            continue;
        }
        let empty: Vec<bool> = seats.iter().map(|(.., u)| u.is_empty()).collect();
        if !all && hides_harness(&empty, logins.len()) {
            hidden.push((
                h,
                seats
                    .iter()
                    .map(|(s, ..)| ctx.show_path(&s.dir.to_string_lossy()))
                    .collect(),
            ));
            continue;
        }
        if !first {
            println!();
        }
        first = false;
        let quota = h.supports_limits();
        println!(
            "{} {}",
            p.paint(&h.display_name().to_uppercase(), Tone::Header),
            p.paint(
                &format!(
                    "· {} seat{} · {} saved login{}{}",
                    seats.len(),
                    if seats.len() == 1 { "" } else { "s" },
                    logins.len(),
                    if logins.len() == 1 { "" } else { "s" },
                    if quota { "" } else { " · no local quota" }
                ),
                Tone::Dim
            )
        );
        // Seats: where the harness runs, and the login each holds now.
        println!(
            "{}",
            p.paint(
                &format!(
                    "  seats · the config dirs {} runs in, and the login in each",
                    h.display_name()
                ),
                Tone::Dim
            )
        );
        let mut cols = vec![
            ("SEAT", Align::Left),
            ("DIR", Align::Left),
            ("LOGIN IN IT", Align::Left),
            ("ACCOUNT", Align::Left),
        ];
        if quota {
            cols.extend([("WEEK", Align::Left), ("RESETS", Align::Left)]);
        }
        cols.push(("LIVE", Align::Right));
        let mut t = Table::new(&cols);
        for (s, si, used) in &seats {
            let st = states.get(&s.id).cloned().unwrap_or_default();
            let acct = si.account.as_ref().map(|a| a.id().to_string());
            let mut row = vec![
                Cell::tone(&s.label, Tone::Bold),
                Cell::tone(ctx.show_path(&s.dir.to_string_lossy()), Tone::Dim),
                match (&st.saved_as, &st.fingerprint) {
                    (Some(l), _) => {
                        let email = logins
                            .iter()
                            .find(|x| &x.label == l)
                            .and_then(|x| x.email.as_deref());
                        Cell::tone(
                            format!("⚿ {}", mask::login_label(l, email, ctx.mask)),
                            Tone::Good,
                        )
                    }
                    (None, Some(_)) => Cell::tone(
                        format!("not saved: swarmy accounts capture {}", s.label),
                        Tone::Warn,
                    ),
                    (None, None) if vault::in_keychain(h) => {
                        Cell::tone("in the Keychain", Tone::Dim)
                    }
                    (None, None) => Cell::tone("(logged out)", Tone::Dim),
                },
                match (&st.unverified_since, acct.as_deref()) {
                    (Some(t), _) => Cell::tone(
                        format!(
                            "? new login {} (open {} there to confirm)",
                            t.with_timezone(&Local).format("%d %b %H:%M"),
                            h.display_name()
                        ),
                        Tone::Warn,
                    ),
                    (None, Some(a)) => Cell::new(ctx.shown_account_name(a)),
                    (None, None) => Cell::tone("-", Tone::Dim),
                },
            ];
            if quota {
                let (week, resets, _) = week_cell(acct.as_deref());
                row.extend([week, resets]);
            }
            row.push(Cell::new(used.live.to_string()));
            t.row(row);
        }
        if seats.is_empty() {
            println!("{}", p.paint("    none found", Tone::Dim));
        } else {
            println!("{}", indent(&t.render(p)));
        }
        // Saved logins: what the vault keeps for this harness, and which seats use each.
        if let Some(e) = vault::unavailable(h) {
            println!("{}", p.paint(&format!("  saved logins · {e}"), Tone::Dim));
            continue;
        }
        println!(
            "{}",
            p.paint(
                &format!(
                    "  saved logins · in the vault, each fits {} seats only",
                    h.display_name()
                ),
                Tone::Dim
            )
        );
        if logins.is_empty() {
            println!(
                "{}",
                p.paint(
                    &format!(
                        "    none yet: swarmy accounts capture <seat> saves a seat's login, \
                         swarmy accounts add --harness {} --label <name> a new one",
                        h.key()
                    ),
                    Tone::Dim
                )
            );
            continue;
        }
        let mut cols = vec![
            ("LOGIN", Align::Left),
            ("EMAIL", Align::Left),
            ("IN SEATS", Align::Left),
        ];
        if quota {
            cols.push(("WEEK", Align::Left));
        }
        cols.extend([("LOGIN EXPIRES", Align::Left), ("GRANT", Align::Left)]);
        let mut t = Table::new(&cols);
        for l in logins {
            let label = mask::login_label(&l.label, l.email.as_deref(), ctx.mask);
            let mut row = vec![
                Cell::tone(format!("⚿ {label}"), Tone::Bold),
                Cell::new(mask::email(
                    l.email.as_deref().unwrap_or_default(),
                    ctx.mask,
                )),
                if l.mounted.is_empty() {
                    Cell::tone("- (free)", Tone::Dim)
                } else {
                    Cell::tone(
                        l.mounted
                            .iter()
                            .map(|s| ctx.seat_label(s))
                            .collect::<Vec<_>>()
                            .join(", "),
                        Tone::Good,
                    )
                },
            ];
            if quota {
                let (week, _, used) = week_cell(l.account_uuid.as_deref());
                if let Some(u) = used
                    && u < 100.0
                    && l.mounted.is_empty()
                    && best.as_ref().is_none_or(|(b, ..)| u < *b)
                {
                    best = Some((u, label.clone(), h));
                }
                row.push(week);
            }
            let expires = l.meta.refresh_expires_at.map(|t| {
                let left = t - ctx.now;
                Cell::tone(
                    format!(
                        "{} (in {})",
                        t.with_timezone(&Local).format("%b %d"),
                        fmt::duration(left)
                    ),
                    if left.num_days() < 7 {
                        Tone::Warn
                    } else {
                        Tone::Dim
                    },
                )
            });
            row.push(expires.unwrap_or_else(|| Cell::tone("-", Tone::Dim)));
            row.push(Cell::tone(
                crate::core::ids::short(&l.meta.fingerprint).to_string(),
                Tone::Dim,
            ));
            t.row(row);
        }
        println!("{}", indent(&t.render(p)));
    }
    for (h, dirs) in &hidden {
        if !first {
            println!();
        }
        first = false;
        println!(
            "{}",
            p.paint(
                &format!(
                    "hidden: {} — {} empty seat{} ({}): no login, no transcripts · swarmy accounts --all lists {}",
                    h.display_name(),
                    dirs.len(),
                    if dirs.len() == 1 { "" } else { "s" },
                    dirs.join(", "),
                    if dirs.len() == 1 { "it" } else { "them" }
                ),
                Tone::Dim
            )
        );
    }
    if first {
        println!(
            "{}",
            p.paint(
                "no seats found — run claude, agy or codex once, or add [[seat]] to config.toml",
                Tone::Dim
            )
        );
    }
    println!();
    println!(
        "{}",
        p.paint(
            &format!(
                "vault: {} · {}",
                v.store_name(),
                ctx.show_path(&ctx.dirs.vault_dir().to_string_lossy())
            ),
            Tone::Dim
        )
    );
    if let Some((u, label, h)) = best {
        println!(
            "{}",
            p.paint(
                &format!(
                    "tip: '{label}' is free and has the most limit left ({} used): swarmy accounts switch <{} seat> {label}",
                    fmt::pct(u),
                    h.display_name()
                ),
                Tone::Accent
            )
        );
    }
    Ok(())
}

/// A table under a sub-heading.
fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

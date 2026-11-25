//! `swarmy doctor`: what was found, what is readable, what looks off.

use crate::claude::registry::{self, LiveSession, SessionStatus};
use crate::claude::transcript::ParseStats;
use crate::core::harness::Harness;
use crate::core::period::Period;
use crate::index::query::{Counted, Filter};
use crate::ui::fmt;
use crate::ui::style::Tone;
use crate::ui::table::{Align, Cell, Table};

use crate::app::context::Ctx;
use crate::index::Index;

pub fn run(ctx: &Ctx) -> anyhow::Result<()> {
    let p = &ctx.painter;
    let ok = |s: &str| format!("{} {s}", p.paint("[ OK ]", Tone::Good));
    let warn = |s: &str| format!("{} {s}", p.paint("[WARN]", Tone::Warn));
    let info = |s: &str| format!("{} {s}", p.paint("[ .. ]", Tone::Dim));

    println!("{}", p.paint("SEATS (config/data dirs)", Tone::Header));
    if ctx.seats.is_empty() {
        println!(
            "{}",
            warn(
                "no agent config dir found (looked for ~/.claude*, ~/.gemini/antigravity-cli, ~/.codex*; add [[seat]] to config.toml)"
            )
        );
    }
    // What could not be read: numbers below may be off because of it.
    for problem in &ctx.problems {
        println!("{}", warn(problem));
    }
    let mut agy_cache = crate::agy::live::AgyLiveCache::default();
    let mut codex_cache = crate::codex::live::CodexLiveCache::default();
    let mut live_by_seat: std::collections::HashMap<crate::core::ids::SeatId, Vec<LiveSession>> =
        Default::default();
    for s in &ctx.seats {
        let sessions = match s.harness {
            Harness::Claude => registry::live_sessions(&s.id, &s.sessions_dir(), &ctx.procs),
            Harness::Agy => {
                let locked = agy_cache.locked_conversations(&s.dir);
                agy_cache.resolve_procs(&s.dir, &locked);
                let metas = crate::agy::summaries::read_all(&s.dir);
                crate::agy::live::build_live_sessions(
                    &s.id, &locked, &metas, &agy_cache, &ctx.procs, ctx.now,
                )
            }
            Harness::Codex => {
                let open = codex_cache.open_threads(&s.dir);
                codex_cache.resolve_procs(&s.dir, &open);
                codex_cache.live_sessions(&s.id, &s.dir, &open, ctx.now)
            }
        };
        live_by_seat.insert(s.id.clone(), sessions);
    }
    let mut t = Table::new(&[
        ("SEAT", Align::Left),
        ("HARNESS", Align::Left),
        ("DIR", Align::Left),
        ("ACCOUNT", Align::Left),
        ("PLAN", Align::Left),
        ("LIVE", Align::Right),
        ("FOUND BY", Align::Left),
    ]);
    t.flex_col = 2;
    for (s, si) in ctx.seats.iter().zip(&ctx.seat_info) {
        let live_n = live_by_seat.get(&s.id).map_or(0, Vec::len);
        let acct = si
            .account
            .as_ref()
            .map(|a| ctx.shown_account_name(a.id().as_str()))
            .unwrap_or_else(|| "(not logged in)".into());
        let plan = si
            .account
            .as_ref()
            .and_then(|a| a.plan_label())
            .unwrap_or_default();
        t.row(vec![
            Cell::tone(&s.label, Tone::Bold),
            Cell::tone(s.harness.key(), Tone::Dim),
            Cell::new(ctx.show_path(&s.dir.to_string_lossy())),
            Cell::new(acct),
            Cell::tone(plan, Tone::Dim),
            Cell::new(live_n.to_string()),
            Cell::tone(s.found_by.join(", "), Tone::Dim),
        ]);
    }
    println!("{}", t.render(p));

    // The optional status line: exact limits after every response (Claude Code seats).
    let claude_seats = ctx
        .seats
        .iter()
        .filter(|s| s.harness.supports_statusline())
        .count();
    let with: Vec<&str> = ctx
        .seats
        .iter()
        .filter(|s| {
            s.harness.supports_statusline()
                && matches!(
                    super::setup::seat_status_line(s),
                    super::setup::StatusLine::Swarmy { .. }
                )
        })
        .map(|s| s.label.as_str())
        .collect();
    if claude_seats > 0 {
        println!();
        if with.is_empty() {
            println!(
                "{}",
                p.paint(
                    "limits come from Claude Code's cached readings (≈ between them); optional: `swarmy setup` adds a status line for exact ones",
                    Tone::Dim
                )
            );
        } else {
            println!(
                "{}",
                ok(&format!(
                    "swarmy status line in: {} (exact limits after every response)",
                    with.join(", ")
                ))
            );
        }
    }

    // Accounts shared by several seats share one limit.
    let mut by_acct: std::collections::BTreeMap<String, Vec<&str>> = Default::default();
    for si in &ctx.seat_info {
        if let Some(a) = &si.account {
            by_acct
                .entry(a.id().to_string())
                .or_default()
                .push(&si.label);
        }
    }
    println!();
    println!(
        "{}",
        ok(&format!(
            "{} seats, {} accounts",
            ctx.seats.len(),
            by_acct.len()
        ))
    );
    for (acct, seats) in &by_acct {
        if seats.len() > 1 {
            println!(
                "{}",
                info(&format!(
                    "seats {} are logged into the same account ({}): they share one limit",
                    seats.join(" + "),
                    ctx.shown_account_name(acct)
                ))
            );
        }
    }

    println!();
    println!("{}", p.paint("LIVE SESSIONS", Tone::Header));
    let mut any = false;
    for s in &ctx.seats {
        let sessions = match live_by_seat.get(&s.id) {
            Some(v) => v.as_slice(),
            None => &[],
        };
        for l in sessions {
            any = true;
            let (st, tone) = match &l.status {
                SessionStatus::Busy => ("busy".to_string(), Tone::Good),
                SessionStatus::Idle => ("idle".to_string(), Tone::Dim),
                SessionStatus::Waiting(w) => (format!("NEEDS YOU: {w}"), Tone::Warn),
                SessionStatus::Other(o) => (o.clone(), Tone::Plain),
            };
            let ver = match l.version.as_deref() {
                Some(v) if !v.is_empty() => format!(" · v{v}"),
                Some(_) | None => String::new(),
            };
            let pid_s = if l.pid > 0 {
                format!("pid {} · ", l.pid)
            } else {
                String::new()
            };
            let raw_name = l
                .name
                .clone()
                .unwrap_or_else(|| l.session_id.short().to_string());
            let name = if raw_name.chars().count() > 24 {
                let mut t: String = raw_name.chars().take(23).collect();
                t.push('…');
                t
            } else {
                raw_name
            };
            println!(
                "  {} {:<24} {:<10} {} {}",
                p.paint("●", tone),
                name,
                s.label,
                p.paint(&st, tone),
                p.paint(&format!("{pid_s}{}{ver}", ctx.show_path(&l.cwd)), Tone::Dim)
            );
        }
    }
    if !any {
        println!("  {}", p.paint("none", Tone::Dim));
    }
    let unregistered = ctx
        .procs
        .claude
        .iter()
        .filter(|c| c.config_dir.is_none() && !ctx.seats.iter().any(|s| s.is_default))
        .count();
    if unregistered > 0 {
        println!(
            "{}",
            warn(&format!(
                "{unregistered} claude processes use the default dir, which was not found"
            ))
        );
    }

    println!();
    println!("{}", p.paint("INDEX", Tone::Header));
    let (mut idx, stats) = ctx.index()?;
    println!(
        "{}",
        ok(&format!(
            "{} ({})",
            ctx.show_path(&ctx.dirs.index_file().to_string_lossy()),
            fmt::human_bytes(
                std::fs::metadata(ctx.dirs.index_file())
                    .map(|m| m.len())
                    .unwrap_or(0)
            )
        ))
    );
    println!(
        "{}",
        ok(&format!(
            "{} transcripts · {} requests · {} tool calls · {} sessions · {} agents",
            fmt::count(stats.files_total as u64),
            fmt::count(idx.count(Counted::Requests)?),
            fmt::count(idx.count(Counted::ToolCalls)?),
            fmt::count(idx.count(Counted::Sessions)?),
            fmt::count(idx.count(Counted::Agents)?)
        ))
    );
    println!(
        "{}",
        info(&format!(
            "this run read {} files, {:.1} MB in {:.2}s",
            stats.files_read,
            stats.bytes_read as f64 / 1e6,
            stats.elapsed.as_secs_f64()
        ))
    );
    let (empty_lines, hidden) = empty_seats(ctx, &idx, &live_by_seat)?;
    for line in empty_lines {
        println!("{}", info(&line));
    }
    let mut ps = ParseStats::default();
    for (t, n) in idx.record_types()? {
        if t == crate::claude::transcript::MALFORMED {
            ps.malformed = n;
        } else {
            ps.by_type.insert(t, n);
        }
    }
    if ps.malformed > 0 {
        println!(
            "{}",
            warn(&format!(
                "{} transcript records are damaged (no JSON, or numbers out of range): what they held is not counted",
                fmt::count(ps.malformed)
            ))
        );
    }
    if let Some(n) = idx.meta("rebuild_refused")?.filter(|n| n != "0") {
        println!(
            "{}",
            warn(&format!(
                "a cache rebuild left out {n} kept rows (of transcripts that are gone) the new schema refused; \
                 their spend is missing"
            ))
        );
    }
    for (path, error, since) in idx.unreadable_files()? {
        println!(
            "{}",
            warn(&format!(
                "cannot read {} since {}: {error}; nothing of it is counted until it can be read",
                ctx.show_path(&path),
                crate::index::from_ms(since)
                    .with_timezone(&chrono::Local)
                    .format("%a %d %b %H:%M")
            ))
        );
    }
    let malformed = crate::claude::transcript::MALFORMED;
    let by_harness = idx.record_types_by_harness()?;
    let unknown: Vec<(&str, u64)> = by_harness
        .iter()
        .filter(|(h, t, _)| match h {
            Harness::Claude => !crate::claude::transcript::is_known_type(t),
            Harness::Agy => {
                !crate::agy::transcript::KNOWN_TYPES.contains(&t.as_str()) && t != malformed
            }
            Harness::Codex => {
                !crate::codex::rollout::KNOWN_TYPES.contains(&t.as_str()) && t != malformed
            }
        })
        .map(|(_, t, n)| (t.as_str(), *n))
        .collect();
    if unknown.is_empty() {
        println!("{}", ok("every transcript record type is recognised"));
    } else {
        let list: Vec<String> = unknown.iter().map(|(t, n)| format!("{t} ({n})")).collect();
        println!(
            "{}",
            warn(&format!(
                "unrecognised record types (Claude Code format moved on?): {}",
                list.join(", ")
            ))
        );
    }
    let week = Period::Days(7).span(ctx.now, None);
    let total = idx.total(week, &Filter::default())?;
    if total.unpriced > 0 {
        println!(
            "{}",
            warn(&format!(
                "{} requests in the last 7 days have no price (see `swarmy prices`)",
                total.unpriced
            ))
        );
    }
    let (n, theirs, ours) = idx.cost_crosscheck(week)?;
    if n > 0 && theirs > 0.0 {
        let diff = (ours - theirs) / theirs * 100.0;
        let line = format!(
            "cost cross-check, {n} sessions of the last 7 days: Claude Code counted {}, transcripts give {} ({:+.1}%; background calls such as titles are not in transcripts)",
            fmt::money(theirs),
            fmt::money(ours),
            diff
        );
        println!(
            "{}",
            if diff.abs() <= 5.0 {
                ok(&line)
            } else {
                warn(&line)
            }
        );
    }
    let errs = idx.api_errors(week)?;
    if !errs.is_empty() {
        let list: Vec<String> = errs.iter().map(|(k, n)| format!("{k} ×{n}")).collect();
        println!(
            "{}",
            info(&format!(
                "API errors in the last 7 days: {}",
                list.join(", ")
            ))
        );
    }

    println!();
    println!("{}", p.paint("STORAGE", Tone::Header));
    idx.sample_storage(ctx.now.with_timezone(&chrono::Local).date_naive())?;
    for line in storage(ctx, &mut idx)? {
        println!("{line}");
    }

    println!();
    println!("{}", p.paint("LIMIT READINGS", Tone::Header));
    let readings = ctx.latest_readings(&idx)?;
    if readings.is_empty() && hidden.contains(&Harness::Claude) {
        // A machine without Claude Code in use (its seats are empty): nothing to read.
        println!(
            "{}",
            info("no Claude Code seat in use: no limit readings to show")
        );
    } else if readings.is_empty() {
        println!(
            "{}",
            warn("no cached utilization in any .claude.json (open /usage in Claude Code once)")
        );
    }
    for (acct, rs) in readings {
        for r in rs {
            println!(
                "{}",
                ok(&format!(
                    "{} {} {} · resets {} · reading {} old",
                    ctx.shown_account_name(&acct),
                    r.kind.label(),
                    fmt::pct(r.utilization),
                    r.resets_at
                        .with_timezone(&chrono::Local)
                        .format("%a %d %b %H:%M"),
                    fmt::duration(ctx.now - r.fetched_at)
                ))
            );
        }
    }
    println!();
    println!("{}", p.paint("CONFIG", Tone::Header));
    let cfg = ctx.dirs.config_file();
    println!(
        "{}",
        if cfg.exists() {
            ok(&ctx.show_path(&cfg.to_string_lossy()))
        } else {
            info(&format!(
                "{} (not present; defaults in use)",
                ctx.show_path(&cfg.to_string_lossy())
            ))
        }
    );
    Ok(())
}

/// The STORAGE section: the index's size, the raw window and the rollups, growth and the
/// 12-month projection against the budget, the seats' transcript retention, the last
/// compaction, and what waits (the one-time VACUUM, rows priced by an old long-context band).
/// One line per empty seat (`core::seat_use`): doctor hides nothing, it says which seats the
/// dashboard and `swarmy accounts` leave out, or why one is kept. Also returns the harnesses
/// the dashboard leaves out.
fn empty_seats(
    ctx: &Ctx,
    idx: &Index,
    live: &std::collections::HashMap<crate::core::ids::SeatId, Vec<LiveSession>>,
) -> anyhow::Result<(Vec<String>, Vec<Harness>)> {
    let vault_seats: Vec<crate::claude::seats::Seat> = ctx
        .seats
        .iter()
        .filter(|s| s.harness.supports_vault())
        .cloned()
        .collect();
    let read = crate::vault::Vault::open(&ctx.dirs.vault_dir())
        .and_then(|v| Ok((v.seat_states(&vault_seats)?, v.logins().to_vec())));
    let (states, logins) = match read {
        Ok(r) => r,
        Err(e) => {
            return Ok((
                vec![format!(
                    "empty seats not checked: the vault cannot be read ({e:#})"
                )],
                Vec::new(),
            ));
        }
    };
    let uses = ctx.seat_uses(idx, &states, |s| live.get(&s.id).map_or(0, Vec::len))?;
    let mut out = Vec::new();
    let mut hidden_harnesses = Vec::new();
    for h in Harness::ALL {
        let of: Vec<(&crate::claude::seats::Seat, bool)> = ctx
            .seats
            .iter()
            .zip(&uses)
            .filter(|(s, _)| s.harness == h)
            .map(|(s, u)| (s, u.is_empty()))
            .collect();
        let saved = logins.iter().filter(|l| l.harness == h).count();
        let empty: Vec<bool> = of.iter().map(|(_, e)| *e).collect();
        let hidden = crate::core::seat_use::hides_harness(&empty, saved);
        if hidden {
            hidden_harnesses.push(h);
        }
        for (s, _) in of.iter().filter(|(_, e)| *e) {
            let what = if hidden {
                "hidden in the dashboard (. shows it) and in swarmy accounts (--all lists it)"
            } else {
                "shown: its harness has other seats in use or saved logins that could go in"
            };
            out.push(format!(
                "seat '{}' ({}): empty (no login, no transcripts): {what}",
                s.label,
                ctx.show_path(&s.dir.to_string_lossy())
            ));
        }
    }
    Ok((out, hidden_harnesses))
}

fn storage(ctx: &Ctx, idx: &mut Index) -> anyhow::Result<Vec<String>> {
    let p = &ctx.painter;
    let ok = |s: &str| format!("{} {s}", p.paint("[ OK ]", Tone::Good));
    let warn = |s: &str| format!("{} {s}", p.paint("[WARN]", Tone::Warn));
    let info = |s: &str| format!("{} {s}", p.paint("[ .. ]", Tone::Dim));
    let hb = fmt::human_bytes;
    let local = |t: i64| crate::index::from_ms(t).with_timezone(&chrono::Local);
    let keep = ctx.keep_raw_days(None)?;
    let st = idx.storage_report()?;
    let mut out = Vec::new();
    out.push(ok(&format!(
        "index {} (+ WAL {}, {} free) · auto-vacuum {}",
        hb(st.file.bytes()),
        hb(st.wal_bytes),
        hb(st.file.free_bytes()),
        if st.file.incremental {
            "incremental"
        } else {
            "off"
        }
    )));
    // Since the last compaction's cutoff, or the oldest request when nothing was rolled up.
    let since = if st.rolled_until > 0 {
        Some(st.rolled_until)
    } else {
        st.oldest_raw
    };
    out.push(info(&format!(
        "detail   since {}: {} requests, {} tool calls ≈ {} · keep_raw_days = {keep}",
        since.map_or("-".into(), |t| local(t).format("%Y-%m-%d").to_string()),
        fmt::count(st.raw_requests),
        fmt::count(st.raw_tool_calls),
        hb(crate::core::storage::raw_bytes(
            st.raw_requests,
            st.raw_tool_calls
        ))
    )));
    out.push(info(&match st.archive_since {
        Some(t) => format!(
            "history  since {} in rollups: {} rows ≈ {}",
            local(t).format("%Y-%m-%d"),
            fmt::count(st.usage_rows + st.tool_rows + st.edit_rows),
            hb(st.rollup_bytes())
        ),
        None => "history  nothing rolled up yet: all of it is per-request detail".into(),
    }));
    let o = st.outlook(ctx.now, keep);
    let budget = ctx.config.storage.budget_mb << 20;
    let pace = if o.from_samples {
        format!("over the last {} days", st.samples.len())
    } else {
        "estimated from the requests a day".into()
    };
    let growth = format!(
        "growth   +{}/day {pace} → ≈ {} in 12 months (budget {} MB)",
        hb(o.per_day as u64),
        hb(o.in_12_months),
        ctx.config.storage.budget_mb
    );
    out.push(if o.in_12_months > budget {
        warn(&growth)
    } else {
        ok(&growth)
    });
    let mut retention: Vec<(u32, bool, usize)> = Vec::new();
    for (_, days, set) in ctx.retention() {
        match retention.iter_mut().find(|r| r.0 == days && r.1 == set) {
            Some(r) => r.2 += 1,
            None => retention.push((days, set, 1)),
        }
    }
    if !retention.is_empty() {
        let parts: Vec<String> = retention
            .iter()
            .map(|(days, set, n)| {
                format!(
                    "after {days} days on {n} seat{} ({})",
                    if *n == 1 { "" } else { "s" },
                    if *set {
                        "cleanupPeriodDays"
                    } else {
                        "cleanupPeriodDays, default"
                    }
                )
            })
            .collect();
        out.push(info(&format!(
            "claude   transcripts are deleted {}: older history exists only in this index",
            parts.join(", ")
        )));
    }
    out.push(info(&match &st.last_compaction {
        Some(c) => {
            let at = local(c.at);
            let day = if at.date_naive() == ctx.now.with_timezone(&chrono::Local).date_naive() {
                "today".to_string()
            } else {
                at.format("%a %d %b").to_string()
            };
            format!(
                "last compaction {day} {}, {} chunks, {} requests, {:.1} s · skipped as already rolled up: {} records",
                at.format("%H:%M"),
                c.chunks,
                fmt::count(c.requests),
                c.ms as f64 / 1000.0,
                fmt::count(st.skipped_records)
            )
        }
        None => "last compaction never: `swarmy compact --dry-run` shows what it would do".into(),
    }));
    if !ctx.config.storage.auto_compact {
        out.push(info(
            "auto_compact = false: the dashboard does not compact; `swarmy compact` does",
        ));
    }
    if let Some(why) = idx.vacuum_skipped()? {
        out.push(warn(&format!(
            "auto-vacuum off: the one-time VACUUM {why}; `swarmy compact --vacuum` runs it now"
        )));
    } else if !st.file.incremental && st.file.free_pages > 0 {
        out.push(info(&format!(
            "auto-vacuum off: {} of free pages stay in the file for new rows; the dashboard (on mains power) or `swarmy compact --vacuum` gives them back once",
            hb(st.file.free_bytes())
        )));
    }
    for d in idx.band_drift(&ctx.prices)? {
        out.push(warn(&format!(
            "{} {}..{}: {} rolled-up rows keep the long-context band of an older price (its threshold changed)",
            d.model,
            d.first,
            d.last,
            fmt::count(d.rows)
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Dirs};
    use crate::core::pricing::PriceBook;
    use crate::index::archive_scenarios::{NOW, populate, refresh};
    use crate::index::compact::CompactOpts;
    use crate::index::scenarios::{World, at};
    use crate::procs::ProcTable;
    use crate::ui::style::{ColorChoice, Painter};

    #[test]
    fn the_storage_section() {
        let w = World::new(&["work", "home", "lab"]);
        populate(&w);
        std::fs::write(
            w.seats[2].dir.join("settings.json"),
            r#"{"cleanupPeriodDays": 60}"#,
        )
        .unwrap();
        let mut idx = w.index("i.sqlite");
        refresh(&w, &mut idx);
        let ctx = Ctx {
            dirs: Dirs {
                data: w.dir.path().join("data"),
                config: w.dir.path().join("config"),
                home: w.dir.path().to_path_buf(),
            },
            config: Config::default(),
            seats: w.seats.clone(),
            seat_info: Vec::new(),
            prices: PriceBook::builtin(),
            procs: ProcTable::empty(),
            painter: Painter::new(ColorChoice::Never),
            mask: false,
            now: at(NOW),
            login_labels: Default::default(),
            login_tiers: Default::default(),
            problems: Vec::new(),
        };
        let text = |idx: &mut Index| storage(&ctx, idx).unwrap().join("\n");
        let before = text(&mut idx);
        assert!(before.contains("auto-vacuum off"), "{before}");
        assert!(before.contains("nothing rolled up yet"), "{before}");
        assert!(before.contains("last compaction never"), "{before}");
        assert!(before.contains("keep_raw_days = 14"), "{before}");
        assert!(
            before.contains(
                "transcripts are deleted after 30 days on 2 seats (cleanupPeriodDays, default), after 60 days on 1 seat (cleanupPeriodDays)"
            ),
            "{before}"
        );
        assert!(
            before.contains("estimated from the requests a day")
                && before.contains("(budget 1024 MB)"),
            "{before}"
        );
        idx.compact(&CompactOpts {
            keep_raw_days: 14,
            now: at(NOW),
            prices: &ctx.prices,
            seats: &ctx.seats,
            max_chunks: None,
        })
        .unwrap();
        let after = text(&mut idx);
        assert!(after.contains("history  since 2026-09-2"), "{after}");
        assert!(after.contains(" in rollups: "), "{after}");
        assert!(before.contains("detail   since 2026-09-21: "), "{before}");
        // From rolled_until: at most a chunk before the local midnight 14 days back.
        let until = idx.storage_report().unwrap().rolled_until;
        let since = crate::index::from_ms(until).with_timezone(&chrono::Local);
        assert!(
            after.contains(&format!("detail   since {}: ", since.format("%Y-%m-%d"))),
            "{after}"
        );
        assert!(
            after.contains("chunks") && after.contains("skipped as already rolled up: 0"),
            "{after}"
        );
        assert!(after.contains("[ OK ] growth"), "under the budget: {after}");
        assert!(after.contains("of free pages stay in the file"), "{after}");
        idx.vacuum_to_incremental(true, None, false)
            .unwrap()
            .unwrap_err();
        let waits = text(&mut idx);
        assert!(
            waits.contains("[WARN] auto-vacuum off: the one-time VACUUM waits for mains power"),
            "{waits}"
        );
    }
}

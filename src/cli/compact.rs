//! `swarmy compact`: rolls per-request detail older than the raw window up into the archive's
//! quarter-hour rollups and gives the space back (`docs/design/storage.md`). The dashboard does
//! the same on its own; this runs it in the foreground, previews it, or proves it on a copy.

use std::io::{IsTerminal, Write};

use chrono::{DateTime, Local, Utc};
use clap::{Args, ValueEnum};

use crate::app::context::Ctx;
use crate::core::period::Period;
use crate::index::compact::{self, CompactOpts, VacuumSkip};
use crate::index::query::{Dim, Filter, Row};
use crate::index::{Index, from_ms};
use crate::ui::fmt;
use crate::ui::style::Tone;

#[derive(Args, Debug, Clone, Default)]
pub struct CompactArgs {
    /// Show what would be rolled up and how much space it would free; change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Days of per-request detail to keep, this once (at least 8).
    #[arg(long, value_name = "DAYS")]
    pub keep_days: Option<i64>,
    /// Also the one-time VACUUM that switches the index to incremental auto-vacuum, so space
    /// freed from now on goes back to the disk (needs twice the index free on disk).
    #[arg(long)]
    pub vacuum: bool,
    /// Compact a scratch copy and check that every total by every grouping stays the same; the
    /// index itself is not changed.
    #[arg(long, conflicts_with_all = ["dry_run", "vacuum"])]
    pub verify: bool,
}

pub fn run(ctx: &Ctx, a: &CompactArgs) -> anyhow::Result<()> {
    // Read what changed first: prices and accounts are settled before anything is rolled up.
    let (mut idx, _) = ctx.index()?;
    run_on(ctx, &mut idx, a, &mut std::io::stdout().lock())
}

/// `swarmy compact` on an open, refreshed index, printing to `out`.
pub fn run_on(
    ctx: &Ctx,
    idx: &mut Index,
    a: &CompactArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let keep = ctx.keep_raw_days(a.keep_days)?;
    if a.verify {
        return verify(ctx, idx, keep, out);
    }
    let p = &ctx.painter;
    let when = |t: i64| {
        from_ms(t)
            .with_timezone(&Local)
            .format("%a %d %b %Y %H:%M")
            .to_string()
    };
    let file = idx.file_stats()?;
    writeln!(
        out,
        "index    {} ({} free) · auto-vacuum {}",
        fmt::human_bytes(file.bytes()),
        fmt::human_bytes(file.free_bytes()),
        if file.incremental {
            "incremental"
        } else {
            "off"
        }
    )?;
    if a.dry_run {
        let v = idx.compact_preview(keep, ctx.now)?;
        writeln!(
            out,
            "keep     {keep} days of per-request detail: everything before {} is rolled up",
            when(v.cutoff)
        )?;
        if v.requests + v.tool_calls + v.prompts + v.api_errors == 0 {
            writeln!(out, "{}", p.paint("nothing to roll up", Tone::Good))?;
            return Ok(());
        }
        writeln!(
            out,
            "would roll up {} requests, {} tool calls, {} prompts, {} API errors into ≈ {} rollup rows",
            fmt::count(v.requests),
            fmt::count(v.tool_calls),
            fmt::count(v.prompts),
            fmt::count(v.api_errors),
            fmt::count(v.usage_rows + v.tool_rows)
        )?;
        writeln!(
            out,
            "would free ≈ {} {}",
            fmt::human_bytes(v.bytes),
            if file.incremental {
                "(given back to the disk)"
            } else {
                "(as free pages new rows reuse; `--vacuum` makes the index give space back)"
            }
        )?;
        if v.pending > 0 {
            writeln!(
                out,
                "{}",
                p.paint(
                    &format!(
                        "{} requests are not priced yet: their days wait for the next refresh",
                        fmt::count(v.pending)
                    ),
                    Tone::Warn
                )
            )?;
        }
        return Ok(());
    }
    let started = std::time::Instant::now();
    let tty = std::io::stderr().is_terminal();
    let mut progress = |r: &compact::CompactReport| {
        if tty {
            eprint!(
                "\r\x1b[2K[ .. ] rolled up {} chunks, {} requests",
                r.chunks,
                fmt::count(r.requests)
            );
        }
    };
    let r = idx.compact_with(
        &CompactOpts {
            keep_raw_days: keep,
            now: ctx.now,
            prices: &ctx.prices,
            seats: &ctx.seats,
            max_chunks: None,
        },
        &mut progress,
    )?;
    if tty && r.chunks > 0 {
        eprint!("\r\x1b[2K");
    }
    if r.chunks == 0 {
        writeln!(
            out,
            "{}",
            p.paint(
                &format!("nothing older than {keep} days to roll up"),
                Tone::Good
            )
        )?;
    } else {
        writeln!(
            out,
            "rolled up {} requests, {} tool calls, {} prompts, {} API errors in {} chunks ({:.1}s)",
            fmt::count(r.requests),
            fmt::count(r.tool_calls),
            fmt::count(r.prompts),
            fmt::count(r.api_errors),
            r.chunks,
            started.elapsed().as_secs_f64()
        )?;
    }
    if let Some(w) = r.waiting {
        writeln!(
            out,
            "{}",
            p.paint(
                &format!(
                    "stopped at {}: requests not priced yet (the next refresh prices them)",
                    when(w)
                ),
                Tone::Warn
            )
        )?;
    }
    if r.rolled_until > 0 {
        writeln!(out, "detail kept from {} on", when(r.rolled_until))?;
    }
    if r.freed_bytes > 0 {
        writeln!(out, "freed {}", fmt::human_bytes(r.freed_bytes))?;
    }
    if a.vacuum {
        match idx.vacuum_to_incremental(false, compact::free_disk(&ctx.dirs.data), true)? {
            Ok(shrank) => writeln!(
                out,
                "VACUUM: auto-vacuum is incremental now; the index shrank by {}",
                fmt::human_bytes(shrank)
            )?,
            Err(VacuumSkip::Done) => {}
            Err(skip) => writeln!(
                out,
                "{}",
                p.paint(&format!("VACUUM skipped: {}", skip.describe()), Tone::Warn)
            )?,
        }
    } else if !idx.file_stats()?.incremental && idx.file_stats()?.free_pages > 0 {
        writeln!(
            out,
            "{}",
            p.paint(
                "the freed pages stay inside the index for new rows; `swarmy compact --vacuum` gives them back once",
                Tone::Dim
            )
        )?;
    }
    writeln!(
        out,
        "index    {}",
        fmt::human_bytes(idx.file_stats()?.bytes())
    )?;
    Ok(())
}

/// Every total a report can show, by period, grouping and key.
type Numbers = std::collections::BTreeMap<String, Row>;

/// The periods `--verify` compares: all, rolling ones reaching into the archive or ending at its
/// edge, a calendar month, and today (raw only). Each grouping over all of the index costs
/// about a third of a CPU-second on a month of heavy use, so the list stays short.
fn periods() -> [Period; 5] {
    [
        Period::All,
        Period::Days(30),
        Period::Month,
        Period::Days(14),
        Period::Today,
    ]
}

fn numbers(idx: &Index, now: DateTime<Utc>) -> anyhow::Result<Numbers> {
    let f = Filter::default();
    let mut n = Numbers::new();
    for p in periods() {
        let span = p.span(now, None);
        n.insert(format!("{} · total", p.label()), idx.total(span, &f)?);
        for dim in Dim::value_variants() {
            for row in idx.spend_by(*dim, span, &f)? {
                n.insert(format!("{} · {dim:?} · {}", p.label(), row.key), row);
            }
        }
    }
    Ok(n)
}

/// What differs between two sets of numbers: counts and tokens must be equal, money agree to
/// nine significant digits (sums over other groupings add in another order).
fn differences(before: &Numbers, after: &Numbers) -> Vec<String> {
    let mut out = Vec::new();
    for (k, a) in before {
        let Some(b) = after.get(k) else {
            out.push(format!("{k}: gone"));
            continue;
        };
        let same_money = (a.cost - b.cost).abs() <= 1e-9 * a.cost.abs().max(b.cost.abs()).max(1.0);
        let counts = |r: &Row| (r.requests, r.unpriced, r.pending, r.usage);
        if !same_money || counts(a) != counts(b) {
            out.push(format!(
                "{k}: {} requests {} → {} requests {}",
                a.requests,
                fmt::money(a.cost),
                b.requests,
                fmt::money(b.cost)
            ));
        }
    }
    out.extend(
        after
            .keys()
            .filter(|k| !before.contains_key(*k))
            .map(|k| format!("{k}: new")),
    );
    out
}

/// Compacts a scratch copy of the index (in the data dir) and compares every total by every
/// grouping before and after. The index itself is only read.
fn verify(ctx: &Ctx, idx: &Index, keep: i64, out: &mut dyn Write) -> anyhow::Result<()> {
    let dir = ctx.dirs.data.join("tmp");
    std::fs::create_dir_all(&dir)?;
    // Per run: two `compact --verify` at once never share a copy (review A-L2).
    let scratch = dir.join(format!(
        "verify-{}-{}.sqlite",
        std::process::id(),
        ctx.now.timestamp_millis()
    ));
    let remove = || {
        for ext in ["", "-wal", "-shm"] {
            let p = std::path::PathBuf::from(format!("{}{ext}", scratch.display()));
            if p.exists() {
                std::fs::remove_file(&p)?;
            }
        }
        anyhow::Ok(())
    };
    remove()?;
    idx.copy_to(&scratch)?;
    let tty = std::io::stderr().is_terminal();
    let step = |what: &str| {
        if tty {
            eprint!("\r\x1b[2K[ .. ] verify: {what}");
        }
    };
    let result = (|| {
        let mut copy = Index::open(&scratch)?;
        step("every total before");
        let before = numbers(&copy, ctx.now)?;
        step("compacting the copy");
        let r = copy.compact(&CompactOpts {
            keep_raw_days: keep,
            now: ctx.now,
            prices: &ctx.prices,
            seats: &ctx.seats,
            max_chunks: None,
        })?;
        step("every total after");
        let after = numbers(&copy, ctx.now)?;
        anyhow::Ok((before.len(), r, differences(&before, &after)))
    })();
    if tty {
        eprint!("\r\x1b[2K");
    }
    remove()?;
    let (n, r, diffs) = result?;
    let p = &ctx.painter;
    writeln!(
        out,
        "verify   on a scratch copy (the index is not changed): rolled up {} requests, {} tool calls",
        fmt::count(r.requests),
        fmt::count(r.tool_calls)
    )?;
    let groupings = Dim::value_variants().len();
    if diffs.is_empty() {
        writeln!(
            out,
            "{}",
            p.paint(
                &format!(
                    "every total is the same: {} numbers ({groupings} groupings × {} periods, and totals), 0 differences",
                    fmt::count(n as u64),
                    periods().len()
                ),
                Tone::Good
            )
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "{}",
        p.paint(
            &format!("{} differences in {} numbers:", diffs.len(), n),
            Tone::Bad
        )
    )?;
    for d in diffs.iter().take(20) {
        writeln!(out, "  {d}")?;
    }
    anyhow::bail!("compaction would change numbers; the index was not touched")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Dirs};
    use crate::core::pricing::PriceBook;
    use crate::index::archive_scenarios::{NOW, populate, readings, refresh};
    use crate::index::query::Counted;
    use crate::index::scenarios::{World, at};
    use crate::procs::ProcTable;
    use crate::ui::style::{ColorChoice, Painter};

    fn ctx(w: &World) -> Ctx {
        Ctx {
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
        }
    }

    fn run(ctx: &Ctx, idx: &mut Index, a: CompactArgs) -> anyhow::Result<String> {
        let mut out = Vec::new();
        run_on(ctx, idx, &a, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn rows(idx: &Index) -> (u64, u64) {
        (
            idx.count(Counted::Requests).unwrap(),
            idx.count(Counted::UsageRollup).unwrap(),
        )
    }

    #[test]
    fn dry_run_verify_then_compact_on_a_temp_index() {
        let w = World::new(&["work", "home", "lab"]);
        populate(&w);
        let mut idx = w.index("i.sqlite");
        refresh(&w, &mut idx);
        readings(&idx);
        let ctx = ctx(&w);
        let raw = rows(&idx);
        assert_eq!(raw.1, 0);

        let out = run(
            &ctx,
            &mut idx,
            CompactArgs {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            out.contains("would roll up") && out.contains("would free ≈"),
            "{out}"
        );
        // The local midnight 14 days back, whatever the zone the test runs in.
        let cut = from_ms(compact::cutoff(at(NOW), 14)).with_timezone(&Local);
        assert!(
            out.contains(&format!(
                "everything before {}",
                cut.format("%a %d %b %Y 00:00")
            )),
            "{out}"
        );
        assert_eq!(rows(&idx), raw, "a dry run changes nothing");

        let out = run(
            &ctx,
            &mut idx,
            CompactArgs {
                verify: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(out.contains("0 differences"), "{out}");
        assert_eq!(rows(&idx), raw, "verify works on a copy");
        assert_eq!(
            std::fs::read_dir(ctx.dirs.data.join("tmp"))
                .unwrap()
                .count(),
            0,
            "the scratch copy is gone"
        );

        let short = CompactArgs {
            keep_days: Some(7),
            ..Default::default()
        };
        assert!(
            run(&ctx, &mut idx, short).is_err(),
            "the weekly window stays raw"
        );
        // Claude Code keeps these seats' transcripts 30 days: a rebuild must find 28 on disk.
        let long = CompactArgs {
            keep_days: Some(29),
            ..Default::default()
        };
        let e = run(&ctx, &mut idx, long).unwrap_err().to_string();
        assert!(
            e.starts_with("--keep-days: keep_raw_days = 29: at most 28"),
            "{e}"
        );
        let mut strict = super::tests::ctx(&w);
        strict.config.storage.keep_raw_days = Some(40);
        let err = run(&strict, &mut idx, CompactArgs::default()).unwrap_err();
        let e = err.to_string();
        assert!(e.contains("[storage] keep_raw_days = 40"), "{e}");
        // A config error with its code (review B-L10).
        assert!(matches!(
            err.downcast_ref::<crate::errors::ConfigError>(),
            Some(crate::errors::ConfigError::Invalid { .. })
        ));
        assert_eq!(rows(&idx), raw, "refused before anything moved");

        let out = run(
            &ctx,
            &mut idx,
            CompactArgs {
                vacuum: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(out.contains("rolled up") && out.contains("chunks"), "{out}");
        assert!(
            out.contains("VACUUM: auto-vacuum is incremental now"),
            "{out}"
        );
        let (left, rolled) = rows(&idx);
        assert!(left < raw.0 && rolled > 0);
        assert!(idx.file_stats().unwrap().incremental);

        let out = run(&ctx, &mut idx, CompactArgs::default()).unwrap();
        assert!(
            out.contains("nothing older than 14 days to roll up"),
            "{out}"
        );
    }
}

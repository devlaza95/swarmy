//! `swarmy prices`: the price in force for every model, and where it comes from.

use std::collections::BTreeMap;

use crate::core::models::{Family, short_name};
use crate::ui::fmt;
use crate::ui::style::Tone;
use crate::ui::table::{Align, Cell, Table};

use super::Format;
use crate::app::context::Ctx;

pub fn run(ctx: &Ctx, format: Format) -> anyhow::Result<()> {
    let today = ctx.now.with_timezone(&chrono::Local).date_naive();
    // Claude Code's prices carry the day the index first saw them.
    let mut idx = crate::index::Index::open(&ctx.dirs.index_file())?;
    idx.set_now(ctx.now);
    let prices = idx.dated_prices(&ctx.prices, ctx.now)?;
    // A cached price names the seat it came from; seats gone since keep their last label.
    let gone: BTreeMap<_, _> = idx.seat_labels()?.into_iter().collect();
    let seat_label = |id: &crate::core::ids::SeatId| match ctx.seats.iter().find(|s| &s.id == id) {
        Some(s) => s.label.clone(),
        None => gone.get(id).cloned().unwrap_or_else(|| id.to_string()),
    };
    let mut models: BTreeMap<String, ()> = BTreeMap::new();
    for e in prices.entries() {
        if !e.model.ends_with('*') {
            models.insert(e.model.clone(), ());
        }
    }
    let rows: Vec<_> = models
        .keys()
        .filter_map(|m| prices.lookup(m, today).map(|e| (m.clone(), e.clone())))
        .collect();
    if format == Format::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows.iter().map(|(_, e)| e).collect::<Vec<_>>())?
        );
        return Ok(());
    }
    let p = &ctx.painter;
    println!(
        "{} {}",
        p.paint("PRICES", Tone::Header),
        p.paint("· USD per million tokens, in force today", Tone::Dim)
    );
    println!();
    let mut t = Table::new(&[
        ("MODEL", Align::Left),
        ("INPUT", Align::Right),
        ("OUTPUT", Align::Right),
        ("WRITE 5M", Align::Right),
        ("WRITE 1H", Align::Right),
        ("READ", Align::Right),
        ("FAST", Align::Right),
        ("SOURCE", Align::Left),
    ]);
    let mut sorted = rows;
    sorted.sort_by(|a, b| {
        Family::of(&a.0)
            .cmp(&Family::of(&b.0))
            .then(b.1.rates.input.total_cmp(&a.1.rates.input))
            .then(a.0.cmp(&b.0))
    });
    for (m, e) in &sorted {
        let r = e.rates;
        let n = |x: f64| {
            if x >= 1.0 {
                format!("{x:.2}")
            } else {
                format!("{x:.3}")
            }
        };
        t.row(vec![
            Cell::tone(
                format!("{} ({m})", short_name(m)),
                Tone::of_family(Family::of(m)),
            ),
            Cell::new(n(r.input)),
            Cell::new(n(r.output)),
            Cell::new(n(r.cache_write_5m)),
            Cell::new(n(r.cache_write_1h)),
            Cell::new(n(r.cache_read)),
            Cell::new(
                r.fast_multiplier
                    .map(|x| format!("×{x}"))
                    .unwrap_or_else(|| "-".into()),
            ),
            Cell::tone(e.source.label(seat_label), Tone::Dim),
        ]);
    }
    println!("{}", t.render(p));
    println!(
        "{}",
        p.paint(
            &format!(
                "web search {} per request · override in {} with [[price]]",
                fmt::money(0.01),
                ctx.show_path(&ctx.dirs.config_file().to_string_lossy())
            ),
            Tone::Dim
        )
    );
    Ok(())
}

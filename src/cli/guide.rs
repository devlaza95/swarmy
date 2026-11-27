//! `swarmy guide`: the first five minutes, in order. Static text: it reads nothing from the
//! machine, so it is safe to run anywhere, before anything is set up.

use super::explain::{dim_paragraph, hanging, text_width, wrap};
use crate::ui::style::{Painter, Tone};

/// (command, what it does and when to use it)
const STEPS: &[(&str, &str)] = &[
    (
        "swarmy demo",
        "Look around on a made-up world of agents. No real data is read, nothing is written.",
    ),
    (
        "swarmy doctor",
        "What swarmy found on this machine (config dirs, transcripts, logins) and whether it \
         can read it. Run it first when something looks empty: it ends with NEXT STEPS, what to \
         do about each problem.",
    ),
    (
        "swarmy",
        "The live dashboard. Keys: ? help and legend, Tab next panel, n next agent that needs \
         you, J jump to its terminal, / filter, t change the period, q quit.",
    ),
    (
        "swarmy limits",
        "Each account's weekly and 5-hour limits: how much is used, which projects used it, \
         and when it runs out at the current pace.",
    ),
    (
        "swarmy report --by project",
        "Where the money went (API-equivalent, not your bill). Group by project, model, day, \
         hour, session, branch and more; add --period today|week|month|all|7d.",
    ),
    (
        "swarmy web",
        "The same live view, limits, spend and glossary in a browser tab at 127.0.0.1:7878. \
         This computer only, read-only. Try it on made-up data with `swarmy web --demo`.",
    ),
    (
        "swarmy setup",
        "Optional. Adds swarmy's status line to Claude Code so limit numbers are exact instead \
         of estimated. Asks before it changes anything and backs up the file first.",
    ),
    (
        "swarmy explain",
        "Plain-language meaning of every word and glyph on screen. `swarmy explain stalled`, \
         `swarmy explain ≈`.",
    ),
];

pub fn render(p: &Painter, width: usize) -> String {
    let mut out = String::new();
    let head = |out: &mut String, t: &str| {
        out.push_str(&p.paint(t, Tone::Header));
        out.push('\n');
    };
    let para = |out: &mut String, t: &str| {
        for l in wrap(t, width.saturating_sub(2), width.saturating_sub(2)) {
            out.push_str("  ");
            out.push_str(&l);
            out.push('\n');
        }
    };
    head(&mut out, "WHAT SWARMY IS");
    para(
        &mut out,
        "A dashboard for people who run several Claude Code sessions at once. One screen shows \
         every session and subagent, tells you the moment one needs you, and tracks where your \
         weekly limit and your spend go.",
    );
    out.push('\n');
    head(&mut out, "WHAT IT TOUCHES");
    para(
        &mut out,
        "It reads files Claude Code already writes: transcripts, the session registry and \
         .claude.json. It makes no outbound network calls and sends no telemetry. It writes in \
         Claude Code's folders only when you ask: a login switch in the vault, or the status \
         line you accept in `swarmy setup`. Both back up first.",
    );
    out.push('\n');
    head(&mut out, "THE FIRST FIVE MINUTES");
    let labels: Vec<String> = STEPS
        .iter()
        .enumerate()
        .map(|(i, (cmd, _))| format!("{}. {cmd}", i + 1))
        .collect();
    let label_w = labels.iter().map(|l| l.len()).max().unwrap_or(10) + 2;
    for (label, (_, what)) in labels.iter().zip(STEPS) {
        out.push_str(&hanging(p, label, label_w, what, width));
    }
    out.push('\n');
    head(&mut out, "THREE IDEAS THAT EXPLAIN MOST OF THE SCREEN");
    for t in [
        "Cost is API-equivalent: the tokens used, priced at list prices. A subscription does not bill \
         it; it tells you how much work you got.",
        "A seat is one Claude Code config folder, a login is a sign-in, an account owns the \
         limit. Two seats on one account share one weekly limit.",
        "A number with ≈ is an estimate between two limit readings. The optional status line \
         turns estimates into exact readings.",
    ] {
        out.push_str("  • ");
        let room = width.saturating_sub(4);
        out.push_str(&wrap(t, room, room).join("\n    "));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&dim_paragraph(
        p,
        "Stuck? `swarmy doctor` says what it found. `swarmy --help` lists every command.",
        width,
    ));
    out
}

pub fn run(p: &Painter) -> anyhow::Result<()> {
    print!("{}", render(p, text_width()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn the_guide_fits_the_width_and_names_every_step() {
        for width in [60, 80, 100] {
            let out = render(&Painter::plain(), width);
            for (cmd, _) in STEPS {
                assert!(out.contains(cmd), "{cmd} missing at {width}");
            }
            let too_wide: Vec<&str> = out.lines().filter(|l| l.width() > width).collect();
            assert!(too_wide.is_empty(), "width {width}: {too_wide:#?}");
        }
    }

    /// The guide only recommends commands that exist (`swarmy --help` is the authority).
    #[test]
    fn every_command_it_recommends_is_a_real_subcommand() {
        use clap::CommandFactory;
        let cli = super::super::Cli::command();
        let known: Vec<&str> = cli.get_subcommands().map(|c| c.get_name()).collect();
        for (cmd, _) in STEPS {
            let Some(name) = cmd.split_whitespace().nth(1) else {
                continue; // plain `swarmy`: the dashboard
            };
            assert!(
                known.contains(&name),
                "`{cmd}`: no such subcommand ({known:?})"
            );
        }
    }
}

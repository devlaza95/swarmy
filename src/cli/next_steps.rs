//! The NEXT STEPS section that ends `swarmy doctor`: for each thing doctor found wrong or
//! missing, what it means in plain words and the exact command or action that fixes it.
//!
//! Pure: [`Findings`] holds the facts `doctor` gathered, [`steps`] decides which steps apply and
//! in which order, [`render`] lays them out. Nothing here reads the machine, so every branch is
//! tested on made-up facts. The words come from the glossary (`ui::glossary`): each step names
//! the term it is about, and the section prints that term's one-line meaning and the
//! `swarmy explain <term>` that tells more.

use unicode_width::UnicodeWidthStr;

use super::explain::{dim_paragraph, wrap};
use crate::core::harness::Harness;
use crate::core::seat_use::SeatUse;
use crate::ui::glossary;
use crate::ui::style::{Painter, Tone};

/// What doctor found, shaped as problems: the default value is a machine with nothing to fix.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Findings {
    /// No config dir of any harness was found.
    pub no_seats: bool,
    /// Things swarmy could not read while setting up (`Ctx::problems`).
    pub unreadable_setup: usize,
    /// `claude` processes running in the default config dir, which was not found.
    pub unseen_default_dir: usize,
    /// Seats that hold no login (label, harness) although they are in use.
    pub signed_out: Vec<(String, Harness)>,
    /// Seats were found but the index has not read a single transcript.
    pub no_transcripts: bool,
    /// Transcripts were found but the index counted no request from them.
    pub no_requests: bool,
    /// Transcript files that could not be read.
    pub unreadable_files: usize,
    /// Transcript records that were damaged.
    pub damaged_records: u64,
    /// Rows a cache rebuild left out (the count `doctor` reads from the index).
    pub rebuild_refused: Option<String>,
    /// Record types swarmy does not know.
    pub unknown_types: Vec<String>,
    /// Requests of the last 7 days with no known price.
    pub unpriced: u64,
    /// Claude Code's cost count and swarmy's differ by more than 5%.
    pub cost_mismatch: bool,
    /// Claude Code seats are in use but no weekly limit reading exists.
    pub no_limit_readings: bool,
    /// The index would pass its size budget within 12 months.
    pub index_over_budget: bool,
    /// Why the one-time VACUUM has not run, when it waits.
    pub vacuum_waiting: Option<String>,
    /// Claude Code seats are in use and none has swarmy's status line.
    pub no_status_line: bool,
}

/// Whether a step repairs something or only improves something that works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fix,
    Optional,
}

/// One thing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub kind: Kind,
    /// The glossary term the step is about (`swarmy explain <term>`).
    pub term: &'static str,
    /// What was found, in plain words.
    pub means: String,
    /// What to do: the exact command or action.
    pub fix: String,
}

/// Whether a seat holds no login although something uses it (a transcript, a running session, a
/// `[[seat]]` entry). A seat that is empty in every way is left out: the dashboard hides it too.
pub fn in_use_without_login(u: &SeatUse) -> bool {
    !u.login && !u.account && !u.is_empty()
}

/// The steps that apply to `f`, most important first.
pub fn steps(f: &Findings) -> Vec<Step> {
    let mut out = Vec::new();
    if f.no_seats {
        out.push(no_seats());
    }
    if f.unreadable_setup > 0 {
        out.push(unreadable_setup(f.unreadable_setup));
    }
    if f.unseen_default_dir > 0 {
        out.push(unseen_default_dir(f.unseen_default_dir));
    }
    out.extend(f.signed_out.iter().map(|(l, h)| signed_out(l, *h)));
    // Without a seat there is nothing to read: these would only repeat the first step.
    if !f.no_seats {
        if f.no_transcripts {
            out.push(no_transcripts());
        } else if f.no_requests {
            out.push(no_requests());
        }
    }
    if f.unreadable_files > 0 {
        out.push(unreadable_files(f.unreadable_files));
    }
    if f.damaged_records > 0 {
        out.push(damaged_records(f.damaged_records));
    }
    if let Some(n) = &f.rebuild_refused {
        out.push(rebuild_refused(n));
    }
    if !f.unknown_types.is_empty() {
        out.push(unknown_types(&f.unknown_types));
    }
    if f.unpriced > 0 {
        out.push(unpriced(f.unpriced));
    }
    if f.cost_mismatch {
        out.push(cost_mismatch());
    }
    if f.no_limit_readings && !f.no_seats {
        out.push(no_limit_readings());
    }
    if f.index_over_budget {
        out.push(index_over_budget());
    }
    if let Some(why) = &f.vacuum_waiting {
        out.push(vacuum_waiting(why));
    }
    if f.no_status_line && !f.no_seats {
        out.push(no_status_line());
    }
    out
}

fn fix(term: &'static str, means: String, fix: String) -> Step {
    Step {
        kind: Kind::Fix,
        term,
        means,
        fix,
    }
}

/// "3 files" / "1 file".
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn no_seats() -> Step {
    fix(
        "seat",
        "swarmy found no seat, so there is nothing to show yet.".into(),
        "Start Claude Code, Antigravity or Codex once and exit: that creates its folder \
         (~/.claude, ~/.gemini/antigravity-cli or ~/.codex), then run `swarmy doctor` again. If \
         your folder is somewhere else, add it to config.toml (the CONFIG section above gives \
         its path) as `[[seat]]` with `path = \"~/my-agent-dir\"`. To look around first: \
         `swarmy demo`."
            .into(),
    )
}

fn unreadable_setup(n: usize) -> Step {
    fix(
        "seat",
        format!(
            "swarmy could not read {} of your setup (the warnings under SEATS say which), so \
             numbers that depend on {} may be off.",
            count(n as u64, "thing", "things"),
            if n == 1 { "it" } else { "them" }
        ),
        "Open the file or entry each warning names, repair or remove it, then run `swarmy \
         doctor` again."
            .into(),
    )
}

fn unseen_default_dir(n: usize) -> Step {
    fix(
        "seat",
        format!(
            "{} the default Claude Code folder, which swarmy did not find, so {} not shown.",
            if n == 1 {
                "1 `claude` process uses"
            } else {
                "Some `claude` processes use"
            },
            if n == 1 {
                "its session is"
            } else {
                "their sessions are"
            }
        ),
        "Add it as a seat in config.toml: `[[seat]]` with `path = \"~/.claude\"`, then run \
         `swarmy doctor` again."
            .into(),
    )
}

/// How to sign in to a harness, in the words of its own CLI.
fn sign_in(h: Harness) -> &'static str {
    match h {
        Harness::Claude => {
            "Start `claude` in that seat (set `CLAUDE_CONFIG_DIR` to its DIR unless it is \
             ~/.claude) and run `/login`"
        }
        Harness::Agy => "Sign in with the `agy` CLI",
        Harness::Codex => "Run `codex login` (set `CODEX_HOME` to its DIR unless it is ~/.codex)",
    }
}

fn signed_out(label: &str, h: Harness) -> Step {
    fix(
        "login",
        format!(
            "seat '{label}' ({}) holds no login, so swarmy cannot tell which account it belongs \
             to.",
            h.display_name()
        ),
        format!(
            "{}. Or put a saved login into it: `swarmy accounts switch {label} <login>` \
             (`swarmy accounts` lists the saved ones).",
            sign_in(h)
        ),
    )
}

fn no_transcripts() -> Step {
    fix(
        "seat",
        "swarmy found your seats but not one transcript in them, so nothing has been counted \
         yet: no spend, no history."
            .into(),
        "Use the agent in one of the seats until it has answered once, then run `swarmy doctor` \
         again. To see a full dashboard right now: `swarmy demo`."
            .into(),
    )
}

fn no_requests() -> Step {
    fix(
        "index",
        "swarmy found transcripts but the index holds no request from them: they hold no model \
         response yet, or they could not be read."
            .into(),
        "If a warning above says swarmy cannot read a file, fix that first. Otherwise let a \
         session finish one response and run `swarmy doctor` again. To see a full dashboard \
         right now: `swarmy demo`."
            .into(),
    )
}

fn unreadable_files(n: usize) -> Step {
    fix(
        "index",
        format!(
            "swarmy cannot read {} (named under INDEX above), so nothing in {} is counted.",
            count(n as u64, "transcript file", "transcript files"),
            if n == 1 { "it" } else { "them" }
        ),
        "Check that the files exist and that your user may read them. swarmy tries again at \
         every run; run `swarmy doctor` once that is done."
            .into(),
    )
}

fn damaged_records(n: u64) -> Step {
    fix(
        "index",
        format!(
            "{} (not valid JSON, or numbers out of range), so what {} is not counted.",
            count(
                n,
                "transcript record is damaged",
                "transcript records are damaged"
            ),
            if n == 1 { "it held" } else { "they held" }
        ),
        "Nothing to repair on your side. If the count keeps growing, the agent may have changed \
         its file format: install the newest swarmy release and run `swarmy doctor` again."
            .into(),
    )
}

fn rebuild_refused(n: &str) -> Step {
    fix(
        "index",
        format!(
            "A rebuild of the index left out {n} kept rows that the new schema refused, so \
             their spend is missing from every report."
        ),
        "Those rows cannot be rebuilt: their transcripts are gone. Report it as a bug and keep \
         the `swarmy doctor` output; spend from before the rebuild may read low."
            .into(),
    )
}

fn unknown_types(types: &[String]) -> Step {
    fix(
        "harness",
        format!(
            "swarmy does not recognise these record types: {}. The agent's file format may have \
             moved on, and swarmy ignores what they carry.",
            types.join(", ")
        ),
        "Install the newest swarmy release and run `swarmy doctor` again. If the same names are \
         still listed, report them."
            .into(),
    )
}

fn unpriced(n: u64) -> Step {
    fix(
        "unpriced",
        format!(
            "{} of the last 7 days use a model with no known price, so they show as unpriced (—) \
             instead of a cost.",
            count(n, "request", "requests")
        ),
        "`swarmy prices` lists the prices in force. Add the missing model to config.toml with a \
         `[[price]]` entry (`model`, `from`, `input`, `output`: USD per million tokens): it is \
         priced from then on, past requests included."
            .into(),
    )
}

fn cost_mismatch() -> Step {
    fix(
        "api-equivalent cost",
        "Claude Code's own cost count and swarmy's differ by more than 5% over the last 7 days."
            .into(),
        "A small gap is normal: background calls such as titles are not in the transcripts. For \
         a bigger one, `swarmy prices` shows which price each model uses and where it came \
         from; a wrong or outdated price is the first thing to rule out."
            .into(),
    )
}

fn no_limit_readings() -> Step {
    fix(
        "estimate",
        "swarmy has no reading of your weekly limit yet, so it cannot show how much of the week \
         is used."
            .into(),
        "Open `/usage` in Claude Code once: it caches a reading. For an exact reading after \
         every response, add the status line with `swarmy setup` (optional)."
            .into(),
    )
}

fn index_over_budget() -> Step {
    fix(
        "index",
        "At its current growth the index would pass its size budget within 12 months.".into(),
        "`swarmy compact --dry-run` shows what rolling old detail up would free; `swarmy \
         compact` does it. Or lower `keep_raw_days`, or raise `budget_mb`, under `[storage]` in \
         config.toml."
            .into(),
    )
}

fn vacuum_waiting(why: &str) -> Step {
    fix(
        "index",
        format!(
            "The index file still holds free space it could give back: the one-time VACUUM {why}."
        ),
        "`swarmy compact --vacuum` runs it now.".into(),
    )
}

fn no_status_line() -> Step {
    Step {
        kind: Kind::Optional,
        term: "estimate",
        means: "Limits are estimates (≈) between the readings Claude Code caches.".into(),
        fix: "`swarmy setup` adds swarmy's status line to Claude Code for exact numbers after \
              every response. It asks before changing anything and backs up the file first."
            .into(),
    }
}

/// `label` in `tone`, then `text` wrapped to `width` with its lines aligned after the label.
fn labelled(
    p: &Painter,
    indent: usize,
    label: &str,
    text: &str,
    width: usize,
    tone: Tone,
) -> String {
    let lead = indent + label.width() + 1;
    let room = width.saturating_sub(lead).max(20);
    format!(
        "{}{} {}\n",
        " ".repeat(indent),
        p.paint(label, tone),
        wrap(text, room, room).join(&format!("\n{}", " ".repeat(lead)))
    )
}

fn block(p: &Painter, width: usize, number: usize, digits: usize, s: &Step) -> String {
    let lead = 2 + digits + 2;
    let room = width.saturating_sub(lead).max(20);
    let mut out = format!(
        "  {number:>digits$}. {}\n",
        wrap(&s.means, room, room).join(&format!("\n{}", " ".repeat(lead)))
    );
    if let Some(t) = glossary::by_name(s.term) {
        let line = format!("{}: {} (swarmy explain {})", t.name, t.short, t.name);
        out.push_str(&p.paint(
            &format!(
                "{}{}\n",
                " ".repeat(lead),
                wrap(&line, room, room).join(&format!("\n{}", " ".repeat(lead)))
            ),
            Tone::Dim,
        ));
    }
    let (label, tone) = match s.kind {
        Kind::Fix => ("Fix:", Tone::Bold),
        Kind::Optional => ("Optional:", Tone::Dim),
    };
    out.push_str(&labelled(p, lead, label, &s.fix, width, tone));
    out
}

/// The body of the section (its heading is printed by the caller): the steps, or the all-clear.
pub fn render(p: &Painter, width: usize, steps: &[Step]) -> String {
    let nothing_to_fix = steps.iter().all(|s| s.kind == Kind::Optional);
    let mut out = String::new();
    if nothing_to_fix {
        // "[ OK ] " is 7 columns: the lines after the first line up under the text.
        let room = width.saturating_sub(7).max(20);
        out.push_str(&format!(
            "{} {}\n",
            p.paint("[ OK ]", Tone::Good),
            wrap(
                "Nothing needs fixing: swarmy found your seats, can read them, and has data to \
                 show.",
                room,
                room
            )
            .join("\n       ")
        ));
    }
    let digits = steps.len().to_string().len();
    for (i, s) in steps.iter().enumerate() {
        out.push_str(&block(p, width, i + 1, digits, s));
    }
    let closing = if nothing_to_fix {
        "New here? `swarmy guide` walks through the first five minutes; `swarmy` opens the live \
         dashboard."
    } else {
        "When you have done these, run `swarmy doctor` again to check."
    };
    out.push_str(&dim_paragraph(p, closing, width));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field set: each step at once.
    fn everything() -> Findings {
        Findings {
            no_seats: false,
            unreadable_setup: 2,
            unseen_default_dir: 1,
            signed_out: vec![
                ("alpha".into(), Harness::Claude),
                ("beta".into(), Harness::Agy),
                ("gamma".into(), Harness::Codex),
            ],
            no_transcripts: false,
            no_requests: true,
            unreadable_files: 3,
            damaged_records: 4,
            rebuild_refused: Some("5".into()),
            unknown_types: vec!["mystery (2)".into()],
            unpriced: 6,
            cost_mismatch: true,
            no_limit_readings: true,
            index_over_budget: true,
            vacuum_waiting: Some("waits for mains power".into()),
            no_status_line: true,
        }
    }

    fn plain(f: &Findings) -> String {
        render(&Painter::plain(), 80, &steps(f))
    }

    /// The steps of `f` and the section as one line of single-spaced text: a phrase is found
    /// wherever the layout wrapped it.
    fn only(f: Findings) -> (Vec<Step>, String) {
        let s = steps(&f);
        let text = render(&Painter::plain(), 80, &s);
        (s, text.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    #[test]
    fn a_machine_with_nothing_to_fix_is_told_so_and_pointed_to_the_guide() {
        let (s, text) = only(Findings::default());
        assert!(s.is_empty());
        assert!(text.contains("[ OK ] Nothing needs fixing"), "{text}");
        assert!(text.contains("`swarmy guide`"), "{text}");
        assert!(!text.contains("Fix:"), "{text}");
    }

    #[test]
    fn no_seat_points_at_the_demo_and_the_config_entry() {
        let (s, text) = only(Findings {
            no_seats: true,
            // What follows from having no seat is not said again.
            no_transcripts: true,
            no_requests: true,
            no_limit_readings: true,
            no_status_line: true,
            ..Findings::default()
        });
        assert_eq!(s.len(), 1, "{text}");
        assert_eq!(s[0].kind, Kind::Fix);
        assert!(text.contains("swarmy found no seat"), "{text}");
        assert!(text.contains("`swarmy demo`"), "{text}");
        assert!(text.contains("`[[seat]]`"), "{text}");
        assert!(
            text.contains("seat: one Claude Code config directory"),
            "{text}"
        );
        assert!(text.contains("swarmy explain seat"), "{text}");
        assert!(!text.contains("Nothing needs fixing"), "{text}");
        assert!(text.contains("run `swarmy doctor` again"), "{text}");
    }

    #[test]
    fn unreadable_setup_says_to_follow_the_warnings() {
        let (_, one) = only(Findings {
            unreadable_setup: 1,
            ..Findings::default()
        });
        assert!(
            one.contains("could not read 1 thing of your setup"),
            "{one}"
        );
        assert!(one.contains("depend on it"), "{one}");
        let (_, two) = only(Findings {
            unreadable_setup: 2,
            ..Findings::default()
        });
        assert!(two.contains("could not read 2 things"), "{two}");
        assert!(two.contains("warnings under SEATS"), "{two}");
    }

    #[test]
    fn a_default_dir_that_was_not_found_is_added_as_a_seat() {
        let (_, one) = only(Findings {
            unseen_default_dir: 1,
            ..Findings::default()
        });
        assert!(one.contains("1 `claude` process uses the default"), "{one}");
        assert!(one.contains("path = \"~/.claude\""), "{one}");
        let (_, many) = only(Findings {
            unseen_default_dir: 3,
            ..Findings::default()
        });
        assert!(many.contains("Some `claude` processes use"), "{many}");
    }

    #[test]
    fn a_seat_without_a_login_gets_the_sign_in_of_its_own_harness() {
        for (h, name, how) in [
            (Harness::Claude, "Claude Code", "run `/login`"),
            (Harness::Agy, "Antigravity", "`agy` CLI"),
            (Harness::Codex, "Codex", "`codex login`"),
        ] {
            let (s, text) = only(Findings {
                signed_out: vec![("work".into(), h)],
                ..Findings::default()
            });
            assert_eq!(s.len(), 1, "{text}");
            assert_eq!(s[0].term, "login");
            assert!(
                text.contains(&format!("seat 'work' ({name}) holds no login")),
                "{text}"
            );
            assert!(text.contains(how), "{h:?}: {text}");
            assert!(
                text.contains("`swarmy accounts switch work <login>`"),
                "{text}"
            );
            assert!(text.contains("login: an OAuth sign-in"), "{text}");
        }
        // One step per seat.
        let (s, _) = only(Findings {
            signed_out: vec![("a".into(), Harness::Claude), ("b".into(), Harness::Codex)],
            ..Findings::default()
        });
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn only_a_seat_in_use_counts_as_signed_out() {
        let empty = SeatUse::default();
        // Nothing in use: the dashboard hides it, so it is not a step.
        assert!(!in_use_without_login(&empty));
        // In use and no login, in each way a seat can be in use.
        for u in [
            SeatUse {
                transcripts: true,
                ..empty
            },
            SeatUse { live: 1, ..empty },
            SeatUse {
                configured: true,
                ..empty
            },
        ] {
            assert!(in_use_without_login(&u), "{u:?}");
        }
        // A login or an account, even without the other, is a sign-in.
        for u in [
            SeatUse {
                transcripts: true,
                login: true,
                ..empty
            },
            SeatUse {
                transcripts: true,
                account: true,
                ..empty
            },
        ] {
            assert!(!in_use_without_login(&u), "{u:?}");
        }
    }

    #[test]
    fn no_transcripts_and_no_requests_are_told_apart() {
        let (s, none) = only(Findings {
            no_transcripts: true,
            no_requests: true,
            ..Findings::default()
        });
        // Both facts hold (no transcript means no request) but it is said once.
        assert_eq!(s.len(), 1, "{none}");
        assert!(none.contains("not one transcript"), "{none}");
        assert!(none.contains("`swarmy demo`"), "{none}");
        let (s, empty) = only(Findings {
            no_requests: true,
            ..Findings::default()
        });
        assert_eq!(s.len(), 1, "{empty}");
        assert!(empty.contains("holds no request"), "{empty}");
        assert!(empty.contains("`swarmy demo`"), "{empty}");
        assert!(empty.contains("index: swarmy's own database"), "{empty}");
    }

    #[test]
    fn unreadable_files_and_damaged_records_say_what_is_not_counted() {
        let (_, files) = only(Findings {
            unreadable_files: 2,
            ..Findings::default()
        });
        assert!(files.contains("cannot read 2 transcript files"), "{files}");
        assert!(files.contains("nothing in them is counted"), "{files}");
        let (_, one) = only(Findings {
            unreadable_files: 1,
            ..Findings::default()
        });
        assert!(one.contains("1 transcript file (named"), "{one}");
        let (_, damaged) = only(Findings {
            damaged_records: 7,
            ..Findings::default()
        });
        assert!(
            damaged.contains("7 transcript records are damaged"),
            "{damaged}"
        );
        assert!(
            damaged.contains("what they held is not counted"),
            "{damaged}"
        );
        let (_, one) = only(Findings {
            damaged_records: 1,
            ..Findings::default()
        });
        assert!(one.contains("1 transcript record is damaged"), "{one}");
        assert!(one.contains("what it held is not counted"), "{one}");
    }

    #[test]
    fn a_rebuild_that_left_rows_out_and_unknown_types_are_named() {
        let (_, text) = only(Findings {
            rebuild_refused: Some("12".into()),
            unknown_types: vec!["mystery (2)".into(), "odd (1)".into()],
            ..Findings::default()
        });
        assert!(text.contains("left out 12 kept rows"), "{text}");
        assert!(text.contains("mystery (2), odd (1)"), "{text}");
        assert!(text.contains("newest swarmy release"), "{text}");
    }

    #[test]
    fn unpriced_requests_point_at_prices_and_the_price_entry() {
        let (s, text) = only(Findings {
            unpriced: 1,
            ..Findings::default()
        });
        assert_eq!(s[0].term, "unpriced");
        assert!(text.contains("1 request of the last 7 days"), "{text}");
        assert!(text.contains("`swarmy prices`"), "{text}");
        assert!(text.contains("`[[price]]`"), "{text}");
        assert!(text.contains("a model with no known list price"), "{text}");
        let (_, many) = only(Findings {
            unpriced: 40,
            ..Findings::default()
        });
        assert!(many.contains("40 requests of the last 7 days"), "{many}");
    }

    #[test]
    fn a_cost_gap_is_explained_and_checked_through_the_prices() {
        let (_, text) = only(Findings {
            cost_mismatch: true,
            ..Findings::default()
        });
        assert!(text.contains("differ by more than 5%"), "{text}");
        assert!(text.contains("`swarmy prices`"), "{text}");
        assert!(
            text.contains("api-equivalent cost: what the same tokens"),
            "{text}"
        );
    }

    #[test]
    fn missing_limit_readings_point_at_usage_and_the_status_line() {
        let (s, text) = only(Findings {
            no_limit_readings: true,
            ..Findings::default()
        });
        assert_eq!(s[0].kind, Kind::Fix);
        assert!(text.contains("no reading of your weekly limit"), "{text}");
        assert!(text.contains("`/usage`"), "{text}");
        assert!(text.contains("`swarmy setup`"), "{text}");
    }

    #[test]
    fn storage_warnings_name_the_compaction_commands() {
        let (_, over) = only(Findings {
            index_over_budget: true,
            ..Findings::default()
        });
        assert!(over.contains("size budget within 12 months"), "{over}");
        assert!(over.contains("`swarmy compact --dry-run`"), "{over}");
        assert!(over.contains("`budget_mb`"), "{over}");
        let (_, vacuum) = only(Findings {
            vacuum_waiting: Some("waits for mains power".into()),
            ..Findings::default()
        });
        assert!(
            vacuum.contains("the one-time VACUUM waits for mains power"),
            "{vacuum}"
        );
        assert!(vacuum.contains("`swarmy compact --vacuum`"), "{vacuum}");
    }

    #[test]
    fn a_missing_status_line_is_optional_and_does_not_hide_the_all_clear() {
        let (s, text) = only(Findings {
            no_status_line: true,
            ..Findings::default()
        });
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, Kind::Optional);
        assert!(text.contains("Nothing needs fixing"), "{text}");
        assert!(text.contains("Optional:"), "{text}");
        assert!(text.contains("`swarmy setup`"), "{text}");
        assert!(text.contains("backs up the file first"), "{text}");
        assert!(text.contains("`swarmy guide`"), "{text}");
        assert!(!text.contains("Fix:"), "{text}");
        // A real problem next to it takes the all-clear away and the guide with it.
        let (_, both) = only(Findings {
            no_status_line: true,
            unpriced: 1,
            ..Findings::default()
        });
        assert!(!both.contains("Nothing needs fixing"), "{both}");
        assert!(!both.contains("`swarmy guide`"), "{both}");
        assert!(
            both.contains("Fix:") && both.contains("Optional:"),
            "{both}"
        );
    }

    #[test]
    fn the_most_important_step_comes_first() {
        let s = steps(&Findings {
            no_seats: true,
            unreadable_setup: 1,
            unpriced: 1,
            no_status_line: true,
            ..Findings::default()
        });
        let terms: Vec<&str> = s.iter().map(|s| s.term).collect();
        assert_eq!(
            terms,
            ["seat", "seat", "unpriced"],
            "optional ones go with no seat to attach to"
        );
        let s = steps(&everything());
        assert_eq!(s.last().map(|s| s.kind), Some(Kind::Optional));
        assert!(s[..s.len() - 1].iter().all(|s| s.kind == Kind::Fix));
    }

    #[test]
    fn every_step_names_a_glossary_term() {
        let all = steps(&everything());
        assert!(all.len() >= 14, "{}", all.len());
        for s in all.iter().chain(&steps(&Findings {
            no_seats: true,
            ..Findings::default()
        })) {
            assert!(
                glossary::by_name(s.term).is_some(),
                "`{}` is not in the glossary",
                s.term
            );
        }
    }

    /// Every command a step recommends is a real subcommand (`swarmy --help` is the authority),
    /// and every flag of `compact` it names exists.
    #[test]
    fn every_command_it_recommends_is_a_real_subcommand() {
        use clap::CommandFactory;
        let cli = super::super::Cli::command();
        let known: Vec<&str> = cli.get_subcommands().map(|c| c.get_name()).collect();
        let mut all = steps(&everything());
        all.extend(steps(&Findings {
            no_seats: true,
            ..Findings::default()
        }));
        let mut seen = 0;
        for s in &all {
            let text = format!("{} {}", s.means, s.fix);
            for cmd in text.split('`').skip(1).step_by(2) {
                let mut words = cmd.split_whitespace();
                if words.next() != Some("swarmy") {
                    continue;
                }
                seen += 1;
                let Some(name) = words.next() else {
                    continue; // plain `swarmy`: the dashboard
                };
                if name.starts_with('<') {
                    continue;
                }
                assert!(
                    known.contains(&name),
                    "`{cmd}`: no such subcommand ({known:?})"
                );
                if name == "compact" {
                    let sub = cli.find_subcommand("compact").expect("compact");
                    for flag in words.filter(|w| w.starts_with("--")) {
                        let long = flag.trim_start_matches("--");
                        assert!(
                            sub.get_arguments().any(|a| a.get_long() == Some(long)),
                            "`{cmd}`: compact has no {flag}"
                        );
                    }
                }
                if name == "accounts" && cmd.contains("switch") {
                    let sub = cli.find_subcommand("accounts").expect("accounts");
                    assert!(
                        sub.get_subcommands().any(|c| c.get_name() == "switch"),
                        "`{cmd}`: accounts has no switch"
                    );
                }
            }
        }
        assert!(seen >= 8, "only {seen} commands found in the steps");
    }

    #[test]
    fn the_section_fits_the_width() {
        for width in [60, 80, 100] {
            let out = render(&Painter::plain(), width, &steps(&everything()));
            let too_wide: Vec<&str> = out.lines().filter(|l| l.width() > width).collect();
            assert!(too_wide.is_empty(), "width {width}: {too_wide:#?}");
            // Each step has its fix.
            assert_eq!(
                out.matches("Fix:").count() + out.matches("Optional:").count(),
                steps(&everything()).len(),
                "{out}"
            );
        }
    }

    /// The all-clear, with and without an optional step, also fits narrow terminals.
    #[test]
    fn the_all_clear_fits_the_width() {
        for width in [40, 60, 80, 100] {
            for f in [
                Findings::default(),
                Findings {
                    no_status_line: true,
                    ..Findings::default()
                },
            ] {
                let out = render(&Painter::plain(), width, &steps(&f));
                assert!(out.contains("[ OK ] Nothing needs fixing"), "{out}");
                let too_wide: Vec<&str> = out.lines().filter(|l| l.width() > width).collect();
                assert!(too_wide.is_empty(), "width {width}: {too_wide:#?}");
            }
        }
    }

    #[test]
    fn steps_are_numbered_and_aligned_from_one() {
        // Sixteen steps: the numbers are two digits wide, so "1." is padded to line up.
        let out = plain(&everything());
        assert_eq!(steps(&everything()).len(), 16);
        assert!(
            out.starts_with("   1. ") && out.contains("\n  10. "),
            "{out}"
        );
        // Fewer than ten: one digit.
        let out = plain(&Findings {
            unpriced: 1,
            cost_mismatch: true,
            ..Findings::default()
        });
        assert!(out.starts_with("  1. ") && out.contains("\n  2. "), "{out}");
        assert!(!out.contains("\n  3. "), "{out}");
    }
}

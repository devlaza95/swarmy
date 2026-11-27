//! `swarmy explain [TERM]`: the words, glyphs and numbers of the dashboard in plain language.
//! Needs no index and no config: it works on a machine swarmy has never seen.

use unicode_width::UnicodeWidthStr;

use crate::ui::glossary::{self, Group, Lookup, TERMS, Term};
use crate::ui::style::{Painter, Tone};

/// The terminal's width, kept readable: neither cramped nor a line across a 4K wall.
pub fn text_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .clamp(60, 100)
}

/// Greedy word wrap on display width. The first line may be `first` columns wide, the others
/// `rest` (a caller that indents its continuation lines passes the narrower width). Lines come
/// back without indentation; the caller joins them with its own.
pub fn wrap(text: &str, first: usize, rest: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let room = if lines.is_empty() { first } else { rest };
        if !cur.is_empty() && cur.width() + 1 + word.width() > room {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// `name` padded to `name_w`, then `text` wrapped to what is left of `width`, continuation
/// lines aligned under the first.
pub(super) fn hanging(p: &Painter, name: &str, name_w: usize, text: &str, width: usize) -> String {
    let lead = 2 + name_w;
    let room = width.saturating_sub(lead).max(20);
    let gap = " ".repeat(name_w.saturating_sub(name.width()));
    format!(
        "  {}{gap}{}\n",
        p.paint(name, Tone::Bold),
        wrap(text, room, room).join(&format!("\n{}", " ".repeat(lead)))
    )
}

/// A dim paragraph wrapped to `width`, ending in a newline.
pub(super) fn dim_paragraph(p: &Painter, text: &str, width: usize) -> String {
    let mut out = wrap(text, width, width).join("\n");
    out.push('\n');
    p.paint(&out, Tone::Dim)
}

/// Every term with its one-line meaning, by group.
pub fn render_list(p: &Painter, width: usize) -> String {
    let name_w = TERMS.iter().map(|t| t.name.width()).max().unwrap_or(10) + 2;
    let mut out = String::new();
    for g in Group::ALL {
        out.push_str(&p.paint(&g.title().to_uppercase(), Tone::Header));
        out.push('\n');
        for t in TERMS.iter().filter(|t| t.group == g) {
            out.push_str(&hanging(p, t.name, name_w, t.short, width));
        }
        out.push('\n');
    }
    out.push_str(&dim_paragraph(
        p,
        "Ask about one for the whole story: swarmy explain <term>. A glyph works too: swarmy explain ▲",
        width,
    ));
    out
}

/// One term in full: meaning, how it is decided, other names, related terms.
pub fn render_term(p: &Painter, t: &Term, width: usize) -> String {
    let mut out = format!(
        "{}  {}\n\n",
        p.paint(&t.name.to_uppercase(), Tone::Header),
        p.paint(&format!("({})", t.group.title()), Tone::Dim)
    );
    out.push_str(&p.paint(t.short, Tone::Bold));
    out.push_str("\n\n");
    out.push_str(&wrap(t.long, width, width).join("\n"));
    out.push('\n');
    if !t.aliases.is_empty() {
        out.push('\n');
        out.push_str(&p.paint(
            &format!("Also called: {}\n", t.aliases.join(", ")),
            Tone::Dim,
        ));
    }
    let related: Vec<&Term> = t.see.iter().filter_map(|n| glossary::by_name(n)).collect();
    if !related.is_empty() {
        out.push('\n');
        out.push_str(&p.paint("See also:\n", Tone::Dim));
        let name_w = related.iter().map(|r| r.name.width()).fold(0, usize::max) + 2;
        for r in related {
            out.push_str(&hanging(p, r.name, name_w, r.short, width));
        }
    }
    out
}

fn render_candidates(p: &Painter, query: &str, found: &[&Term]) -> String {
    let mut out = format!("Several terms match “{query}”. Pick one:\n\n");
    for t in found {
        out.push_str(&format!(
            "  {}  {}\n",
            p.paint(t.name, Tone::Bold),
            p.paint(t.short, Tone::Dim)
        ));
    }
    out
}

pub fn run(p: &Painter, words: &[String]) -> anyhow::Result<()> {
    let width = text_width();
    let query = words.join(" ");
    if query.trim().is_empty() {
        print!("{}", render_list(p, width));
        return Ok(());
    }
    match glossary::lookup(&query) {
        Lookup::One(t) => {
            print!("{}", render_term(p, t, width));
            Ok(())
        }
        Lookup::Several(found) => {
            print!("{}", render_candidates(p, query.trim(), &found));
            Ok(())
        }
        Lookup::None => anyhow::bail!(
            "no term matches “{}”. `swarmy explain` lists them all.",
            query.trim()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_keeps_each_line_inside_its_own_width() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let lines = wrap(text, 20, 12);
        assert!(lines.len() > 1);
        assert!(lines[0].width() <= 20, "{lines:#?}");
        assert!(lines[1..].iter().all(|l| l.width() <= 12), "{lines:#?}");
        // Nothing is lost.
        let joined: Vec<&str> = lines.iter().flat_map(|l| l.split_whitespace()).collect();
        assert_eq!(joined.join(" "), text);
    }

    #[test]
    fn the_list_shows_every_term_under_its_group() {
        let out = render_list(&Painter::plain(), 80);
        for g in Group::ALL {
            assert!(out.contains(&g.title().to_uppercase()), "{}", g.title());
        }
        for t in TERMS {
            assert!(out.contains(t.name), "{}", t.name);
        }
        assert!(
            out.lines().all(|l| l.width() <= 80),
            "a line is too wide:\n{out}"
        );
    }

    #[test]
    fn a_term_page_has_the_story_and_the_links() {
        let t = glossary::by_name("estimate").expect("a known term");
        let out = render_term(&Painter::plain(), t, 80);
        assert!(out.starts_with("ESTIMATE"), "{out}");
        assert!(out.contains("status line"), "{out}");
        // Related terms come with their one-line meaning.
        assert!(out.contains("See also:"), "{out}");
        assert!(
            out.contains("weekly limit  your plan's usage allowance"),
            "{out}"
        );
        assert!(out.lines().all(|l| l.width() <= 80), "{out}");
    }

    #[test]
    fn an_unknown_word_is_an_error_not_silence() {
        let err = run(&Painter::plain(), &["qqqq".into()]).unwrap_err();
        assert!(format!("{err}").contains("qqqq"));
    }
}

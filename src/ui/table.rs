//! A small table renderer aware of display width (CJK, emoji, box glyphs).

use unicode_width::UnicodeWidthStr;

use super::style::{Painter, Tone};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
}

impl Cell {
    pub fn new(text: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            tone: Tone::Plain,
        }
    }

    pub fn tone(text: impl Into<String>, tone: Tone) -> Cell {
        Cell {
            text: text.into(),
            tone,
        }
    }
}

impl From<String> for Cell {
    fn from(s: String) -> Self {
        Cell::new(s)
    }
}

impl From<&str> for Cell {
    fn from(s: &str) -> Self {
        Cell::new(s)
    }
}

pub struct Table {
    cols: Vec<(String, Align)>,
    rows: Vec<Vec<Cell>>,
    /// Rows rendered after a rule, e.g. totals.
    footer: Vec<Vec<Cell>>,
    /// The column that is truncated when the table is wider than this.
    pub max_width: Option<usize>,
    pub flex_col: usize,
}

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
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

pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

impl Table {
    pub fn new(cols: &[(&str, Align)]) -> Table {
        Table {
            cols: cols.iter().map(|(n, a)| (n.to_string(), *a)).collect(),
            rows: Vec::new(),
            footer: Vec::new(),
            max_width: crossterm::terminal::size().ok().map(|(w, _)| w as usize),
            flex_col: 0,
        }
    }

    pub fn row(&mut self, cells: Vec<Cell>) {
        self.rows.push(cells);
    }

    pub fn footer(&mut self, cells: Vec<Cell>) {
        self.footer.push(cells);
    }

    pub fn render(&self, p: &Painter) -> String {
        let n = self.cols.len();
        let mut w: Vec<usize> = self.cols.iter().map(|(h, _)| width(h)).collect();
        for r in self.rows.iter().chain(&self.footer) {
            for (i, c) in r.iter().enumerate().take(n) {
                w[i] = w[i].max(width(&c.text));
            }
        }
        if let Some(max) = self.max_width {
            let total: usize = w.iter().sum::<usize>() + 2 * n.saturating_sub(1);
            if total > max && self.flex_col < n {
                let others = total - w[self.flex_col];
                w[self.flex_col] = max.saturating_sub(others).max(12);
            }
        }
        let fmt_row = |cells: &[Cell], header: bool| -> String {
            let mut line = String::new();
            for (i, wi) in w.iter().copied().enumerate().take(n) {
                let (text, tone) = match cells.get(i) {
                    Some(c) => (truncate(&c.text, wi), c.tone),
                    None => (String::new(), Tone::Plain),
                };
                let pad = " ".repeat(wi.saturating_sub(width(&text)));
                let tone = if header { Tone::Header } else { tone };
                let painted = p.paint(&text, tone);
                if i > 0 {
                    line.push_str("  ");
                }
                match self.cols[i].1 {
                    Align::Left if i + 1 == n => line.push_str(&painted),
                    Align::Left => {
                        line.push_str(&painted);
                        line.push_str(&pad);
                    }
                    Align::Right => {
                        line.push_str(&pad);
                        line.push_str(&painted);
                    }
                }
            }
            line.trim_end().to_string()
        };
        let header: Vec<Cell> = self
            .cols
            .iter()
            .map(|(h, _)| Cell::new(h.clone()))
            .collect();
        let mut out = vec![fmt_row(&header, true)];
        for r in &self.rows {
            out.push(fmt_row(r, false));
        }
        if !self.footer.is_empty() {
            let total: usize = w.iter().sum::<usize>() + 2 * n.saturating_sub(1);
            out.push(p.paint(&"─".repeat(total), Tone::Dim));
            for r in &self.footer {
                out.push(fmt_row(r, false));
            }
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_and_truncates() {
        let mut t = Table::new(&[("NAME", Align::Left), ("COST", Align::Right)]);
        t.max_width = Some(20);
        t.row(vec!["a-very-long-project-name".into(), "$1.00".into()]);
        t.row(vec!["b".into(), "$10.00".into()]);
        let out = t.render(&Painter::plain());
        let lines: Vec<&str> = out.lines().collect();
        // The flexible first column shrinks to 20 - 2 - 6 = 12 cells.
        assert_eq!(lines[0], "NAME            COST");
        assert_eq!(lines[1], "a-very-long…   $1.00");
        assert_eq!(lines[2], "b             $10.00");
        assert!(lines.iter().all(|l| width(l) <= 20), "{out}");
    }
}

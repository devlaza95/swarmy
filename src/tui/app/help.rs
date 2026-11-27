//! State of the help overlay: which page is open and where the glossary page is.
//!
//! The pages are cycled with Tab (`keys.rs`); what each draws is in `view/help.rs`.

use crate::ui::glossary::TERMS;

/// A page of the help overlay. Keys and legend are one page side by side on a wide screen
/// (`App::help_wide`); the legend page is then never shown on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelpPage {
    #[default]
    Keys,
    Legend,
    Glossary,
}

impl HelpPage {
    /// The pages Tab goes through, in order.
    pub fn cycle(wide: bool) -> &'static [HelpPage] {
        if wide {
            &[HelpPage::Keys, HelpPage::Glossary]
        } else {
            &[HelpPage::Keys, HelpPage::Legend, HelpPage::Glossary]
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            HelpPage::Keys => "KEYS",
            HelpPage::Legend => "LEGEND",
            HelpPage::Glossary => "GLOSSARY",
        }
    }

    /// The page after (or before) this one; on a wide screen the legend is part of the keys.
    pub fn step(self, wide: bool, back: bool) -> HelpPage {
        let order = HelpPage::cycle(wide);
        let here = if wide && self == HelpPage::Legend {
            HelpPage::Keys
        } else {
            self
        };
        let at = order.iter().position(|p| *p == here).unwrap_or(0);
        let n = order.len();
        order[(at + if back { n - 1 } else { 1 }) % n]
    }
}

/// Where the glossary page is: the selected term (an index into [`TERMS`]) and the first row
/// on screen. Drawing keeps the selected term in view and writes `scroll` back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlossaryAt {
    pub term: usize,
    pub scroll: usize,
}

impl GlossaryAt {
    /// The selection `step` terms on, kept inside the list.
    pub fn moved(self, step: isize) -> GlossaryAt {
        let last = TERMS.len().saturating_sub(1) as isize;
        GlossaryAt {
            term: (self.term as isize).saturating_add(step).clamp(0, last) as usize,
            ..self
        }
    }
}

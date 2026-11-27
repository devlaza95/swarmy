//! Keys, the mouse and `:` commands.

use super::*;

const PERIODS: [&str; 4] = ["today", "7d", "30d", "all"];
const GROUPS: [Dim; 8] = [
    Dim::Project,
    Dim::Worktree,
    Dim::Model,
    Dim::AgentType,
    Dim::Account,
    Dim::Seat,
    Dim::Harness,
    Dim::Session,
];

/// What a key does while the help is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpKey {
    /// The next / previous page.
    Next,
    Prev,
    Close,
    /// `?` again: the legend on a small screen, else close.
    Question,
    /// The glossary's selection, by this many terms.
    Move(isize),
}

/// The keys of the help overlay, as a table: a key that is not here does nothing. A page is
/// 8 terms for PgUp / PgDn; Home and End go past either end and stop there.
const HELP_KEYS: &[(KeyCode, HelpKey)] = &[
    (KeyCode::Tab, HelpKey::Next),
    (KeyCode::Right, HelpKey::Next),
    (KeyCode::BackTab, HelpKey::Prev),
    (KeyCode::Left, HelpKey::Prev),
    (KeyCode::Esc, HelpKey::Close),
    (KeyCode::Char('q'), HelpKey::Close),
    (KeyCode::Char('?'), HelpKey::Question),
    (KeyCode::Down, HelpKey::Move(1)),
    (KeyCode::Char('j'), HelpKey::Move(1)),
    (KeyCode::Up, HelpKey::Move(-1)),
    (KeyCode::Char('k'), HelpKey::Move(-1)),
    (KeyCode::PageDown, HelpKey::Move(8)),
    (KeyCode::PageUp, HelpKey::Move(-8)),
    (KeyCode::Home, HelpKey::Move(-100_000)),
    (KeyCode::End, HelpKey::Move(100_000)),
];

impl App {
    // ───────────── keys ─────────────

    /// Returns true to quit.
    pub fn on_key(&mut self, k: KeyEvent) -> bool {
        if let Some(b) = &mut self.boot {
            b.skip = true;
            return k.code == KeyCode::Char('q');
        }
        if let Some(cmd) = &mut self.command {
            match k.code {
                KeyCode::Esc => self.command = None,
                KeyCode::Enter => {
                    let c = std::mem::take(cmd);
                    self.command = None;
                    return self.run_command(&c);
                }
                KeyCode::Backspace => {
                    cmd.pop();
                }
                KeyCode::Char(c) => cmd.push(c),
                _ => {}
            }
            return false;
        }
        if self.filter_edit {
            let f = self.filter.get_or_insert_with(String::new);
            match k.code {
                KeyCode::Esc => {
                    self.filter = None;
                    self.filter_edit = false;
                }
                KeyCode::Enter => self.filter_edit = false,
                KeyCode::Backspace => {
                    f.pop();
                }
                KeyCode::Char(c) => f.push(c),
                _ => {}
            }
            return false;
        }
        if self.help {
            self.help_key(k);
            return false;
        }
        // Dialogs of the vault take every key.
        if (self.vault.modal.is_some() || self.vault.picker.is_some()) && self.vault_key(k) {
            return false;
        }
        if self.global_key(k) {
            return true;
        }
        false
    }

    /// A key while the help is open: Tab and the arrows turn its pages, the glossary page
    /// also moves its selection.
    fn help_key(&mut self, k: KeyEvent) {
        let Some((_, key)) = HELP_KEYS.iter().find(|(code, _)| *code == k.code) else {
            return;
        };
        match *key {
            HelpKey::Next => self.help_page = self.help_page.step(self.help_wide, false),
            HelpKey::Prev => self.help_page = self.help_page.step(self.help_wide, true),
            HelpKey::Close => self.close_help(),
            HelpKey::Question if self.help_page == HelpPage::Keys && self.shape == Shape::Tabs => {
                self.help_page = HelpPage::Legend
            }
            HelpKey::Question => self.close_help(),
            HelpKey::Move(step) => self.glossary_move(step),
        }
    }

    fn close_help(&mut self) {
        self.help = false;
        self.help_page = HelpPage::Keys;
    }

    /// Moves the glossary's selection, on its page only.
    fn glossary_move(&mut self, step: isize) {
        if self.help_page == HelpPage::Glossary {
            self.glossary = self.glossary.moved(step);
        }
    }

    /// Returns true to quit.
    fn global_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(':') => self.command = Some(String::new()),
            KeyCode::Char('/') => {
                self.filter_edit = true;
                self.filter.get_or_insert_with(String::new);
            }
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            KeyCode::Char(c @ '0'..='9')
                if self.shape == Shape::Tabs && ('1'..='5').contains(&c) && self.zoom.is_none() =>
            {
                let (t, _, p) = TABS[c as usize - '1' as usize];
                self.tab = t;
                self.focus = p;
            }
            KeyCode::Char(c @ '0'..='9') => {
                let p = Panel::from_number(c as u8 - b'0');
                if self.focus == p && self.visible_panels().len() > 1 {
                    self.zoom = Some(p);
                } else {
                    self.focus_panel(p);
                }
            }
            KeyCode::F(n) if n <= 10 => {
                let p = Panel::from_number(n % 10);
                self.focus_panel(p);
            }
            KeyCode::Char('z') => {
                self.zoom = if self.zoom.is_some() {
                    None
                } else {
                    Some(self.focus)
                }
            }
            KeyCode::Esc => {
                if self.zoom.is_some() {
                    self.zoom = None;
                } else if self.filter.is_some() {
                    self.filter = None;
                } else if self.shape == Shape::Tabs {
                    self.tab = Tab::Ops;
                    self.focus = Panel::Swarm;
                }
            }
            KeyCode::Char('.') => self.toggle_hidden(),
            KeyCode::Char('n') => self.jump_attention(1),
            KeyCode::Char('N') => self.jump_attention(-1),
            KeyCode::Char('J') => self.jump(),
            KeyCode::Char('v') => {
                self.lanes = !self.lanes;
                self.toast(
                    if self.lanes {
                        "SWARM: activity of the last 30 min (thinking · tool · waiting for agents · waiting for you)"
                    } else {
                        "SWARM: spend of the last 15 min"
                    },
                    Tone::Info,
                );
            }
            KeyCode::Char('m') => {
                self.mask = !self.mask;
                self.toast(
                    if self.mask {
                        "mask on: names, emails and paths hidden"
                    } else {
                        "mask off"
                    },
                    Tone::Info,
                );
            }
            KeyCode::Char('p') => {
                self.paused = !self.paused;
                self.toast(
                    if self.paused {
                        "animations paused"
                    } else {
                        "animations on"
                    },
                    Tone::Info,
                );
            }
            KeyCode::Char('T') => {
                self.theme = self.theme.next();
                self.toast(format!("theme: {}", self.theme.name.as_str()), Tone::Info);
            }
            KeyCode::Char('L') => {
                self.layout = match self.layout {
                    LayoutMode::Auto => LayoutMode::Wall,
                    LayoutMode::Wall => LayoutMode::Column,
                    LayoutMode::Column => LayoutMode::Tabs,
                    LayoutMode::Tabs => LayoutMode::Auto,
                };
                self.zoom = None;
                self.toast(
                    format!("layout: {:?}", self.layout).to_lowercase(),
                    Tone::Info,
                );
            }
            KeyCode::Char('t') => {
                let cur = PERIODS
                    .iter()
                    .position(|p| *p == period_key(&self.period))
                    .unwrap_or(1);
                self.period = PERIODS[(cur + 1) % PERIODS.len()]
                    .parse()
                    .unwrap_or(Period::Days(7));
                self.request_analytics();
                self.toast(format!("period: {}", self.period.label()), Tone::Info);
            }
            KeyCode::Char('g') => {
                let cur = GROUPS.iter().position(|g| *g == self.group).unwrap_or(0);
                self.group = GROUPS[(cur + 1) % GROUPS.len()];
                self.request_analytics();
                self.toast(
                    format!("spend grouped by {}", self.group.title().to_lowercase()),
                    Tone::Info,
                );
            }
            _ => self.panel_key(k),
        }
        false
    }

    fn list_len(&self, p: Panel) -> usize {
        match p {
            // Alerts, then the event log under them.
            Panel::Attention => self.attention().len() + self.events.len(),
            // Pages of the efficiency panel.
            Panel::Efficiency => 3,
            // Days of the daily bars (0 = today).
            Panel::History => 14,
            Panel::Projects => self.analytics.projects.len(),
            Panel::Agent => self.timeline.1.len(),
            _ => 0,
        }
    }

    fn panel_key(&mut self, k: KeyEvent) {
        let step: isize = match k.code {
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::PageDown => 10,
            KeyCode::PageUp => -10,
            KeyCode::Home => -100_000,
            KeyCode::End => 100_000,
            _ => 0,
        };
        match self.focus {
            Panel::Swarm => {
                if step != 0 {
                    self.move_agent(step);
                } else if k.code == KeyCode::Enter {
                    self.focus_panel(Panel::Agent);
                }
            }
            Panel::Limits => {
                self.vault_key(k);
            }
            Panel::Attention if k.code == KeyCode::Enter => {
                let items = self.attention();
                if let Some(a) = items.get(self.cur(Panel::Attention)).cloned() {
                    self.act_on_attention(&a);
                }
            }
            Panel::Projects if k.code == KeyCode::Enter => {
                if let Some(p) = self.analytics.projects.get(self.cur(Panel::Projects)) {
                    self.filter = Some(format!("p:{}", p.name.to_lowercase()));
                    self.toast(format!("agents of {} (Esc clears)", p.name), Tone::Info);
                    self.focus_panel(Panel::Swarm);
                }
            }
            Panel::History
                if matches!(
                    k.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l')
                ) =>
            {
                let step = if matches!(k.code, KeyCode::Left | KeyCode::Char('h')) {
                    1
                } else {
                    -1
                };
                let v = (self.cur(Panel::History) as isize + step).clamp(0, 13) as usize;
                self.set_cur(Panel::History, v);
            }
            p @ (Panel::Attention
            | Panel::Projects
            | Panel::Efficiency
            | Panel::Agent
            | Panel::History)
                if step != 0 =>
            {
                let n = self.list_len(p).max(1);
                let v = (self.cur(p) as isize + step).clamp(0, n as isize - 1) as usize;
                self.set_cur(p, v);
            }
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        // The wheel over the glossary page moves its selection, not the panels behind it.
        if self.help && self.help_page == HelpPage::Glossary {
            let step = if m.kind == MouseEventKind::ScrollDown {
                1
            } else if m.kind == MouseEventKind::ScrollUp {
                -1
            } else {
                0
            };
            if step != 0 {
                self.glossary_move(step);
                return;
            }
        }
        let hit = self
            .rects
            .iter()
            .find(|(_, r)| {
                m.column >= r.x
                    && m.column < r.x + r.width
                    && m.row >= r.y
                    && m.row < r.y + r.height
            })
            .map(|(p, r)| (*p, *r));
        let Some((p, r)) = hit else { return };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = p;
                // A click on an agent row selects it (rows start two lines below the border).
                if p == Panel::Swarm {
                    let row = m.row.saturating_sub(r.y + 2) as usize;
                    let keys: Vec<String> = self.visible_agents().iter().map(|a| a.key()).collect();
                    let first = self.swarm_offset(r.height.saturating_sub(3) as usize);
                    if let Some(k) = keys.get(first + row) {
                        self.selected_key = Some(k.clone());
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let prev = self.focus;
                self.focus = p;
                let code = if m.kind == MouseEventKind::ScrollDown {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                };
                self.panel_key(KeyEvent::new(code, KeyModifiers::NONE));
                self.focus = prev;
            }
            _ => {}
        }
    }

    /// First visible row of the agent list for a list `rows` tall (keeps the selection on screen).
    pub fn swarm_offset(&self, rows: usize) -> usize {
        let sel = self.selected_index();
        if rows == 0 || sel < rows {
            0
        } else {
            sel + 1 - rows
        }
    }

    /// Take the user to the terminal or multiplexer pane of the selected agent's Claude Code.
    fn jump(&mut self) {
        let Some(a) = self.selected_agent() else {
            return;
        };
        let main = self
            .snapshot
            .agents
            .iter()
            .find(|x| x.is_main() && x.seat == a.seat && x.session_id == a.session_id);
        let Some(pane) = main.and_then(|m| m.pane.clone()).or_else(|| a.pane.clone()) else {
            self.toast(
                "session terminal not supported for jump (tmux, zellij, kitty, wezterm, iTerm2, Warp)",
                Tone::Warn,
            );
            return;
        };
        match crate::terminal::jump(&pane) {
            crate::terminal::Jump::Done => self.toast(format!("jumped to {pane}"), Tone::Good),
            crate::terminal::Jump::Unreachable(why) => self.toast(why, Tone::Warn),
            crate::terminal::Jump::Failed(why) => self.toast(why, Tone::Bad),
        }
    }

    fn run_command(&mut self, line: &str) -> bool {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["q"] | ["quit"] => return true,
            ["period", p] => match p.parse::<Period>() {
                Ok(p) => {
                    self.period = p;
                    self.request_analytics();
                }
                Err(e) => self.toast(e, Tone::Bad),
            },
            ["theme", t] => match t.parse() {
                Ok(name) => self.theme = Theme::named(Some(name)),
                Err(e) => self.toast(e, Tone::Bad),
            },
            ["layout", l] => {
                self.layout = match *l {
                    "wall" => LayoutMode::Wall,
                    "column" => LayoutMode::Column,
                    "tabs" => LayoutMode::Tabs,
                    _ => LayoutMode::Auto,
                }
            }
            ["filter", rest @ ..] => self.filter = Some(rest.join(" ")),
            ["capture", seat] | ["capture", seat, _] => {
                let label = parts.get(2).copied();
                let res = self.with_vault(|v, seats| {
                    let s = seats.iter().find(|s| s.label == *seat).ok_or_else(|| crate::errors::LookupError::NoSeat {
                        name: seat.to_string(),
                        known: Vec::new(),
                    })?;
                    v.capture(s, label)?.ok_or_else(|| anyhow::anyhow!("seat '{seat}' has no login"))
                });
                match res {
                    Ok(_) => self.toast(format!("saved the login of seat '{seat}'"), Tone::Good),
                    Err(e) => self.toast(format!("{e:#}"), Tone::Bad),
                }
                self.vault_reload();
                self.request_analytics();
            }
            ["switch", seat, login] => self.request_switch(seat.to_string(), login.to_string()),
            [] => {}
            _ => self.toast("commands: switch <seat> <login> · capture <seat> [label] · period 7d · filter p:shop · layout wall|column|tabs · theme <name> · q", Tone::Info),
        }
        false
    }

    fn with_vault<T>(
        &mut self,
        f: impl FnOnce(&mut VaultTx<'_>, &[Seat]) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let seats: Vec<Seat> = self
            .seats
            .iter()
            .filter(|s| s.harness.supports_vault())
            .cloned()
            .collect();
        Vault::open(&self.vault_dir)?.edit(|tx| {
            tx.sync(&seats)?;
            f(tx, &seats)
        })
    }
}

fn period_key(p: &Period) -> &'static str {
    match p {
        Period::Today => "today",
        Period::Days(7) => "7d",
        Period::Days(30) => "30d",
        Period::All => "all",
        _ => "7d",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{MouseEvent, MouseEventKind};

    fn app() -> App {
        let t = tempfile::tempdir().unwrap();
        let mut app = App::new(Theme::named(None), false, true, Vec::new(), t.path().into());
        app.boot = None;
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn wheel(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn tab_turns_the_help_pages_and_wraps_round() {
        let mut a = app();
        press(&mut a, KeyCode::Char('?'));
        assert!(a.help);
        assert_eq!(a.help_page, HelpPage::Keys);
        let mut seen = Vec::new();
        for _ in 0..3 {
            press(&mut a, KeyCode::Tab);
            seen.push(a.help_page);
        }
        assert_eq!(
            seen,
            [HelpPage::Legend, HelpPage::Glossary, HelpPage::Keys],
            "keys, legend and glossary, then round again"
        );
        // Shift-Tab goes the other way; so do the arrows (Right on, Left back).
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.help_page, HelpPage::Glossary);
        press(&mut a, KeyCode::Left);
        assert_eq!(a.help_page, HelpPage::Legend);
        press(&mut a, KeyCode::Right);
        assert_eq!(a.help_page, HelpPage::Glossary);
    }

    /// On a wide screen the keys and the legend are one page: Tab does not stop on a legend
    /// that looks the same as the keys.
    #[test]
    fn on_a_wide_screen_the_legend_is_not_a_page_of_its_own() {
        let mut a = app();
        a.help = true;
        a.help_wide = true;
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.help_page, HelpPage::Glossary);
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.help_page, HelpPage::Keys);
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.help_page, HelpPage::Glossary);
        // The screen was narrow a moment ago and the legend was open: it counts as the keys.
        a.help_page = HelpPage::Legend;
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.help_page, HelpPage::Glossary);
    }

    #[test]
    fn question_mark_still_goes_to_the_legend_on_tabs_then_closes() {
        let mut a = app();
        a.shape = Shape::Tabs;
        a.help = true;
        press(&mut a, KeyCode::Char('?'));
        assert!(a.help);
        assert_eq!(a.help_page, HelpPage::Legend);
        press(&mut a, KeyCode::Char('?'));
        assert!(!a.help);
        assert_eq!(a.help_page, HelpPage::Keys, "help opens on the keys again");
        // Not on Tabs: `?` closes at once, from any page.
        a.shape = Shape::Wall;
        a.help = true;
        a.help_page = HelpPage::Glossary;
        press(&mut a, KeyCode::Char('?'));
        assert!(!a.help);
        assert_eq!(a.help_page, HelpPage::Keys);
    }

    #[test]
    fn the_glossary_selection_moves_inside_the_list() {
        use crate::ui::glossary::TERMS;
        let mut a = app();
        a.help = true;
        a.help_page = HelpPage::Glossary;
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(a.glossary.term, 2);
        press(&mut a, KeyCode::Up);
        press(&mut a, KeyCode::Char('k'));
        press(&mut a, KeyCode::Char('k'));
        assert_eq!(a.glossary.term, 0, "stops at the first term");
        press(&mut a, KeyCode::PageDown);
        assert_eq!(a.glossary.term, 8);
        press(&mut a, KeyCode::PageUp);
        assert_eq!(a.glossary.term, 0);
        press(&mut a, KeyCode::End);
        assert_eq!(a.glossary.term, TERMS.len() - 1, "stops at the last term");
        press(&mut a, KeyCode::Down);
        assert_eq!(a.glossary.term, TERMS.len() - 1);
        press(&mut a, KeyCode::Home);
        assert_eq!(a.glossary.term, 0);
    }

    /// The glossary's keys do nothing on the other pages, and no key reaches the panels
    /// behind the help.
    #[test]
    fn the_selection_keys_belong_to_the_glossary_page() {
        let mut a = app();
        a.help = true;
        a.focus = Panel::Swarm;
        for page in [HelpPage::Keys, HelpPage::Legend] {
            a.help_page = page;
            press(&mut a, KeyCode::Down);
            press(&mut a, KeyCode::Char('j'));
            press(&mut a, KeyCode::End);
            assert_eq!(a.glossary.term, 0, "{page:?}");
            assert_eq!(a.help_page, page);
        }
        // A key the table does not know changes nothing, and `t` is not a period change.
        let period = a.period.clone();
        press(&mut a, KeyCode::Char('t'));
        press(&mut a, KeyCode::F(5));
        assert!(a.help);
        assert_eq!(a.period, period);
    }

    #[test]
    fn closing_help_keeps_the_term_for_next_time() {
        let mut a = app();
        a.help = true;
        a.help_page = HelpPage::Glossary;
        press(&mut a, KeyCode::PageDown);
        press(&mut a, KeyCode::Esc);
        assert!(!a.help);
        assert_eq!(a.help_page, HelpPage::Keys);
        assert_eq!(a.glossary.term, 8);
        // `q` closes the help too (it does not quit while the help is open).
        a.help = true;
        let quit = a.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!quit && !a.help);
    }

    #[test]
    fn the_wheel_moves_the_glossary_and_nothing_behind_it() {
        let mut a = app();
        a.help = true;
        a.help_page = HelpPage::Glossary;
        a.on_mouse(wheel(MouseEventKind::ScrollDown));
        a.on_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(a.glossary.term, 2);
        a.on_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(a.glossary.term, 1);
        assert!(a.cursor.is_empty(), "no panel behind was scrolled");
    }
}

//! Plain-language glossary: the words, glyphs and numbers swarmy puts on screen, explained once.
//!
//! One table feeds `swarmy explain` and the dashboard's help (`?`), so the two never disagree.
//! The statements here describe what the code does; when a rule changes (a threshold, a
//! default), change the line here too. The test below keeps the table well-formed.

/// A section of the glossary, in the order the sections are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// What an agent is doing right now.
    States,
    /// Money, tokens and the context window.
    Spend,
    /// Weekly and 5-hour limits.
    Limits,
    /// Where work happens and who it is billed to.
    Where,
    /// swarmy's own data and privacy switches.
    Data,
}

impl Group {
    pub const ALL: [Group; 5] = [
        Group::States,
        Group::Spend,
        Group::Limits,
        Group::Where,
        Group::Data,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::States => "What an agent is doing",
            Group::Spend => "Money and context",
            Group::Limits => "Limits",
            Group::Where => "Where work happens",
            Group::Data => "swarmy's own data",
        }
    }
}

/// One entry: a name, other names people may type, a one-line meaning and the longer story.
#[derive(Debug)]
pub struct Term {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub group: Group,
    /// One line, shown in lists and in the dashboard.
    pub short: &'static str,
    /// A few sentences: what it is, how it is decided, what to do about it.
    pub long: &'static str,
    /// Names of related terms.
    pub see: &'static [&'static str],
}

const fn term(
    name: &'static str,
    aliases: &'static [&'static str],
    group: Group,
    short: &'static str,
    long: &'static str,
    see: &'static [&'static str],
) -> Term {
    Term {
        name,
        aliases,
        group,
        short,
        long,
        see,
    }
}

/// Every term, grouped, in the order of [`Group::ALL`].
pub const TERMS: &[Term] = &[
    // ── What an agent is doing ────────────────────────────────────────────────────────────
    term(
        "working",
        &["tool", "running", "●"],
        Group::States,
        "running a tool (a command, an edit, a search)",
        "The agent asked for a tool and the tool has not finished. The dashboard shows the tool's \
         name and a one-line summary of what it was asked to do. How long it has been in this \
         state is the IN STATE column.",
        &["thinking", "stalled"],
    ),
    term(
        "thinking",
        &["⠋", "◌"],
        Group::States,
        "waiting for the model's answer",
        "The agent sent a request and the model is still answering. Long thinking times are \
         normal for big contexts or high effort; if it lasts far longer than usual, check \
         `stalled`.",
        &["effort", "context", "working"],
    ),
    term(
        "waiting for agents",
        &["waiting", "◐"],
        Group::States,
        "the main thread is idle while its subagents work",
        "A main thread that has handed work to background agents shows this until they finish. \
         The subagents are listed under it in SWARM.",
        &["working"],
    ),
    term(
        "needs you",
        &["attention", "needs", "▲"],
        Group::States,
        "blocked on you: a question, a permission, a plan to approve",
        "Claude Code is waiting for a human. The dashboard highlights it, rings the terminal \
         bell, and `n` jumps to the next one. `J` (or `swarmy jump`) switches your terminal to \
         that session when it runs in tmux, zellij, kitty, WezTerm, iTerm2 or Warp.",
        &["stalled"],
    ),
    term(
        "stalled",
        &["✖"],
        Group::States,
        "unfinished, with no activity for a while",
        "The session has not written anything for `stall_minutes` (20 by default, set in \
         config.toml) although it has not finished. Often it is waiting on a hung command or a \
         closed terminal. Look at the pane, or press Esc in it.",
        &["thinking", "idle"],
    ),
    term(
        "idle",
        &["done", "gone", "exited", "○", "✓"],
        Group::States,
        "nothing running: idle, finished, or the process is gone",
        "`idle` is a live session waiting for your next prompt. `done` is an agent that \
         finished its task. `gone` means the Claude Code process is no longer running. Finished \
         agents stay on screen for a few minutes under RECENTLY DONE.",
        &["needs you"],
    ),
    // ── Money and context ─────────────────────────────────────────────────────────────────
    term(
        "api-equivalent cost",
        &["cost", "$", "spend", "price"],
        Group::Spend,
        "what the same tokens would cost at API list prices",
        "swarmy counts the tokens each request used and multiplies by the model's list price. \
         It is not what your plan bills: on a subscription you pay a flat fee, and this number \
         tells you how much work you got and where it went. Each response is counted once, even \
         though Claude Code writes several records for it. Prices are versioned data; see \
         `swarmy prices`.",
        &["unpriced", "burn"],
    ),
    term(
        "unpriced",
        &["—"],
        Group::Spend,
        "a model with no known list price",
        "swarmy shows a dash and counts the requests instead of pretending the cost is zero. \
         Add the model with a [[price]] entry in config.toml and it is priced from then on, \
         including its past requests.",
        &["api-equivalent cost"],
    ),
    term(
        "burn",
        &["burn rate", "per hour", "/h"],
        Group::Spend,
        "recent spend rate in dollars per hour",
        "Computed from what was spent over the last hour. In LIMITS it is the pace used to \
         project when a limit runs out.",
        &["runs out", "api-equivalent cost"],
    ),
    term(
        "session total",
        &["Σ", "sigma", "session"],
        Group::Spend,
        "the whole session: the main thread and all its agents",
        "A session's cost is the main thread plus every subagent it started. The main thread's \
         own row shows both numbers: its own cost and `session Σ`.",
        &["api-equivalent cost"],
    ),
    term(
        "context",
        &["context window", "context fill"],
        Group::Spend,
        "how full the model's context window is",
        "The size of the last request's prompt divided by the model's context window. From \
         80% the gauge turns to the warning colour and the AGENT card says `filling up`. A \
         full context means Claude Code summarises the conversation (see `compaction`), which \
         costs time and loses detail; a fresh session is sometimes the better move.",
        &["compaction"],
    ),
    term(
        "compaction",
        &[],
        Group::Spend,
        "the conversation was summarised to free context",
        "In the AGENT life chart a vertical marker shows where the context fell to under half \
         of the previous request's size after being above 50,000 tokens: that is Claude Code \
         summarising the conversation. swarmy infers it from the numbers; it is not a recorded \
         event.",
        &["context"],
    ),
    term(
        "effort",
        &["lo", "md", "hi", "xh", "mx"],
        Group::Spend,
        "how hard the model was asked to think: lo md hi xh mx",
        "Low, medium, high, extra high, max. Higher effort means longer thinking and more \
         output tokens, so a higher cost per request. The MODELS panel shows the mix.",
        &["thinking"],
    ),
    term(
        "trend",
        &["▲▼", "vs previous"],
        Group::Spend,
        "change against the previous period of the same length",
        "A ▲ or ▼ next to a number compares it with the period just before (the previous 7 \
         days against the last 7 days; in HISTORY, the day before). The text beside it always \
         says what it is compared with.",
        &[],
    ),
    // ── Limits ────────────────────────────────────────────────────────────────────────────
    term(
        "weekly limit",
        &[
            "week",
            "week used",
            "5-hour",
            "5h",
            "five hour",
            "window",
            "limit",
        ],
        Group::Limits,
        "your plan's usage allowance for a 7-day or 5-hour window",
        "Claude Code limits are per account, not per machine. The gauge shows how much of the \
         current window is used; colours change at 60% (warning) and 85% (critical). The window \
         ends at the reset time shown, and a new one starts.",
        &["estimate", "runs out", "elsewhere"],
    ),
    term(
        "estimate",
        &["≈", "approx", "reading", "exact"],
        Group::Limits,
        "a number between two limit readings, assumed from spend",
        "Exact limit readings reach swarmy only when Claude Code caches them (after you show \
         /usage) or when the optional status line records them (`swarmy setup`). Between \
         readings swarmy assumes limit usage grows in proportion to API-equivalent cost and \
         marks the number with ≈. Install the status line for exact numbers.",
        &["weekly limit", "elsewhere"],
    ),
    term(
        "runs out",
        &["empty", "limit reached", "eta", "projection"],
        Group::Limits,
        "when the window reaches 100% at the current pace",
        "If the recent pace continues, the time the weekly window is used up, shown as a clock \
         time and a distance (`runs out in 7h (Mon 03:34)`). `lasts until reset` means the \
         window ends first; `pace unknown` means there is not enough data yet to convert \
         dollars into limit percent.",
        &["burn", "weekly limit", "1% ≈ $"],
    ),
    term(
        "1% ≈ $",
        &["dollars per point", "usd per point", "per point", "pt"],
        Group::Limits,
        "API dollars that use up one percent of the limit",
        "Worked out from the readings and the spend between them. It lets swarmy turn a \
         project's dollars into a share of the limit, and a full window into dollars.",
        &["estimate", "% of week"],
    ),
    term(
        "% of week",
        &["share", "shares"],
        Group::Limits,
        "a project's share of the weekly spend",
        "In LIMITS each project gets its share of the window's spend, and with `1% ≈ $` that \
         becomes `≈ N%` of your limit. The top rows are the ones that used your week.",
        &["1% ≈ $", "project"],
    ),
    term(
        "elsewhere",
        &["used elsewhere", "outside"],
        Group::Limits,
        "limit used outside the transcripts swarmy can see",
        "The account's reading is higher than what the work on this machine explains: the \
         rest came from claude.ai, the desktop app, or other computers. swarmy shows it as its \
         own line so a project is not blamed for it.",
        &["weekly limit"],
    ),
    // ── Where work happens ────────────────────────────────────────────────────────────────
    term(
        "seat",
        &["config dir", "config directory", "~/.claude"],
        Group::Where,
        "one Claude Code config directory",
        "`~/.claude` is one seat; `CLAUDE_CONFIG_DIR=~/.claude-work` makes another. Each holds \
         its own transcripts and its own sign-in. Other agents have seats too (Antigravity, \
         Codex). `swarmy doctor` lists the ones it found.",
        &["login", "account"],
    ),
    term(
        "login",
        &["oauth", "sign-in", "saved login", "⚿"],
        Group::Where,
        "an OAuth sign-in; saved ones live in the vault",
        "A login is a sign-in grant. Saving it in the vault (⚿) lets you put it into a seat \
         later without signing in again. A login fits only the client that made it. One saved \
         login can serve several seats of the same client.",
        &["vault", "seat", "account"],
    ),
    term(
        "account",
        &[],
        Group::Where,
        "the Anthropic account behind a login",
        "Limits belong to the account. Two seats signed in with the same account share one \
         weekly limit; two accounts have separate ones. swarmy names an account after its \
         saved login, else its email or organisation.",
        &["seat", "login", "weekly limit"],
    ),
    term(
        "vault",
        &["switch", "capture"],
        Group::Where,
        "a store of saved logins you can switch between",
        "Switching saves the seat's current login first, backs up the files it will touch, \
         and writes atomically. It is the one thing swarmy writes to Claude Code's \
         directories, and only when you ask. Running sessions keep the old login until they \
         are restarted. Switching is refused while a Codex session runs in that seat.",
        &["login", "seat"],
    ),
    term(
        "project",
        &[],
        Group::Where,
        "the git repository an agent works in",
        "A working directory resolves to its git root, so subdirectories and linked worktrees \
         of one repository count as one project. A directory outside any repository takes the \
         project of the session it ran in. Rename or merge projects with [[project]] entries \
         in config.toml.",
        &["checkout"],
    ),
    term(
        "checkout",
        &["worktree", "wt", "⇄", "⎇", "branch"],
        Group::Where,
        "where an agent works: the main checkout or a linked git worktree",
        "Shown as `project/worktree`, with ⎇ and the branch when it is not main. `⇄2` means \
         two live sessions work in the same checkout, so their edits can collide; giving each \
         its own worktree avoids it. `g` in the dashboard groups spend by checkout.",
        &["project"],
    ),
    term(
        "harness",
        &["claude", "agy", "cdx", "codex", "antigravity", "cc"],
        Group::Where,
        "which coding agent: Claude Code, Antigravity or Codex",
        "Short tags in the dashboard: cc, agy, cdx. Antigravity and Codex have no local limit \
         readings, so their seats show spend instead of limit gauges.",
        &["seat"],
    ),
    // ── swarmy's own data ─────────────────────────────────────────────────────────────────
    term(
        "index",
        &["database", "storage", "compact", "raw window", "archive"],
        Group::Data,
        "swarmy's own database of what it read",
        "A SQLite file in swarmy's data directory. Per-request detail is kept for 14 days by \
         default (`keep_raw_days`), then rolled up into 15-minute summaries, so reports over \
         long periods stay fast and the history outlives Claude Code's own transcript cleanup. \
         `swarmy compact` does the rolling-up by hand; `swarmy doctor` shows its size.",
        &["mask"],
    ),
    term(
        "mask",
        &["--mask", "privacy", "screen share"],
        Group::Data,
        "hide emails, names and paths for screenshots",
        "`--mask` (or `m` in the dashboard) blanks emails, login labels made from emails, the \
         OS user name and the directory part of paths at draw time. Nothing on disk changes.",
        &[],
    ),
];

/// The result of looking a word up.
#[derive(Debug)]
pub enum Lookup {
    One(&'static Term),
    /// Several terms contain the word: show their names so the user can pick.
    Several(Vec<&'static Term>),
    None,
}

fn norm(s: &str) -> String {
    s.trim().trim_matches(['`', '"', '\'']).to_lowercase()
}

/// Finds a term by exact name or alias, else by a word inside a name, alias or one-line
/// meaning. Case does not matter.
pub fn lookup(query: &str) -> Lookup {
    let q = norm(query);
    if q.is_empty() {
        return Lookup::None;
    }
    if let Some(t) = TERMS
        .iter()
        .find(|t| norm(t.name) == q || t.aliases.iter().any(|a| norm(a) == q))
    {
        return Lookup::One(t);
    }
    let by_name: Vec<&Term> = TERMS
        .iter()
        .filter(|t| norm(t.name).contains(&q) || t.aliases.iter().any(|a| norm(a).contains(&q)))
        .collect();
    let found = if by_name.is_empty() {
        TERMS
            .iter()
            .filter(|t| norm(t.short).contains(&q))
            .collect()
    } else {
        by_name
    };
    match found.as_slice() {
        [] => Lookup::None,
        [one] => Lookup::One(one),
        [_, _, ..] => Lookup::Several(found),
    }
}

/// The term with this exact name (for `see` links).
pub fn by_name(name: &str) -> Option<&'static Term> {
    TERMS.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_term_is_well_formed() {
        let mut names = BTreeSet::new();
        for t in TERMS {
            assert!(names.insert(t.name), "duplicate term {}", t.name);
            assert!(
                !t.short.is_empty() && t.short.len() <= 80,
                "{}: short",
                t.name
            );
            assert!(t.long.len() >= 60, "{}: too thin", t.name);
            for s in t.see {
                assert!(by_name(s).is_some(), "{} sees unknown term {s}", t.name);
                assert_ne!(*s, t.name, "{} sees itself", t.name);
            }
        }
    }

    #[test]
    fn terms_are_ordered_by_group() {
        let order = |g: Group| Group::ALL.iter().position(|x| *x == g);
        let positions: Vec<_> = TERMS.iter().map(|t| order(t.group)).collect();
        assert!(positions.windows(2).all(|w| w[0] <= w[1]), "{positions:?}");
        for g in Group::ALL {
            assert!(TERMS.iter().any(|t| t.group == g), "empty group {g:?}");
        }
    }

    #[test]
    fn an_alias_or_a_glyph_finds_its_term() {
        for (q, want) in [
            ("stalled", "stalled"),
            ("▲", "needs you"),
            ("≈", "estimate"),
            ("WORKTREE", "checkout"),
            ("`seat`", "seat"),
            ("Σ", "session total"),
        ] {
            match lookup(q) {
                Lookup::One(t) => assert_eq!(t.name, want, "{q}"),
                other => panic!("{q}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_fragment_lists_the_candidates() {
        match lookup("limit") {
            // "limit" is an alias of `weekly limit`: exactly one hit.
            Lookup::One(t) => assert_eq!(t.name, "weekly limit"),
            other => panic!("{other:?}"),
        }
        match lookup("lo") {
            Lookup::One(t) => assert_eq!(t.name, "effort"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(lookup("zzz-no-such-word"), Lookup::None));
        assert!(matches!(lookup("   "), Lookup::None));
    }
}

# Changelog

All notable changes to swarmy. Versions follow [Semantic Versioning](https://semver.org/); the
release workflow takes a release's notes from its section here.

## Unreleased

- **Jump to Warp** (macOS): `J` and `swarmy jump` focus the Warp tab and pane a Claude Code runs
  in, through the session's `warp://session/<id>` URL. No setup needed.

## v0.2.1

Codex logins in the vault, and Codex 0.172's tool calls read as what they run.

- **Codex logins in the vault**: save, add (`swarmy accounts add --harness codex`) and switch
  them like the others. OpenAI's refresh tokens are single-use, so a Codex login is in one seat
  only: putting it into another seat moves it (the two seats swap their logins, or the one it
  leaves keeps none, its copy saved), and never while Codex runs in either seat. The vault follows
  each refresh Codex makes in the seat.
- Codex 0.172's code mode: a script that runs tools is counted as one call named after the tool
  it calls (`exec_command`) with its commands as the summary, not as `exec` with the script.

## v0.2.0

Three clients side by side (Claude Code, Antigravity, Codex), worktrees and branches, a clearer
vault, and Homebrew.

**Clients**

- **Antigravity (`agy`) next to Claude Code**: its seats (`~/.gemini/antigravity-cli`, the `HOME`
  of running agys, `[[seat]] harness = "agy"`), live conversations and subagents, spend, tools
  and projects in every panel and command.
- **OpenAI Codex as a third client**: its homes (`~/.codex`, `~/.codex-*`, the `CODEX_HOME` of
  running `codex` processes, `[[seat]] harness = "codex"`), threads read from their rollouts
  (each response once, though Codex writes its usage twice), tool calls, prompts, thread names
  and the account a thread runs under; open threads are live in SWARM (thinking, a tool, idle).
  Codex seats show their spend in LIMITS · SEATS (no local quota is read yet); its logins are not
  kept in the vault yet.
- One project per repository whichever client worked on it: PROJECTS shows which did (`cc+agy`);
  MODELS shows which client ran a model; BURN splits today's spend per client; `--harness` and
  `--by harness` everywhere.
- **Prices**: Gemini (4 Neon, Carbon, Argon, Hydrogen; 3.x; 2.5, including the Carbon/Argon
  change on 2026-12-01) and OpenAI (GPT-6 and 5.x, the Codex models, the 272K long-context rule
  and the priority tier) as dated built-in rows; `tools/prices-from-litellm.py` compares the
  built-in rows with LiteLLM's list.

**Worktrees and branches**

- SWARM names a session's checkout (`shop/fix-1` for a linked git worktree), its branch, and
  `⇄N` when several live sessions work in the same checkout; the AGENT card has a checkout line;
  PROJECTS shows the share of spend made in worktrees (IN WT); `g` groups by checkout;
  `report --by worktree`, `--by branch` and `--checkout worktrees|main`.

**Vault**

- LIMITS · SEATS shows, per client, a table of seats (dir, the login in each) and a table of
  saved logins (the seats each is in, or free); `swarmy accounts` the same.
- A saved login stays listed after it goes into a seat and can go into more seats; the seat's own
  login is still saved first. `--share` is gone.
- Antigravity logins are saved, added (`swarmy accounts add --harness agy`) and switched like
  Claude Code's; a login only goes into seats of its own client; labels are unique per client.
- macOS: the dashboard and the commands say up front that Claude Code logins cannot be switched
  there (Claude Code keeps them in the Keychain) instead of failing after a try.

**Elsewhere**

- **Jump beyond tmux**: `J` and `swarmy jump` reach a Claude Code in Zellij, kitty, WezTerm and
  iTerm2 too (read from the process's environment, on Linux and macOS).
- **Accounts per organization**: an account is the pair of user and organization, so one login
  in two organizations keeps two weeks and two plans; the migration keeps parked organizations
  apart and finishes an interrupted re-attribution.
- **Homebrew**: `brew install koorchik/tap/swarmy` (macOS universal binary, Linux x86_64 and
  aarch64).
- The index is read again once on the first start (cache version 7, facts version 5).

## v0.1.0

The first release: mission control for Claude Code agents in the terminal.

- **Live swarm**: every agent of every Claude Code session across several config dirs as a live
  tree with activity lanes, timers, context fill and cost; agents that need you blink, ring the
  bell and come first in ATTENTION; `J` jumps to their tmux pane.
- **Spend at API prices** by project, account, model, agent type, session and day, from the
  transcripts Claude Code writes; prices are versioned data.
- **Weekly limits**: how much of each account's week is used, where it went per project, and when
  it runs out at the current burn rate; optional exact readings through the status line
  (`swarmy setup`, `swarmy statusline`).
- **Login vault**: save a config dir's OAuth login, switch another account in when a limit runs
  out, and back, with backups and atomic writes.
- Commands: `report`, `models`, `limits`, `accounts`, `doctor`, `prices`, `compact`, `setup`,
  `jump`, `statusline`, `demo`, `man`, `completions`.
- Offline and read-only (the vault switch is the one write); a bounded SQLite index with a
  quarter-hour archive; built to stay light on a laptop battery.
- Static binaries for Linux (x86_64, aarch64, ARMv7, ARMv6 — Raspberry Pi included), Windows
  x86_64 and macOS (universal), `.deb` and `.rpm` packages, and a Homebrew formula.

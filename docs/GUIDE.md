# How swarmy works: a guide for readers of the code

swarmy never talks to Anthropic. Everything it shows is reconstructed from files that Claude Code
already leaves on disk. This guide explains which files, how they turn into numbers, and where in
the source each step lives. Read `AGENTS.md` for the rules contributors must keep; this guide is
the "why" behind them.

Contents: [1 Data sources](#1-data-sources) · [2 Cost](#2-cost-is-api-equivalent) ·
[3 Limits](#3-limits-reading-estimating-projecting) · [4 Live agents](#4-the-live-agent-tree) ·
[5 Vault](#5-the-login-vault) · [6 Front ends](#6-three-front-ends-one-model) ·
[7 Module map](#7-module-map) · [8 Guard rails](#8-guard-rails-that-shape-every-change) ·
[9 Where to start](#9-where-to-start)

## 1. Data sources

| What | Where (per seat, a Claude Code config dir) | Used for |
|---|---|---|
| Transcripts | `projects/<project>/<session>.jsonl` | every request: model, tokens, tool calls, cwd, timestamps |
| Session registry | `sessions/<pid>.json` | which Claude Code processes are running, and their state |
| Global config | `.claude.json` | account identity (`oauthAccount`), cached usage numbers (`cachedUsageUtilization`) |
| Credentials | `.credentials.json` | only read by the vault, never by reports |
| Status line input | the JSON Claude Code pipes to a status-line command | exact limit readings, once `swarmy setup` is done |

A *seat* is one config dir. One machine can have several (work, personal). One *account* owns the
weekly limit, and two seats signed in to the same account share it. Code: `src/claude/` reads the
formats, `src/index/source.rs` and `src/index/write.rs` feed the SQLite index.

Claude Code deletes old transcripts (30 days by default), so the index is not just a cache. Once
a transcript is gone the index is the only record. `docs/design/storage.md` explains how detail
is kept for 14 days and then rolled up into quarter-hour rows that are kept for years.

## 2. Cost is API-equivalent

Each assistant message in a transcript carries a usage block. swarmy multiplies those tokens by a
list price per model. This is **not your bill**. On a subscription it measures how much work you
got. Three details matter when you change anything here:

- **Deduplication by `message.id`.** Claude Code writes the same message to a transcript more than
  once (streaming, resumed sessions). Counting each line would double the cost.
- **Prices are versioned data.** `src/core/pricing.rs` plus the built-in table; Claude Code's
  cached prices and `config.toml` can override it. A price change applies from the day the index
  first saw it, so history does not move when prices do.
- **Long-context and fast modes** are separate dimensions, because they price differently.

Reports group these rows by any dimension (`Dim` in `src/index/query.rs`): project, model, day,
hour, session, branch, agent, and so on. `swarmy report --by <dim> --period <p>` and the web
Spend tab both call `Ctx::spend_report` in `src/app/views.rs`.

## 3. Limits: reading, estimating, projecting

Anthropic reports a utilization percentage for a 5-hour and a weekly window. swarmy cannot ask for
it, so it gets a *reading* two ways:

1. `cachedUsageUtilization` inside `.claude.json`, which Claude Code refreshes.
2. The status line: after `swarmy setup`, Claude Code pipes exact numbers to swarmy on every
   prompt.

Between readings swarmy **estimates**. It knows the API-equivalent cost since the last reading and
the account's measured dollars per percentage point, so `reading + cost_since / usd_per_point`
gives the number shown with a leading `≈`. From the same ratio it derives:

- **burn**: dollars per hour over the last hour, and the same in percent per hour;
- **runs out**: when the window reaches 100% at that burn, and whether that is before it resets;
- **per-project shares**: the window's cost split by project, so "≈ 26% of the limit" is
  attributable.

The utilization Anthropic reports also covers claude.ai, the desktop app and other computers,
which leave no transcripts here. When two accounts have the same plan, the one with the highest
spend per point shows the least hidden usage, and its ratio is the reference for the rest; the
gap is reported as usage outside this machine.

Level thresholds are 60% (`Warn`), 85% (`Bad`) and 100% (`Reached`), in `core::limits::Level`. Code:
`src/limits/mod.rs` builds the report, `src/core/limits.rs` holds the pure maths.

## 4. The live agent tree

A running Claude Code process registers itself in `sessions/<pid>.json`. swarmy joins that with the
transcript tail and the process table (`sysinfo`) to get each agent's state:

| State | Meaning |
|---|---|
| working | running a tool |
| thinking | waiting for the model |
| waiting for you | blocked on a permission, question or dialog |
| waiting for agents | idle main thread with background agents still running |
| stalled | unfinished, with no transcript activity for a while |
| idle / done | waiting for the next prompt / finished its task |
| exited | the process is gone |

`State::hint` in `src/live/state.rs` turns each state into one sentence of advice, which the AGENT
card and the web detail rows show. Sub-agents hang under their parent, so a session reads as a
tree. `live::Reads` caches per-agent reads and only re-reads an agent when the index reports new
data, which is what keeps the dashboard under its CPU budget.

An agent's latest tool calls and its life (what each request cost, how full the context was) are
read on request, for one agent at a time: `Reads::detail` in `src/live/detail.rs`, under the same
rule. The AGENT card of the terminal dashboard and the web view's expanded row show the same two
things; the web page asks for an agent (`/api/agent?key=…`) only while its row is open.

## 5. The login vault

The vault stores OAuth logins and switches which one a seat uses. It is the only code that ever
writes into a Claude Code config dir, so it is the most defended part of the project.

- Claude Code rotates the refresh token on most refreshes, so a saved copy goes stale while a
  login is in use. Before every change the vault takes the seat's current tokens back first.
- Every change goes through `Vault::edit`: take the OS lock, re-read under it, back up, write
  atomically. A switch is all or nothing for the seat; if the second file fails to write, the first
  is put back.
- `.credentials.json` is replaced only while it still holds what was saved
  (`store::replace_if_unchanged`). `.claude.json` is merged, touching only `oauthAccount`, and the
  write fails rather than overwrite a version it has not read.
- Read paths (`Ctx`, reports, the web view) use `Vault::seat_states`: no lock, no write.
- Secrets live in `vault/secrets.json`, mode 0600, the same protection Claude Code gives the same
  tokens.

Never test the vault against real config dirs. Use temp dirs (see `src/vault/tests.rs`) or a fake
`HOME`.

## 6. Three front ends, one model

All three get their numbers from `Ctx` (`src/app/`), which owns the index, the vault, prices and
the clock. A front end formats; it does not compute.

- **TUI** (`src/tui/`): ratatui. A worker thread owns all slow work on the schedule in
  `src/tui/jobs.rs`; the UI thread only draws cached cells.
- **CLI** (`src/cli/`): `report`, `limits`, `accounts`, `doctor`, `setup`, plus the plain-language
  `guide` and `explain`. `limits` and `report` build their data in `src/app/views.rs`.
- **Web** (`src/web/`): `swarmy web`. A std-only HTTP server with reader threads and one worker
  that owns a `Hub` (context, index, process table). The page is plain HTML/CSS/JS embedded in the
  binary and built with DOM text nodes only.

The web server's security posture, because it exposes real project names:

- binds 127.0.0.1 only; GET only;
- `Host` must be loopback (defeats DNS rebinding); `/api/*` rejects `Sec-Fetch-Site: cross-site`;
- strict CSP (no inline script or style, no external origins), nosniff, frame denial;
- no secrets are ever put into a response: the API serves the same masked views as the CLI.

The glossary (`src/ui/glossary.rs`) is the single source for every plain-language definition: the
`explain` command, the web Help tab and the web tooltips all read it, and a test fails if the
page uses a term the glossary lacks.

## 7. Module map

```
src/
  claude/   parse Claude Code files: transcripts, registry, .claude.json, seats
  index/    SQLite index: ingest, queries, settled-span cache, archive rollups
  core/     pure types and maths: ids, units, periods, pricing, limits
  limits/   per-account limit report built from index + readings
  live/     the agent tree: states, hints, per-agent read cache (detail.rs: tools and life)
  procs.rs  process table (CPU, memory, children)
  vault/    saved logins and seat switching (the only writer)
  app/      Ctx: the shared model, plus views.rs (report data shared by front ends)
  ui/       glyphs, palette, glossary
  tui/      terminal dashboard
  cli/      subcommands
  web/      browser dashboard (http.rs parse/respond, api.rs JSON, timeline.rs the open row's
            data, mod.rs server, assets/)
  demo/     the made-up world behind `swarmy demo` and `swarmy web --demo`
  agy/ codex/   Antigravity and Codex readers (not covered here)
```

Data flow: files on disk → `claude/` parsers → `index/` (SQLite) → `Ctx` queries → `limits/` and
`live/` → a front end.

`tests/arch/edges.txt` lists every allowed module-to-module dependency, each with a reason. A
dependency that is not on the list fails `tests/architecture.rs`.

## 8. Guard rails that shape every change

- **Offline and read-only**, except the vault's explicit switch. No network, no telemetry.
- **No personal data in the repo.** Tests and screenshots use synthetic data; screenshots come from
  `swarmy demo`.
- **Every fix comes with a test** that fails before and passes after.
- **Ratchets** (`tests/arch/ratchets.txt`) count things like `_ =>` match arms, silently dropped
  errors and over-long functions. A number may go down, never up. Prefer exhaustive matches so a
  new enum variant is a compile error instead of a silent default.
- **Battery budget**: about 2 CPU-seconds per minute on a busy wall. `tools/perf.sh` is the guard;
  run it before touching the worker, the index reads or the view.
- `dead_code` is denied; unused code does not stay.

## 9. Where to start

| You want to… | Read first |
|---|---|
| see it with no setup | `swarmy demo` or `swarmy web --demo` |
| understand a number on screen | `swarmy explain <word>` or `src/ui/glossary.rs` |
| change how cost is computed | `src/core/pricing.rs`, `src/index/query.rs` |
| change limit estimates | `src/core/limits.rs`, then `src/limits/mod.rs` |
| change what an agent state means | `src/live/state.rs` |
| add a report grouping | `Dim` in `src/index/query.rs`, then `src/app/views.rs` |
| add a web view | `src/web/api.rs` (JSON), `src/web/assets/app.js` (view), the guard tests in `src/web/mod.rs` |
| touch the vault | `src/vault/mod.rs` header, then `src/vault/tests.rs`; read section 5 twice |

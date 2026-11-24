# AGENTS.md — swarmy

A real-time analytics dashboard for coding agents (Claude Code, Antigravity, Codex): a Rust CLI + TUI that reads what they write to disk
(transcripts, session registry, `.claude.json`) across several config dirs, shows the live agent
swarm, API-equivalent spend, weekly-limit attribution per project, and keeps a vault of OAuth
logins. Users run it in a spare terminal, mostly on laptops.

## Ground rules

- **English only**: UI text, docs, code, commits.
- **Offline and read-only.** No network calls, no telemetry. The harnesses' directories (Claude
  Code, Antigravity, Codex) are only read; the single write is an explicit vault switch (`src/vault`),
  which saves the seat's current login first, backs up the seat's files, writes atomically,
  merges only `oauthAccount` into `.claude.json` as it is right before the write, and keeps
  `mcpOAuth` (Claude Code); for an agy seat it replaces `antigravity-oauth-token` whole, for a
  Codex seat `auth.json` whole. A login belongs to one harness and may be in several seats of it
  (`Login::mounted`) — except Codex (`Harness::shares_logins`): OpenAI's refresh tokens are
  single-use, so a Codex login is in one seat only and putting it into another moves it
  (`VaultTx::switch_moving`: the seats swap, or the one it leaves keeps no login), never while
  Codex runs in either seat. Every vault change goes through
  `Vault::edit` (OS lock, re-read under it, backup, atomic writes); never write vault files elsewhere.
  A switch is all or nothing for the seat (`vault/switch.rs`): `.credentials.json` is replaced only
  while it holds what was saved (`store::replace_if_unchanged`), `.claude.json` through
  `store::rewrite_merging`, which fails rather than write over a version it has not read; a failed
  second write puts the first back. Read paths (`Ctx`, reports) use `Vault::seat_states` (no lock,
  no write); the dashboard does the bookkeeping with `Vault::try_edit` (skipped when busy).
- **No personal data in git**: no real home paths, user names, emails, account UUIDs or real config
  dir names in code, tests, fixtures, docs or screenshots. Tests use synthetic data; screenshots and
  GIFs come only from `swarmy demo`. Check the staged diff before every commit.
- **Never test the vault on real config dirs.** Use temp dirs (see `src/vault/tests.rs`) or a fake
  `HOME`.
- **Battery budget.** The dashboard must stay around 2 CPU-seconds per minute (≈3.4% of one core)
  on a 4K wall with 15–20 live agents.
  Measure CPU time of the process, not wall time (other agents load the machine):
  ```sh
  P=$(pgrep -x swarmy); T0=$(awk '{print $14+$15}' /proc/$P/stat); sleep 30
  T1=$(awk '{print $14+$15}' /proc/$P/stat); echo "$(( (T1-T0)/30 ))% of a core"
  ```
  Measure *your* process (`pgrep -x swarmy` may find the owner's own instance), per thread from
  `/proc/<pid>/task/*/stat`. `SWARMY_DEBUG=1 swarmy 2>log` prints per-tick and per-frame
  timings (and the job table at start). Every cadence lives in the job table `src/tui/jobs.rs`
  (period, battery period, what it reads; tests pin the measured values and keep heavy jobs at
  ≥ 30 s); the worker holds no intervals of its own. Per-second work may only touch running sessions (`Index::refresh_hot` with cached dir
  listings, `ProcTable::for_pids`, per-agent totals reused when no new request arrived); period
  aggregates (2 min), limits (1 min), efficiency (5 min), the previous period (10 min), lanes and
  session totals (10 s), history charts (5 min), the full process table and disks
  (SYSTEM open) run rarely or on demand. `.claude.json` is rewritten by Claude Code every few
  seconds — never trigger heavy work on its mtime.
  What keeps it there (each was a regression once; keep them when changing the code around):
  - Long spans are not summed again on every pass: `index/settled.rs` cuts a span at the settled
    horizon (a quarter hour ≥ 15 min old) and keeps the part before it per (settled generation,
    price fingerprint); only the last quarter hours are read again. `spend_by`, `total`,
    `matrix`, `model_substitutions`, the quarter-hour buckets, `spend_series`, `spend_since` and
    the per-minute burn go through it (dims whose labels can change later — agent type,
    session, agent — do not). A new recurring sum over a long span goes through it too.
  - Per-agent reads in `live::build` are kept in `live::Reads` and read again only when
    `Index::agent_seq` moved (the index read that agent's transcript further, or another
    process committed) and on heavy ticks; the selected agent's timeline likewise.
  - The hot refresh looks up its few files by path (`plan`), never the whole `files` table,
    runs no `COUNT(*)` over `requests`, and its writes use cached statements
    (`prepare_cached`); a live session's spend is read again only when `Index::session_seq`
    moved.
  - The UI keeps each panel's cells (`App::shots`, `view::draw_panel`) and pastes them until a
    message or an input arrives (`App::rev`) or the second changes; pulses and the spinner draw
    marks that are patched each frame (`kit::pulse`, `SPINNER_MARK`), so animated panels are
    kept too; a blinking panel is drawn again when the blink flips; anything else reading
    `App::t`/`animating` makes its panel drawn every frame. `--mask` blanks words in one pass
    per row (`mask_words`).
  - The guard: `tools/perf.sh` (and `tools/perf.sh --mask`) runs a release build on
    `swarmy demo --wall` (three Claude Code seats, one agy seat and one Codex home, ten days of ~12,600 requests a
    day, ~18 agents at work)
    in a 326×81 tmux window and fails above 2.0 (2.3 with `--mask`) CPU-seconds per minute.
    Run it before merging anything that touches the worker, the index reads or the view. CPU
    time still moves by 10–15% with the machine's load (SMT siblings): near the budget, run it
    again on a quieter machine before concluding.
- Prices are versioned data (`src/core/pricing.rs`, the built-in table in `pricing/builtin.rs`, overridable by Claude Code's cache and
  `config.toml`), never ad-hoc constants. Built-in prices carry their model's date; each version
  of Claude Code's cached prices applies from the day the index first saw it; an override derives
  what it leaves out from the entry it overrides (`Rates::derive`). Money numbers in tests are
  computed by hand in comments.
- Limit rules live once, in `src/core/limits.rs`: `Level` (60/85/100 thresholds; front ends only
  map a level to their colour), `current_window`, `runs_out`, `projection_at_reset`, `was_reset`, and `window_of`, the window in force (a reset by hand moves its start; `limits`, the dashboard and `report --period week` all use it). Readings of one window are named by their reset to the minute; a fall counts as a reset only when it holds across sources (`effective_start`).
  Every surface reads an account's limit from `Ctx::latest_readings` (index + seats, newest wins)
  through `limits::all_reports`; the `limit_threshold_copies` ratchet keeps copies out.
- Identities are newtypes in `src/core/ids.rs`: `SeatId` (the first 16 hex digits of the SHA-256
  of the seat's canonical config dir), `SessionId` (`short()` for display, never `[..8]`),
  `AccountId`. Every `seat` column, the vault (`mounted`, `seats`), the status-line files and
  `fleet.json` hold the `SeatId`; a seat's label is display only (the index's `seats` table names
  ids for reports). Stores written by an older swarmy keyed seats by label: the index lists them
  as legacy rows of `seats` (`FACTS_VERSION` 2) and the vault as `mounted_label`/`legacy_seats`
  (`logins.json` `v` 2); both move to the id the first time a seat with that label is seen
  (`index::seats`, `vault::logins`), nothing is dropped.
- The index schema (`src/index/schema.rs`) has raw tables (requests, tool calls, … dropped and
  read again from transcripts when `CACHE_VERSION` changes), dimension tables that label them and
  outlive Claude Code's 30-day transcript retention (`sessions`, `agents`, `projects`, `branches`,
  `cost_states`: kept across a bump, merged into on a re-read), archive tables (quarter-hour
  rollups of raw rows that are gone, `ARCHIVE_VERSION`, never dropped), and fact tables that
  nothing else records (`seat_accounts`, `accounts`, `readings`, `observed_prices`, `seats`: migrated by
  `FACTS_VERSION` steps, never dropped). A new table goes into exactly one list; data rules are `CHECK`s, enum
  lists generated from the Rust enums.
  `cost` is NULL until priced (`priced` NULL) or when no price is known (`priced = 0`).
- Bounded storage (`docs/design/storage.md`, §11 for what the code does): raw rows live
  `keep_raw_days` (`[storage]`, default 14, `core::storage::keep_raw_days` checks it against the
  weekly window and each seat's `cleanupPeriodDays`), then `Index::compact` rolls them into the
  quarter-hour archive. Every read of a span reaching `rolled_until` goes through
  `archive::plan`; a repricing (`price_all`) and a projects recompute cover the rollups too
  (a login change never moves a rollup: it is dated when seen, and owners decide); a file read
  from its start skips rolled-up records (`marks.rs`: whole responses, decided by their first
  record; only what was read before or what the archive provably holds), and a refresh rolls up
  at once whatever a newly found seat added below `rolled_until`. A `CACHE_VERSION` rebuild keeps
  the raw rows no transcript gives back (`schema::stash_unrereadable`); nothing read is dropped
  silently (`unreadable_files`, damaged records in `record_types`). Compaction is `Job::Compact`
  in the dashboard and `swarmy compact` (`--dry-run`, `--verify` on a copy, `--vacuum`); prove a
  change with
  `compact_scenarios.rs` (compacted index = never-compacted index on every surface). SQLite's
  temp files go to `<data>/tmp` (`index::use_temp_dir`), never `/tmp`.

## Layout

| Path | What |
|---|---|
| `src/lib.rs`, `src/main.rs` | the library holds everything; the binary only parses arguments and calls `cli::run` |
| `src/core/` | pure domain: usage, pricing, periods, limits math, storage rules (raw window, growth, projection; no I/O) |
| `src/agy/` | readers of Google Antigravity's files: seat discovery, `brain/<cid>/…/transcript.jsonl`, conversation metadata (SQLite reads in `index/agy_summaries.rs`, cached by file stamp), live conversations from `presence/*.lock` (Linux: `/proc/locks`, no lock of ours), the token file (`credentials.rs`) and its account |
| `src/codex/` | readers of OpenAI Codex's files: home discovery, rollouts (`rollout.rs`: the state before an offset kept per file, replayed once when missing), thread names and finding a thread's rollout (`meta.rs`), open threads from `thread-writer-locks/*.lock` (`live.rs`), the account of `auth.json` (`account.rs`) |
| `src/fsutil/locks.rs` | which lock files of a dir another process holds (Linux: `/proc/locks`, with the holder's pid; elsewhere a shared-lock probe), for agy and Codex |
| `src/core/harness.rs`, `src/core/login.rs` | the harnesses and what each supports; a login's secret and non-secret facts per harness (pure) |
| `src/claude/` | readers of Claude Code's files: seats discovery, transcripts, registry, `.claude.json`, credentials, processes, projects |
| `src/index/` | SQLite index (incremental by inode/offset, offsets checked again inside an immediate transaction), schema (in-place `CACHE_STEPS`), queries; `write.rs`: one file's records into the batch transaction; `insights.rs`: efficiency figures (tools, commands, waits, cache, agent types, fleet, conflicts); `prices.rs`: dating Claude Code's prices, repricing; `archive.rs`: rollup columns declared once (`KEYS`, `SUMS`), read plans (a span inside the raw window reads `requests` with the old SQL; one reaching `rolled_until` reads the union with the rollups); `archive_scenarios.rs`: the proof that rolled-up spend reads the same everywhere; `compact.rs`: the compactor (raw rows older than `keep_raw_days` into the rollups, one transaction per 6-hour chunk; giving space back, the one-time `VACUUM`), proved in `compact_scenarios.rs`; `projects.rs`: the cwd → project table, recomputed over raw and rolled-up cwds, never downgraded; `storage.rs`: the index's size, daily samples, last compaction (doctor, SYSTEM); `marks.rs`: read marks, the one rule that keeps a file read again after a compaction from counting rolled-up records twice; `scenarios.rs`: scenario tests on synthetic seats; `live.rs`: typed reads for `live` (the connection is private: SQL stays in `index/`) |
| `src/live/` | live agent states for the dashboard; `state.rs`: how an agent's state is decided (pure, table-tested) |
| `src/statusline/` | `swarmy statusline`, its own front end: Claude Code's stdin (`input.rs`, the ACL), the reading file shared with the dashboard (`reading.rs`), the pure renderer (`render.rs`), the runner (`mod.rs`) |
| `src/vault/` | login vault (`mod.rs`: `Vault`, `VaultTx`, saving), what a seat holds and following its login (`seats.rs`), switching (`switch.rs`), files, lock and checked writers (`store.rs`, private) |
| `src/app/` | composition root `Ctx` (dirs, config, seats, prices, index), used by commands and dashboard |
| `src/limits/` | limit analysis per account (`all_reports`): queries feeding the pure rules of `core::limits` |
| `src/cli/` | commands: report, models, limits, accounts, doctor, prices, compact |
| `src/tui/` | dashboard: UI loop (`mod.rs`), worker thread (`worker.rs`) on the cadences of `jobs.rs`, analytics passes (`analytics.rs`), state (`app/mod.rs`) and keys (`app/keys.rs`), what needs attention and what gets logged (`alerts.rs`: pure, tested), vault in the LIMITS · SEATS panel (`vault_ui.rs`), boot animation, themes |
| `src/tui/view/` | drawing: layouts wall / column / tabs, header, footer (`mod.rs`); dialogs and toast (`overlays.rs`); help and legend (`help.rs`); panels that fill any rect (`panels/`, one file per panel, shared helpers in `panels/mod.rs`); primitives (`kit.rs`) |
| `src/demo.rs` | made-up world for `swarmy demo` and README media (`demo/agy.rs`, `demo/codex.rs`: those harnesses' seats) |
| `tools/render_frames.py` | renders recorded frames to PNG/GIF |
| `tools/record-readme.sh` | regenerates `docs/img/*` |
| `tools/prices-from-litellm.py` | compares the built-in prices with LiteLLM's list (run by hand; swarmy stays offline) |
| `tools/perf.sh` | the battery budget measured on `swarmy demo --wall`, fails above it |
| `tools/dist.sh`, `tools/release-local.sh` | release steps shared with `.github/workflows/release.yml`; the local Linux + Windows build into `dist/` |
| `packaging/` | Homebrew formula template (`homebrew/swarmy.rb`) and the release / tap / signing notes (`README.md`) |

## Claude Code format notes (verified on 2.1.329)

- One API response is written as several `assistant` records (one per content block) with the same
  usage: dedupe by `message.id`, merge with `max`.
- `session-owner` records give the account per turn; subagent files inherit it from the session.
  They decide a request's account (`index::SEAT_ACCOUNT_AT`); a seat's login history
  (`seat_accounts`, a change dated when swarmy sees it, never at `profileFetchedAt`) only fills
  in for sessions without one.
- `.credentials.json`: Claude Code refreshes the access token every few hours and keeps the
  refresh token, so a login (OAuth grant) is named by the refresh token's fingerprint. A rotated
  refresh token (same account and plan, fresh access token) is followed as the same login
  (`vault/seats.rs::is_rotation`), never taken for a new one.
- `sessions/<pid>.json` holds live status (`busy`/`idle`/`waiting` + `waitingFor`), tmux pane and
  `procStart` (check it against `/proc/<pid>/stat` to ignore reused pids). Other terminals and
  multiplexers (Zellij, kitty, WezTerm, iTerm2, Warp) are not recorded there: `claude::proc_env` reads
  their environment variables once per process (`/proc/<pid>/environ` on Linux, `sysinfo` single-PID
  refresh on macOS) and parses them through `core::pane::Pane::from_environ_pairs`.
- `.claude.json`: `cachedUsageUtilization` (limits, no network needed), `additionalModelCostsCache`
  (prices), `oauthAccount`. For `~/.claude` it lives at `~/.claude.json`, otherwise inside the dir.
- Unknown record types are counted; `swarmy doctor` reports them (format drift).
- Status line (optional, `swarmy setup`): Claude Code runs `statusLine.command` after each response
  with a JSON on stdin; `rate_limits.{five_hour,seven_day}.{used_percentage,resets_at (epoch s)}`
  are exact. `swarmy statusline` must stay fast (no index, no scan): it writes
  `<data>/statusline/<seat id>.json` (`SeatId`) and prints one line. A reading (`v` 2) names
  its account (the owner of the session's last response, read from the transcript's tail only
  when the numbers moved; else the seat's `oauthAccount`) and the time its numbers were true (that
  response's time; a repeat never moves it, an older reading never goes over a newer one). The
  dashboard merges a reading only into the seat's account it names, when newer than
  `.claude.json`'s. `setup` writes only `settings.json` (backup
  first, never over an older backup) and only after a yes. The status line reads the dashboard's
  `statusline/fleet.json` (`src/fleet.rs`; fresh while < `fresh_seconds` and its writer pid runs)
  and otherwise scans the session registries; segments shorten in a fixed order to `$COLUMNS`
  (Claude Code sets it). Keep its vocabulary and glyphs the same as the dashboard's.
- Testing `setup` or anything that discovers seats: seats are also found through running
  `claude` processes' `CLAUDE_CONFIG_DIR`, so a fake `HOME` is NOT isolation — the real seats
  still show up. Set `SWARMY_ISOLATED=1` together with the fake `HOME` (and unset
  `CLAUDE_CONFIG_DIR`), and check the seat list before any `--yes`.
- Checkouts: `projects.worktree` names the linked worktree a cwd is in (`.git` file); each
  record's `gitBranch` is kept as `branches` rows (one per change, per agent; `HEAD` is a detached
  HEAD), which label the rollups too (a branch is read by quarter hour, so raw and rolled-up
  reports agree). `report --by worktree|branch`, `--checkout worktrees|main`, `g` in the
  dashboard, `⇄N` on sessions sharing one checkout (`live/checkout.rs`, pure). Nothing is read
  from disk per tick: the live view takes the cwd of the agent's last request and `projects`.
- Projects: a cwd resolves to its git root; removed worktrees under `<repo>-wt/` fold into
  `<repo>`; a dir outside any repo (gone, or a scratch dir) takes the project of the session it ran
  in, or the session's own dir when it lies under it (never a home dir). Changing these rules means
  bumping `PROJECTS_REV` in `src/index/mod.rs`, which recomputes the table once.

## Antigravity (agy) format notes (verified on the agy CLI of 2026-11)

- Seat dir `$HOME/.gemini/antigravity-cli`: the CLI keys it on `HOME` and ignores
  `ANTIGRAVITY_APP_DATA_DIR` for its token and conversations (verified), so a scratch seat for a
  new login runs agy with `HOME=<scratch>` (`vault::new_login`); running agys' seats come from the
  `HOME` of their environment. One conversation per
  `brain/<cid>/.system_generated/logs/transcript.jsonl`, one step per line: `PLANNER_RESPONSE`
  (`model` with an effort suffix such as `gemini-4-neon-high`, `input_tokens` without the cached
  part, `cache_read_tokens`, `output_tokens`, `tool_calls`), `GENERIC` (tool `k`'s result at
  `step_index + 1 + k`), `USER_INPUT`, `CHECKPOINT` (tokens but no model: not priced),
  `ERROR_MESSAGE`, `SYSTEM_MESSAGE`. Older steps may have neither model nor tokens: they are
  `<synthetic>` records (tools kept, no request).
- Titles, workspaces, run status and subagent parents in `conversation_summaries.db` (read-only,
  whole table, cached by the stamp of the db, its WAL and `cache/conversation_metadata.json`, the
  fallback without status or parents).
- A running agy holds `FLOCK WRITE` on `presence/<cid>.lock`; stale lock files stay behind.
- The login is the whole `antigravity-oauth-token` (`token.refresh_token`, `token.expiry`,
  `auth_method`, `id_token_email`); agy skips saving a refreshed token when the file changed
  under it, so a vault switch is not undone by a running agy.

## Codex format notes (verified on Codex CLI 0.172.0)

- Home `~/.codex`, or `CODEX_HOME`. Threads in
  `sessions/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<thread uuid>.jsonl`, names in
  `session_index.jsonl` (`{id, thread_name, updated_at}`), login in `auth.json`.
- A rollout line is `{timestamp, type, payload}`: `session_meta` (first line: `id`, `cwd`, `git`
  with `branch`, `cli_version`, `creator_user_id`, `creator_account_id`), `turn_context` (`model`,
  `cwd`, effort in `collaboration_mode.settings.reasoning_effort`), `response_item` (`message`,
  `additional_tools`, …), `event_msg` (`task_started`, `task_complete` with `duration_ms`,
  `thread_settings_applied`, `item_completed` whose `item.type` `UserMessage` is a prompt,
  `token_count`), `token_usage_record`, `world_state` (the whole context: skipped).
- Usage is per response: `token_usage_record.usage` (with `response_id` and the thread's running
  `thread_token_usage`), then a `token_count` with the same numbers (`info.last_token_usage`,
  `info.total_token_usage`). `input_tokens` includes `cached_input_tokens`; `output_tokens`
  includes `reasoning_output_tokens` (`last_usage_includes_reasoning: true`). A response is named
  by the thread's running total (`cdx:<thread>:<total>`) so both lines are one request. The model
  is only on the `turn_context` before: `codex::rollout` keeps each file's state at the end of a
  read and replays the file up to an offset it has no state for.
- `token_count.rate_limits` gives `primary`/`secondary` `{used_percent, window_minutes,
  resets_at}`, `plan_type`; seen: one 43,200-minute (30-day) primary window on a `go` plan, no
  secondary. Not read yet: `core::limits` knows 5-hour and 7-day windows only.
- An open thread: Codex's `codex app-server` (a child of the `codex` the user runs) holds
  `FLOCK WRITE` on `thread-writer-locks/<thread>.lock` (`.coordination.lock` is not a thread).
  No process keeps the rollout open between writes.
- `auth.json`: `auth_mode` (`chatgpt`), `OPENAI_API_KEY`, `tokens` (`id_token`, `access_token`,
  `refresh_token`, `account_id`, …), `last_refresh`. The `id_token` claims name the account
  (`https://api.openai.com/auth`: `chatgpt_user_id` = `session_meta.creator_user_id`,
  `chatgpt_account_id` = `creator_account_id` = `tokens.account_id`, `chatgpt_plan_type`): an
  account is `cdx:<user>/<workspace>`. The access token lives 10 days. Refresh tokens are
  single-use: Codex says "your refresh token was already used. Please log out and sign in again"
  (in its binary), so the vault keeps a Codex login in one seat and follows each refresh there
  (`vault::seats::is_rotation`); the whole file is the login (`codex::credentials`).
- Code mode (0.172): the model writes a short script that calls tools
  (`await tools.exec_command({cmd: "ls -la"})`), run as one `custom_tool_call` named `exec`; the
  commands it ran come later as `item_completed` items of type `CommandExecution` (`command`,
  `parsed_cmd`, `exit_code`, `duration`). swarmy counts the script as one call, named after the
  tool it calls (`exec_command`, a shell) and summed up by its commands (`codex::rollout::code_mode`);
  the `CommandExecution` items are not counted again.
- Rollouts of 2025 versions have no envelope (a header `{id, timestamp, instructions, git}`, then
  bare items and `{"record_type": "state"}`), no timestamps and no tokens: counted as `legacy`.

## Commands

```sh
export CARGO_TARGET_DIR=/path/with/space   # builds are large
cargo test --no-fail-fast
cargo clippy --all-targets
cargo fmt
cargo run --release -- doctor
cargo run --release -- demo                # try the dashboard on fake data

# static Linux binary (zig 0.16 + cargo-zigbuild)
cargo zigbuild --release --target x86_64-unknown-linux-musl
readelf -l target/x86_64-unknown-linux-musl/release/swarmy | grep -c INTERP   # must print 0

# release packages as the tag workflow makes them (packaging/README.md): 4 musl targets, .deb,
# .rpm, Windows (cargo-xwin) into dist/
tools/release-local.sh

tools/record-readme.sh                     # refresh docs/img (screenshots + demo.gif)
tools/perf.sh && tools/perf.sh --mask      # battery budget on the wall load (needs tmux, ~5 min)
```

Releases: a `v<version>` tag matching `Cargo.toml` runs `.github/workflows/release.yml`
(notes from the `## v<version>` section of `CHANGELOG.md`). Code must build for Linux musl
(x86_64, aarch64, ARMv7, ARMv6 — 32-bit: no `usize` for file sizes or offsets), Windows (no
inode, no Unix modes; a rename over a file being replaced can fail for a moment —
`fsutil::rename_over` retries) and macOS; CI runs the tests on all three.

Allocator (measured 23.11.2026, x86_64 musl release, 32 cores; mimalloc was not adopted, the
memory doubled): cold index of 2,742 transcripts (10.6 GB, 3 seats) — musl malloc 27–37 s
wall, 32–45 CPU-s, peak RSS 270–480 MB; mimalloc 24–27 s, 33–35 CPU-s, 580–630 MB. Dashboard
on `demo --wall` 326×81, 12 min, side by side — musl malloc 1.46 CPU-s/min, RSS 178 MB;
mimalloc 1.32 CPU-s/min, RSS 365 MB (peak 408 MB).

Visual changes to the dashboard: record at the sizes that matter (326×81 wall, 160×81 column,
140×34 tabs) with `swarmy demo --record … --size WxH --keys …` and `tools/render_frames.py`,
look at the PNGs, fix, repeat; `tools/record-readme.sh` refreshes the README media.

UX principles (from the owner): use the whole screen — no tabs of mostly empty space on a big
terminal; every panel fills its rect; positions are stable (the selection is kept by key, the agent
tree is in start order, panel sizes do not depend on the agent count); colour only where it means
something; everything works from the keyboard.

Words on screen (keep them, and the legend in `view/help.rs::legend_lines`, in sync):
- **checkout** = where an agent works: a project's main checkout or one of its linked git
  worktrees, shown as `project/worktree`; "⇄2" = two live sessions in one checkout.
- **seat** = a Claude Code config dir; **login** = an OAuth sign-in (saved ones live in the vault);
  **account** = the Anthropic account behind a login, named after its saved login.
- Limits: "week used N%", "runs out in 7h (Mon 03:34)", "limit reached"; shares as "% of week",
  "1% ≈ $X". Never "mana" or "pt" on screen.
- Trends say what they compare with ("▲▼ vs previous 7d", "vs day before").
- Every new glyph, unit or colour gets a legend line; the footer keeps "? help & legend" visible.
- `--mask` hides by type at render time (`ui::mask`): emails, login labels made from an email's
  local part (`alice`, `alice-2`), and, in the finished frame, those local parts and the OS user
  name wherever they were drawn (`mask_word`).

Style rules the owner asked for:
- Panel titles and in-panel section headings sit on a plate (`Theme::heading()`, the
  `title`/`title_bg` colours of each theme); the focused panel's plate is the accent colour. New
  headings use `heading()`, not bold text in a data colour.
- Sessions in SWARM are separated by a rule; project and directory names use the theme's `dir`
  colour everywhere (session rows, agent card, recently done).
- Big block digits (`bigdigits.rs`) show the number only, with a blank column on each side of
  the decimal point; the unit goes in the caption under them (`$ per hour`), because a block `$`
  reads as another digit.

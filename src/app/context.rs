//! Everything a command needs: dirs, config, seats, prices, a fresh index.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::agy::discovery::{self as agy_discovery, AgyCandidates};
use crate::claude::config_json;
use crate::claude::project::Resolver;
use crate::claude::seats::{self, Candidates, Seat, SeatId};
use crate::codex::discovery::{self as codex_discovery, CodexCandidates};
use crate::config::{Config, Dirs};
use crate::core::harness::Harness;
use crate::core::ids::AccountId;
use crate::core::limits::{Reading, WindowKind};
use crate::core::period::Span;
use crate::core::pricing::PriceBook;
use crate::errors::LookupError;
use crate::index::{Index, Progress, RefreshStats, SeatInfo};
use crate::procs::ProcTable;
use crate::ui::fmt;
use crate::ui::style::Painter;

pub struct Ctx {
    pub dirs: Dirs,
    pub config: Config,
    pub seats: Vec<Seat>,
    pub seat_info: Vec<SeatInfo>,
    pub prices: PriceBook,
    pub procs: ProcTable,
    pub painter: Painter,
    pub mask: bool,
    pub now: DateTime<Utc>,
    /// Saved logins by account: names and plan tiers for accounts not in any seat right now.
    pub login_labels: std::collections::BTreeMap<String, LoginName>,
    pub login_tiers: std::collections::BTreeMap<String, String>,
    /// What could not be read and so may make numbers wrong (a damaged `.claude.json`, an
    /// unreadable vault): `doctor` lists them, the dashboard shows them in ATTENTION.
    pub problems: Vec<String>,
}

/// A saved login's name for its account, with the email it was made from (for `--mask`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginName {
    pub label: String,
    pub email: Option<String>,
}

impl Ctx {
    pub fn load(painter: Painter, mask: bool) -> anyhow::Result<Ctx> {
        let dirs = Dirs::resolve()?;
        let config = Config::load(&dirs)?;
        // SQLite's temporary files (a VACUUM's copy) next to the index, not in a RAM-backed /tmp.
        crate::index::use_temp_dir(&dirs.data.join("tmp"))?;
        let procs = ProcTable::snapshot();
        // `SWARMY_ISOLATED=1` (tests, demos): only dirs under $HOME and the config count, not the
        // config dirs of running `claude`, `agy` or `codex` processes, which belong to the real
        // user.
        let isolated = std::env::var_os("SWARMY_ISOLATED").is_some();
        let claude_cfg = config.seats_of(&dirs.home, Harness::Claude);
        let agy_cfg = config.seats_of(&dirs.home, Harness::Agy);
        let codex_cfg = config.seats_of(&dirs.home, Harness::Codex);
        let mut seats = Vec::new();
        if config.harness_enabled(Harness::Claude) {
            let mut env_dirs: Vec<PathBuf> = procs
                .claude
                .iter()
                .filter(|_| !isolated)
                .filter_map(|p| p.config_dir.clone())
                .collect();
            if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|_| !isolated) {
                env_dirs.push(PathBuf::from(d));
            }
            seats.extend(seats::discover(&Candidates {
                home: &dirs.home,
                env_dirs,
                configured: claude_cfg,
            }));
        }
        if config.harness_enabled(Harness::Agy) {
            let env_dirs: Vec<PathBuf> = procs
                .agy
                .iter()
                .filter(|_| !isolated)
                .filter_map(|p| p.app_data_dir.clone())
                .collect();
            seats.extend(agy_discovery::discover(&AgyCandidates {
                home: &dirs.home,
                env_dirs,
                configured: agy_cfg,
            }));
        }
        if config.harness_enabled(Harness::Codex) {
            let mut env_dirs: Vec<PathBuf> = procs
                .codex
                .iter()
                .filter(|_| !isolated)
                .filter_map(|p| p.home.clone())
                .collect();
            if let Some(d) = std::env::var_os("CODEX_HOME").filter(|_| !isolated) {
                env_dirs.push(PathBuf::from(d));
            }
            seats.extend(codex_discovery::discover(&CodexCandidates {
                home: &dirs.home,
                env_dirs,
                configured: codex_cfg,
            }));
        }
        // Keep labels unique across harnesses (seat ids never change): a later duplicate takes
        // `-2`, `-3`… whichever no seat has.
        let mut taken: std::collections::BTreeSet<String> = Default::default();
        for i in 0..seats.len() {
            let base = seats[i].label.clone();
            let mut label = base.clone();
            let mut n = 2;
            while taken.contains(&label)
                || seats[i + 1..]
                    .iter()
                    .any(|s| s.label == label && label != base)
            {
                label = format!("{base}-{n}");
                n += 1;
            }
            taken.insert(label.clone());
            seats[i].label = label;
        }
        let mut seat_info = Vec::new();
        let mut caches = Vec::new();
        let mut problems = Vec::new();
        for s in &seats {
            let (info, costs) = seat_info_of(s, &dirs.data, &mut problems);
            if let Some(costs) = costs {
                caches.push((s.id.clone(), costs));
            }
            seat_info.push(info);
        }
        seat_problems(&config, &dirs.home, &seats, &mut problems);
        let prices = price_book(&caches, &config, Utc::now())?;
        let mut ctx = Ctx {
            dirs,
            config,
            seats,
            seat_info,
            prices,
            procs,
            painter,
            mask,
            now: Utc::now(),
            login_labels: Default::default(),
            login_tiers: Default::default(),
            problems,
        };
        // `[storage] keep_raw_days` must fit the seats' transcript retention.
        ctx.keep_raw_days(None)?;
        ctx.check_unconfirmed();
        Ok(ctx)
    }

    /// Days each retention-bearing seat keeps transcripts (`cleanupPeriodDays`, else Claude Code's
    /// default), and whether the seat sets it. Harnesses without automatic deletion are skipped.
    pub fn retention(&self) -> Vec<(String, u32, bool)> {
        self.seats
            .iter()
            .filter(|s| s.harness.has_transcript_retention())
            .map(|s| match s.cleanup_period_days() {
                Ok(Some(d)) => (s.label.clone(), d, true),
                // A damaged file is in `problems` (`Ctx::load`).
                Ok(None) | Err(_) => (
                    s.label.clone(),
                    crate::core::storage::DEFAULT_RETENTION_DAYS,
                    false,
                ),
            })
            .collect()
    }

    /// The raw window: `explicit` (`compact --keep-days`), else `[storage] keep_raw_days`, else
    /// the default, checked against the weekly window and the seats' transcript retention
    /// (`core::storage::keep_raw_days`).
    pub fn keep_raw_days(&self, explicit: Option<i64>) -> anyhow::Result<i64> {
        let retention: Vec<(String, u32)> = self
            .retention()
            .into_iter()
            .map(|(s, d, _)| (s, d))
            .collect();
        crate::core::storage::keep_raw_days(
            explicit.or(self.config.storage.keep_raw_days),
            &retention,
        )
        .map_err(|e| {
            if explicit.is_some() {
                anyhow::anyhow!("--keep-days: {e}")
            } else {
                // A config error keeps its code (`config-invalid`; review B-L10).
                crate::errors::ConfigError::Invalid {
                    path: self.dirs.config_file(),
                    message: format!("[storage] {e}"),
                }
                .into()
            }
        })
    }

    /// Re-reads what every seat says about itself (account, limit readings, Claude Code's cached
    /// prices) and the `[[price]]` overrides of `config.toml`. The price book is replaced when its
    /// fingerprint changed, so the next refresh reprices the index with the same book a CLI run
    /// would build now. Returns whether the prices changed; an invalid config is an error and
    /// keeps the prices in force.
    pub fn reload_seat_info(&mut self) -> anyhow::Result<bool> {
        let mut caches = Vec::new();
        let mut problems = Vec::new();
        self.seat_info = self
            .seats
            .iter()
            .map(|s| {
                let (info, costs) = seat_info_of(s, &self.dirs.data, &mut problems);
                if let Some(costs) = costs {
                    caches.push((s.id.clone(), costs));
                }
                info
            })
            .collect();
        let config = Config::load(&self.dirs)?;
        seat_problems(&config, &self.dirs.home, &self.seats, &mut problems);
        self.problems = problems;
        self.check_unconfirmed();
        self.config.price = config.price;
        let prices = price_book(&caches, &self.config, self.now)?;
        if prices.fingerprint() == self.prices.fingerprint() {
            return Ok(false);
        }
        self.prices = prices;
        Ok(true)
    }

    /// [`Ctx::drop_unconfirmed`], with a failure kept in [`Ctx::problems`] (and on the debug
    /// channel) instead of stopping the command: the numbers are shown, with the reason they may
    /// be off.
    pub fn check_unconfirmed(&mut self) {
        if let Err(e) = self.drop_unconfirmed() {
            let msg = format!("vault unreadable, new logins cannot be confirmed: {e:#}");
            crate::diag::log("vault", &msg);
            self.problems.push(msg);
        }
    }

    /// Seats whose current login is not confirmed lose the stale profile, so they are not merged
    /// into another account and do not borrow its limit reading. A read: it takes no vault lock
    /// and writes nothing (`Vault::seat_states`), so a report never waits for a switch.
    pub fn drop_unconfirmed(&mut self) -> anyhow::Result<()> {
        let v = crate::vault::Vault::open(&self.dirs.vault_dir())?;
        // Names and plan tiers of saved logins, for accounts that are in no seat right now.
        self.login_labels.clear();
        self.login_tiers.clear();
        for l in v.logins() {
            if let Some(a) = &l.account_uuid {
                // Unmasked: `--mask` is applied where a name is shown (`shown_account_name`), so
                // what joins (`[subscription]` keys, `fleet.json`) never depends on it.
                self.login_labels
                    .entry(a.clone())
                    .or_insert_with(|| LoginName {
                        label: l.label.clone(),
                        email: l.email.clone(),
                    });
                if let Some(t) = l.plan.clone().or_else(|| l.meta.rate_limit_tier.clone()) {
                    self.login_tiers.insert(a.clone(), t);
                }
            }
        }
        let vault_seats: Vec<Seat> = self
            .seats
            .iter()
            .filter(|s| s.harness.supports_vault())
            .cloned()
            .collect();
        let states = v.seat_states(&vault_seats)?;
        for si in &mut self.seat_info {
            if let Some(st) = states.get(&si.seat) {
                si.switched_at = st.switched_at;
                if st.unverified_since.is_some() {
                    si.account = None;
                    si.utilization = None;
                } else if let (Some(switched), Some(u)) = (st.switched_at, si.utilization.as_ref())
                    && u.org_from_profile
                    && u.fetched_at < switched
                {
                    // `cachedUsageUtilization` had no `organizationUuid` of its own and predates
                    // the vault switch: it may belong to the seat's previous organization under
                    // the same `accountUuid`.
                    si.utilization = None;
                }
            }
        }
        // A reading for another user than the profile's (left in `.claude.json` by the login the
        // seat ran before a switch) names no organization: it takes the one the saved logins know
        // that user by, or is dropped rather than booked to a bare id no limit report matches.
        let known: Vec<AccountId> = v
            .logins()
            .iter()
            .filter_map(|l| l.account_uuid.as_deref().map(AccountId::new))
            .collect();
        for si in &mut self.seat_info {
            if let Some(u) = si.utilization.as_mut()
                && !u.account_id.has_org()
            {
                let profile = si.account.as_ref().map(|a| a.id());
                let done = std::mem::take(&mut u.account_id).completed(profile.as_ref(), &known);
                if done.has_org() || profile.is_some_and(|p| p == done) {
                    u.account_id = done;
                } else {
                    si.utilization = None;
                }
            }
        }
        Ok(())
    }

    /// A context over the demo world only: no discovery, its own index and vault.
    pub fn demo(painter: Painter, seats: Vec<Seat>, root: &std::path::Path) -> anyhow::Result<Ctx> {
        let base = Dirs::resolve()?;
        let dirs = Dirs {
            data: root.join("swarmy-data"),
            config: root.join("swarmy-config"),
            home: base.home,
        };
        crate::index::use_temp_dir(&dirs.data.join("tmp"))?;
        let mut seat_info = Vec::new();
        let mut caches = Vec::new();
        for s in &seats {
            if s.harness != Harness::Claude {
                let mut problems = Vec::new();
                seat_info.push(seat_info_of(s, &dirs.data, &mut problems).0);
                continue;
            }
            let cj = config_json::read_seat_file(&s.claude_json)?;
            if let Some(costs) = cj.model_costs {
                caches.push((s.id.clone(), costs));
            }
            seat_info.push(SeatInfo {
                seat: s.id.clone(),
                label: s.label.clone(),
                account: cj.account,
                switched_at: None,
                utilization: cj.utilization,
            });
        }
        let config = Config::default();
        let prices = price_book(&caches, &config, Utc::now())?;
        let mut ctx = Ctx {
            dirs,
            config,
            seats,
            seat_info,
            prices,
            procs: ProcTable::snapshot(),
            painter,
            mask: false,
            now: Utc::now(),
            login_labels: Default::default(),
            login_tiers: Default::default(),
            problems: Vec::new(),
        };
        ctx.check_unconfirmed();
        Ok(ctx)
    }

    /// The accounts of the vault's saved logins (read without the lock): organizations the index
    /// must know of even while no seat runs them (`Index::set_known_accounts`). An unreadable
    /// vault knows of none; it is reported where the vault is shown.
    pub fn saved_accounts(&self) -> Vec<AccountId> {
        match crate::vault::Vault::open(&self.dirs.vault_dir()) {
            Ok(v) => v
                .logins()
                .iter()
                .filter_map(|l| l.account_uuid.as_deref().map(AccountId::new))
                .collect(),
            Err(e) => {
                crate::diag::log("vault", &format!("saved accounts: {e:#}"));
                Vec::new()
            }
        }
    }

    /// Opens the index and reads whatever changed since the last run. The first run shows
    /// progress on stderr.
    pub fn index(&self) -> anyhow::Result<(Index, RefreshStats)> {
        let mut idx = Index::open(&self.dirs.index_file())?;
        idx.set_now(self.now);
        idx.set_known_accounts(self.saved_accounts());
        let mut resolver = Resolver::new(self.config.project.clone());
        let tty = std::io::stderr().is_terminal();
        let mut last_draw = std::time::Instant::now();
        let mut shown = false;
        let mut progress = |p: Progress| {
            if !tty || p.files_total < 50 {
                return;
            }
            if last_draw.elapsed().as_millis() < 100 && p.files_done < p.files_total {
                return;
            }
            last_draw = std::time::Instant::now();
            shown = true;
            let frac = if p.bytes_total > 0 {
                p.bytes_done as f64 / p.bytes_total as f64
            } else {
                1.0
            };
            eprint!(
                "\r\x1b[2K[ .. ] indexing {} transcripts {} {:>3.0}%  {:.0} MB",
                fmt::count(p.files_total as u64),
                fmt::gauge(frac, 24),
                frac * 100.0,
                p.bytes_done as f64 / 1e6
            );
            let _ = std::io::stderr().flush();
        };
        let stats = idx.refresh(
            &self.seats,
            &self.seat_info,
            &mut resolver,
            &self.prices,
            &mut progress,
        )?;
        if shown {
            eprintln!(
                "\r\x1b[2K[ OK ] indexed {} transcripts in {:.1}s",
                fmt::count(stats.files_read as u64),
                stats.elapsed.as_secs_f64()
            );
        }
        Ok((idx, stats))
    }

    /// Name of an account: the config label, else a saved login's label, else the seats it is
    /// logged into, else the email / org name, else a short id. Unmasked: what front ends print
    /// goes through [`Ctx::shown_account_name`].
    pub fn account_name(&self, uuid: &str) -> String {
        self.account_name_as(uuid, false)
    }

    /// [`Ctx::account_name`] as printed: `--mask` hides a login label made from its email and
    /// an email or org name.
    pub fn shown_account_name(&self, uuid: &str) -> String {
        self.account_name_as(uuid, self.mask)
    }

    fn account_name_as(&self, uuid: &str, mask: bool) -> String {
        let user = AccountId::new(uuid);
        if let Some(l) = self.config.accounts.get(uuid) {
            return l.clone();
        }
        // Backwards compatibility: a `[accounts]` entry keyed by bare `accountUuid` still applies
        // when only one known account belongs to that user.
        if user.has_org()
            && let Some(l) = self.config.accounts.get(user.user_uuid())
        {
            let matching_orgs: std::collections::BTreeSet<AccountId> = self
                .seat_info
                .iter()
                .filter_map(|s| s.account.as_ref().map(config_json::Account::id))
                .filter(|id| id.user_uuid() == user.user_uuid())
                .collect();
            if matching_orgs.len() <= 1 {
                return l.clone();
            }
        }
        // A saved login's label names its account best: seat names would mix the two ideas.
        if let Some(l) = self.login_labels.get(uuid) {
            return crate::ui::mask::login_label(&l.label, l.email.as_deref(), mask);
        }
        let seats: Vec<&str> = self
            .seat_info
            .iter()
            .filter(|s| s.account.as_ref().is_some_and(|a| a.id().as_str() == uuid))
            .map(|s| s.label.as_str())
            .collect();
        if !seats.is_empty() {
            return format!("account in {}", seats.join("+"));
        }
        if !mask
            && let Some(a) = self
                .seat_info
                .iter()
                .filter_map(|s| s.account.as_ref())
                .find(|a| a.id().as_str() == uuid)
        {
            if let Some(e) = &a.email {
                return e.clone();
            }
            if let Some(org) = &a.organization_name {
                return org.clone();
            }
        }
        format!("acct-{}", crate::core::ids::short_account(uuid))
    }

    pub fn email(&self, uuid: &str) -> Option<String> {
        if self.mask {
            return None;
        }
        self.seat_info
            .iter()
            .filter_map(|s| s.account.as_ref())
            .find(|a| a.id().as_str() == uuid)
            .and_then(|a| a.email.clone())
    }

    pub fn org_name(&self, uuid: &str) -> Option<String> {
        if self.mask {
            return None;
        }
        self.seat_info
            .iter()
            .filter_map(|s| s.account.as_ref())
            .find(|a| a.id().as_str() == uuid)
            .and_then(|a| a.organization_name.clone())
    }

    /// The freshest utilization reading of every account Claude Code has cached in a seat now
    /// (the status line's when newer).
    fn seat_readings(&self) -> Vec<(String, Vec<Reading>)> {
        let mut best: std::collections::BTreeMap<String, &config_json::Utilization> =
            std::collections::BTreeMap::new();
        for si in &self.seat_info {
            if let Some(u) = &si.utilization {
                let e = best.entry(u.account_id.to_string()).or_insert(u);
                if u.fetched_at > e.fetched_at {
                    *e = u;
                }
            }
        }
        best.into_iter()
            .map(|(k, u)| (k, u.readings.clone()))
            .collect()
    }

    /// The latest reading of every account and window kind: the newest stored in the index, or
    /// the one a seat holds now when newer. Every account ever read is there, not only those in a
    /// seat now: a login parked in the vault keeps its last known limit and reset time. Every
    /// surface (`swarmy limits`, `swarmy accounts`, `doctor`, the dashboard, the status line
    /// through `fleet`) starts from these readings, so they agree on an account.
    /// What the limit analysis takes (`limits::all_reports`): the latest readings of every
    /// account, with each one's plan tier (its seat's, else its saved login's) and name.
    pub fn limits_inputs(&self, idx: &Index) -> anyhow::Result<crate::limits::LimitsInputs> {
        let readings = self.latest_readings(idx)?;
        let mut tiers = std::collections::HashMap::new();
        let mut names = std::collections::HashMap::new();
        for (acct, _) in &readings {
            let seat_tier = self
                .seat_info
                .iter()
                .filter_map(|s| s.account.as_ref())
                .find(|a| a.id().as_str() == acct)
                .and_then(|a| a.effective_tier());
            if let Some(t) = seat_tier.or_else(|| self.login_tiers.get(acct).cloned()) {
                tiers.insert(acct.clone(), t);
            }
            names.insert(acct.clone(), self.account_name(acct));
        }
        Ok(crate::limits::LimitsInputs {
            now: self.now,
            readings,
            tiers,
            names,
        })
    }

    pub fn latest_readings(&self, idx: &Index) -> anyhow::Result<Vec<(String, Vec<Reading>)>> {
        let mut merged: std::collections::BTreeMap<String, Vec<Reading>> =
            idx.latest_readings()?.into_iter().collect();
        for (acct, rs) in self.seat_readings() {
            let e = merged.entry(acct).or_default();
            for r in rs {
                match e.iter_mut().find(|x| x.kind == r.kind) {
                    Some(x) if x.fetched_at < r.fetched_at => *x = r,
                    Some(_) => {}
                    None => e.push(r),
                }
            }
        }
        Ok(merged.into_iter().collect())
    }

    /// The weekly window in force (`core::limits::window_of`: a reset by hand moves its start) of
    /// the given account, else of the most recently read one, for `--period week`: the same
    /// window `swarmy limits` and the dashboard show.
    pub fn limit_week(
        &self,
        idx: &Index,
        account: Option<&AccountId>,
    ) -> anyhow::Result<Option<Span>> {
        let readings = self.latest_readings(idx)?;
        let pick = match account {
            Some(a) => readings.iter().find(|(u, _)| u == a.as_str()),
            None => readings
                .iter()
                .max_by_key(|(_, r)| r.iter().map(|x| x.fetched_at).max()),
        };
        let Some((acct, r)) = pick.and_then(|(acct, rs)| {
            rs.iter()
                .find(|r| r.kind == WindowKind::SevenDay)
                .map(|r| (acct, r))
        }) else {
            return Ok(None);
        };
        let history = idx.readings(acct, WindowKind::SevenDay)?;
        let w = crate::core::limits::window_of(r, &history, self.now).window;
        Ok(Some(Span {
            start: w.start,
            end: w.end.min(self.now + chrono::Duration::seconds(1)),
        }))
    }

    /// The account a user named (`--account`): an account uuid, an email, a saved login's label
    /// or a name from `[accounts]` of config.toml, resolved once so every period and filter
    /// treats it the same (review H7). No match, or several accounts behind one name, is an
    /// error that lists what is known.
    pub fn resolve_account(&self, idx: &Index, input: &str) -> anyhow::Result<AccountId> {
        let key = input.trim();
        let mut found: std::collections::BTreeSet<AccountId> =
            idx.accounts_matching(key)?.into_iter().collect();
        for a in self.seat_info.iter().filter_map(|s| s.account.as_ref()) {
            let id = a.id();
            let email = a
                .email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(key));
            let org_name = a
                .organization_name
                .as_deref()
                .is_some_and(|o| o.eq_ignore_ascii_case(key));
            if id.as_str() == key
                || a.account_uuid == key
                || a.organization_uuid.as_deref() == Some(key)
                || email
                || org_name
            {
                found.insert(id);
            }
        }
        for (uuid, name) in &self.config.accounts {
            if name == key || uuid == key {
                let cfg_id = AccountId::new(uuid.clone());
                if cfg_id.has_org() {
                    found.insert(cfg_id);
                } else {
                    // A config entry keyed by bare `accountUuid`: map to any matching org-scoped
                    // account when present, else keep the bare id.
                    // Seats first, then the vault's saved logins (an account in no seat now).
                    let mut scoped: Vec<AccountId> = self
                        .seat_info
                        .iter()
                        .filter_map(|s| s.account.as_ref().map(config_json::Account::id))
                        .chain(self.saved_accounts())
                        .filter(|id| id.has_org() && id.user_uuid() == uuid)
                        .collect();
                    scoped.sort();
                    scoped.dedup();
                    if scoped.is_empty() {
                        found.insert(cfg_id);
                    } else {
                        found.extend(scoped);
                    }
                }
            }
        }
        // Saved logins: their labels name accounts too.
        match crate::vault::Vault::open(&self.dirs.vault_dir()) {
            Ok(v) => {
                for l in v.logins() {
                    let email = l
                        .email
                        .as_deref()
                        .is_some_and(|e| e.eq_ignore_ascii_case(key));
                    if let Some(a) = &l.account_uuid {
                        let id = AccountId::new(a.clone());
                        if l.label == key
                            || email
                            || id.as_str() == key
                            || id.user_uuid() == key
                            || id.org_uuid() == Some(key)
                        {
                            found.insert(id);
                        }
                    }
                }
            }
            Err(e) => crate::diag::log(
                "accounts",
                &format!("cannot read the vault, login labels are not matched: {e:#}"),
            ),
        }
        let name = |a: &AccountId| format!("{} ({a})", self.shown_account_name(a.as_str()));
        let mut it = found.iter();
        match (it.next(), it.next()) {
            (Some(one), None) => Ok(one.clone()),
            (None, _) => Err(LookupError::NoAccount(key.to_string()).into()),
            (Some(_), Some(_)) => Err(LookupError::AmbiguousAccount {
                key: key.to_string(),
                candidates: found.iter().map(name).collect(),
            }
            .into()),
        }
    }

    /// The seat a user named: by its label, else by its id. An unknown name is an error that
    /// lists the seats.
    pub fn resolve_seat(&self, name: &str) -> anyhow::Result<&Seat> {
        // A label, else an id, else the dir (as written or canonical): one rule for every
        // command (review A-M3: `accounts` had its own, without ids and without an error code).
        let canonical = crate::fsutil::canonical(Path::new(name));
        self.seats
            .iter()
            .find(|s| s.label == name)
            .or_else(|| self.seats.iter().find(|s| s.id.as_str() == name))
            .or_else(|| {
                self.seats.iter().find(|s| {
                    s.dir.to_string_lossy() == name || canonical.as_ref().is_ok_and(|c| *c == s.dir)
                })
            })
            .ok_or_else(|| {
                LookupError::NoSeat {
                    name: name.to_string(),
                    known: self.seats.iter().map(|s| s.label.clone()).collect(),
                }
                .into()
            })
    }

    /// The label of a seat id: the seat's current label, else the id itself (a seat that is
    /// not discovered in this run).
    pub fn seat_label(&self, id: &SeatId) -> String {
        self.seats
            .iter()
            .find(|s| &s.id == id)
            .map_or_else(|| id.to_string(), |s| s.label.clone())
    }

    /// `/home/user/x` → `~/x`, and just the last component when masking.
    pub fn show_path(&self, p: &str) -> String {
        let home = self.dirs.home.to_string_lossy();
        if self.mask {
            return std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        match p.strip_prefix(home.as_ref()) {
            Some(rest) => format!("~{rest}"),
            None => p.to_string(),
        }
    }
}

/// The prices in force: the built-in table, Claude Code's cached prices of every seat (in seat
/// order), then the user's `[[price]]` overrides. The CLI and the dashboard build it the same way,
/// so they agree on the fingerprint and never reprice the index back and forth.
pub fn price_book(
    caches: &[(SeatId, serde_json::Value)],
    config: &Config,
    now: DateTime<Utc>,
) -> anyhow::Result<PriceBook> {
    let mut prices = PriceBook::builtin();
    for (seat, costs) in caches {
        prices.merge_claude_code_cache(seat, costs);
    }
    config.apply_prices(&mut prices, now.with_timezone(&chrono::Local).date_naive())?;
    Ok(prices)
}

/// What is wrong with the seats themselves, the same at startup and on every reload: a
/// `[[seat]]` whose files fit several harnesses, and a seat whose transcript retention cannot be
/// read (the raw window is then checked against Claude Code's default).
fn seat_problems(config: &Config, home: &Path, seats: &[Seat], problems: &mut Vec<String>) {
    for (path, label, explicit) in config.seat_entries(home) {
        if explicit.is_some() {
            continue;
        }
        if let Err(found) = crate::config::detect_harness(&path) {
            let names: Vec<&str> = found.iter().map(|h| h.key()).collect();
            problems.push(format!(
                "[[seat]] {}: its files fit {}; set `harness = \"…\"` (read as claude until then)",
                label.unwrap_or_else(|| path.display().to_string()),
                names.join(" and "),
            ));
        }
    }
    for s in seats {
        if let Err(e) = s.cleanup_period_days() {
            problems.push(format!("seat {}: {e}", s.label));
        }
    }
}

/// What a seat says about itself: its account from `.claude.json`, and its limit readings — the
/// status line's when newer than Claude Code's cached ones and of the same account (see
/// `statusline::reading`). A file that cannot be read adds to `problems` (the seat then shows no
/// account and no reading, and says why).
pub fn seat_info_of(
    s: &Seat,
    data_dir: &std::path::Path,
    problems: &mut Vec<String>,
) -> (SeatInfo, Option<serde_json::Value>) {
    let other_harness = match s.harness {
        Harness::Agy => Some(crate::agy::account::read_account(&s.dir)),
        Harness::Codex => Some(crate::codex::account::read_account(&s.dir)),
        Harness::Claude => None,
    };
    if let Some(read) = other_harness {
        let account = match read {
            Ok(a) => a,
            Err(e) => {
                let msg = format!("seat {}: {e}", s.label);
                crate::diag::log("seats", &msg);
                problems.push(msg);
                None
            }
        };
        return (
            SeatInfo {
                seat: s.id.clone(),
                label: s.label.clone(),
                switched_at: None,
                account,
                utilization: None,
            },
            None,
        );
    }
    let mut cj = match config_json::read_seat_file(&s.claude_json) {
        Ok(cj) => cj,
        Err(e) => {
            let msg = format!("seat {}: {e:#}", s.label);
            crate::diag::log("seats", &msg);
            problems.push(msg);
            config_json::ClaudeJson::default()
        }
    };
    // Best-effort non-secret metadata from `.credentials.json` (subscriptionType / rateLimitTier
    // scoped to this seat's OAuth grant). Never touches macOS Keychain; missing files are ignored.
    if let Some(a) = cj.account.as_mut()
        && let Ok(text) = std::fs::read_to_string(s.credentials_file())
        && let Ok(Some(secret)) = crate::claude::credentials::parse_login(&text)
    {
        a.apply_credentials(&secret.meta());
    }
    let live =
        crate::statusline::reading::read(&crate::statusline::reading::file_of(data_dir, &s.id));
    // A status-line reading counts only for the account it names (review H2).
    let utilization = match cj.account.as_ref() {
        Some(a) => crate::statusline::reading::merge(cj.utilization, &a.id(), live),
        None => cj.utilization,
    };
    (
        SeatInfo {
            seat: s.id.clone(),
            label: s.label.clone(),
            account: cj.account,
            switched_at: None,
            utilization,
        },
        cj.model_costs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review B-H4: a used Claude Code dir has `sessions/` and `history.jsonl`, which Codex
    /// writes too; the README's `[[seat]]` (no `harness`) must stay a Claude Code seat.
    #[test]
    fn a_configured_seat_without_harness_is_judged_by_its_own_files() {
        let t = tempfile::tempdir().unwrap();
        let cc = t.path().join("agents/cc-work");
        for d in ["sessions", "projects"] {
            std::fs::create_dir_all(cc.join(d)).unwrap();
        }
        std::fs::write(cc.join("history.jsonl"), "").unwrap();
        assert_eq!(crate::config::detect_harness(&cc), Ok(Harness::Claude));
        let both = t.path().join("both");
        std::fs::create_dir_all(both.join("projects")).unwrap();
        std::fs::write(both.join("auth.json"), "{}").unwrap();
        let config: Config = toml::from_str(&format!(
            "[[seat]]\npath = {:?}\nlabel = \"work\"\n\n[[seat]]\npath = {:?}\nlabel = \"odd\"\n\n\
             [[seat]]\npath = {:?}\nharness = \"codex\"\n",
            cc.display().to_string(),
            both.display().to_string(),
            both.display().to_string(),
        ))
        .unwrap();
        let mut problems = Vec::new();
        seat_problems(&config, t.path(), &[], &mut problems);
        // Only the ambiguous dir without `harness` is a problem; saying it settles it.
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("[[seat]] odd:") && problems[0].contains("claude and codex")
        );
    }

    /// First review H5: a damaged `.claude.json` is a problem that is reported, never a seat
    /// without an account; a seat that has none yet is no problem.
    #[test]
    fn a_damaged_seat_file_is_reported_not_taken_for_empty() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join(".claude-work");
        std::fs::create_dir_all(&dir).unwrap();
        let seat = Seat::new("work", dir.clone(), dir.join(".claude.json"), false);
        let mut problems = Vec::new();
        let (info, _) = seat_info_of(&seat, t.path(), &mut problems);
        assert!(info.account.is_none());
        assert!(problems.is_empty(), "no file yet: {problems:?}");
        std::fs::write(&seat.claude_json, "{\"oauthAccount\": {\"accountUuid\": ").unwrap();
        let (info, _) = seat_info_of(&seat, t.path(), &mut problems);
        assert!(info.account.is_none());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("seat work:") && problems[0].contains("damaged"));
        // The vault refuses to read the seat rather than save a login without its account.
        assert!(crate::vault::read_seat(&seat).is_err());
    }

    /// Review H2: the status line wrote acc-a's 80% at 09:00; the seat was switched to acc-b at
    /// 12:00 and Claude Code's cached reading (acc-b's) is from before. The seat's reading stays
    /// acc-b's own: acc-a's 80% is not relabelled, so neither `readings` nor the login history
    /// get a wrong fact from it.
    #[test]
    fn a_status_line_reading_of_the_previous_account_is_not_relabelled() {
        use crate::statusline::reading::{LiveLimits, VERSION, file_of, write};
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join(".claude-work");
        std::fs::create_dir_all(&dir).unwrap();
        let seat = Seat::new("work", dir.clone(), dir.join(".claude.json"), false);
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let cj = serde_json::json!({
            "oauthAccount": {"accountUuid": "acc-b"},
            "cachedUsageUtilization": {"accountUuid": "acc-b", "fetchedAtMs": at("2026-11-22T08:00:00Z").timestamp_millis(),
                "utilization": {"seven_day": {"utilization": 10.0, "resets_at": "2026-11-25T00:00:00Z"}}}
        });
        std::fs::write(&seat.claude_json, cj.to_string()).unwrap();
        let reading = |account: &str| LiveLimits {
            v: VERSION,
            at: at("2026-11-22T09:00:00Z"),
            account: AccountId::new(account),
            config_dir: dir.to_string_lossy().into_owned(),
            session_id: None,
            five_hour: None,
            seven_day: Some((80.0, at("2026-11-25T00:00:00Z"))),
        };
        write(&file_of(t.path(), &seat.id), &reading("acc-a")).unwrap();
        let mut problems = Vec::new();
        let (info, _) = seat_info_of(&seat, t.path(), &mut problems);
        let u = info.utilization.unwrap();
        assert_eq!(u.account_id.as_str(), "acc-b");
        assert_eq!(u.readings[0].utilization, 10.0, "acc-b's own reading");
        // The same reading under acc-b's name is acc-b's, and newer: it wins.
        write(&file_of(t.path(), &seat.id), &reading("acc-b")).unwrap();
        let (info, _) = seat_info_of(&seat, t.path(), &mut problems);
        assert_eq!(info.utilization.unwrap().readings[0].utilization, 80.0);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn credentials_enrich_plan_when_present_without_requiring_file() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join(".claude-team");
        std::fs::create_dir_all(&dir).unwrap();
        let seat = Seat::new("team", dir.clone(), dir.join(".claude.json"), false);
        std::fs::write(
            &seat.claude_json,
            serde_json::json!({
                "oauthAccount": {
                    "accountUuid": "acc-1",
                    "organizationUuid": "org-team",
                    "userRateLimitTier": "default_claude_max_5x"
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            seat.credentials_file(),
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "a",
                    "refreshToken": "r",
                    "subscriptionType": "team",
                    "rateLimitTier": "default_claude_max_5x"
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut problems = Vec::new();
        let (info, _) = seat_info_of(&seat, t.path(), &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
        let acct = info.account.unwrap();
        assert_eq!(acct.id().as_str(), "acc-1/org-team");
        assert_eq!(
            acct.effective_tier().as_deref(),
            Some("team:default_claude_max_5x")
        );
        assert_eq!(acct.plan_label().as_deref(), Some("team (max_5x)"));
    }
}

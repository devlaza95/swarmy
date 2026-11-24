//! `config.toml` and the tool's own directories.
//!
//! Everything is optional: with no config file the tool discovers seats on its own.

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::core::harness::Harness;
use crate::core::pricing::{PriceBook, RateOverrides, Rates};
use crate::errors::ConfigError;
use crate::model::project::Alias;

#[derive(Debug, Clone)]
pub struct Dirs {
    /// Index database, vault metadata, backups.
    pub data: PathBuf,
    /// `config.toml`.
    pub config: PathBuf,
    pub home: PathBuf,
}

impl Dirs {
    pub fn resolve() -> anyhow::Result<Dirs> {
        let base = directories::BaseDirs::new().context("cannot determine the home directory")?;
        let home = base.home_dir().to_path_buf();
        let pd = directories::ProjectDirs::from("", "", "swarmy")
            .context("cannot determine data directories")?;
        let data = std::env::var_os("SWARMY_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| pd.data_dir().to_path_buf());
        let config = std::env::var_os("SWARMY_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| pd.config_dir().to_path_buf());
        // The tool was called agentmana before: move its data and config (index, vault of saved
        // logins, config.toml) over once. Never while the old program runs (it would recreate
        // and write its dirs), and never over anything already in the new place.
        if let Some(old) = directories::ProjectDirs::from("", "", "agentmana")
            && (old.data_dir().exists() || old.config_dir().exists())
            && !old_program_running()
        {
            for (from, to) in [(old.data_dir(), &data), (old.config_dir(), &config)] {
                migrate_dir(from, to);
            }
        }
        Ok(Dirs { data, config, home })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn index_file(&self) -> PathBuf {
        self.data.join("index.sqlite")
    }

    pub fn vault_dir(&self) -> PathBuf {
        self.data.join("vault")
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Extra config dirs: `[[seat]] path = "~/claude-work"  label = "work"`.
    pub seat: Vec<SeatConf>,
    /// Project aliases: `[[project]] match = "/old/path"  name = "shop"`.
    pub project: Vec<Alias>,
    /// Price overrides: `[[price]] model = "claude-opus-5-5"  from = "2026-12-01"  input = 4 …`.
    pub price: Vec<PriceConf>,
    /// Display names for accounts: `[accounts] "<account uuid>" = "work"`.
    pub accounts: std::collections::BTreeMap<String, String>,
    /// The Claude Code status line (`swarmy statusline`).
    pub statusline: StatuslineConf,
    /// Monthly subscription price per account label or uuid, for the payback figure.
    pub subscription: std::collections::BTreeMap<String, f64>,
    /// Minutes without transcript activity after which an unfinished agent counts as stalled.
    pub stall_minutes: Option<u64>,
    /// The dashboard's colours; a name not in [`ThemeName`] is a config error (review L3).
    pub theme: Option<ThemeName>,
    /// How long per-request detail is kept, and the size the index may grow to.
    pub storage: StorageConf,
    /// Harnesses to skip during auto-discovery (e.g. `disabled_harnesses = ["agy"]`).
    pub disabled_harnesses: Vec<Harness>,
}

/// The dashboard's colour themes, by the name `config.toml` and `:theme` use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    #[default]
    Ops,
    Phosphor,
    Amber,
    Plain,
}

impl ThemeName {
    pub const ALL: [ThemeName; 4] = [
        ThemeName::Ops,
        ThemeName::Phosphor,
        ThemeName::Amber,
        ThemeName::Plain,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeName::Ops => "ops",
            ThemeName::Phosphor => "phosphor",
            ThemeName::Amber => "amber",
            ThemeName::Plain => "plain",
        }
    }

    /// The theme after this one, for the key that cycles them.
    pub fn next(self) -> ThemeName {
        let i = Self::ALL
            .iter()
            .position(|t| *t == self)
            .map_or(0, |i| i + 1);
        Self::ALL[i % Self::ALL.len()]
    }
}

impl std::str::FromStr for ThemeName {
    type Err = String;

    fn from_str(s: &str) -> Result<ThemeName, String> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|t| t.as_str()).collect();
                format!("unknown theme '{s}' (themes: {})", names.join(", "))
            })
    }
}

/// `[storage]` in config.toml (`docs/design/storage.md` §5).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConf {
    /// Days of per-request and per-tool-call detail; older detail is kept as quarter-hour
    /// rollups for good. 8 ..= the shortest transcript retention of the seats − 2
    /// (`core::storage::keep_raw_days`); unset: 14, less when a seat keeps transcripts shorter.
    pub keep_raw_days: Option<i64>,
    /// `doctor` and SYSTEM warn when the 12-month projection goes over this (MB); nothing is
    /// ever deleted because of it.
    pub budget_mb: u64,
    /// The dashboard compacts on its own; `swarmy compact` works either way.
    pub auto_compact: bool,
}

impl Default for StorageConf {
    fn default() -> Self {
        StorageConf {
            keep_raw_days: None,
            budget_mb: crate::core::storage::DEFAULT_BUDGET_MB,
            auto_compact: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeatConf {
    pub path: String,
    pub label: Option<String>,
    /// Optional harness (`claude`, `agy`, `codex`); auto-detected from the directory shape when omitted.
    #[serde(default)]
    pub harness: Option<Harness>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PriceConf {
    /// Exact id or a prefix ending in `*`.
    pub model: String,
    pub from: Option<NaiveDate>,
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_read: Option<f64>,
    pub web_search: Option<f64>,
    pub fast_multiplier: Option<f64>,
}

impl Config {
    pub fn load(dirs: &Dirs) -> anyhow::Result<Config> {
        let p = dirs.config_file();
        let invalid = |message: String| ConfigError::Invalid {
            path: p.clone(),
            message,
        };
        match std::fs::read_to_string(&p) {
            Ok(text) => {
                let c: Config = toml::from_str(&text).map_err(|e| invalid(e.to_string()))?;
                c.validate().map_err(invalid)?;
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(source) => Err(ConfigError::Unreadable { path: p, source }.into()),
        }
    }

    pub fn stall_minutes(&self) -> u64 {
        self.stall_minutes.unwrap_or(20)
    }

    pub fn seat_paths(&self, home: &Path) -> Vec<(PathBuf, Option<String>)> {
        self.seat
            .iter()
            .filter(|s| s.harness.is_none_or(|h| h == Harness::Claude))
            .map(|s| (expand_home(&s.path, home), s.label.clone()))
            .collect()
    }

    /// Configured seats with their explicit harness (when set).
    pub fn seat_entries(&self, home: &Path) -> Vec<(PathBuf, Option<String>, Option<Harness>)> {
        self.seat
            .iter()
            .map(|s| (expand_home(&s.path, home), s.label.clone(), s.harness))
            .collect()
    }

    /// The `[[seat]]` paths and labels of harness `h`: those that say so, and those without
    /// `harness` whose files are `h`'s ([`detect_harness`]; a dir whose files fit several is read
    /// as Claude Code's). Every front end finds configured seats through this (reviews A-M3,
    /// B-M19: the status line and `jump` had their own scans, which missed them).
    pub fn seats_of(&self, home: &Path, h: Harness) -> Vec<(PathBuf, Option<String>)> {
        self.seat_entries(home)
            .into_iter()
            .filter(|(path, _, explicit)| {
                explicit.unwrap_or_else(|| detect_harness(path).unwrap_or(Harness::Claude)) == h
            })
            .map(|(path, label, _)| (path, label))
            .collect()
    }

    pub fn harness_enabled(&self, h: Harness) -> bool {
        !self.disabled_harnesses.contains(&h)
    }

    /// Rules TOML cannot express: prices are finite and not negative; a subscription costs a
    /// finite amount above zero (review A-M5: a negative or NaN price gave a payback figure).
    pub fn validate(&self) -> Result<(), String> {
        for (account, usd) in &self.subscription {
            if !(usd.is_finite() && *usd > 0.0) {
                return Err(format!(
                    "[subscription] {account}: a monthly price above 0 (got {usd})"
                ));
            }
        }
        for p in &self.price {
            let rates = Rates::derive(p.input, p.output, &p.overrides(), None);
            rates
                .validate()
                .map_err(|e| format!("[[price]] {}: {e}", p.model))?;
            // USD per request, unlike the rest (per million tokens): a price per 1000 searches
            // would price each one 1000 times too high (review B-L11).
            if rates.web_search > crate::core::pricing::MAX_WEB_SEARCH_USD {
                return Err(format!(
                    "[[price]] {}: web_search is USD per request (got {}; $10 per 1000 searches is 0.01)",
                    p.model, rates.web_search
                ));
            }
        }
        Ok(())
    }

    /// Adds the `[[price]]` overrides to `book`; what an override leaves out is derived from the
    /// entry it overrides (`PriceBook::push_override`).
    pub fn apply_prices(&self, book: &mut PriceBook, today: NaiveDate) -> anyhow::Result<()> {
        for p in &self.price {
            book.push_override(&p.model, p.from, p.input, p.output, &p.overrides(), today)
                .map_err(|e| anyhow::anyhow!("[[price]] {}: {e}", p.model))?;
        }
        Ok(())
    }
}

impl PriceConf {
    fn overrides(&self) -> RateOverrides {
        RateOverrides {
            cache_write_5m: self.cache_write_5m,
            cache_write_1h: self.cache_write_1h,
            cache_read: self.cache_read,
            web_search: self.web_search,
            fast_multiplier: self.fast_multiplier,
        }
    }
}

pub fn expand_home(p: &str, home: &Path) -> PathBuf {
    if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// The harness of a config dir, by the files in it (`Harness::detect`); `Err` names the
/// harnesses whose files are all there.
pub fn detect_harness(dir: &Path) -> Result<Harness, Vec<Harness>> {
    Harness::detect(|f| dir.join(f).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review A-M5: a typo in `segments` or a nonsense subscription price fails the config,
    /// instead of a segment silently missing or a payback from a negative price.
    #[test]
    fn segments_and_subscriptions_are_checked() {
        assert!(toml::from_str::<Config>("[statusline]\nsegments = [\"wek\"]\n").is_err());
        let ok: Config =
            toml::from_str("[statusline]\nsegments = [\"week\", \"five_hour\"]\n").unwrap();
        assert_eq!(
            ok.statusline.segments,
            [StatusSegment::Week, StatusSegment::FiveHour]
        );
        let bad: Config = toml::from_str("[subscription]\nwork = -20\n").unwrap();
        assert!(bad.validate().is_err());
        let good: Config = toml::from_str("[subscription]\nwork = 200\n").unwrap();
        assert!(good.validate().is_ok());
    }

    #[test]
    fn a_web_search_price_per_thousand_is_refused() {
        let c: Config = toml::from_str(
            "[[price]]\nmodel = \"claude-opus-5-5\"\ninput = 4\noutput = 20\nweb_search = 10\n",
        )
        .unwrap();
        let e = c.validate().unwrap_err();
        assert!(e.contains("USD per request"), "{e}");
        let ok: Config = toml::from_str(
            "[[price]]\nmodel = \"claude-opus-5-5\"\ninput = 4\noutput = 20\nweb_search = 0.01\n",
        )
        .unwrap();
        assert!(ok.validate().is_ok());
    }

    /// First review L3: a misspelt theme is an error naming the themes, not a silent default.
    #[test]
    fn themes_are_checked() {
        let c: Config = toml::from_str("theme = \"amber\"").unwrap();
        assert_eq!(c.theme, Some(ThemeName::Amber));
        let e = toml::from_str::<Config>("theme = \"ambr\"")
            .unwrap_err()
            .to_string();
        assert!(e.contains("ambr") && e.contains("phosphor"), "{e}");
        assert_eq!("plain".parse::<ThemeName>(), Ok(ThemeName::Plain));
        assert!(
            "Ops"
                .parse::<ThemeName>()
                .unwrap_err()
                .contains("ops, phosphor, amber, plain")
        );
        let mut t = ThemeName::default();
        for _ in 0..ThemeName::ALL.len() {
            t = t.next();
        }
        assert_eq!(t, ThemeName::Ops, "the cycle visits every theme");
        // Loaded from a file, the error names the file and has a code.
        let d = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            data: d.path().join("data"),
            config: d.path().to_path_buf(),
            home: d.path().to_path_buf(),
        };
        std::fs::write(dirs.config_file(), "theme = \"ambr\"\n").unwrap();
        let e = Config::load(&dirs).unwrap_err();
        assert_eq!(crate::errors::code_of(&e), Some("config-invalid"));
    }

    #[test]
    fn parses_a_full_config() {
        let c: Config = toml::from_str(
            r#"
            stall_minutes = 30
            [[seat]]
            path = "~/cc-work"
            label = "work"
            [[project]]
            match = "/home/user/work/old"
            name = "shop"
            [[price]]
            model = "claude-opus-5-5"
            from = "2026-12-01"
            input = 3
            output = 15
            [accounts]
            "acc-1" = "personal"
            [subscription]
            personal = 200
            [storage]
            keep_raw_days = 21
            auto_compact = false
            "#,
        )
        .unwrap();
        assert_eq!(c.stall_minutes(), 30);
        assert_eq!(c.storage.keep_raw_days, Some(21));
        assert!(!c.storage.auto_compact);
        assert_eq!(
            c.storage.budget_mb, 1024,
            "what is left out keeps its default"
        );
        assert!(Config::default().storage.auto_compact);
        assert_eq!(
            c.seat_paths(Path::new("/h"))[0].0,
            PathBuf::from("/h/cc-work")
        );
        let mut b = PriceBook::default();
        c.apply_prices(&mut b, NaiveDate::from_ymd_opt(2026, 11, 22).unwrap())
            .unwrap();
        assert_eq!(b.entries()[0].rates.cache_write_1h, 6.0);
        let bad: Config =
            toml::from_str("[[price]]\nmodel = \"claude-opus-5-5\"\ninput = -3\noutput = nan\n")
                .unwrap();
        let e = bad.validate().unwrap_err();
        assert!(e.contains("claude-opus-5-5") && e.contains("input"), "{e}");
        assert!(
            toml::from_str::<Config>("bogus = 1").is_err(),
            "typos are reported"
        );
    }
}

fn old_program_running() -> bool {
    let Ok(mut sys) = sysinfo::System::new() else {
        // Cannot tell: assume it runs, and migrate another time.
        return true;
    };
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.processes()
        .values()
        .any(|p| p.name().to_string_lossy().starts_with("agentmana"))
}

/// Moves `from` into `to` entry by entry; entries that already exist in `to` stay where they are.
fn migrate_dir(from: &std::path::Path, to: &std::path::Path) {
    if !from.is_dir() || from == to {
        return;
    }
    let _ = std::fs::create_dir_all(to);
    if let Ok(entries) = std::fs::read_dir(from) {
        for e in entries.flatten() {
            let dest = to.join(e.file_name());
            if dest.exists() {
                if e.path().is_dir() && dest.is_dir() {
                    migrate_dir(&e.path(), &dest);
                }
                continue;
            }
            let _ = std::fs::rename(e.path(), &dest);
        }
    }
    // Remove the old dir only when everything moved.
    let _ = std::fs::remove_dir(from);
}

/// A piece of the status line (`[statusline] segments`). A name it does not know fails
/// `config.toml` when it is read, instead of a segment silently missing (review A-M5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSegment {
    Attention,
    Agents,
    Week,
    FiveHour,
    Context,
    Cost,
    Burn,
}

/// `[statusline]` in config.toml.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslineConf {
    /// Segments in display order: attention, agents, week, five_hour, context, cost, burn.
    pub segments: Vec<StatusSegment>,
    /// 1, or 2 for a second line with the model, effort, branch and session name.
    pub lines: u8,
    /// Seconds between refreshes of an idle session's line (`swarmy setup` writes it into
    /// Claude Code's `refreshInterval`); 0 = only on Claude Code's own events.
    pub refresh_seconds: u32,
    /// The dashboard's summary counts while it is younger than this.
    pub fresh_seconds: u32,
    /// Columns left free on the right for Claude Code's own notifications.
    pub reserve: u16,
    /// Hide session and project names (also `SWARMY_MASK=1`).
    pub mask: bool,
}

impl Default for StatuslineConf {
    fn default() -> Self {
        StatuslineConf {
            segments: vec![
                StatusSegment::Attention,
                StatusSegment::Agents,
                StatusSegment::Week,
                StatusSegment::FiveHour,
                StatusSegment::Context,
                StatusSegment::Cost,
                StatusSegment::Burn,
            ],
            lines: 1,
            refresh_seconds: 10,
            fresh_seconds: 15,
            reserve: 0,
            mask: false,
        }
    }
}

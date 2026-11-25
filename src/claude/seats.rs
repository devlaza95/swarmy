//! Discovery of Claude Code config dirs ("seats").
//!
//! Claude Code keeps everything in `~/.claude` unless `CLAUDE_CONFIG_DIR` points elsewhere, which
//! people do with shell aliases to run several accounts side by side. A seat is found by any of:
//! the default dir, `~/.claude-*` siblings that look like config dirs, `CLAUDE_CONFIG_DIR` of this
//! process or of a running `claude`, and the `seats` list of `config.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use crate::core::harness::Harness;
pub use crate::core::ids::SeatId;
pub use crate::model::seat::{Seat, seat_id};

/// Where the `.claude.json` of config dir `dir` lives: `~/.claude.json` for the default dir
/// (`~/.claude`), `<dir>/.claude.json` for any other (the rule [`discover`] follows).
pub fn claude_json_of(dir: &Path, home: &Path) -> PathBuf {
    let canon = |p: &Path| crate::fsutil::canonical(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(dir) == canon(&home.join(".claude")) {
        home.join(".claude.json")
    } else {
        dir.join(".claude.json")
    }
}

/// A config dir is recognised by the files Claude Code creates in it.
fn looks_like_config_dir(p: &Path) -> bool {
    p.is_dir()
        && [
            "projects",
            "sessions",
            ".credentials.json",
            "settings.json",
            "history.jsonl",
        ]
        .iter()
        .any(|f| p.join(f).exists())
}

pub fn label_for(dir: &Path, home: &Path) -> String {
    if dir == home.join(".claude") {
        return "default".into();
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    for prefix in [".claude-", ".claude_", "claude-", "claude_"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return rest.to_string();
        }
    }
    name.trim_start_matches('.').to_string()
}

pub struct Candidates<'a> {
    pub home: &'a Path,
    /// `CLAUDE_CONFIG_DIR` values seen in this process and in running `claude` processes.
    pub env_dirs: Vec<PathBuf>,
    /// From `config.toml`, with optional labels.
    pub configured: Vec<(PathBuf, Option<String>)>,
}

pub fn discover(c: &Candidates) -> Vec<Seat> {
    let mut found: BTreeMap<PathBuf, (Vec<&'static str>, Option<String>)> = BTreeMap::new();
    let mut add = |p: PathBuf, by: &'static str, label: Option<String>| {
        let key = crate::fsutil::canonical(&p).unwrap_or(p);
        let e = found.entry(key).or_default();
        if !e.0.contains(&by) {
            e.0.push(by);
        }
        if label.is_some() {
            e.1 = label;
        }
    };

    let default = c.home.join(".claude");
    if looks_like_config_dir(&default) {
        add(default.clone(), "default", None);
    }
    if let Ok(rd) = std::fs::read_dir(c.home) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if (name.starts_with(".claude-") || name.starts_with(".claude_"))
                && looks_like_config_dir(&e.path())
            {
                add(e.path(), "home scan", None);
            }
        }
    }
    for d in &c.env_dirs {
        if looks_like_config_dir(d) {
            add(d.clone(), "CLAUDE_CONFIG_DIR", None);
        }
    }
    for (d, label) in &c.configured {
        if d.is_dir() {
            add(d.clone(), "config.toml", label.clone());
        }
    }

    let canon_default = crate::fsutil::canonical(&default).unwrap_or(default);
    let mut seats: Vec<Seat> = found
        .into_iter()
        .map(|(dir, (found_by, label))| {
            let is_default = dir == canon_default;
            let claude_json = if is_default {
                c.home.join(".claude.json")
            } else {
                dir.join(".claude.json")
            };
            let label = label.unwrap_or_else(|| {
                if is_default {
                    "default".into()
                } else {
                    label_for(&dir, c.home)
                }
            });
            Seat {
                // `dir` is canonical already (the key of `found`).
                id: SeatId::of_canonical_dir(&dir),
                label,
                dir,
                claude_json,
                is_default,
                found_by,
                harness: Harness::Claude,
            }
        })
        .collect();
    // Default first, then by label; labels made unique (for display: ids never change).
    seats.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.label.cmp(&b.label)));
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for s in &mut seats {
        let n = seen.entry(s.label.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            s.label = format!("{}-{}", s.label, n);
        }
    }
    seats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_default_siblings_env_and_config() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        for d in [".claude", ".claude-work", ".claude-empty", "custom/cc"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
        std::fs::create_dir_all(home.join(".claude-work/sessions")).unwrap();
        std::fs::write(home.join("custom/cc/settings.json"), "{}").unwrap();
        let seats = discover(&Candidates {
            home,
            env_dirs: vec![home.join("custom/cc")],
            configured: vec![(home.join(".claude-work"), Some("job".into()))],
        });
        let labels: Vec<_> = seats.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["default", "cc", "job"], "empty dirs are not seats");
        assert_eq!(seats[0].claude_json, home.join(".claude.json"));
        assert!(seats[2].claude_json.ends_with(".claude-work/.claude.json"));
        assert_eq!(seats[2].found_by, ["home scan", "config.toml"]);
        // `[[seat]]` makes a seat asked for: never hidden as empty (issue #8).
        assert!(seats[2].configured() && !seats[0].configured());
        // Transcript retention: `cleanupPeriodDays`, else Claude Code's default.
        assert_eq!(
            seats[1].cleanup_period_days().unwrap(),
            None,
            "settings.json without it"
        );
        assert_eq!(
            seats[0].cleanup_period_days().unwrap(),
            None,
            "no settings.json"
        );
        std::fs::write(
            home.join("custom/cc/settings.json"),
            r#"{"cleanupPeriodDays": 45}"#,
        )
        .unwrap();
        assert_eq!(seats[1].cleanup_period_days().unwrap(), Some(45));
        std::fs::write(
            home.join("custom/cc/settings.json"),
            r#"{"cleanupPeriodDays": 0}"#,
        )
        .unwrap();
        assert_eq!(
            seats[1].cleanup_period_days().unwrap(),
            None,
            "below Claude Code's minimum"
        );
        // A damaged file is no "not set": the retention is unknown, and said so (coded).
        std::fs::write(
            home.join("custom/cc/settings.json"),
            r#"{"cleanupPeriodDays": 7"#,
        )
        .unwrap();
        let e = anyhow::Error::from(seats[1].cleanup_period_days().unwrap_err());
        assert_eq!(crate::errors::code_of(&e), Some("config-seat-settings"));
    }

    /// Review H3: a seat's id comes from its dir, never from its label. Discovering again with
    /// another dir of the same name (which takes the label and pushes this one to `work-2`) or
    /// with a `label` in config.toml changes no id.
    #[test]
    fn ids_survive_relabelling() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        for d in [
            ".claude-work/projects",
            "zz/claude-work/projects",
            "aa/.claude-work/projects",
        ] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let ids = |seats: &[Seat]| -> BTreeMap<PathBuf, (SeatId, String)> {
            seats
                .iter()
                .map(|s| (s.dir.clone(), (s.id.clone(), s.label.clone())))
                .collect()
        };
        let first = discover(&Candidates {
            home,
            env_dirs: vec![home.join("zz/claude-work")],
            configured: vec![],
        });
        let before = ids(&first);
        assert_eq!(first.len(), 2);
        let second = discover(&Candidates {
            home,
            env_dirs: vec![home.join("zz/claude-work"), home.join("aa/.claude-work")],
            configured: vec![(home.join(".claude-work"), Some("job".into()))],
        });
        let after = ids(&second);
        assert_eq!(second.len(), 3);
        let mut relabelled = 0;
        for (dir, (id, label)) in &before {
            let (id2, label2) = &after[dir];
            assert_eq!(id, id2, "{} keeps its id", dir.display());
            relabelled += usize::from(label != label2);
            assert_eq!(*id, seat_id(dir));
        }
        assert!(
            relabelled >= 1,
            "the labels did move: {before:?} → {after:?}"
        );
        let unique: std::collections::BTreeSet<_> = after.values().map(|v| &v.0).collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn labels() {
        let h = Path::new("/h");
        assert_eq!(label_for(Path::new("/h/.claude"), h), "default");
        assert_eq!(label_for(Path::new("/h/.claude-work"), h), "work");
        assert_eq!(label_for(Path::new("/srv/agents"), h), "agents");
    }
}

//! Discovery of OpenAI Codex CLI homes (`~/.codex`, or `CODEX_HOME`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::core::harness::Harness;
use crate::core::ids::SeatId;
use crate::model::seat::Seat;

/// Recognises a Codex home by the files Codex creates in it.
pub fn looks_like_codex_dir(p: &Path) -> bool {
    p.is_dir()
        && [
            "sessions",
            "session_index.jsonl",
            "history.jsonl",
            "version.json",
            "models_cache.json",
        ]
        .iter()
        .any(|f| p.join(f).exists())
}

pub fn label_for(dir: &Path, default_dir: &Path) -> String {
    if dir == default_dir {
        return "codex".into();
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    let name = name.trim_start_matches('.');
    for prefix in ["codex-", "codex_"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return format!("codex-{rest}");
        }
    }
    name.to_string()
}

pub struct CodexCandidates<'a> {
    pub home: &'a Path,
    /// Homes of running `codex` processes (`CODEX_HOME` of their environment, else
    /// `$HOME/.codex`) and this process's `CODEX_HOME`.
    pub env_dirs: Vec<PathBuf>,
    /// Explicitly configured `[[seat]]` entries targeting `codex` (or auto-detected as `codex`).
    pub configured: Vec<(PathBuf, Option<String>)>,
}

/// Discovers Codex seats: `~/.codex`, `~/.codex-*` siblings that look like Codex homes, the
/// homes of running `codex` processes and the configured ones.
pub fn discover(c: &CodexCandidates) -> Vec<Seat> {
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

    let default = c.home.join(".codex");
    if looks_like_codex_dir(&default) {
        add(default.clone(), "default", None);
    }
    if let Ok(rd) = std::fs::read_dir(c.home) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if (name.starts_with(".codex-") || name.starts_with(".codex_"))
                && looks_like_codex_dir(&e.path())
            {
                add(e.path(), "home scan", None);
            }
        }
    }
    for d in &c.env_dirs {
        if looks_like_codex_dir(d) {
            add(d.clone(), "running codex", None);
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
            let label = label.unwrap_or_else(|| label_for(&dir, &canon_default));
            Seat {
                id: SeatId::of_canonical_dir(&dir),
                label,
                dir: dir.clone(),
                claude_json: PathBuf::new(),
                is_default,
                found_by,
                harness: Harness::Codex,
            }
        })
        .collect();
    seats.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.label.cmp(&b.label)));
    seats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_default_sibling_running_and_configured_codex_homes() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
        std::fs::create_dir_all(home.join(".codex-work")).unwrap();
        std::fs::write(home.join(".codex-work/history.jsonl"), "").unwrap();
        // A sibling without Codex's files is not a seat.
        std::fs::create_dir_all(home.join(".codex-empty")).unwrap();
        std::fs::create_dir_all(home.join("ci/codex-home/sessions")).unwrap();
        std::fs::create_dir_all(home.join("lab-codex")).unwrap();
        let seats = discover(&CodexCandidates {
            home,
            env_dirs: vec![home.join("ci/codex-home"), home.join(".codex")],
            configured: vec![(home.join("lab-codex"), Some("lab".into()))],
        });
        let got: Vec<(&str, Harness, bool)> = seats
            .iter()
            .map(|s| (s.label.as_str(), s.harness, s.is_default))
            .collect();
        assert_eq!(
            got,
            vec![
                ("codex", Harness::Codex, true),
                ("codex-home", Harness::Codex, false),
                ("codex-work", Harness::Codex, false),
                ("lab", Harness::Codex, false),
            ]
        );
        let default = &seats[0];
        assert!(default.found_by.contains(&"default"));
        assert!(default.found_by.contains(&"running codex"));
    }
}

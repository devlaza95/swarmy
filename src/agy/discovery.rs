//! Discovery of Google Antigravity CLI (`agy`) data directories.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::core::harness::Harness;
use crate::core::ids::SeatId;
use crate::model::seat::Seat;

/// Recognises an Antigravity CLI data directory (`~/.gemini/antigravity-cli`, etc.).
pub fn looks_like_agy_dir(p: &Path) -> bool {
    p.is_dir() && Harness::Agy.markers().iter().any(|f| p.join(f).exists())
}

pub fn label_for(dir: &Path, default_dir: &Path) -> String {
    if dir == default_dir {
        return "agy".into();
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    for prefix in [
        "antigravity-cli-",
        "antigravity-cli_",
        "antigravity-",
        "agy-",
        "agy_",
    ] {
        if let Some(rest) = name.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return format!("agy-{rest}");
        }
    }
    if name == "antigravity-cli" || name == "antigravity" {
        return "agy".into();
    }
    name.trim_start_matches('.').to_string()
}

pub struct AgyCandidates<'a> {
    pub home: &'a Path,
    /// Data dirs of running `agy` processes (`$HOME/.gemini/antigravity-cli` of their
    /// environment: the CLI keys its dir on `HOME`).
    pub env_dirs: Vec<PathBuf>,
    /// Explicitly configured `[[seat]]` entries targeting `agy` (or auto-detected as `agy`).
    pub configured: Vec<(PathBuf, Option<String>)>,
}

/// Discovers Antigravity CLI seats.
pub fn discover(c: &AgyCandidates) -> Vec<Seat> {
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

    let gemini_dir = c.home.join(".gemini");
    let default = gemini_dir.join("antigravity-cli");
    if looks_like_agy_dir(&default) {
        add(default.clone(), "default", None);
    }
    if let Ok(rd) = std::fs::read_dir(&gemini_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if (name.starts_with("antigravity-cli-") || name.starts_with("antigravity-cli_"))
                && looks_like_agy_dir(&e.path())
            {
                add(e.path(), "home scan", None);
            }
        }
    }
    for d in &c.env_dirs {
        if looks_like_agy_dir(d) {
            add(d.clone(), "running agy", None);
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
                harness: Harness::Agy,
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
    fn discovers_default_and_configured_agy_seats() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        std::fs::create_dir_all(home.join(".gemini/antigravity-cli/brain")).unwrap();
        std::fs::create_dir_all(home.join(".gemini/antigravity-cli-work/presence")).unwrap();
        std::fs::create_dir_all(home.join("custom-agy")).unwrap();
        let seats = discover(&AgyCandidates {
            home,
            env_dirs: vec![],
            configured: vec![(home.join("custom-agy"), Some("lab".into()))],
        });
        let got: Vec<(&str, Harness)> = seats
            .iter()
            .map(|s| (s.label.as_str(), s.harness))
            .collect();
        assert_eq!(
            got,
            vec![
                ("agy", Harness::Agy),
                ("agy-work", Harness::Agy),
                ("lab", Harness::Agy),
            ]
        );
    }
}

//! Architecture checks over the source text: which top-level modules may name which, and
//! counters that may only go down (ratchets). When a check fails because something got
//! *better*, lower the number in `tests/arch/` — that is the point.
#![allow(
    clippy::unwrap_used,
    reason = "a test binary: a failed read is a failed test"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn src_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            src_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The top-level module a file belongs to (`src/index/query.rs` → `index`, `src/fleet.rs` →
/// `fleet`, `src/main.rs` → `main`).
fn module_of(root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap();
    let first = rel
        .components()
        .next()
        .unwrap()
        .as_os_str()
        .to_string_lossy()
        .to_string();
    first.trim_end_matches(".rs").to_string()
}

/// The file without its `#[cfg(test)] mod tests` (by convention at the end).
fn production(text: &str) -> &str {
    match text.find("#[cfg(test)]\nmod tests") {
        Some(i) => &text[..i],
        None => text,
    }
}

/// Files that are test modules themselves, declared `#[cfg(test)] mod x;` (`src/vault/tests.rs`,
/// `src/index/scenarios.rs`, …): none of their text is production code (review M1).
fn test_only_files(files: &[(PathBuf, String)]) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for (p, text) in files {
        // `a/mod.rs` and `lib.rs` declare children in their own dir, `a/b.rs` in `a/b/`.
        let dir = match p.file_name().and_then(|n| n.to_str()) {
            Some("mod.rs" | "lib.rs" | "main.rs") => p.parent().unwrap().to_path_buf(),
            _ => p.with_extension(""),
        };
        let mut lines = text.lines().map(str::trim);
        while let Some(l) = lines.next() {
            if l != "#[cfg(test)]" {
                continue;
            }
            let Some(next) = lines.next() else { break };
            let decl = next
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub(super) ")
                .trim_start_matches("pub ");
            if let Some(name) = decl.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')) {
                out.insert(dir.join(format!("{name}.rs")));
                out.insert(dir.join(name).join("mod.rs"));
            }
        }
    }
    out
}

fn files() -> (PathBuf, Vec<(PathBuf, String)>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut v = Vec::new();
    src_files(&root, &mut v);
    v.sort();
    let all: Vec<(PathBuf, String)> = v
        .into_iter()
        .map(|p| {
            let t = std::fs::read_to_string(&p).unwrap();
            (p, t)
        })
        .collect();
    // Test-only files count as empty production code.
    let tests = test_only_files(&all);
    let texts = all.into_iter().map(|(p, t)| {
        if tests.contains(&p) {
            (p, String::new())
        } else {
            (p, t)
        }
    });
    (root.clone(), texts.collect())
}

#[test]
fn test_modules_are_not_production_code() {
    let (root, files) = files();
    let empty = |rel: &str| {
        files
            .iter()
            .find(|(p, _)| p == &root.join(rel))
            .is_some_and(|(_, t)| t.is_empty())
    };
    for f in [
        "vault/tests.rs",
        "index/scenarios.rs",
        "index/archive_scenarios.rs",
        "index/compact_scenarios.rs",
        "index/seat_scenarios.rs",
        "index/account_scenarios.rs",
    ] {
        assert!(empty(f), "{f} is a test module");
    }
    assert!(!empty("vault/mod.rs"));
}

fn read_list(name: &str) -> BTreeSet<String> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/arch")
        .join(name);
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Whether `text` names the top-level module `m`: `crate::m::…`, `use crate::m;`,
/// `use crate::m as x;`, or `m` (or `m::…`) inside a `use crate::{…}` group (review B-L8: the
/// check saw only the first form).
fn names_module(text: &str, m: &str) -> bool {
    if [
        format!("crate::{m}::"),
        format!("crate::{m};"),
        format!("crate::{m} as "),
    ]
    .iter()
    .any(|p| text.contains(p.as_str()))
    {
        return true;
    }
    let mut rest = text;
    while let Some(i) = rest.find("crate::{") {
        let group = &rest[i + "crate::{".len()..];
        let (mut depth, mut item, mut end) = (0usize, String::new(), group.len());
        let mut items = Vec::new();
        for (j, ch) in group.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' if depth == 0 => {
                    end = j;
                    break;
                }
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    items.push(std::mem::take(&mut item));
                    continue;
                }
                _ => {}
            }
            item.push(ch);
        }
        items.push(item);
        let hit = items.iter().map(|x| x.trim()).any(|x| {
            x == m || x.starts_with(&format!("{m}::")) || x.starts_with(&format!("{m} as "))
        });
        if hit {
            return true;
        }
        rest = &group[end..];
    }
    false
}

#[test]
fn the_edge_check_sees_every_form_of_use() {
    assert!(names_module("crate::limits::all_reports(ctx)", "limits"));
    assert!(names_module("use crate::limits;", "limits"));
    assert!(names_module("use crate::limits as l;", "limits"));
    assert!(names_module("use crate::{app, limits};", "limits"));
    assert!(names_module(
        "use crate::{\n    limits::{a, b},\n    ui,\n};",
        "limits"
    ));
    assert!(names_module("use crate::{app::{x, y}, ui as u};", "ui"));
    assert!(!names_module(
        "use crate::{app::{limits, y}, ui};",
        "limits"
    ));
    assert!(!names_module("use crate::limitsx;", "limits"));
}

#[test]
fn module_edges_only_shrink() {
    let (root, files) = files();
    let modules: BTreeSet<String> = files.iter().map(|(p, _)| module_of(&root, p)).collect();
    let mut edges: BTreeSet<String> = BTreeSet::new();
    for (p, text) in &files {
        let from = module_of(&root, p);
        for m in &modules {
            if *m != from && m != "main" && names_module(production(text), m) {
                edges.insert(format!("{from} -> {m}"));
            }
        }
    }
    let allowed = read_list("edges.txt");
    let new: Vec<&String> = edges.difference(&allowed).collect();
    let gone: Vec<&String> = allowed.difference(&edges).collect();
    assert!(
        new.is_empty(),
        "new dependencies between modules (not in tests/arch/edges.txt): {new:?}"
    );
    assert!(
        gone.is_empty(),
        "these dependencies are gone, remove them from tests/arch/edges.txt: {gone:?}"
    );
}

/// Functions whose body runs over `max` lines (reviews A-M7, B-M23: orchestrators of 300+ lines
/// mixed every level of abstraction). Counted by braces from the `fn` line; braces inside
/// strings may shift a count, which a ratchet can live with.
fn long_functions(text: &str, max: usize) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let is_fn = |l: &str| {
        let t = l.trim_start();
        let t = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub(super) "))
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t);
        let t = t.strip_prefix("const ").unwrap_or(t);
        let t = t.strip_prefix("async ").unwrap_or(t);
        t.starts_with("fn ")
    };
    let mut n = 0;
    let mut i = 0;
    while i < lines.len() {
        if !is_fn(lines[i]) {
            i += 1;
            continue;
        }
        let (mut depth, mut opened, mut j) = (0i64, false, i);
        while j < lines.len() {
            for ch in lines[j].chars() {
                match ch {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if lines[j].trim_end().ends_with(';') && !opened {
                break; // a declaration without a body
            }
            if opened && depth <= 0 {
                break;
            }
            j += 1;
        }
        if opened && j - i + 1 > max {
            n += 1;
        }
        i += 1;
    }
    n
}

/// What the ratchet counts, in production code only.
fn counts() -> BTreeMap<&'static str, usize> {
    let (root, files) = files();
    let mut c: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (p, text) in &files {
        let m = module_of(&root, p);
        let prod = production(text);
        let n = |pat: &str| prod.matches(pat).count();
        // Silent fallbacks where errors decide who pays and what is saved.
        if matches!(m.as_str(), "index" | "vault" | "app" | "cli" | "limits") {
            *c.entry("fallbacks_index_vault_app_cli_limits").or_default() +=
                n("unwrap_or_default()") + n(".ok()") + n("let _ =") + n("unwrap_or(0");
        }
        // Limit thresholds written again outside `core::limits::Level` (review M1).
        if m != "core" {
            *c.entry("limit_threshold_copies").or_default() +=
                n(">= 85.0") + n(">= 60.0") + n("estimate_now >= 100.0") + n("pct >= 100.0");
        }
        // Intervals belong in the dashboard's job table (`tui/jobs.rs`, review M10).
        if m == "tui" && !p.ends_with("tui/jobs.rs") {
            *c.entry("tui_duration_literals_outside_jobs").or_default() +=
                n("Duration::from_secs(") + n("Duration::from_millis(") + n(".as_secs() <");
        }
        // The clock below the composition root and the front ends (reviews A-M6, B-M17): time
        // is an input there, so a pass reads one instant and tests can fix it.
        if !matches!(
            m.as_str(),
            "app" | "cli" | "tui" | "main" | "demo" | "statusline"
        ) {
            *c.entry("clock_reads_below_front_ends").or_default() +=
                n("Utc::now()") + n("Local::now()");
        }
        // The environment is read where the process starts (reviews A-M6, B-M26).
        if !matches!(m.as_str(), "app" | "cli" | "config" | "main" | "diag") {
            *c.entry("env_reads_outside_config_cli_app").or_default() += n("std::env::var");
        }
        // `--mask` is presentation: applied by the front ends only (reviews A-H4, B-M16).
        if matches!(m.as_str(), "index" | "limits" | "live" | "vault" | "core") {
            *c.entry("mask_below_front_ends").or_default() += n(".mask") + n("show_path(");
        }
        // One seat discovery (`claude::seats::discover`; reviews A-M3, B-M19).
        if !p.ends_with("claude/seats.rs") {
            *c.entry("claude_dir_scans_outside_seats").or_default() +=
                n("starts_with(\".claude-\")") + n("starts_with(\".claude_\")");
        }
        // One local midnight (`core::period`; review B-M20).
        if m != "core" {
            *c.entry("midnight_copies_outside_core").or_default() += n("NaiveTime::MIN)");
        }
        // Never "pt" on screen (AGENTS.md; reviews A-L1, B-L2).
        *c.entry("pts_on_screen").or_default() +=
            n("} pts") + n(" pts\"") + n("pts/h") + n("/pt\"");
        *c.entry("unwraps").or_default() += n(".unwrap()") + n(".expect(");
        *c.entry("wildcard_arms").or_default() += n("_ =>");
        *c.entry("functions_over_120_lines").or_default() += long_functions(prod, 120);
        if prod.lines().count() > 900 {
            *c.entry("files_over_900_lines").or_default() += 1;
        }
    }
    c
}

#[test]
fn ratchets_only_go_down() {
    let limits: BTreeMap<String, usize> = read_list("ratchets.txt")
        .into_iter()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse().ok()?))
        })
        .collect();
    let now = counts();
    let mut errors = Vec::new();
    for (k, v) in &now {
        match limits.get(*k) {
            None => errors.push(format!("{k} = {v} is not in tests/arch/ratchets.txt")),
            Some(max) if v > max => errors.push(format!("{k}: {v} > {max} (it may only go down)")),
            Some(max) if v < max => errors.push(format!(
                "{k}: {v} < {max} — good, lower it to {v} in tests/arch/ratchets.txt"
            )),
            Some(_) => {}
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// `core` is the pure domain: no clock, no files, no database (review M2). Time comes in as an
/// argument, so every rule there is testable with fixed numbers.
#[test]
fn core_is_pure() {
    let (root, files) = files();
    let mut found = Vec::new();
    for (p, text) in &files {
        if module_of(&root, p) != "core" {
            continue;
        }
        for pat in ["Utc::now", "Local::now", "std::fs", "rusqlite", "std::env"] {
            if production(text).contains(pat) {
                found.push(format!(
                    "{}: {pat}",
                    p.strip_prefix(&root).unwrap().display()
                ));
            }
        }
    }
    assert!(found.is_empty(), "core must stay pure: {found:?}");
}

/// The index owns its schema: no SQL and no `rusqlite` outside `src/index/` (review M4). Other
/// modules read through typed methods (`index::live`, `index::query`, …).
#[test]
fn sql_stays_in_the_index() {
    let (root, files) = files();
    // One reader of another program's database may query it, read-only (review A-M2: it was
    // kept in `src/index/` only to pass this check).
    let read_only_readers = ["agy/summaries.rs"];
    let found: Vec<String> = files
        .iter()
        .filter(|(p, _)| module_of(&root, p) != "index")
        .filter(|(_, t)| t.contains("SELECT ") || t.contains("rusqlite"))
        .map(|(p, _)| p.strip_prefix(&root).unwrap().display().to_string())
        .filter(|p| !read_only_readers.contains(&p.replace('\\', "/").as_str()))
        .collect();
    assert!(found.is_empty(), "SQL outside src/index/: {found:?}");
    for r in read_only_readers {
        let (_, text) = files.iter().find(|(p, _)| p.ends_with(r)).unwrap();
        let text = production(text);
        assert!(
            text.contains("SQLITE_OPEN_READ_ONLY")
                && !text.contains("OpenFlags::SQLITE_OPEN_READ_WRITE")
                && !text.contains("Connection::open(")
                && !["INSERT ", "UPDATE ", "DELETE ", "CREATE "]
                    .iter()
                    .any(|w| text.contains(w)),
            "{r} must only read its database"
        );
    }
}

/// One way to replace a file (review M8): `fsutil::atomic_write`. A rename elsewhere is a second
/// implementation with weaker guarantees. `config.rs` moves the old program's dirs over once.
#[test]
fn renames_only_in_fsutil() {
    let (root, files) = files();
    let found: Vec<String> = files
        .iter()
        .filter(|(p, _)| !p.ends_with("fsutil.rs") && !p.ends_with("config.rs"))
        .filter(|(_, t)| production(t).contains("fs::rename("))
        .map(|(p, _)| p.strip_prefix(&root).unwrap().display().to_string())
        .collect();
    assert!(found.is_empty(), "rename outside src/fsutil.rs: {found:?}");
}

/// One name per concept (first review L1): the screen says "limits", so does the code. Only the
/// one-time move from the old program's dirs (`agentmana`) may say the old word.
#[test]
fn limits_are_never_called_mana() {
    let (root, files) = files();
    let found: Vec<String> = files
        .iter()
        .filter(|(_, t)| {
            // `mana`, `mana_x`, `Mana`, `ManaRow`; not `manage` or `manager`.
            let t = t.replace("agentmana", "");
            ["mana", "Mana", "MANA"].iter().any(|w| {
                t.match_indices(w).any(|(i, _)| {
                    !t[i + w.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase())
                })
            })
        })
        .map(|(p, _)| p.strip_prefix(&root).unwrap().display().to_string())
        .collect();
    assert!(found.is_empty(), "\"mana\" in {found:?}");
}

/// Short ids come from `core::ids::short` / `SessionId::short` (review M10): a byte slice like
/// `[..8]` panics on a short or multi-byte id.
#[test]
fn short_ids_are_never_sliced() {
    let (root, files) = files();
    let found: Vec<String> = files
        .iter()
        .filter(|(_, t)| {
            // Digests (fixed-size arrays) may be sliced; ids read from files may not.
            [
                "[..8.min(",
                ".min(8)]",
                "_id[..",
                "uuid[..",
                "fingerprint[..",
            ]
            .iter()
            .any(|p| t.contains(p))
        })
        .map(|(p, _)| p.strip_prefix(&root).unwrap().display().to_string())
        .collect();
    assert!(found.is_empty(), "id slicing in {found:?}");
}

/// The vault's raw writers stay private to it (review L5), and the seat files Claude Code also
/// writes (`.credentials.json`, `.claude.json`) are written only through the checked writers,
/// which never go over a version they have not read (`store::replace_if_unchanged`,
/// `store::rewrite_merging`; review M7): never with a plain `atomic_write`.
#[test]
fn seat_files_are_written_only_through_checked_writers() {
    let (root, files) = files();
    let mut found: Vec<String> = Vec::new();
    for (p, t) in &files {
        let name = p.strip_prefix(&root).unwrap().display().to_string();
        let prod = production(t);
        if module_of(&root, p) != "vault" && prod.contains("vault::store") {
            found.push(format!("{name}: uses vault::store"));
        }
        if prod.contains("pub mod store") {
            found.push(format!("{name}: exports a raw writer module"));
        }
        for line in prod.lines() {
            if line.contains("atomic_write(")
                && (line.contains("credentials_file") || line.contains("claude_json"))
            {
                found.push(format!("{name}: {}", line.trim()));
            }
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

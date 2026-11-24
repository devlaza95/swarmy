//! Mapping a working directory to a project.
//!
//! A project is the root of a git repository. Git worktrees fold into the repository they belong
//! to (`.git` there is a file pointing into `<repo>/.git/worktrees/<name>`), so twenty agents in
//! twenty worktrees count as one project. Paths that no longer exist, or come from another machine,
//! fall back to user aliases and then to the path itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

pub use crate::model::project::Alias;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Project {
    /// Stable key: the repository root path (or the alias name).
    pub key: String,
    /// Short display name.
    pub name: String,
    /// Set when the cwd is inside a linked worktree.
    pub worktree: Option<String>,
    /// No repository or alias was found: the dir is gone, or it is a scratch dir. Such a cwd takes
    /// the project of the session it was used in (see `Index::post_pass`).
    #[serde(skip)]
    pub orphan: bool,
}

#[derive(Default)]
pub struct Resolver {
    aliases: Vec<Alias>,
    cache: HashMap<String, Project>,
}

impl Resolver {
    pub fn new(mut aliases: Vec<Alias>) -> Self {
        // Longest prefix first.
        aliases.sort_by_key(|a| std::cmp::Reverse(a.prefix.len()));
        Resolver {
            aliases,
            cache: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, cwd: &str) -> Project {
        if let Some(p) = self.cache.get(cwd) {
            return p.clone();
        }
        let p = self.compute(cwd);
        self.cache.insert(cwd.to_string(), p.clone());
        p
    }

    fn compute(&self, cwd: &str) -> Project {
        if let Some(a) = self
            .aliases
            .iter()
            .find(|a| cwd.starts_with(a.prefix.trim_end_matches('*')))
        {
            return Project {
                key: format!("alias:{}", a.name),
                name: a.name.clone(),
                worktree: None,
                orphan: false,
            };
        }
        // A dir that is gone is named by the sibling rule first: walking up from it would find
        // any repository that encloses `<repo>-wt/` (a dotfiles repo at home, a monorepo).
        let p = Path::new(cwd);
        let found = if p.exists() {
            git_root(p)
        } else {
            worktree_sibling(p).or_else(|| git_root(p))
        };
        if let Some((root, worktree)) = found {
            let name = dir_name(&root);
            return Project {
                key: root.to_string_lossy().into_owned(),
                name,
                worktree,
                orphan: false,
            };
        }
        let p = Path::new(cwd);
        // The home dir is named "~", not after the user.
        let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
        let name = if home.as_deref() == Some(p) {
            "~".to_string()
        } else {
            dir_name(p)
        };
        Project {
            key: cwd.to_string(),
            name,
            worktree: None,
            orphan: true,
        }
    }
}

fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// Worktrees kept next to the repository, as `<repo>-wt/<name>` or `<repo>-worktrees/<name>`:
/// once such a worktree is removed, its old path still names the repository.
fn worktree_sibling(dir: &Path) -> Option<(PathBuf, Option<String>)> {
    let mut cur = dir;
    while let Some(parent) = cur.parent() {
        let name = cur.file_name()?.to_string_lossy();
        let base = name
            .strip_suffix("-wt")
            .or_else(|| name.strip_suffix("-worktrees"));
        if let Some(base) = base.filter(|b| !b.is_empty()) {
            let repo = parent.join(base);
            if repo.join(".git").exists() {
                let wt = dir
                    .ancestors()
                    .find(|a| a.parent() == Some(cur))
                    .unwrap_or(dir);
                return Some((repo, Some(wt.to_string_lossy().into_owned())));
            }
        }
        cur = parent;
    }
    None
}

/// The main repository root of `dir` and, for a linked worktree, the worktree path.
pub fn git_root(dir: &Path) -> Option<(PathBuf, Option<String>)> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let git = d.join(".git");
        if git.is_dir() {
            return Some((d.to_path_buf(), None));
        }
        if git.is_file() {
            // `gitdir: /repo/.git/worktrees/<name>` (relative paths are relative to `d`).
            let text = std::fs::read_to_string(&git).ok()?;
            let gitdir = text
                .lines()
                .find_map(|l| l.strip_prefix("gitdir:"))
                .map(str::trim)?;
            let gitdir = if Path::new(gitdir).is_absolute() {
                PathBuf::from(gitdir)
            } else {
                d.join(gitdir)
            };
            let comps: Vec<_> = gitdir.components().collect();
            if let Some(i) = comps.iter().rposition(|c| c.as_os_str() == "worktrees")
                && i >= 1
                && comps[i - 1].as_os_str() == ".git"
            {
                let main: PathBuf = comps[..i - 1].iter().collect();
                return Some((main, Some(d.to_string_lossy().into_owned())));
            }
            // A submodule or something else: treat `d` as its own root.
            return Some((d.to_path_buf(), None));
        }
        cur = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_subdir_worktree_and_missing_paths() {
        // Hermetic: the temp root is itself inside a repository (as when TMPDIR lies in a git
        // checkout, or home is a dotfiles repo), and the rules below still hold.
        let outer = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outer.path().join(".git")).unwrap();
        let t = outer.path().join("work");
        std::fs::create_dir_all(&t).unwrap();
        let repo = t.as_path().join("shop");
        std::fs::create_dir_all(repo.join(".git/worktrees/feat")).unwrap();
        std::fs::create_dir_all(repo.join("backend/src")).unwrap();
        let wt = t.as_path().join("shop-wt-feat");
        std::fs::create_dir_all(wt.join("app")).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/feat").display()),
        )
        .unwrap();

        let mut r = Resolver::new(vec![Alias {
            prefix: "/home/user/work/old-name".into(),
            name: "shop".into(),
        }]);
        let p = r.resolve(&repo.join("backend/src").to_string_lossy());
        assert_eq!(p.name, "shop");
        assert_eq!(p.key, repo.to_string_lossy());
        assert_eq!(p.worktree, None);

        let w = r.resolve(&wt.join("app").to_string_lossy());
        assert_eq!(
            w.key,
            repo.to_string_lossy(),
            "worktree folds into its repository"
        );
        assert_eq!(w.worktree.as_deref(), Some(wt.to_string_lossy().as_ref()));

        let a = r.resolve("/home/user/work/old-name/app");
        assert_eq!((a.key.as_str(), a.name.as_str()), ("alias:shop", "shop"));

        // A dir that is gone inside a repository still belongs to it.
        let gone_sub = r.resolve(&repo.join("target/tmp/x").to_string_lossy());
        assert_eq!(gone_sub.key, repo.to_string_lossy());

        let m = r.resolve("/nowhere/at/all/thing");
        assert_eq!(
            (m.key.as_str(), m.name.as_str()),
            ("/nowhere/at/all/thing", "thing")
        );
        assert!(m.orphan);
        assert!(!p.orphan);

        // A removed worktree under `<repo>-wt/` still belongs to the repository, not to the
        // repository that encloses them all.
        let gone = r.resolve(&t.as_path().join("shop-wt/fix-1/backend").to_string_lossy());
        assert_eq!((gone.name.as_str(), gone.orphan), ("shop", false));
        assert_eq!(
            gone.worktree.as_deref(),
            Some(t.as_path().join("shop-wt/fix-1").to_string_lossy().as_ref())
        );
        // An existing dir outside `shop` belongs to the enclosing repository.
        std::fs::create_dir_all(t.as_path().join("notes")).unwrap();
        let notes = r.resolve(&t.as_path().join("notes").to_string_lossy());
        assert_eq!(notes.key, outer.path().to_string_lossy());
    }
}

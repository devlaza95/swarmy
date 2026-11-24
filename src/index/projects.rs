//! The `projects` table: the project of every working directory, recorded while the directory
//! still exists (a dimension table: it outlives the raw rows and labels the archive's rollups,
//! which keep the cwd, not the project; design §3.8).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::claude::project::{Project, Resolver};

/// Every working directory of the index: those of raw requests and those only the archive's
/// rollups still name.
pub(super) fn every_cwd(tx: &Connection) -> rusqlite::Result<Vec<String>> {
    tx.prepare(
        "SELECT cwd FROM requests WHERE cwd IS NOT NULL
         UNION SELECT cwd FROM usage_rollup WHERE cwd != ''",
    )?
    .query_map([], |r| r.get::<_, String>(0))?
    .collect()
}

/// Records the project of each of `cwds`. A dir outside any repository (a removed worktree, a
/// scratch dir) takes the project of the repository the sessions that used it run in, counted
/// over raw requests and rollups alike; failing that, the session's own dir when it lies under it.
/// **Never downgrade:** a dir that is gone, resolves to no project now and has no such session
/// keeps the project recorded while it existed (the resolver can only do worse than that now).
pub(super) fn assign(
    tx: &Connection,
    resolver: &mut Resolver,
    cwds: &[String],
) -> anyhow::Result<()> {
    let mut orphans: Vec<(&str, Project)> = Vec::new();
    let mut put = tx.prepare(
        "INSERT OR REPLACE INTO projects(cwd, key, name, worktree) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for cwd in cwds {
        let p = resolver.resolve(cwd);
        if p.orphan {
            orphans.push((cwd, p));
        } else {
            put.execute(params![cwd, p.key, p.name, p.worktree])?;
        }
    }
    if orphans.is_empty() {
        return Ok(());
    }
    let votes = votes(tx, orphans.iter().map(|(c, _)| *c).collect())?;
    // Never fold into a home dir or anything as shallow (`/home/x/work` is the least).
    let under = |dir: &str, root: &str| {
        root.matches(['/', '\\']).count() >= 4
            && dir
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with(['/', '\\']))
    };
    let mut recorded = tx.prepare("SELECT 1 FROM projects WHERE cwd = ?1")?;
    for (cwd, own) in orphans {
        let mains = votes.get(cwd);
        // A repository the session runs in; else the session's own dir when this one lies under
        // it (subdirs and `.claude/worktrees/*` of a dir that is gone now).
        let best = mains.and_then(|mains| {
            mains
                .iter()
                .map(|(m, n)| (resolver.resolve(m), *n))
                .filter(|(p, _)| !p.orphan)
                .max_by_key(|(_, n)| *n)
                .map(|x| x.0)
                .or_else(|| {
                    mains
                        .keys()
                        .filter(|m| under(cwd, m))
                        .max_by_key(|m| m.len())
                        .map(|m| resolver.resolve(m))
                })
        });
        let p = match best {
            Some(p) => Project {
                worktree: Some(cwd.to_string()),
                ..p
            },
            None if !Path::new(cwd).exists()
                && recorded.query_row([cwd], |_| Ok(())).optional()?.is_some() =>
            {
                continue;
            }
            None => own,
        };
        put.execute(params![cwd, p.key, p.name, p.worktree])?;
    }
    Ok(())
}

/// For each of `orphans`, the working directories of the sessions it was used in (when they
/// differ from it), weighted by requests, raw and rolled up.
fn votes(
    tx: &Connection,
    orphans: HashSet<&str>,
) -> rusqlite::Result<HashMap<String, HashMap<String, i64>>> {
    let mut votes: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut st = tx.prepare(
        "SELECT r.cwd, s.cwd, SUM(r.n) FROM (
           SELECT seat, session_id, cwd, 1 AS n FROM requests
           UNION ALL SELECT seat, session_id, NULLIF(cwd, ''), requests FROM usage_rollup
         ) r JOIN sessions s ON s.seat = r.seat AND s.session_id = r.session_id
         WHERE s.cwd IS NOT NULL AND s.cwd != r.cwd GROUP BY r.cwd, s.cwd",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (cwd, main, n) = row?;
        if orphans.contains(cwd.as_str()) {
            *votes.entry(cwd).or_default().entry(main).or_default() += n;
        }
    }
    Ok(votes)
}

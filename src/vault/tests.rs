//! Vault behaviour on throwaway seats. Never touches a real config dir.

use chrono::Utc;
use serde_json::{Value, json};

use super::*;

fn seat(root: &Path, label: &str) -> anyhow::Result<Seat> {
    let dir = root.join(format!(".claude-{label}"));
    std::fs::create_dir_all(&dir)?;
    Ok(Seat::new(
        label,
        dir.clone(),
        dir.join(".claude.json"),
        false,
    ))
}

fn write_seat(s: &Seat, refresh: &str, account: &str) -> anyhow::Result<()> {
    let creds = json!({
        "mcpOAuth": {"plugin:x|1": {"accessToken": format!("mcp-{}", s.label)}},
        "claudeAiOauth": {"accessToken": format!("acc-{refresh}"), "refreshToken": refresh, "expiresAt": 1_795_377_583_375_i64}
    });
    std::fs::write(s.credentials_file(), creds.to_string())?;
    let cj = json!({"numStartups": 7, "oauthAccount": {"accountUuid": account, "emailAddress": format!("{account}@example.com")}});
    std::fs::write(&s.claude_json, serde_json::to_string_pretty(&cj)?)?;
    Ok(())
}

fn read_json(p: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
}

fn find<'a>(v: &'a Vault, key: &str) -> Option<&'a Login> {
    v.index.find(key).ok()
}

fn login<'a>(v: &'a Vault, key: &str) -> anyhow::Result<&'a Login> {
    find(v, key).with_context(|| format!("no login '{key}'"))
}

/// A saved secret, read from disk the way another process would see it.
fn secret_of(v: &Vault, id: &LoginId) -> anyhow::Result<Value> {
    Ok(read_json(&v.dir.join("secrets.json"))?[id.as_str()].clone())
}

fn secret(refresh: &str) -> Secret {
    Secret::new(json!({"accessToken": format!("acc-{refresh}"), "refreshToken": refresh}))
}

/// Files in `dir` whose names end with `suffix`.
fn count(dir: &Path, suffix: &str) -> usize {
    std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(suffix))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn capture_switch_and_back() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let spare = seat(t.path(), "spare")?;
    write_seat(&work, "r-alice", "alice")?;
    write_seat(&spare, "r-bob", "bob")?;
    let mut v = Vault::open(&t.path().join("vault"))?;

    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.capture(&spare, Some("bob")))?;
    assert_eq!(login(&v, "alice")?.mounted, vec![work.id.clone()]);
    assert_eq!(login(&v, "bob")?.email.as_deref(), Some("bob@example.com"));

    // Free bob by putting a third login into `spare` (sharing has its own test).
    let carol = Secret::new(json!({"accessToken": "acc-c", "refreshToken": "r-carol"}));
    v.edit(|tx| {
        tx.put(
            &carol,
            Some(json!({"accountUuid": "carol"})),
            None,
            Some("carol"),
            None,
        )
    })?;
    v.edit(|tx| tx.switch(&spare, "carol"))?;
    assert_eq!(login(&v, "bob")?.mounted, Vec::<SeatId>::new());

    let rep = v.edit(|tx| tx.switch(&work, "bob"))?;
    assert_eq!(
        (rep.from.as_deref(), rep.to.as_str()),
        (Some("alice"), "bob")
    );
    let creds = read_json(&work.credentials_file())?;
    assert_eq!(creds["claudeAiOauth"]["refreshToken"], "r-bob");
    assert_eq!(
        creds["mcpOAuth"]["plugin:x|1"]["accessToken"], "mcp-work",
        "MCP tokens belong to the seat"
    );
    let cj = read_json(&work.claude_json)?;
    assert_eq!(cj["oauthAccount"]["accountUuid"], "bob");
    assert_eq!(
        cj["numStartups"], 7,
        "the rest of .claude.json is untouched"
    );
    assert_eq!(login(&v, "alice")?.mounted, Vec::<SeatId>::new());
    assert_eq!(login(&v, "bob")?.mounted, vec![work.id.clone()]);
    let backups = std::fs::read_dir(rep.backup.context("no backup")?)?.count();
    assert!(backups >= 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(work.credentials_file())?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    // Back again.
    v.edit(|tx| tx.switch(&work, "alice"))?;
    assert_eq!(
        read_json(&work.credentials_file())?["claudeAiOauth"]["refreshToken"],
        "r-alice"
    );
    Ok(())
}

fn write_creds(s: &Seat, access: &str, refresh: &str) -> anyhow::Result<()> {
    let creds =
        json!({"mcpOAuth": {}, "claudeAiOauth": {"accessToken": access, "refreshToken": refresh}});
    std::fs::write(s.credentials_file(), creds.to_string())?;
    Ok(())
}

#[test]
fn a_fresh_access_token_is_followed() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    // Claude Code refreshes the access token and keeps the refresh token.
    write_creds(&work, "acc-new", "r-alice")?;
    v.edit(|tx| tx.sync(std::slice::from_ref(&work)))?;
    let alice = login(&v, "alice")?.clone();
    assert_eq!(alice.mounted, vec![work.id.clone()]);
    assert_eq!(secret_of(&v, &alice.id)?["accessToken"], "acc-new");
    Ok(())
}

#[test]
fn a_new_login_never_overwrites_the_saved_one() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    // A backup of another account is copied in; the profile still names alice.
    write_creds(&work, "acc-bob", "r-bob")?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    let st = &states[&work.id];
    assert!(
        st.unverified_since.is_some(),
        "account not confirmed: {st:?}"
    );
    assert_eq!(st.account_uuid, None);
    assert_eq!(st.saved_as, None);
    let alice = login(&v, "alice")?.clone();
    assert_eq!(alice.mounted, Vec::<SeatId>::new());
    assert_eq!(
        secret_of(&v, &alice.id)?["refreshToken"],
        "r-alice",
        "alice keeps her tokens"
    );
    // Saving it creates a separate login without borrowing alice's account.
    v.edit(|tx| tx.capture(&work, Some("bob")))?;
    let bob = login(&v, "bob")?;
    assert_eq!(bob.account_uuid, None);
    assert_eq!(bob.mounted, vec![work.id.clone()]);
    assert_eq!(v.logins().len(), 2);
    // Claude Code re-reads the profile: now the account is known.
    let cj = json!({"oauthAccount": {"accountUuid": "bob", "emailAddress": "bob@example.com", "profileFetchedAt": Utc::now().timestamp_millis()}});
    std::fs::write(&work.claude_json, cj.to_string())?;
    let st = &v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?[&work.id];
    assert_eq!(st.unverified_since, None);
    assert_eq!(st.account_uuid.as_deref(), Some("bob"));
    assert_eq!(st.saved_as.as_deref(), Some("bob"));
    Ok(())
}

#[test]
fn switching_away_from_an_unsaved_login_keeps_it() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    write_creds(&work, "acc-x", "r-unknown")?;
    let rep = v.edit(|tx| tx.switch(&work, "alice"))?;
    assert_eq!(rep.auto_saved.as_deref(), Some("work"));
    assert_eq!(
        read_json(&work.credentials_file())?["claudeAiOauth"]["refreshToken"],
        "r-alice"
    );
    let kept = v
        .logins()
        .iter()
        .find(|l| l.label == "work")
        .expect("the unknown login was saved under the seat's name");
    assert_eq!(kept.account_uuid, None);
    assert_eq!(secret_of(&v, &kept.id)?["refreshToken"], "r-unknown");
    Ok(())
}

#[test]
fn remove_refuses_mounted_logins_and_secrets_never_print() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-secret-value", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    assert!(v.edit(|tx| tx.remove("alice", false)).is_err());
    let logins = std::fs::read_to_string(t.path().join("vault/logins.json"))?;
    assert!(
        !logins.contains("r-secret-value"),
        "metadata file holds no tokens"
    );
    v.edit(|tx| tx.rename("alice", "personal"))?;
    assert!(find(&v, "personal").is_some());
    v.edit(|tx| tx.remove("personal", true))?;
    assert!(v.logins().is_empty());
    let secrets = std::fs::read_to_string(t.path().join("vault/secrets.json"))?;
    assert!(!secrets.contains("r-secret-value"));
    Ok(())
}

#[test]
fn concurrent_changes_never_lose_a_login_or_corrupt_a_file() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    // Enough to interleave every kind of change many times, small enough that no edit waits
    // anywhere near `LOCK_TIMEOUT` on a loaded machine (review L4: 8 × 40 once timed out).
    const THREADS: usize = 4;
    const ROUNDS: usize = 16;
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    let seats: Vec<Seat> = (0..3)
        .map(|i| seat(t.path(), &format!("s{i}")))
        .collect::<anyhow::Result<_>>()?;
    for (i, s) in seats.iter().enumerate() {
        write_seat(s, &format!("r-seat{i}"), &format!("acct{i}"))?;
    }
    Vault::open(&vdir)?;
    let stop = AtomicBool::new(false);
    std::thread::scope(|sc| -> anyhow::Result<()> {
        // A lock-free reader always sees whole files.
        sc.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                for f in ["logins.json", "secrets.json"] {
                    if let Ok(text) = std::fs::read_to_string(vdir.join(f)) {
                        serde_json::from_str::<Value>(&text)
                            .unwrap_or_else(|e| panic!("{f} is damaged: {e}"));
                    }
                }
            }
        });
        // Claude Code keeps rewriting `.claude.json` meanwhile.
        let claude = sc.spawn(|| -> anyhow::Result<()> {
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                for s in &seats {
                    n += 1;
                    let mut cj = read_json(&s.claude_json)?;
                    cj["numStartups"] = json!(n);
                    store::atomic_write(&s.claude_json, cj.to_string().as_bytes())?;
                }
            }
            Ok(())
        });
        let workers: Vec<_> = (0..THREADS)
            .map(|th| {
                let (vdir, seats) = (&vdir, &seats);
                sc.spawn(move || -> anyhow::Result<()> {
                    let mut v = Vault::open(vdir)?;
                    for r in 0..ROUNDS {
                        let seat = &seats[(th + r) % seats.len()];
                        let step = r % 4;
                        let done = if step == 0 {
                            let sec = secret(&format!("r-{th}-{r}"));
                            let label = format!("t{th}-{r}");
                            v.edit(|tx| tx.put(&sec, None, None, Some(&label), None))
                                .map(drop)
                        } else if step == 1 {
                            v.edit(|tx| tx.capture(seat, None)).map(drop)
                        } else if step == 2 {
                            v.edit(|tx| tx.seat_states(seats)).map(drop)
                        } else {
                            let login = format!("t{th}-{}", r - 3);
                            v.edit(|tx| {
                                tx.sync(seats)?;
                                tx.switch(seat, &login)
                            })
                            .map(drop)
                        };
                        // Says which change failed and why, so a failure is never a mystery.
                        done.with_context(|| format!("thread {th}, round {r}, step {step}"))?;
                    }
                    Ok(())
                })
            })
            .collect();
        let mut result = Ok(());
        for w in workers {
            let r = w.join().map_err(|_| anyhow::anyhow!("a worker panicked"));
            result = result.and(r.and_then(|r| r));
        }
        stop.store(true, Ordering::Relaxed);
        result?;
        claude
            .join()
            .map_err(|_| anyhow::anyhow!("the writer panicked"))?
    })?;

    let v = Vault::open(&vdir)?;
    let secrets = read_json(&vdir.join("secrets.json"))?;
    for th in 0..THREADS {
        for r in (0..ROUNDS).step_by(4) {
            assert!(find(&v, &format!("t{th}-{r}")).is_some(), "t{th}-{r} lost");
        }
    }
    for l in v.logins() {
        let sec = Secret::new(secrets[l.id.as_str()].clone());
        assert_eq!(sec.meta().fingerprint, l.meta.fingerprint, "{}", l.label);
    }
    // The seats' first logins were saved before anything overwrote them.
    for i in 0..seats.len() {
        let refresh = format!("r-seat{i}");
        assert!(
            secrets
                .as_object()
                .context("secrets.json holds an object")?
                .values()
                .any(|s| s["refreshToken"] == refresh.as_str()),
            "{refresh} lost"
        );
    }
    for s in &seats {
        let mounted = v.logins().iter().filter(|l| l.is_in(&s.id)).count();
        assert!(mounted <= 1, "{} logins mounted in {}", mounted, s.label);
    }
    let mut labels: Vec<&str> = v.logins().iter().map(|l| l.label.as_str()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), v.logins().len(), "labels are unique");
    assert_eq!(count(&vdir, ".tmp"), 0, "no temp file is left behind");
    Ok(())
}

#[test]
fn an_edit_rereads_the_vault_under_the_lock() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    // Both views are opened before either change.
    let mut first = Vault::open(&vdir)?;
    let mut second = Vault::open(&vdir)?;
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    std::thread::scope(|sc| -> anyhow::Result<()> {
        let holder = sc.spawn(|| {
            first.edit(|tx| {
                tx.put(&secret("r-a"), None, None, Some("a"), None)?;
                locked_tx.send(()).context("the main thread is gone")?;
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(())
            })
        });
        locked_rx.recv()?;
        // Waits for the first edit, then builds on what it saved.
        second.edit(|tx| tx.put(&secret("r-b"), None, None, Some("b"), None))?;
        holder
            .join()
            .map_err(|_| anyhow::anyhow!("the first edit panicked"))?
    })?;
    let v = Vault::open(&vdir)?;
    assert!(find(&v, "a").is_some() && find(&v, "b").is_some());
    assert_eq!(secret_of(&v, &login(&v, "a")?.id)?["refreshToken"], "r-a");
    assert_eq!(second.logins().len(), 2, "the view shows the saved vault");
    Ok(())
}

#[test]
fn a_lock_file_left_behind_does_not_block() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    std::fs::create_dir_all(&vdir)?;
    // What an older swarmy or a crashed process may leave: a pid that is not running.
    std::fs::write(vdir.join("lock"), "4194300")?;
    let mut v = Vault::open(&vdir)?;
    v.edit(|tx| tx.put(&secret("r-a"), None, None, Some("a"), None))?;
    v.edit(|tx| tx.rename("a", "b"))?;
    assert!(find(&v, "b").is_some());
    Ok(())
}

#[test]
fn a_failed_edit_saves_nothing() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    let res: anyhow::Result<()> = v.edit(|tx| {
        tx.put(&secret("r-a"), None, None, Some("a"), None)?;
        anyhow::bail!("changed my mind")
    });
    assert!(res.is_err());
    assert!(v.logins().is_empty());
    assert!(!v.dir.join("logins.json").exists());
    assert!(!v.dir.join("secrets.json").exists());
    Ok(())
}

#[test]
fn switch_keeps_what_claude_code_wrote_meanwhile() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let path = t.path().join(".claude.json");
    std::fs::write(
        &path,
        r#"{"numStartups": 7, "oauthAccount": {"accountUuid": "a"}}"#,
    )?;
    let calls = std::cell::Cell::new(0);
    let wrote = store::rewrite_merging(&path, |text| {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            // Claude Code replaces the file while swarmy prepares its version.
            let newer = r#"{"numStartups": 8, "projects": {"p": {}}, "oauthAccount": {"accountUuid": "a"}}"#;
            store::atomic_write(&path, newer.as_bytes())?;
        }
        config_json::with_oauth_account(text, Some(&json!({"accountUuid": "b"})))
    })?;
    assert!(wrote);
    assert_eq!(
        calls.get(),
        2,
        "the change is applied again to the newer text"
    );
    let cj = read_json(&path)?;
    assert_eq!(cj["oauthAccount"]["accountUuid"], "b");
    assert_eq!(cj["numStartups"], 8);
    assert!(cj["projects"]["p"].is_object());
    assert!(!store::rewrite_merging(
        &t.path().join("missing.json"),
        |t| Ok(t.into())
    )?);
    Ok(())
}

#[test]
fn labels_stay_unique() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some("bob"), None))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    // Re-labelling an existing login with another login's label is refused everywhere.
    let err = v
        .edit(|tx| tx.capture(&work, Some("bob")))
        .unwrap_err()
        .to_string();
    assert!(err.contains("'bob' already exists"), "{err}");
    let err = v
        .edit(|tx| tx.put(&secret("r-alice"), None, None, Some("bob"), None))
        .unwrap_err()
        .to_string();
    assert!(err.contains("'bob' already exists"), "{err}");
    assert!(v.edit(|tx| tx.rename("alice", "bob")).is_err());
    assert!(v.edit(|tx| tx.rename("alice", " ")).is_err());
    assert_eq!(
        login(&v, "alice")?.meta.fingerprint,
        secret("r-alice").meta().fingerprint
    );
    // Its own label is fine.
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    // Without a label a new login gets a free one.
    v.edit(|tx| tx.put(&secret("r-bob2"), None, None, None, None))?;
    v.edit(|tx| tx.put(&secret("r-x"), None, None, None, None))?;
    assert!(find(&v, "login").is_some() && find(&v, "login-2").is_some());
    // Re-saving an unconfirmed login keeps its label instead of renaming it.
    write_creds(&work, "acc-u", "r-unknown")?;
    v.edit(|tx| tx.capture(&work, None))?;
    v.edit(|tx| tx.capture(&work, None))?;
    assert!(find(&v, "work").is_some() && find(&v, "work-2").is_none());
    Ok(())
}

#[test]
fn duplicate_labels_from_older_versions_are_repaired() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    let mut v = Vault::open(&vdir)?;
    v.edit(|tx| tx.put(&secret("r-a"), None, None, Some("a"), None))?;
    v.edit(|tx| tx.put(&secret("r-b"), None, None, Some("b"), None))?;
    let mut idx = read_json(&vdir.join("logins.json"))?;
    idx["logins"][1]["label"] = json!("a");
    std::fs::write(vdir.join("logins.json"), idx.to_string())?;
    let mut v = Vault::open(&vdir)?;
    assert_eq!(
        login(&v, "a")?.meta.fingerprint,
        secret("r-a").meta().fingerprint
    );
    v.edit(|_| Ok(()))?;
    assert_eq!(
        login(&v, "a-2")?.meta.fingerprint,
        secret("r-b").meta().fingerprint
    );
    Ok(())
}

#[test]
fn remove_saves_the_index_before_deleting_the_secret() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    let mut v = Vault::open(&vdir)?;
    v.edit(|tx| tx.put(&secret("r-a"), None, None, Some("a"), None))?;
    let id = login(&v, "a")?.id.clone();
    // Make the secrets file impossible to replace: only the index can be written.
    let res = v.edit(|tx| {
        tx.remove("a", false)?;
        std::fs::remove_file(vdir.join("secrets.json"))?;
        std::fs::create_dir(vdir.join("secrets.json"))?;
        Ok(())
    });
    assert!(res.is_err());
    let idx = read_json(&vdir.join("logins.json"))?;
    assert_eq!(
        idx["logins"].as_array().context("a list")?.len(),
        0,
        "the index went first"
    );
    std::fs::remove_dir(vdir.join("secrets.json"))?;
    // The normal path: both are gone.
    let mut v = Vault::open(&vdir)?;
    v.edit(|tx| tx.put(&secret("r-a"), None, None, Some("a"), None))?;
    v.edit(|tx| tx.remove("a", false))?;
    assert!(
        read_json(&vdir.join("secrets.json"))?
            .get(id.as_str())
            .is_none()
    );
    Ok(())
}

#[test]
fn the_vault_backs_up_its_own_files_bounded() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    let bdir = vdir.join("backups/vault");
    let mut v = Vault::open(&vdir)?;
    v.edit(|tx| tx.put(&secret("r-a"), None, None, Some("a0"), None))?;
    assert_eq!(count(&bdir, "-logins.json"), 0, "nothing to back up yet");
    v.edit(|tx| tx.put(&secret("r-b"), None, None, Some("b"), None))?;
    assert_eq!(count(&bdir, "-logins.json"), 1);
    assert_eq!(count(&bdir, "-secrets.json"), 1);
    let before = std::fs::read_dir(&bdir)?.count();
    v.edit(|tx| tx.seat_states(&[]))?;
    assert_eq!(
        std::fs::read_dir(&bdir)?.count(),
        before,
        "no change, no backup"
    );
    for i in 1..=VAULT_BACKUPS + 5 {
        let (old, new) = (format!("a{}", i - 1), format!("a{i}"));
        v.edit(|tx| tx.rename(&old, &new))?;
    }
    assert_eq!(count(&bdir, "-logins.json"), VAULT_BACKUPS);
    assert_eq!(
        count(&bdir, "-secrets.json"),
        1,
        "only changed files are copied"
    );
    // The newest copy holds the state before the last change.
    let mut names: Vec<_> = std::fs::read_dir(&bdir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with("-logins.json"))
        .collect();
    names.sort();
    let last = read_json(names.last().context("a backup")?)?;
    assert!(
        last.to_string()
            .contains(&format!("a{}", VAULT_BACKUPS + 4))
    );
    Ok(())
}

#[test]
fn seat_backups_are_kept_per_seat_and_never_overwrite() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let spare = seat(t.path(), "spare")?;
    write_seat(&work, "r-alice", "alice")?;
    write_seat(&spare, "r-bob", "bob")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.capture(&spare, Some("bob")))?;
    for name in ["carol", "dave"] {
        let acct = json!({"accountUuid": name});
        v.edit(|tx| {
            tx.put(
                &secret(&format!("r-{name}")),
                Some(acct),
                None,
                Some(name),
                None,
            )
        })?;
    }
    let rep = v.edit(|tx| tx.switch(&spare, "dave"))?;
    let spare_dir = rep.backup.context("no backup")?;
    // Three switches within one second: three copies.
    for login in ["carol", "alice", "carol"] {
        v.edit(|tx| tx.switch(&work, login))?;
    }
    // Kept under the seat's id: a new label changes nothing.
    let work_dir = t.path().join("vault/backups/seats").join(work.id.as_str());
    assert_eq!(count(&work_dir, "-credentials.json"), 3);
    assert_eq!(count(&work_dir, "-claude.json"), 3);
    for i in 0..SEAT_BACKUPS + 5 {
        let login = if i % 2 == 0 { "alice" } else { "carol" };
        v.edit(|tx| tx.switch(&work, login))?;
    }
    assert_eq!(count(&work_dir, "-credentials.json"), SEAT_BACKUPS);
    assert_eq!(count(&work_dir, "-claude.json"), SEAT_BACKUPS);
    assert_eq!(
        count(&spare_dir, "-credentials.json"),
        1,
        "a rarely switched seat keeps its backup"
    );
    Ok(())
}

/// Review H3: a vault an older swarmy wrote names seats by label (`mounted: "work"`, `seats`
/// keyed by label, no `v`). It is read in place, nothing is lost, and the labels become seat ids
/// the first time the seat is seen; a label no seat carries waits. Afterwards a new label of the
/// seat changes nothing.
#[test]
fn an_old_vault_keyed_by_labels_moves_to_seat_ids() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let vdir = t.path().join("vault");
    std::fs::create_dir_all(&vdir)?;
    // What the version before seat ids wrote (fingerprints as `credentials::LoginMeta` makes them).
    let fp = |r: &str| secret(r).meta().fingerprint;
    let login = |id: &str, label: &str, refresh: &str, mounted: &str| {
        json!({"id": id, "label": label, "account_uuid": label, "email": null, "plan": null,
               "meta": serde_json::to_value(secret(refresh).meta()).unwrap(),
               "oauth_account": null, "captured_at": "2026-11-01T00:00:00Z",
               "updated_at": "2026-11-01T00:00:00Z", "mounted": mounted})
    };
    let old = json!({
        "logins": [login("l1", "alice", "r-alice", "work"), login("l2", "bob", "r-bob", "gone")],
        "seats": {
            "work": {"fingerprint": fp("r-alice"), "account_uuid": "alice", "profile_at": null, "unverified_since": null},
            "gone": {"fingerprint": fp("r-bob"), "account_uuid": "bob", "profile_at": null, "unverified_since": null}
        }
    });
    std::fs::write(
        vdir.join("logins.json"),
        serde_json::to_string_pretty(&old)?,
    )?;
    let secrets = json!({"l1": secret("r-alice").expose(), "l2": secret("r-bob").expose()});
    std::fs::write(vdir.join("secrets.json"), secrets.to_string())?;

    // Read as it is: the labels are kept aside, not taken for ids.
    let mut v = Vault::open(&vdir)?;
    assert_eq!(v.logins().len(), 2);
    assert!(v.logins().iter().all(|l| l.mounted.is_empty()));
    assert_eq!(v.index.logins[0].mounted_label.as_deref(), Some("work"));

    // The first change that sees the seat adopts its label.
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    assert_eq!(states[&work.id].saved_as.as_deref(), Some("alice"));
    let alice = login_of(&v, "alice")?;
    assert_eq!(alice.mounted, vec![work.id.clone()]);
    assert_eq!(alice.mounted_label, None);
    let bob = login_of(&v, "bob")?;
    assert_eq!(bob.mounted, Vec::<SeatId>::new());
    assert_eq!(
        bob.mounted_label.as_deref(),
        Some("gone"),
        "waits for its seat"
    );
    let disk = read_json(&vdir.join("logins.json"))?;
    assert_eq!(disk["v"], 4);
    assert!(disk["seats"].get(work.id.as_str()).is_some(), "{disk}");
    assert!(disk["seats"].get("work").is_none());
    assert!(
        disk["legacy_seats"].get("gone").is_some(),
        "nothing dropped: {disk}"
    );
    // Both secrets are kept; alice's copy follows the seat (the mount was recognised).
    let disk_secrets = read_json(&vdir.join("secrets.json"))?;
    assert_eq!(disk_secrets["l2"], secrets["l2"]);
    assert_eq!(disk_secrets["l1"]["refreshToken"], "r-alice");
    assert_eq!(disk_secrets["l1"]["expiresAt"], 1_795_377_583_375_i64);
    // A login still mounted under an old label is not forgotten by accident.
    assert!(v.edit(|tx| tx.remove("bob", false)).is_err());

    // The seat gets another label: the vault still knows what is in it.
    let job = Seat::new("job", work.dir.clone(), work.claude_json.clone(), false);
    assert_eq!(job.id, work.id);
    v.edit(|tx| tx.capture(&job, None))?;
    assert_eq!(v.logins().len(), 2, "the same login, not a new one");
    assert_eq!(login_of(&v, "alice")?.mounted, vec![work.id.clone()]);
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&job)))?;
    assert_eq!(states[&job.id].unverified_since, None);
    Ok(())
}

fn login_of(v: &Vault, label: &str) -> anyhow::Result<Login> {
    login(v, label).cloned()
}

/// Review L3: a key that names two logins is an error naming both, never the first match.
#[test]
fn a_key_naming_two_logins_is_ambiguous() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    let acct = |uuid: &str, email: &str| crate::claude::config_json::Account {
        account_uuid: uuid.into(),
        email: Some(email.into()),
        ..Default::default()
    };
    v.edit(|tx| {
        tx.put(
            &secret("r-1"),
            None,
            Some(&acct("u1", "a@example.com")),
            Some("first"),
            None,
        )?;
        // A label equal to the other login's email is refused now (review B-M5)…
        let refused = tx.put(
            &secret("r-2"),
            None,
            Some(&acct("u2", "b@example.com")),
            Some("a@example.com"),
            None,
        );
        assert!(refused.is_err());
        let id = tx.put(
            &secret("r-2"),
            None,
            Some(&acct("u2", "b@example.com")),
            Some("second"),
            None,
        )?;
        // …but an older vault may hold one.
        if let Some(l) = tx.index.find_mut(&id) {
            l.label = "a@example.com".into();
        }
        Ok(())
    })?;
    let e = v.edit(|tx| tx.rename("a@example.com", "x")).unwrap_err();
    assert_eq!(crate::errors::code_of(&e), Some("vault-ambiguous-login"));
    let msg = e.to_string();
    assert!(
        msg.contains("first") && msg.contains("a@example.com"),
        "{msg}"
    );
    // Unique keys still work: a label, an id, an email.
    assert_eq!(login(&v, "first")?.account_uuid.as_deref(), Some("u1"));
    assert_eq!(login(&v, "b@example.com")?.label, "a@example.com");
    let e = v.edit(|tx| tx.remove("nobody", false)).unwrap_err();
    assert_eq!(crate::errors::code_of(&e), Some("vault-no-login"));
    let e = v.edit(|tx| tx.rename("first", "")).unwrap_err();
    assert_eq!(crate::errors::code_of(&e), Some("vault-label-empty"));
    Ok(())
}

/// Test-only hooks into the middle of a switch.
pub(super) mod hooks {
    use std::cell::RefCell;

    type Hook = Box<dyn FnMut()>;

    type Running = Box<dyn Fn(&crate::claude::seats::Seat) -> (usize, usize)>;

    thread_local! {
        static BEFORE_SEAT_WRITE: RefCell<Option<Hook>> = RefCell::new(None);
        static CODEX_RUNNING: RefCell<Option<Running>> = RefCell::new(None);
    }

    /// What runs in a Codex seat (open threads, processes), when a test says so; else the real
    /// look (none in a temp dir).
    pub(in crate::vault) fn codex_running(
        seat: &crate::claude::seats::Seat,
    ) -> Option<(usize, usize)> {
        CODEX_RUNNING.with(|h| h.borrow().as_ref().map(|f| f(seat)))
    }

    pub(super) fn set_codex_running(f: Option<Running>) {
        CODEX_RUNNING.with(|h| *h.borrow_mut() = f);
    }

    /// Runs the hook set on this thread, right before a switch writes the seat's credentials.
    pub(in crate::vault) fn before_seat_write() {
        BEFORE_SEAT_WRITE.with(|h| {
            if let Some(f) = h.borrow_mut().as_mut() {
                f();
            }
        });
    }

    pub(super) fn set_before_seat_write(f: Option<Hook>) {
        BEFORE_SEAT_WRITE.with(|h| *h.borrow_mut() = f);
    }
}

#[cfg(unix)]
/// A seat whose `.claude.json` lives outside its dir (as the default seat's does, in `$HOME`).
fn seat_with_home(root: &Path, label: &str) -> anyhow::Result<(Seat, PathBuf)> {
    let dir = root.join(format!(".claude-{label}"));
    let home = root.join(format!("home-{label}"));
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(&home)?;
    Ok((
        Seat::new(label, dir.clone(), home.join(".claude.json"), false),
        home,
    ))
}

#[cfg(unix)]
fn chmod(p: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Review H4: the `.claude.json` write fails after the credentials were written (here: its dir
/// cannot be written, as `$HOME` may not be for the default seat). The seat goes back to what it
/// held, and the vault still says so: the login stays mounted, the target stays free.
#[cfg(unix)]
#[test]
fn a_switch_that_fails_half_way_puts_the_seat_back() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let (work, home) = seat_with_home(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| {
        tx.put(
            &secret("r-bob"),
            Some(json!({"accountUuid": "bob"})),
            None,
            Some("bob"),
            None,
        )
    })?;
    let creds_before = std::fs::read_to_string(work.credentials_file())?;
    chmod(&home, 0o500)?;
    if std::fs::write(home.join("probe"), "").is_ok() {
        // Running as root: permissions do not stop the write, nothing to prove here.
        chmod(&home, 0o700)?;
        return Ok(());
    }
    let res = v.edit(|tx| tx.switch(&work, "bob"));
    chmod(&home, 0o700)?;
    let err = format!("{:#}", res.err().context("the switch had to fail")?);
    assert!(err.contains("the seat is as it was"), "{err}");
    assert_eq!(
        std::fs::read_to_string(work.credentials_file())?,
        creds_before,
        "the credentials are back as they were"
    );
    assert_eq!(
        read_json(&work.claude_json)?["oauthAccount"]["accountUuid"],
        "alice"
    );
    assert_eq!(login(&v, "alice")?.mounted, vec![work.id.clone()]);
    assert_eq!(login(&v, "bob")?.mounted, Vec::<SeatId>::new());
    // Nothing is half done: the same switch works once the file can be written.
    v.edit(|tx| tx.switch(&work, "bob"))?;
    assert_eq!(
        read_json(&work.credentials_file())?["claudeAiOauth"]["refreshToken"],
        "r-bob"
    );
    assert_eq!(
        read_json(&work.claude_json)?["oauthAccount"]["accountUuid"],
        "bob"
    );
    Ok(())
}

/// Review H4: a switch that died between its two writes (or a seat file put back by hand)
/// leaves a saved login in a seat while the vault calls it free. The next sync mounts it there,
/// so the one-grant-one-seat guard still holds.
#[test]
fn a_seat_holding_a_free_saved_login_gets_it_mounted() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let spare = seat(t.path(), "spare")?;
    write_seat(&work, "r-alice", "alice")?;
    write_seat(&spare, "r-carol", "carol")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.capture(&spare, Some("carol")))?;
    v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some("bob"), None))?;
    // The credentials are bob's, `.claude.json` still names alice.
    write_creds(&work, "acc-r-bob", "r-bob")?;
    let both = [work.clone(), spare.clone()];
    v.edit(|tx| tx.sync(&both))?;
    assert_eq!(login(&v, "bob")?.mounted, vec![work.id.clone()]);
    assert_eq!(login(&v, "alice")?.mounted, Vec::<SeatId>::new());
    Ok(())
}

/// Review B-H1: the same crash, with credentials as Claude Code writes them (an access token
/// fresh for hours). `.claude.json` still names alice's account, so bob's grant in her seat
/// passed for a rotation of hers and overwrote her saved tokens. A grant the vault already
/// holds is that login, never a rotation of another one.
#[test]
fn an_interrupted_switch_never_overwrites_the_previous_login() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some("bob"), None))?;
    let alice_id = login(&v, "alice")?.id.clone();
    let fresh = (chrono::Utc::now() + chrono::Duration::hours(8)).timestamp_millis();
    let creds = json!({"mcpOAuth": {}, "claudeAiOauth": {"accessToken": "acc-r-bob",
        "refreshToken": "r-bob", "expiresAt": fresh}});
    std::fs::write(work.credentials_file(), creds.to_string())?;
    v.edit(|tx| tx.sync(std::slice::from_ref(&work)))?;
    assert_eq!(
        secret_of(&v, &alice_id)?["refreshToken"],
        "r-alice",
        "alice keeps her own grant"
    );
    assert_eq!(login(&v, "bob")?.mounted, vec![work.id.clone()]);
    assert_eq!(login(&v, "alice")?.mounted, Vec::<SeatId>::new());
    Ok(())
}

/// Review M7: Claude Code refreshes the seat's token while a switch runs (after swarmy saved the
/// seat's login, before it replaced the file). The newer token is saved first, never lost.
#[test]
fn a_token_refreshed_during_a_switch_is_saved_not_overwritten() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some("bob"), None))?;
    let creds = work.credentials_file();
    let mut fired = false;
    hooks::set_before_seat_write(Some(Box::new(move || {
        if !fired {
            fired = true;
            let refreshed = json!({"mcpOAuth": {"plugin:x|1": {"accessToken": "mcp-new"}},
                "claudeAiOauth": {"accessToken": "acc-refreshed", "refreshToken": "r-alice"}});
            if let Err(e) = store::atomic_write(&creds, refreshed.to_string().as_bytes()) {
                panic!("cannot simulate Claude Code: {e}");
            }
        }
    })));
    let res = v.edit(|tx| tx.switch(&work, "bob"));
    hooks::set_before_seat_write(None);
    res?;
    let alice = login(&v, "alice")?.clone();
    assert_eq!(
        secret_of(&v, &alice.id)?["accessToken"],
        "acc-refreshed",
        "the token Claude Code wrote meanwhile was saved"
    );
    let now = read_json(&work.credentials_file())?;
    assert_eq!(now["claudeAiOauth"]["refreshToken"], "r-bob");
    assert_eq!(
        now["mcpOAuth"]["plugin:x|1"]["accessToken"], "mcp-new",
        "the MCP token written meanwhile is kept too"
    );
    Ok(())
}

/// Review M7 (defects): a file another program keeps rewriting is never overwritten blindly
/// (the eighth attempt used to write anyway), and a torn read is read again instead of failing
/// the change half way.
#[test]
fn merging_writes_never_go_over_an_unseen_version() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let path = t.path().join(".claude.json");
    std::fs::write(&path, r#"{"n": 0, "oauthAccount": {"accountUuid": "a"}}"#)?;
    let n = std::cell::Cell::new(0);
    let res = store::rewrite_merging(&path, |text| {
        n.set(n.get() + 1);
        // Claude Code writes a newer version every time swarmy has read one.
        let newer = format!(
            r#"{{"n": {}, "oauthAccount": {{"accountUuid": "a"}}}}"#,
            n.get()
        );
        store::atomic_write(&path, newer.as_bytes())?;
        config_json::with_oauth_account(text, Some(&json!({"accountUuid": "b"})))
    });
    let e = res
        .err()
        .context("a file that never holds still is not written")?;
    assert_eq!(crate::errors::code_of(&e), Some("vault-seat-busy"));
    let cj = read_json(&path)?;
    assert_eq!(cj["n"], n.get(), "the other writer's last version stands");
    assert_eq!(cj["oauthAccount"]["accountUuid"], "a");

    let calls = std::cell::Cell::new(0);
    let wrote = store::rewrite_merging(&path, |text| {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            anyhow::bail!("EOF while parsing (Claude Code is mid-write)");
        }
        config_json::with_oauth_account(text, Some(&json!({"accountUuid": "b"})))
    })?;
    assert!(wrote);
    assert_eq!(read_json(&path)?["oauthAccount"]["accountUuid"], "b");

    // Replaced only while it holds exactly what was read.
    let text = std::fs::read_to_string(&path)?;
    assert!(!store::replace_if_unchanged(
        &path,
        Some("{}"),
        Some(b"{\"x\": 1}")
    )?);
    assert_eq!(std::fs::read_to_string(&path)?, text);
    assert!(store::replace_if_unchanged(
        &path,
        Some(&text),
        Some(b"{\"x\": 1}")
    )?);
    assert_eq!(read_json(&path)?["x"], 1);
    assert_eq!(count(t.path(), ".tmp"), 0, "no temp file is left behind");
    Ok(())
}

/// Review M6: Claude Code keeps the refresh token when it refreshes (verified on 2.1.329), so a
/// login is named by it. Should a refresh ever rotate it, the login keeps its identity: same
/// account and plan, a fresh access token written by Claude Code. The saved copy follows, the
/// login stays mounted, the seat stays confirmed, and switching back writes the live grant. A
/// copied-in file with a stale access token is still another login.
#[test]
fn a_rotated_refresh_token_keeps_the_login() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    let write = |refresh: &str, expires: chrono::DateTime<Utc>| -> anyhow::Result<()> {
        let creds = json!({"mcpOAuth": {}, "claudeAiOauth": {
            "accessToken": format!("acc-{refresh}"), "refreshToken": refresh,
            "expiresAt": expires.timestamp_millis()}});
        std::fs::write(work.credentials_file(), creds.to_string())?;
        Ok(())
    };
    write("r-alice-2", Utc::now() + chrono::Duration::hours(8))?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    let st = &states[&work.id];
    assert_eq!(st.unverified_since, None, "{st:?}");
    assert_eq!(st.account_uuid.as_deref(), Some("alice"));
    assert_eq!(st.saved_as.as_deref(), Some("alice"));
    let alice = login(&v, "alice")?.clone();
    assert_eq!(alice.mounted, vec![work.id.clone()]);
    assert_eq!(secret_of(&v, &alice.id)?["refreshToken"], "r-alice-2");
    assert_eq!(v.logins().len(), 1, "no second login for the same grant");

    // Review A-H1: the pre-rotation token comes back (`accounts import` of an old file). The
    // saved login is named after its first grant, so the new one must not take its id (which
    // would have overwritten the live secret and made `forget` remove both).
    v.edit(|tx| tx.put(&secret("r-alice"), None, None, Some("old"), None))?;
    let old = login(&v, "old")?.clone();
    assert_ne!(old.id, alice.id);
    assert_eq!(secret_of(&v, &alice.id)?["refreshToken"], "r-alice-2");
    assert_eq!(secret_of(&v, &old.id)?["refreshToken"], "r-alice");
    v.edit(|tx| tx.remove("old", true))?;
    assert!(
        find(&v, "alice").is_some(),
        "forgetting one keeps the other"
    );

    // Away and back: the live, rotated grant goes back into the seat.
    v.edit(|tx| {
        tx.put(
            &secret("r-bob"),
            Some(json!({"accountUuid": "bob"})),
            None,
            Some("bob"),
            None,
        )
    })?;
    v.edit(|tx| tx.switch(&work, "bob"))?;
    v.edit(|tx| tx.switch(&work, "alice"))?;
    assert_eq!(
        read_json(&work.credentials_file())?["claudeAiOauth"]["refreshToken"],
        "r-alice-2"
    );
    assert_eq!(v.logins().len(), 2);

    // An old copy of the same account (its access token long expired) is not a rotation.
    write("r-alice-old", Utc::now() - chrono::Duration::hours(1))?;
    v.edit(|tx| tx.sync(std::slice::from_ref(&work)))?;
    let alice = login(&v, "alice")?.clone();
    assert_eq!(alice.mounted, Vec::<SeatId>::new());
    assert_eq!(secret_of(&v, &alice.id)?["refreshToken"], "r-alice-2");
    Ok(())
}

/// Seen on Claude Code 2.1.332: right after a switch, the running Claude Code refreshes the new
/// grant and rotates its refresh token. The seat still holds the login the vault put there, so
/// the switch stays dated (the index and the status line date the login change by it); another
/// login signed in there ends it.
#[test]
fn a_switch_stays_dated_across_a_rotation_of_its_login() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let spare = seat(t.path(), "spare")?;
    write_seat(&work, "r-alice", "alice")?;
    write_seat(&spare, "r-bob", "bob")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.capture(&spare, Some("bob")))?;
    v.edit(|tx| tx.switch(&work, "bob"))?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    let st = &states[&work.id];
    assert_eq!(st.account_uuid.as_deref(), Some("bob"), "{st:?}");
    let switched = st.switched_at;
    assert!(switched.is_some(), "{st:?}");

    let write = |refresh: &str| -> anyhow::Result<()> {
        let creds = json!({"claudeAiOauth": {
            "accessToken": format!("acc-{refresh}"), "refreshToken": refresh,
            "expiresAt": (Utc::now() + chrono::Duration::hours(8)).timestamp_millis()}});
        std::fs::write(work.credentials_file(), creds.to_string())?;
        Ok(())
    };
    write("r-bob-2")?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    let st = &states[&work.id];
    assert_eq!(
        st.saved_as.as_deref(),
        Some("bob"),
        "the rotation was followed"
    );
    assert_eq!(st.switched_at, switched, "{st:?}");
    // Read paths see the same.
    assert_eq!(
        v.seat_states(std::slice::from_ref(&work))?[&work.id].switched_at,
        switched
    );

    // A `/login` of someone else: not the vault's login, no switch of the vault's any more.
    write_seat(&work, "r-carol", "carol")?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    assert_eq!(states[&work.id].switched_at, None, "{:?}", states[&work.id]);
    Ok(())
}

/// Seen on Claude Code 2.1.332: one saved login in two seats, and the Claude Code of one of them
/// refreshes it and rotates the refresh token. The other seat still holds the token from before,
/// which looks like a rotation too (same account, same grant expiry, an access token that was
/// fresh when written), so the saved login used to follow each seat in turn, flipping between
/// the two tokens on every look (journal and backups filled with "rotated"), and kept whichever
/// seat was looked at last, the spent token as often as the live one. A rotation only moves
/// forward: the login follows the seat whose access token is newer, and the other seat's older
/// token is another login ("not saved"), in whichever order the seats are looked at.
#[test]
fn one_login_in_two_seats_follows_the_rotation_and_never_flips_back() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let spare = seat(t.path(), "spare")?;
    let grant_expires = Utc::now() + chrono::Duration::days(20);
    let write = |s: &Seat, refresh: &str, access_expires: chrono::DateTime<Utc>| {
        let creds = json!({"claudeAiOauth": {
            "accessToken": format!("acc-{refresh}"), "refreshToken": refresh,
            "expiresAt": access_expires.timestamp_millis(),
            "refreshTokenExpiresAt": grant_expires.timestamp_millis()}});
        std::fs::write(s.credentials_file(), creds.to_string())
    };
    write_seat(&spare, "r-bob", "bob")?;
    write(&work, "r-1", Utc::now() + chrono::Duration::hours(2))?;
    let cj = json!({"oauthAccount": {"accountUuid": "alice", "emailAddress": "alice@example.com"}});
    std::fs::write(&work.claude_json, cj.to_string())?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.capture(&spare, Some("bob")))?;
    v.edit(|tx| tx.switch(&spare, "alice"))?;
    let both = [work.clone(), spare.clone()];
    v.edit(|tx| tx.seat_states(&both))?;
    assert_eq!(login(&v, "alice")?.mounted.len(), 2, "one login, two seats");

    // The work seat's Claude Code refreshes: a new refresh token, a later access token.
    write(&work, "r-2", Utc::now() + chrono::Duration::hours(8))?;
    let journal_file = v.dir.join("journal.log");
    let journal = || std::fs::read_to_string(&journal_file).unwrap_or_default();
    let before = journal().matches("rotated").count();
    for order in [[&work, &spare], [&spare, &work], [&work, &spare]] {
        let seats = [order[0].clone(), order[1].clone()];
        let states = v.edit(|tx| tx.seat_states(&seats))?;
        assert_eq!(states[&work.id].saved_as.as_deref(), Some("alice"));
        assert_eq!(
            states[&spare.id].saved_as, None,
            "the older token is not saved"
        );
        let alice = login(&v, "alice")?.clone();
        assert_eq!(secret_of(&v, &alice.id)?["refreshToken"], "r-2");
        assert_eq!(alice.mounted, vec![work.id.clone()]);
    }
    assert_eq!(
        journal().matches("rotated").count() - before,
        1,
        "{}",
        journal()
    );
    Ok(())
}

/// A grant as Claude Code 2.1.332 writes it after a refresh: a fresh access token, the grant's
/// own expiry, and (`named`) the account the grant names itself. The profile in `.claude.json`
/// was read a day ago and names `acc-1/org-1`.
fn write_grant(
    s: &Seat,
    refresh: &str,
    refresh_expires: chrono::DateTime<Utc>,
    named: bool,
) -> anyhow::Result<()> {
    let now = Utc::now();
    let mut oauth = json!({
        "accessToken": format!("acc-{refresh}"),
        "refreshToken": refresh,
        "expiresAt": (now + chrono::Duration::hours(8)).timestamp_millis(),
        "refreshTokenExpiresAt": refresh_expires.timestamp_millis(),
        "subscriptionType": "max",
        "rateLimitTier": "default_claude_max_20x",
    });
    if named {
        oauth["accountUuid"] = json!("acc-1");
        oauth["organizationUuid"] = json!("org-1");
    }
    std::fs::write(
        s.credentials_file(),
        json!({"claudeAiOauth": oauth}).to_string(),
    )?;
    let profile = json!({"oauthAccount": {
        "accountUuid": "acc-1", "organizationUuid": "org-1", "emailAddress": "sam@example.com",
        "profileFetchedAt": (now - chrono::Duration::days(1)).timestamp_millis()}});
    std::fs::write(&s.claude_json, profile.to_string())?;
    Ok(())
}

/// Seen on Claude Code 2.1.332: a rotation writes the grant's `refreshTokenExpiresAt` again from
/// its own clock, here 1.2 s earlier than the saved copy. It is still the same grant: the login
/// follows it, stays mounted, and the seat stays saved (it used to show "not saved" and leave the
/// login mounted nowhere).
#[test]
fn a_rotation_that_moves_the_grant_expiry_back_a_second_is_followed() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let expiry = Utc::now() + chrono::Duration::days(28);
    write_grant(&work, "r-1", expiry, false)?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("sam")))?;
    assert_eq!(
        login(&v, "sam")?.account_uuid.as_deref(),
        Some("acc-1/org-1")
    );

    write_grant(
        &work,
        "r-2",
        expiry - chrono::Duration::milliseconds(1_181),
        false,
    )?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    let st = &states[&work.id];
    assert_eq!(st.saved_as.as_deref(), Some("sam"), "{st:?}");
    assert_eq!(st.unverified_since, None, "{st:?}");
    let sam = login(&v, "sam")?.clone();
    assert_eq!(sam.mounted, vec![work.id.clone()]);
    assert_eq!(secret_of(&v, &sam.id)?["refreshToken"], "r-2");

    // A grant that expires a day earlier is not this one, even of the same account.
    write_grant(&work, "r-3", expiry - chrono::Duration::days(1), false)?;
    v.edit(|tx| tx.sync(std::slice::from_ref(&work)))?;
    let sam = login(&v, "sam")?.clone();
    assert!(sam.mounted.is_empty());
    assert_eq!(secret_of(&v, &sam.id)?["refreshToken"], "r-2");
    Ok(())
}

/// A grant that names its account confirms the seat without a newer profile, and its rotation is
/// followed even for a login saved with no account (saved while the seat was unconfirmed, which
/// used to make every later rotation look like another login). The login then takes the profile.
#[test]
fn a_grant_naming_its_account_confirms_the_seat_and_its_rotation() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let expiry = Utc::now() + chrono::Duration::days(28);
    write_grant(&work, "r-1", expiry, true)?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    let first = read_seat(&work)?.secret.context("no grant")?;
    assert_eq!(first.meta().account.as_deref(), Some("acc-1/org-1"));
    // Saved with no account, as an unconfirmed seat saves it.
    v.edit(|tx| tx.put(&first, None, None, Some("sam"), Some(&work.id)))?;
    assert_eq!(login(&v, "sam")?.account_uuid, None);

    write_grant(&work, "r-2", expiry, true)?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    let st = &states[&work.id];
    assert_eq!(st.unverified_since, None, "{st:?}");
    assert_eq!(st.account_uuid.as_deref(), Some("acc-1/org-1"));
    assert_eq!(st.saved_as.as_deref(), Some("sam"));
    let sam = login(&v, "sam")?.clone();
    assert_eq!(sam.mounted, vec![work.id.clone()]);
    assert_eq!(sam.account_uuid.as_deref(), Some("acc-1/org-1"));
    assert_eq!(sam.email.as_deref(), Some("sam@example.com"));
    assert_eq!(secret_of(&v, &sam.id)?["refreshToken"], "r-2");
    Ok(())
}

/// A rotation the vault did not follow left the saved login mounted nowhere and the seat holding
/// a grant the vault does not know. The next look takes the login back: same grant lifetime,
/// same account, the only such login. Two such logins, or one whose grant expires at another
/// time, are left alone.
#[test]
fn a_login_left_unmounted_by_a_missed_rotation_is_taken_back() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let expiry = Utc::now() + chrono::Duration::days(28);
    let grant = |refresh: &str, exp: chrono::DateTime<Utc>| {
        Secret::new(
            json!({"accessToken": format!("acc-{refresh}"), "refreshToken": refresh,
            "refreshTokenExpiresAt": exp.timestamp_millis(), "subscriptionType": "max",
            "accountUuid": "acc-1", "organizationUuid": "org-1"}),
        )
    };
    let mut v = Vault::open(&t.path().join("vault"))?;
    let acct = json!({"accountUuid": "acc-1", "organizationUuid": "org-1"});
    v.edit(|tx| {
        tx.put(
            &grant("r-1", expiry),
            Some(acct.clone()),
            None,
            Some("sam"),
            None,
        )
    })?;
    v.edit(|tx| {
        tx.put(
            &grant("r-other", expiry - chrono::Duration::days(3)),
            Some(acct.clone()),
            None,
            Some("older"),
            None,
        )
    })?;

    write_grant(&work, "r-2", expiry + chrono::Duration::seconds(1), true)?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    assert_eq!(states[&work.id].saved_as.as_deref(), Some("sam"));
    let sam = login(&v, "sam")?.clone();
    assert_eq!(sam.mounted, vec![work.id.clone()]);
    assert_eq!(secret_of(&v, &sam.id)?["refreshToken"], "r-2");
    assert!(login(&v, "older")?.mounted.is_empty());

    // Two candidates with the same lifetime: which one it is cannot be told.
    let later = expiry + chrono::Duration::days(1);
    v.edit(|tx| {
        tx.put(
            &grant("r-a", later),
            Some(acct.clone()),
            None,
            Some("a"),
            None,
        )
    })?;
    v.edit(|tx| {
        tx.put(
            &grant("r-b", later),
            Some(acct.clone()),
            None,
            Some("b"),
            None,
        )
    })?;
    let other = seat(t.path(), "other")?;
    write_grant(&other, "r-c", later, true)?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&other)))?;
    assert_eq!(states[&other.id].saved_as, None);
    assert!(login(&v, "a")?.mounted.is_empty());
    assert!(login(&v, "b")?.mounted.is_empty());
    Ok(())
}

/// Signing out leaves `claudeAiOauth` with empty tokens: no login to save (it used to be saved
/// as one, named by the fingerprint of an empty string), and the login that was there is no
/// longer mounted.
#[test]
fn a_signed_out_seat_holds_no_login() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    let creds = json!({"claudeAiOauth": {"accessToken": "", "refreshToken": "",
        "refreshTokenExpiresAt": 1_797_969_583_375_i64, "subscriptionType": "max"}});
    std::fs::write(work.credentials_file(), creds.to_string())?;
    assert!(read_seat(&work)?.secret.is_none());
    assert_eq!(v.edit(|tx| tx.capture(&work, None))?, None);
    assert!(login(&v, "alice")?.mounted.is_empty());
    assert_eq!(v.logins().len(), 1);
    Ok(())
}

/// Review L4: reading the seats' state takes no lock and writes nothing, so a report or the
/// dashboard never waits for (or fails on) another swarmy's change. The dashboard does the
/// bookkeeping on the side, only when the vault is free.
#[test]
fn reading_seat_states_takes_no_lock_and_writes_nothing() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&vdir)?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    // Something to keep current: Claude Code refreshed the access token.
    write_creds(&work, "acc-new", "r-alice")?;
    let files = || -> anyhow::Result<(String, String)> {
        Ok((
            std::fs::read_to_string(vdir.join("logins.json"))?,
            std::fs::read_to_string(vdir.join("secrets.json"))?,
        ))
    };
    let before = files()?;
    // Another swarmy is changing the vault.
    let held = store::Lock::acquire(&vdir.join("lock"), std::time::Duration::ZERO)?;
    let t0 = std::time::Instant::now();
    let states = v.seat_states(std::slice::from_ref(&work))?;
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "it waited"
    );
    assert_eq!(states[&work.id].saved_as.as_deref(), Some("alice"));
    assert_eq!(states[&work.id].account_uuid.as_deref(), Some("alice"));
    assert!(
        v.try_edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?
            .is_none(),
        "busy: skipped at once"
    );
    assert_eq!(files()?, before, "nothing was written");
    drop(held);
    assert!(
        v.try_edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?
            .is_some()
    );
    let alice = login(&v, "alice")?.clone();
    assert_eq!(secret_of(&v, &alice.id)?["accessToken"], "acc-new");
    Ok(())
}

#[test]
fn multi_org_logins_stay_distinct_and_do_not_trigger_rotation() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    let def = seat(t.path(), "default")?;
    let work = seat(t.path(), "work")?;
    let write_org_seat = |s: &Seat,
                          refresh: &str,
                          org: &str,
                          sub: &str,
                          org_tier: Option<&str>|
     -> anyhow::Result<()> {
        let now = Utc::now();
        let creds = json!({
            "claudeAiOauth": {
                "accessToken": format!("acc-{refresh}"),
                "refreshToken": refresh,
                "expiresAt": (now + chrono::Duration::hours(6)).timestamp_millis(),
                "refreshTokenExpiresAt": (now + chrono::Duration::days(25)).timestamp_millis(),
                "subscriptionType": sub,
                "rateLimitTier": "default_claude_max_5x"
            }
        });
        std::fs::write(s.credentials_file(), creds.to_string())?;
        let mut oauth = json!({
            "accountUuid": "acc-same",
            "organizationUuid": org,
            "emailAddress": "dev@example.com",
            "userRateLimitTier": "default_claude_max_5x",
            "profileFetchedAt": now.timestamp_millis()
        });
        if let Some(ot) = org_tier {
            oauth["organizationRateLimitTier"] = json!(ot);
        }
        std::fs::write(&s.claude_json, json!({"oauthAccount": oauth}).to_string())?;
        Ok(())
    };
    write_org_seat(&def, "r-team", "org-team", "team", Some("default_raven"))?;
    write_org_seat(&work, "r-max", "org-max", "max", None)?;

    let mut v = Vault::open(&vdir)?;
    v.edit(|tx| tx.capture(&def, Some("dev-team")))?;
    v.edit(|tx| tx.capture(&work, Some("dev-max")))?;
    let l_team = login(&v, "dev-team")?.clone();
    let l_max = login(&v, "dev-max")?.clone();
    assert_eq!(l_team.account_uuid.as_deref(), Some("acc-same/org-team"));
    assert_eq!(l_max.account_uuid.as_deref(), Some("acc-same/org-max"));
    assert_eq!(l_team.plan.as_deref(), Some("default_raven"));
    assert_eq!(l_max.plan.as_deref(), Some("default_claude_max_5x"));

    // Logging into org-max inside the `default` seat (same accountUuid, different org, fresh token)
    // must NOT be treated as a token rotation of `dev-team`.
    write_org_seat(
        &def,
        "r-other-org",
        "org-other",
        "team",
        Some("default_raven"),
    )?;
    v.edit(|tx| tx.sync(std::slice::from_ref(&def)))?;
    let l_team_after = login(&v, "dev-team")?;
    assert_eq!(
        l_team_after.meta.fingerprint, l_team.meta.fingerprint,
        "dev-team was not overwritten by another organization's grant"
    );
    assert!(
        l_team_after.mounted.is_empty(),
        "dev-team unmounted cleanly"
    );
    Ok(())
}

#[test]
fn v2_logins_upgrade_bare_account_uuid_and_plan_from_oauth_account() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vdir = t.path().join("vault");
    std::fs::create_dir_all(&vdir)?;
    let now = Utc::now().to_rfc3339();
    let v2 = json!({
        "v": 2,
        "logins": [{
            "id": "l0123456789",
            "label": "team-login",
            "account_uuid": "acc-1",
            "email": "dev@example.com",
            "plan": "default_claude_max_5x",
            "meta": {
                "fingerprint": "0123456789abcdef",
                "subscription_type": "team",
                "rate_limit_tier": "default_claude_max_5x",
                "access_expires_at": null,
                "refresh_expires_at": null,
                "scopes": []
            },
            "oauth_account": {
                "accountUuid": "acc-1",
                "organizationUuid": "org-team",
                "organizationRateLimitTier": "default_raven",
                "userRateLimitTier": "default_claude_max_5x"
            },
            "captured_at": now,
            "updated_at": now,
            "mounted": null
        }],
        "seats": {}
    });
    std::fs::write(vdir.join("logins.json"), v2.to_string())?;
    let v = Vault::open(&vdir)?;
    let l = login(&v, "team-login")?;
    assert_eq!(l.account_uuid.as_deref(), Some("acc-1/org-team"));
    assert_eq!(l.plan.as_deref(), Some("default_raven"));
    Ok(())
}

/// Review A6: a login saved without a profile (`oauth_account = None`) leaves `.claude.json`
/// naming the previous account after a switch; the seat must not count as verified under it.
#[test]
fn a_switch_to_a_login_without_profile_leaves_the_seat_unverified() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some("bob"), None))?;
    v.edit(|tx| tx.switch(&work, "bob"))?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&work)))?;
    let st = &states[&work.id];
    assert!(st.unverified_since.is_some(), "{st:?}");
    assert_eq!(st.account_uuid, None);
    Ok(())
}

/// One login in two seats: the user asks for it when a second seat has to keep working. The
/// seat's own login is saved first; the shared one stays in both and stays listed.
#[test]
fn one_login_goes_into_several_seats() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    let lab = seat(t.path(), "lab")?;
    write_seat(&work, "r-alice", "alice")?;
    write_seat(&lab, "r-unsaved", "carol")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    let rep = v.edit(|tx| tx.switch(&lab, "alice"))?;
    assert_eq!(rep.shared_with, ["work"]);
    assert_eq!(
        rep.auto_saved.as_deref(),
        Some("lab"),
        "lab's login saved first"
    );
    let alice = login(&v, "alice")?.clone();
    assert_eq!(alice.mounted, vec![work.id.clone(), lab.id.clone()]);
    assert_eq!(
        secret_of(&v, &login(&v, "lab")?.id)?["refreshToken"],
        "r-unsaved"
    );

    // Each seat refreshes its own access token: the vault keeps the newer one and does not
    // rewrite itself when the older copy is seen again.
    let creds = |access: &str, exp: i64| {
        json!({"claudeAiOauth": {"accessToken": access, "refreshToken": "r-alice", "expiresAt": exp}})
            .to_string()
    };
    std::fs::write(work.credentials_file(), creds("acc-w", 1_900_000_000_000))?;
    std::fs::write(lab.credentials_file(), creds("acc-l", 1_900_000_100_000))?;
    let both = [work.clone(), lab.clone()];
    v.edit(|tx| tx.sync(&both))?;
    assert_eq!(secret_of(&v, &alice.id)?["accessToken"], "acc-l");
    let logins = std::fs::read_to_string(v.dir.join("logins.json"))?;
    let secrets = std::fs::read_to_string(v.dir.join("secrets.json"))?;
    v.edit(|tx| tx.sync(&both))?;
    assert_eq!(std::fs::read_to_string(v.dir.join("logins.json"))?, logins);
    assert_eq!(
        std::fs::read_to_string(v.dir.join("secrets.json"))?,
        secrets
    );

    // Moving `work` to another login leaves alice in `lab`.
    v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some("bob"), None))?;
    v.edit(|tx| tx.switch(&work, "bob"))?;
    assert_eq!(login(&v, "alice")?.mounted, vec![lab.id.clone()]);
    // Forgetting a login in use names every seat it is in.
    v.edit(|tx| tx.switch(&work, "alice"))?;
    let e = v
        .edit(|tx| tx.remove("alice", false))
        .unwrap_err()
        .to_string();
    assert!(e.contains("lab, work") || e.contains("work, lab"), "{e}");
    Ok(())
}

/// A login only goes into seats of its harness.
#[test]
fn a_login_never_goes_into_another_harness() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let agy = agy_seat(t.path(), "agy")?;
    write_agy(&agy, "ya-sam", "sam@example.com")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    let e = v.edit(|tx| tx.switch(&agy, "alice")).unwrap_err();
    assert_eq!(crate::errors::code_of(&e), Some("vault-wrong-harness"));
    Ok(())
}

fn agy_seat(root: &Path, label: &str) -> anyhow::Result<Seat> {
    let dir = root.join(format!(".gemini-{label}"));
    std::fs::create_dir_all(&dir)?;
    Ok(Seat::agy(label, dir))
}

fn write_agy(s: &Seat, refresh: &str, email: &str) -> anyhow::Result<()> {
    let v = json!({
        "token": {"access_token": format!("acc-{refresh}"), "token_type": "Bearer",
                  "refresh_token": refresh, "expiry": "2026-11-24T10:00:00Z", "expires_in": 3599},
        "auth_method": "consumer",
        "id_token_email": email
    });
    std::fs::write(s.credentials_file(), v.to_string())?;
    Ok(())
}

/// Antigravity: capture, switch (the token file replaced whole, backed up first), and the
/// account read from the token itself, never unverified.
#[test]
fn agy_logins_are_saved_and_switched() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let main = agy_seat(t.path(), "agy")?;
    let other = agy_seat(t.path(), "agy-work")?;
    write_agy(&main, "ya-sam", "sam@example.com")?;
    write_agy(&other, "ya-team", "team@example.com")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&main, Some("sam")))?;
    let sam = login(&v, "sam")?.clone();
    assert_eq!(sam.harness, crate::core::harness::Harness::Agy);
    assert_eq!(sam.account_uuid.as_deref(), Some("agy:sam@example.com"));

    let rep = v.edit(|tx| tx.switch(&other, "sam"))?;
    assert_eq!(rep.auto_saved.as_deref(), Some("agy-work"));
    assert_eq!(
        read_json(&other.credentials_file())?["token"]["refresh_token"],
        "ya-sam"
    );
    assert!(std::fs::read_dir(rep.backup.context("no backup")?)?.count() >= 1);
    let states = v.edit(|tx| tx.seat_states(&[main.clone(), other.clone()]))?;
    for s in [&main, &other] {
        let st = &states[&s.id];
        assert_eq!(st.unverified_since, None);
        assert_eq!(st.account_uuid.as_deref(), Some("agy:sam@example.com"));
        assert_eq!(st.saved_as.as_deref(), Some("sam"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(other.credentials_file())?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    // And back: the saved team login returns intact.
    v.edit(|tx| tx.switch(&other, "agy-work"))?;
    assert_eq!(
        read_json(&other.credentials_file())?["token"]["refresh_token"],
        "ya-team"
    );
    Ok(())
}

/// `logins.json` v3 (one seat or `null` in `mounted`, no harness) reads as v4.
#[test]
fn v3_logins_read_with_one_seat_and_claude_harness() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let dir = t.path().join("vault");
    std::fs::create_dir_all(&dir)?;
    let l = |id: &str, mounted: Value| {
        json!({"id": id, "label": id, "account_uuid": null, "email": null, "plan": null,
               "meta": {"fingerprint": id, "subscription_type": null, "rate_limit_tier": null,
                        "access_expires_at": null, "refresh_expires_at": null, "scopes": []},
               "oauth_account": null, "captured_at": "2026-11-01T00:00:00Z",
               "updated_at": "2026-11-01T00:00:00Z", "mounted": mounted})
    };
    std::fs::write(
        dir.join("logins.json"),
        json!({"v": 3, "logins": [l("a", json!("0123456789abcdef")), l("b", Value::Null)], "seats": {}})
            .to_string(),
    )?;
    let v = Vault::open(&dir)?;
    let a = login(&v, "a")?;
    assert_eq!(a.mounted, vec![SeatId::from_stored("0123456789abcdef")]);
    assert_eq!(a.harness, crate::core::harness::Harness::Claude);
    assert!(login(&v, "b")?.mounted.is_empty());
    Ok(())
}

#[test]
fn a_label_names_one_login_per_harness() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let claude = seat(t.path(), "work")?;
    write_seat(&claude, "rt-sam", "acc-sam")?;
    let agy = agy_seat(t.path(), "agy")?;
    let agy_other = agy_seat(t.path(), "agy-2")?;
    write_agy(&agy, "ya-sam", "sam@example.com")?;
    write_agy(&agy_other, "ya-team", "team@example.com")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&claude, Some("sam")))?;
    // The same label for a login of another harness: fine.
    v.edit(|tx| tx.capture(&agy, Some("sam")))?;
    // Twice in one harness: refused, and the message names the harness.
    let err = v
        .edit(|tx| tx.capture(&agy_other, Some("sam")))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("Antigravity login labelled 'sam' already exists"),
        "{err}"
    );
    // A bare label is ambiguous; with the harness it names one.
    let err = v.find("sam").unwrap_err().to_string();
    assert!(
        err.contains("sam (Claude Code)") && err.contains("sam (Antigravity)"),
        "{err}"
    );
    let agy_sam = v
        .find_in("sam", Some(crate::core::harness::Harness::Agy))?
        .clone();
    assert_eq!(agy_sam.account_uuid.as_deref(), Some("agy:sam@example.com"));
    // A switch by label takes the seat's harness.
    v.edit(|tx| tx.switch(&agy_other, "sam"))?;
    assert_eq!(
        read_json(&agy_other.credentials_file())?["token"]["refresh_token"],
        "ya-sam"
    );
    // A new login named after its seat gets a free label within its harness only.
    let claude2 = seat(t.path(), "agy")?;
    write_seat(&claude2, "rt-other", "acc-other")?;
    v.edit(|tx| tx.capture(&claude2, None))?;
    let named: Vec<String> = v
        .logins()
        .iter()
        .filter(|l| l.harness == crate::core::harness::Harness::Claude)
        .map(|l| l.label.clone())
        .collect();
    assert!(named.contains(&"agy".to_string()), "{named:?}");
    Ok(())
}

fn codex_seat(root: &Path, label: &str) -> anyhow::Result<Seat> {
    let dir = root.join(format!(".codex-{label}"));
    std::fs::create_dir_all(&dir)?;
    Ok(Seat::codex(label, dir))
}

/// A Codex `auth.json`: a ChatGPT login of `user` in workspace `ws`, its access token fresh.
fn write_codex(s: &Seat, refresh: &str, user: &str, ws: &str) -> anyhow::Result<()> {
    use crate::core::jwt::b64url;
    let jwt = |claims: Value| {
        format!(
            "{}.{}.sig",
            b64url(b"{}"),
            b64url(claims.to_string().as_bytes())
        )
    };
    let exp = (chrono::Utc::now() + chrono::Duration::days(10)).timestamp();
    let v = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": jwt(json!({"email": format!("{user}@example.com"),
                "https://api.openai.com/auth": {"chatgpt_user_id": user, "chatgpt_account_id": ws,
                                                "chatgpt_plan_type": "plus"}})),
            "access_token": jwt(json!({"exp": exp})),
            "refresh_token": refresh,
            "account_id": ws
        },
        "last_refresh": "2026-11-24T10:00:00Z"
    });
    std::fs::write(s.credentials_file(), v.to_string())?;
    Ok(())
}

fn codex_refresh(s: &Seat) -> anyhow::Result<Value> {
    Ok(read_json(&s.credentials_file())?["tokens"]["refresh_token"].clone())
}

#[test]
fn codex_logins_are_switched_followed_and_kept_to_one_seat() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let main = codex_seat(t.path(), "codex")?;
    let other = codex_seat(t.path(), "codex-work")?;
    write_codex(&main, "rt-sam-1", "user-sam", "ws-1")?;
    write_codex(&other, "rt-team-1", "user-team", "ws-2")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&main, Some("sam")))?;
    let sam = login(&v, "sam")?.clone();
    assert_eq!(sam.harness, crate::core::harness::Harness::Codex);
    assert!(sam.account_uuid.is_some());
    assert_eq!(
        sam.plan
            .as_deref()
            .or(sam.meta.subscription_type.as_deref()),
        Some("plus")
    );

    // Codex refreshed in the seat: a new refresh token of the same account. The saved login
    // follows it (the old one is spent), and stays in the seat.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_codex(&main, "rt-sam-2", "user-sam", "ws-1")?;
    let states = v.edit(|tx| tx.seat_states(std::slice::from_ref(&main)))?;
    assert_eq!(states[&main.id].saved_as.as_deref(), Some("sam"));
    assert_eq!(
        secret_of(&v, &sam.id)?["tokens"]["refresh_token"],
        "rt-sam-2"
    );

    // One seat only: sam is in `codex`, so `codex-work` cannot take it too.
    let err = v
        .edit(|tx| tx.switch(&other, "sam"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("one seat only"), "{err}");
    assert_eq!(codex_refresh(&other)?, "rt-team-1", "nothing was written");

    // Moving it instead: the two seats swap their logins; each seat's copy is saved first.
    let seats = [main.clone(), other.clone()];
    let rep = v.edit(|tx| tx.switch_moving(&other, "sam", &seats))?;
    assert_eq!(rep.to, "sam");
    assert_eq!(
        rep.auto_saved.as_deref(),
        Some("codex-work"),
        "the team login was not saved yet"
    );
    assert_eq!(
        rep.moved,
        vec![("codex".to_string(), Some("codex-work".to_string()))]
    );
    assert_eq!(codex_refresh(&other)?, "rt-sam-2", "sam's newest token");
    assert_eq!(codex_refresh(&main)?, "rt-team-1");
    let sam = login(&v, "sam")?.clone();
    assert_eq!(sam.mounted, vec![other.id.clone()], "one seat only");
    // And with nothing to give back: the seat it leaves is left without a login (saved).
    let empty = codex_seat(t.path(), "codex-empty")?;
    let seats = [main.clone(), other.clone(), empty.clone()];
    let rep = v.edit(|tx| tx.switch_moving(&empty, "sam", &seats))?;
    assert_eq!(rep.moved, vec![("codex-work".to_string(), None)]);
    assert!(!other.credentials_file().exists());
    assert_eq!(codex_refresh(&empty)?, "rt-sam-2");
    assert_eq!(login(&v, "sam")?.mounted, vec![empty.id.clone()]);
    Ok(())
}

/// Reviews A-H2, B-M1: a Codex move writes three times (empty the seat the login leaves, put it
/// into the seat asked for, give that seat's old login to the seat emptied). Between any two
/// writes no refresh token is in two seats, and every save keeps the vault's invariants
/// (`Index::violations`, checked by `save` in debug builds).
#[test]
fn a_codex_move_never_puts_one_token_in_two_seats() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let main = codex_seat(t.path(), "codex")?;
    let other = codex_seat(t.path(), "codex-work")?;
    write_codex(&main, "rt-sam-1", "user-sam", "ws-1")?;
    write_codex(&other, "rt-team-1", "user-team", "ws-2")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&main, Some("sam")))?;
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let (a, b, log) = (main.clone(), other.clone(), seen.clone());
    hooks::set_before_seat_write(Some(Box::new(move || {
        let rt = |s: &Seat| {
            codex_refresh(s)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
        };
        log.borrow_mut().push((rt(&a), rt(&b)));
    })));
    let seats = [main.clone(), other.clone()];
    let rep = v.edit(|tx| tx.switch_moving(&other, "sam", &seats));
    hooks::set_before_seat_write(None);
    let rep = rep?;
    assert_eq!(
        rep.moved,
        vec![("codex".to_string(), Some("codex-work".to_string()))]
    );
    let seen = seen.borrow();
    assert_eq!(seen.len(), 3, "{seen:?}");
    for (m, o) in seen.iter() {
        assert!(m.is_none() || m != o, "one token in two seats: {seen:?}");
    }
    assert_eq!(codex_refresh(&other)?, "rt-sam-1");
    assert_eq!(codex_refresh(&main)?, "rt-team-1");
    Ok(())
}

/// Review B-M2: Codex refreshes the login in the seat it leaves while the move runs. The newest
/// token is saved and is the one moved; the seat is not emptied over a copy the vault lacks.
#[test]
fn a_codex_refresh_during_a_move_is_saved_and_moved() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let main = codex_seat(t.path(), "codex")?;
    let other = codex_seat(t.path(), "codex-work")?;
    write_codex(&main, "rt-sam-1", "user-sam", "ws-1")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&main, Some("sam")))?;
    let fired = std::rc::Rc::new(std::cell::Cell::new(false));
    let (m, f) = (main.clone(), fired.clone());
    hooks::set_before_seat_write(Some(Box::new(move || {
        if !f.replace(true) {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = write_codex(&m, "rt-sam-2", "user-sam", "ws-1");
        }
    })));
    let seats = [main.clone(), other.clone()];
    let rep = v.edit(|tx| tx.switch_moving(&other, "sam", &seats));
    hooks::set_before_seat_write(None);
    let rep = rep?;
    assert!(fired.get());
    assert_eq!(rep.moved, vec![("codex".to_string(), None)]);
    assert!(!main.credentials_file().exists());
    assert_eq!(
        codex_refresh(&other)?,
        "rt-sam-2",
        "the refreshed token moved"
    );
    let sam = login(&v, "sam")?.clone();
    assert_eq!(
        secret_of(&v, &sam.id)?["tokens"]["refresh_token"],
        "rt-sam-2"
    );
    Ok(())
}

/// Reviews A-H3, B-M3: the vault itself refuses to switch a Codex seat while Codex runs in it
/// or in the seat the login moves out of, looked at under its lock right before the writes.
#[test]
fn a_running_codex_stops_the_switch_inside_the_vault() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let main = codex_seat(t.path(), "codex")?;
    let other = codex_seat(t.path(), "codex-work")?;
    write_codex(&main, "rt-sam-1", "user-sam", "ws-1")?;
    write_codex(&other, "rt-team-1", "user-team", "ws-2")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&main, Some("sam")))?;
    v.edit(|tx| tx.capture(&other, Some("team")))?;
    // A third login in no seat: saved from a spare seat that was then emptied.
    let spare = codex_seat(t.path(), "codex-spare")?;
    write_codex(&spare, "rt-free-1", "user-free", "ws-3")?;
    v.edit(|tx| tx.capture(&spare, Some("free")))?;
    std::fs::remove_file(spare.credentials_file())?;
    v.edit(|tx| tx.sync(std::slice::from_ref(&spare)))?;
    assert!(login(&v, "free")?.mounted.is_empty());
    hooks::set_codex_running(Some(Box::new(|s: &Seat| {
        if s.label == "codex" { (0, 1) } else { (0, 0) }
    })));
    let seats = [main.clone(), other.clone()];
    // Codex runs in the seat the login moves out of, then in the seat switched.
    let moving = v.edit(|tx| tx.switch_moving(&other, "sam", &seats));
    let plain = v.edit(|tx| tx.switch(&main, "free"));
    hooks::set_codex_running(None);
    for err in [moving.unwrap_err(), plain.unwrap_err()] {
        let e = err.downcast_ref::<VaultError>();
        assert!(
            matches!(e, Some(VaultError::HarnessRunning { seat, .. }) if seat == "codex"),
            "{err:#}"
        );
    }
    assert_eq!(codex_refresh(&main)?, "rt-sam-1", "nothing was written");
    assert_eq!(codex_refresh(&other)?, "rt-team-1");
    Ok(())
}

/// Review B-M4: the sign-in in a scratch seat is the only copy of a new grant. When saving it
/// fails (here the label was taken while the harness ran), the scratch dir stays and the error
/// says where; when it is saved, the dir goes.
#[test]
fn a_new_login_that_cannot_be_saved_is_kept_in_its_scratch_dir() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let vault_dir = t.path().join("vault");
    let mut v = Vault::open(&vault_dir)?;
    v.edit(|tx| tx.put(&secret("r-alice"), None, None, Some("work"), None))?;
    let a = super::new_login_scratch(&vault_dir, Utc::now());
    let b = super::new_login_scratch(&vault_dir, Utc::now());
    assert_ne!(a, b, "two adds in one second get two dirs");
    std::fs::create_dir_all(&a)?;
    let (_, scratch_seat) = super::new_login(Harness::Claude, "work", &a);
    write_seat(&scratch_seat, "r-new", "bob")?;
    assert!(v.check_new_label("work", Harness::Claude).is_err());
    let err = super::save_new_login(&mut v, "work", &a, &scratch_seat).unwrap_err();
    assert!(
        format!("{err:#}").contains(&a.display().to_string()),
        "{err:#}"
    );
    assert!(
        scratch_seat.credentials_file().exists(),
        "the grant is kept"
    );
    let saved = super::save_new_login(&mut v, "bob", &a, &scratch_seat)?;
    assert!(saved.left_behind.is_none());
    assert!(!a.exists());
    assert!(find(&v, "bob").is_some());
    Ok(())
}

/// Review A-M4: `accounts import` reads a credentials file of every harness the vault keeps,
/// and an agy or Codex file brings the account it names.
#[test]
fn an_import_reads_every_harness_and_its_account() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let cc = seat(t.path(), "work")?;
    write_seat(&cc, "r-cc", "acc-cc")?;
    let agy = agy_seat(t.path(), "agy")?;
    write_agy(&agy, "ya-sam", "sam@example.com")?;
    let cdx = codex_seat(t.path(), "codex")?;
    write_codex(&cdx, "rt-sam-1", "user-sam", "ws-1")?;
    let read = |s: &Seat| std::fs::read_to_string(s.credentials_file());
    let (sec, acct) = super::parse_import(&read(&cc)?)?;
    assert_eq!((sec.harness(), acct.is_none()), (Harness::Claude, true));
    let (sec, acct) = super::parse_import(&read(&agy)?)?;
    assert_eq!(sec.harness(), Harness::Agy);
    assert_eq!(
        acct.map(|a| a.id().to_string()).as_deref(),
        Some("agy:sam@example.com")
    );
    let (sec, acct) = super::parse_import(&read(&cdx)?)?;
    assert_eq!(sec.harness(), Harness::Codex);
    assert_eq!(
        acct.map(|a| a.id().to_string()).as_deref(),
        Some("cdx:user-sam/ws-1")
    );
    assert!(super::parse_import("{\"other\": 1}").is_err());
    Ok(())
}

/// Review B-M5: a label that is another login's id or email would make lookups by id
/// ambiguous (inside a Codex move too); it is refused like a taken label.
#[test]
fn a_label_never_shadows_another_logins_id_or_email() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let work = seat(t.path(), "work")?;
    write_seat(&work, "r-alice", "alice")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&work, Some("alice")))?;
    let alice = login(&v, "alice")?.clone();
    let id = alice.id.clone();
    let taken = v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some(id.as_str()), None));
    assert!(taken.is_err(), "the id of another login");
    if let Some(email) = alice.email.clone() {
        let taken = v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some(&email), None));
        assert!(taken.is_err(), "the email of another login");
    }
    v.edit(|tx| tx.put(&secret("r-bob"), None, None, Some("bob"), None))?;
    assert_eq!(v.find(id.as_str())?.label, "alice");
    Ok(())
}

/// Review B-L7: `.claude.json` is Claude Code's file. A switch keeps its mode, and a symlink
/// to it stays a link (the file it points at is the one rewritten).
#[cfg(unix)]
#[test]
fn a_seat_file_keeps_its_mode_and_its_link() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir()?;
    let real = t.path().join("dotfiles-claude.json");
    std::fs::write(&real, r#"{"oauthAccount": {"accountUuid": "a"}}"#)?;
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644))?;
    let link = t.path().join(".claude.json");
    std::os::unix::fs::symlink(&real, &link)?;
    assert!(store::rewrite_merging(&link, |text| {
        config_json::with_oauth_account(text, Some(&json!({"accountUuid": "b"})))
    })?);
    assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
    assert_eq!(read_json(&real)?["oauthAccount"]["accountUuid"], "b");
    assert_eq!(
        std::fs::metadata(&real)?.permissions().mode() & 0o777,
        0o644
    );
    Ok(())
}

/// Review B-M28: a grant taken out of the vault (its login forgotten, or its copy replaced by
/// another grant) is kept in `retired.json`, beyond the backup ring's reach; a refreshed access
/// token of the same grant is not.
#[test]
fn grants_taken_out_of_the_vault_are_kept_apart() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let dir = t.path().join("vault");
    let mut v = Vault::open(&dir)?;
    v.edit(|tx| tx.put(&secret("r-alice"), None, None, Some("alice"), None))?;
    let retired = || -> anyhow::Result<Value> {
        Ok(read_json(&dir.join("retired.json")).unwrap_or(json!({})))
    };
    assert_eq!(retired()?.as_object().map(|m| m.len()), Some(0));
    v.edit(|tx| tx.remove("alice", true))?;
    let kept = retired()?;
    let entries: Vec<&Value> = kept
        .as_object()
        .map(|m| m.values().collect())
        .unwrap_or_default();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["secret"]["refreshToken"], "r-alice");
    Ok(())
}

/// A Codex move empties only a seat that still holds the grant that moves. Signed into another
/// account there meanwhile (not saved yet), the seat is left alone and nothing moves: emptying
/// it would delete a grant the vault does not have.
#[test]
fn a_codex_move_never_empties_a_seat_holding_another_login() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let main = codex_seat(t.path(), "codex")?;
    let other = codex_seat(t.path(), "codex-work")?;
    write_codex(&main, "rt-sam-1", "user-sam", "ws-1")?;
    let mut v = Vault::open(&t.path().join("vault"))?;
    v.edit(|tx| tx.capture(&main, Some("sam")))?;
    // Another account signed in in `codex` since; the vault still records sam there.
    write_codex(&main, "rt-new-1", "user-new", "ws-9")?;
    let seats = [main.clone(), other.clone()];
    let err = v
        .edit(|tx| tx.switch_moving(&other, "sam", &seats))
        .unwrap_err();
    assert_eq!(
        crate::errors::code_of(&err),
        Some("vault-seat-changed"),
        "{err:#}"
    );
    assert_eq!(
        codex_refresh(&main)?,
        "rt-new-1",
        "the new login is still there"
    );
    assert!(!other.credentials_file().exists(), "nothing moved");
    Ok(())
}

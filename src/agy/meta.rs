//! Conversation metadata for Google Antigravity (`agy`): titles, workspace URIs (cwds),
//! run status and parent-child subagent links.
//!
//! SQLite reads of `conversation_summaries.db` live in `agy::summaries` (the crate
//! keeps all SQL inside `src/index/`); this module holds the shared types, URI decoding, tree
//! resolution and the JSON fallback (`cache/conversation_metadata.json`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::core::harness::Harness;
use crate::core::ids::SeatId;
use crate::model::files::{AgentMeta, FileKind, TranscriptFile};

/// Live run state reported by Antigravity in `conversation_summaries.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunStatus {
    Running,
    WaitingForUser,
    #[default]
    Idle,
}

impl RunStatus {
    pub fn parse(raw: &str) -> RunStatus {
        match raw {
            "CASCADE_RUN_STATUS_RUNNING" => RunStatus::Running,
            "CASCADE_RUN_STATUS_WAITING_FOR_USER" => RunStatus::WaitingForUser,
            other => {
                if other.contains("WAITING") {
                    RunStatus::WaitingForUser
                } else if other.contains("RUNNING") {
                    RunStatus::Running
                } else {
                    RunStatus::Idle
                }
            }
        }
    }
}

/// What Antigravity records about one conversation in `conversation_summaries.db` or
/// `cache/conversation_metadata.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationMeta {
    pub conversation_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub status: RunStatus,
    pub parent_conversation_id: Option<String>,
    pub nesting_depth: u32,
    pub agent_name: Option<String>,
    pub not_fully_idle: bool,
    pub killed: bool,
    pub last_modified: Option<DateTime<Utc>>,
}

/// Converts a `file:///path/to/dir` URI to a filesystem path (percent-decoding UTF-8 escapes).
/// Plain paths without a scheme are returned as-is.
pub fn parse_file_uri(uri: &str) -> Option<String> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return None;
    }
    let raw = match trimmed.strip_prefix("file://") {
        Some(rest) => rest,
        None => return Some(trimmed.to_string()),
    };
    let decoded = percent_decode(raw);
    // Windows URIs look like `file:///C:/work/repo` → strip leading `/` before `C:`.
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(decoded[1..].to_string());
    }
    Some(decoded)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    char::from(b)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// Parses `workspace_uris` from `conversation_summaries.db` (JSON array, comma-separated list, or
/// single URI) and returns the first valid filesystem path.
pub fn parse_workspace_uris(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with('[')
        && let Ok(arr) = serde_json::from_str::<Vec<String>>(t)
    {
        return arr.iter().find_map(|u| parse_file_uri(u));
    }
    t.split([',', '\n']).find_map(parse_file_uri)
}

/// Parses an RFC3339 or SQLite datetime string (`YYYY-MM-DD HH:MM:SS.nnnnnnnnn+00:00`) into UTC.
pub fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let t = s.trim();
    if t.is_empty() || t.starts_with("0001-") {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Some(dt.with_timezone(&Utc));
    }
    // Go's SQLite driver writes `2026-11-23 21:19:18.364289093+00:00` (space instead of `T`).
    let normalized = t.replacen(' ', "T", 1);
    DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CacheFile {
    conversations: HashMap<String, CacheEntry>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CacheEntry {
    summary: CacheSummary,
    last_modified_time: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CacheSummary {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "WorkspaceURIs")]
    workspace_uris: Vec<String>,
    #[serde(rename = "AgentName")]
    agent_name: String,
    #[serde(rename = "UpdatedAt")]
    updated_at: Option<String>,
}

/// Fallback metadata reader: `cache/conversation_metadata.json`, used when
/// `conversation_summaries.db` is missing or temporarily unreadable.
pub fn read_metadata_json(seat_dir: &Path) -> HashMap<String, ConversationMeta> {
    let path = seat_dir.join("cache").join("conversation_metadata.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(parsed) = serde_json::from_str::<CacheFile>(&text) else {
        return HashMap::new();
    };
    let mut out = HashMap::with_capacity(parsed.conversations.len());
    for (id, entry) in parsed.conversations {
        let cid = if entry.summary.id.is_empty() {
            id
        } else {
            entry.summary.id
        };
        let cwd = entry
            .summary
            .workspace_uris
            .iter()
            .find_map(|u| parse_file_uri(u));
        let title = (!entry.summary.title.trim().is_empty()).then_some(entry.summary.title);
        let agent_name =
            (!entry.summary.agent_name.trim().is_empty()).then_some(entry.summary.agent_name);
        let last_modified = entry
            .summary
            .updated_at
            .as_deref()
            .and_then(parse_timestamp)
            .or_else(|| {
                entry
                    .last_modified_time
                    .as_deref()
                    .and_then(parse_timestamp)
            });
        out.insert(
            cid.clone(),
            ConversationMeta {
                conversation_id: cid,
                title,
                cwd,
                status: RunStatus::Idle,
                parent_conversation_id: None,
                nesting_depth: 0,
                agent_name,
                not_fully_idle: false,
                killed: false,
                last_modified,
            },
        );
    }
    out
}

/// Resolves the root conversation ID for `cid` by walking `parent_conversation_id` links, guarded
/// against cycles. Returns `(root_cid, direct_parent_agent_id, depth)`.
pub fn resolve_lineage(
    cid: &str,
    metas: &HashMap<String, ConversationMeta>,
) -> (String, Option<String>, u32) {
    let Some(first_parent) = metas
        .get(cid)
        .and_then(|m| m.parent_conversation_id.as_deref())
        .filter(|p| !p.is_empty() && *p != cid)
    else {
        return (cid.to_string(), None, 0);
    };
    let mut cur = first_parent;
    let mut depth = 1u32;
    let mut seen = HashSet::new();
    seen.insert(cid);
    seen.insert(cur);
    while let Some(next) = metas
        .get(cur)
        .and_then(|m| m.parent_conversation_id.as_deref())
        .filter(|p| !p.is_empty() && !seen.contains(*p))
    {
        seen.insert(next);
        cur = next;
        depth += 1;
    }
    let root = cur.to_string();
    let parent_agent = if first_parent == root {
        String::new()
    } else {
        first_parent.to_string()
    };
    let reported_depth = metas.get(cid).map(|m| m.nesting_depth).unwrap_or(0);
    (root, Some(parent_agent), depth.max(reported_depth))
}

/// What the files behind a seat's conversation metadata look like (size and modification time
/// of `conversation_summaries.db`, its WAL and the JSON fallback; `None` for one that is not
/// there): equal stamps, equal contents.
pub type Stamp = Vec<Option<(u64, std::time::SystemTime)>>;

pub fn summaries_stamp(seat_dir: &Path) -> Stamp {
    ["conversation_summaries.db", "conversation_summaries.db-wal"]
        .iter()
        .map(|f| seat_dir.join(f))
        .chain(std::iter::once(
            seat_dir.join("cache").join("conversation_metadata.json"),
        ))
        .map(|p| {
            std::fs::metadata(&p)
                .and_then(|m| Ok((m.len(), m.modified()?)))
                .ok()
        })
        .collect()
}

/// Path to one conversation's append-only compact transcript.
pub fn transcript_path(seat_dir: &Path, conversation_id: &str) -> PathBuf {
    seat_dir
        .join("brain")
        .join(conversation_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl")
}

/// Builds a `TranscriptFile` for one Antigravity conversation directory using `metas`.
pub fn transcript_file(
    seat: &SeatId,
    seat_dir: &Path,
    cid: &str,
    metas: &HashMap<String, ConversationMeta>,
) -> Option<TranscriptFile> {
    let path = transcript_path(seat_dir, cid);
    if !path.is_file() {
        return None;
    }
    let meta = metas.get(cid);
    let (root_cid, parent_agent_id, depth) = resolve_lineage(cid, metas);
    let is_root = root_cid == cid;
    let cwd = meta
        .and_then(|m| m.cwd.clone())
        .or_else(|| metas.get(&root_cid).and_then(|m| m.cwd.clone()));
    let title = meta.and_then(|m| m.title.clone());
    let inline_meta = if is_root {
        None
    } else {
        let finished = meta
            .map(|m| m.killed || (m.status == RunStatus::Idle && !m.not_fully_idle))
            .unwrap_or(true);
        let agent_type = meta.and_then(|m| m.agent_name.clone());
        let description = title.clone().or_else(|| agent_type.clone());
        Some(AgentMeta {
            agent_type,
            description,
            tool_use_id: None,
            parent_agent_id,
            spawn_depth: Some(depth),
            request_shape: None,
            model: None,
            effort: None,
            picked_effort_model: None,
            finished,
        })
    };
    Some(TranscriptFile {
        path,
        seat: seat.clone(),
        session_id: root_cid,
        agent_id: if is_root {
            String::new()
        } else {
            cid.to_string()
        },
        kind: if is_root {
            FileKind::Main
        } else {
            FileKind::Subagent
        },
        harness: Harness::Agy,
        default_cwd: cwd,
        session_title: if is_root { title } else { None },
        inline_meta,
    })
}

/// Lists every Antigravity conversation transcript in `seat_dir/brain/`.
pub fn list_transcripts(
    seat: &SeatId,
    seat_dir: &Path,
    metas: &HashMap<String, ConversationMeta>,
) -> Vec<TranscriptFile> {
    let Ok(rd) = std::fs::read_dir(seat_dir.join("brain")) else {
        return Vec::new();
    };
    let mut cids: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    cids.sort();
    cids.iter()
        .filter_map(|cid| transcript_file(seat, seat_dir, cid, metas))
        .collect()
}

/// Lists the transcripts belonging to one root Antigravity session (the root conversation plus any
/// descendant conversations recorded in `metas`).
pub fn list_session_transcripts(
    seat: &SeatId,
    seat_dir: &Path,
    root_cid: &str,
    metas: &HashMap<String, ConversationMeta>,
) -> Vec<TranscriptFile> {
    let mut cids = vec![root_cid.to_string()];
    for cid in metas.keys() {
        if cid != root_cid && resolve_lineage(cid, metas).0 == root_cid {
            cids.push(cid.clone());
        }
    }
    cids.sort();
    cids.dedup();
    cids.iter()
        .filter_map(|cid| transcript_file(seat, seat_dir, cid, metas))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_uris_and_windows_paths() {
        assert_eq!(
            parse_file_uri("file:///w/my%20repo/app").as_deref(),
            Some("/w/my repo/app")
        );
        assert_eq!(
            parse_file_uri("file:///C:/work/app").as_deref(),
            Some("C:/work/app")
        );
        assert_eq!(
            parse_workspace_uris(r#"["file:///w/shop","file:///w/docs"]"#).as_deref(),
            Some("/w/shop")
        );
        assert_eq!(parse_workspace_uris(""), None);
    }

    #[test]
    fn resolves_subagent_lineage_and_breaks_cycles() {
        let mut metas = HashMap::new();
        metas.insert(
            "root".into(),
            ConversationMeta {
                conversation_id: "root".into(),
                cwd: Some("/w/shop".into()),
                ..Default::default()
            },
        );
        metas.insert(
            "child".into(),
            ConversationMeta {
                conversation_id: "child".into(),
                parent_conversation_id: Some("root".into()),
                nesting_depth: 1,
                ..Default::default()
            },
        );
        metas.insert(
            "grand".into(),
            ConversationMeta {
                conversation_id: "grand".into(),
                parent_conversation_id: Some("child".into()),
                nesting_depth: 2,
                ..Default::default()
            },
        );
        assert_eq!(resolve_lineage("root", &metas), ("root".into(), None, 0));
        assert_eq!(
            resolve_lineage("child", &metas),
            ("root".into(), Some(String::new()), 1)
        );
        assert_eq!(
            resolve_lineage("grand", &metas),
            ("root".into(), Some("child".into()), 2)
        );
        // Cycle safety: A -> B -> A terminates cleanly.
        metas.insert(
            "cyc-a".into(),
            ConversationMeta {
                conversation_id: "cyc-a".into(),
                parent_conversation_id: Some("cyc-b".into()),
                ..Default::default()
            },
        );
        metas.insert(
            "cyc-b".into(),
            ConversationMeta {
                conversation_id: "cyc-b".into(),
                parent_conversation_id: Some("cyc-a".into()),
                ..Default::default()
            },
        );
        let (root, _, depth) = resolve_lineage("cyc-a", &metas);
        assert_eq!(root, "cyc-b");
        assert_eq!(depth, 1);
    }

    #[test]
    fn reads_metadata_json_fallback_and_lists_transcripts() {
        let t = tempfile::tempdir().unwrap();
        let cache = t.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            cache.join("conversation_metadata.json"),
            r#"{"conversations":{"c1":{"summary":{"ID":"c1","Title":"Refactor","WorkspaceURIs":["file:///w/shop"],"UpdatedAt":"2026-11-23T12:00:00Z"}}}}"#,
        )
        .unwrap();
        let logs = t.path().join("brain/c1/.system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("transcript.jsonl"), "").unwrap();

        let metas = read_metadata_json(t.path());
        assert_eq!(metas["c1"].title.as_deref(), Some("Refactor"));
        assert_eq!(metas["c1"].cwd.as_deref(), Some("/w/shop"));

        let files = list_transcripts(&"agy".into(), t.path(), &metas);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].harness, Harness::Agy);
        assert_eq!(files[0].session_id, "c1");
        assert_eq!(files[0].default_cwd.as_deref(), Some("/w/shop"));
        assert_eq!(files[0].session_title.as_deref(), Some("Refactor"));
    }
}

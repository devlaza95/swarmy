//! Google Antigravity CLI (`agy`) reader: seat discovery, append-only JSONL transcripts,
//! conversation metadata, live presence detection and non-secret account identity.
//!
//! Offline and read-only, like the Claude Code readers: the one write into an agy seat is the
//! vault's switch of `antigravity-oauth-token` (`credentials`, `src/vault`). Tokens are never
//! logged; only the vault stores them.

pub mod account;
pub mod credentials;
pub mod discovery;
pub mod live;
pub mod meta;
pub mod summaries;
pub mod transcript;

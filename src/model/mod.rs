//! The normalized model every harness reader produces and the rest of swarmy reads: seats,
//! transcript events, transcript files, live sessions. It belongs to no reader, so a change to
//! one harness's reader cannot reach the others through it (reviews A-H5, B-M14).

// Strict lints that hold here already: no catch-all over domain enums, no ignored result,
// no string cut by bytes, in production code (reviews A-H7, B-M24).
#![cfg_attr(not(test), deny(clippy::wildcard_enum_match_arm))]
#![cfg_attr(not(test), deny(clippy::let_underscore_must_use))]
#![cfg_attr(not(test), deny(clippy::string_slice))]

pub mod account;
pub mod event;
pub mod files;
pub mod live;
pub mod project;
pub mod seat;

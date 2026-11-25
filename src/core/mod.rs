//! The domain model: usage, prices, periods, limits, tool classes, harnesses. Pure functions, no I/O.

// A new variant of a domain enum (a harness, a window kind, a dim) must be handled where it
// is matched, not fall into a catch-all (reviews A-H7, B-M24, B-L3).
#![deny(clippy::wildcard_enum_match_arm)]
// No result ignored, no string cut by bytes, and (where there are none) no silent numeric
// casts: the strict lints switched on, in production code, where they hold already (reviews
// A-H7, B-M24).
#![cfg_attr(not(test), deny(clippy::let_underscore_must_use))]
#![cfg_attr(not(test), deny(clippy::string_slice))]
#![cfg_attr(not(test), deny(clippy::cast_possible_truncation))]
#![cfg_attr(not(test), deny(clippy::cast_possible_wrap))]
#![cfg_attr(not(test), deny(clippy::cast_sign_loss))]

pub mod harness;
pub mod ids;
pub mod jwt;
pub mod limits;
pub mod login;
pub mod models;
pub mod pane;
pub mod period;
pub mod pricing;
pub mod seat_use;
pub mod storage;
pub mod tools;
pub mod units;
pub mod usage;

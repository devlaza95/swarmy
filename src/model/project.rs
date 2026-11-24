//! A `[[project]]` entry of `config.toml`: a path prefix that names a project (paths that moved,
//! or come from another machine).

use serde::{Deserialize, Serialize};

/// `[[project]]` entries of `config.toml`: every cwd starting with `match` belongs to `name`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Alias {
    #[serde(rename = "match")]
    pub prefix: String,
    pub name: String,
}

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const MANIFEST_VERSION: u32 = 1;
pub const INLINE_TEXT_LIMIT: u64 = 2 * 1024 * 1024;
pub const PANE_TEXT_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Manifest {
    pub version: u32,
    pub pane_id: String,
    pub agent: String,
    pub session_ref: Option<String>,
    pub root: PathBuf,
    pub captured_unix_ms: u128,
    pub files: BTreeMap<PathBuf, FileRecord>,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileRecord {
    pub size: u64,
    pub modified_unix_ns: Option<u128>,
    pub hash: Option<String>,
    pub blob: Option<String>,
    pub text: TextEligibility,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextEligibility {
    Text,
    Binary,
    Oversized,
    InvalidUtf8,
    Unreadable,
    RetentionCap,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentFile {
    pub relative: PathBuf,
    pub absolute: PathBuf,
    pub size: u64,
    pub modified_unix_ns: Option<u128>,
    pub hash: Option<String>,
    pub text: TextEligibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub baseline: Option<FileRecord>,
    pub current: Option<CurrentFile>,
}

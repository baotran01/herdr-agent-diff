use std::path::PathBuf;

pub const INLINE_TEXT_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextEligibility {
    Text,
    Binary,
    Oversized,
    InvalidUtf8,
    Unreadable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentFile {
    pub relative: PathBuf,
    pub absolute: PathBuf,
    pub size: u64,
    pub modified_unix_ns: Option<u128>,
    pub text: TextEligibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

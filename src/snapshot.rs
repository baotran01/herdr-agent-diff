use std::collections::BTreeMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{CurrentFile, INLINE_TEXT_LIMIT, TextEligibility};
use crate::{Error, Result};
use ignore::{DirEntry, WalkBuilder};

const ALWAYS_IGNORED: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    ".next",
    ".cache",
];
const MAX_SCAN_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_SCAN_FILES: usize = 100_000;

pub fn scan(root: &Path) -> Result<(BTreeMap<PathBuf, CurrentFile>, Vec<String>)> {
    let root = root.canonicalize()?;
    let mut files = BTreeMap::new();
    let mut notices = Vec::new();
    let mut scanned_bytes = 0_u64;
    let mut scanned_files = 0_usize;
    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(true)
        .parents(true)
        .filter_entry(|entry| !is_always_ignored(entry));

    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                notices.push(error.to_string());
                continue;
            }
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&root).map(Path::to_path_buf) else {
            notices.push(format!(
                "escaped traversal root: {}",
                entry.path().display()
            ));
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                notices.push(format!("{}: {error}", relative.display()));
                continue;
            }
        };
        if scanned_files >= MAX_SCAN_FILES
            || scanned_bytes.saturating_add(metadata.len()) > MAX_SCAN_BYTES
        {
            return Err(Error::Message(format!(
                "scan limits exceeded ({MAX_SCAN_FILES} files or {MAX_SCAN_BYTES} bytes)"
            )));
        }
        scanned_files = scanned_files.saturating_add(1);
        scanned_bytes = scanned_bytes.saturating_add(metadata.len());
        match inspect_file(&root, &relative, metadata.len()) {
            Ok(text) => {
                files.insert(
                    relative.clone(),
                    CurrentFile {
                        relative,
                        absolute: entry.path().to_path_buf(),
                        size: metadata.len(),
                        modified_unix_ns: metadata.modified().ok().and_then(system_time_ns),
                        text,
                    },
                );
            }
            Err(error) => {
                notices.push(format!("{}: {error}", relative.display()));
                files.insert(
                    relative.clone(),
                    CurrentFile {
                        relative,
                        absolute: entry.path().to_path_buf(),
                        size: metadata.len(),
                        modified_unix_ns: metadata.modified().ok().and_then(system_time_ns),
                        text: TextEligibility::Unreadable,
                    },
                );
            }
        }
    }
    Ok((files, notices))
}

pub fn workspace_fingerprint(root: &Path) -> Result<(u64, usize)> {
    let root = root.canonicalize()?;
    let mut fingerprint = 0_u64;
    let mut files = 0_usize;
    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(true)
        .parents(true)
        .filter_entry(|entry| !is_always_ignored(entry));

    for item in builder.build() {
        let Ok(entry) = item else {
            continue;
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        relative.hash(&mut hasher);
        metadata.len().hash(&mut hasher);
        metadata
            .modified()
            .ok()
            .and_then(system_time_ns)
            .hash(&mut hasher);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            metadata.dev().hash(&mut hasher);
            metadata.ino().hash(&mut hasher);
        }
        fingerprint = fingerprint.wrapping_add(hasher.finish());
        files = files.saturating_add(1);
    }
    Ok((fingerprint, files))
}

pub fn safe_read(root: &Path, relative: &Path, limit: u64) -> Result<Vec<u8>> {
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Message("path escapes workspace root".into()));
    }
    let canonical_root = root.canonicalize()?;
    read_limited(open_read_only_beneath(&canonical_root, relative)?, limit)
}

fn inspect_file(root: &Path, relative: &Path, size: u64) -> Result<TextEligibility> {
    if size > INLINE_TEXT_LIMIT {
        return Ok(TextEligibility::Oversized);
    }
    let file = open_read_only_beneath(root, relative)?;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() as u64 > INLINE_TEXT_LIMIT {
            return Ok(TextEligibility::Oversized);
        }
    }
    let text = if bytes.contains(&0) {
        TextEligibility::Binary
    } else if std::str::from_utf8(&bytes).is_err() {
        TextEligibility::InvalidUtf8
    } else {
        TextEligibility::Text
    };
    Ok(text)
}

fn read_limited(file: File, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Message(format!(
            "file exceeds inline limit ({limit} bytes)"
        )));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_read_only_beneath(root: &Path, relative: &Path) -> Result<File> {
    use rustix::fs::{Mode, OFlags, open, openat};

    let mut directory = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let mut components = relative.components();
    let Some(last) = components.next_back() else {
        return Err(Error::Message("path is empty".into()));
    };
    for component in components {
        let Component::Normal(name) = component else {
            return Err(Error::Message("path escapes workspace root".into()));
        };
        let next = openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        directory = next;
    }
    let Component::Normal(name) = last else {
        return Err(Error::Message("path escapes workspace root".into()));
    };
    let file = openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(File::from(file))
}

#[cfg(not(unix))]
fn open_read_only_beneath(root: &Path, relative: &Path) -> Result<File> {
    let joined = root.join(relative);
    let canonical = joined.canonicalize()?;
    if !canonical.starts_with(root) || std::fs::symlink_metadata(&joined)?.file_type().is_symlink()
    {
        return Err(Error::Message("path escapes workspace root".into()));
    }
    Ok(File::open(canonical)?)
}

fn is_always_ignored(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|kind| kind.is_dir())
        && ALWAYS_IGNORED
            .iter()
            .any(|ignored| entry.file_name() == *ignored)
}

fn system_time_ns(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_nanos())
}

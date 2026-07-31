use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ignore::{DirEntry, WalkBuilder};
use sha2::{Digest, Sha256};

use crate::model::{
    Change, ChangeKind, CurrentFile, FileRecord, INLINE_TEXT_LIMIT, MANIFEST_VERSION, Manifest,
    PANE_TEXT_LIMIT, TextEligibility,
};
use crate::state::StateStore;
use crate::{Error, Result};

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
const MAX_HASH_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCAN_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_SCAN_FILES: usize = 100_000;

pub struct CaptureRequest<'a> {
    pub pane_id: &'a str,
    pub agent: &'a str,
    pub session_ref: Option<String>,
    pub root: &'a Path,
}

pub fn capture(store: &StateStore, request: CaptureRequest<'_>) -> Result<bool> {
    let Some(guard) = store.begin_capture(request.pane_id)? else {
        return Ok(false);
    };
    let root = request.root.canonicalize()?;
    if !root.is_dir() {
        return Err(Error::Message(format!(
            "capture root is not a directory: {}",
            root.display()
        )));
    }
    let (current, mut notices) = scan(&root)?;
    let mut retained = 0_u64;
    let mut files = BTreeMap::new();
    for (path, current_file) in current {
        let (blob, text) = if current_file.text == TextEligibility::Text {
            if retained.saturating_add(current_file.size) > PANE_TEXT_LIMIT {
                (None, TextEligibility::RetentionCap)
            } else {
                match safe_read(&root, &path, INLINE_TEXT_LIMIT) {
                    Ok(bytes) => {
                        retained = retained.saturating_add(bytes.len() as u64);
                        (Some(store.write_blob(&bytes)?), TextEligibility::Text)
                    }
                    Err(error) => {
                        notices.push(format!("{}: {error}", path.display()));
                        (None, TextEligibility::Unreadable)
                    }
                }
            }
        } else {
            (None, current_file.text)
        };
        files.insert(
            path,
            FileRecord {
                size: current_file.size,
                modified_unix_ns: current_file.modified_unix_ns,
                hash: current_file.hash,
                blob,
                text,
            },
        );
    }
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        pane_id: request.pane_id.to_owned(),
        agent: request.agent.to_owned(),
        session_ref: request.session_ref,
        root,
        captured_unix_ms: now_unix_ms(),
        files,
        notices,
    };
    store.commit_manifest(&manifest)?;
    drop(guard);
    store.gc_blobs()?;
    Ok(true)
}

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
            Ok((hash, text)) => {
                files.insert(
                    relative.clone(),
                    CurrentFile {
                        relative,
                        absolute: entry.path().to_path_buf(),
                        size: metadata.len(),
                        modified_unix_ns: metadata.modified().ok().and_then(system_time_ns),
                        hash,
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
                        hash: None,
                        text: TextEligibility::Unreadable,
                    },
                );
            }
        }
    }
    Ok((files, notices))
}

#[must_use]
pub fn classify(manifest: &Manifest, current: &BTreeMap<PathBuf, CurrentFile>) -> Vec<Change> {
    let mut added = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    let mut changes = Vec::new();

    for (path, record) in &manifest.files {
        match current.get(path) {
            None => {
                deleted.insert(path.clone());
            }
            Some(now) if file_changed(record, now) => changes.push(Change {
                kind: ChangeKind::Modified,
                path: path.clone(),
                old_path: None,
                baseline: Some(record.clone()),
                current: Some(now.clone()),
            }),
            Some(_) => {}
        }
    }
    for path in current.keys() {
        if !manifest.files.contains_key(path) {
            added.insert(path.clone());
        }
    }

    let mut additions_by_hash: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in &added {
        if let Some(hash) = current.get(path).and_then(|record| record.hash.clone()) {
            additions_by_hash
                .entry(hash)
                .or_default()
                .push(path.clone());
        }
    }
    let mut renamed_additions = BTreeSet::new();
    let mut renamed_deletions = BTreeSet::new();
    for old_path in &deleted {
        let Some(hash) = manifest
            .files
            .get(old_path)
            .and_then(|record| record.hash.as_ref())
        else {
            continue;
        };
        let Some(candidates) = additions_by_hash.get_mut(hash) else {
            continue;
        };
        let Some(new_path) = candidates
            .iter()
            .find(|path| !renamed_additions.contains(*path))
            .cloned()
        else {
            continue;
        };
        renamed_additions.insert(new_path.clone());
        renamed_deletions.insert(old_path.clone());
        changes.push(Change {
            kind: ChangeKind::Renamed,
            path: new_path.clone(),
            old_path: Some(old_path.clone()),
            baseline: manifest.files.get(old_path).cloned(),
            current: current.get(&new_path).cloned(),
        });
    }

    for path in added.difference(&renamed_additions) {
        changes.push(Change {
            kind: ChangeKind::Added,
            path: path.clone(),
            old_path: None,
            baseline: None,
            current: current.get(path).cloned(),
        });
    }
    for path in deleted.difference(&renamed_deletions) {
        changes.push(Change {
            kind: ChangeKind::Deleted,
            path: path.clone(),
            old_path: None,
            baseline: manifest.files.get(path).cloned(),
            current: None,
        });
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes
}

fn file_changed(record: &FileRecord, current: &CurrentFile) -> bool {
    if let (Some(old_hash), Some(new_hash)) = (&record.hash, &current.hash) {
        return old_hash != new_hash;
    }
    record.size != current.size
        || record.modified_unix_ns != current.modified_unix_ns
        || record.text != current.text
}

pub fn safe_read(root: &Path, relative: &Path, limit: u64) -> Result<Vec<u8>> {
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Message("path escapes capture root".into()));
    }
    let canonical_root = root.canonicalize()?;
    read_limited(open_read_only_beneath(&canonical_root, relative)?, limit)
}

fn inspect_file(
    root: &Path,
    relative: &Path,
    size: u64,
) -> Result<(Option<String>, TextEligibility)> {
    if size > MAX_HASH_FILE_BYTES {
        return Ok((None, TextEligibility::Oversized));
    }
    let file = open_read_only_beneath(root, relative)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut preview = if size <= INLINE_TEXT_LIMIT {
        Some(Vec::with_capacity(usize::try_from(size).unwrap_or(0)))
    } else {
        None
    };
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut read = 0_u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        read = read.saturating_add(count as u64);
        if read > MAX_HASH_FILE_BYTES {
            return Ok((None, TextEligibility::Oversized));
        }
        hasher.update(&buffer[..count]);
        if let Some(bytes) = &mut preview {
            if read <= INLINE_TEXT_LIMIT {
                bytes.extend_from_slice(&buffer[..count]);
            } else {
                preview = None;
            }
        }
    }
    let text = match preview {
        None => TextEligibility::Oversized,
        Some(bytes) if bytes.contains(&0) => TextEligibility::Binary,
        Some(bytes) if std::str::from_utf8(&bytes).is_err() => TextEligibility::InvalidUtf8,
        Some(_) => TextEligibility::Text,
    };
    Ok((Some(format!("{:x}", hasher.finalize())), text))
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
            return Err(Error::Message("path escapes capture root".into()));
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
        return Err(Error::Message("path escapes capture root".into()));
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
        return Err(Error::Message("path escapes capture root".into()));
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

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

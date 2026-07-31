use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{MANIFEST_VERSION, Manifest};
use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct StateStore {
    root: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ViewerMapping {
    pub target_pane_id: String,
    pub viewer_pane_id: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ViewerPlacement {
    #[default]
    Split,
    Tab,
}

impl ViewerPlacement {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Tab => "tab",
        }
    }
}

pub struct CaptureGuard {
    path: PathBuf,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl StateStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        set_private_directory_permissions(&root)?;
        for directory in ["manifests", "blobs", "markers", "viewers"] {
            let path = root.join(directory);
            fs::create_dir_all(&path)?;
            harden_state_tree(&path)?;
        }
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn begin_capture(&self, pane_id: &str) -> Result<Option<CaptureGuard>> {
        let path = self.marker_path(pane_id);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                if let Err(error) = set_private_file_permissions(&path) {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                Ok(Some(CaptureGuard { path }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    #[must_use]
    pub fn capturing(&self, pane_id: &str) -> bool {
        self.marker_path(pane_id).exists()
    }

    pub fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        let hash = hex_digest(bytes);
        let path = self.root.join("blobs").join(&hash);
        if !path.exists() {
            atomic_write(&path, bytes)?;
        }
        Ok(hash)
    }

    pub fn read_blob(&self, hash: &str) -> Result<Vec<u8>> {
        if !is_blob_hash(hash) {
            return Err(Error::Message("invalid blob hash".into()));
        }
        let path = self.root.join("blobs").join(hash);
        let file = open_state_file(&path)?;
        let mut bytes = Vec::new();
        file.take(crate::model::INLINE_TEXT_LIMIT.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > crate::model::INLINE_TEXT_LIMIT {
            return Err(Error::Message("blob exceeds inline limit".into()));
        }
        Ok(bytes)
    }

    pub fn commit_manifest(&self, manifest: &Manifest) -> Result<()> {
        if manifest.version != MANIFEST_VERSION {
            return Err(Error::Message("unsupported manifest version".into()));
        }
        let bytes = serde_json::to_vec(manifest)?;
        atomic_write(&self.manifest_path(&manifest.pane_id), &bytes)?;
        self.gc_blobs()
    }

    pub fn load_manifest(&self, pane_id: &str) -> Result<Option<Manifest>> {
        let path = self.manifest_path(pane_id);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        if manifest.version != MANIFEST_VERSION {
            return Err(Error::Message(format!(
                "manifest version {} is not supported",
                manifest.version
            )));
        }
        Ok(Some(manifest))
    }

    pub fn remove_pane(&self, pane_id: &str) -> Result<()> {
        remove_if_exists(&self.manifest_path(pane_id))?;
        remove_if_exists(&self.marker_path(pane_id))?;
        for placement in [ViewerPlacement::Split, ViewerPlacement::Tab] {
            remove_if_exists(&self.viewer_path(pane_id, placement))?;
        }
        self.remove_mappings_for_viewer(pane_id)?;
        self.gc_blobs()
    }

    pub fn set_viewer_mapping(&self, mapping: &ViewerMapping) -> Result<()> {
        self.set_viewer_mapping_for(mapping, ViewerPlacement::Split)
    }

    pub fn set_viewer_mapping_for(
        &self,
        mapping: &ViewerMapping,
        placement: ViewerPlacement,
    ) -> Result<()> {
        atomic_write(
            &self.viewer_path(&mapping.target_pane_id, placement),
            &serde_json::to_vec(mapping)?,
        )
    }

    pub fn viewer_mapping(&self, target_pane_id: &str) -> Result<Option<ViewerMapping>> {
        self.viewer_mapping_for(target_pane_id, ViewerPlacement::Split)
    }

    pub fn viewer_mapping_for(
        &self,
        target_pane_id: &str,
        placement: ViewerPlacement,
    ) -> Result<Option<ViewerMapping>> {
        let bytes = match fs::read(self.viewer_path(target_pane_id, placement)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    pub fn remove_viewer_mapping(&self, target_pane_id: &str) -> Result<()> {
        self.remove_viewer_mapping_for(target_pane_id, ViewerPlacement::Split)
    }

    pub fn remove_viewer_mapping_for(
        &self,
        target_pane_id: &str,
        placement: ViewerPlacement,
    ) -> Result<()> {
        remove_if_exists(&self.viewer_path(target_pane_id, placement))
    }

    fn remove_mappings_for_viewer(&self, viewer_pane_id: &str) -> Result<()> {
        for entry in fs::read_dir(self.root.join("viewers"))? {
            let entry = entry?;
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(mapping) = serde_json::from_slice::<ViewerMapping>(&bytes) else {
                continue;
            };
            if mapping.viewer_pane_id == viewer_pane_id {
                remove_if_exists(&entry.path())?;
            }
        }
        Ok(())
    }

    pub fn gc_blobs(&self) -> Result<()> {
        let mut markers = fs::read_dir(self.root.join("markers"))?;
        if markers.next().transpose()?.is_some() {
            return Ok(());
        }
        let mut referenced = BTreeSet::new();
        for entry in fs::read_dir(self.root.join("manifests"))? {
            let entry = entry?;
            let Ok(bytes) = fs::read(entry.path()) else {
                return Ok(());
            };
            let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
                return Ok(());
            };
            for blob in manifest
                .files
                .values()
                .filter_map(|record| record.blob.as_ref())
            {
                referenced.insert(blob.clone());
            }
        }
        for entry in fs::read_dir(self.root.join("blobs"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !referenced.contains(&name) {
                remove_if_exists(&entry.path())?;
            }
        }
        Ok(())
    }

    fn manifest_path(&self, pane_id: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(format!("{}.json", pane_key(pane_id)))
    }

    fn marker_path(&self, pane_id: &str) -> PathBuf {
        self.root
            .join("markers")
            .join(format!("{}.capture", pane_key(pane_id)))
    }

    fn viewer_path(&self, pane_id: &str, placement: ViewerPlacement) -> PathBuf {
        let suffix = match placement {
            ViewerPlacement::Split => "",
            ViewerPlacement::Tab => ".tab",
        };
        self.root
            .join("viewers")
            .join(format!("{}{suffix}.json", pane_key(pane_id)))
    }
}

fn pane_key(pane_id: &str) -> String {
    pane_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Message("state path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        set_private_file_permissions(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[must_use]
pub fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_blob_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(unix)]
fn open_state_file(path: &Path) -> Result<File> {
    use rustix::fs::{Mode, OFlags, open};

    let file = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(File::from(file))
}

#[cfg(not(unix))]
fn open_state_file(path: &Path) -> Result<File> {
    Ok(File::open(path)?)
}

fn harden_state_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(Error::Message(format!(
            "state path must not be a symlink: {}",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(Error::Message(format!(
            "state path is not a directory: {}",
            path.display()
        )));
    }
    set_private_directory_permissions(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::Message(format!(
                "state path must not contain symlinks: {}",
                child.display()
            )));
        }
        if metadata.is_dir() {
            harden_state_tree(&child)?;
        } else {
            set_private_file_permissions(&child)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

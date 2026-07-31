use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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

impl StateStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        set_private_directory_permissions(&root)?;
        let viewers = root.join("viewers");
        fs::create_dir_all(&viewers)?;
        harden_state_tree(&viewers)?;
        Ok(Self { root })
    }

    pub fn remove_pane(&self, pane_id: &str) -> Result<()> {
        for placement in [ViewerPlacement::Split, ViewerPlacement::Tab] {
            remove_if_exists(&self.viewer_path(pane_id, placement))?;
        }
        self.remove_mappings_for_viewer(pane_id)
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

pub mod app;
pub mod context;
pub mod diff;
pub mod git;
pub mod herdr;
pub mod model;
pub mod snapshot;
pub mod state;

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Notify(#[from] notify::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub const PLUGIN_ID: &str = "herdr-agent-diff";

pub fn state_dir() -> Result<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Message("HERDR_PLUGIN_STATE_DIR is not set".into()))
}

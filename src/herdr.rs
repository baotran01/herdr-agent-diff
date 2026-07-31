use std::process::{Command, Output};

use serde_json::Value;

use crate::context::PluginContext;
use crate::state::{StateStore, ViewerMapping, ViewerPlacement};
use crate::{Error, PLUGIN_ID, Result};

pub trait Herdr {
    fn output(&self, arguments: &[String]) -> Result<Output>;
}

pub struct ProcessHerdr {
    binary: String,
}

impl ProcessHerdr {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            binary: std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        }
    }
}

impl Herdr for ProcessHerdr {
    fn output(&self, arguments: &[String]) -> Result<Output> {
        Ok(Command::new(&self.binary).args(arguments).output()?)
    }
}

pub fn pane_exists(herdr: &impl Herdr, pane_id: &str) -> bool {
    herdr
        .output(&["pane".into(), "get".into(), pane_id.into()])
        .is_ok_and(|output| output.status.success())
}

pub fn pane_info(herdr: &impl Herdr, pane_id: &str) -> Result<Value> {
    checked(herdr, &["pane".into(), "get".into(), pane_id.to_owned()])
}

pub fn pane_root(herdr: &impl Herdr, pane_id: &str) -> Result<std::path::PathBuf> {
    let value = pane_info(herdr, pane_id)?;
    let pane = value
        .get("result")
        .and_then(|result| result.get("pane"))
        .ok_or_else(|| Error::Message("Herdr pane response has no pane record".into()))?;
    pane.get("foreground_cwd")
        .or_else(|| pane.get("cwd"))
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| Error::Message("Herdr pane has no working directory".into()))
}

pub fn pane_left_neighbor(herdr: &impl Herdr, pane_id: &str) -> Result<Option<String>> {
    let value = checked(
        herdr,
        &[
            "pane".into(),
            "neighbor".into(),
            "--pane".into(),
            pane_id.into(),
            "--direction".into(),
            "left".into(),
        ],
    )?;
    Ok(value
        .get("result")
        .and_then(|result| result.get("neighbor"))
        .and_then(|neighbor| neighbor.get("neighbor_pane_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}

pub fn open_or_focus(
    herdr: &impl Herdr,
    store: &StateStore,
    context: &PluginContext,
) -> Result<()> {
    open_or_focus_at(herdr, store, context, ViewerPlacement::Split)
}

pub fn open_or_focus_tab(
    herdr: &impl Herdr,
    store: &StateStore,
    context: &PluginContext,
) -> Result<()> {
    open_or_focus_at(herdr, store, context, ViewerPlacement::Tab)
}

fn open_or_focus_at(
    herdr: &impl Herdr,
    store: &StateStore,
    context: &PluginContext,
    placement: ViewerPlacement,
) -> Result<()> {
    let target = context
        .pane_id
        .as_deref()
        .ok_or_else(|| Error::Message("Herdr did not provide an invoking pane".into()))?;
    if let Some(mapping) = store.viewer_mapping_for(target, placement)? {
        if pane_exists(herdr, &mapping.viewer_pane_id) {
            checked(
                herdr,
                &[
                    "plugin".into(),
                    "pane".into(),
                    "focus".into(),
                    mapping.viewer_pane_id,
                ],
            )?;
            return Ok(());
        }
        store.remove_viewer_mapping_for(target, placement)?;
    }
    let mut arguments = vec![
        "plugin".into(),
        "pane".into(),
        "open".into(),
        "--plugin".into(),
        PLUGIN_ID.into(),
        "--entrypoint".into(),
        "viewer".into(),
        "--placement".into(),
        placement.as_str().into(),
    ];
    if placement == ViewerPlacement::Split {
        arguments.extend([
            "--target-pane".into(),
            target.into(),
            "--direction".into(),
            "right".into(),
            "--env".into(),
            format!("HERDR_AGENT_DIFF_TARGET_PANE={target}"),
            "--focus".into(),
        ]);
    } else {
        let workspace = context
            .workspace_id
            .as_deref()
            .ok_or_else(|| Error::Message("Herdr did not provide an invoking workspace".into()))?;
        arguments.extend([
            "--workspace".into(),
            workspace.into(),
            "--env".into(),
            format!("HERDR_AGENT_DIFF_TARGET_PANE={target}"),
            "--env".into(),
            format!("HERDR_AGENT_DIFF_VIEWER_PLACEMENT={}", placement.as_str()),
            "--focus".into(),
        ]);
    }
    checked(herdr, &arguments)?;
    Ok(())
}

pub fn register_viewer(store: &StateStore, target_pane_id: &str) -> Result<()> {
    register_viewer_for(store, target_pane_id, ViewerPlacement::Split)
}

pub fn register_viewer_for(
    store: &StateStore,
    target_pane_id: &str,
    placement: ViewerPlacement,
) -> Result<()> {
    let viewer_pane_id = std::env::var("HERDR_PANE_ID")
        .map_err(|_| Error::Message("viewer pane id is unavailable".into()))?;
    store.set_viewer_mapping_for(
        &ViewerMapping {
            target_pane_id: target_pane_id.into(),
            viewer_pane_id,
        },
        placement,
    )
}

fn checked(herdr: &impl Herdr, arguments: &[String]) -> Result<Value> {
    let output = herdr.output(arguments)?;
    if !output.status.success() {
        return Err(Error::Message(format!(
            "Herdr command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    serde_json::from_slice(&output.stdout).map_err(Into::into)
}

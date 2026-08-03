use herdr_agent_diff::app;
use herdr_agent_diff::context::PluginContext;
use herdr_agent_diff::herdr::{
    Herdr, ProcessHerdr, open_or_focus, open_or_focus_tab, pane_left_neighbor, pane_root,
    register_viewer_for,
};
use herdr_agent_diff::state::StateStore;
use herdr_agent_diff::{Error, Result, state_dir};
use std::env;

fn main() {
    if let Err(error) = run() {
        eprintln!("herdr-agent-diff: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let command = env::args().nth(1).ok_or_else(|| {
        Error::Message("usage: herdr-agent-diff <event|open|open-tab|view>".into())
    })?;
    let store = StateStore::new(state_dir()?)?;
    match command.as_str() {
        "event" => handle_event(&store),
        "open" => handle_open(&store),
        "open-tab" => handle_open_tab(&store),
        "view" => handle_view(&store),
        _ => Err(Error::Message(format!("unknown command: {command}"))),
    }
}

fn handle_event(store: &StateStore) -> Result<()> {
    let event = env::var("HERDR_PLUGIN_EVENT").unwrap_or_default();
    let context = PluginContext::from_env();
    let pane_id = context
        .pane_id
        .ok_or_else(|| Error::Message("event did not identify a pane".into()))?;
    if matches!(event.as_str(), "pane.closed" | "pane.exited") {
        return store.remove_pane(&pane_id);
    }
    Ok(())
}

fn handle_open(store: &StateStore) -> Result<()> {
    let context = PluginContext::from_env();
    let herdr = ProcessHerdr::from_env();
    open_or_focus(&herdr, store, &context)
}

fn handle_open_tab(store: &StateStore) -> Result<()> {
    let context = PluginContext::from_env();
    let herdr = ProcessHerdr::from_env();
    open_or_focus_tab(&herdr, store, &context)
}

fn handle_view(store: &StateStore) -> Result<()> {
    let requested_target = env::var("HERDR_AGENT_DIFF_TARGET_PANE")
        .map_err(|_| Error::Message("viewer target pane is unavailable".into()))?;
    let viewer_pane_id = env::var("HERDR_PANE_ID")
        .map_err(|_| Error::Message("viewer pane id is unavailable".into()))?;
    let placement = match env::var("HERDR_AGENT_DIFF_VIEWER_PLACEMENT").as_deref() {
        Ok("tab") => herdr_agent_diff::state::ViewerPlacement::Tab,
        _ => herdr_agent_diff::state::ViewerPlacement::Split,
    };
    let herdr = ProcessHerdr::from_env();
    let target = match placement {
        herdr_agent_diff::state::ViewerPlacement::Split => {
            pane_left_neighbor(&herdr, &viewer_pane_id)?
                .ok_or_else(|| Error::Message("viewer has no agent pane on its left".into()))?
        }
        herdr_agent_diff::state::ViewerPlacement::Tab => requested_target,
    };
    register_viewer_for(store, &target, placement)?;
    let _mapping_guard = MappingGuard {
        store: store.clone(),
        target: target.clone(),
        placement,
    };
    let root = viewer_root(&herdr, &target)?;
    app::run(&root, target, &herdr)
}

fn viewer_root(herdr: &impl Herdr, target: &str) -> Result<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("HERDR_AGENT_DIFF_ROOT") {
        let root = std::path::PathBuf::from(root);
        if root.is_absolute() && root.is_dir() {
            return Ok(root);
        }
        return Err(Error::Message(
            "invoking pane workspace directory is unavailable".into(),
        ));
    }
    pane_root(herdr, target)
}

struct MappingGuard {
    store: StateStore,
    target: String,
    placement: herdr_agent_diff::state::ViewerPlacement,
}

impl Drop for MappingGuard {
    fn drop(&mut self) {
        let _ = self
            .store
            .remove_viewer_mapping_for(&self.target, self.placement);
    }
}

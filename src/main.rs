use herdr_agent_diff::app;
use herdr_agent_diff::context::PluginContext;
use herdr_agent_diff::herdr::{
    ProcessHerdr, open_or_focus, open_or_focus_tab, pane_agent_details, pane_root,
    register_viewer_for,
};
use herdr_agent_diff::snapshot::{CaptureRequest, capture};
use herdr_agent_diff::state::StateStore;
use herdr_agent_diff::{Error, Result, state_dir};
use serde_json::json;
use std::env;

fn main() {
    if let Err(error) = run() {
        eprintln!("herdr-agent-diff: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let command = env::args().nth(1).ok_or_else(|| {
        Error::Message("usage: herdr-agent-diff <event|open|open-tab|view|status>".into())
    })?;
    let store = StateStore::new(state_dir()?)?;
    match command.as_str() {
        "event" => handle_event(&store),
        "open" => handle_open(&store),
        "open-tab" => handle_open_tab(&store),
        "view" => handle_view(&store),
        "status" => handle_status(&store),
        _ => Err(Error::Message(format!("unknown command: {command}"))),
    }
}

fn handle_event(store: &StateStore) -> Result<()> {
    let event = env::var("HERDR_PLUGIN_EVENT").unwrap_or_default();
    let context = PluginContext::from_env();
    let pane_id = context
        .pane_id
        .ok_or_else(|| Error::Message("event did not identify a pane".into()))?;
    if matches!(event.as_str(), "pane.closed" | "pane.exited") || PluginContext::is_release_event()
    {
        return store.remove_pane(&pane_id);
    }
    if event != "pane.agent_detected" {
        return Ok(());
    }

    let herdr = ProcessHerdr::from_env();
    let root = context
        .cwd
        .map_or_else(|| pane_root(&herdr, &pane_id), Ok)?
        .canonicalize()?;
    let (reported_agent, reported_session) =
        pane_agent_details(&herdr, &pane_id).unwrap_or((None, None));
    let agent = context
        .agent
        .or(reported_agent)
        .unwrap_or_else(|| "agent".into());
    let session_ref = context.session_ref.or(reported_session);

    if store.load_manifest(&pane_id)?.is_some_and(|manifest| {
        manifest.root == root && manifest.agent == agent && manifest.session_ref == session_ref
    }) {
        return Ok(());
    }
    let _captured = capture(
        store,
        CaptureRequest {
            pane_id: &pane_id,
            agent: &agent,
            session_ref,
            root: &root,
        },
    )?;
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
    let target = env::var("HERDR_AGENT_DIFF_TARGET_PANE")
        .map_err(|_| Error::Message("viewer target pane is unavailable".into()))?;
    let placement = match env::var("HERDR_AGENT_DIFF_VIEWER_PLACEMENT").as_deref() {
        Ok("tab") => herdr_agent_diff::state::ViewerPlacement::Tab,
        _ => herdr_agent_diff::state::ViewerPlacement::Split,
    };
    register_viewer_for(store, &target, placement)?;
    let _mapping_guard = MappingGuard {
        store: store.clone(),
        target: target.clone(),
        placement,
    };
    let manifest = store.load_manifest(&target)?;
    let herdr = ProcessHerdr::from_env();
    let root = manifest
        .as_ref()
        .map(|value| value.root.clone())
        .map_or_else(|| pane_root(&herdr, &target), Ok)?;
    app::run(store, manifest, &root, target, &herdr)
}

fn handle_status(store: &StateStore) -> Result<()> {
    let arguments: Vec<String> = env::args().skip(2).collect();
    let pane = arguments
        .windows(2)
        .find(|pair| pair[0] == "--pane")
        .map(|pair| pair[1].clone())
        .ok_or_else(|| Error::Message("status requires --pane <id>".into()))?;
    let manifest = store.load_manifest(&pane)?;
    let output = match manifest {
        Some(manifest) => json!({
            "pane_id": pane,
            "status": "captured",
            "capturing": store.capturing(&pane),
            "root": manifest.root,
            "agent": manifest.agent,
            "session_ref": manifest.session_ref,
            "captured_unix_ms": manifest.captured_unix_ms,
            "files": manifest.files.len(),
            "notices": manifest.notices,
        }),
        None if store.capturing(&pane) => json!({
            "pane_id": pane,
            "status": "capturing",
            "message": "Capturing baseline…",
        }),
        None => json!({
            "pane_id": pane,
            "status": "missing",
            "message": "Restart this agent to capture changes from session start.",
        }),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
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

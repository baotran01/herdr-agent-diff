use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Output};
use std::sync::Mutex;

use herdr_agent_diff::Result;
use herdr_agent_diff::context::PluginContext;
use herdr_agent_diff::herdr::{Herdr, open_or_focus, open_or_focus_tab};
use herdr_agent_diff::state::{StateStore, ViewerMapping};
use tempfile::TempDir;

struct FakeHerdr {
    calls: Mutex<Vec<Vec<String>>>,
    pane_exists: bool,
}

impl Herdr for FakeHerdr {
    fn output(&self, arguments: &[String]) -> Result<Output> {
        self.calls.lock().expect("calls").push(arguments.to_vec());
        let is_get = arguments.first().is_some_and(|value| value == "pane")
            && arguments.get(1).is_some_and(|value| value == "get");
        let success = !is_get || self.pane_exists;
        Ok(Output {
            status: ExitStatus::from_raw(if success { 0 } else { 1 << 8 }),
            stdout: if success { b"{}".to_vec() } else { Vec::new() },
            stderr: Vec::new(),
        })
    }
}

fn context() -> PluginContext {
    PluginContext {
        pane_id: Some("w1:p1".into()),
        workspace_id: Some("w1".into()),
        cwd: Some("/tmp/project".into()),
    }
}

#[test]
fn manifest_declares_documented_070_build_events_action_and_split_pane() {
    let raw = fs::read_to_string("herdr-plugin.toml").expect("manifest");
    let manifest: toml::Value = toml::from_str(&raw).expect("valid TOML");
    assert_eq!(manifest["min_herdr_version"].as_str(), Some("0.7.0"));
    assert_eq!(
        manifest["build"][0]["command"]
            .as_array()
            .expect("build command"),
        &[
            toml::Value::String("/bin/sh".into()),
            toml::Value::String("scripts/install.sh".into()),
        ]
    );
    assert_eq!(
        manifest["platforms"].as_array().expect("platforms")[0].as_str(),
        Some("macos")
    );
    let events = manifest["events"].as_array().expect("events");
    let names: Vec<_> = events
        .iter()
        .filter_map(|event| event["on"].as_str())
        .collect();
    assert_eq!(names, ["pane.closed", "pane.exited"]);
    assert_eq!(manifest["actions"][0]["id"].as_str(), Some("open"));
    assert_eq!(manifest["actions"][1]["id"].as_str(), Some("open-tab"));
    assert_eq!(manifest["panes"][0]["id"].as_str(), Some("viewer"));
    assert_eq!(manifest["panes"][0]["placement"].as_str(), Some("split"));
}

#[test]
fn open_uses_exact_documented_plugin_pane_arguments() {
    let state = TempDir::new().expect("state");
    let store = StateStore::new(state.path()).expect("store");
    let herdr = FakeHerdr {
        calls: Mutex::new(Vec::new()),
        pane_exists: false,
    };
    open_or_focus(&herdr, &store, &context()).expect("open");
    assert_eq!(
        herdr.calls.lock().expect("calls")[0],
        [
            "plugin",
            "pane",
            "open",
            "--plugin",
            "herdr-agent-diff",
            "--entrypoint",
            "viewer",
            "--placement",
            "split",
            "--target-pane",
            "w1:p1",
            "--direction",
            "right",
            "--env",
            "HERDR_AGENT_DIFF_TARGET_PANE=w1:p1",
            "--focus",
        ]
    );
}

#[test]
fn open_tab_uses_tab_placement_and_keeps_split_opening_available() {
    let state = TempDir::new().expect("state");
    let store = StateStore::new(state.path()).expect("store");
    let herdr = FakeHerdr {
        calls: Mutex::new(Vec::new()),
        pane_exists: false,
    };
    open_or_focus_tab(&herdr, &store, &context()).expect("open tab");
    assert_eq!(
        herdr.calls.lock().expect("calls")[0],
        [
            "plugin",
            "pane",
            "open",
            "--plugin",
            "herdr-agent-diff",
            "--entrypoint",
            "viewer",
            "--placement",
            "tab",
            "--workspace",
            "w1",
            "--env",
            "HERDR_AGENT_DIFF_TARGET_PANE=w1:p1",
            "--env",
            "HERDR_AGENT_DIFF_VIEWER_PLACEMENT=tab",
            "--focus",
        ]
    );
}

#[test]
fn split_and_tab_mappings_are_independent() {
    let state = TempDir::new().expect("state");
    let store = StateStore::new(state.path()).expect("store");
    store
        .set_viewer_mapping(&ViewerMapping {
            target_pane_id: "w1:p1".into(),
            viewer_pane_id: "w1:p2".into(),
        })
        .expect("split mapping");
    store
        .set_viewer_mapping_for(
            &ViewerMapping {
                target_pane_id: "w1:p1".into(),
                viewer_pane_id: "w1:p3".into(),
            },
            herdr_agent_diff::state::ViewerPlacement::Tab,
        )
        .expect("tab mapping");

    assert_eq!(
        store
            .viewer_mapping("w1:p1")
            .expect("split mapping")
            .expect("split")
            .viewer_pane_id,
        "w1:p2"
    );
    assert_eq!(
        store
            .viewer_mapping_for("w1:p1", herdr_agent_diff::state::ViewerPlacement::Tab,)
            .expect("tab mapping")
            .expect("tab")
            .viewer_pane_id,
        "w1:p3"
    );
}

#[test]
fn open_focuses_live_viewer_and_replaces_stale_mapping() {
    let state = TempDir::new().expect("state");
    let store = StateStore::new(state.path()).expect("store");
    store
        .set_viewer_mapping(&ViewerMapping {
            target_pane_id: "w1:p1".into(),
            viewer_pane_id: "w1:p2".into(),
        })
        .expect("mapping");
    let live = FakeHerdr {
        calls: Mutex::new(Vec::new()),
        pane_exists: true,
    };
    open_or_focus(&live, &store, &context()).expect("focus");
    let calls = live.calls.lock().expect("calls");
    assert_eq!(calls[0], ["pane", "get", "w1:p2"]);
    assert_eq!(calls[1], ["plugin", "pane", "focus", "w1:p2"]);
    drop(calls);

    let missing_viewer = FakeHerdr {
        calls: Mutex::new(Vec::new()),
        pane_exists: false,
    };
    open_or_focus(&missing_viewer, &store, &context()).expect("replace");
    assert!(
        missing_viewer
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|call| call.get(2).is_some_and(|value| value == "open"))
    );
}

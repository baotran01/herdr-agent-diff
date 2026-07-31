use std::path::PathBuf;

use serde_json::Value;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginContext {
    pub pane_id: Option<String>,
    pub workspace_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub agent: Option<String>,
    pub session_ref: Option<String>,
}

impl PluginContext {
    #[must_use]
    pub fn from_env() -> Self {
        let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        let event = std::env::var("HERDR_PLUGIN_EVENT_JSON")
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());

        let values = [event.as_ref(), context.as_ref()];
        let pane_id = find_string(&values, &["pane_id", "target_pane_id"])
            .or_else(|| std::env::var("HERDR_PANE_ID").ok());
        let workspace_id = find_string(&values, &["workspace_id"])
            .or_else(|| std::env::var("HERDR_WORKSPACE_ID").ok());
        let cwd = find_string(&values, &["foreground_cwd", "cwd"])
            .or_else(|| std::env::var("HERDR_ACTIVE_PANE_CWD").ok())
            .map(PathBuf::from);
        let agent = find_string(&values, &["agent", "agent_label"]);
        let session_ref = find_session(&values);

        Self {
            pane_id,
            workspace_id,
            cwd,
            agent,
            session_ref,
        }
    }

    #[must_use]
    pub fn is_release_event() -> bool {
        let event_name = std::env::var("HERDR_PLUGIN_EVENT").unwrap_or_default();
        if event_name != "pane.agent_detected" {
            return false;
        }
        let Ok(raw) = std::env::var("HERDR_PLUGIN_EVENT_JSON") else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        value
            .get("released")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || value
                .get("reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| reason.eq_ignore_ascii_case("released"))
            || find_value(&value, "agent").is_some_and(Value::is_null)
    }
}

fn find_string(values: &[Option<&Value>], keys: &[&str]) -> Option<String> {
    values.iter().flatten().find_map(|value| {
        keys.iter()
            .find_map(|key| find_value(value, key).and_then(Value::as_str))
            .map(ToOwned::to_owned)
    })
}

fn find_session(values: &[Option<&Value>]) -> Option<String> {
    find_string(values, &["agent_session_id", "session_id"]).or_else(|| {
        values.iter().flatten().find_map(|value| {
            find_value(value, "agent_session")
                .and_then(|session| session.get("value"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
    })
}

fn find_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(found) = value.get(key) {
        return Some(found);
    }
    value.as_object()?.values().find_map(|child| {
        if child.is_object() {
            find_value(child, key)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{find_session, find_string};

    #[test]
    fn parses_nested_event_shapes_defensively() {
        let event = json!({
            "event": {
                "pane": {"pane_id": "w1:p2", "foreground_cwd": "/tmp/demo"},
                "agent": "codex",
                "agent_session": {"value": "native-1"}
            }
        });
        let values = [Some(&event)];
        assert_eq!(find_string(&values, &["pane_id"]).as_deref(), Some("w1:p2"));
        assert_eq!(find_session(&values).as_deref(), Some("native-1"));
    }
}

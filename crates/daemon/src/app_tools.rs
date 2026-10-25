//! The app's own tools (`lynshen_agent_core::app_tools`) for the sessions
//! the daemon hosts, switched in three groups in config.json:
//!
//! - `app_tools.present_files` (default on): `present_files`;
//! - `app_tools.propose_schedule` (default on): `propose_schedule`;
//! - `sessions.messages` (`off`, `ask` or `on`, the default): the session
//!   tools (see `session_messages`).
//!
//! LynShen sessions get them as host tools (`extensions` here and in
//! `session_messages`); Claude Code and Codex sessions through `engines`
//! (an in-process MCP server, dynamic tools). ACP agents get none. An engine
//! started after a switch leaves out what is off; a call to a tool turned
//! off since it started is refused.

use lynshen_agent_core::{
    app_tools::{self as tools, PRESENT_FILES, PROPOSE_SCHEDULE, SESSION_TOOLS},
    host::{HostExtensions, HostGate},
};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

use crate::session_messages::Setting;

/// config.json as it is now (empty when unreadable).
pub fn config() -> Value {
    crate::lynshen_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_default()
}

/// Whether the `app_tools.<key>` switch is on (missing: on).
fn switch(config: &Value, key: &str) -> bool {
    config["app_tools"][key] != false
}

/// The app tools `config` leaves on, in the order engines offer them.
pub fn enabled(config: &Value) -> Vec<&'static str> {
    let mut on = Vec::new();
    for name in [PRESENT_FILES, PROPOSE_SCHEDULE] {
        if switch(config, name) {
            on.push(name);
        }
    }
    if Setting::parse(config) != Setting::Off {
        on.extend(SESSION_TOOLS);
    }
    on
}

/// Whether app tool `name` is on now.
pub fn is_on(name: &str) -> bool {
    enabled(&config()).contains(&name)
}

/// Said to a model calling an app tool the user has turned off.
pub fn turned_off(name: &str) -> String {
    format!("{name} is turned off in LynShen's settings")
}

/// The switches, as the `app_tools` and `app_tools_set` ops answer.
pub fn json(config: &Value) -> Value {
    json!({
        "type": "app_tools",
        "present_files": switch(config, PRESENT_FILES),
        "propose_schedule": switch(config, PROPOSE_SCHEDULE),
        "sessions": Setting::parse(config).as_str(),
    })
}

/// Sets the switches `op` names (`present_files` and `propose_schedule`:
/// true or false; `sessions`: `off`, `ask`, `on`, or true / false for on /
/// off); the others stay. Engines started from then on follow them.
pub fn set(op: &Value) -> Result<Value, String> {
    let sessions = match &op["sessions"] {
        Value::Null => None,
        Value::Bool(true) => Some("on"),
        Value::Bool(false) => Some("off"),
        Value::String(value) if matches!(value.as_str(), "off" | "ask" | "on") => {
            Some(value.as_str())
        }
        other => return Err(format!("sessions must be off, ask or on, not {other}")),
    };
    let mut switches = Vec::new();
    for key in [PRESENT_FILES, PROPOSE_SCHEDULE] {
        match &op[key] {
            Value::Null => {}
            Value::Bool(on) => switches.push((key, *on)),
            other => return Err(format!("{key} must be true or false, not {other}")),
        }
    }
    lynshen_agent_core::edit_config(|config| {
        if !switches.is_empty() {
            let entry = config.entry("app_tools").or_insert_with(|| json!({}));
            if !entry.is_object() {
                *entry = json!({});
            }
            for (key, on) in &switches {
                entry[*key] = json!(on);
            }
        }
        if let Some(value) = sessions {
            let entry = config.entry("sessions").or_insert_with(|| json!({}));
            if !entry.is_object() {
                *entry = json!({});
            }
            entry["messages"] = json!(value);
        }
        Ok(())
    })?;
    Ok(json(&config()))
}

/// `present_files` and `propose_schedule` (when `schedule`) for a LynShen
/// session in `cwd`, as host tools, leaving out what is off; None when both
/// are. An agent's session proposes schedules with its own `schedule` tool.
pub fn extensions(cwd: PathBuf, schedule: bool) -> Option<HostExtensions> {
    let on: Vec<&'static str> = enabled(&config())
        .into_iter()
        .filter(|name| *name == PRESENT_FILES || (schedule && *name == PROPOSE_SCHEDULE))
        .collect();
    if on.is_empty() {
        return None;
    }
    Some(HostExtensions {
        tools: tools::definitions(&on),
        run_tool: Arc::new(move |name, arguments, _| {
            let result = serde_json::from_str::<Value>(arguments)
                .map_err(|error| format!("invalid JSON arguments: {error}"))
                .and_then(|args| run(name, &args, &cwd));
            match result {
                Ok(output) => (output.to_string(), false),
                Err(error) => (json!({ "error": error }).to_string(), true),
            }
        }),
        prompt: Arc::new(String::new),
        context: Arc::new(String::new),
        exclusive: false,
        // They only show the user something.
        gate: Arc::new(|_| HostGate::ReadOnly),
        summary: Arc::new(|_, _| String::new()),
    })
}

/// Runs `present_files` or `propose_schedule` for a session in `cwd`,
/// unless the user has turned it off.
pub fn run(name: &str, args: &Value, cwd: &std::path::Path) -> Result<Value, String> {
    if !is_on(name) {
        return Err(turned_off(name));
    }
    tools::run(name, args, cwd).unwrap_or_else(|| Err(format!("unknown tool {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_groups_all_on_by_default() {
        let all = enabled(&json!({}));
        assert_eq!(all.len(), 7);
        assert_eq!(
            json(&json!({})),
            json!({ "type": "app_tools", "present_files": true, "propose_schedule": true, "sessions": "on" })
        );
        let some = json!({ "app_tools": { "present_files": false }, "sessions": { "messages": "off" } });
        assert_eq!(enabled(&some), [PROPOSE_SCHEDULE]);
        assert_eq!(json(&some)["sessions"], "off");
        assert_eq!(json(&some)["present_files"], false);
        let ask = json!({ "app_tools": { "propose_schedule": false }, "sessions": { "messages": "ask" } });
        assert_eq!(
            enabled(&ask),
            [
                PRESENT_FILES,
                "list_sessions",
                "read_session",
                "start_session",
                "send_to_session",
                "wait_session"
            ]
        );
    }

    #[test]
    fn a_bad_switch_value_changes_nothing() {
        assert!(set(&json!({ "sessions": "sometimes" })).is_err());
        assert!(set(&json!({ "present_files": "no" })).is_err());
    }
}

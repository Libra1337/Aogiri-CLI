//! What a paired device may do (docs/daemon-protocol.md, "Device scopes").
//! By default a phone may look, send messages and approve ordinary requests.
//! Four elevated scopes are granted on the computer only, per device:
//!
//! - `full_auto`: switch a session to 全自动 (`full-access`), dispatch or
//!   approve a plan in it, change an agent's approval mode or sandbox;
//! - `full_access`: leave the sandbox: `set_sandbox` to `full-access`, and
//!   approve a request that leaves it (`escalation`, `elevation`);
//! - `add_folder`: add or create projects, add a writable directory to a
//!   session (`add_directory`, an approval's `add_dir`), write outside the
//!   workspace (`outside_write`);
//! - `terminal`: open a terminal or a session's own TUI.
//!
//! A device paired before scopes (no `scopes` in its record) keeps the
//! defaults: nothing elevated. Local clients (the desktop) are not limited.

use crate::hub::Hub;
use serde_json::Value;

pub const FULL_AUTO: &str = "full_auto";
pub const FULL_ACCESS: &str = "full_access";
pub const ADD_FOLDER: &str = "add_folder";
pub const TERMINAL: &str = "terminal";
/// Every elevated scope, in the order the desktop lists them.
pub const ELEVATED: [&str; 4] = [FULL_AUTO, FULL_ACCESS, ADD_FOLDER, TERMINAL];

/// Why an op was refused: the scope it needs, for the phone to say where
/// to grant it (设置 → 远程访问 → this device).
pub fn refusal(scope: &str) -> String {
    let what = match scope {
        FULL_AUTO => "switching to full access (全自动)",
        FULL_ACCESS => "running outside the sandbox",
        ADD_FOLDER => "adding folders",
        TERMINAL => "opening a terminal",
        _ => "this",
    };
    format!(
        "scope_required:{scope}: this device may not do {what}; grant it on the computer (Settings → Remote access → this device)"
    )
}

/// The scope `op` needs from a paired device, or None when the defaults
/// allow it. `kind_of` finds what an approval waiting in a session is
/// about (its `kind`), for `approve` and `decide_action`.
pub fn needed(
    name: &str,
    op: &Value,
    kind_of: impl Fn(&str, &str) -> Option<String>,
) -> Option<&'static str> {
    let mode = op["mode"].as_str().unwrap_or_default();
    match name {
        // Viewing a session's TUI (`term_attach`, `term_ack`, `term_detach`)
        // needs nothing more than the conversation does; typing does.
        "term_open" | "term_input" | "term_resize" | "term_close" | "session_tui" => Some(TERMINAL),
        "workspaces_set" | "project_add" | "project_create" | "add_directory" => Some(ADD_FOLDER),
        "set_approval_mode" | "approve_plan" if full_mode(mode) => Some(FULL_AUTO),
        "dispatch_send" if op["approval_mode"] == "full-access" => Some(FULL_AUTO),
        // A requirement's run in 全自动 (`requirements::engine_mode`).
        "requirement_begin" | "requirement_confirm" if full_mode(mode) => Some(FULL_AUTO),
        "agent_update" | "agent_create"
            if op["approval_mode"] == "full-access"
                || op["sandbox"] == "full-access"
                || !op["directories"].is_null()
                || !op["command_rules"].is_null() =>
        {
            Some(FULL_AUTO)
        }
        "set_sandbox" if mode == "full-access" => Some(FULL_ACCESS),
        "command" => {
            let input = op["input"].as_str().unwrap_or_default().trim();
            let mut words = input.split_whitespace();
            match (words.next(), words.next()) {
                (Some("/permissions"), Some("full-access")) => Some(FULL_AUTO),
                (Some("/sandbox"), Some("full-access")) => Some(FULL_ACCESS),
                (Some("/trust"), _) => Some(ADD_FOLDER),
                _ => None,
            }
        }
        "approve" if op["decision"] == "allow" => {
            if !op["add_dir"].is_null() {
                return Some(ADD_FOLDER);
            }
            let session = op["session"].as_str().unwrap_or_default();
            let call = op["call_id"].as_str().unwrap_or_default();
            approval_scope(kind_of(session, call).as_deref())
        }
        "decide_action" if op["decision"] == "allow" => {
            let session = op["session"].as_str().unwrap_or_default();
            let action = op["action"].as_str().unwrap_or_default();
            approval_scope(kind_of(session, action).as_deref())
        }
        _ => None,
    }
}

/// A mode name for 全自动 in any engine's words.
fn full_mode(mode: &str) -> bool {
    matches!(
        mode,
        "full-access" | "full-auto" | "all" | "bypassPermissions"
    )
}

/// The scope approving a request of this kind needs.
fn approval_scope(kind: Option<&str>) -> Option<&'static str> {
    match kind {
        Some("escalation" | "elevation") => Some(FULL_ACCESS),
        Some("outside_write") => Some(ADD_FOLDER),
        _ => None,
    }
}

/// Ok when `device` may send `op`.
pub fn check(hub: &Hub, device: &str, name: &str, op: &Value) -> Result<(), String> {
    let kind_of = |session: &str, id: &str| hub.approval_kind(session, id);
    let Some(scope) = needed(name, op, kind_of) else {
        return Ok(());
    };
    let granted = hub
        .store
        .device(device)
        .is_some_and(|device| device.scopes.iter().any(|s| s == scope));
    if granted {
        Ok(())
    } else {
        Err(refusal(scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kind(kind: &'static str) -> impl Fn(&str, &str) -> Option<String> {
        move |_, _| Some(kind.to_string())
    }

    // Every op a phone could use to do more than look, send and approve
    // ordinary requests, and what it needs (audit 8.2, 8.5).
    #[test]
    fn elevated_ops_need_their_scope_and_ordinary_ones_none() {
        let none = |_: &str, _: &str| None;
        let cases: Vec<(&str, Value, Option<&str>)> = vec![
            ("user_message", json!({ "content": "hi" }), None),
            ("watch", json!({}), None),
            ("fs_read", json!({ "path": "/p/a" }), None),
            ("set_approval_mode", json!({ "mode": "auto" }), None),
            ("set_approval_mode", json!({ "mode": "auto-edit" }), None),
            ("set_approval_mode", json!({ "mode": "manual" }), None),
            (
                "set_approval_mode",
                json!({ "mode": "full-access" }),
                Some(FULL_AUTO),
            ),
            (
                "approve_plan",
                json!({ "decision": "approve", "mode": "auto" }),
                None,
            ),
            (
                "approve_plan",
                json!({ "decision": "approve", "mode": "full-access" }),
                Some(FULL_AUTO),
            ),
            ("dispatch_send", json!({ "approval_mode": "auto" }), None),
            (
                "dispatch_send",
                json!({ "approval_mode": "full-access" }),
                Some(FULL_AUTO),
            ),
            ("requirement_begin", json!({ "mode": "auto" }), None),
            (
                "requirement_begin",
                json!({ "mode": "all" }),
                Some(FULL_AUTO),
            ),
            (
                "requirement_confirm",
                json!({ "mode": "full-access" }),
                Some(FULL_AUTO),
            ),
            (
                "set_approval_mode",
                json!({ "mode": "bypassPermissions" }),
                Some(FULL_AUTO),
            ),
            (
                "set_approval_mode",
                json!({ "mode": "full-auto" }),
                Some(FULL_AUTO),
            ),
            ("agent_update", json!({ "name": "x" }), None),
            (
                "agent_update",
                json!({ "approval_mode": "full-access" }),
                Some(FULL_AUTO),
            ),
            (
                "agent_update",
                json!({ "sandbox": "full-access" }),
                Some(FULL_AUTO),
            ),
            (
                "agent_update",
                json!({ "directories": [] }),
                Some(FULL_AUTO),
            ),
            (
                "agent_update",
                json!({ "command_rules": [] }),
                Some(FULL_AUTO),
            ),
            ("set_sandbox", json!({ "mode": "workspace-write" }), None),
            (
                "set_sandbox",
                json!({ "mode": "full-access" }),
                Some(FULL_ACCESS),
            ),
            (
                "command",
                json!({ "input": "/permissions full-access" }),
                Some(FULL_AUTO),
            ),
            (
                "command",
                json!({ "input": "  /sandbox full-access" }),
                Some(FULL_ACCESS),
            ),
            (
                "command",
                json!({ "input": "/trust yes" }),
                Some(ADD_FOLDER),
            ),
            ("command", json!({ "input": "/model gpt" }), None),
            ("project_add", json!({ "path": "/" }), Some(ADD_FOLDER)),
            ("project_create", json!({}), Some(ADD_FOLDER)),
            ("workspaces_set", json!({}), Some(ADD_FOLDER)),
            ("project_remove", json!({}), None),
            ("add_directory", json!({ "path": "/x" }), Some(ADD_FOLDER)),
            ("term_open", json!({}), Some(TERMINAL)),
            ("term_input", json!({}), Some(TERMINAL)),
            ("session_tui", json!({}), Some(TERMINAL)),
            ("term_attach", json!({}), None),
            ("term_ack", json!({}), None),
            ("term_detach", json!({}), None),
            ("approve", json!({ "decision": "deny" }), None),
            (
                "approve",
                json!({ "decision": "allow", "add_dir": { "path": "/x" } }),
                Some(ADD_FOLDER),
            ),
        ];
        for (name, op, want) in cases {
            assert_eq!(needed(name, &op, none), want, "{name} {op}");
        }
    }

    #[test]
    fn approving_a_request_needs_the_scope_of_its_kind() {
        let allow = json!({ "decision": "allow", "session": "s", "call_id": "c" });
        let deny = json!({ "decision": "deny", "session": "s", "call_id": "c" });
        assert_eq!(needed("approve", &allow, kind("command")), None);
        assert_eq!(needed("approve", &allow, kind("edit")), None);
        assert_eq!(
            needed("approve", &allow, kind("escalation")),
            Some(FULL_ACCESS)
        );
        assert_eq!(
            needed("approve", &allow, kind("elevation")),
            Some(FULL_ACCESS)
        );
        assert_eq!(
            needed("approve", &allow, kind("outside_write")),
            Some(ADD_FOLDER)
        );
        assert_eq!(
            needed("approve", &deny, kind("escalation")),
            None,
            "a deny is always allowed"
        );
        let action = json!({ "decision": "allow", "session": "s", "action": "a" });
        assert_eq!(
            needed("decide_action", &action, kind("escalation")),
            Some(FULL_ACCESS)
        );
        assert_eq!(needed("decide_action", &action, kind("rule")), None);
    }

    #[test]
    fn a_refusal_names_its_scope() {
        assert!(refusal(TERMINAL).starts_with("scope_required:terminal:"));
    }
}

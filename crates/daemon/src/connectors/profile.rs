//! The chat profile (docs/chat-connectors.md, "The chat profile"): what a
//! chat binding's client may do. `handle()` asks this before anything else
//! for every op of a chat client, whatever the connector sends, so the
//! limits hold in one place: no full access, no "always allow", no
//! elevation approvals, no terminals, nothing that widens what the daemon
//! lets clients read, and nothing that drives or answers a session of
//! another engine (Claude Code, Codex, ACP agents such as OpenCode), whose
//! modes and approvals mean something else.

use super::MODES;
use crate::{hub::Hub, session_messages::rank};
use serde_json::Value;

const TERMINAL: &str = "a chat connector cannot open a terminal: use the desktop or the phone page";
const ALWAYS: &str = "from a chat connector an approval allows this call once: \"always allow\", partial edits and new writable directories are decided on the desktop or the phone page";
const ELEVATION: &str = "an elevation (sudo and the like) is approved on the desktop or the phone page, never from a chat connector";
pub(crate) const OTHER_ENGINE: &str = "a chat connector drives LynShen sessions only: a Claude Code, Codex or ACP (OpenCode) session is used on the desktop or the phone page";

/// Whether `session` is a session the daemon recorded as run by the LynShen
/// engine. Only those have the approval modes and approval kinds the chat
/// profile reasons about: another engine's `plan` may run commands, and its
/// approval requests carry no `kind`, so an elevation looks like any tool.
pub(crate) fn lynshen_session(hub: &Hub, session: &str) -> bool {
    !session.is_empty()
        && hub.store.sessions().into_iter().any(|record| {
            record.id == session && record.engine.as_deref().is_none_or(|e| e == "lynshen")
        })
}

fn lynshen(hub: &Hub, session: &str) -> Result<(), String> {
    if lynshen_session(hub, session) {
        Ok(())
    } else {
        Err(OTHER_ENGINE.to_string())
    }
}

/// Ok when a chat client may send `op`.
pub(crate) fn check(hub: &Hub, client: u64, name: &str, op: &Value) -> Result<(), String> {
    let device = hub
        .device_of(client)
        .and_then(|id| hub.store.device(&id))
        .ok_or("this chat binding was revoked")?;
    let chat = device.chat.ok_or("not a chat binding")?;
    let settings = hub.connectors.settings(&chat.connector).unwrap_or_default();
    let cap = settings.max_mode.as_str();
    let session = op["session"].as_str().unwrap_or_default();
    match name {
        "ping" | "session_list" | "session_history" | "workspaces" | "agent_list"
        | "question_list" | "actions_list" | "report_list" | "watch" | "unwatch" | "interrupt"
        | "fs_read" | "git_status" | "git_diff" => Ok(()),
        // The answer goes into the session that asked.
        "question_answer" => {
            let question = op["question"].as_str().unwrap_or_default();
            let asking = hub
                .store
                .question(question)
                .ok_or("no such question")?
                .session;
            lynshen(hub, &asking)
        }
        "session_open" => {
            if !op["engine"].is_null() || !op["options"].is_null() || !op["cwd"].is_null() {
                return Err("a chat connector reopens a session by its id only".to_string());
            }
            Ok(())
        }
        "session_create" => {
            let lynshen = op["engine"].is_null() || op["engine"] == "lynshen";
            if !lynshen || !op["agent"].is_null() || !op["options"].is_null() {
                return Err(
                    "a chat connector starts LynShen sessions only, without options".to_string(),
                );
            }
            if op["chat"] == true && op["cwd"].is_null() {
                return Ok(());
            }
            let known = op["cwd"].as_str().is_some_and(|cwd| {
                crate::projects::project_of_cwd(hub, std::path::Path::new(cwd)).is_some()
            });
            if known {
                Ok(())
            } else {
                Err("a chat connector starts sessions in a known project only".to_string())
            }
        }
        "user_message" | "steer" => {
            lynshen(hub, session)?;
            let mode = hub
                .messages
                .mode(session)
                .ok_or("the session's approval mode is not known yet: watch it first")?;
            if rank(&mode) > rank(cap) {
                return Err(format!(
                    "the session runs in {mode}, looser than {cap}, the most a chat connector may drive"
                ));
            }
            for image in op["images"].as_array().into_iter().flatten() {
                let path = image.as_str().unwrap_or_default();
                hub.uploads
                    .finished(path)
                    .map_err(|_| "a chat connector attaches its own uploads only".to_string())?;
            }
            Ok(())
        }
        "approve" => {
            if !settings.approvals {
                return Err("approvals from chat are turned off for this connector".to_string());
            }
            let decision = op["decision"].as_str().unwrap_or_default();
            if decision != "allow" && decision != "deny" {
                return Err("approve requires decision allow or deny".to_string());
            }
            let once = op["scope"].is_null() || op["scope"] == "once";
            let extras = [
                "always",
                "always_scope",
                "prefix",
                "hunks",
                "add_dir",
                "answers",
            ]
            .iter()
            .any(|field| !op[*field].is_null() && op[*field] != false);
            if !once || extras {
                return Err(ALWAYS.to_string());
            }
            lynshen(hub, session)?;
            let call = op["call_id"].as_str().unwrap_or_default();
            let kind = hub
                .connectors
                .pending_kind(session, call)
                .ok_or("no such approval request waits in this session")?;
            if kind == "elevation" && decision == "allow" {
                return Err(ELEVATION.to_string());
            }
            Ok(())
        }
        "decide_action" => {
            if !settings.approvals {
                return Err("approvals from chat are turned off for this connector".to_string());
            }
            let id = op["action"].as_str().unwrap_or_default();
            let action = hub
                .store
                .open_actions()
                .into_iter()
                .find(|action| action.id == id && action.session_id == session)
                .ok_or("no such action waits in this session")?;
            lynshen(hub, session)?;
            if op["decision"] == "allow" && action.kind.as_deref() == Some("elevation") {
                return Err(ELEVATION.to_string());
            }
            Ok(())
        }
        "set_approval_mode" => {
            lynshen(hub, session)?;
            let mode = op["mode"].as_str().unwrap_or_default();
            if !MODES.contains(&mode) || rank(mode) > rank(cap) {
                return Err(format!(
                    "from a chat connector the approval mode can be {cap} at most; full access is switched on the desktop or the phone page"
                ));
            }
            Ok(())
        }
        "approve_plan" => match op["decision"].as_str() {
            _ if !lynshen_session(hub, session) => Err(OTHER_ENGINE.to_string()),
            Some("revise") => Ok(()),
            Some("approve") => {
                let mode = op["mode"].as_str().unwrap_or("auto-edit");
                if !MODES.contains(&mode) || mode == "plan" || rank(mode) > rank(cap) {
                    return Err(format!(
                        "from a chat connector a plan runs in {cap} at most"
                    ));
                }
                Ok(())
            }
            _ => Err("approve_plan requires decision approve or revise".to_string()),
        },
        "term_open" | "term_input" | "term_resize" | "term_close" | "session_tui" => {
            Err(TERMINAL.to_string())
        }
        _ => Err(format!("{name} is not available from a chat connector")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        store::{now, ChatPeer, Device, Question},
        Agents,
    };
    use lynshen_agent_core::actions::DeferredAction;
    use serde_json::json;
    use std::{fs, sync::mpsc};

    /// A hub with one chat binding's client, and LynShen sessions in `auto`
    /// (`s1`) and full access (`s-full`).
    fn setup(label: &str) -> (std::sync::Arc<Hub>, u64, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-profile-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let _ = fs::remove_dir_all(&dir);
        let hub = Hub::new(
            crate::Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        hub.store
            .record_device(&Device {
                id: "dev-chat".to_string(),
                name: "Alice".to_string(),
                token_hash: "x".to_string(),
                paired_at: 1,
                revoked: false,
                chat: Some(ChatPeer {
                    connector: "tg-1".to_string(),
                    platform: "telegram".to_string(),
                    peer: "42".to_string(),
                    chat: "42".to_string(),
                    lang: "zh".to_string(),
                }),
            })
            .unwrap();
        let (outbox, _inbox) = mpsc::channel();
        let client = hub.add_client(outbox, Some("dev-chat".to_string()));
        for session in ["s1", "s-full"] {
            hub.store.record_session(session, &dir, None).unwrap();
        }
        crate::session_messages::observe(
            &hub,
            "s1",
            &json!({ "type": "approval_mode", "mode": "auto" }),
        );
        crate::session_messages::observe(
            &hub,
            "s-full",
            &json!({ "type": "approval_mode", "mode": "full-access" }),
        );
        (hub, client, dir)
    }

    fn allowed(hub: &Hub, client: u64, op: Value) -> Result<(), String> {
        let name = op["op"].as_str().unwrap_or_default().to_string();
        check(hub, client, &name, &op)
    }

    /// An open question and a deferred action of `session`, and an approval
    /// request (`c1`) waiting in it.
    fn waiting(hub: &Hub, session: &str) {
        hub.store
            .record_question(&Question {
                id: format!("q-{session}"),
                agent: "ops".to_string(),
                session: session.to_string(),
                title: "Which region?".to_string(),
                body: String::new(),
                assumption: String::new(),
                default_action: String::new(),
                importance: "normal".to_string(),
                due_at: None,
                asked_at: 1,
            })
            .unwrap();
        hub.store
            .record_deferred(&DeferredAction {
                id: format!("a-{session}"),
                session_id: session.to_string(),
                cwd: std::path::PathBuf::from("/p"),
                call_id: "c".to_string(),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
                summary: "ls".to_string(),
                subagent_id: None,
                digest: session.to_string(),
                created_at: 1,
                kind: Some("command".to_string()),
                justification: None,
            })
            .unwrap();
        // Other engines' requests carry no kind.
        hub.connectors.observe(
            session,
            &json!({ "type": "approval_request", "call_id": "c1" }),
        );
    }

    /// Every op that drives, sets the mode of, approves in or answers
    /// `session`.
    fn acting(session: &str) -> Vec<Value> {
        vec![
            json!({ "op": "user_message", "session": session, "content": "hi" }),
            json!({ "op": "steer", "session": session, "content": "hi" }),
            json!({ "op": "set_approval_mode", "session": session, "mode": "plan" }),
            json!({ "op": "set_approval_mode", "session": session, "mode": "manual" }),
            json!({ "op": "approve", "session": session, "call_id": "c1", "decision": "allow" }),
            json!({ "op": "approve", "session": session, "call_id": "c1", "decision": "deny" }),
            json!({ "op": "approve_plan", "session": session, "id": "p", "decision": "approve", "mode": "manual" }),
            json!({ "op": "approve_plan", "session": session, "id": "p", "decision": "revise", "feedback": "x" }),
            json!({ "op": "question_answer", "question": format!("q-{session}"), "answer": "eu" }),
            json!({ "op": "decide_action", "session": session, "action": format!("a-{session}"), "decision": "allow" }),
            json!({ "op": "decide_action", "session": session, "action": format!("a-{session}"), "decision": "deny" }),
        ]
    }

    #[test]
    fn a_chat_client_acts_on_lynshen_sessions_only() {
        let (hub, client, dir) = setup("engines");
        // Claude Code, Codex and ACP agents (OpenCode): their `plan` may run
        // commands and their approvals carry no elevation kind, so a chat
        // can neither drive, switch, approve nor answer them, in any mode.
        for engine in ["claude", "codex", "acp"] {
            let session = format!("s-{engine}");
            hub.store
                .record_engine_session(&session, &dir, None, Some(engine), false)
                .unwrap();
            crate::session_messages::observe(
                &hub,
                &session,
                &json!({ "type": "approval_mode", "mode": "plan" }),
            );
            waiting(&hub, &session);
            for op in acting(&session) {
                assert_eq!(
                    allowed(&hub, client, op.clone()),
                    Err(OTHER_ENGINE.to_string()),
                    "{op}"
                );
            }
            // Watching and stopping it stay allowed.
            for op in [
                json!({ "op": "watch", "session": session }),
                json!({ "op": "interrupt", "session": session }),
            ] {
                assert_eq!(allowed(&hub, client, op.clone()), Ok(()), "{op}");
            }
        }
        // A session the daemon never recorded counts as another engine's.
        crate::session_messages::observe(
            &hub,
            "s-unknown",
            &json!({ "type": "approval_mode", "mode": "plan" }),
        );
        waiting(&hub, "s-unknown");
        for op in acting("s-unknown") {
            assert!(allowed(&hub, client, op.clone()).is_err(), "{op}");
        }
        // The same ops on a LynShen session pass.
        waiting(&hub, "s1");
        for op in acting("s1") {
            assert_eq!(allowed(&hub, client, op.clone()), Ok(()), "{op}");
        }
    }

    #[test]
    fn a_chat_client_may_drive_a_session_within_its_mode() {
        let (hub, client, _dir) = setup("drive");
        assert!(hub.is_chat(client));
        for op in [
            json!({ "op": "session_list" }),
            json!({ "op": "watch", "session": "s1" }),
            json!({ "op": "user_message", "session": "s1", "content": "hi" }),
            json!({ "op": "interrupt", "session": "s1" }),
            json!({ "op": "set_approval_mode", "session": "s1", "mode": "manual" }),
            json!({ "op": "set_approval_mode", "session": "s1", "mode": "auto" }),
            json!({ "op": "session_create", "chat": true }),
            json!({ "op": "approve_plan", "session": "s1", "id": "p", "decision": "approve", "mode": "auto-edit" }),
        ] {
            assert_eq!(allowed(&hub, client, op.clone()), Ok(()), "{op}");
        }
    }

    #[test]
    fn a_chat_client_never_reaches_full_access_terminals_or_settings() {
        let (hub, client, dir) = setup("refused");
        for op in [
            json!({ "op": "set_approval_mode", "session": "s1", "mode": "full-access" }),
            json!({ "op": "set_approval_mode", "session": "s1", "mode": "bypassPermissions" }),
            json!({ "op": "user_message", "session": "s-full", "content": "hi" }),
            json!({ "op": "user_message", "session": "unknown", "content": "hi" }),
            json!({ "op": "user_message", "session": "s1", "content": "hi", "images": ["/etc/passwd"] }),
            json!({ "op": "approve_plan", "session": "s1", "id": "p", "decision": "approve", "mode": "full-access" }),
            json!({ "op": "term_open", "cwd": "~" }),
            json!({ "op": "session_tui", "session": "s1" }),
            json!({ "op": "command", "session": "s1", "input": "/approvals full-access" }),
            json!({ "op": "set_sandbox", "session": "s1", "mode": "full-access" }),
            json!({ "op": "project_add", "path": "/" }),
            json!({ "op": "workspaces_set", "rev": 0, "workspaces": [] }),
            json!({ "op": "session_create", "cwd": dir.display().to_string() }),
            json!({ "op": "session_create", "chat": true, "engine": "acp" }),
            json!({ "op": "session_open", "session": "s1", "cwd": "/" }),
            json!({ "op": "agent_update", "agent": "ops", "approval_mode": "full-access" }),
            json!({ "op": "schedule_save", "schedule": {} }),
            json!({ "op": "dispatch_send", "text": "x", "approval_mode": "full-access" }),
            json!({ "op": "message_send", "agent": "ops", "body": "x" }),
            json!({ "op": "mcp_set", "server": {} }),
            json!({ "op": "skill_install", "source": "anthropic", "skill": "x" }),
            json!({ "op": "device_list" }),
            json!({ "op": "device_revoke", "device": "dev-chat" }),
            json!({ "op": "pair_start" }),
            json!({ "op": "relay_set", "enabled": true }),
            json!({ "op": "connector_list" }),
            json!({ "op": "upload", "name": "x", "data": "" }),
            json!({ "op": "push_test" }),
            json!({ "op": "requirement_create", "text": "x" }),
        ] {
            assert!(allowed(&hub, client, op.clone()).is_err(), "{op}");
        }
    }

    #[test]
    fn approvals_from_chat_are_once_and_never_an_elevation() {
        let (hub, client, _dir) = setup("approve");
        hub.connectors.observe(
            "s1",
            &json!({ "type": "approval_request", "call_id": "c1", "kind": "command" }),
        );
        hub.connectors.observe(
            "s1",
            &json!({ "type": "approval_request", "call_id": "c2", "kind": "elevation" }),
        );
        let approve = |extra: Value| {
            let mut op =
                json!({ "op": "approve", "session": "s1", "call_id": "c1", "decision": "allow" });
            for (key, value) in extra.as_object().unwrap() {
                op[key] = value.clone();
            }
            allowed(&hub, client, op)
        };
        assert_eq!(approve(json!({})), Ok(()));
        assert_eq!(approve(json!({ "decision": "deny" })), Ok(()));
        assert_eq!(approve(json!({ "scope": "once" })), Ok(()));
        for extra in [
            json!({ "always": true }),
            json!({ "scope": "session" }),
            json!({ "scope": "project" }),
            json!({ "prefix": "npm install" }),
            json!({ "hunks": ["h1"] }),
            json!({ "add_dir": { "path": "/tmp" } }),
            json!({ "decision": "yes" }),
            json!({ "call_id": "unknown" }),
        ] {
            assert!(approve(extra.clone()).is_err(), "{extra}");
        }
        let elevation = |decision: &str| {
            allowed(
                &hub,
                client,
                json!({ "op": "approve", "session": "s1", "call_id": "c2", "decision": decision }),
            )
        };
        assert_eq!(elevation("allow"), Err(ELEVATION.to_string()));
        assert_eq!(elevation("deny"), Ok(()));
    }

    #[test]
    fn the_connector_settings_narrow_the_profile() {
        let (hub, client, _dir) = setup("settings");
        let connector = super::super::Connector {
            id: "tg-1".to_string(),
            platform: "telegram".to_string(),
            name: "@bot".to_string(),
            username: "bot".to_string(),
            bot_id: "1".to_string(),
            app_id: None,
            enabled: false,
            proxy: None,
            api: None,
            token: String::new(),
            sealed: String::new(),
            settings: super::super::Settings {
                approvals: false,
                notify: true,
                max_mode: "manual".to_string(),
            },
            created_at: 1,
        };
        crate::hub::lock(&hub.connectors.list).push(connector);
        hub.connectors.observe(
            "s1",
            &json!({ "type": "approval_request", "call_id": "c1", "kind": "command" }),
        );
        for op in [
            json!({ "op": "approve", "session": "s1", "call_id": "c1", "decision": "allow" }),
            json!({ "op": "user_message", "session": "s1", "content": "hi" }),
            json!({ "op": "set_approval_mode", "session": "s1", "mode": "auto" }),
        ] {
            assert!(allowed(&hub, client, op.clone()).is_err(), "{op}");
        }
        assert_eq!(
            allowed(
                &hub,
                client,
                json!({ "op": "set_approval_mode", "session": "s1", "mode": "manual" })
            ),
            Ok(())
        );
        // Revoked: nothing at all.
        hub.revoke_device("dev-chat").unwrap();
        assert!(allowed(&hub, client, json!({ "op": "session_list" })).is_err());
    }
}

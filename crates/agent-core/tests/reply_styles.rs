//! Reply styles against the fake model: the style a conversation uses
//! reaches the model as the system prompt's last section, a later change as
//! a note that keeps the request prefix, and the choice is saved with the
//! session. Nothing here reads the developer's `~/.lynshen`: `setup` points
//! HOME at a temporary folder.

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{requests, setup, temp_dir};
use lynshen_agent_core::{protocol, AgentCore, AgentEvent, ApprovalMode, ReplyStyleView};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn pump(core: &mut AgentCore) -> Vec<AgentEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let batch = core.poll_events();
        let ready = batch
            .iter()
            .any(|event| matches!(event, AgentEvent::Status(status) if status == "ready"));
        seen.extend(batch);
        if ready {
            return seen;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out; events: {seen:#?}");
}

/// A request as the provider reads it: the system prompt, the tools, then
/// the messages.
struct Request {
    system: String,
    tools: String,
    messages: Vec<Value>,
}

impl Request {
    fn from_body(body: &Value) -> Self {
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let (system, rest) = match messages.first() {
            Some(first) if first["role"] == "system" => (text(first), &messages[1..]),
            _ => (String::new(), &messages[..]),
        };
        Self {
            system,
            tools: body["tools"].to_string(),
            messages: rest.to_vec(),
        }
    }

    fn notes(&self) -> Vec<String> {
        self.messages
            .iter()
            .map(text)
            .filter(|text| text.starts_with("<system-reminder>"))
            .collect()
    }
}

fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn last_request() -> Request {
    Request::from_body(requests().last().expect("a request"))
}

fn turn(core: &mut AgentCore, message: &str) -> Request {
    core.submit_user_message(message.to_string());
    pump(core);
    last_request()
}

fn profile() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".lynshen")
}

/// Sets config.json's `reply_style` (None removes it), keeping the rest.
fn set_default_style(id: Option<&str>) {
    let path = profile().join("config.json");
    let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let object = config.as_object_mut().unwrap();
    match id {
        Some(id) => object.insert("reply_style".to_string(), json!(id)),
        None => object.remove("reply_style"),
    };
    fs::write(path, config.to_string()).unwrap();
}

fn style_events(events: &[AgentEvent]) -> Vec<ReplyStyleView> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ReplyStyle(view) => Some(view.clone()),
            _ => None,
        })
        .collect()
}

fn open(label: &str) -> AgentCore {
    let mut core = AgentCore::open(temp_dir(label)).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core
}

/// The part of a system prompt that providers cache across sessions.
fn stable(system: &str) -> &str {
    &system[..system.find("\n\n<env>\n").expect("an env block")]
}

#[test]
fn the_default_for_new_conversations_is_the_last_section_of_the_prompt() {
    let _guard = setup();
    set_default_style(None);
    let mut plain = open("style-plain");
    let unstyled = turn(&mut plain, "hello");
    assert!(!unstyled.system.contains("<reply_style"));
    assert_eq!(
        style_events(&plain.state_events()),
        [ReplyStyleView {
            style: "default".to_string(),
            name: "默认".to_string(),
            source: "builtin",
            chosen: false,
            missing: false,
        }]
    );

    set_default_style(Some("concise"));
    let mut styled = open("style-default");
    let first = turn(&mut styled, "hello");
    set_default_style(None);
    assert!(first.system.ends_with("</reply_style>"), "{}", first.system);
    assert!(first
        .system
        .contains("<reply_style id=\"concise\" name=\"简洁\">"));
    assert!(first.system.contains("第一句给出结论或结果"));
    // What providers cache across sessions is the same with any style.
    assert_eq!(stable(&first.system), stable(&unstyled.system));
    assert_eq!(first.tools, unstyled.tools);
    assert!(first.notes().is_empty());
}

#[test]
fn a_style_picked_mid_conversation_is_a_note_and_keeps_the_prefix() {
    let _guard = setup();
    set_default_style(None);
    let dir = temp_dir("style-switch");
    let mut core = AgentCore::open(dir.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    let first = turn(&mut core, "first");

    let (_, events) =
        protocol::apply_op(&mut core, &json!({ "op": "set_style", "style": "formal" }));
    let told = style_events(&events);
    assert_eq!(told.len(), 1);
    assert_eq!(
        (
            told[0].style.as_str(),
            told[0].name.as_str(),
            told[0].chosen
        ),
        ("formal", "正式汇报", true)
    );
    let second = turn(&mut core, "second");
    assert_eq!(second.system, first.system);
    assert_eq!(second.tools, first.tools);
    assert_eq!(second.messages[..first.messages.len()], first.messages[..]);
    let notes = second.notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].starts_with(
        "<system-reminder>\nThe reply style changed. From now on:\n<reply_style id=\"formal\""
    ));
    assert!(notes[0].contains("结论、背景、所做的工作、验证、风险与后续"));
    // The note comes just before the user's message.
    let messages = &second.messages;
    assert_eq!(text(&messages[messages.len() - 2]), notes[0]);
    assert_eq!(text(messages.last().unwrap()), "second");

    // Nothing changed: no new note.
    assert_eq!(turn(&mut core, "third").notes().len(), 1);

    // The choice is saved with the session.
    let session = core.session_id().to_string();
    drop(core);
    let mut core = AgentCore::open(dir).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.handle_command(&format!("/resume {session}"));
    let reopened = style_events(&core.state_events());
    assert_eq!(
        (reopened[0].style.as_str(), reopened[0].chosen),
        ("formal", true)
    );
    assert_eq!(turn(&mut core, "after reopening").system, first.system);

    // Following the default again (none): told once.
    let (_, events) = protocol::apply_op(&mut core, &json!({ "op": "set_style", "style": null }));
    assert!(!style_events(&events)[0].chosen);
    let back = turn(&mut core, "back to none");
    let notes = back.notes();
    assert_eq!(notes.len(), 2);
    assert!(
        notes[1].contains("The reply style is now the default"),
        "{}",
        notes[1]
    );
    assert_eq!(back.system, first.system);
}

#[test]
fn a_personal_style_file_works_and_an_unknown_id_changes_nothing() {
    let _guard = setup();
    set_default_style(None);
    let styles = profile().join("styles");
    fs::create_dir_all(&styles).unwrap();
    fs::write(
        styles.join("terse.md"),
        "---\nname: Terse\ndescription: One line\n---\nAnswer in one line.\n",
    )
    .unwrap();
    let mut core = open("style-file");
    let (_, events) =
        protocol::apply_op(&mut core, &json!({ "op": "set_style", "style": "terse" }));
    assert_eq!(style_events(&events)[0].source, "user");
    let first = turn(&mut core, "hello");
    assert!(first
        .system
        .contains("<reply_style id=\"terse\" name=\"Terse\">"));
    assert!(first
        .system
        .ends_with("Answer in one line.\n</reply_style>"));

    let (_, events) = protocol::apply_op(&mut core, &json!({ "op": "set_style", "style": "nope" }));
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::Error(error) if error.contains("unknown reply style: nope"))
    ));
    assert_eq!(style_events(&events)[0].style, "terse");
    let (_, events) = protocol::apply_op(&mut core, &json!({ "op": "set_style", "style": 3 }));
    assert!(matches!(&events[..], [AgentEvent::Error(_)]));

    // `/style` lists it; `--global` makes it the default, which the
    // conversation then follows.
    let (_, events) = core.handle_command("/style");
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::Info(text) if text.contains("* terse") && text.contains("简洁"))
    ));
    let (_, events) = core.handle_command("/style concise --global");
    let told = style_events(&events);
    assert_eq!((told[0].style.as_str(), told[0].chosen), ("concise", false));
    let config: Value =
        serde_json::from_str(&fs::read_to_string(profile().join("config.json")).unwrap()).unwrap();
    assert_eq!(config["reply_style"], "concise");
    assert_eq!(config["model"], "fake-model");

    // The file goes away: the conversation runs with no style, and says so.
    set_default_style(Some("terse"));
    fs::remove_file(styles.join("terse.md")).unwrap();
    let mut events = core.submit_user_message("after the file went away".to_string());
    events.extend(pump(&mut core));
    set_default_style(None);
    let told = style_events(&events);
    assert_eq!(
        (told[0].style.as_str(), told[0].source, told[0].missing),
        ("terse", "missing", true)
    );
    assert!(last_request()
        .notes()
        .last()
        .unwrap()
        .contains("The reply style is now the default"));
}

#[test]
fn a_subagent_gets_no_reply_style() {
    let _guard = setup();
    set_default_style(None);
    let mut core = open("style-subagent");
    protocol::apply_op(
        &mut core,
        &json!({ "op": "set_style", "style": "learning" }),
    );
    let start = requests().len();
    let args = json!({ "task_name": "checker", "message": "look around" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    pump(&mut core);
    let deadline = Instant::now() + Duration::from_secs(20);
    let child = loop {
        let child = requests()[start..].iter().find_map(|body| {
            let request = Request::from_body(body);
            request
                .messages
                .last()
                .is_some_and(|last| text(last).starts_with("<subagent_task"))
                .then_some(request)
        });
        if let Some(child) = child {
            break child;
        }
        assert!(Instant::now() < deadline, "the subagent sent no request");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(child.system.contains("<subagent_context>"));
    assert!(!child.system.contains("<reply_style"), "{}", child.system);
    let parent = Request::from_body(&requests()[start]);
    assert!(parent.system.contains("<reply_style id=\"learning\""));
}

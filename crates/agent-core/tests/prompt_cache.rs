//! How much of each request a provider can serve from its prompt cache: a
//! scripted session (plain turns, a tool call, plan mode on and off, a goal
//! created and finished, reopening, a compaction) against the fake model,
//! which records every request. Providers reuse the cache only for a
//! byte-identical prefix (system prompt, then tools, then the earlier
//! messages), so each request is compared with the one before it. Run with
//! `--nocapture` for the table.

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{requests, setup, temp_dir};
use lynshen_agent_core::{
    host::{HostExtensions, HostGate},
    AgentCore, AgentEvent, ApprovalMode,
};
use serde_json::Value;
use std::{
    fs,
    sync::{Arc, Mutex},
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

/// A request as the provider reads it, in cache order: the system prompt,
/// the tools, then each message.
struct Request {
    system: String,
    tools: String,
    messages: Vec<Value>,
}

impl Request {
    fn from_body(body: &Value) -> Self {
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let (system, rest) = match messages.first() {
            Some(first) if first["role"] == "system" => (first.to_string(), &messages[1..]),
            _ => (String::new(), &messages[..]),
        };
        Self {
            system,
            tools: body["tools"].to_string(),
            messages: rest.to_vec(),
        }
    }

    fn bytes(&self) -> String {
        let mut text = format!("{}{}", self.system, self.tools);
        for message in &self.messages {
            text.push_str(&message.to_string());
        }
        text
    }

    /// The engine's notes among the messages, in order.
    fn notes(&self) -> Vec<String> {
        self.messages
            .iter()
            .map(text)
            .filter(|text| text.starts_with("<system-reminder>"))
            .collect()
    }
}

fn last_request() -> Request {
    Request::from_body(requests().last().expect("a request"))
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

fn common_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

#[test]
fn a_scripted_session_keeps_the_request_prefix() {
    let _guard = setup();
    let dir = temp_dir("prompt-cache");
    fs::write(dir.join("notes.txt"), "hello\n").unwrap();
    let mut core = AgentCore::open(dir.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    let start = requests().len();
    // The step each request belongs to: (requests sent so far, step).
    let mut steps: Vec<(usize, &str)> = Vec::new();
    let mut step = |label, core: &mut AgentCore| {
        pump(core);
        steps.push((requests().len() - start, label));
    };

    core.submit_user_message("hello".to_string());
    step("plain turn", &mut core);
    core.submit_user_message("RUN: echo hi".to_string());
    step("tool call", &mut core);
    core.set_approval_mode(ApprovalMode::Plan);
    core.submit_user_message(r#"CALL read {"path":"notes.txt"}"#.to_string());
    step("plan mode on", &mut core);
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message("plan mode is off again".to_string());
    step("plan mode off", &mut core);
    // The second message waits for the first turn, so no goal continuation
    // runs in between.
    core.submit_user_message(r#"CALL create_goal {"objective":"ship it"}"#.to_string());
    core.submit_user_message(r#"CALL update_goal {"status":"complete"}"#.to_string());
    step("goal created, completed", &mut core);
    let session = core.session_id().to_string();
    drop(core);
    let mut core = AgentCore::open(dir.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.handle_command(&format!("/resume {session}"));
    core.submit_user_message("after reopening".to_string());
    step("session reopened", &mut core);
    core.submit_user_message("lorem ipsum dolor sit amet ".repeat(6000));
    step("long message", &mut core);
    core.submit_user_message("short again".to_string());
    step("short message", &mut core);
    let (_, events) = core.handle_command("/compact");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Info(info) if info.contains("nothing"))),
        "no compaction: {events:#?}"
    );
    step("compaction", &mut core);
    core.submit_user_message("after the compaction".to_string());
    step("turn after compaction", &mut core);

    let sent: Vec<Request> = requests()[start..].iter().map(Request::from_body).collect();
    let (mut full, mut same_system, mut same_tools) = (0, 0, 0);
    let (mut shared, mut total) = (0usize, 0usize);
    let mut at = 0;
    println!("request | step | system | tools | shared prefix / bytes");
    for (index, pair) in sent.windows(2).enumerate() {
        let (previous, current) = (&pair[0], &pair[1]);
        let (before, now) = (previous.bytes(), current.bytes());
        let prefix = common_prefix(&before, &now);
        while steps[at].0 < index + 2 {
            at += 1;
        }
        let same = |same: bool| if same { "same" } else { "CHANGED" };
        full += usize::from(prefix == before.len());
        same_system += usize::from(previous.system == current.system);
        same_tools += usize::from(previous.tools == current.tools);
        shared += prefix;
        total += now.len();
        println!(
            "{:>7} | {} | {} | {} | {prefix} / {}",
            index + 2,
            steps[at].1,
            same(previous.system == current.system),
            same(previous.tools == current.tools),
            now.len()
        );
    }
    let compared = sent.len() - 1;
    println!(
        "{compared} requests after the first: {full} start with the whole previous request, {same_system} keep its system prompt, {same_tools} keep its tools; {:.1}% of the bytes sent are a cached prefix",
        shared as f64 * 100.0 / total as f64
    );
    // Plan mode, the goal, reopening and the compaction leave the system
    // prompt and the tools as they were.
    assert_eq!((same_system, same_tools), (compared, compared));
    // Each request starts with the whole one before it but two: the summary
    // request ends with its request instead of the latest user message, and
    // after it the summary replaces the conversation.
    assert_eq!(full, compared - 2);
}

#[test]
fn a_plan_mode_switch_keeps_the_system_prompt_and_the_tools() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("plan-switch")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message("before".to_string());
    pump(&mut core);
    let before = last_request();
    core.set_approval_mode(ApprovalMode::Plan);
    core.submit_user_message("in plan mode".to_string());
    pump(&mut core);
    let during = last_request();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message("after".to_string());
    pump(&mut core);
    let after = last_request();

    for (previous, next) in [(&before, &during), (&during, &after)] {
        assert_eq!(next.system, previous.system);
        assert_eq!(next.tools, previous.tools);
        assert_eq!(
            next.messages[..previous.messages.len()],
            previous.messages[..]
        );
    }
    assert!(during.tools.contains("\"propose_plan\""));
    // The switch is a note, just before the user's message.
    let second_last = |request: &Request| text(&request.messages[request.messages.len() - 2]);
    assert!(second_last(&during).starts_with("<system-reminder>\n<plan_mode>"));
    assert_eq!(text(during.messages.last().unwrap()), "in plan mode");
    assert!(second_last(&after).contains("Plan mode is off now."));
    assert_eq!(after.notes().len(), 2);
}

#[test]
fn the_host_state_reaches_the_model_as_a_note_when_it_changes() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("host-state")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    let state = Arc::new(Mutex::new("<state>one</state>".to_string()));
    let read = Arc::clone(&state);
    core.set_host_extensions(HostExtensions {
        tools: Vec::new(),
        run_tool: Arc::new(|_, _, _| (String::new(), true)),
        prompt: Arc::new(|| "<brief>steady</brief>".to_string()),
        context: Arc::new(move || read.lock().unwrap().clone()),
        exclusive: false,
        gate: Arc::new(|_| HostGate::Run),
        summary: Arc::new(|_, _| String::new()),
    });

    core.submit_user_message("first".to_string());
    pump(&mut core);
    let first = last_request();
    assert!(first.system.contains("<brief>steady</brief>"));
    assert_eq!(
        first.notes(),
        ["<system-reminder>\n<state>one</state>\n</system-reminder>"]
    );
    core.submit_user_message("second".to_string());
    pump(&mut core);
    assert_eq!(last_request().notes().len(), 1);

    *state.lock().unwrap() = "<state>two</state>".to_string();
    core.submit_user_message("third".to_string());
    pump(&mut core);
    let third = last_request();
    assert_eq!(third.system, first.system);
    assert_eq!(
        third.notes(),
        [
            "<system-reminder>\n<state>one</state>\n</system-reminder>",
            "<system-reminder>\n<state>two</state>\n</system-reminder>"
        ]
    );
}

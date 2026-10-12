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
    sandbox::{SandboxMode, SandboxPolicy},
    AgentCore, AgentEvent, ApprovalMode,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
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

    /// The system prompt's text.
    fn system_text(&self) -> String {
        serde_json::from_str::<Value>(&self.system)
            .map(|system| text(&system))
            .unwrap_or_default()
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

#[test]
fn a_changed_host_text_builds_the_system_prompt_again() {
    let _guard = setup();
    let dir = temp_dir("host-prompt");
    let brief = Arc::new(Mutex::new("<state>v1</state>".to_string()));
    let host = |brief: &Arc<Mutex<String>>| {
        let read = Arc::clone(brief);
        HostExtensions {
            tools: Vec::new(),
            run_tool: Arc::new(|_, _, _| (String::new(), true)),
            prompt: Arc::new(move || read.lock().unwrap().clone()),
            context: Arc::new(String::new),
            exclusive: false,
            gate: Arc::new(|_| HostGate::Run),
            summary: Arc::new(|_, _| String::new()),
        }
    };
    let mut core = AgentCore::open(dir.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.set_host_extensions(host(&brief));
    let turn = |core: &mut AgentCore, message: &str| {
        core.submit_user_message(message.to_string());
        pump(core);
        last_request()
    };
    let first = turn(&mut core, "first");
    assert!(first.system_text().ends_with("<state>v1</state>"));
    // Nothing changed: the same prefix.
    let second = turn(&mut core, "second");
    assert_eq!(second.system, first.system);
    assert_eq!(second.messages[..first.messages.len()], first.messages[..]);

    // The agent's brief changed between turns (its `brief` tool, say).
    *brief.lock().unwrap() = "<state>v2</state>".to_string();
    let third = turn(&mut core, "third");
    assert!(third.system_text().ends_with("<state>v2</state>"));
    assert_eq!(third.messages[..second.messages.len()], second.messages[..]);
    assert!(third.notes().is_empty());
    assert_eq!(turn(&mut core, "fourth").system, third.system);

    // A reopened session takes the host's text as it is now.
    let session = core.session_id().to_string();
    drop(core);
    *brief.lock().unwrap() = "<state>v3</state>".to_string();
    let mut core = AgentCore::open(dir.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.set_host_extensions(host(&brief));
    core.handle_command(&format!("/resume {session}"));
    assert!(turn(&mut core, "reopened")
        .system_text()
        .ends_with("<state>v3</state>"));
}

fn sandbox(mode: SandboxMode) -> Option<SandboxPolicy> {
    Some(SandboxPolicy {
        mode,
        ..SandboxPolicy::default_for_platform()
    })
}

#[test]
fn a_sandbox_change_reaches_the_model_as_a_note() {
    let _guard = setup();
    let dir = temp_dir("sandbox-note");
    let mut core = AgentCore::open(dir.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.set_sandbox(sandbox(SandboxMode::WorkspaceWrite));
    let turn = |core: &mut AgentCore, message: &str| {
        core.submit_user_message(message.to_string());
        pump(core);
        last_request()
    };
    let first = turn(&mut core, "first");
    assert!(first
        .system_text()
        .contains("<sandbox mode=\"workspace-write\">"));
    assert!(first.notes().is_empty());

    // `/sandbox read-only` mid-session.
    core.set_sandbox(sandbox(SandboxMode::ReadOnly));
    let read_only = turn(&mut core, "read-only now");
    assert_eq!(read_only.system, first.system);
    assert_eq!(read_only.tools, first.tools);
    let note = &read_only.notes()[0];
    assert!(note.contains("<sandbox mode=\"read-only\">"), "{note}");
    assert!(note.contains("Nothing is writable."), "{note}");

    // The mode a closed engine kept, taken back by the session's next one.
    let mut other = AgentCore::open(temp_dir("sandbox-kept")).unwrap();
    other.set_sandbox(sandbox(SandboxMode::WorkspaceWrite));
    core.restore_kept_state(other.kept_state());
    let restored = turn(&mut core, "workspace again");
    assert_eq!(restored.notes().len(), 2);
    assert!(restored.notes()[1].contains("<sandbox mode=\"workspace-write\">"));

    // A directory added to the workspace.
    let extra = temp_dir("sandbox-extra");
    core.add_writable_dirs(std::slice::from_ref(&extra));
    let widened = turn(&mut core, "with another directory");
    let notes = widened.notes();
    assert_eq!(notes.len(), 3);
    assert!(
        notes[2].contains(&extra.display().to_string()),
        "{}",
        notes[2]
    );

    // Nothing changed: no note.
    assert_eq!(turn(&mut core, "same again").notes(), notes);
    assert_eq!(last_request().system, first.system);
}

#[test]
fn a_subagent_is_told_the_sandbox_of_now() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("subagent-sandbox")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.set_sandbox(sandbox(SandboxMode::WorkspaceWrite));
    core.submit_user_message("first".to_string());
    pump(&mut core);
    let parent = last_request();

    // `/sandbox read-only`, then a spawn.
    core.set_sandbox(sandbox(SandboxMode::ReadOnly));
    let start = requests().len();
    let args = json!({ "task_name": "checker", "message": "look around" });
    core.submit_user_message(format!("CALL spawn_agent {args}"));
    pump(&mut core);
    let deadline = Instant::now() + Duration::from_secs(20);
    let child = loop {
        let sent = requests();
        let child = sent[start..].iter().find(|body| {
            let last = body["messages"].as_array().and_then(|m| m.last()).cloned();
            last.is_some_and(|last| text(&last).starts_with("<subagent_task"))
        });
        if let Some(child) = child {
            break Request::from_body(child);
        }
        assert!(Instant::now() < deadline, "the subagent sent no request");
        thread::sleep(Duration::from_millis(20));
    };
    // The parent keeps its prompt and is told by a note.
    assert_eq!(last_request().system, parent.system);
    let system = child.system_text();
    assert!(system.contains("<sandbox mode=\"read-only\">"), "{system}");
    assert!(!system.contains("workspace-write"), "{system}");
    assert!(system.contains("<subagent_context>"));
}

#[test]
fn propose_plan_outside_plan_mode_shows_no_draft() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("draft-outside-plan")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message(
        r###"CALL propose_plan {"title":"Snake","plan":"## Goal\nShip it"}"###.to_string(),
    );
    let events = pump(&mut core);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PlanDraft { .. })),
        "{events:#?}"
    );
    // The call itself fails.
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolOutput { output, is_error: true, .. } if output.contains("only available in plan mode")
    )));

    // In plan mode the plan is shown as it is written.
    core.set_approval_mode(ApprovalMode::Plan);
    core.submit_user_message(
        r###"CALL propose_plan {"title":"Snake","plan":"## Goal\nShip it"}"###.to_string(),
    );
    let events = pump(&mut core);
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::PlanDraft { title, .. } if title == "Snake")));
}

/// The summary of the first compaction in `events`; panics without one.
fn compaction_summary(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::CompactionEnd { summary } => Some(summary.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no compaction: {events:#?}"))
}

fn assistant_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AssistantDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_summary_answered_with_a_tool_call_is_asked_again_without_tools() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("summary-tool-call")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message("SUMMARY_CALLS_TOOL first".to_string());
    pump(&mut core);
    core.submit_user_message("lorem ipsum dolor sit amet ".repeat(6000));
    pump(&mut core);
    let start = requests().len();

    // Refused as too long: the turn compacts and retries.
    core.submit_user_message("OVERFLOW go on".to_string());
    let events = pump(&mut core);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(_) | AgentEvent::CompactionFailed(_)
        )),
        "{events:#?}"
    );
    assert!(compaction_summary(&events).starts_with("summary without tools"));
    assert_eq!(assistant_text(&events), "user said: OVERFLOW go on");

    // The retry is the same request without tools.
    let sent = requests()[start..].to_vec();
    let asks: Vec<&Value> = sent
        .iter()
        .filter(|body| {
            let last = body["messages"].as_array().and_then(|m| m.last()).cloned();
            last.is_some_and(|last| text(&last).starts_with("<compaction_request>"))
        })
        .collect();
    assert_eq!(asks.len(), 2, "{sent:#?}");
    // Both with less reasoning than the turns (the model offers "low").
    assert_eq!(sent.last().unwrap()["reasoning_effort"], "medium");
    assert!(asks.iter().all(|ask| ask["reasoning_effort"] == "low"));
    assert!(asks[0]["tools"].is_array());
    assert!(asks[1].get("tools").is_none());
    assert_eq!(asks[0]["messages"], asks[1]["messages"]);
}

#[test]
fn a_summary_answer_that_also_calls_a_tool_is_asked_again_without_tools() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("summary-preamble")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message("SUMMARY_PREAMBLE_CALLS_TOOL first".to_string());
    pump(&mut core);
    core.submit_user_message("lorem ipsum dolor sit amet ".repeat(6000));
    pump(&mut core);
    core.submit_user_message("short again".to_string());
    pump(&mut core);
    let start = requests().len();

    let (_, mut events) = core.handle_command("/compact");
    events.extend(pump(&mut core));
    // "I'll read notes.txt first." leads up to the call: the summary is the
    // answer to the request without tools.
    let summary = compaction_summary(&events);
    assert!(summary.starts_with("summary without tools"), "{summary}");
    assert!(!summary.contains("notes.txt first"), "{summary}");
    let asks = requests()[start..].to_vec();
    assert_eq!(asks.len(), 2, "{asks:#?}");
    assert!(asks[0]["tools"].is_array());
    assert!(asks[1].get("tools").is_none());
}

#[test]
fn a_summary_the_model_only_answers_with_tool_calls_falls_back_to_plain_text() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("summary-only-tool-calls")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message("SUMMARY_ONLY_CALLS_TOOL first".to_string());
    pump(&mut core);
    core.submit_user_message("lorem ipsum dolor sit amet ".repeat(6000));
    pump(&mut core);
    core.submit_user_message("short again".to_string());
    pump(&mut core);

    let (_, mut events) = core.handle_command("/compact");
    events.extend(pump(&mut core));
    let summary = compaction_summary(&events);
    // The summarizer got the conversation as text.
    assert!(
        summary.starts_with("user said: Summarize this earlier conversation:\n\nUser: SUMMARY_ONLY_CALLS_TOOL first\nAssistant: user said: SUMMARY_ONLY_CALLS_TOOL first"),
        "{summary}"
    );
    let last = requests().last().cloned().unwrap();
    assert!(last.get("tools").is_none());
}

/// config.json with `changes` set, until dropped: the engines opened
/// meanwhile read them.
struct ConfigChange {
    path: PathBuf,
    original: String,
}

impl ConfigChange {
    fn set(changes: Value) -> Self {
        let path = PathBuf::from(std::env::var("HOME").unwrap())
            .join(".lynshen")
            .join("config.json");
        let original = fs::read_to_string(&path).unwrap();
        let mut config: Value = serde_json::from_str(&original).unwrap();
        for (key, value) in changes.as_object().unwrap() {
            config[key] = value.clone();
        }
        fs::write(&path, config.to_string()).unwrap();
        Self { path, original }
    }
}

impl Drop for ConfigChange {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.original);
    }
}

#[test]
fn a_refused_summary_request_falls_back_to_plain_text_on_the_compact_model() {
    let _guard = setup();
    let _config = ConfigChange::set(json!({
        "models": [{ "name": "fake-model" }, { "name": "fake-compact" }],
        "compact_model": "fake-compact",
    }));
    let mut core = AgentCore::open(temp_dir("summary-refused")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message("SUMMARY_REFUSED first".to_string());
    pump(&mut core);
    core.submit_user_message("lorem ipsum dolor sit amet ".repeat(6000));
    pump(&mut core);
    core.submit_user_message("short again".to_string());
    pump(&mut core);
    let start = requests().len();

    let (_, mut events) = core.handle_command("/compact");
    events.extend(pump(&mut core));
    let summary = compaction_summary(&events);
    assert!(
        summary.starts_with(
            "user said: Summarize this earlier conversation:\n\nUser: SUMMARY_REFUSED first"
        ),
        "{summary}"
    );
    // The refused request goes as a turn's, the plain text to compact_model;
    // the refused one is not sent again with tools off.
    let sent = requests()[start..].to_vec();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[0]["model"], "fake-model");
    assert!(sent[0]["tools"].is_array());
    assert_eq!(sent[1]["model"], "fake-compact");
    assert!(sent[1].get("tools").is_none());
}

#[test]
fn a_trust_decision_reaches_the_kept_system_prompt() {
    let _guard = setup();
    let dir = temp_dir("trust-rebuild");
    let skill = dir.join(".lynshen").join("skills").join("checklist");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: checklist\ndescription: Run the release checklist\n---\nSteps.\n",
    )
    .unwrap();
    let mut core = AgentCore::open(dir.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    let turn = |core: &mut AgentCore, message: &str| {
        core.submit_user_message(message.to_string());
        pump(core);
        last_request()
    };
    let untrusted = turn(&mut core, "first");
    assert!(!untrusted
        .system_text()
        .contains("Run the release checklist"));

    core.handle_command("/trust yes");
    let trusted = turn(&mut core, "trusted now");
    assert!(trusted.system_text().contains("Run the release checklist"));
    // Built again once: the next turn keeps it.
    assert_eq!(turn(&mut core, "again").system, trusted.system);

    // So does a reopened session.
    let session = core.session_id().to_string();
    drop(core);
    let mut core = AgentCore::open(dir.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.handle_command(&format!("/resume {session}"));
    core.handle_command("/trust no");
    let reopened = turn(&mut core, "after reopening");
    assert!(!reopened.system_text().contains("Run the release checklist"));
    assert_eq!(reopened.system, untrusted.system);
}

/// Skill folders and skills-state.json as they were, once dropped: the
/// tests share one HOME.
struct SkillsRestore {
    dirs: Vec<PathBuf>,
    state: PathBuf,
    original: Option<String>,
}

impl Drop for SkillsRestore {
    fn drop(&mut self) {
        for dir in &self.dirs {
            let _ = fs::remove_dir_all(dir);
        }
        match &self.original {
            Some(original) => {
                let _ = fs::write(&self.state, original);
            }
            None => {
                let _ = fs::remove_file(&self.state);
            }
        }
    }
}

#[test]
fn a_skill_switched_off_or_on_reaches_the_kept_system_prompt() {
    let _guard = setup();
    let own_root = lynshen_agent_core::skills::profile_skills_dir().unwrap();
    let own = own_root.join("tidy-imports");
    let shared = PathBuf::from(std::env::var("HOME").unwrap())
        .join(".agents")
        .join("skills")
        .join("release-notes");
    let state = own_root.with_file_name("skills-state.json");
    let restore = SkillsRestore {
        dirs: vec![own.clone(), shared.clone(), own_root.join("draft-notes")],
        original: fs::read_to_string(&state).ok(),
        state,
    };
    for (dir, name, description) in [
        (&own, "tidy-imports", "Sort and prune imports"),
        (&shared, "release-notes", "Write the release notes"),
    ] {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nSteps.\n"),
        )
        .unwrap();
    }
    let mut core = AgentCore::open(temp_dir("skill-switch")).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    let turn = |core: &mut AgentCore, message: &str| {
        core.submit_user_message(message.to_string());
        pump(core);
        last_request()
    };
    let first = turn(&mut core, "first");
    assert!(first.system_text().contains("Sort and prune imports"));
    assert!(first.system_text().contains("Write the release notes"));
    // Nothing changed: the same bytes.
    let second = turn(&mut core, "second");
    assert_eq!(second.system, first.system);
    assert_eq!(second.messages[..first.messages.len()], first.messages[..]);

    // Off by name (skills-state.json's `disabled`), as `/skills disable` does.
    core.handle_command("/skills disable tidy-imports");
    let third = turn(&mut core, "third");
    assert!(!third.system_text().contains("Sort and prune imports"));
    assert!(third.system_text().contains("Write the release notes"));
    // Only the system prompt changed; the conversation is kept as it was.
    assert_eq!(third.messages[..second.messages.len()], second.messages[..]);
    assert!(third.notes().is_empty());
    assert_eq!(turn(&mut core, "fourth").system, third.system);

    // Off by path (`disabled_paths`), as the desktop switches a shared skill.
    lynshen_agent_core::skills::switch_skill(&shared, false).unwrap();
    assert!(fs::read_to_string(&restore.state)
        .unwrap()
        .contains("\"disabled_paths\""));
    let fifth = turn(&mut core, "fifth");
    assert!(!fifth.system_text().contains("Write the release notes"));
    assert_ne!(fifth.system, third.system);
    assert_eq!(turn(&mut core, "sixth").system, fifth.system);

    // Both on again: the first turn's bytes.
    core.handle_command("/skills enable tidy-imports");
    lynshen_agent_core::skills::switch_skill(&shared, true).unwrap();
    assert_eq!(turn(&mut core, "seventh").system, first.system);

    // A skill created, then deleted.
    let created =
        lynshen_agent_core::skills::create_skill(&own_root, "draft-notes", "Draft notes").unwrap();
    assert!(turn(&mut core, "eighth")
        .system_text()
        .contains("<skill name=\"draft-notes\""));
    lynshen_agent_core::skills::delete_skill(&own_root, created.parent().unwrap()).unwrap();
    assert_eq!(turn(&mut core, "ninth").system, first.system);
}

#[test]
fn a_chat_session_is_not_offered_propose_plan() {
    let _guard = setup();
    let chats = lynshen_agent_core::chat::ensure_chats_dir().unwrap();
    let mut core = AgentCore::open(chats).unwrap();
    assert!(core.is_chat());
    core.set_approval_mode(ApprovalMode::FullAccess);
    core.submit_user_message("hello".to_string());
    pump(&mut core);
    let tools = last_request().tools;
    assert!(tools.contains("\"update_plan\""));
    assert!(!tools.contains("\"propose_plan\""));
}

#[test]
fn a_chat_session_in_plan_mode_is_not_sent_to_propose_plan() {
    let _guard = setup();
    let chats = lynshen_agent_core::chat::ensure_chats_dir().unwrap();
    let mut core = AgentCore::open(chats.clone()).unwrap();
    core.set_approval_mode(ApprovalMode::Plan);
    let call = json!({ "path": "plan-mode-report.md", "content": "x" });
    core.submit_user_message(format!("CALL write {call}"));
    let events = pump(&mut core);
    // Writes stay refused, as before the tool list was fixed, but the
    // refusal does not name a tool the session is not offered.
    let output = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolOutput {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("a tool output");
    assert!(output.1, "{output:?}");
    assert!(output.0.contains("Describe the change"), "{output:?}");
    assert!(!output.0.contains("propose_plan"), "{output:?}");
    assert!(!chats.join("plan-mode-report.md").exists());
    // The model answers with text and the turn ends.
    assert!(assistant_text(&events).starts_with("tool said:"));
    assert!(!last_request().tools.contains("\"propose_plan\""));
}

#[test]
fn a_rewound_message_sent_again_in_plan_mode_gets_one_reminder() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("plan-rewind")).unwrap();
    core.set_approval_mode(ApprovalMode::Plan);
    let turn = |core: &mut AgentCore, message: &str| {
        core.submit_user_message(message.to_string());
        pump(core);
        last_request()
    };
    turn(&mut core, "plan something");
    let sent = turn(&mut core, "and more");
    assert_eq!(sent.notes().len(), 2);

    let (_, events) = core.handle_command("/rewind");
    let id = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::CheckpointView(turns) => turns
                .iter()
                .find(|turn| turn.label == "and more")
                .map(|turn| turn.id.clone()),
            _ => None,
        })
        .expect("a rewind point");
    core.handle_command(&format!("/rewind {id}"));
    let again = turn(&mut core, "and more, again");
    // The same request as before, with the new message.
    assert_eq!(again.notes(), sent.notes());
    assert_eq!(again.messages.len(), sent.messages.len());
    assert_eq!(text(again.messages.last().unwrap()), "and more, again");
}

#[test]
fn each_user_turn_in_plan_mode_ends_with_a_short_reminder() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("plan-reminder")).unwrap();
    core.set_approval_mode(ApprovalMode::Plan);
    let turn = |core: &mut AgentCore, message: &str| {
        core.submit_user_message(message.to_string());
        pump(core);
        last_request()
    };
    let first = turn(&mut core, "plan something");
    assert_eq!(first.notes().len(), 1);
    assert!(first.notes()[0].contains("<plan_mode>"));
    let second = turn(&mut core, "and more");
    let notes = second.notes();
    assert_eq!(notes.len(), 2);
    assert!(notes[1].contains("Plan mode is still on"), "{}", notes[1]);
    assert!(!notes[1].contains("<plan_mode>"));
    // Just before the user's message, which stays last.
    let messages = &second.messages;
    assert_eq!(text(&messages[messages.len() - 2]), notes[1]);
    assert_eq!(text(messages.last().unwrap()), "and more");
}

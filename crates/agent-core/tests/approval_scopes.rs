//! How far an approval reaches (`approve` with `scope`): once, for the
//! session (a command prefix, never the whole shell), or saved as an allow
//! rule for the project. A deny can tell the model what to do instead.
//!
//! Runs against the temporary HOME and fake model of `support/fake_model.rs`
//! (`RUN: <command>`, `CALL <tool> <arguments>`).

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{setup, temp_dir};
use lynshen_agent_core::{
    approval::{ApprovalInfo, ApprovalKind, ApprovalScope},
    AgentCore, AgentEvent, ApprovalMode, ApproveOptions,
};
use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

fn pump(core: &mut AgentCore, done: impl Fn(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let batch = core.poll_events();
        let finished = batch.iter().any(&done);
        seen.extend(batch);
        if finished {
            return seen;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out; events: {seen:#?}");
}

fn is_ready(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::Status(status) if status == "ready")
}

fn is_approval(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::ApprovalRequest { .. })
}

fn approval(events: &[AgentEvent]) -> Option<(String, ApprovalInfo)> {
    events.iter().find_map(|event| match event {
        AgentEvent::ApprovalRequest { call_id, info, .. } => Some((call_id.clone(), info.clone())),
        _ => None,
    })
}

fn errors(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Error(message) => Some(message.clone()),
            _ => None,
        })
        .collect()
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

fn open(cwd: &Path, mode: ApprovalMode) -> AgentCore {
    let mut core = AgentCore::open(cwd.to_path_buf()).unwrap();
    core.set_approval_mode(mode);
    core
}

fn submit(core: &mut AgentCore, message: String) -> Vec<AgentEvent> {
    core.submit_user_message(message);
    pump(core, |event| is_ready(event) || is_approval(event))
}

fn run(core: &mut AgentCore, command: &str) -> Vec<AgentEvent> {
    submit(core, format!("RUN: {command}"))
}

fn escalate(core: &mut AgentCore, command: &str) -> Vec<AgentEvent> {
    let arguments = json!({ "command": command, "escalate": true, "justification": "test" });
    submit(core, format!("CALL bash {arguments}"))
}

fn allow(core: &mut AgentCore, call_id: &str, options: ApproveOptions) -> Vec<AgentEvent> {
    let mut events = core.approve_with(call_id, true, options);
    events.extend(pump(core, is_ready));
    events
}

fn config_path() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap()).join(".lynshen/config.json")
}

fn config() -> Value {
    serde_json::from_str(&fs::read_to_string(config_path()).unwrap()).unwrap()
}

/// Puts config.json back as it was when dropped: the tests share it.
struct RestoreConfig(String);

impl RestoreConfig {
    fn new() -> Self {
        Self(fs::read_to_string(config_path()).unwrap())
    }
}

impl Drop for RestoreConfig {
    fn drop(&mut self) {
        fs::write(config_path(), &self.0).unwrap();
    }
}

#[test]
fn a_session_approval_remembers_the_command_prefix_not_the_shell() {
    let _guard = setup();
    let dir = temp_dir("scope-session");
    let mut core = open(&dir, ApprovalMode::Manual);

    let events = run(&mut core, "printf ok > one.txt");
    let (call_id, info) = approval(&events).expect("manual asks");
    assert_eq!(info.kind, ApprovalKind::Command);
    assert_eq!(info.prefix.as_deref(), Some("printf ok"));
    // In an OS sandbox nothing is saved for the project: an allow rule
    // would let the command out of the sandbox.
    let enforced = cfg!(any(target_os = "macos", target_os = "linux"));
    assert_eq!(
        info.scopes,
        if enforced {
            vec![ApprovalScope::Once, ApprovalScope::Session]
        } else {
            vec![
                ApprovalScope::Once,
                ApprovalScope::Session,
                ApprovalScope::Project,
            ]
        }
    );
    let events = allow(
        &mut core,
        &call_id,
        ApproveOptions {
            scope: Some(ApprovalScope::Session),
            ..ApproveOptions::default()
        },
    );
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(fs::read_to_string(dir.join("one.txt")).unwrap(), "ok");

    // The same prefix runs unasked; another command still asks.
    let events = run(&mut core, "printf ok > two.txt");
    assert!(approval(&events).is_none(), "{events:#?}");
    assert_eq!(fs::read_to_string(dir.join("two.txt")).unwrap(), "ok");
    // What the user allowed ran in the sandbox: leaving it still asks.
    if enforced {
        let events = escalate(&mut core, "printf ok > out.txt");
        let (call_id, info) = approval(&events).expect("leaving the sandbox asks");
        assert_eq!(info.kind, ApprovalKind::Escalation);
        core.approve(&call_id, false, false, None);
        pump(&mut core, is_ready);
        assert!(!dir.join("out.txt").exists());
    }
    let events = run(&mut core, "printf other > three.txt");
    let (call_id, _) = approval(&events).expect("the shell is not allowed whole");
    core.approve(&call_id, false, false, None);
    pump(&mut core, is_ready);
    assert!(!dir.join("three.txt").exists());

    // The legacy `always` is a session approval of the prefix.
    let events = run(&mut core, "printf legacy > four.txt");
    let (call_id, _) = approval(&events).expect("asks");
    core.approve(&call_id, true, true, None);
    pump(&mut core, is_ready);
    let events = run(&mut core, "printf legacy > five.txt");
    assert!(approval(&events).is_none(), "{events:#?}");

    // What the session allowed goes to the session's next engine.
    let mut next = open(&temp_dir("scope-session-next"), ApprovalMode::Manual);
    next.restore_kept_state(core.kept_state());
    let events = run(&mut next, "printf ok > six.txt");
    assert!(approval(&events).is_none(), "{events:#?}");
}

#[test]
fn a_project_approval_saves_an_allow_rule_for_the_project() {
    let _guard = setup();
    let _restore = RestoreConfig::new();
    let dir = temp_dir("scope-project");
    let mut core = open(&dir, ApprovalMode::AutoEdit);

    let events = escalate(&mut core, "printf saved > out.txt");
    let (call_id, info) = approval(&events).expect("auto-edit asks to leave the sandbox");
    assert_eq!(info.kind, ApprovalKind::Escalation);
    assert_eq!(info.justification.as_deref(), Some("test"));
    let events = allow(
        &mut core,
        &call_id,
        ApproveOptions {
            scope: Some(ApprovalScope::Project),
            ..ApproveOptions::default()
        },
    );
    assert!(errors(&events).is_empty(), "{events:#?}");
    let key = dir.display().to_string();
    assert_eq!(
        config()["projects"][&key]["command_rules"],
        json!([{ "prefix": "printf saved", "action": "allow" }])
    );

    // A new engine for the project reads it; another project does not.
    let mut again = open(&dir, ApprovalMode::AutoEdit);
    let events = escalate(&mut again, "printf saved > again.txt");
    assert!(approval(&events).is_none(), "{events:#?}");
    assert_eq!(fs::read_to_string(dir.join("again.txt")).unwrap(), "saved");
    let other = temp_dir("scope-project-other");
    let mut elsewhere = open(&other, ApprovalMode::AutoEdit);
    let events = escalate(&mut elsewhere, "printf saved > x.txt");
    let (call_id, _) = approval(&events).expect("another project still asks");

    // With always_allow_scope "global" the rule goes to every project.
    let mut saved = config();
    saved["always_allow_scope"] = json!("global");
    fs::write(config_path(), saved.to_string()).unwrap();
    allow(
        &mut elsewhere,
        &call_id,
        ApproveOptions {
            scope: Some(ApprovalScope::Project),
            ..ApproveOptions::default()
        },
    );
    let rules = config()["command_rules"].clone();
    assert!(
        rules
            .as_array()
            .unwrap()
            .contains(&json!({ "prefix": "printf saved", "action": "allow" })),
        "{rules}"
    );
    assert!(config()["projects"]
        .get(other.display().to_string())
        .is_none());
}

#[test]
fn a_prefix_may_only_narrow_and_a_scope_must_be_offered() {
    let _guard = setup();
    let dir = temp_dir("scope-checks");
    let mut core = open(&dir, ApprovalMode::Manual);

    let events = run(&mut core, "printf narrow wide > a.txt");
    let (call_id, info) = approval(&events).expect("asks");
    assert_eq!(info.prefix.as_deref(), Some("printf narrow"));
    // Shorter, or longer with words the command does not have (another
    // rule's prefix, say): refused, and the request keeps waiting.
    for prefix in [
        "printf",
        "printf narrow other",
        "printf narrow wide > a.txt",
    ] {
        let rejected = core.approve_with(
            &call_id,
            true,
            ApproveOptions {
                scope: Some(ApprovalScope::Session),
                prefix: Some(prefix.to_string()),
                ..ApproveOptions::default()
            },
        );
        assert!(
            errors(&rejected)[0].contains("must start with `printf narrow`"),
            "{prefix}: {rejected:#?}"
        );
    }
    // The command's own next words are fine.
    let events = allow(
        &mut core,
        &call_id,
        ApproveOptions {
            scope: Some(ApprovalScope::Session),
            prefix: Some("printf narrow wide".to_string()),
            ..ApproveOptions::default()
        },
    );
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert!(dir.join("a.txt").exists());
    let events = run(&mut core, "printf narrow other > b.txt");
    let (call_id, _) = approval(&events).expect("only the narrower prefix was allowed");
    core.approve(&call_id, false, false, None);
    pump(&mut core, is_ready);

    // A delete is approved once at most.
    fs::create_dir_all(dir.join("build")).unwrap();
    let events = run(&mut core, "rm -r build");
    let (call_id, info) = approval(&events).expect("asks");
    assert_eq!(info.scopes, vec![ApprovalScope::Once]);
    assert_eq!(info.prefix, None);
    let rejected = core.approve_with(
        &call_id,
        true,
        ApproveOptions {
            scope: Some(ApprovalScope::Session),
            ..ApproveOptions::default()
        },
    );
    assert!(errors(&rejected)[0].contains("can be allowed only once"));
    // The legacy `always` falls back to once.
    core.approve(&call_id, true, true, None);
    pump(&mut core, is_ready);
    assert!(!dir.join("build").exists());
    fs::create_dir_all(dir.join("build")).unwrap();
    let events = run(&mut core, "rm -r build");
    assert!(approval(&events).is_some(), "asked again: {events:#?}");
}

#[test]
fn a_deny_can_tell_the_model_what_to_do_instead() {
    let _guard = setup();
    let dir = temp_dir("scope-feedback");
    let mut core = open(&dir, ApprovalMode::Manual);

    let events = run(&mut core, "npm install left-pad");
    let (call_id, _) = approval(&events).expect("asks");
    let mut events = core.approve_with(
        &call_id,
        false,
        ApproveOptions {
            feedback: Some("Use pnpm instead.".to_string()),
            ..ApproveOptions::default()
        },
    );
    events.extend(pump(&mut core, is_ready));
    let text = assistant_text(&events);
    assert!(text.contains("denied by user"), "{text}");
    assert!(text.contains("Use pnpm instead."), "{text}");
}

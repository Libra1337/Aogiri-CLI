//! A command rule that says `ask` always reaches a person: full-access does
//! not approve it, the session's "always allow" does not cover it, and a
//! switch to a looser mode does not approve it while it waits. A `forbid`
//! rule still refuses, and other commands still run as the mode says.
//!
//! Runs against the temporary HOME and fake model of `support/fake_model.rs`
//! (`RUN: <command>`, `CALL <tool> <arguments>`).

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{setup, temp_dir};
use lynshen_agent_core::{
    sandbox::{CommandRule, RuleAction, SandboxPolicy},
    AgentCore, AgentEvent, ApprovalMode,
};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Polls until an event satisfies `done`, returning every event seen.
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

/// Polls for `how_long`, returning every event.
fn pump_for(core: &mut AgentCore, how_long: Duration) -> Vec<AgentEvent> {
    let until = Instant::now() + how_long;
    let mut seen = Vec::new();
    while Instant::now() < until {
        seen.extend(core.poll_events());
        thread::sleep(Duration::from_millis(10));
    }
    seen
}

fn is_ready(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::Status(status) if status == "ready")
}

fn is_approval(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::ApprovalRequest { .. })
}

/// The call id and summary of the first approval request in `events`.
fn approval(events: &[AgentEvent]) -> Option<(String, String)> {
    events.iter().find_map(|event| match event {
        AgentEvent::ApprovalRequest {
            call_id, summary, ..
        } => Some((call_id.clone(), summary.clone())),
        _ => None,
    })
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

/// An engine in `mode` with the default sandbox and command rules, plus
/// `printf asked` (ask) and `printf forbidden` (forbid).
fn open(cwd: &Path, mode: ApprovalMode) -> AgentCore {
    let mut core = AgentCore::open(cwd.to_path_buf()).unwrap();
    core.set_approval_mode(mode);
    let mut sandbox = SandboxPolicy::default_for_platform();
    sandbox.rules.extend([
        CommandRule {
            prefix: "printf asked".into(),
            action: RuleAction::Ask,
        },
        CommandRule {
            prefix: "printf forbidden".into(),
            action: RuleAction::Forbid,
        },
    ]);
    core.set_sandbox(Some(sandbox));
    core
}

/// Sends `RUN: <command>` and waits until the turn asks or ends.
fn run(core: &mut AgentCore, command: &str) -> Vec<AgentEvent> {
    core.submit_user_message(format!("RUN: {command}"));
    pump(core, |event| is_ready(event) || is_approval(event))
}

#[test]
fn an_ask_rule_asks_in_full_access() {
    let _guard = setup();
    let dir = temp_dir("ask-full-access");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    // The default rule for git push.
    let events = run(&mut core, "git push origin main");
    let (call_id, summary) = approval(&events).expect("git push asks in full-access");
    assert_eq!(summary, "git push origin main");
    core.approve(&call_id, false, false, None);
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("denied by user"));

    // The call waits for the answer, and runs only once allowed.
    let events = run(&mut core, "printf asked > marker.txt");
    let (call_id, _) = approval(&events).expect("the ask rule asks in full-access");
    let waiting = pump_for(&mut core, Duration::from_millis(300));
    assert!(!waiting.iter().any(is_ready), "{waiting:#?}");
    assert!(!dir.join("marker.txt").exists());
    core.approve(&call_id, true, false, None);
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join("marker.txt")).unwrap(), "asked");
}

#[test]
fn always_allowing_bash_for_the_session_does_not_cover_an_ask_rule() {
    let _guard = setup();
    let dir = temp_dir("ask-always");
    let mut core = open(&dir, ApprovalMode::Manual);

    // "Always allow" on an ordinary command: the next one that starts the
    // same way runs unasked.
    let events = run(&mut core, "printf two > one.txt");
    let (call_id, _) = approval(&events).expect("manual asks");
    core.approve(&call_id, true, true, None);
    pump(&mut core, is_ready);
    let events = run(&mut core, "printf two > two.txt");
    assert!(approval(&events).is_none(), "{events:#?}");
    assert_eq!(fs::read_to_string(dir.join("two.txt")).unwrap(), "two");

    // The ask rule still asks, and "always allow" on it covers only this call.
    let events = run(&mut core, "printf asked > marker.txt");
    let (call_id, _) = approval(&events).expect("the ask rule still asks");
    assert!(!dir.join("marker.txt").exists());
    core.approve(&call_id, true, true, None);
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join("marker.txt")).unwrap(), "asked");
    let events = run(&mut core, "printf asked again > again.txt");
    let (call_id, _) = approval(&events).expect("the ask rule asks every time");
    core.approve(&call_id, false, false, None);
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("denied by user"));
    assert!(!dir.join("again.txt").exists());
}

#[test]
fn switching_to_full_access_does_not_approve_a_waiting_ask_rule_call() {
    let _guard = setup();
    let dir = temp_dir("ask-switch");
    let mut core = open(&dir, ApprovalMode::Manual);

    let events = run(&mut core, "printf asked > marker.txt");
    let (call_id, _) = approval(&events).expect("the ask rule asks");
    core.set_approval_mode(ApprovalMode::FullAccess);
    let waiting = pump_for(&mut core, Duration::from_millis(500));
    assert!(!waiting.iter().any(is_ready), "{waiting:#?}");
    assert!(!dir.join("marker.txt").exists());
    core.approve(&call_id, false, false, None);
    let events = pump(&mut core, is_ready);
    assert!(assistant_text(&events).contains("denied by user"));
    assert!(!dir.join("marker.txt").exists());

    // An ordinary call waiting on manual mode still runs on the switch.
    core.set_approval_mode(ApprovalMode::Manual);
    let events = run(&mut core, "printf freed > freed.txt");
    assert!(approval(&events).is_some(), "manual asks");
    core.set_approval_mode(ApprovalMode::FullAccess);
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join("freed.txt")).unwrap(), "freed");
}

#[test]
fn a_forbid_rule_still_refuses_in_full_access() {
    let _guard = setup();
    let dir = temp_dir("ask-forbid");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let events = run(&mut core, "printf forbidden > marker.txt");
    assert!(approval(&events).is_none(), "{events:#?}");
    assert!(assistant_text(&events).contains("forbidden by the agent's command rules"));
    assert!(!dir.join("marker.txt").exists());
}

#[test]
fn ordinary_commands_run_without_asking_in_full_access() {
    let _guard = setup();
    let dir = temp_dir("ask-ordinary");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let events = run(&mut core, "printf ran > ran.txt");
    assert!(approval(&events).is_none(), "{events:#?}");
    assert_eq!(fs::read_to_string(dir.join("ran.txt")).unwrap(), "ran");

    // Leaving the sandbox without a rule: full-access lets it out unasked.
    core.submit_user_message(
        r#"CALL bash {"command":"printf out > out.txt","escalate":true,"justification":"test"}"#
            .to_string(),
    );
    let events = pump(&mut core, |event| is_ready(event) || is_approval(event));
    assert!(approval(&events).is_none(), "{events:#?}");
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "out");
}

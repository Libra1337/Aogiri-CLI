//! Approval modes and the sandbox: full-access approval keeps the sandbox
//! and reports what leaves it (`escalated`), an allow rule does the same,
//! `set_sandbox` switches the sandbox and every change emits `sandbox`.
//!
//! Runs against the temporary HOME and fake model of `support/fake_model.rs`
//! (`RUN: <command>`, `CALL <tool> <arguments>`).

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{setup, temp_dir};
use lynshen_agent_core::{
    approval::ApprovalKind,
    protocol,
    sandbox::{CommandRule, RuleAction, SandboxMode, SandboxPolicy},
    AgentCore, AgentEvent, ApprovalMode, SandboxView,
};
use serde_json::json;
use std::{
    fs,
    path::Path,
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

fn escalated(events: &[AgentEvent]) -> Vec<(&'static str, ApprovalKind, Option<String>)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Escalated { by, info, .. } => {
                Some((*by, info.kind, info.justification.clone()))
            }
            _ => None,
        })
        .collect()
}

fn sandbox_view(events: &[AgentEvent]) -> Option<SandboxView> {
    events.iter().rev().find_map(|event| match event {
        AgentEvent::Sandbox(view) => Some(view.clone()),
        _ => None,
    })
}

fn open(cwd: &Path, mode: ApprovalMode) -> AgentCore {
    let mut core = AgentCore::open(cwd.to_path_buf()).unwrap();
    core.set_approval_mode(mode);
    core.set_sandbox(Some(SandboxPolicy {
        mode: SandboxMode::WorkspaceWrite,
        ..SandboxPolicy::default_for_platform()
    }));
    core
}

fn submit(core: &mut AgentCore, message: String) -> Vec<AgentEvent> {
    core.submit_user_message(message);
    pump(core, |event| is_ready(event) || is_approval(event))
}

fn escalate(core: &mut AgentCore, command: &str) -> Vec<AgentEvent> {
    let arguments = json!({ "command": command, "escalate": true, "justification": "needs it" });
    submit(core, format!("CALL bash {arguments}"))
}

#[test]
fn full_access_keeps_the_sandbox_and_reports_what_leaves_it() {
    let _guard = setup();
    let dir = temp_dir("modes-full-access");
    let mut core = open(&dir, ApprovalMode::FullAccess);

    let events = submit(&mut core, "RUN: printf in > in.txt".to_string());
    assert!(!events.iter().any(is_approval), "{events:#?}");
    assert!(escalated(&events).is_empty(), "{events:#?}");
    assert!(dir.join("in.txt").exists());

    let events = escalate(&mut core, "printf out > out.txt");
    assert!(!events.iter().any(is_approval), "{events:#?}");
    assert_eq!(
        escalated(&events),
        vec![(
            "mode",
            ApprovalKind::Escalation,
            Some("needs it".to_string())
        )]
    );
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "out");

    // The built-in list still asks.
    let events = submit(&mut core, "RUN: sudo -n true".to_string());
    assert!(events.iter().any(is_approval), "{events:#?}");
}

#[test]
fn manual_mode_runs_read_only_commands_without_asking() {
    // Only enforced where an OS sandbox runs (macOS and the Linux CI);
    // elsewhere manual mode gates every command and there is nothing new.
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return;
    }
    let _guard = setup();
    let dir = temp_dir("modes-manual-read-only");
    fs::write(dir.join("note.txt"), "hi\n").unwrap();
    let mut core = open(&dir, ApprovalMode::Manual);

    // A read-only command, and a compound of them, run without a prompt.
    for command in ["cat note.txt", "ls -la | head -5; wc -l note.txt"] {
        let events = submit(&mut core, format!("RUN: {command}"));
        assert!(
            !events.iter().any(is_approval),
            "{command} asked: {events:#?}"
        );
    }

    // A write still asks, and runs only once allowed.
    let events = submit(&mut core, "RUN: printf wrote > out.txt".to_string());
    let call = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ApprovalRequest { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .expect("a write asks in manual mode");
    assert!(!dir.join("out.txt").exists());
    core.approve(&call, true, false, None);
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "wrote");
}

#[test]
fn an_allow_rule_lets_an_escalation_out_and_says_so() {
    let _guard = setup();
    let dir = temp_dir("modes-allow-rule");
    let mut core = open(&dir, ApprovalMode::AutoEdit);
    let mut sandbox = SandboxPolicy {
        mode: SandboxMode::WorkspaceWrite,
        ..SandboxPolicy::default_for_platform()
    };
    sandbox.rules.push(CommandRule {
        prefix: "printf ruled".to_string(),
        action: RuleAction::Allow,
    });
    core.set_sandbox(Some(sandbox));

    let events = escalate(&mut core, "printf ruled > r.txt");
    assert!(!events.iter().any(is_approval), "{events:#?}");
    assert_eq!(escalated(&events)[0].0, "rule");
    assert!(dir.join("r.txt").exists());

    // Only as written: a variable, `env`, another path for the program or a
    // `PATH` exported before can make it something else.
    for command in [
        "PATH=. printf ruled > v.txt",
        "env FOO=1 printf ruled > v.txt",
        "/usr/bin/printf ruled > v.txt",
        "export PATH=.:$PATH; printf ruled > v.txt",
    ] {
        let events = escalate(&mut core, command);
        let call_id = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ApprovalRequest { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{command} asks: {events:#?}"));
        core.approve(&call_id, false, false, None);
        pump(&mut core, is_ready);
    }
    assert!(!dir.join("v.txt").exists());

    // Without the rule auto-edit asks; a switch to full-access lets the
    // waiting call out, and that is reported too.
    let events = escalate(&mut core, "printf waited > w.txt");
    assert!(events.iter().any(is_approval), "{events:#?}");
    let switched = core.set_approval_mode(ApprovalMode::FullAccess);
    assert_eq!(escalated(&switched)[0].0, "mode");
    pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(dir.join("w.txt")).unwrap(), "waited");
}

#[test]
fn set_sandbox_switches_the_sandbox_and_every_change_is_reported() {
    let _guard = setup();
    let dir = temp_dir("modes-set-sandbox");
    let mut core = open(&dir, ApprovalMode::AutoEdit);
    let extra = temp_dir("modes-set-sandbox-extra");
    core.add_writable_dirs(std::slice::from_ref(&extra));

    let start = sandbox_view(&core.state_events()).expect("a sandbox event at startup");
    assert_eq!(start.mode, "workspace-write");
    assert!(start.network);
    assert_eq!(
        start.enforced,
        cfg!(any(target_os = "macos", target_os = "linux"))
    );
    assert!(start
        .directories
        .iter()
        .any(|dir| dir.path == extra.display().to_string()
            && dir.mode == "rw"
            && dir.source == "project"));

    let (_, events) = protocol::apply_op(
        &mut core,
        &json!({ "op": "set_sandbox", "mode": "full-access", "network": false }),
    );
    let view = sandbox_view(&events).expect("set_sandbox reports the sandbox");
    assert_eq!(view.mode, "full-access");
    assert!(!view.network && !view.enforced);
    assert_eq!(view.unavailable, None);

    // The session's next engine keeps it.
    let mut next = open(&temp_dir("modes-set-sandbox-next"), ApprovalMode::AutoEdit);
    next.restore_kept_state(core.kept_state());
    let kept = sandbox_view(&next.state_events()).unwrap();
    assert_eq!((kept.mode.as_str(), kept.network), ("full-access", false));

    let (_, events) =
        protocol::apply_op(&mut core, &json!({ "op": "set_sandbox", "mode": "open" }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error(message) if message.contains("unknown sandbox"))),
        "{events:#?}"
    );
    let (_, events) = core.handle_command("/sandbox workspace-write");
    assert_eq!(sandbox_view(&events).unwrap().mode, "workspace-write");
}

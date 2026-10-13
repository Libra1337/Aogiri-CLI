//! File tools outside the workspace: reads need nothing, a write asks
//! (`outside_write`, once at most) and runs once approved, full-access writes
//! and reports it (`escalated`), and checkpoints do not cover it.
//!
//! The workspace, temp and the test's HOME are all inside the boundary, so
//! the outside directory is one every user may write that is not temp:
//! `/Users/Shared` on macOS, `/dev/shm` on Linux.

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{setup, temp_dir};
use lynshen_agent_core::{
    approval::{ApprovalInfo, ApprovalKind, ApprovalScope},
    AgentCore, AgentEvent, ApprovalMode,
};
use serde_json::json;
use std::{
    fs,
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

fn tool_output(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolOutput { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect()
}

/// A fresh directory outside temp, removed when dropped.
struct Outside(PathBuf);

impl Outside {
    fn new(label: &str) -> Self {
        let root = if cfg!(target_os = "macos") {
            Path::new("/Users/Shared")
        } else {
            Path::new("/dev/shm")
        };
        let dir = root.join(format!("lynshen-outside-{}-{label}", std::process::id()));
        fs::create_dir_all(&dir).expect("a writable directory outside temp");
        Self(dir.canonicalize().unwrap())
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Turns the `write` tool on in the shared config.json for one test, and
/// puts the file back when dropped.
struct WriteTool(PathBuf, String);

impl WriteTool {
    fn on() -> Self {
        let path = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".lynshen/config.json");
        let saved = fs::read_to_string(&path).unwrap();
        let mut config: serde_json::Value = serde_json::from_str(&saved).unwrap();
        config["edit_tools"] = json!(["hashline_edit", "write"]);
        fs::write(&path, config.to_string()).unwrap();
        Self(path, saved)
    }
}

impl Drop for WriteTool {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, &self.1);
    }
}

fn open(cwd: &Path, mode: ApprovalMode) -> AgentCore {
    let mut core = AgentCore::open(cwd.to_path_buf()).unwrap();
    core.set_approval_mode(mode);
    core
}

fn call(core: &mut AgentCore, tool: &str, arguments: serde_json::Value) -> Vec<AgentEvent> {
    core.submit_user_message(format!("CALL {tool} {arguments}"));
    pump(core, |event| is_ready(event) || is_approval(event))
}

#[test]
fn a_write_outside_the_workspace_asks_and_runs_once_approved() {
    let _guard = setup();
    let _write = WriteTool::on();
    let outside = Outside::new("ask");
    let dir = temp_dir("outside-ask");
    let mut core = open(&dir, ApprovalMode::AutoEdit);
    fs::write(outside.0.join("notes.txt"), "outside notes").unwrap();

    // Reading needs nothing.
    let events = call(
        &mut core,
        "read",
        json!({ "path": outside.0.join("notes.txt").display().to_string() }),
    );
    assert!(approval(&events).is_none(), "{events:#?}");
    assert!(tool_output(&events).contains("outside notes"));

    let target = outside.0.join("report.md");
    let write = json!({ "path": target.display().to_string(), "content": "# Report\n" });
    let events = call(&mut core, "write", write.clone());
    let (call_id, info) = approval(&events).expect("a write outside asks in auto-edit");
    assert_eq!(info.kind, ApprovalKind::OutsideWrite);
    assert_eq!(info.paths, vec![target.clone()]);
    assert_eq!(info.scopes, vec![ApprovalScope::Once]);
    assert!(info.dir_suggestion.is_some());
    assert!(!info.must_ask);
    core.approve(&call_id, false, false, None);
    pump(&mut core, is_ready);
    assert!(!target.exists());

    let events = call(&mut core, "write", write);
    let (call_id, _) = approval(&events).expect("asks again");
    core.approve(&call_id, true, false, None);
    let events = pump(&mut core, is_ready);
    assert_eq!(fs::read_to_string(&target).unwrap(), "# Report\n");
    assert!(tool_output(&events).contains("checkpoints"), "{events:#?}");
}

#[test]
fn full_access_writes_outside_the_workspace_and_reports_it() {
    let _guard = setup();
    let _write = WriteTool::on();
    let outside = Outside::new("full");
    let dir = temp_dir("outside-full");
    let mut core = open(&dir, ApprovalMode::FullAccess);
    let target = outside.0.join("chart.svg");
    let events = call(
        &mut core,
        "write",
        json!({ "path": target.display().to_string(), "content": "<svg/>" }),
    );
    let events = if events.iter().any(is_ready) {
        events
    } else {
        pump(&mut core, is_ready)
    };
    assert!(approval(&events).is_none(), "{events:#?}");
    let reported = events.iter().find_map(|event| match event {
        AgentEvent::Escalated { by, info, .. } => Some((*by, info.kind, info.paths.clone())),
        _ => None,
    });
    assert_eq!(
        reported,
        Some(("mode", ApprovalKind::OutsideWrite, vec![target.clone()]))
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "<svg/>");
}

fn sandbox_dirs(events: &[AgentEvent]) -> Option<Vec<(String, &'static str, &'static str)>> {
    events.iter().rev().find_map(|event| match event {
        AgentEvent::Sandbox(view) => Some(
            view.directories
                .iter()
                .map(|dir| (dir.path.clone(), dir.mode, dir.source))
                .collect(),
        ),
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

fn config() -> serde_json::Value {
    let path = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".lynshen/config.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn an_approval_can_make_the_directory_writable() {
    let _guard = setup();
    let _write = WriteTool::on();
    let outside = Outside::new("add-dir");
    let dir = temp_dir("outside-add-dir");
    let mut core = open(&dir, ApprovalMode::AutoEdit);
    let path = outside.0.display().to_string();

    let events = call(
        &mut core,
        "write",
        json!({ "path": outside.0.join("a.txt").display().to_string(), "content": "a" }),
    );
    let (call_id, _) = approval(&events).expect("asks");
    let mut events = core.approve_with(
        &call_id,
        true,
        lynshen_agent_core::ApproveOptions {
            add_dir: Some((outside.0.clone(), "session".to_string())),
            ..Default::default()
        },
    );
    assert_eq!(
        sandbox_dirs(&events),
        Some(vec![(path.clone(), "rw", "session")])
    );
    events.extend(pump(&mut core, is_ready));
    assert!(outside.0.join("a.txt").exists(), "{events:#?}");

    // The next write there needs nothing.
    let events = call(
        &mut core,
        "write",
        json!({ "path": outside.0.join("b.txt").display().to_string(), "content": "b" }),
    );
    assert!(approval(&events).is_none(), "{events:#?}");
    assert!(outside.0.join("b.txt").exists());

    // Removed again, it asks again.
    let events = core.remove_directory(&path, "session");
    assert_eq!(sandbox_dirs(&events), Some(Vec::new()));
    let events = call(
        &mut core,
        "write",
        json!({ "path": outside.0.join("c.txt").display().to_string(), "content": "c" }),
    );
    let (call_id, _) = approval(&events).expect("asks again");
    core.approve(&call_id, false, false, None);
    pump(&mut core, is_ready);
}

#[test]
fn directories_are_saved_for_the_project_or_every_project() {
    let _guard = setup();
    let _write = WriteTool::on();
    let outside = Outside::new("persist");
    let dir = temp_dir("outside-persist");
    let mut core = open(&dir, ApprovalMode::AutoEdit);
    let path = outside.0.display().to_string();
    let key = dir.display().to_string();

    let events = core.add_directory(&path, "rw", "project");
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        config()["projects"][&key]["sandbox_directories"],
        json!([{ "path": path, "mode": "rw" }])
    );
    // A new engine for the project starts with it.
    let next = open(&dir, ApprovalMode::AutoEdit);
    assert_eq!(
        sandbox_dirs(&next.state_events()),
        Some(vec![(path.clone(), "rw", "project")])
    );
    let events = core.remove_directory(&path, "project");
    assert_eq!(sandbox_dirs(&events), Some(Vec::new()));
    assert_eq!(config()["projects"][&key]["sandbox_directories"], json!([]));

    let events = core.add_directory(&path, "ro", "global");
    assert_eq!(
        sandbox_dirs(&events),
        Some(vec![(path.clone(), "ro", "global")])
    );
    assert!(config()["sandbox_directories"]
        .as_array()
        .unwrap()
        .contains(&json!({ "path": path, "mode": "ro" })));
    core.remove_directory(&path, "global");
    assert!(!config()["sandbox_directories"]
        .as_array()
        .unwrap()
        .contains(&json!({ "path": path, "mode": "ro" })));

    // Errors change nothing.
    for (events, wanted) in [
        (
            core.add_directory("relative/dir", "rw", "session"),
            "not an existing absolute directory",
        ),
        (core.add_directory(&path, "rx", "session"), "rw or ro"),
        (
            core.add_directory(&path, "rw", "forever"),
            "persist must be",
        ),
        (
            core.remove_directory(&path, "session"),
            "is not a session directory",
        ),
    ] {
        let found = errors(&events);
        assert!(
            found.iter().any(|error| error.contains(wanted)),
            "{wanted}: {found:?}"
        );
    }
}

#[test]
fn credentials_and_settings_ask_even_from_a_workspace_at_home() {
    let _guard = setup();
    let _write = WriteTool::on();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(home.join(".ssh/id_test"), "PRIVATE-KEY-BODY").unwrap();
    let mut core = open(&home, ApprovalMode::FullAccess);

    // A workspace at home does not make credentials readable.
    for path in ["~/.ssh/id_test", ".ssh/id_test", "./.SSH/../.ssh/id_test"] {
        let events = call(&mut core, "read", json!({ "path": path }));
        let output = tool_output(&events);
        assert!(!output.contains("PRIVATE-KEY-BODY"), "{path}: {output}");
        assert!(output.contains("holds credentials"), "{path}: {output}");
    }

    // Writing credentials or LynShen's settings asks, in full-access too.
    for path in [".ssh/authorized_keys", ".lynshen/config.json"] {
        let before = fs::read_to_string(home.join(path)).ok();
        let events = call(&mut core, "write", json!({ "path": path, "content": "{}" }));
        let (call_id, info) = approval(&events).expect("a person decides");
        assert!(info.must_ask, "{path}");
        assert_eq!(info.scopes, vec![ApprovalScope::Once], "{path}");
        core.approve(&call_id, false, false, None);
        pump(&mut core, is_ready);
        assert_eq!(fs::read_to_string(home.join(path)).ok(), before, "{path}");
    }

    // Nor can home, the root or credentials become writable.
    for dir in [home.clone(), PathBuf::from("/"), home.join(".ssh")] {
        let events = core.add_directory(&dir.display().to_string(), "rw", "session");
        assert!(
            errors(&events)
                .iter()
                .any(|error| error.contains("cannot be made writable")),
            "{}: {events:#?}",
            dir.display()
        );
    }
    let _ = fs::remove_dir_all(home.join(".ssh"));
}

//! Stopping a turn while its model request waits: before a retry, for an
//! answer that never comes, for the rest of a stream that stopped. The turn
//! ends at once, the request is not sent again, an active goal does not
//! start the next turn, and nothing of the stopped turn reaches the next.
//!
//! Runs against the temporary HOME and fake model of `support/fake_model.rs`
//! (`STALL`, `STALL_MIDWAY`, `BUSY`).

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{requests, setup, temp_dir};
use lynshen_agent_core::{AgentCore, AgentEvent};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
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

fn is_interrupted(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::Status(status) if status == "interrupted")
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

/// Events a stopped turn must not send any more.
fn from_a_turn(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Retrying { .. }
            | AgentEvent::AssistantDelta(_)
            | AgentEvent::AssistantStart
            | AgentEvent::Error(_)
    )
}

/// How many requests the fake model got whose last user message is `text`.
fn requests_saying(text: &str) -> usize {
    requests()
        .iter()
        .filter(|body| {
            body["messages"]
                .as_array()
                .and_then(|messages| messages.iter().rev().find(|m| m["role"] == "user"))
                .is_some_and(|message| match &message["content"] {
                    Value::String(content) => content == text,
                    Value::Array(parts) => parts.iter().any(|part| part["text"] == text),
                    _ => false,
                })
        })
        .count()
}

/// Waits until the fake model got `count` requests saying `text`.
fn wait_for_requests(text: &str, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while requests_saying(text) < count {
        assert!(Instant::now() < deadline, "the request never came");
        thread::sleep(Duration::from_millis(10));
    }
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
fn stop_during_the_wait_before_a_retry_sends_nothing_more() {
    let _guard = setup();
    let _config = ConfigChange::set(json!({ "retry_attempts": 3 }));
    let mut core = AgentCore::open(temp_dir("busy")).unwrap();
    let before = requests_saying("BUSY");

    core.submit_user_message("BUSY".to_string());
    pump(&mut core, |event| {
        matches!(event, AgentEvent::Retrying { .. })
    });
    assert_eq!(requests_saying("BUSY"), before + 1);
    let stopped = core.interrupt();
    assert!(stopped.iter().any(is_interrupted), "{stopped:#?}");

    // The next attempt was due 2 s after the 503 (its Retry-After): a stop
    // that only dropped the turn's events still sent it then.
    let later = pump_for(&mut core, Duration::from_secs(3));
    assert_eq!(requests_saying("BUSY"), before + 1, "sent again after Stop");
    assert!(!later.iter().any(from_a_turn), "{later:#?}");

    core.submit_user_message("after busy".to_string());
    let next = pump(&mut core, is_ready);
    assert_eq!(assistant_text(&next), "user said: after busy");
    assert!(!next
        .iter()
        .any(|event| matches!(event, AgentEvent::Retrying { .. } | AgentEvent::Error(_))));
}

#[test]
fn stop_while_the_answer_never_comes_leaves_the_next_turn_clean() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("stall")).unwrap();
    let before = requests_saying("STALL");

    core.submit_user_message("STALL".to_string());
    wait_for_requests("STALL", before + 1);
    let stopped = core.interrupt();
    assert!(stopped.iter().any(is_interrupted), "{stopped:#?}");

    core.submit_user_message("after stall".to_string());
    let next = pump(&mut core, is_ready);
    assert_eq!(assistant_text(&next), "user said: after stall");
    assert!(!next
        .iter()
        .any(|event| matches!(event, AgentEvent::Retrying { .. } | AgentEvent::Error(_))));
    assert_eq!(requests_saying("STALL"), before + 1);
}

#[test]
fn stop_while_a_stream_stalls_midway_leaves_the_next_turn_clean() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("midway")).unwrap();

    core.submit_user_message("STALL_MIDWAY".to_string());
    let started = pump(
        &mut core,
        |event| matches!(event, AgentEvent::AssistantDelta(delta) if delta == "partial"),
    );
    assert_eq!(assistant_text(&started), "partial");
    let stopped = core.interrupt();
    assert!(stopped.iter().any(is_interrupted), "{stopped:#?}");

    core.submit_user_message("after midway".to_string());
    let next = pump(&mut core, is_ready);
    assert_eq!(assistant_text(&next), "user said: after midway");
    assert!(!next
        .iter()
        .any(|event| matches!(event, AgentEvent::Error(_))));
}

#[test]
fn stop_pauses_an_active_goal_instead_of_starting_its_next_turn() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("goal")).unwrap();
    let before = requests_saying("STALL");

    core.submit_user_message("STALL".to_string());
    wait_for_requests("STALL", before + 1);
    let (_, set) = core.handle_command("/goal finish the report");
    assert!(set.iter().any(|event| matches!(
        event,
        AgentEvent::Goal(Some(goal)) if goal.status == "active"
    )));

    let stopped = core.interrupt();
    assert!(stopped.iter().any(is_interrupted), "{stopped:#?}");
    assert!(
        stopped.iter().any(|event| matches!(
            event,
            AgentEvent::Goal(Some(goal)) if goal.status == "paused"
        )),
        "{stopped:#?}"
    );
    // Goal continuation looks for an idle engine every poll.
    let later = pump_for(&mut core, Duration::from_millis(500));
    assert!(!later.iter().any(from_a_turn), "a turn started: {later:#?}");

    core.handle_command("/goal clear");
}

/// The turn stats the engine records: one per turn, after its reply, for a
/// finished turn and for a stopped one, kept in the transcript a reopened
/// conversation shows (ticket #39).
#[test]
fn each_turn_records_its_model_and_duration_finished_or_stopped() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("turn-stats")).unwrap();

    core.submit_user_message("first".to_string());
    let finished = pump(&mut core, is_ready);
    let live: Vec<_> = finished
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TurnStats(stats) => Some(stats.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(live.len(), 1, "{finished:#?}");
    assert!(!live[0].model.is_empty());
    assert!(live[0]
        .ttft_ms
        .is_some_and(|ttft| ttft <= live[0].elapsed_ms));
    assert!(live[0].output_tokens > 0);

    let before = requests_saying("STALL");
    core.submit_user_message("STALL".to_string());
    wait_for_requests("STALL", before + 1);
    let stopped = core.interrupt();
    assert!(
        stopped
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnStats(stats) if stats.ttft_ms.is_none())),
        "{stopped:#?}"
    );
    // Stopping again (nothing running) records nothing more.
    assert!(core.interrupt().is_empty());

    let AgentEvent::Transcript(items) = core.transcript_event() else {
        panic!("not a transcript");
    };
    let roles: Vec<&str> = items
        .iter()
        .map(|item| match item {
            lynshen_agent_core::TranscriptItem::User(_) => "user",
            lynshen_agent_core::TranscriptItem::Assistant(_) => "assistant",
            lynshen_agent_core::TranscriptItem::TurnStats(_) => "stats",
            _ => "other",
        })
        .collect();
    assert_eq!(roles, ["user", "assistant", "stats", "user", "stats"]);
}

//! A reply with thinking and no answer ends the turn with a visible error:
//! on the output limit and on a refusal at once (no hidden reminders), and
//! after the empty-response reminders for an empty normal stop.
//!
//! Runs against the temporary HOME and fake model of `support/fake_model.rs`
//! (`THINK_LENGTH`, `THINK_FILTER`, `THINK_EMPTY`).

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{requests, setup, temp_dir};
use lynshen_agent_core::{AgentCore, AgentEvent};
use serde_json::Value;
use std::{
    thread,
    time::{Duration, Instant},
};

/// Polls until the turn is over, returning every event seen.
fn run_turn(core: &mut AgentCore, message: &str) -> Vec<AgentEvent> {
    core.submit_user_message(message.to_string());
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

fn errors(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Error(error) => Some(error.clone()),
            _ => None,
        })
        .collect()
}

/// Output tokens the turn reported.
fn output_tokens(events: &[AgentEvent]) -> u64 {
    events
        .iter()
        .map(|event| match event {
            AgentEvent::Usage { output_tokens, .. } => *output_tokens,
            _ => 0,
        })
        .sum()
}

/// How many turn requests the fake model got for `text`: its last user
/// message is `text` or a runtime reminder right after it (a title request
/// quotes the text inside its own message).
fn requests_for(text: &str) -> usize {
    requests()
        .iter()
        .filter(|body| {
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            messages
                .iter()
                .filter(|m| m["role"] == "user")
                .map(|m| match &m["content"] {
                    Value::String(content) => content.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                })
                .rfind(|content| !content.starts_with("<runtime_reminder>"))
                .is_some_and(|content| content == text)
        })
        .count()
}

#[test]
fn thinking_that_used_up_the_output_limit_ends_the_turn_with_an_error() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("think-length")).unwrap();
    let before = requests_for("THINK_LENGTH");

    let events = run_turn(&mut core, "THINK_LENGTH");
    let errors = errors(&events);
    assert_eq!(errors.len(), 1, "{events:#?}");
    assert!(
        errors[0].starts_with("模型只输出了思考，没有给出回答：输出上限用完了。"),
        "{errors:?}"
    );
    // One request: no hidden reminder spends the thinking again.
    assert_eq!(requests_for("THINK_LENGTH"), before + 1);
    // The tokens were spent and are counted.
    assert_eq!(output_tokens(&events), 8192);
}

#[test]
fn a_refusal_ends_the_turn_with_an_error() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("think-filter")).unwrap();
    let before = requests_for("THINK_FILTER");

    let events = run_turn(&mut core, "THINK_FILTER");
    let errors = errors(&events);
    assert_eq!(errors.len(), 1, "{events:#?}");
    assert!(
        errors[0].starts_with("模型拒绝回答了这个请求。"),
        "{errors:?}"
    );
    assert_eq!(requests_for("THINK_FILTER"), before + 1);
}

#[test]
fn an_empty_reply_after_the_reminders_ends_the_turn_with_an_error() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("think-empty")).unwrap();
    let before = requests_for("THINK_EMPTY");

    let events = run_turn(&mut core, "THINK_EMPTY");
    assert_eq!(errors(&events), vec!["模型没有给出回答。".to_string()]);
    // The request and its two reminders.
    assert_eq!(requests_for("THINK_EMPTY"), before + 3);

    // The next turn is unaffected.
    let next = run_turn(&mut core, "after empty");
    assert!(errors(&next).is_empty(), "{next:#?}");
}

/// Ticket #67 follow-up: an active goal used to start its next turn as soon
/// as a turn failed, so a refused request went out again and again (94
/// requests in 25 s against a refusing server). A failed turn pauses the goal
/// now: one request, then the engine waits for a person.
#[test]
fn a_refusal_pauses_an_active_goal_instead_of_sending_again() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("think-filter-goal")).unwrap();
    let (_, set) = core.handle_command("/goal finish the change");
    assert!(set.iter().any(|event| matches!(
        event,
        AgentEvent::Goal(Some(goal)) if goal.status == "active"
    )));
    let before = requests().len();

    let events = run_turn(&mut core, "THINK_FILTER");
    assert!(
        errors(&events)
            .iter()
            .any(|error| error.starts_with("模型拒绝回答了这个请求。")),
        "{events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Goal(Some(goal)) if goal.status == "paused"
        )),
        "{events:#?}"
    );
    // Goal continuation looks for an idle engine on every poll.
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline {
        core.poll_events();
        thread::sleep(Duration::from_millis(10));
    }
    // Requests that carry the refused message: the turn and any continuation
    // (a title request quotes it inside a message of its own).
    let turn_requests = requests()[before..]
        .iter()
        .filter(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|m| {
                    m["role"] == "user"
                        && match &m["content"] {
                            Value::String(text) => text == "THINK_FILTER",
                            Value::Array(parts) => parts
                                .iter()
                                .any(|part| part["text"].as_str() == Some("THINK_FILTER")),
                            _ => false,
                        }
                })
            })
        })
        .count();
    assert_eq!(turn_requests, 1, "the refused request was sent again");
    assert!(core.is_idle());

    core.handle_command("/goal clear");
}

/// `continue` (the desktop's in-place retry) after a reply without an answer
/// would send the same conversation to the same model again, for the same
/// reply. It is refused with a hint until something changes: a new message
/// goes through.
#[test]
fn continue_after_a_refusal_is_refused_until_the_conversation_changes() {
    let _guard = setup();
    let mut core = AgentCore::open(temp_dir("think-filter-continue")).unwrap();
    run_turn(&mut core, "THINK_FILTER");
    let before = requests_for("THINK_FILTER");

    let refused = core.continue_turn();
    let message = errors(&refused);
    assert_eq!(message.len(), 1, "{refused:#?}");
    assert!(
        message[0].starts_with("模型拒绝回答了这个请求。"),
        "{message:?}"
    );
    assert!(message[0].contains("换一个模型"), "{message:?}");
    assert!(core.is_idle());
    thread::sleep(Duration::from_millis(200));
    core.poll_events();
    assert_eq!(
        requests_for("THINK_FILTER"),
        before,
        "the refused request was sent again"
    );

    // A new message changes the conversation: it is sent.
    let next = run_turn(&mut core, "after the refusal");
    assert!(errors(&next).is_empty(), "{next:#?}");
}

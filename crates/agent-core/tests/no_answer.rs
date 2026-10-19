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

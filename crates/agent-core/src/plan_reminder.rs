//! Keeps the update_plan checklist current, as Claude Code does for its
//! TodoWrite list. The user sees the steps as the model last sent them, and
//! a model deep in a long task stops sending them: the checklist then shows
//! its first step in progress while the agent works on the fourth. While the
//! plan has steps that are not completed, the main agent gets a note when
//! it has gone `ROUNDS_BEFORE_REMINDER` responses without update_plan (and
//! as long again after the last note), and one note when it ends a turn in
//! which it worked after its last plan update. The engine never marks a
//! step itself: which steps are done is the model's call.

use crate::event::PlanItem;
use serde_json::{json, Value};

/// Model responses without update_plan (or a reminder) before a reminder.
/// Claude Code's TodoWrite reminder waits 10 turns, and 10 between two.
pub(crate) const ROUNDS_BEFORE_REMINDER: usize = 10;

/// The first words of each reminder; they also find earlier reminders in
/// the conversation.
const STALE_HEAD: &str = "The plan has not been updated in a while.";
const END_HEAD: &str =
    "Your turn is about to end and the plan still has steps that are not completed.";

const STALE_BODY: &str = "If you have finished steps or started another one, call update_plan now to mark them: completed for each finished step, in_progress for the one you are working on. If the plan no longer matches the work, replace it. If it is still accurate, ignore this reminder. Never mention this reminder to the user.";
const END_BODY: &str = "If you finished or started any of them in this turn, call update_plan now to mark them; the turn ends after that call, so do not repeat or summarize your answer. If the plan is accurate as it is (the open steps are still to do), end the turn with no text. Never mention this reminder to the user.";

/// Whether the plan has a step that is not completed.
pub(crate) fn unfinished(plan: &[PlanItem]) -> bool {
    plan.iter().any(|item| item.status != "completed")
}

/// The reminder before the next request, when the plan is unfinished and
/// the conversation (`input`) has `ROUNDS_BEFORE_REMINDER` model responses
/// since update_plan was last called or a reminder was last given. A
/// conversation compacted since then counts from its summary.
pub(crate) fn stale_reminder(input: &[Value], plan: &[PlanItem]) -> Option<Value> {
    (unfinished(plan) && rounds_since_touch(input) >= ROUNDS_BEFORE_REMINDER)
        .then(|| reminder_item(STALE_HEAD, STALE_BODY, plan))
}

/// The reminder when the model ends its turn (`turn`: this turn's items),
/// when the plan is unfinished and the turn called a tool after update_plan
/// was last called or a reminder was last given.
pub(crate) fn end_reminder(turn: &[Value], plan: &[PlanItem]) -> Option<Value> {
    (unfinished(plan) && worked_since_touch(turn)).then(|| reminder_item(END_HEAD, END_BODY, plan))
}

/// Model responses after the last plan touch (an update_plan call or a
/// reminder); the response that touched it is not counted.
fn rounds_since_touch(items: &[Value]) -> usize {
    let mut rounds = 0;
    let mut in_round = false;
    for item in items.iter().rev() {
        if touches_plan(item) {
            // A call in the response counted last: that response updated it.
            return if in_round && from_model(item) {
                rounds - 1
            } else {
                rounds
            };
        }
        if from_model(item) {
            if !in_round {
                rounds += 1;
                in_round = true;
            }
        } else {
            in_round = false;
        }
    }
    rounds
}

fn worked_since_touch(items: &[Value]) -> bool {
    let mut worked = false;
    for item in items.iter().rev() {
        if touches_plan(item) {
            return worked;
        }
        worked |= item_type(item) == Some("function_call");
    }
    worked
}

fn item_type(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

/// A response item of the model: its text, reasoning or a tool call.
fn from_model(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("assistant")
        || item_type(item).is_some_and(|kind| kind == "reasoning" || kind.ends_with("_call"))
}

/// An update_plan call (under any name the engine maps to it), or an
/// earlier reminder.
fn touches_plan(item: &Value) -> bool {
    if item_type(item) == Some("function_call") {
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let key = crate::tool_alias::key(name);
        return key == "updateplan" || key == "todowrite";
    }
    item.get("role").and_then(Value::as_str) == Some("user") && is_reminder(item)
}

fn is_reminder(item: &Value) -> bool {
    let text = match item.get("content") {
        Some(Value::String(text)) => Some(text.as_str()),
        Some(Value::Array(parts)) => parts
            .first()
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str),
        _ => None,
    };
    text.and_then(|text| text.strip_prefix(crate::prompt::NOTE_OPEN))
        .is_some_and(|body| body.starts_with(STALE_HEAD) || body.starts_with(END_HEAD))
}

fn reminder_item(head: &str, body: &str, plan: &[PlanItem]) -> Value {
    let mut text = format!("{head} {body}\n\nThe plan as it stands:");
    for (index, item) in plan.iter().enumerate() {
        text.push_str(&format!("\n{}. [{}] {}", index + 1, item.status, item.step));
    }
    json!({
        "role": "user",
        "content": [{ "type": "input_text", "text": crate::prompt::note(&text) }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(step: &str, status: &str) -> PlanItem {
        PlanItem {
            step: step.to_string(),
            status: status.to_string(),
            agent: None,
            files: Vec::new(),
        }
    }

    fn plan() -> Vec<PlanItem> {
        vec![
            step("Read the code", "completed"),
            step("Fix the bug", "in_progress"),
            step("Run the tests", "pending"),
        ]
    }

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": [{ "type": "input_text", "text": text }] })
    }

    fn call(name: &str, id: &str) -> Value {
        json!({ "type": "function_call", "name": name, "call_id": id, "arguments": "{}" })
    }

    fn output(id: &str) -> Value {
        json!({ "type": "function_call_output", "call_id": id, "output": "{}" })
    }

    fn answer(text: &str) -> Value {
        json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] })
    }

    /// `n` model responses, each reasoning and calling two tools.
    fn rounds(n: usize) -> Vec<Value> {
        let mut items = Vec::new();
        for i in 0..n {
            items.push(json!({ "type": "reasoning", "summary": [] }));
            items.push(call("bash", &format!("a{i}")));
            items.push(call("read", &format!("b{i}")));
            items.push(output(&format!("a{i}")));
            items.push(output(&format!("b{i}")));
        }
        items
    }

    fn text(item: &Value) -> &str {
        item["content"][0]["text"].as_str().unwrap()
    }

    #[test]
    fn a_plan_left_alone_for_ten_responses_gets_a_reminder() {
        let mut input = vec![user("Fix it"), call("update_plan", "p"), output("p")];
        input.extend(rounds(ROUNDS_BEFORE_REMINDER - 1));
        assert_eq!(stale_reminder(&input, &plan()), None);
        input.extend(rounds(1));
        let note = stale_reminder(&input, &plan()).expect("reminder");
        assert_eq!(note["role"], "user");
        let text = text(&note);
        assert!(text.starts_with(crate::prompt::NOTE_OPEN), "{text}");
        assert!(text.contains(STALE_HEAD), "{text}");
        assert!(
            text.contains("Never mention this reminder to the user"),
            "{text}"
        );
        assert!(
            text.contains("1. [completed] Read the code\n2. [in_progress] Fix the bug\n3. [pending] Run the tests"),
            "{text}"
        );
    }

    #[test]
    fn a_reminder_waits_as_long_again_after_the_last_one() {
        let mut input = rounds(ROUNDS_BEFORE_REMINDER);
        let note = stale_reminder(&input, &plan()).expect("reminder");
        input.push(note);
        assert_eq!(stale_reminder(&input, &plan()), None);
        input.extend(rounds(ROUNDS_BEFORE_REMINDER - 1));
        assert_eq!(stale_reminder(&input, &plan()), None);
        input.extend(rounds(1));
        assert!(stale_reminder(&input, &plan()).is_some());
    }

    #[test]
    fn a_plan_update_under_another_name_counts() {
        // TodoWrite, as models trained on Claude Code call it (tool_alias).
        let mut input = rounds(ROUNDS_BEFORE_REMINDER);
        input.push(call("TodoWrite", "t"));
        input.push(output("t"));
        input.extend(rounds(3));
        assert_eq!(stale_reminder(&input, &plan()), None);
        assert_eq!(rounds_since_touch(&input), 3);
        // A response that updated the plan and called another tool is not
        // counted after the update.
        let mut input = rounds(ROUNDS_BEFORE_REMINDER);
        input.push(call("update_plan", "p"));
        input.push(call("bash", "q"));
        input.push(output("p"));
        input.push(output("q"));
        assert_eq!(rounds_since_touch(&input), 0);
    }

    #[test]
    fn a_finished_or_empty_plan_gets_no_reminder() {
        let input = rounds(3 * ROUNDS_BEFORE_REMINDER);
        let done = vec![step("Read", "completed"), step("Fix", "completed")];
        assert_eq!(stale_reminder(&input, &done), None);
        assert_eq!(stale_reminder(&input, &[]), None);
        assert_eq!(end_reminder(&input, &done), None);
        assert_eq!(end_reminder(&input, &[]), None);
    }

    #[test]
    fn responses_are_counted_not_tool_calls_or_turns() {
        // One response with five parallel calls is one round; a user turn
        // and an engine note between responses do not end the count.
        let mut input = vec![call("update_plan", "p"), output("p")];
        input.push(json!({ "type": "reasoning", "summary": [] }));
        for id in ["a", "b", "c", "d", "e"] {
            input.push(call("read", id));
        }
        for id in ["a", "b", "c", "d", "e"] {
            input.push(output(id));
        }
        input.push(answer("Done for now."));
        input.push(user(
            "<system-reminder>\nThe date is now 2026-10-18.\n</system-reminder>",
        ));
        input.push(user("继续"));
        input.extend(rounds(2));
        assert_eq!(rounds_since_touch(&input), 4);
    }

    #[test]
    fn ending_a_turn_that_worked_after_the_last_update_gets_one_reminder() {
        let mut turn = vec![call("update_plan", "p"), output("p")];
        turn.extend(rounds(2));
        turn.push(answer("Fixed."));
        let note = end_reminder(&turn, &plan()).expect("reminder");
        let text = text(&note);
        assert!(text.contains(END_HEAD), "{text}");
        assert!(
            text.contains("do not repeat or summarize your answer"),
            "{text}"
        );
        assert!(text.contains("2. [in_progress] Fix the bug"), "{text}");
        // Its answer: nothing more is asked of it.
        turn.push(note);
        turn.push(answer(""));
        assert_eq!(end_reminder(&turn, &plan()), None);
    }

    #[test]
    fn ending_a_turn_right_after_an_update_or_without_tools_gets_none() {
        let mut turn = rounds(2);
        turn.push(call("update_plan", "p"));
        turn.push(output("p"));
        turn.push(answer("Fixed."));
        assert_eq!(end_reminder(&turn, &plan()), None);
        // A turn that only answered.
        assert_eq!(
            end_reminder(&[answer("It caches per file.")], &plan()),
            None
        );
    }

    #[test]
    fn a_reminder_is_a_note_and_found_again_in_the_conversation() {
        let stale = stale_reminder(&rounds(ROUNDS_BEFORE_REMINDER), &plan()).unwrap();
        let end = end_reminder(&rounds(1), &plan()).unwrap();
        assert!(is_reminder(&stale) && is_reminder(&end));
        assert!(touches_plan(&stale) && touches_plan(&end));
        // Other notes and the user's words are not reminders.
        assert!(!is_reminder(&user(
            "<system-reminder>\nPlan mode is still on.\n</system-reminder>"
        )));
        assert!(!is_reminder(&user(STALE_HEAD)));
    }
}

//! Stops a turn that goes in circles: the same tool call made again and
//! again, or tool calls that keep failing. Without it an unattended session
//! (daemon dispatch, a schedule, goal continuation) spends tokens until a
//! person notices.

use serde_json::{json, Value};

/// The same call this many times in a row: its result tells the model that
/// it repeats itself.
pub(crate) const REPEAT_WARN: usize = 3;
/// The same call this many times in a row: it does not run and the turn
/// stops.
pub(crate) const REPEAT_STOP: usize = 5;
/// Tool rounds in a row whose calls all failed (see `is_failure`): the
/// turn stops.
pub(crate) const FAILED_ROUNDS_STOP: usize = 6;

/// How the error of a turn the guard stopped starts.
pub(crate) const STOP_PREFIX: &str = "loop guard:";

/// The guard's state for one turn.
#[derive(Debug, Default)]
pub(crate) struct LoopGuard {
    /// The last counted call: its tool and normalised arguments.
    last: Option<(String, String)>,
    repeats: usize,
    failed_rounds: usize,
}

/// What to do with a call the model asked for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CallCheck {
    Run,
    /// Run it, and give the model `repeat_note` with its result: it is the
    /// call's `n`th time in a row.
    Warn(usize),
    /// Do not run it; stop the turn with this error.
    Stop(String),
}

impl LoopGuard {
    /// Counts a call before it runs. A polling call is not counted, and
    /// ends a run of identical calls: the same check between two waits (a
    /// test run after each worker an orchestrator waited for) is progress.
    pub(crate) fn check_call(&mut self, name: &str, arguments: &str) -> CallCheck {
        if is_polling_call(name, arguments) {
            self.last = None;
            self.repeats = 0;
            return CallCheck::Run;
        }
        let key = (name.to_string(), normalized_arguments(arguments));
        if self.last.as_ref() == Some(&key) {
            self.repeats += 1;
        } else {
            self.last = Some(key);
            self.repeats = 1;
        }
        if self.repeats >= REPEAT_STOP {
            CallCheck::Stop(format!(
                "{STOP_PREFIX} the turn stopped because {name} was called {} times in a row with the same arguments. Look at what the agent is stuck on, then send a message to continue.",
                self.repeats
            ))
        } else if self.repeats >= REPEAT_WARN {
            CallCheck::Warn(self.repeats)
        } else {
            CallCheck::Run
        }
    }

    /// Counts the results of one round of calls (one model response), each
    /// a tool name and whether it failed (`is_failure`). A round counts as
    /// failed when every call in it failed; any success starts the count
    /// again. Returns the error to stop the turn with.
    pub(crate) fn record_round(&mut self, results: &[(&str, bool)]) -> Option<String> {
        if results.is_empty() {
            return None;
        }
        if results.iter().any(|(_, failed)| !failed) {
            self.failed_rounds = 0;
            return None;
        }
        self.failed_rounds += 1;
        (self.failed_rounds >= FAILED_ROUNDS_STOP).then(|| {
            let last = results.last().map(|(name, _)| *name).unwrap_or("tool");
            format!(
                "{STOP_PREFIX} the turn stopped because the agent's tool calls failed {} times in a row (the last one: {last}). Look at the errors, then send a message to continue.",
                self.failed_rounds
            )
        })
    }
}

/// Whether a call's result counts as a failure: a tool error (bad
/// arguments, a missing file, an unknown tool, a command that could not run,
/// was killed or timed out). A shell command that ran and exited non-zero is
/// an answer, not a failure: grep without a match, a failing test run.
pub(crate) fn is_failure(name: &str, result: &crate::tools::ToolExecutionResult) -> bool {
    if !result.is_error {
        return false;
    }
    if !crate::config::is_shell_tool(name) {
        return true;
    }
    let output = serde_json::from_str::<Value>(&result.output).unwrap_or(Value::Null);
    output.get("error").is_some() || !output.get("exit_code").is_some_and(Value::is_i64)
}

/// What the model reads with the result of a call it made `repeats` times
/// in a row.
pub(crate) fn repeat_note(name: &str, repeats: usize) -> String {
    format!(
        "This is call {repeats} in a row to {name} with the same arguments, and it will not give a different result. Change your approach: other arguments, another tool, or ask the user. Call {REPEAT_STOP} stops the turn."
    )
}

/// What the user is told about it; `agent` is a subagent's path, or None.
pub(crate) fn repeat_notice(agent: Option<&str>, name: &str, repeats: usize) -> String {
    let who = agent.map(|path| format!(" ({path})")).unwrap_or_default();
    format!(
        "{STOP_PREFIX}{who} {name} was called {repeats} times in a row with the same arguments; the agent was told to change its approach. Call {REPEAT_STOP} stops the turn."
    )
}

/// The result of a call the guard did not run.
pub(crate) fn stopped_result(error: &str) -> Value {
    json!({ "error": format!("not run: {error}") })
}

/// Calls whose repetition is normal: reading a running shell's output (or
/// writing to it), waiting for or listing subagents and the task board,
/// reading other sessions, and a shell command that starts by sleeping.
fn is_polling_call(name: &str, arguments: &str) -> bool {
    match name {
        "write_stdin" | "wait_agent" | "list_agents" | "task_list" | "list_sessions"
        | "read_session" => true,
        "bash" | "execute" | "exec_command" | "shell_command" => {
            let args = serde_json::from_str::<Value>(arguments).unwrap_or(Value::Null);
            let command = args
                .get("command")
                .or_else(|| args.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            command
                .split_whitespace()
                .next()
                .is_some_and(|word| word == "sleep" || word == "Start-Sleep")
        }
        _ => false,
    }
}

/// The arguments as compact JSON with sorted keys (serde_json's map is
/// ordered: `preserve_order` is off), so spacing and key order do not make
/// two identical calls differ. Arguments that are not JSON stay as they are.
fn normalized_arguments(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| arguments.trim().to_string())
}

/// `output` with the guard's note added: as a `loop_guard` field of a JSON
/// object, else after the text.
pub(crate) fn with_note(output: &str, note: &str) -> String {
    match serde_json::from_str::<Value>(output) {
        Ok(Value::Object(mut map)) => {
            map.insert("loop_guard".to_string(), json!(note));
            Value::Object(map).to_string()
        }
        _ => format!("{output}\n\n[loop guard] {note}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(guard: &mut LoopGuard, name: &str, arguments: &str, times: usize) -> Vec<CallCheck> {
        (0..times)
            .map(|_| guard.check_call(name, arguments))
            .collect()
    }

    #[test]
    fn the_third_identical_call_gets_a_note_and_the_fifth_stops_the_turn() {
        let mut guard = LoopGuard::default();
        let results = checks(&mut guard, "read", r#"{"path":"src/main.rs"}"#, 5);
        assert_eq!(results[0], CallCheck::Run);
        assert_eq!(results[1], CallCheck::Run);
        assert_eq!(results[2], CallCheck::Warn(3));
        assert_eq!(results[3], CallCheck::Warn(4));
        let note = repeat_note("read", 3);
        assert!(note.contains("call 3 in a row to read"), "{note}");
        assert!(
            repeat_notice(Some("/root/a"), "read", 3).contains("(/root/a) read was called 3 times")
        );
        let CallCheck::Stop(error) = &results[4] else {
            panic!("{results:?}");
        };
        assert!(error.starts_with(STOP_PREFIX), "{error}");
        assert!(error.contains("read was called 5 times"), "{error}");
    }

    #[test]
    fn spacing_and_key_order_do_not_make_calls_differ() {
        let mut guard = LoopGuard::default();
        guard.check_call("ripgrep", r#"{"pattern":"fn main","path":"src"}"#);
        guard.check_call("ripgrep", r#"{ "path": "src", "pattern": "fn main" }"#);
        assert!(matches!(
            guard.check_call("ripgrep", "{\"path\":\"src\",\n\"pattern\":\"fn main\"}"),
            CallCheck::Warn(_)
        ));
    }

    #[test]
    fn a_different_argument_or_tool_starts_the_count_again() {
        let mut guard = LoopGuard::default();
        checks(&mut guard, "bash", r#"{"command":"cargo test"}"#, 2);
        assert_eq!(
            guard.check_call("bash", r#"{"command":"cargo test -p core"}"#),
            CallCheck::Run
        );
        checks(&mut guard, "bash", r#"{"command":"cargo test"}"#, 2);
        assert_eq!(
            guard.check_call("read", r#"{"command":"cargo test"}"#),
            CallCheck::Run
        );
        let results = checks(&mut guard, "bash", r#"{"command":"cargo test"}"#, 3);
        assert_eq!(results[..2], [CallCheck::Run, CallCheck::Run]);
        assert!(matches!(results[2], CallCheck::Warn(_)));
    }

    #[test]
    fn polling_calls_are_not_counted_and_end_a_run_of_identical_calls() {
        let mut guard = LoopGuard::default();
        for (name, arguments) in [
            ("write_stdin", r#"{"session_id":1,"chars":""}"#),
            ("wait_agent", r#"{"targets":["builder"]}"#),
            ("list_agents", "{}"),
            ("task_list", "{}"),
            ("read_session", r#"{"session":"s1"}"#),
            ("bash", r#"{"command":"sleep 30 && gh pr checks 4"}"#),
            ("exec_command", r#"{"cmd":"sleep 5; cat out.log"}"#),
        ] {
            assert!(checks(&mut guard, name, arguments, 10)
                .iter()
                .all(|check| *check == CallCheck::Run));
        }
        // An orchestrator runs the tests after each worker it waited for.
        let call = r#"{"command":"cargo test"}"#;
        for _ in 0..REPEAT_STOP + 2 {
            assert_eq!(
                guard.check_call("wait_agent", r#"{"targets":["w"]}"#),
                CallCheck::Run
            );
            assert_eq!(guard.check_call("bash", call), CallCheck::Run);
            guard.check_call("write_stdin", r#"{"session_id":1}"#);
            assert_eq!(guard.check_call("bash", call), CallCheck::Run);
        }
    }

    #[test]
    fn a_command_that_ran_and_exited_non_zero_is_no_failure() {
        let result = |output: Value, is_error: bool| crate::tools::ToolExecutionResult {
            output: output.to_string(),
            model_output: String::new(),
            is_error,
        };
        let no_match = result(json!({ "exit_code": 1, "stdout": "", "stderr": "" }), true);
        assert!(!is_failure("bash", &no_match));
        assert!(!is_failure("exec_command", &no_match));
        let timed_out = result(json!({ "exit_code": null, "timed_out": true }), true);
        assert!(is_failure("bash", &timed_out));
        let killed = result(
            json!({ "exit_code": null, "error": "terminated by signal 9" }),
            true,
        );
        assert!(is_failure("bash", &killed));
        let bad_arguments = result(json!({ "error": "missing command" }), true);
        assert!(is_failure("bash", &bad_arguments));
        // Another tool's non-zero exit (git apply refusing a patch) fails.
        assert!(is_failure("apply_patch", &no_match));
        assert!(is_failure(
            "read",
            &result(json!({ "error": "not found" }), true)
        ));
        assert!(!is_failure(
            "read",
            &result(json!({ "content": "x" }), false)
        ));
    }

    #[test]
    fn failed_rounds_in_a_row_stop_the_turn_and_a_success_resets_them() {
        let mut guard = LoopGuard::default();
        for _ in 0..FAILED_ROUNDS_STOP - 1 {
            assert_eq!(guard.record_round(&[("bash", true)]), None);
        }
        // A round with one success among failures is not a failed round.
        assert_eq!(guard.record_round(&[("read", true), ("ls", false)]), None);
        for _ in 0..FAILED_ROUNDS_STOP - 1 {
            assert_eq!(guard.record_round(&[("read", true), ("read", true)]), None);
        }
        let error = guard.record_round(&[("bash", true)]).expect("stops");
        assert!(error.starts_with(STOP_PREFIX), "{error}");
        assert!(error.contains("failed 6 times in a row"), "{error}");
        assert!(error.contains("bash"), "{error}");
        assert_eq!(guard.record_round(&[]), None);
    }

    #[test]
    fn the_note_joins_json_output_as_a_field_and_text_output_as_a_line() {
        let output = with_note(r#"{"exit_code":0,"stdout":"ok"}"#, "stop repeating");
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["loop_guard"], "stop repeating");
        assert_eq!(value["stdout"], "ok");
        assert_eq!(
            with_note("plain text", "stop repeating"),
            "plain text\n\n[loop guard] stop repeating"
        );
    }
}

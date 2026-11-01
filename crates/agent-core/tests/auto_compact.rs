//! Automatic compaction against a scripted local upstream, on each wire
//! protocol: the engine compacts before any request that would go out over
//! the threshold the context gauge shows, whatever the provider reports (or
//! does not report) as usage, and says so when it cannot.

#[path = "support/fake_model.rs"]
mod fake_model;

use fake_model::{read_request_body, setup, temp_dir};
use lynshen_agent_core::{event::ContextBreakdown, AgentCore, AgentEvent, ApprovalMode};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const ASK: &str = "Please audit the timers, this is the only thing I asked.";
const SUMMARY: &str = "the rounds so far, summarized";

#[derive(Clone)]
enum Reply {
    Call(&'static str, String),
    Text(String),
}

/// A reply of the main agent and the input tokens it reports (None: no usage).
struct Step {
    reply: Reply,
    usage: Option<u64>,
}

#[derive(Default)]
struct Script {
    steps: VecDeque<Step>,
    /// Refuse every summary request (the fold request and the plain-text one).
    refuse_summaries: bool,
    /// Refuse a turn's request longer than this as over the context window.
    reject_over_bytes: Option<usize>,
    /// The main agent's requests, then the summary requests.
    turns: Vec<Value>,
    summaries: Vec<Value>,
}

type Shared = Arc<Mutex<Script>>;

/// The body's items, whatever the wire calls them.
fn items(body: &Value) -> Vec<Value> {
    body.get("input")
        .or_else(|| body.get("messages"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A call's id: its arguments' letters and digits, so each call has its own.
fn call_id(arguments: &str) -> String {
    let id: String = arguments
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    format!("c{id}")
}

fn sse_responses(reply: &Reply, usage: Option<u64>) -> String {
    let item = match reply {
        Reply::Call(name, arguments) => json!({
            "type": "function_call", "call_id": call_id(arguments),
            "name": name, "arguments": arguments
        }),
        Reply::Text(text) => json!({
            "type": "message", "role": "assistant",
            "content": [{ "type": "output_text", "text": text }]
        }),
    };
    let mut response = json!({});
    if let Some(input) = usage {
        response["usage"] = json!({ "input_tokens": input, "output_tokens": 1 });
    }
    let mut out = String::new();
    if let Reply::Text(text) = reply {
        let delta = json!({ "type": "response.output_text.delta", "delta": text });
        out.push_str(&format!("data: {delta}\n\n"));
    }
    out.push_str(&format!(
        "data: {}\n\ndata: {}\n\n",
        json!({ "type": "response.output_item.done", "item": item }),
        json!({ "type": "response.completed", "response": response })
    ));
    out
}

fn sse_chat(reply: &Reply, usage: Option<u64>) -> String {
    let (delta, finish) = match reply {
        Reply::Call(name, arguments) => (
            json!({ "tool_calls": [{
                "index": 0, "id": call_id(arguments), "type": "function",
                "function": { "name": name, "arguments": arguments }
            }] }),
            "tool_calls",
        ),
        Reply::Text(text) => (json!({ "content": text }), "stop"),
    };
    let mut out = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({ "choices": [{ "index": 0, "delta": delta, "finish_reason": null }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": finish }] })
    );
    if let Some(input) = usage {
        let usage = json!({ "prompt_tokens": input, "completion_tokens": 1 });
        out.push_str(&format!(
            "data: {}\n\n",
            json!({ "choices": [], "usage": usage })
        ));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn sse_anthropic(reply: &Reply, usage: Option<u64>) -> String {
    let (start, delta, stop) = match reply {
        Reply::Call(name, arguments) => (
            json!({ "type": "tool_use", "id": call_id(arguments), "name": name, "input": {} }),
            json!({ "type": "input_json_delta", "partial_json": arguments }),
            "tool_use",
        ),
        Reply::Text(text) => (
            json!({ "type": "text", "text": "" }),
            json!({ "type": "text_delta", "text": text }),
            "end_turn",
        ),
    };
    let message_usage = json!({ "input_tokens": usage.unwrap_or(0), "output_tokens": 0 });
    [
        json!({ "type": "message_start", "message": { "usage": message_usage } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": start }),
        json!({ "type": "content_block_delta", "index": 0, "delta": delta }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": stop }, "usage": { "output_tokens": 1 } }),
        json!({ "type": "message_stop" }),
    ]
    .iter()
    .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()))
    .collect()
}

/// A local upstream that answers from the script: nothing leaves this machine.
fn start_upstream(wire: &'static str, script: Shared) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let script = Arc::clone(&script);
            thread::spawn(move || {
                let body = read_request_body(&mut stream);
                let last = items(&body)
                    .last()
                    .map(Value::to_string)
                    .unwrap_or_default();
                let sse = |reply: &Reply, usage| match wire {
                    "responses" => sse_responses(reply, usage),
                    "anthropic" => sse_anthropic(reply, usage),
                    _ => sse_chat(reply, usage),
                };
                let reply = if last.contains("<subagent_task") {
                    // The helper takes its time, so the parent goes on meanwhile.
                    thread::sleep(Duration::from_millis(1500));
                    Some(sse(&Reply::Text("helper done".to_string()), None))
                } else if last.contains("<compaction_request>")
                    || last.contains("Summarize this earlier conversation")
                {
                    let mut script = script.lock().unwrap();
                    script.summaries.push(body);
                    (!script.refuse_summaries).then(|| sse(&Reply::Text(SUMMARY.to_string()), None))
                } else if script
                    .lock()
                    .unwrap()
                    .reject_over_bytes
                    .is_some_and(|limit| body.to_string().len() > limit)
                {
                    None
                } else {
                    let mut script = script.lock().unwrap();
                    script.turns.push(body);
                    let step = script.steps.pop_front().unwrap_or(Step {
                        reply: Reply::Text("all done".to_string()),
                        usage: None,
                    });
                    Some(sse(&step.reply, step.usage))
                };
                let _ = match reply {
                    Some(reply) => write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{reply}"
                    ),
                    None => {
                        let error = json!({ "error": { "message": "maximum context length exceeded" } }).to_string();
                        write!(
                            stream,
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                            error.len()
                        )
                    }
                };
            });
        }
    });
    format!("http://{address}/v1")
}

/// config.json pointed at the upstream, until dropped.
struct Upstream {
    script: Shared,
    config: PathBuf,
    original: String,
}

const WINDOW: u64 = 60_000;

impl Upstream {
    fn start(wire: &'static str) -> Self {
        Self::with_window(wire, WINDOW)
    }

    fn with_window(wire: &'static str, window: u64) -> Self {
        let script = Shared::default();
        let base_url = start_upstream(wire, Arc::clone(&script));
        let config = PathBuf::from(std::env::var("HOME").unwrap())
            .join(".lynshen")
            .join("config.json");
        let original = fs::read_to_string(&config).unwrap();
        let mut changed: Value = serde_json::from_str(&original).unwrap();
        changed["base_url"] = json!(base_url);
        changed["protocol"] = json!(wire);
        changed["compaction_threshold_percent"] = json!(95);
        changed["models"] = json!([{
            "name": "fake-model", "context_window": window, "max_output_tokens": 4096
        }]);
        fs::write(&config, changed.to_string()).unwrap();
        Self {
            script,
            config,
            original,
        }
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        let _ = fs::write(&self.config, &self.original);
    }
}

fn pump(core: &mut AgentCore) -> Vec<AgentEvent> {
    let deadline = Instant::now() + Duration::from_secs(60);
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
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out; events: {}", brief(&seen));
}

/// The events worth reading when a test fails (the rest are long).
fn brief(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Connecting => Some("request".to_string()),
            AgentEvent::CompactionStart => Some("compaction start".to_string()),
            AgentEvent::CompactionEnd { .. } => Some("compaction end".to_string()),
            AgentEvent::CompactionFailed(error) => Some(format!("compaction failed: {error}")),
            AgentEvent::Error(error) => Some(format!("error: {error}")),
            AgentEvent::Info(info) => Some(format!("info: {info}")),
            AgentEvent::ContextUsage { .. } => Some(format!("gauge {}", gauge(event)?)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The number the gauge shows for a `context_usage` event: the whole request
/// when it comes in parts, as the desktop and the TUI add it up.
fn gauge(event: &AgentEvent) -> Option<u64> {
    let AgentEvent::ContextUsage {
        tokens, breakdown, ..
    } = event
    else {
        return None;
    };
    Some(
        breakdown
            .as_ref()
            .map_or(*tokens, |parts: &ContextBreakdown| {
                parts.system_prompt
                    + parts.skills
                    + parts.system_tools
                    + parts.mcp_tools
                    + parts.messages
            }),
    )
}

/// The threshold the gauge shows (its caption and its mark).
fn shown_limit(core: &AgentCore) -> u64 {
    match core.model_status_event() {
        AgentEvent::ModelStatus { context_limit, .. } => context_limit,
        _ => unreachable!(),
    }
}

/// Files big enough that a few rounds of reading them pass the threshold.
fn big_files(dir: &Path, count: usize) {
    for n in 0..count {
        let lines: String = (0..400)
            .map(|line| format!("file {n} line {line}: lorem ipsum dolor sit amet consectetur\n"))
            .collect();
        fs::write(dir.join(format!("big-{n}.txt")), lines).unwrap();
    }
}

fn read_steps(count: usize, usage: Option<u64>) -> VecDeque<Step> {
    (0..count)
        .map(|n| Step {
            reply: Reply::Call(
                "read",
                json!({ "path": format!("big-{n}.txt") }).to_string(),
            ),
            usage,
        })
        .collect()
}

fn open(dir: &Path) -> AgentCore {
    let mut core = AgentCore::open(dir.to_path_buf()).unwrap();
    core.set_approval_mode(ApprovalMode::FullAccess);
    core
}

fn errors(events: &[AgentEvent]) -> Vec<&String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Error(error) => Some(error),
            _ => None,
        })
        .collect()
}

fn count(events: &[AgentEvent], wanted: impl Fn(&AgentEvent) -> bool) -> usize {
    events.iter().filter(|event| wanted(event)).count()
}

/// No request of the turn went out while the gauge stood over `limit`.
fn assert_no_request_over(events: &[AgentEvent], limit: u64) {
    let mut shown = 0;
    for event in events {
        if let Some(tokens) = gauge(event) {
            shown = tokens;
        }
        if matches!(event, AgentEvent::Connecting) {
            assert!(
                shown <= limit,
                "a request went out at {shown} with the threshold at {limit}:\n{}",
                brief(events)
            );
        }
    }
}

#[test]
fn one_long_turn_compacts_at_the_threshold_shown_on_every_wire_whatever_the_usage() {
    let _guard = setup();
    // The ticket: GPT-6.1 Sol on Responses reported about 80% of the gauge.
    // The provider's number being low (or absent) must not hold compaction back.
    for (wire, usage) in [
        ("responses", Some(100)),
        ("responses", None),
        ("chat", Some(100)),
        ("chat", None),
        ("anthropic", Some(100)),
    ] {
        let upstream = Upstream::start(wire);
        let dir = temp_dir("long-turn");
        big_files(&dir, 60);
        upstream.script().steps = read_steps(60, usage);
        let mut core = open(&dir);
        let limit = shown_limit(&core);
        assert_eq!(limit, WINDOW * 95 / 100);

        // One message, and everything after it is the same turn's tool rounds.
        core.submit_user_message(ASK.to_string());
        let events = pump(&mut core);

        let context = format!("{wire} usage {usage:?}:\n{}", brief(&events));
        assert!(errors(&events).is_empty(), "{context}");
        assert!(
            count(&events, |e| matches!(e, AgentEvent::CompactionEnd { .. })) >= 1,
            "{context}"
        );
        assert_no_request_over(&events, limit);
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::AssistantDelta(text) if text.contains("all done"))
            ),
            "the turn did not finish: {context}"
        );

        // After it the turn goes on from the summary and the user's request,
        // word for word, and every tool call still has its result.
        let script = upstream.script();
        let after = script
            .turns
            .iter()
            .find(|body| body.to_string().contains(SUMMARY))
            .unwrap_or_else(|| panic!("no request after the compaction: {context}"));
        let text = after.to_string();
        let (summary_at, ask_at) = (text.find(SUMMARY).unwrap(), text.find(ASK));
        assert!(ask_at.is_some_and(|at| summary_at < at), "{text}");
        // The summary request read the rounds it replaces.
        assert!(script.summaries[0].to_string().contains("big-0.txt"));
        drop(script);
        drop(upstream);
    }
}

#[test]
fn a_kept_tool_round_keeps_its_call_after_a_compaction() {
    let _guard = setup();
    let upstream = Upstream::start("responses");
    let dir = temp_dir("pairs");
    big_files(&dir, 60);
    upstream.script().steps = read_steps(60, Some(100));
    let mut core = open(&dir);
    core.submit_user_message(ASK.to_string());
    pump(&mut core);

    for body in &upstream.script().turns {
        let items = items(body);
        let id_of = |wanted: &str| -> Vec<String> {
            items
                .iter()
                .filter(|item| item["type"] == wanted)
                .filter_map(|item| item["call_id"].as_str().map(str::to_string))
                .collect()
        };
        assert_eq!(id_of("function_call"), id_of("function_call_output"));
    }
}

#[test]
fn a_turn_over_the_threshold_with_nothing_to_fold_says_so_and_goes_on() {
    let _guard = setup();
    let upstream = Upstream::start("responses");
    let dir = temp_dir("nothing-to-fold");
    big_files(&dir, 3);
    // By the provider's count the first request is already over the threshold;
    // the conversation is one message and a small tool round: too recent to fold.
    let mut steps = read_steps(3, Some(100));
    steps[0].usage = Some(WINDOW);
    upstream.script().steps = steps;
    let mut core = open(&dir);
    let limit = shown_limit(&core);

    core.submit_user_message(ASK.to_string());
    let events = pump(&mut core);

    let context = brief(&events);
    assert!(errors(&events).is_empty(), "{context}");
    assert_eq!(
        count(&events, |e| matches!(e, AgentEvent::CompactionFailed(_))),
        1,
        "{context}"
    );
    // The gauge showed the provider's number the engine went by.
    assert!(
        events.iter().filter_map(gauge).any(|n| n > limit),
        "{context}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::AssistantDelta(text) if text.contains("all done"))),
        "{context}"
    );
}

#[test]
fn a_failed_compaction_between_rounds_is_shown_and_not_tried_on_every_round() {
    let _guard = setup();
    let upstream = Upstream::start("responses");
    let dir = temp_dir("refused-summary");
    big_files(&dir, 60);
    {
        let mut script = upstream.script();
        script.steps = read_steps(40, Some(100));
        script.refuse_summaries = true;
    }
    let mut core = open(&dir);

    core.submit_user_message(ASK.to_string());
    let events = pump(&mut core);

    let context = brief(&events);
    let failed = count(&events, |e| matches!(e, AgentEvent::CompactionFailed(_)));
    assert!(failed >= 1, "{context}");
    // The turn carries on with its whole context.
    assert!(errors(&events).is_empty(), "{context}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::AssistantDelta(text) if text.contains("all done"))),
        "{context}"
    );
    // Another try only once the context has grown: well under one per round.
    assert!(failed < 10, "{failed} tries in 40 rounds:\n{context}");
}

#[test]
fn a_compaction_between_rounds_leaves_a_running_subagent_alone() {
    let _guard = setup();
    let upstream = Upstream::start("responses");
    let dir = temp_dir("subagent-survives");
    big_files(&dir, 3);
    let call = |name, arguments: Value, usage| Step {
        reply: Reply::Call(name, arguments.to_string()),
        usage: Some(usage),
    };
    upstream.script().steps = VecDeque::from([
        Step {
            reply: Reply::Text("noted".to_string()),
            usage: Some(100),
        },
        call(
            "spawn_agent",
            json!({ "task_name": "helper", "background": false, "message": "look around" }),
            100,
        ),
        // The provider's count passes the threshold while the helper works.
        call("read", json!({ "path": "big-0.txt" }), WINDOW),
        call(
            "wait_agent",
            json!({ "targets": ["helper"], "timeout_ms": 20000 }),
            100,
        ),
    ]);
    let mut core = open(&dir);
    // An earlier turn to fold, under the threshold by itself.
    core.submit_user_message("lorem ipsum dolor sit amet ".repeat(5000));
    pump(&mut core);

    core.submit_user_message(ASK.to_string());
    let events = pump(&mut core);

    let context = brief(&events);
    assert!(
        count(&events, |e| matches!(e, AgentEvent::CompactionEnd { .. })) >= 1,
        "{context}"
    );
    let AgentEvent::AgentRuns(rows) = core.agent_runs_event() else {
        unreachable!()
    };
    let helper = rows
        .first()
        .unwrap_or_else(|| panic!("no helper\n{context}"));
    assert_eq!(helper["state"], "completed", "{helper}\n{context}");
}

#[test]
fn a_rejection_as_too_long_in_one_long_turn_compacts_that_turns_rounds() {
    let _guard = setup();
    // The window configured is far past what the route serves: only the
    // upstream's rejection tells. (The ring shows 100% long before.)
    let upstream = Upstream::with_window("responses", 100_000_000);
    let dir = temp_dir("rejected");
    big_files(&dir, 40);
    {
        let mut script = upstream.script();
        script.steps = read_steps(40, Some(100));
        script.reject_over_bytes = Some(200_000);
    }
    let mut core = open(&dir);

    core.submit_user_message(ASK.to_string());
    let events = pump(&mut core);

    let context = brief(&events);
    assert!(errors(&events).is_empty(), "{context}");
    assert!(
        count(&events, |e| matches!(e, AgentEvent::CompactionEnd { .. })) >= 1,
        "{context}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::AssistantDelta(text) if text.contains("all done"))),
        "{context}"
    );
}

// What a person can do while a long turn runs.

#[test]
fn stopping_a_long_turn_and_sending_a_message_goes_on_under_the_threshold() {
    let _guard = setup();
    let upstream = Upstream::start("responses");
    let dir = temp_dir("stop-then-message");
    big_files(&dir, 60);
    upstream.script().steps = read_steps(60, Some(100));
    let mut core = open(&dir);
    let limit = shown_limit(&core);
    core.submit_user_message(ASK.to_string());

    // Stopped well past the threshold's worth of rounds.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut outputs = 0;
    let mut peak = 0;
    while outputs < 30 && Instant::now() < deadline {
        for event in core.poll_events() {
            peak = peak.max(gauge(&event).unwrap_or(0));
            outputs += usize::from(matches!(event, AgentEvent::ToolOutput { .. }));
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(outputs >= 30, "only {outputs} rounds");
    core.interrupt();
    upstream.script().steps.clear();
    eprintln!("stopped at {outputs} rounds, gauge peak {peak}, limit {limit}");

    core.submit_user_message("go on".to_string());
    let next = pump(&mut core);
    eprintln!("after the message:\n{}", brief(&next));
    assert!(errors(&next).is_empty(), "{}", brief(&next));
    assert_no_request_over(&next, limit);
}

#[test]
fn the_compact_command_waits_for_the_turn_and_folds_its_rounds() {
    let _guard = setup();
    let upstream = Upstream::start("responses");
    let dir = temp_dir("compact-command");
    // Past what a compaction keeps, and short of the threshold (about 1,450 tokens each).
    big_files(&dir, 34);
    upstream.script().steps = read_steps(34, Some(100));
    let mut core = open(&dir);
    core.submit_user_message(ASK.to_string());
    let (_, running) = core.handle_command("/compact");
    assert!(
        errors(&running)
            .iter()
            .any(|error| error.contains("while a response is running")),
        "{}",
        brief(&running)
    );
    let turn = pump(&mut core);
    let before = turn.iter().filter_map(gauge).next_back().unwrap_or(0);

    // The only user message is the one that started all those rounds.
    let (_, mut events) = core.handle_command("/compact");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Info(info) if info.contains("nothing old enough"))),
        "/compact found nothing to fold in a turn of {before} tokens"
    );
    events.extend(pump(&mut core));
    let after = events.iter().filter_map(gauge).next_back().unwrap_or(0);
    let context = format!("{before} -> {after}\n{}", brief(&events));
    eprintln!("/compact after one long turn: {context}");
    assert!(errors(&events).is_empty(), "{context}");
    assert_eq!(
        count(&events, |e| matches!(e, AgentEvent::CompactionEnd { .. })),
        1,
        "{context}"
    );
    assert!(after < before / 2, "{context}");

    // The next request still has what the user asked for.
    core.submit_user_message("thanks".to_string());
    pump(&mut core);
    let last = upstream.script().turns.last().unwrap().to_string();
    assert!(last.contains(ASK) && last.contains(SUMMARY), "{last}");
}

#[test]
fn a_small_window_compacts_too() {
    let _guard = setup();
    // Its whole context is less than a compaction keeps on a large window.
    let upstream = Upstream::with_window("responses", 12_000);
    let dir = temp_dir("small-window");
    big_files(&dir, 12);
    upstream.script().steps = read_steps(12, Some(100));
    let mut core = open(&dir);
    let limit = shown_limit(&core);

    core.submit_user_message(ASK.to_string());
    let events = pump(&mut core);

    let context = brief(&events);
    assert!(errors(&events).is_empty(), "{context}");
    assert!(
        count(&events, |e| matches!(e, AgentEvent::CompactionEnd { .. })) >= 1,
        "{context}"
    );
    assert_no_request_over(&events, limit);
}

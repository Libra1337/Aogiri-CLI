//! A temporary HOME and a scripted local chat-completions server, shared by
//! the agent-core and daemon integration tests. Nothing here reads or writes
//! the developer's `~/.lynshen` or calls a real provider.

use serde_json::{json, Value};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    thread,
};

/// Every request body the fake model received, oldest first.
static REQUESTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

/// The request bodies received so far (see `REQUESTS`).
#[allow(dead_code)]
pub fn requests() -> Vec<Value> {
    REQUESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// Tests share one HOME and one fake server; they run one at a time because
/// the engines share `~/.lynshen/config.json`.
pub fn setup() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    static INIT: OnceLock<()> = OnceLock::new();
    let guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    INIT.get_or_init(|| {
        let home = temp_dir("home");
        env::set_var("HOME", &home);
        env::remove_var("USERPROFILE");
        env::set_var("LYNSHEN_FAKE_KEY", "test-key");
        let base_url = start_fake_model();
        let profile = home.join(".lynshen");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("config.json"),
            json!({
                "provider": "fake",
                "protocol": "chat",
                "model": "fake-model",
                "base_url": base_url,
                "api_key_env": "LYNSHEN_FAKE_KEY",
                "retry_attempts": 0,
                "include_project_instructions": false,
            })
            .to_string(),
        )
        .unwrap();
    });
    guard
}

pub fn temp_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = env::temp_dir().join(format!(
        "lynshen-hosted-{}-{}-{label}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A scripted model: a user message `RUN: <command>` answers with one bash
/// call, `CALL <name> <arguments>` with that call, `CALLS [{name,
/// arguments}, …]` with all of them in one reply; `LOOP <tool> <arguments>`
/// makes that call again after every result (`{n}` in the arguments becomes
/// the call's number), `REPEAT <times> <tool> <arguments>` `times` calls in
/// all; anything after a tool result or a deferred-action
/// message answers with plain text naming what it saw; `HISTORY` answers
/// with every user message of the request; `OVERFLOW …` is refused as over
/// the context window until the conversation was compacted. A compaction's
/// summary request (`<compaction_request>`) is answered with a tool call
/// only when an earlier user message says `SUMMARY_CALLS_TOOL` and the
/// request offers tools, with a sentence and a tool call when one says
/// `SUMMARY_PREAMBLE_CALLS_TOOL` and it offers tools, always with a tool
/// call when one says `SUMMARY_ONLY_CALLS_TOOL`, and refused as over the
/// context window when one says `SUMMARY_REFUSED`. In a subagent's task, `[sleep:N]`
/// waits N ms before answering and a line `RUN: <command>` runs it;
/// `[history]` answers with every user message of the request. A message
/// from another conversation (`<session_message …>`) is scripted by its body.
/// The engine's plan reminder at a turn's end is answered, once a user
/// message said `MARK_PLAN_DONE`, with update_plan marking every step it
/// lists completed.
/// `SLOW_REASONING <n>` streams n reasoning chunks 50 ms apart, then a short
/// answer; `SLOW_RUN <command>` streams a bash call's arguments in chunks
/// 50 ms apart (about a second in all). A network that went quiet:
/// `STALL` is never answered, `STALL_MIDWAY` streams "partial" and then
/// nothing (the connection stays open for a minute either way); `BUSY` is
/// answered 503 with Retry-After: 2.
fn start_fake_model() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let body = read_request_body(&mut stream);
                REQUESTS
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(body.clone());
                let reply = script(&body);
                if reply == STALL {
                    thread::sleep(std::time::Duration::from_secs(60));
                    return;
                }
                if reply == BUSY {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 2\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy"
                    );
                    return;
                }
                let _ = match reply.strip_prefix("ERROR ") {
                    Some(message) => {
                        let error = json!({ "error": { "message": message } }).to_string();
                        write!(
                            stream,
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                            error.len()
                        )
                    }
                    None => write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                    )
                    .and_then(|()| {
                        // A slow reply (`sse_slow`) waits between its chunks;
                        // the client may hang up meanwhile. One that stalls
                        // (`HANG`) sends nothing more.
                        let (reply, hangs) = match reply.split_once(HANG) {
                            Some((sent, _)) => (sent, true),
                            None => (reply.as_str(), false),
                        };
                        for (index, part) in reply.split(PAUSE).enumerate() {
                            if index > 0 {
                                stream.flush()?;
                                thread::sleep(std::time::Duration::from_millis(50));
                            }
                            stream.write_all(part.as_bytes())?;
                        }
                        if hangs {
                            stream.flush()?;
                            thread::sleep(std::time::Duration::from_secs(60));
                        }
                        Ok(())
                    }),
                };
            });
        }
    });
    format!("http://{address}/v1")
}

fn read_request_body(stream: &mut std::net::TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn script(request: &Value) -> String {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or(Value::Null);
    let text = content_text(&last["content"]);
    let task = text.starts_with("<subagent_task");
    if task {
        if let Some(ms) = text
            .split("[sleep:")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .and_then(|ms| ms.parse::<u64>().ok())
        {
            thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
    if task && text.contains("[history]") {
        let history = messages
            .iter()
            .filter(|message| message["role"] == "user")
            .map(|message| content_text(&message["content"]))
            .collect::<Vec<_>>()
            .join(" | ");
        return sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "content": format!("history: {history}") }, "finish_reason": null }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
        ]);
    }
    if let Some(reply) = plan_reminder_reply(&messages, &text) {
        return reply;
    }
    if let Some(reply) = loop_call(&messages) {
        return reply;
    }
    if let Some(reply) = summary_reply(request, &messages, &text) {
        return reply;
    }
    let compacted = messages.iter().any(|message| {
        content_text(&message["content"]).starts_with("[Earlier conversation compacted")
    });
    if text.starts_with("OVERFLOW") && !compacted {
        return "ERROR maximum context length exceeded".to_string();
    }
    let task_command = task
        .then(|| text.lines().find_map(|line| line.strip_prefix("RUN: ")))
        .flatten();
    if let (true, Some(command)) = (last["role"] == "user", task_command) {
        let arguments = json!({ "command": command }).to_string();
        return sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "bash", "arguments": arguments }
            }] }, "finish_reason": null }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
        ]);
    }
    if let Some(reply) = thinking_only_reply(&messages) {
        return reply;
    }
    if last["role"] == "user" {
        match text.as_str() {
            "STALL" => return STALL.to_string(),
            "BUSY" => return BUSY.to_string(),
            "STALL_MIDWAY" => {
                let partial = json!({ "choices": [{ "index": 0, "delta": { "content": "partial" }, "finish_reason": null }] });
                return format!("data: {partial}\n\n{HANG}");
            }
            _ => {}
        }
        let text = text
            .strip_prefix("<session_message ")
            .and_then(|rest| rest.split_once(">\n"))
            .map(|(_, body)| body.split("\n</session_message>").next().unwrap_or(body))
            .unwrap_or(&text);
        if let Some(calls) = text
            .strip_prefix("CALLS ")
            .and_then(|calls| serde_json::from_str::<Vec<Value>>(calls).ok())
        {
            let tool_calls = calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    json!({
                        "index": index, "id": format!("call_{}", index + 1), "type": "function",
                        "function": { "name": call["name"], "arguments": call["arguments"].to_string() }
                    })
                })
                .collect::<Vec<_>>();
            return sse(&[
                json!({ "choices": [{ "index": 0, "delta": { "tool_calls": tool_calls }, "finish_reason": null }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            ]);
        }
        if let Some(count) = text
            .strip_prefix("SLOW_REASONING ")
            .and_then(|count| count.parse::<usize>().ok())
        {
            let mut chunks = (1..=count)
                .map(|n| json!({ "choices": [{ "index": 0, "delta": { "reasoning_content": format!("thinking {n} ") }, "finish_reason": null }] }))
                .collect::<Vec<_>>();
            chunks.push(json!({ "choices": [{ "index": 0, "delta": { "content": "done thinking" }, "finish_reason": null }] }));
            chunks
                .push(json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }));
            return sse_slow(&chunks);
        }
        if let Some(command) = text.strip_prefix("SLOW_RUN ") {
            let arguments = json!({ "command": command }).to_string();
            let pieces = arguments
                .as_bytes()
                .chunks(arguments.len().div_ceil(20))
                .map(|piece| String::from_utf8_lossy(piece).into_owned())
                .collect::<Vec<_>>();
            let mut chunks = pieces
                .iter()
                .enumerate()
                .map(|(n, piece)| {
                    let call = if n == 0 {
                        json!({ "index": 0, "id": "call_slow", "type": "function", "function": { "name": "bash", "arguments": piece } })
                    } else {
                        json!({ "index": 0, "function": { "arguments": piece } })
                    };
                    json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [call] }, "finish_reason": null }] })
                })
                .collect::<Vec<_>>();
            chunks.push(
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            );
            return sse_slow(&chunks);
        }
        if let Some(call) = text.strip_prefix("CALL ") {
            let (name, arguments) = call.split_once(' ').unwrap_or((call, "{}"));
            return sse(&[
                json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }] }, "finish_reason": null }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            ]);
        }
        if text == "HISTORY" {
            let history = messages
                .iter()
                .filter(|message| message["role"] == "user")
                .map(|message| content_text(&message["content"]))
                .collect::<Vec<_>>()
                .join(" | ");
            return sse(&[
                json!({ "choices": [{ "index": 0, "delta": { "content": format!("history: {history}") }, "finish_reason": null }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
            ]);
        }
        if text == "SYSTEM" {
            let system = messages
                .iter()
                .find(|message| message["role"] == "system")
                .and_then(|message| message["content"].as_str())
                .unwrap_or_default();
            // The end of the system prompt: the session's own parts (chat or
            // agent prompt, sandbox note) come last.
            let tail: String = system
                .chars()
                .rev()
                .take(4000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return sse(&[
                json!({ "choices": [{ "index": 0, "delta": { "content": tail }, "finish_reason": null }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
            ]);
        }
        if let Some(command) = text.strip_prefix("RUN: ") {
            let arguments = json!({ "command": command }).to_string();
            return sse(&[
                json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": { "name": "bash", "arguments": arguments }
                }] }, "finish_reason": null }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            ]);
        }
    }
    let reply = if last["role"] == "tool" {
        format!("tool said: {text}")
    } else {
        format!("user said: {text}")
    };
    sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "content": reply }, "finish_reason": null }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
    ])
}

/// Thinking and no answer, scripted by the user's latest message (the
/// engine's empty-response reminders after it do not count):
/// `THINK_LENGTH` stops on the output limit, `THINK_FILTER` on a content
/// filter, `THINK_EMPTY` normally (every time it is reminded, too).
fn thinking_only_reply(messages: &[Value]) -> Option<String> {
    let asked = messages
        .iter()
        .rev()
        .filter(|message| message["role"] == "user")
        .map(|message| content_text(&message["content"]))
        .find(|text| !text.starts_with("<runtime_reminder>"))?;
    let finish = match asked.as_str() {
        "THINK_LENGTH" => "length",
        "THINK_FILTER" => "content_filter",
        "THINK_EMPTY" => "stop",
        _ => return None,
    };
    Some(sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "reasoning_content": "thinking" }, "finish_reason": null }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": finish }] }),
        json!({ "choices": [], "usage": { "prompt_tokens": 5, "completion_tokens": 8192 } }),
    ]))
}

/// The answer to a summary request scripted by `SUMMARY_CALLS_TOOL`,
/// `SUMMARY_PREAMBLE_CALLS_TOOL`, `SUMMARY_ONLY_CALLS_TOOL` or
/// `SUMMARY_REFUSED` (see `start_fake_model`); None otherwise.
fn summary_reply(request: &Value, messages: &[Value], text: &str) -> Option<String> {
    if !text.starts_with("<compaction_request>") {
        return None;
    }
    let said = |marker: &str| {
        messages.iter().any(|message| {
            message["role"] == "user" && content_text(&message["content"]).contains(marker)
        })
    };
    if said("SUMMARY_REFUSED") {
        return Some("ERROR maximum context length exceeded".to_string());
    }
    let has_tools = request["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty());
    let tool_call = json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
        "index": 0, "id": "call_summary", "type": "function",
        "function": { "name": "read", "arguments": "{\"path\":\"notes.txt\"}" }
    }] }, "finish_reason": null }] });
    let tool_calls_end =
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] });
    if said("SUMMARY_PREAMBLE_CALLS_TOOL") && has_tools {
        return Some(sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "content": "I'll read notes.txt first." }, "finish_reason": null }] }),
            tool_call,
            tool_calls_end,
        ]));
    }
    if said("SUMMARY_ONLY_CALLS_TOOL") || (said("SUMMARY_CALLS_TOOL") && has_tools) {
        return Some(sse(&[tool_call, tool_calls_end]));
    }
    (said("SUMMARY_CALLS_TOOL") || said("SUMMARY_PREAMBLE_CALLS_TOOL")).then(|| {
        sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "content": "summary without tools" }, "finish_reason": null }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
        ])
    })
}

/// `LOOP <tool> <arguments>` as the latest user message: the same call
/// again, whatever the tool said; `REPEAT <times> <tool> <arguments>`: the
/// same, `times` calls in all.
fn loop_call(messages: &[Value]) -> Option<String> {
    let at = messages
        .iter()
        .rposition(|message| message["role"] == "user")?;
    let text = content_text(&messages[at]["content"]);
    let (times, call) = match text.strip_prefix("REPEAT ") {
        Some(rest) => {
            let (times, call) = rest.split_once(' ')?;
            (times.parse::<usize>().ok()?, call)
        }
        None => (usize::MAX, text.strip_prefix("LOOP ")?),
    };
    let (name, arguments) = call.split_once(' ').unwrap_or((call, "{}"));
    let n = messages[at..]
        .iter()
        .filter(|message| message["role"] == "tool")
        .count()
        + 1;
    if n > times {
        return None;
    }
    let arguments = arguments.replace("{n}", &n.to_string());
    Some(sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0, "id": format!("call_{n}"), "type": "function",
            "function": { "name": name, "arguments": arguments }
        }] }, "finish_reason": null }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
    ]))
}

/// The answer to the plan reminder at a turn's end scripted by
/// `MARK_PLAN_DONE` (see `start_fake_model`); None otherwise.
fn plan_reminder_reply(messages: &[Value], text: &str) -> Option<String> {
    if !text.starts_with("<system-reminder>\nYour turn is about to end") {
        return None;
    }
    let marked = messages.iter().any(|message| {
        message["role"] == "user" && content_text(&message["content"]) == "MARK_PLAN_DONE"
    });
    if !marked {
        return None;
    }
    // The reminder lists the plan as `N. [status] step`.
    let plan = text
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(". [")?;
            let (_, step) = rest.split_once("] ")?;
            Some(json!({ "step": step, "status": "completed" }))
        })
        .collect::<Vec<_>>();
    let arguments = json!({ "plan": plan }).to_string();
    Some(sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0, "id": "call_plan", "type": "function",
            "function": { "name": "update_plan", "arguments": arguments }
        }] }, "finish_reason": null }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
    ]))
}

/// Where a slow reply waits before it streams on (see `sse_slow`).
const PAUSE: &str = "\u{1}";
/// Where a reply stops for good, the connection left open.
const HANG: &str = "\u{2}";
/// Replies that are not HTTP responses (see `start_fake_model`).
const STALL: &str = "\u{3}STALL";
const BUSY: &str = "\u{3}BUSY";

/// `sse` with a pause of 50 ms after each chunk.
fn sse_slow(chunks: &[Value]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n{PAUSE}"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

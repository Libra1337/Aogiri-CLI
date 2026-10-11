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
/// the context window until the conversation was compacted. In a subagent's task, `[sleep:N]`
/// waits N ms before answering and a line `RUN: <command>` runs it;
/// `[history]` answers with every user message of the request. A message
/// from another conversation (`<session_message …>`) is scripted by its body.
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
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{reply}"
                    ),
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
    if let Some(reply) = loop_call(&messages) {
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
    if last["role"] == "user" {
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
            let tail: String = system
                .chars()
                .rev()
                .take(2000)
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

fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

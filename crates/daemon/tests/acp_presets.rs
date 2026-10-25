//! The built-in ACP agents end to end: a real daemon starts oh-my-pi (a
//! fake `omp` first on PATH, playing the agent over ACP) by id, from a phone
//! too; the agent signs in once, gets the user's MCP servers, reports its
//! config options, asks a form, restarts for another approval mode and moves
//! to its TUI, which any client attaches to. Its own test binary: it changes
//! PATH.
#![cfg(unix)]

#[path = "../../agent-core/tests/support/fake_model.rs"]
mod fake_model;

use base64::{engine::general_purpose::STANDARD, Engine};
use fake_model::{setup, temp_dir};
use lynshen_daemon::Store;
use serde_json::{json, Value};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

struct Client {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next: u64,
}

impl Client {
    fn connect(address: &str, token: &str) -> Self {
        let (socket, _) = tungstenite::connect(format!("ws://{address}/?token={token}")).unwrap();
        if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
        }
        Self { socket, next: 1000 }
    }

    fn send(&mut self, frame: Value) {
        self.socket.send(Message::text(frame.to_string())).unwrap();
    }

    fn until(&mut self, mut done: impl FnMut(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                    let finished = done(&frame);
                    seen.push(frame);
                    if finished {
                        return seen;
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("socket error {error}; frames: {seen:#?}"),
            }
        }
        panic!("timed out; frames: {seen:#?}");
    }

    fn request(&mut self, mut op: Value) -> Value {
        self.next += 1;
        let id = self.next;
        op["id"] = json!(id);
        self.send(op);
        self.until(|frame| frame["id"] == id).pop().unwrap()
    }
}

fn of<'a>(session: &'a str, kind: &'a str) -> impl FnMut(&Value) -> bool + 'a {
    move |f| f["session"] == session && f["type"] == kind
}

/// The reply text of the turn among `frames`.
fn reply(frames: &[Value], session: &str) -> String {
    frames
        .iter()
        .filter(|f| f["session"] == session && f["type"] == "assistant_delta")
        .filter_map(|f| f["delta"].as_str())
        .collect()
}

fn ready_after_reply(session: &str) -> impl FnMut(&Value) -> bool + '_ {
    let mut replied = false;
    move |f| {
        replied |= f["session"] == session && f["type"] == "assistant_delta";
        replied && f["session"] == session && f["type"] == "status" && f["message"] == "ready"
    }
}

fn term_text(frame: &Value, term: &str) -> String {
    if frame["type"] != "term_output" || frame["term"] != term {
        return String::new();
    }
    String::from_utf8_lossy(&STANDARD.decode(frame["data"].as_str().unwrap()).unwrap()).to_string()
}

#[test]
fn a_built_in_agent_runs_from_any_client_with_its_options_forms_sign_in_mcp_and_tui() {
    let _guard = setup();
    let support = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support");
    let bin = temp_dir("acp-presets-bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(support.join("fake_omp.sh"), bin.join("omp")).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(bin).chain(std::env::split_paths(&path));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    std::env::set_var("FAKE_ACP", support.join("fake_acp.py"));
    std::env::set_var("FAKE_ACP_AUTH", "1");
    // The user's MCP servers: an enabled stdio one goes to the agent; an
    // http one does not (this agent takes none), nor a disabled one.
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let config_path = home.join(".lynshen/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let before = config.clone();
    config["mcp_servers"] = json!([
        { "name": "files", "command": "mcp-files" },
        { "name": "web", "transport": "http", "url": "https://example.com/mcp" },
        { "name": "off", "command": "mcp-off", "enabled": false },
    ]);
    fs::write(&config_path, config.to_string()).unwrap();
    let dir = temp_dir("acp-presets");
    fs::create_dir_all(&dir).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let state = temp_dir("acp-presets-state");
    let store = Store::open(state.join("daemon")).unwrap();
    let token = store.token().unwrap();
    let agents = lynshen_daemon::Agents::open(state.join("agents")).unwrap();
    thread::spawn(move || lynshen_daemon::serve(listener, store, agents, None, "test", None));
    let mut desktop = Client::connect(&address, &token);

    // The presets and whether each can start here.
    let presets = desktop.request(json!({ "op": "acp_presets" }));
    let omp = presets["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "omp")
        .unwrap()
        .clone();
    assert_eq!(omp["status"], "ready", "{presets}");
    assert_eq!(omp["version"], "18.1.21");
    assert_eq!(omp["approvals"], true);
    let pi = presets["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "pi")
        .unwrap()
        .clone();
    assert_eq!(pi["approvals"], false);

    // A phone starts it by id (it names no program).
    desktop.send(json!({ "op": "pair_start", "id": 1 }));
    let code = desktop.until(|f| f["id"] == 1).pop().unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();
    let paired = pair(&address, &code);
    let confirmed = desktop.request(json!({ "op": "device_confirm", "device": paired["device"] }));
    assert_eq!(confirmed["type"], "device_confirmed", "{confirmed}");
    let mut phone = Client::connect(&address, paired["token"].as_str().unwrap());
    let refused = phone.request(json!({ "op": "session_create", "cwd": dir, "engine": "acp",
        "options": { "agent": "omp", "command": "/bin/sh" } }));
    assert_eq!(refused["type"], "error", "{refused}");
    // Pi never asks: a phone without the 全自动 scope may not start it.
    let refused = phone.request(json!({ "op": "session_create", "cwd": dir, "engine": "acp",
        "options": { "agent": "pi", "approval_mode": "full-access" } }));
    assert!(
        refused["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("scope_required:full_auto"),
        "{refused}"
    );
    let created = phone.request(json!({ "op": "session_create", "cwd": dir, "engine": "acp",
        "options": { "agent": "omp", "approval_mode": "read-only", "effort": "high" } }));
    let session = created["session"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    desktop.send(json!({ "op": "watch", "session": session }));
    let opened = desktop.until(of(&session, "engine_caps"));
    // It signed in once (the agent's own way), then opened.
    assert!(opened.iter().any(|f| f["type"] == "info"
        && f["message"]
            .as_str()
            .unwrap_or_default()
            .contains("signing in to oh-my-pi")));
    let caps = opened.last().unwrap();
    assert_eq!(caps["agent"]["id"], "omp");
    assert_eq!(caps["caps"]["approvalModes"], true);
    let status = desktop.until(of(&session, "model_status"));
    let status = status.last().unwrap();
    assert_eq!(status["model_label"], "One");
    assert_eq!(status["default_effort"], "auto");
    assert_eq!(status["reasoning_efforts"], json!(["low", "high"]));

    // A turn: the model and MCP servers it runs with, and the effort asked for.
    desktop.send(json!({ "op": "user_message", "session": session, "content": "hi" }));
    let frames = desktop.until(ready_after_reply(&session));
    let text = reply(&frames, &session);
    assert!(text.contains("omp=always-ask"), "{text}");
    assert!(text.contains("mcp=files"), "{text}");
    assert!(!text.contains("web") && !text.contains("off"), "{text}");
    // Its settings besides model and effort, and one set.
    desktop.send(
        json!({ "op": "set_session_option", "session": session, "id": "compact", "value": false }),
    );
    let options = desktop.until(|f| {
        f["session"] == session.as_str()
            && f["type"] == "session_options"
            && f["options"][0]["value"] == false
    });
    assert_eq!(options.last().unwrap()["options"][0]["id"], "compact");
    // A model switch is a config option.
    desktop.send(json!({ "op": "command", "session": session, "input": "/model lynshen/two" }));
    desktop.until(|f| {
        f["session"] == session.as_str()
            && f["type"] == "model_status"
            && f["model"] == "lynshen/two"
    });

    // A form the agent asks: the phone answers it.
    phone.send(json!({ "op": "watch", "session": session }));
    phone.until(of(&session, "attended"));
    phone.send(json!({ "op": "user_message", "session": session, "content": "form please" }));
    let asked = phone.until(of(&session, "approval_request"));
    let card = asked.last().unwrap().clone();
    assert_eq!(card["name"], "mcp_elicitation");
    phone.send(json!({ "op": "approve", "session": session, "call_id": card["call_id"], "decision": "allow",
        "answers": { (card["questions"][0]["question"].as_str().unwrap()): "Approve" } }));
    let frames = phone.until(ready_after_reply(&session));
    assert!(
        reply(&frames, &session).contains("form=accept:Approve"),
        "{frames:#?}"
    );

    // Another approval mode restarts it on the same conversation.
    desktop.send(json!({ "op": "set_approval_mode", "session": session, "mode": "auto-edit" }));
    desktop.until(|f| {
        f["session"] == session.as_str() && f["type"] == "approval_mode" && f["mode"] == "auto-edit"
    });
    desktop.send(json!({ "op": "user_message", "session": session, "content": "again" }));
    let frames = desktop.until(ready_after_reply(&session));
    assert!(
        reply(&frames, &session).contains("omp=write"),
        "{frames:#?}"
    );

    // Its TUI resumes the agent's conversation in the session's mode; the
    // phone attaches to it.
    let opened = desktop
        .request(json!({ "op": "session_tui", "session": session, "cols": 100, "rows": 30 }));
    let term = opened["term"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"))
        .to_string();
    let mut output = String::new();
    desktop.until(|f| {
        output += &term_text(f, &term);
        output.contains("TUI omp --approval-mode write --resume agent-1")
    });
    let attached = phone.request(json!({ "op": "term_attach", "term": term }));
    assert_eq!(attached["type"], "term_attached", "{attached}");
    let mut seen = String::new();
    phone.until(|f| {
        seen += &term_text(f, &term);
        seen.contains("TUI omp")
    });
    desktop.send(json!({ "op": "term_input", "term": term, "data": STANDARD.encode("quit\n") }));
    desktop.until(|f| {
        f["session"] == session.as_str() && f["type"] == "surface" && f["surface"] == "gui"
    });
    fs::write(&config_path, before.to_string()).unwrap();
    std::env::remove_var("FAKE_ACP_AUTH");
}

/// Pairs a device over the LAN with `code`: its id and token.
fn pair(address: &str, code: &str) -> Value {
    use std::io::{Read, Write};
    let body = json!({ "code": code, "name": "phone" }).to_string();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "POST /api/pair HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

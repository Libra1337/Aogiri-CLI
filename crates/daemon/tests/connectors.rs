//! Chat connectors end to end: a daemon with the scripted fake model, a
//! local client (the desktop) and a fake Telegram Bot API server on
//! loopback. No real bot token or Telegram server is used.

#[path = "../../agent-core/tests/support/fake_model.rs"]
mod fake_model;

use fake_model::{requests, setup, temp_dir};
use lynshen_daemon::Store;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

const TOKEN: &str = "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1";
const OWNER: i64 = 42;
const STRANGER: i64 = 99;

/// What the fake Bot API server keeps.
#[derive(Default)]
struct Fake {
    updates: Vec<Value>,
    next_update: i64,
    next_message: i64,
    /// Every call: method and body (a multipart upload as `{filename, size}`).
    calls: Vec<(String, Value)>,
    /// Downloadable files by `file_id`.
    files: HashMap<String, Vec<u8>>,
    polls: usize,
}

#[derive(Clone)]
struct Telegram {
    url: String,
    fake: Arc<Mutex<Fake>>,
}

impl Telegram {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let fake = Arc::new(Mutex::new(Fake::default()));
        let shared = Arc::clone(&fake);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let fake = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &fake));
            }
        });
        Self { url, fake }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Fake> {
        self.fake
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn push(&self, mut update: Value) {
        let mut fake = self.lock();
        fake.next_update += 1;
        update["update_id"] = json!(fake.next_update);
        fake.updates.push(update);
    }

    /// A text message from `from` in a private chat (or a group).
    fn say(&self, from: i64, text: &str) {
        self.say_in(from, from, "private", text, json!({}));
    }

    fn say_in(&self, from: i64, chat: i64, kind: &str, text: &str, extra: Value) {
        let id = {
            let mut fake = self.lock();
            fake.next_message += 1;
            fake.next_message
        };
        let mut message = json!({
            "message_id": id,
            "from": { "id": from, "is_bot": false, "first_name": format!("User{from}"), "language_code": "zh-hans" },
            "chat": { "id": chat, "type": kind },
        });
        if !text.is_empty() {
            message["text"] = json!(text);
        }
        for (key, value) in extra.as_object().unwrap() {
            message[key] = value.clone();
        }
        self.push(json!({ "message": message }));
    }

    fn press(&self, from: i64, data: &str) -> String {
        let id = format!("cb-{from}-{}", self.lock().calls.len());
        self.push(json!({ "callback_query": {
            "id": id, "from": { "id": from }, "data": data,
            "message": { "message_id": 1, "chat": { "id": from, "type": "private" } }
        }}));
        id
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.lock().calls.clone()
    }

    /// Waits for a call after the first `after` that `matches`.
    fn wait_for(&self, after: usize, matches: impl Fn(&str, &Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some((_, body)) = self
                .calls()
                .into_iter()
                .skip(after)
                .find(|(method, body)| matches(method, body))
            {
                return body;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("no such call; calls: {:#?}", self.calls());
    }

    /// Messages sent to `chat` after the first `after` calls.
    fn texts_to(&self, after: usize, chat: i64) -> Vec<String> {
        self.calls()
            .into_iter()
            .skip(after)
            .filter(|(method, body)| method == "sendMessage" && body["chat_id"] == chat)
            .map(|(_, body)| body["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: &[u8]) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(body);
}

/// One keep-alive connection of the fake Bot API.
fn serve(stream: TcpStream, fake: &Arc<Mutex<Fake>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut stream = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, path) = (
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default().to_string(),
        );
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let lock = || fake.lock().unwrap_or_else(|poison| poison.into_inner());
        if method == "GET" {
            let file = path.rsplit('/').next().unwrap_or_default();
            let bytes = lock().files.get(file).cloned().unwrap_or_default();
            respond(&mut stream, "200 OK", &bytes);
            continue;
        }
        let Some(rest) = path.strip_prefix("/bot") else {
            respond(&mut stream, "404 Not Found", b"{}");
            continue;
        };
        let (token, call) = rest.split_once('/').unwrap_or((rest, ""));
        if token != TOKEN {
            respond(
                &mut stream,
                "401 Unauthorized",
                br#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
            );
            continue;
        }
        let json: Value = if call == "sendDocument" {
            let text = String::from_utf8_lossy(&body);
            let filename = text
                .split("filename=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default()
                .to_string();
            let chat = text
                .split("name=\"chat_id\"\r\n\r\n")
                .nth(1)
                .and_then(|rest| rest.split("\r\n").next())
                .unwrap_or_default()
                .to_string();
            json!({ "filename": filename, "size": body.len(), "chat_id": chat, "body": text })
        } else {
            serde_json::from_slice(&body).unwrap_or(Value::Null)
        };
        let result = match call {
            "getMe" => {
                json!({ "id": 777, "is_bot": true, "first_name": "Lyn", "username": "lyn_test_bot" })
            }
            "getUpdates" => {
                let offset = json["offset"].as_i64().unwrap_or(0);
                let deadline = Instant::now() + Duration::from_millis(400);
                loop {
                    let ready: Vec<Value> = {
                        let mut fake = lock();
                        fake.polls += 1;
                        fake.updates
                            .iter()
                            .filter(|update| update["update_id"].as_i64().unwrap_or(0) >= offset)
                            .cloned()
                            .collect()
                    };
                    if !ready.is_empty() || Instant::now() >= deadline {
                        break json!(ready);
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
            "getFile" => {
                let id = json["file_id"].as_str().unwrap_or_default().to_string();
                let size = lock().files.get(&id).map_or(0, Vec::len);
                json!({ "file_id": id, "file_path": format!("documents/{id}"), "file_size": size })
            }
            "sendMessage" => {
                let mut fake = lock();
                fake.next_message += 1;
                json!({ "message_id": fake.next_message, "chat": { "id": json["chat_id"] }, "text": json["text"] })
            }
            _ => json!(true),
        };
        if call != "getUpdates" {
            lock().calls.push((call.to_string(), json));
        }
        let answer = json!({ "ok": true, "result": result }).to_string();
        respond(&mut stream, "200 OK", answer.as_bytes());
    }
}

struct Client {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl Client {
    fn connect(address: &str, token: &str) -> Self {
        let (socket, _) = tungstenite::connect(format!("ws://{address}/?token={token}")).unwrap();
        if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
        }
        let mut client = Self { socket };
        client.until(|frame| frame["type"] == "hello");
        client
    }

    fn send(&mut self, frame: Value) {
        self.socket.send(Message::text(frame.to_string())).unwrap();
    }

    fn until(&mut self, mut done: impl FnMut(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                    if done(&frame) {
                        return frame;
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("socket error {error}"),
            }
        }
        panic!("timed out waiting for a frame");
    }
}

/// A daemon on its own state, with the address and its local token.
fn start_daemon() -> (String, String, PathBuf) {
    let root = temp_dir("connectors-state");
    let state = root.join("daemon");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let store = Store::open(state.clone()).unwrap();
    let token = store.token().unwrap();
    let agents = lynshen_daemon::Agents::open(root.join("agents")).unwrap();
    thread::spawn(move || lynshen_daemon::serve(listener, store, agents, None, "test", None));
    (address, token, state)
}

/// Adds the fake bot and waits until it polls.
fn add_connector(client: &mut Client, telegram: &Telegram) -> String {
    client.send(json!({
        "op": "connector_add", "id": "add", "platform": "telegram",
        "token": format!(" {TOKEN} "), "api": telegram.url, "proxy": "direct",
    }));
    let added = client.until(|frame| frame["id"] == "add");
    assert_eq!(added["type"], "connector_added", "{added}");
    assert_eq!(added["connector"]["name"], "@lyn_test_bot");
    assert_eq!(added["connector"]["link"], "https://t.me/lyn_test_bot");
    assert!(!added.to_string().contains(TOKEN));
    // Connected once the fake server answered a poll.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        client.send(json!({ "op": "connector_list", "id": "list" }));
        let list = client.until(|frame| frame["id"] == "list");
        if list["connectors"][0]["state"] == "connected" {
            break;
        }
        assert!(Instant::now() < deadline, "never connected: {list}");
        thread::sleep(Duration::from_millis(100));
    }
    added["connector"]["id"].as_str().unwrap().to_string()
}

#[test]
fn a_telegram_binding_drives_a_session_and_answers_approvals_from_chat() {
    if cfg!(windows) {
        return;
    }
    let _guard = setup();
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let telegram = Telegram::start();
    let (address, token, state) = start_daemon();
    let mut desktop = Client::connect(&address, &token);
    let connector = add_connector(&mut desktop, &telegram);

    // A stranger gets nothing while no code waits, and no model is asked.
    let asked = requests().len();
    let before = telegram.calls().len();
    telegram.say(STRANGER, "RUN: echo stranger");
    thread::sleep(Duration::from_millis(1500));
    assert!(telegram.texts_to(before, STRANGER).is_empty());
    assert_eq!(requests().len(), asked);

    // Binding: a wrong code is refused, the right one binds the owner.
    desktop.send(json!({ "op": "connector_bind_start", "id": "code", "connector": connector }));
    let code = desktop.until(|frame| frame["id"] == "code");
    let code_text = code["code"].as_str().unwrap().to_string();
    assert_eq!(
        code["link"],
        format!("https://t.me/lyn_test_bot?start={code_text}")
    );
    let before = telegram.calls().len();
    telegram.say(STRANGER, "/start WRONGCOD");
    telegram.wait_for(before, |method, body| {
        method == "sendMessage"
            && body["chat_id"] == STRANGER
            && body["text"].as_str().unwrap().contains("绑定码不对")
    });
    telegram.say(OWNER, &format!("/start {code_text}"));
    let bound = desktop.until(|frame| frame["type"] == "connector_bound");
    let device = bound["device"].as_str().unwrap().to_string();
    telegram.wait_for(before, |method, body| {
        method == "sendMessage"
            && body["chat_id"] == OWNER
            && body["text"].as_str().unwrap().contains("绑定成功")
    });
    desktop.send(json!({ "op": "device_list", "id": "devices" }));
    let devices = desktop.until(|frame| frame["id"] == "devices");
    let entry = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == device.as_str())
        .unwrap()
        .clone();
    assert_eq!(entry["kind"], "chat");
    assert_eq!(entry["platform"], "telegram");
    assert!(entry["last_seen"].is_u64());

    // The owner's messages in a group never reach a model.
    let asked = requests().len();
    telegram.say_in(OWNER, -100, "group", "RUN: echo group", json!({}));
    // Nor does a forwarded message: it is refused in plain words.
    let before = telegram.calls().len();
    telegram.say_in(
        OWNER,
        OWNER,
        "private",
        "RUN: echo forwarded",
        json!({ "forward_origin": { "type": "user" } }),
    );
    telegram.wait_for(before, |method, body| {
        method == "sendMessage" && body["text"].as_str().unwrap().contains("转发的消息")
    });
    assert_eq!(requests().len(), asked);

    // A chat session, switched to careful (manual) mode from chat.
    let before = telegram.calls().len();
    telegram.say(OWNER, "/chat");
    telegram.wait_for(before, |method, body| {
        method == "sendMessage" && body["text"].as_str().unwrap().contains("已新建聊天会话")
    });
    telegram.say(OWNER, "/mode 全自动");
    telegram.wait_for(before, |method, body| {
        method == "sendMessage"
            && body["text"]
                .as_str()
                .unwrap()
                .contains("全自动和完全访问只能在电脑")
    });
    telegram.say(OWNER, "/mode manual");
    telegram.wait_for(before, |method, body| {
        method == "sendMessage" && body["text"].as_str().unwrap().contains("已切换为「谨慎」")
    });

    // A task that needs an approval: the card comes with buttons.
    let before = telegram.calls().len();
    telegram.say(OWNER, "RUN: echo chat-ok");
    let card = telegram.wait_for(before, |method, body| {
        method == "sendMessage" && body["reply_markup"]["inline_keyboard"][0].is_array()
    });
    assert!(card["text"].as_str().unwrap().contains("echo chat-ok"));
    let buttons = card["reply_markup"]["inline_keyboard"][0]
        .as_array()
        .unwrap();
    let allow = buttons[0]["callback_data"].as_str().unwrap().to_string();
    assert!(allow.starts_with("a:") && allow.ends_with(":y"));

    // Another account's press decides nothing.
    let pressed = telegram.press(STRANGER, &allow);
    telegram.wait_for(before, |method, body| {
        method == "answerCallbackQuery" && body["callback_query_id"] == pressed.as_str()
    });
    assert!(!telegram
        .calls()
        .iter()
        .any(|(method, body)| method == "editMessageText"
            && body["text"].as_str().unwrap_or_default().contains("已允许")));

    // The owner's press runs the call; the reply follows as a new message
    // and the status message ends with the time it took.
    let pressed = telegram.press(OWNER, &allow);
    let ack = telegram.wait_for(before, |method, body| {
        method == "answerCallbackQuery" && body["callback_query_id"] == pressed.as_str()
    });
    assert_eq!(ack["text"], "已允许");
    telegram.wait_for(before, |method, body| {
        method == "sendMessage"
            && body["chat_id"] == OWNER
            && body["text"].as_str().unwrap().contains("tool said")
    });
    telegram.wait_for(before, |method, body| {
        method == "editMessageText" && body["text"].as_str().unwrap().contains("完成")
    });

    // A file in goes to the session with the next message; a file out
    // comes from the session's directory, never one that holds secrets.
    let chats = home.join(".lynshen/chats");
    fs::write(chats.join("report.md"), "# 报告\n").unwrap();
    fs::write(chats.join(".env"), "API_KEY=sk-live-0123456789abcdef").unwrap();
    telegram
        .lock()
        .files
        .insert("doc1".to_string(), b"hello file".to_vec());
    let before = telegram.calls().len();
    telegram.say_in(
        OWNER,
        OWNER,
        "private",
        "",
        json!({ "caption": "看看这个文件", "document": { "file_id": "doc1", "file_name": "notes.txt", "file_size": 10 } }),
    );
    telegram.wait_for(before, |method, body| {
        method == "sendMessage"
            && body["text"]
                .as_str()
                .unwrap()
                .contains("user said: 看看这个文件")
            && body["text"].as_str().unwrap().contains("notes.txt")
    });
    telegram.say(OWNER, "/file report.md");
    let sent = telegram.wait_for(before, |method, _| method == "sendDocument");
    assert_eq!(sent["filename"], "report.md");
    telegram.say(OWNER, "/file .env");
    telegram.wait_for(before, |method, body| {
        method == "sendMessage" && body["text"].as_str().unwrap().contains("可能含有密钥")
    });

    // Terminals stay on the desktop.
    telegram.say(OWNER, "/terminal");
    telegram.wait_for(before, |method, body| {
        method == "sendMessage" && body["text"].as_str().unwrap().contains("不能打开终端")
    });

    // Revoked on the desktop: the account's messages reach nothing.
    desktop.send(json!({ "op": "device_revoke", "id": "revoke", "device": device }));
    desktop.until(|frame| frame["id"] == "revoke");
    let asked = requests().len();
    let before = telegram.calls().len();
    telegram.say(OWNER, "RUN: echo after-revoke");
    thread::sleep(Duration::from_millis(1500));
    assert!(telegram.texts_to(before, OWNER).is_empty());
    assert_eq!(requests().len(), asked);

    // The audit log names what happened, never what was said; the token
    // stays out of every log and reply.
    let audit = fs::read_to_string(state.join("connectors.jsonl")).unwrap();
    for event in [
        "\"added\"",
        "\"bound\"",
        "\"bind_refused\"",
        "\"decide\"",
        "\"revoked\"",
        "\"command\"",
    ] {
        assert!(audit.contains(event), "{event}: {audit}");
    }
    assert!(!audit.contains("chat-ok") && !audit.contains(TOKEN));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(state.join("connectors.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    if let Ok(log) = fs::read_to_string(home.join(".lynshen/logs/lynshen.log")) {
        assert!(!log.contains(TOKEN));
    }
    for (_, body) in telegram.calls() {
        assert!(!body.to_string().contains(TOKEN));
    }
}

/// A paired phone's token (`/api/pair` with a desktop's pairing code).
fn pair_phone(address: &str, desktop: &mut Client) -> String {
    desktop.send(json!({ "op": "pair_start", "id": "pair" }));
    let code = desktop.until(|frame| frame["id"] == "pair")["code"]
        .as_str()
        .unwrap()
        .to_string();
    let body = json!({ "code": code, "name": "phone" }).to_string();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "POST /api/pair HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let json = answer.split("\r\n\r\n").nth(1).unwrap_or_default();
    serde_json::from_str::<Value>(json).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_wrong_token_or_a_remote_client_cannot_add_a_connector() {
    let _guard = setup();
    let telegram = Telegram::start();
    let (address, token, _) = start_daemon();
    let mut desktop = Client::connect(&address, &token);
    desktop.send(json!({
        "op": "connector_add", "id": "bad", "platform": "telegram",
        "token": "123456789:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB", "api": telegram.url,
    }));
    let refused = desktop.until(|frame| frame["id"] == "bad");
    assert_eq!(refused["type"], "error");
    assert!(!refused.to_string().contains("BBBBBBBBBB"), "{refused}");
    desktop.send(
        json!({ "op": "connector_add", "id": "shape", "platform": "telegram", "token": "nope" }),
    );
    assert_eq!(
        desktop.until(|frame| frame["id"] == "shape")["type"],
        "error"
    );
    desktop.send(json!({ "op": "connector_add", "id": "socks", "platform": "telegram", "token": TOKEN, "proxy": "socks5://127.0.0.1:1080" }));
    assert_eq!(
        desktop.until(|frame| frame["id"] == "socks")["type"],
        "error"
    );
    desktop.send(json!({ "op": "connector_list", "id": "list" }));
    let list = desktop.until(|frame| frame["id"] == "list");
    assert_eq!(list["connectors"], json!([]));

    // A paired phone neither sees nor adds connectors.
    let phone_token = pair_phone(&address, &mut desktop);
    let mut phone = Client::connect(&address, &phone_token);
    for (id, op) in [
        ("list", json!({ "op": "connector_list" })),
        (
            "add",
            json!({ "op": "connector_add", "platform": "telegram", "token": TOKEN, "api": telegram.url }),
        ),
    ] {
        let mut op = op;
        op["id"] = json!(id);
        phone.send(op);
        let reply = phone.until(|frame| frame["id"] == id);
        assert_eq!(reply["type"], "error", "{reply}");
    }
}

const DISCORD_TOKEN: &str = concat!(
    "MTA5ODc2NTQz",
    "MjEwOTg3NjU0.",
    "GaBcDe.",
    "f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2"
);
const DC_OWNER: &str = "4242";

/// A fake Discord: REST on one port, the gateway on another. Every REST call
/// is kept with its method and path; events queued with `dispatch` go out
/// on the gateway connection.
#[derive(Clone)]
struct Discord {
    rest: String,
    calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    events: Arc<Mutex<Vec<Value>>>,
    identify: Arc<Mutex<Option<Value>>>,
}

impl Discord {
    fn start() -> Self {
        let gateway = TcpListener::bind("127.0.0.1:0").unwrap();
        let gateway_url = format!("ws://{}", gateway.local_addr().unwrap());
        let rest = TcpListener::bind("127.0.0.1:0").unwrap();
        let fake = Self {
            rest: format!("http://{}", rest.local_addr().unwrap()),
            calls: Arc::default(),
            events: Arc::default(),
            identify: Arc::default(),
        };
        let served = fake.clone();
        thread::spawn(move || {
            for stream in rest.incoming() {
                let Ok(stream) = stream else { continue };
                let fake = served.clone();
                let gateway_url = gateway_url.clone();
                thread::spawn(move || fake.serve_rest(stream, &gateway_url));
            }
        });
        let served = fake.clone();
        thread::spawn(move || {
            for stream in gateway.incoming() {
                let Ok(stream) = stream else { continue };
                let fake = served.clone();
                thread::spawn(move || fake.serve_gateway(stream));
            }
        });
        fake
    }

    fn serve_rest(&self, stream: TcpStream, gateway: &str) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut stream = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (
                parts.next().unwrap_or_default().to_string(),
                parts.next().unwrap_or_default().to_string(),
            );
            let (mut length, mut auth) = (0, String::new());
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    return;
                }
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if name.eq_ignore_ascii_case("authorization") {
                        auth = value.trim().to_string();
                    }
                }
            }
            let mut body = vec![0u8; length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            if path.starts_with("/cdn/") {
                respond(&mut stream, "200 OK", b"discord file");
                continue;
            }
            if auth != format!("Bot {DISCORD_TOKEN}") {
                respond(
                    &mut stream,
                    "401 Unauthorized",
                    br#"{"message":"401: Unauthorized","code":0}"#,
                );
                continue;
            }
            let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let answer = match (method.as_str(), path.as_str()) {
                ("GET", "/users/@me") => json!({ "id": "777", "username": "lyn_dc_bot" }),
                ("GET", "/gateway/bot") => json!({ "url": gateway }),
                ("POST", p) if p.starts_with("/channels/") => {
                    let id = self.calls.lock().unwrap().len() + 1000;
                    json!({ "id": id.to_string() })
                }
                _ => json!({}),
            };
            self.calls.lock().unwrap().push((method, path, json));
            respond(&mut stream, "200 OK", answer.to_string().as_bytes());
        }
    }

    fn serve_gateway(&self, stream: TcpStream) {
        let mut socket = tungstenite::accept(stream).unwrap();
        let send = |socket: &mut WebSocket<TcpStream>, frame: Value| {
            socket.send(Message::text(frame.to_string())).is_ok()
        };
        send(
            &mut socket,
            json!({ "op": 10, "d": { "heartbeat_interval": 45000 } }),
        );
        let identify: Value = match socket.read() {
            Ok(Message::Text(text)) => serde_json::from_str(text.as_str()).unwrap(),
            _ => return,
        };
        *self.identify.lock().unwrap() = Some(identify);
        send(
            &mut socket,
            json!({ "op": 0, "t": "READY", "s": 1, "d": {
                "session_id": "sess-1", "resume_gateway_url": "ws://127.0.0.1:9", "user": { "id": "777", "username": "lyn_dc_bot" }
            }}),
        );
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut seq = 1;
        loop {
            let queued: Vec<Value> = std::mem::take(&mut *self.events.lock().unwrap());
            for (kind, data) in queued.iter().map(|e| (e["t"].clone(), e["d"].clone())) {
                seq += 1;
                if !send(
                    &mut socket,
                    json!({ "op": 0, "t": kind, "s": seq, "d": data }),
                ) {
                    return;
                }
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let frame: Value = serde_json::from_str(text.as_str()).unwrap_or(Value::Null);
                    if frame["op"] == 1 && !send(&mut socket, json!({ "op": 11 })) {
                        return;
                    }
                }
                Ok(Message::Close(_)) => return,
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
        }
    }

    fn dispatch(&self, kind: &str, data: Value) {
        self.events
            .lock()
            .unwrap()
            .push(json!({ "t": kind, "d": data }));
    }

    /// A DM (or, with `guild`, a server message) from `from`.
    fn say(&self, from: &str, text: &str, guild: bool) {
        let mut data = json!({
            "id": format!("m-{}", self.calls.lock().unwrap().len()),
            "channel_id": format!("dm-{from}"),
            "author": { "id": from, "username": format!("user{from}") },
            "content": text,
            "attachments": [],
        });
        if guild {
            data["guild_id"] = json!("g-1");
        }
        self.dispatch("MESSAGE_CREATE", data);
    }

    fn calls(&self) -> Vec<(String, String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn wait_for(&self, after: usize, matches: impl Fn(&str, &str, &Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some((_, _, body)) = self
                .calls()
                .into_iter()
                .skip(after)
                .find(|(method, path, body)| matches(method, path, body))
            {
                return body;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("no such call; calls: {:#?}", self.calls());
    }
}

/// A message the bot sent into `channel` whose text contains `part`.
fn posted<'a>(channel: &'a str, part: &'a str) -> impl Fn(&str, &str, &Value) -> bool + 'a {
    move |method, path, body| {
        method == "POST"
            && path == format!("/channels/{channel}/messages")
            && body["content"].as_str().unwrap_or_default().contains(part)
    }
}

#[test]
fn a_discord_dm_binding_drives_a_session_with_buttons() {
    if cfg!(windows) {
        return;
    }
    let _guard = setup();
    let discord = Discord::start();
    let (address, token, _) = start_daemon();
    let mut desktop = Client::connect(&address, &token);
    desktop.send(json!({
        "op": "connector_add", "id": "add", "platform": "discord",
        "token": DISCORD_TOKEN, "api": discord.rest, "proxy": "direct",
    }));
    let added = desktop.until(|frame| frame["id"] == "add");
    assert_eq!(added["type"], "connector_added", "{added}");
    assert_eq!(added["connector"]["name"], "@lyn_dc_bot");
    assert_eq!(
        added["connector"]["link"],
        "https://discord.com/oauth2/authorize?client_id=777&scope=bot&permissions=0"
    );
    let connector = added["connector"]["id"].as_str().unwrap().to_string();
    assert!(connector.starts_with("dc-"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        desktop.send(json!({ "op": "connector_list", "id": "list" }));
        let list = desktop.until(|frame| frame["id"] == "list");
        if list["connectors"][0]["state"] == "connected" {
            break;
        }
        assert!(Instant::now() < deadline, "never connected: {list}");
        thread::sleep(Duration::from_millis(100));
    }
    // Direct messages only.
    let identify = discord.identify.lock().unwrap().clone().unwrap();
    assert_eq!(identify["op"], 2);
    assert_eq!(identify["d"]["intents"], 4096);

    desktop.send(json!({ "op": "connector_bind_start", "id": "code", "connector": connector }));
    let code = desktop.until(|frame| frame["id"] == "code");
    assert_eq!(code["link"], Value::Null);
    let before = discord.calls().len();
    discord.say(
        DC_OWNER,
        &format!("/bind {}", code["code"].as_str().unwrap()),
        false,
    );
    desktop.until(|frame| frame["type"] == "connector_bound");
    discord.wait_for(before, posted("dm-4242", "绑定成功"));

    // A server message never reaches a model.
    let asked = requests().len();
    discord.say(DC_OWNER, "RUN: echo guild", true);
    thread::sleep(Duration::from_millis(1000));
    assert_eq!(requests().len(), asked);

    let before = discord.calls().len();
    discord.say(DC_OWNER, "/chat", false);
    discord.wait_for(before, posted("dm-4242", "已新建聊天会话"));
    discord.say(DC_OWNER, "/mode manual", false);
    discord.wait_for(before, posted("dm-4242", "已切换为「谨慎」"));
    let before = discord.calls().len();
    discord.say(DC_OWNER, "RUN: echo dc-ok", false);
    let card = discord.wait_for(before, |method, _, body| {
        method == "POST" && body["components"][0]["components"].is_array()
    });
    let allow = card["components"][0]["components"][0]["custom_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(allow.ends_with(":y"));
    discord.dispatch(
        "INTERACTION_CREATE",
        json!({ "id": "i-1", "token": "itok", "type": 3, "channel_id": "dm-4242",
                "user": { "id": DC_OWNER }, "data": { "custom_id": allow } }),
    );
    // Answered at once, then the card says how it ended and the reply follows.
    discord.wait_for(before, |method, path, body| {
        method == "POST" && path == "/interactions/i-1/itok/callback" && body["type"] == 6
    });
    discord.wait_for(before, |method, _, body| {
        method == "PATCH"
            && body["content"]
                .as_str()
                .unwrap_or_default()
                .contains("已允许")
    });
    discord.wait_for(before, posted("dm-4242", "tool said"));
    for (_, _, body) in discord.calls() {
        assert!(!body.to_string().contains(DISCORD_TOKEN));
    }
}

const QQ_APP_ID: &str = "102030405";
const QQ_SECRET: &str = "qqSecretABCDEF123456";
const QQ_OWNER: &str = "OPENID42";

/// A fake QQ open platform: the access token, OpenAPI and the gateway.
#[derive(Clone)]
struct Qq {
    api: String,
    calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    events: Arc<Mutex<Vec<Value>>>,
    identify: Arc<Mutex<Option<Value>>>,
}

impl Qq {
    fn start() -> Self {
        let gateway = TcpListener::bind("127.0.0.1:0").unwrap();
        let gateway_url = format!("ws://{}", gateway.local_addr().unwrap());
        let rest = TcpListener::bind("127.0.0.1:0").unwrap();
        let fake = Self {
            api: format!("http://{}", rest.local_addr().unwrap()),
            calls: Arc::default(),
            events: Arc::default(),
            identify: Arc::default(),
        };
        let served = fake.clone();
        thread::spawn(move || {
            for stream in rest.incoming() {
                let Ok(stream) = stream else { continue };
                let fake = served.clone();
                let gateway_url = gateway_url.clone();
                thread::spawn(move || fake.serve_rest(stream, &gateway_url));
            }
        });
        let served = fake.clone();
        thread::spawn(move || {
            for stream in gateway.incoming() {
                let Ok(stream) = stream else { continue };
                let fake = served.clone();
                thread::spawn(move || fake.serve_gateway(stream));
            }
        });
        fake
    }

    fn serve_rest(&self, stream: TcpStream, gateway: &str) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut stream = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (
                parts.next().unwrap_or_default().to_string(),
                parts.next().unwrap_or_default().to_string(),
            );
            let (mut length, mut auth) = (0, String::new());
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    return;
                }
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if name.eq_ignore_ascii_case("authorization") {
                        auth = value.trim().to_string();
                    }
                }
            }
            let mut body = vec![0u8; length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if path == "/app/getAppAccessToken" {
                let answer = if json["appId"] == QQ_APP_ID && json["clientSecret"] == QQ_SECRET {
                    json!({ "access_token": "qq-access-1", "expires_in": "7200" })
                } else {
                    json!({ "code": 100016, "message": "invalid appid or secret" })
                };
                respond(&mut stream, "200 OK", answer.to_string().as_bytes());
                continue;
            }
            if auth != "QQBot qq-access-1" {
                respond(
                    &mut stream,
                    "401 Unauthorized",
                    br#"{"code":11241,"message":"token invalid"}"#,
                );
                continue;
            }
            let answer = match (method.as_str(), path.as_str()) {
                ("GET", "/users/@me") => json!({ "id": "qqbot-1", "username": "LynShen助手" }),
                ("GET", "/gateway") => json!({ "url": gateway }),
                ("POST", p) if p.starts_with("/v2/users/") => {
                    json!({ "id": format!("sent-{}", self.calls.lock().unwrap().len()), "timestamp": "2026-10-16T10:00:00+08:00" })
                }
                _ => json!({}),
            };
            self.calls.lock().unwrap().push((method, path, json));
            respond(&mut stream, "200 OK", answer.to_string().as_bytes());
        }
    }

    fn serve_gateway(&self, stream: TcpStream) {
        let mut socket = tungstenite::accept(stream).unwrap();
        let send = |socket: &mut WebSocket<TcpStream>, frame: Value| {
            socket.send(Message::text(frame.to_string())).is_ok()
        };
        send(
            &mut socket,
            json!({ "op": 10, "d": { "heartbeat_interval": 45000 } }),
        );
        let identify: Value = match socket.read() {
            Ok(Message::Text(text)) => serde_json::from_str(text.as_str()).unwrap(),
            _ => return,
        };
        *self.identify.lock().unwrap() = Some(identify);
        send(
            &mut socket,
            json!({ "op": 0, "t": "READY", "s": 1, "d": {
                "version": 1, "session_id": "qq-sess-1", "user": { "id": "qqbot-1", "username": "LynShen助手", "bot": true }, "shard": [0, 1]
            }}),
        );
        socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut seq = 1;
        loop {
            let queued: Vec<Value> = std::mem::take(&mut *self.events.lock().unwrap());
            for (kind, data) in queued.iter().map(|e| (e["t"].clone(), e["d"].clone())) {
                seq += 1;
                if !send(
                    &mut socket,
                    json!({ "op": 0, "t": kind, "s": seq, "d": data }),
                ) {
                    return;
                }
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let frame: Value = serde_json::from_str(text.as_str()).unwrap_or(Value::Null);
                    if frame["op"] == 1 && !send(&mut socket, json!({ "op": 11 })) {
                        return;
                    }
                }
                Ok(Message::Close(_)) => return,
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
        }
    }

    /// A single-chat message from `from`, with its message id.
    fn say(&self, from: &str, id: &str, text: &str) {
        self.events
            .lock()
            .unwrap()
            .push(json!({ "t": "C2C_MESSAGE_CREATE", "d": {
                "id": id, "content": text, "author": { "user_openid": from }, "attachments": []
            }}));
    }

    fn calls(&self) -> Vec<(String, String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    /// A message the bot sent to `user` whose text contains `part`.
    fn wait_for_text(&self, after: usize, user: &str, part: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        let path = format!("/v2/users/{user}/messages");
        while Instant::now() < deadline {
            if let Some((_, _, body)) =
                self.calls()
                    .into_iter()
                    .skip(after)
                    .find(|(method, p, body)| {
                        let text = body["markdown"]["content"]
                            .as_str()
                            .or(body["content"].as_str())
                            .unwrap_or_default();
                        method == "POST" && *p == path && text.contains(part)
                    })
            {
                return body;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("no message with {part}; calls: {:#?}", self.calls());
    }
}

#[test]
fn a_qq_single_chat_binding_answers_approvals_as_text() {
    if cfg!(windows) {
        return;
    }
    let _guard = setup();
    let qq = Qq::start();
    let (address, token, state) = start_daemon();
    let mut desktop = Client::connect(&address, &token);
    // A wrong secret is refused before anything is saved.
    desktop.send(json!({
        "op": "connector_add", "id": "bad", "platform": "qq",
        "app_id": QQ_APP_ID, "secret": "wrongSecret123456", "api": qq.api, "proxy": "direct",
    }));
    assert_eq!(desktop.until(|frame| frame["id"] == "bad")["type"], "error");
    desktop.send(json!({
        "op": "connector_add", "id": "add", "platform": "qq",
        "app_id": QQ_APP_ID, "secret": QQ_SECRET, "api": qq.api, "proxy": "direct",
    }));
    let added = desktop.until(|frame| frame["id"] == "add");
    assert_eq!(added["type"], "connector_added", "{added}");
    assert_eq!(added["connector"]["name"], "@LynShen助手");
    assert_eq!(added["connector"]["link"], Value::Null);
    let connector = added["connector"]["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        desktop.send(json!({ "op": "connector_list", "id": "list" }));
        let list = desktop.until(|frame| frame["id"] == "list");
        if list["connectors"][0]["state"] == "connected" {
            break;
        }
        assert!(Instant::now() < deadline, "never connected: {list}");
        thread::sleep(Duration::from_millis(100));
    }
    let identify = qq.identify.lock().unwrap().clone().unwrap();
    assert_eq!(identify["d"]["intents"], 1 << 25);
    assert_eq!(identify["d"]["token"], "QQBot qq-access-1");
    assert_eq!(identify["d"]["shard"], json!([0, 1]));

    desktop.send(json!({ "op": "connector_bind_start", "id": "code", "connector": connector }));
    let code = desktop.until(|frame| frame["id"] == "code");
    let bind = format!("/bind {}", code["code"].as_str().unwrap());
    let before = qq.calls().len();
    // QQ may push one message twice: it binds and answers once.
    qq.say(QQ_OWNER, "m-1", &bind);
    qq.say(QQ_OWNER, "m-1", &bind);
    let welcome = qq.wait_for_text(before, QQ_OWNER, "绑定成功");
    // A reply to the user's message, as Markdown.
    assert_eq!(welcome["msg_type"], 2);
    assert_eq!(welcome["msg_id"], "m-1");
    thread::sleep(Duration::from_millis(800));
    let welcomes = qq
        .calls()
        .iter()
        .filter(|(_, _, body)| body.to_string().contains("绑定成功"))
        .count();
    assert_eq!(welcomes, 1);

    let before = qq.calls().len();
    qq.say(QQ_OWNER, "m-2", "/chat");
    qq.wait_for_text(before, QQ_OWNER, "已新建聊天会话");
    qq.say(QQ_OWNER, "m-3", "/mode manual");
    qq.wait_for_text(before, QQ_OWNER, "已切换为「谨慎」");
    let before = qq.calls().len();
    qq.say(QQ_OWNER, "m-4", "RUN: echo qq-ok");
    // No buttons on QQ: the card names its id for a text answer.
    let card = qq.wait_for_text(before, QQ_OWNER, "回复「允许 ");
    let text = card["markdown"]["content"].as_str().unwrap().to_string();
    let id = text
        .split("回复「允许 ")
        .nth(1)
        .and_then(|rest| rest.split('」').next())
        .unwrap()
        .to_string();
    qq.say(QQ_OWNER, "m-5", &format!("允许 {id}"));
    qq.wait_for_text(before, QQ_OWNER, "已允许");
    qq.wait_for_text(before, QQ_OWNER, "tool said");
    // No status message: QQ cannot edit one.
    assert!(!qq
        .calls()
        .iter()
        .skip(before)
        .any(|(_, _, body)| body.to_string().contains("思考中")));
    for (_, _, body) in qq.calls() {
        assert!(!body.to_string().contains(QQ_SECRET));
    }
    let saved = fs::read_to_string(state.join("connectors.json")).unwrap();
    assert!(saved.contains(QQ_APP_ID));
}

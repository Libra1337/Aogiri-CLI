//! QQ official bot (docs/chat-connectors.md, "QQ"): single chats (C2C)
//! over the WebSocket gateway, which QQ recommends for a bot on one
//! computer (no public address), and the OpenAPI to send. The bot signs in
//! with its AppID and AppSecret for an access token that lasts about two
//! hours. QQ has no message edits and, without an application, no custom
//! buttons: a turn shows no status message and approvals are answered as
//! text (`允许 <id>`).

use super::{
    chat::{Attachment, Button, Event, Inbound, Incoming, Limits},
    http_agent, net, proxy_for, render, Connector, Fault,
};
use crate::hub::{lock, Hub};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::Message;

pub(crate) const ORIGIN: &str = "https://api.bot.qq.com";
/// GROUP_AND_C2C_EVENT: single chats (and group @ messages, which are
/// ignored).
const INTENTS: u64 = 1 << 25;
const TEXT: usize = 3000;
const MAX_IN: u64 = 20 * 1024 * 1024;
/// A reply to the user's message (passive) is possible this long after it,
/// and this many times; after that a message is an active one.
const PASSIVE: Duration = Duration::from_secs(55 * 60);
const PASSIVE_REPLIES: u32 = 4;
const READ: Duration = Duration::from_millis(250);
/// Message ids seen lately: QQ may push one message more than once.
const SEEN: usize = 256;

#[derive(Debug)]
pub(crate) struct Bot {
    pub id: String,
    pub username: String,
}

enum Failure {
    Http(u16, String),
    Net(String),
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Failure::Http(status, message) => format!("QQ answered {status}: {message}"),
            Failure::Net(error) => format!("cannot reach QQ: {error}"),
        }
    }
}

/// An AppID (digits) and an AppSecret as the QQ open platform shows them.
pub(crate) fn check_credentials(app_id: &str, secret: &str) -> Result<(), String> {
    if app_id.is_empty() || !app_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("the QQ AppID is a number (QQ 开放平台 → 开发设置)".to_string());
    }
    if secret.len() < 8 || secret.contains(char::is_whitespace) {
        return Err("this is not a QQ AppSecret (QQ 开放平台 → 开发设置 → AppSecret)".to_string());
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct Resume {
    session: Option<String>,
    seq: Option<u64>,
}

/// The user's message a reply can answer passively: its id, when it came,
/// replies sent to it.
type Passive = (String, Instant, u32);

#[derive(Clone)]
pub(crate) struct Api {
    origin: String,
    app_id: String,
    secret: String,
    agent: ureq::Agent,
    proxy: Option<String>,
    /// The access token and when it stops working.
    access: Arc<Mutex<Option<(String, Instant)>>>,
    /// Per user (openid): the message a reply can answer passively.
    passive: Arc<Mutex<HashMap<String, Passive>>>,
}

impl Api {
    pub(crate) fn new(connector: &Connector) -> Result<Self, String> {
        let origin = connector.api_origin();
        let (proxy, _) = proxy_for(connector.proxy.as_deref(), &origin);
        Ok(Self {
            agent: http_agent(proxy.as_deref(), Duration::from_secs(60))?,
            origin,
            app_id: connector.app_id.clone().unwrap_or_default(),
            secret: connector.token.clone(),
            proxy: connector.proxy.clone(),
            access: Arc::default(),
            passive: Arc::default(),
        })
    }

    /// Neither the secret nor the access token leaves in an error or a log.
    fn scrub(&self, text: &str) -> String {
        let mut text = text.to_string();
        if !self.secret.is_empty() {
            text = text.replace(&self.secret, "<secret>");
        }
        if let Some((token, _)) = lock(&self.access).as_ref() {
            text = text.replace(token.as_str(), "<token>");
        }
        text
    }

    fn answer(&self, response: Result<ureq::Response, ureq::Error>) -> Result<Value, Failure> {
        let (status, text) = match response {
            Ok(response) => {
                let status = response.status();
                (status, response.into_string().unwrap_or_default())
            }
            Err(ureq::Error::Status(status, response)) => {
                (status, response.into_string().unwrap_or_default())
            }
            Err(ureq::Error::Transport(error)) => {
                return Err(Failure::Net(self.scrub(&error.to_string())))
            }
        };
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        // An error comes with `code` and `message`, sometimes under 200.
        let failed = !(200..300).contains(&status)
            || value["code"].as_i64().is_some_and(|code| code != 0) && value["message"].is_string();
        if !failed {
            return Ok(value);
        }
        let message = value["message"]
            .as_str()
            .map_or_else(|| format!("HTTP {status}"), |message| self.scrub(message));
        let code = value["code"]
            .as_i64()
            .map(|code| format!(" ({code})"))
            .unwrap_or_default();
        Err(Failure::Http(status, format!("{message}{code}")))
    }

    /// The access token, fetched again a minute before it runs out. The
    /// lock is not held over the request: `scrub` takes it too.
    fn access_token(&self) -> Result<String, Failure> {
        if let Some((token, until)) = lock(&self.access).as_ref() {
            if Instant::now() + Duration::from_secs(60) < *until {
                return Ok(token.clone());
            }
        }
        let body = json!({ "appId": self.app_id, "clientSecret": self.secret });
        let response = self
            .agent
            .post(&format!("{}/app/getAppAccessToken", self.origin))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let reply = self.answer(response)?;
        let token = reply["access_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                Failure::Http(
                    401,
                    "QQ gave no access token for this AppID and AppSecret".to_string(),
                )
            })?
            .to_string();
        // `expires_in` comes as a string ("7200") or a number.
        let seconds = reply["expires_in"]
            .as_u64()
            .or_else(|| reply["expires_in"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(7200);
        *lock(&self.access) = Some((token.clone(), Instant::now() + Duration::from_secs(seconds)));
        Ok(token)
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
        let token = self.access_token()?;
        let request = self
            .agent
            .request(method, &format!("{}{path}", self.origin))
            .set("Authorization", &format!("QQBot {token}"))
            .set("X-Union-Appid", &self.app_id);
        let response = match body {
            Some(body) => request
                .set("Content-Type", "application/json")
                .send_string(&body.to_string()),
            None => request.call(),
        };
        self.answer(response)
    }

    /// The bot behind the AppID and AppSecret: the token proves them, and
    /// `GET /users/@me` names the bot when QQ answers it.
    pub(crate) fn me(&self) -> Result<Bot, String> {
        self.access_token().map_err(|failure| match failure {
            Failure::Http(..) => {
                "QQ does not accept this AppID and AppSecret: copy them again from QQ 开放平台 → 开发设置".to_string()
            }
            failure => failure.message(),
        })?;
        let username = self
            .call("GET", "/users/@me", None)
            .ok()
            .and_then(|me| me["username"].as_str().map(str::to_string))
            .unwrap_or_default();
        Ok(Bot {
            id: self.app_id.clone(),
            username,
        })
    }

    fn gateway(&self) -> Result<String, Failure> {
        let reply = self.call("GET", "/gateway", None)?;
        reply["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Failure::Http(200, "no gateway url".to_string()))
    }

    /// A message from `openid` that replies can answer passively.
    fn note_message(&self, openid: &str, id: &str) {
        let mut passive = lock(&self.passive);
        passive.retain(|_, (_, at, _)| at.elapsed() < PASSIVE);
        passive.insert(openid.to_string(), (id.to_string(), Instant::now(), 0));
    }

    /// The message to answer passively, with the next `msg_seq`, while one
    /// is fresh and has replies left.
    fn passive_reply(&self, openid: &str) -> Option<(String, u32)> {
        let mut passive = lock(&self.passive);
        let (id, at, sent) = passive.get_mut(openid)?;
        if at.elapsed() >= PASSIVE || *sent >= PASSIVE_REPLIES {
            return None;
        }
        *sent += 1;
        Some((id.clone(), *sent))
    }
}

impl super::chat::Api for Api {
    fn limits(&self) -> Limits {
        Limits {
            text: TEXT,
            file_in: MAX_IN,
            // Sending a file from this computer needs QQ's chunked upload;
            // not yet.
            file_out: 0,
            buttons: false,
            edits: false,
        }
    }

    /// Markdown (`msg_type` 2), as text when QQ refuses it; a passive reply
    /// while the user's message allows one, else an active message.
    fn send(&self, chat: &str, text: &str, _buttons: &[Button]) -> Result<String, String> {
        let path = format!("/v2/users/{chat}/messages");
        let passive = self.passive_reply(chat);
        let markdown =
            json!({ "msg_type": 2, "markdown": { "content": render::clip(text, TEXT) } });
        let plain = json!({ "msg_type": 0, "content": render::clip(text, TEXT) });
        let mut last = String::new();
        for (index, body) in [markdown, plain].into_iter().enumerate() {
            let mut body = body;
            if let Some((id, seq)) = &passive {
                body["msg_id"] = json!(id);
                // A second attempt on the same message needs its own seq.
                body["msg_seq"] = json!(seq + 10 * index as u32);
            }
            match self.call("POST", &path, Some(&body)) {
                Ok(reply) => {
                    return Ok(reply["id"].as_str().unwrap_or_default().to_string());
                }
                Err(failure) => last = failure.message(),
            }
        }
        // The passive window may have closed: once more, as an active message.
        if passive.is_some() {
            let body = json!({ "msg_type": 0, "content": render::clip(text, TEXT) });
            return self
                .call("POST", &path, Some(&body))
                .map(|reply| reply["id"].as_str().unwrap_or_default().to_string())
                .map_err(|failure| failure.message());
        }
        Err(last)
    }

    fn edit(
        &self,
        _chat: &str,
        _message: &str,
        _text: &str,
        _buttons: &[Button],
    ) -> Result<(), String> {
        Err("QQ messages cannot be edited".to_string())
    }

    fn send_file(
        &self,
        _chat: &str,
        _name: &str,
        _bytes: &[u8],
        _caption: &str,
    ) -> Result<(), String> {
        Err("this bot cannot send files on QQ yet".to_string())
    }

    fn download(&self, file: &Attachment, limit: u64) -> Result<Vec<u8>, String> {
        let limit = limit.min(MAX_IN);
        if file.size.is_some_and(|size| size > limit) {
            return Err(format!("larger than {} MB", limit >> 20));
        }
        let url = file.url.as_deref().ok_or("the attachment has no address")?;
        let url = if url.starts_with("//") {
            format!("https:{url}")
        } else {
            url.to_string()
        };
        let response = self
            .agent
            .get(&url)
            .call()
            .map_err(|error| self.scrub(&error.to_string()))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > limit {
            return Err(format!("larger than {} MB", limit >> 20));
        }
        Ok(bytes)
    }

    fn ack(&self, _press: &str, _text: &str) {}
}

/// The gateway connection until it fails or `stop` is set.
pub(crate) fn receive(
    _hub: &Hub,
    _id: &str,
    api: &Api,
    stop: &AtomicBool,
    events: &Sender<Event>,
    connected: &dyn Fn(),
    resume: &mut Resume,
) -> Result<(), Fault> {
    let rejected = |failure: Failure| match failure {
        Failure::Http(status, message) if (400..500).contains(&status) && status != 429 => {
            Fault::Rejected(format!(
                "QQ refused the bot's AppID and AppSecret ({message}): enter them again"
            ))
        }
        failure => Fault::Retry(failure.message()),
    };
    let token = api.access_token().map_err(rejected)?;
    let base = api
        .gateway()
        .map_err(|failure| Fault::Retry(failure.message()))?;
    let (proxy, _) = proxy_for(
        api.proxy.as_deref(),
        &base.replacen("wss://", "https://", 1),
    );
    let mut socket =
        net::connect(&base, proxy.as_deref()).map_err(|error| Fault::Retry(api.scrub(&error)))?;
    net::set_read_timeout(&socket, Duration::from_secs(15)).map_err(Fault::Retry)?;
    let hello = loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                break serde_json::from_str::<Value>(text.as_str()).unwrap_or(Value::Null)
            }
            Ok(Message::Close(frame)) => {
                let code = frame.map(|frame| u16::from(frame.code)).unwrap_or(1000);
                return Err(closed(code, resume));
            }
            Ok(_) => {}
            Err(error) => return Err(Fault::Retry(error.to_string())),
        }
    };
    if hello["op"] != 10 {
        return Err(Fault::Retry(format!(
            "expected Hello from the QQ gateway, got op {}",
            hello["op"]
        )));
    }
    let interval =
        Duration::from_millis(hello["d"]["heartbeat_interval"].as_u64().unwrap_or(45_000));
    net::set_read_timeout(&socket, READ).map_err(Fault::Retry)?;
    let start = match (&resume.session, resume.seq) {
        (Some(session), Some(seq)) => json!({ "op": 6, "d": {
            "token": format!("QQBot {token}"), "session_id": session, "seq": seq,
        }}),
        _ => json!({ "op": 2, "d": {
            "token": format!("QQBot {token}"),
            "intents": INTENTS,
            "shard": [0, 1],
            "properties": { "$os": std::env::consts::OS, "$browser": "lynshen", "$device": "lynshen" },
        }}),
    };
    send(&mut socket, &start)?;
    let mut next_beat = Instant::now() + interval;
    let mut acked = true;
    let mut seen: VecDeque<String> = VecDeque::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = socket.close(None);
            let _ = socket.flush();
            return Ok(());
        }
        if Instant::now() >= next_beat {
            if !acked {
                return Err(Fault::Retry(
                    "the QQ gateway stopped answering heartbeats".to_string(),
                ));
            }
            send(&mut socket, &json!({ "op": 1, "d": resume.seq }))?;
            acked = false;
            next_beat = Instant::now() + interval;
        }
        let frame = match socket.read() {
            Ok(Message::Text(text)) => {
                serde_json::from_str::<Value>(text.as_str()).unwrap_or(Value::Null)
            }
            Ok(Message::Close(frame)) => {
                let code = frame.map(|frame| u16::from(frame.code)).unwrap_or(1000);
                return Err(closed(code, resume));
            }
            Ok(_) => continue,
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(Fault::Retry(api.scrub(&error.to_string()))),
        };
        match frame["op"].as_u64() {
            Some(0) => {
                if let Some(seq) = frame["s"].as_u64() {
                    resume.seq = Some(seq);
                }
                let data = &frame["d"];
                match frame["t"].as_str().unwrap_or_default() {
                    "READY" => {
                        resume.session = data["session_id"].as_str().map(str::to_string);
                        connected();
                    }
                    "RESUMED" => connected(),
                    "C2C_MESSAGE_CREATE" => {
                        let Some(incoming) = parse_c2c(data) else {
                            continue;
                        };
                        let id = data["id"].as_str().unwrap_or_default().to_string();
                        if seen.contains(&id) {
                            continue;
                        }
                        seen.push_back(id.clone());
                        if seen.len() > SEEN {
                            seen.pop_front();
                        }
                        api.note_message(&incoming.peer, &id);
                        if events
                            .send(Event::Inbound(Inbound::Message(incoming)))
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
            Some(1) => send(&mut socket, &json!({ "op": 1, "d": resume.seq }))?,
            Some(7) => {
                return Err(Fault::Retry(
                    "the QQ gateway asked to reconnect".to_string(),
                ))
            }
            Some(9) => {
                *resume = Resume::default();
                thread::sleep(Duration::from_secs(2));
                return Err(Fault::Retry("the QQ gateway ended the session".to_string()));
            }
            Some(11) => acked = true,
            _ => {}
        }
    }
}

fn send(socket: &mut net::Socket, frame: &Value) -> Result<(), Fault> {
    socket
        .send(Message::text(frame.to_string()))
        .map_err(|error| Fault::Retry(error.to_string()))
}

/// What a close code from the QQ gateway means.
fn closed(code: u16, resume: &mut Resume) -> Fault {
    match code {
        4914 => Fault::Rejected(
            "QQ took this bot offline; only its sandbox may connect (QQ 开放平台)".to_string(),
        ),
        4915 => Fault::Rejected("QQ banned this bot; ask QQ to lift the ban first".to_string()),
        4013 | 4014 => Fault::Rejected(format!(
            "QQ refused the bot's events ({code}); the bot needs single-chat (C2C) messages"
        )),
        // The connection expired: resume it.
        4009 => Fault::Retry(format!("the QQ gateway closed ({code})")),
        _ => {
            *resume = Resume::default();
            Fault::Retry(format!("the QQ gateway closed ({code})"))
        }
    }
}

/// A single-chat message as the chat side sees it.
pub(crate) fn parse_c2c(data: &Value) -> Option<Incoming> {
    let author = &data["author"];
    let peer = author["user_openid"]
        .as_str()
        .or(author["id"].as_str())?
        .to_string();
    let files = data["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| Attachment {
            id: file["id"].as_str().unwrap_or_default().to_string(),
            name: file["filename"].as_str().unwrap_or("file").to_string(),
            size: file["size"].as_u64(),
            url: file["url"].as_str().map(str::to_string),
        })
        .collect();
    Some(Incoming {
        name: format!("QQ {}", peer.chars().take(8).collect::<String>()),
        chat: peer.clone(),
        peer,
        private: true,
        from_bot: false,
        lang: String::new(),
        text: data["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string(),
        forwarded: false,
        reply_to: data["message_reference"]["message_id"]
            .as_str()
            .map(str::to_string),
        files,
        unsupported: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_checked_for_their_shape() {
        assert!(check_credentials("102030405", "aBcDeFgHiJkLmNoP").is_ok());
        assert!(check_credentials("", "aBcDeFgHiJkLmNoP").is_err());
        assert!(check_credentials("app", "aBcDeFgHiJkLmNoP").is_err());
        assert!(check_credentials("102030405", "short").is_err());
        assert!(check_credentials("102030405", "has a space in it").is_err());
    }

    #[test]
    fn single_chat_messages_become_messages() {
        let data = json!({
            "id": "ROBOT1.0_x", "content": " /bind ABCD2345 ",
            "author": { "user_openid": "E4F3A2B1C0D9" },
            "attachments": [{ "filename": "a.png", "size": 9, "url": "//multimedia.nt.qq.com.cn/x" }]
        });
        let incoming = parse_c2c(&data).unwrap();
        assert_eq!(incoming.peer, "E4F3A2B1C0D9");
        assert_eq!(incoming.chat, "E4F3A2B1C0D9");
        assert_eq!(incoming.text, "/bind ABCD2345");
        assert!(incoming.private);
        assert_eq!(
            incoming.files[0].url.as_deref(),
            Some("//multimedia.nt.qq.com.cn/x")
        );
        assert!(parse_c2c(&json!({ "content": "x", "author": {} })).is_none());
    }

    #[test]
    fn close_codes_say_whether_to_try_again() {
        let mut resume = Resume {
            session: Some("s".to_string()),
            seq: Some(3),
        };
        assert!(matches!(closed(4009, &mut resume), Fault::Retry(_)));
        assert!(resume.session.is_some());
        assert!(matches!(closed(4914, &mut resume), Fault::Rejected(_)));
        assert!(matches!(closed(4915, &mut resume), Fault::Rejected(_)));
        assert!(matches!(closed(4006, &mut resume), Fault::Retry(_)));
        assert!(resume.session.is_none());
    }

    #[test]
    fn replies_answer_the_users_message_while_qq_allows() {
        let connector = Connector {
            id: "qq-1".to_string(),
            platform: "qq".to_string(),
            name: String::new(),
            username: String::new(),
            bot_id: "1".to_string(),
            app_id: Some("1".to_string()),
            enabled: true,
            proxy: Some("direct".to_string()),
            api: Some("http://127.0.0.1:9".to_string()),
            token: "aBcDeFgHiJkLmNoP".to_string(),
            sealed: String::new(),
            settings: Default::default(),
            created_at: 0,
        };
        let api = Api::new(&connector).unwrap();
        assert_eq!(api.passive_reply("u1"), None);
        api.note_message("u1", "m1");
        for seq in 1..=PASSIVE_REPLIES {
            assert_eq!(api.passive_reply("u1"), Some(("m1".to_string(), seq)));
        }
        assert_eq!(api.passive_reply("u1"), None);
        // The secret never shows in an error.
        assert!(!api
            .scrub("bad secret aBcDeFgHiJkLmNoP")
            .contains("aBcDeFgHiJkLmNoP"));
    }
}

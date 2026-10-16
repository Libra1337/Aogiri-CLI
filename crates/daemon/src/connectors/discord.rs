//! Discord (docs/chat-connectors.md, "Discord"): the Gateway over an
//! outbound WebSocket to receive, REST to send, through the connector's
//! proxy. Intents: direct messages only; a bot gets the content of DMs
//! without the privileged message content intent.

use super::{
    chat::{Attachment, Button, Event, Inbound, Incoming, Limits, Press},
    http_agent, net, proxy_for, render, Connector, Fault,
};
use crate::{hub::Hub, store::random_hex};
use serde_json::{json, Value};
use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::Message;

pub(crate) const ORIGIN: &str = "https://discord.com/api/v10";
const GATEWAY_QUERY: &str = "?v=10&encoding=json";
/// DIRECT_MESSAGES. Button presses (INTERACTION_CREATE) need no intent.
const INTENTS: u64 = 1 << 12;
/// Characters per message: Discord takes 2000.
const TEXT: usize = 1900;
/// The largest file a bot uploads or takes in.
const MAX_FILE: u64 = 20 * 1024 * 1024;
/// How long a read on the gateway waits, so the heartbeat keeps its time.
const READ: Duration = Duration::from_millis(250);

/// The bot behind a token.
#[derive(Debug)]
pub(crate) struct Bot {
    pub id: String,
    pub username: String,
}

/// Why a REST call failed.
enum Failure {
    Http(u16, String),
    Net(String),
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Failure::Http(status, message) => format!("Discord answered {status}: {message}"),
            Failure::Net(error) => format!("cannot reach Discord: {error}"),
        }
    }
}

/// A bot token as the Developer Portal shows it: three dot-separated parts.
pub(crate) fn check_token(token: &str) -> Result<(), String> {
    let parts: Vec<&str> = token.split('.').collect();
    let valid = parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        && token.len() >= 50;
    if valid {
        Ok(())
    } else {
        Err("this is not a Discord bot token (Developer Portal → Bot → Reset Token gives one in three parts joined by dots)".to_string())
    }
}

/// Where the gateway session can continue after a dropped connection.
#[derive(Default)]
pub(crate) struct Resume {
    session: Option<String>,
    url: Option<String>,
    seq: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct Api {
    origin: String,
    token: String,
    agent: ureq::Agent,
    /// The connector's proxy setting, for the gateway's WebSocket.
    proxy: Option<String>,
}

impl Api {
    pub(crate) fn new(connector: &Connector) -> Result<Self, String> {
        let origin = connector.api_origin();
        let (proxy, _) = proxy_for(connector.proxy.as_deref(), &origin);
        Ok(Self {
            agent: http_agent(proxy.as_deref(), Duration::from_secs(60))?,
            origin,
            token: connector.token.clone(),
            proxy: connector.proxy.clone(),
        })
    }

    fn scrub(&self, text: &str) -> String {
        if self.token.is_empty() {
            return text.to_string();
        }
        text.replace(&self.token, "<token>")
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
        if (200..300).contains(&status) {
            return Ok(value);
        }
        let message = value["message"]
            .as_str()
            .map_or_else(|| format!("HTTP {status}"), |message| self.scrub(message));
        Err(Failure::Http(status, message))
    }

    /// A REST call; on 429 it waits as Discord asks (at most 30 s) and
    /// tries once more.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
        let once = || {
            let request = self
                .agent
                .request(method, &format!("{}{path}", self.origin))
                .set("Authorization", &format!("Bot {}", self.token))
                .set("User-Agent", "DiscordBot (https://lynshen.org, 1) LynShen");
            let response = match body {
                Some(body) => request
                    .set("Content-Type", "application/json")
                    .send_string(&body.to_string()),
                None => request.call(),
            };
            match response {
                Err(ureq::Error::Status(429, response)) => {
                    let text = response.into_string().unwrap_or_default();
                    let wait = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|value| value["retry_after"].as_f64())
                        .unwrap_or(1.0);
                    Err(Some(wait))
                }
                other => Ok(self.answer(other)),
            }
        };
        match once() {
            Ok(result) => result,
            Err(wait) => {
                thread::sleep(Duration::from_secs_f64(
                    wait.unwrap_or(1.0).clamp(0.1, 30.0),
                ));
                match once() {
                    Ok(result) => result,
                    Err(_) => Err(Failure::Http(429, "too many requests".to_string())),
                }
            }
        }
    }

    /// The bot behind the token (`GET /users/@me`).
    pub(crate) fn me(&self) -> Result<Bot, String> {
        let me = self.call("GET", "/users/@me", None).map_err(|failure| match failure {
            Failure::Http(401, _) => {
                "Discord does not know this token: reset it in the Developer Portal and copy it again".to_string()
            }
            failure => failure.message(),
        })?;
        Ok(Bot {
            id: me["id"]
                .as_str()
                .ok_or("Discord did not name the bot")?
                .to_string(),
            username: me["username"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// The gateway's address (`GET /gateway/bot`).
    fn gateway(&self) -> Result<String, Failure> {
        let reply = self.call("GET", "/gateway/bot", None)?;
        reply["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Failure::Http(200, "no gateway url".to_string()))
    }

    /// Answers a button press at once, as Discord wants within 3 seconds
    /// (DEFERRED_UPDATE_MESSAGE: the message changes when the chat side
    /// edits it).
    fn acknowledge(&self, id: &str, token: &str) {
        let path = format!("/interactions/{id}/{token}/callback");
        if let Err(failure) = self.call("POST", &path, Some(&json!({ "type": 6 }))) {
            lynshen_agent_core::log_warn!(
                "daemon",
                "discord press not answered",
                error = failure.message()
            );
        }
    }

    fn components(buttons: &[Button]) -> Value {
        if buttons.is_empty() {
            return json!([]);
        }
        let row: Vec<Value> = buttons
            .iter()
            .map(|button| {
                // Allow is the primary action, deny the dangerous one.
                let style = if button.data.ends_with(":y") {
                    1
                } else if button.data.ends_with(":n") {
                    4
                } else {
                    2
                };
                json!({ "type": 2, "style": style, "label": button.label, "custom_id": button.data })
            })
            .collect();
        json!([{ "type": 1, "components": row }])
    }
}

impl super::chat::Api for Api {
    fn limits(&self) -> Limits {
        Limits {
            text: TEXT,
            file_in: MAX_FILE,
            file_out: MAX_FILE,
            buttons: true,
            edits: true,
        }
    }

    fn send(&self, chat: &str, text: &str, buttons: &[Button]) -> Result<String, String> {
        let body = json!({
            "content": render::clip(text, 2000),
            "components": Self::components(buttons),
            // Never ping anyone from a reply.
            "allowed_mentions": { "parse": [] },
        });
        let message = self
            .call("POST", &format!("/channels/{chat}/messages"), Some(&body))
            .map_err(|failure| failure.message())?;
        message["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "Discord did not return the message".to_string())
    }

    fn edit(
        &self,
        chat: &str,
        message: &str,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), String> {
        let body = json!({
            "content": render::clip(text, 2000),
            "components": Self::components(buttons),
            "allowed_mentions": { "parse": [] },
        });
        self.call(
            "PATCH",
            &format!("/channels/{chat}/messages/{message}"),
            Some(&body),
        )
        .map(|_| ())
        .map_err(|failure| failure.message())
    }

    fn send_file(&self, chat: &str, name: &str, bytes: &[u8], caption: &str) -> Result<(), String> {
        let boundary = format!("lynshen-{}", random_hex(12).map_err(|e| e.to_string())?);
        let file_name: String = name
            .chars()
            .map(|c| if c == '"' || c.is_control() { '_' } else { c })
            .collect();
        let payload = json!({
            "content": render::clip(caption, 1900),
            "attachments": [{ "id": 0, "filename": file_name }],
            "allowed_mentions": { "parse": [] },
        });
        let mut body = Vec::with_capacity(bytes.len() + 512);
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n{payload}\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files[0]\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let response = self
            .agent
            .post(&format!("{}/channels/{chat}/messages", self.origin))
            .set("Authorization", &format!("Bot {}", self.token))
            .set(
                "Content-Type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send_bytes(&body);
        self.answer(response)
            .map(|_| ())
            .map_err(|failure| failure.message())
    }

    fn download(&self, file: &Attachment, limit: u64) -> Result<Vec<u8>, String> {
        let limit = limit.min(MAX_FILE);
        if file.size.is_some_and(|size| size > limit) {
            return Err(format!("larger than {} MB", limit >> 20));
        }
        let url = file.url.as_deref().ok_or("the attachment has no address")?;
        let response = self
            .agent
            .get(url)
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

    /// Presses were answered when they arrived (`receive`).
    fn ack(&self, _press: &str, _text: &str) {}
}

/// The gateway connection until it fails or `stop` is set: Hello, Identify
/// (or Resume), heartbeats, and the events that matter here.
pub(crate) fn receive(
    _hub: &Hub,
    _id: &str,
    api: &Api,
    stop: &AtomicBool,
    events: &Sender<Event>,
    connected: &dyn Fn(),
    resume: &mut Resume,
) -> Result<(), Fault> {
    let base = match (&resume.session, &resume.url) {
        (Some(_), Some(url)) => url.clone(),
        _ => {
            resume.session = None;
            api.gateway().map_err(|failure| match failure {
                Failure::Http(401, _) => Fault::Rejected(
                    "Discord no longer accepts this bot token: reset it in the Developer Portal"
                        .to_string(),
                ),
                failure => Fault::Retry(failure.message()),
            })?
        }
    };
    let url = format!("{}/{GATEWAY_QUERY}", base.trim_end_matches('/'));
    let (proxy, _) = proxy_for(api.proxy.as_deref(), &url.replacen("wss://", "https://", 1));
    let mut socket =
        net::connect(&url, proxy.as_deref()).map_err(|error| Fault::Retry(api.scrub(&error)))?;
    let hello = read_frame(&mut socket, Duration::from_secs(15))?;
    if hello["op"] != 10 {
        return Err(Fault::Retry(format!(
            "expected Hello from the gateway, got op {}",
            hello["op"]
        )));
    }
    let interval =
        Duration::from_millis(hello["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250));
    net::set_read_timeout(&socket, READ).map_err(Fault::Retry)?;
    let start = match (&resume.session, resume.seq) {
        (Some(session), Some(seq)) => json!({ "op": 6, "d": {
            "token": format!("Bot {}", api.token), "session_id": session, "seq": seq,
        }}),
        _ => json!({ "op": 2, "d": {
            "token": format!("Bot {}", api.token),
            "intents": INTENTS,
            "properties": { "os": std::env::consts::OS, "browser": "lynshen", "device": "lynshen" },
        }}),
    };
    send(&mut socket, &start)?;
    // The first beat comes after a random part of the interval, as Discord asks.
    let mut jitter = [0u8; 1];
    let _ = getrandom::getrandom(&mut jitter);
    let mut next_beat = Instant::now() + interval.mul_f64(f64::from(jitter[0]) / 255.0);
    let mut acked = true;
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = socket.close(None);
            let _ = socket.flush();
            return Ok(());
        }
        if Instant::now() >= next_beat {
            if !acked {
                return Err(Fault::Retry(
                    "the Discord gateway stopped answering heartbeats".to_string(),
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
                        resume.url = data["resume_gateway_url"].as_str().map(str::to_string);
                        connected();
                    }
                    "RESUMED" => connected(),
                    "MESSAGE_CREATE" => {
                        if let Some(incoming) = parse_message(data) {
                            if events
                                .send(Event::Inbound(Inbound::Message(incoming)))
                                .is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                    "INTERACTION_CREATE" => {
                        if let (Some(id), Some(token)) =
                            (data["id"].as_str(), data["token"].as_str())
                        {
                            api.acknowledge(id, token);
                        }
                        if let Some(press) = parse_press(data) {
                            if events.send(Event::Inbound(Inbound::Press(press))).is_err() {
                                return Ok(());
                            }
                        }
                    }
                    _ => {}
                }
            }
            // The gateway asks for a beat now.
            Some(1) => send(&mut socket, &json!({ "op": 1, "d": resume.seq }))?,
            Some(7) => {
                return Err(Fault::Retry(
                    "the Discord gateway asked to reconnect".to_string(),
                ))
            }
            Some(9) => {
                if frame["d"] != true {
                    *resume = Resume::default();
                }
                thread::sleep(Duration::from_secs(2));
                return Err(Fault::Retry(
                    "the Discord gateway ended the session".to_string(),
                ));
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

/// The next JSON frame, waiting up to `wait`.
fn read_frame(socket: &mut net::Socket, wait: Duration) -> Result<Value, Fault> {
    net::set_read_timeout(socket, wait).map_err(Fault::Retry)?;
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                return Ok(serde_json::from_str(text.as_str()).unwrap_or(Value::Null))
            }
            Ok(Message::Close(frame)) => {
                let code = frame.map(|frame| u16::from(frame.code)).unwrap_or(1000);
                return Err(Fault::Retry(format!("the Discord gateway closed ({code})")));
            }
            Ok(_) => {}
            Err(error) => return Err(Fault::Retry(error.to_string())),
        }
    }
}

/// What a close code from the gateway means.
fn closed(code: u16, resume: &mut Resume) -> Fault {
    match code {
        4004 => Fault::Rejected(
            "Discord refused the bot token: reset it in the Developer Portal".to_string(),
        ),
        4013 | 4014 => Fault::Rejected(format!(
            "Discord refused the intents ({code}); the bot needs only direct messages"
        )),
        4007 | 4009 => {
            *resume = Resume::default();
            Fault::Retry(format!("the Discord gateway session ended ({code})"))
        }
        _ => Fault::Retry(format!("the Discord gateway closed ({code})")),
    }
}

/// A MESSAGE_CREATE as the chat side sees it. A DM has no `guild_id`.
pub(crate) fn parse_message(data: &Value) -> Option<Incoming> {
    let author = &data["author"];
    let username = author["username"].as_str().unwrap_or_default();
    let name = match author["global_name"].as_str() {
        Some(shown) if !shown.is_empty() && shown != username => format!("{shown} (@{username})"),
        _ => format!("@{username}"),
    };
    let reference = &data["message_reference"];
    // A forward is a reference of type 1 with the forwarded message's copy.
    let forwarded = reference["type"] == 1
        || data["message_snapshots"]
            .as_array()
            .is_some_and(|snapshots| !snapshots.is_empty());
    let files: Vec<Attachment> = data["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| {
            Some(Attachment {
                id: file["id"].as_str()?.to_string(),
                name: file["filename"].as_str().unwrap_or("file").to_string(),
                size: file["size"].as_u64(),
                url: file["url"].as_str().map(str::to_string),
            })
        })
        .collect();
    let text = data["content"].as_str().unwrap_or_default().to_string();
    let unsupported = text.is_empty()
        && files.is_empty()
        && (data["sticker_items"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
            || !data["poll"].is_null());
    Some(Incoming {
        peer: author["id"].as_str()?.to_string(),
        chat: data["channel_id"].as_str()?.to_string(),
        private: data["guild_id"].is_null(),
        from_bot: author["bot"] == true,
        name,
        lang: String::new(),
        text,
        forwarded,
        reply_to: (!forwarded)
            .then(|| reference["message_id"].as_str().map(str::to_string))
            .flatten(),
        files,
        unsupported,
    })
}

/// A button press (INTERACTION_CREATE of type 3). In a DM the presser is
/// `user`; in a server, `member.user`.
pub(crate) fn parse_press(data: &Value) -> Option<Press> {
    if data["type"] != 3 {
        return None;
    }
    let user = if data["user"].is_object() {
        &data["user"]
    } else {
        &data["member"]["user"]
    };
    Some(Press {
        peer: user["id"].as_str()?.to_string(),
        private: data["guild_id"].is_null(),
        id: data["id"].as_str()?.to_string(),
        data: data["data"]["custom_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_tokens_are_checked_for_their_shape() {
        let token = concat!(
            "MTA5ODc2NTQz",
            "MjEwOTg3NjU0.",
            "GaBcDe.",
            "f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2"
        );
        assert!(check_token(token).is_ok());
        for bad in [
            "",
            "abc",
            "a.b.c",
            "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1",
        ] {
            assert!(check_token(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dms_become_messages_and_server_messages_do_not_count() {
        let dm = json!({
            "id": "m1", "channel_id": "c1",
            "author": { "id": "42", "username": "alice", "global_name": "Alice" },
            "content": "看看", "message_reference": { "message_id": "m0" },
            "attachments": [{ "id": "a1", "filename": "log.txt", "size": 12, "url": "https://cdn.discordapp.com/x" }]
        });
        let incoming = parse_message(&dm).unwrap();
        assert!(incoming.private && !incoming.from_bot && !incoming.forwarded);
        assert_eq!(
            (incoming.peer.as_str(), incoming.chat.as_str()),
            ("42", "c1")
        );
        assert_eq!(incoming.name, "Alice (@alice)");
        assert_eq!(incoming.reply_to.as_deref(), Some("m0"));
        assert_eq!(
            incoming.files[0].url.as_deref(),
            Some("https://cdn.discordapp.com/x")
        );

        let server = json!({ "channel_id": "c2", "guild_id": "g1", "author": { "id": "42", "username": "alice" }, "content": "hi" });
        assert!(!parse_message(&server).unwrap().private);
        let forward = json!({
            "channel_id": "c1", "author": { "id": "42", "username": "alice" }, "content": "",
            "message_reference": { "type": 1, "message_id": "m9" }, "message_snapshots": [{ "message": { "content": "x" } }]
        });
        let forward = parse_message(&forward).unwrap();
        assert!(forward.forwarded && forward.reply_to.is_none());
        let bot = json!({ "channel_id": "c1", "author": { "id": "7", "username": "me", "bot": true }, "content": "hi" });
        assert!(parse_message(&bot).unwrap().from_bot);

        let press = json!({ "id": "i1", "token": "t", "type": 3, "user": { "id": "42" }, "data": { "custom_id": "a:K7QX2:y" } });
        let press = parse_press(&press).unwrap();
        assert_eq!(
            (press.peer.as_str(), press.data.as_str()),
            ("42", "a:K7QX2:y")
        );
        assert!(press.private);
        let in_server = json!({ "id": "i2", "type": 3, "guild_id": "g", "member": { "user": { "id": "42" } }, "data": { "custom_id": "x" } });
        assert!(!parse_press(&in_server).unwrap().private);
        assert!(parse_press(&json!({ "id": "i3", "type": 2 })).is_none());
    }

    #[test]
    fn close_codes_say_whether_to_try_again() {
        let mut resume = Resume {
            session: Some("s".to_string()),
            url: Some("wss://x".to_string()),
            seq: Some(3),
        };
        assert!(matches!(closed(4004, &mut resume), Fault::Rejected(_)));
        assert!(matches!(closed(4014, &mut resume), Fault::Rejected(_)));
        assert!(resume.session.is_some());
        assert!(matches!(closed(4009, &mut resume), Fault::Retry(_)));
        assert!(resume.session.is_none());
        assert!(matches!(closed(1001, &mut resume), Fault::Retry(_)));
    }

    #[test]
    fn buttons_become_one_row_of_components() {
        let row = Api::components(&[
            Button {
                label: "允许".into(),
                data: "a:X:y".into(),
            },
            Button {
                label: "拒绝".into(),
                data: "a:X:n".into(),
            },
        ]);
        assert_eq!(row[0]["type"], 1);
        assert_eq!(row[0]["components"][0]["style"], 1);
        assert_eq!(row[0]["components"][1]["style"], 4);
        assert_eq!(row[0]["components"][1]["custom_id"], "a:X:n");
        assert_eq!(Api::components(&[]), json!([]));
    }
}

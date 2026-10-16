//! Telegram (docs/chat-connectors.md, "Telegram"): the Bot API over HTTPS,
//! `getUpdates` long polling to receive, through the connector's proxy.

use super::{
    chat::{self, Attachment, Button, Event, Inbound, Incoming, Limits, Press},
    http_agent, proxy_for, render, Connector, Fault,
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
    time::Duration,
};

pub(crate) const ORIGIN: &str = "https://api.telegram.org";
/// How long one `getUpdates` waits for an update.
const POLL_SECONDS: u64 = 25;
/// The largest file a bot may download (`getFile`) and upload.
const MAX_IN: u64 = 20 * 1024 * 1024;
const MAX_OUT: u64 = 50 * 1024 * 1024;
/// Characters of Markdown per message: Telegram takes 4096 after HTML.
const TEXT: usize = 3500;

/// The bot behind a token.
pub(crate) struct Bot {
    pub id: String,
    pub username: String,
}

/// Why a call failed.
pub(crate) enum Failure {
    /// Telegram answered with an error: status, description, `retry_after`.
    Http(u16, String, Option<u64>),
    /// No answer (network, proxy, timeout).
    Net(String),
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Failure::Http(status, description, _) => {
                format!("Telegram answered {status}: {description}")
            }
            Failure::Net(error) => format!("cannot reach Telegram: {error}"),
        }
    }
}

/// A token as @BotFather gives it: `<digits>:<secret>`.
pub(crate) fn check_token(token: &str) -> Result<(), String> {
    let valid = token.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && secret.len() >= 30
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if valid {
        Ok(())
    } else {
        Err(
            "this is not a Telegram bot token (@BotFather gives one like 123456789:AA…)"
                .to_string(),
        )
    }
}

#[derive(Clone)]
pub(crate) struct Api {
    origin: String,
    token: String,
    agent: ureq::Agent,
    /// For `getUpdates`, which waits up to `POLL_SECONDS` for an answer.
    poll: ureq::Agent,
}

impl Api {
    pub(crate) fn new(connector: &Connector) -> Result<Self, String> {
        let origin = connector.api_origin();
        let (proxy, _) = proxy_for(connector.proxy.as_deref(), &origin);
        Ok(Self {
            agent: http_agent(proxy.as_deref(), Duration::from_secs(60))?,
            poll: http_agent(proxy.as_deref(), Duration::from_secs(POLL_SECONDS + 15))?,
            origin,
            token: connector.token.clone(),
        })
    }

    /// The token never leaves in an error or a log line.
    fn scrub(&self, text: &str) -> String {
        if self.token.is_empty() {
            return text.to_string();
        }
        text.replace(&self.token, "<token>")
    }

    fn call_with(&self, agent: &ureq::Agent, method: &str, body: &Value) -> Result<Value, Failure> {
        let url = format!("{}/bot{}/{method}", self.origin, self.token);
        let response = agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        self.answer(response)
    }

    fn call(&self, method: &str, body: &Value) -> Result<Value, Failure> {
        self.call_with(&self.agent, method, body)
    }

    /// Telegram's `{ok, result}` or `{ok: false, error_code, description}`.
    fn answer(&self, response: Result<ureq::Response, ureq::Error>) -> Result<Value, Failure> {
        let (status, text) = match response {
            Ok(response) => {
                let status = response.status();
                let text = response
                    .into_string()
                    .map_err(|error| Failure::Net(self.scrub(&error.to_string())))?;
                (status, text)
            }
            Err(ureq::Error::Status(status, response)) => {
                (status, response.into_string().unwrap_or_default())
            }
            Err(ureq::Error::Transport(error)) => {
                return Err(Failure::Net(self.scrub(&error.to_string())))
            }
        };
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if value["ok"] == true {
            return Ok(value["result"].clone());
        }
        let description = value["description"]
            .as_str()
            .map_or_else(|| format!("HTTP {status}"), |text| self.scrub(text));
        Err(Failure::Http(
            value["error_code"]
                .as_u64()
                .map_or(status, |code| code as u16),
            description,
            value["parameters"]["retry_after"].as_u64(),
        ))
    }

    /// `call`, waiting once as Telegram asks when it answers 429.
    fn call_paced(&self, method: &str, body: &Value) -> Result<Value, Failure> {
        match self.call(method, body) {
            Err(Failure::Http(429, _, retry)) => {
                thread::sleep(Duration::from_secs(retry.unwrap_or(3).min(30)));
                self.call(method, body)
            }
            other => other,
        }
    }

    /// The bot behind the token (`getMe`).
    pub(crate) fn me(&self) -> Result<Bot, String> {
        let me = self
            .call("getMe", &json!({}))
            .map_err(|failure| match failure {
                Failure::Http(401 | 404, ..) => {
                    "Telegram does not know this token: copy it again from @BotFather".to_string()
                }
                failure => failure.message(),
            })?;
        let id = me["id"].as_i64().ok_or("Telegram did not name the bot")?;
        Ok(Bot {
            id: id.to_string(),
            username: me["username"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// The bot's command menu, in Chinese and for English accounts.
    fn set_commands(&self) {
        let commands = |zh: bool| -> Value {
            let list: Vec<Value> = chat::COMMANDS
                .iter()
                .map(|(command, chinese, english)| {
                    let description = if zh { chinese } else { english };
                    json!({ "command": command, "description": description })
                })
                .collect();
            json!(list)
        };
        let _ = self.call("setMyCommands", &json!({ "commands": commands(true) }));
        let _ = self.call(
            "setMyCommands",
            &json!({ "commands": commands(false), "language_code": "en" }),
        );
    }

    fn keyboard(buttons: &[Button]) -> Value {
        let row: Vec<Value> = buttons
            .iter()
            .map(|button| json!({ "text": button.label, "callback_data": button.data }))
            .collect();
        let rows = if row.is_empty() {
            json!([])
        } else {
            json!([row])
        };
        json!({ "inline_keyboard": rows })
    }
}

/// A chat id as Telegram takes it: a number when it is one.
fn chat_id(chat: &str) -> Value {
    chat.parse::<i64>()
        .map_or_else(|_| json!(chat), |id| json!(id))
}

/// Telegram refused the HTML (an entity it cannot parse).
fn bad_markup(failure: &Failure) -> bool {
    matches!(failure, Failure::Http(400, description, _)
        if description.contains("parse") || description.contains("entit"))
}

impl chat::Api for Api {
    fn limits(&self) -> Limits {
        Limits {
            text: TEXT,
            file_in: MAX_IN,
            file_out: MAX_OUT,
            buttons: true,
        }
    }

    fn send(&self, chat: &str, text: &str, buttons: &[Button]) -> Result<String, String> {
        let mut body = json!({
            "chat_id": chat_id(chat),
            "text": render::telegram_html(text),
            "parse_mode": "HTML",
            "link_preview_options": { "is_disabled": true },
        });
        if !buttons.is_empty() {
            body["reply_markup"] = Self::keyboard(buttons);
        }
        let sent = match self.call_paced("sendMessage", &body) {
            Err(failure) if bad_markup(&failure) => {
                body["text"] = json!(text);
                if let Some(body) = body.as_object_mut() {
                    body.remove("parse_mode");
                }
                self.call_paced("sendMessage", &body)
            }
            other => other,
        };
        let message = sent.map_err(|failure| failure.message())?;
        message["message_id"]
            .as_i64()
            .map(|id| id.to_string())
            .ok_or_else(|| "Telegram did not return the message".to_string())
    }

    fn edit(
        &self,
        chat: &str,
        message: &str,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), String> {
        let mut body = json!({
            "chat_id": chat_id(chat),
            "message_id": message.parse::<i64>().unwrap_or_default(),
            "text": render::telegram_html(text),
            "parse_mode": "HTML",
            "link_preview_options": { "is_disabled": true },
            "reply_markup": Self::keyboard(buttons),
        });
        let edited = match self.call_paced("editMessageText", &body) {
            Err(failure) if bad_markup(&failure) => {
                body["text"] = json!(text);
                if let Some(body) = body.as_object_mut() {
                    body.remove("parse_mode");
                }
                self.call_paced("editMessageText", &body)
            }
            other => other,
        };
        match edited {
            Ok(_) => Ok(()),
            Err(Failure::Http(400, description, _)) if description.contains("not modified") => {
                Ok(())
            }
            Err(failure) => Err(failure.message()),
        }
    }

    fn send_file(&self, chat: &str, name: &str, bytes: &[u8], caption: &str) -> Result<(), String> {
        let boundary = format!("lynshen-{}", random_hex(12).map_err(|e| e.to_string())?);
        let mut body = Vec::with_capacity(bytes.len() + 512);
        let mut field = |key: &str, value: &str| {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        };
        field("chat_id", chat);
        if !caption.is_empty() {
            field("caption", &render::clip(caption, 1000));
        }
        let file_name: String = name
            .chars()
            .map(|c| if c == '"' || c.is_control() { '_' } else { c })
            .collect();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = format!("{}/bot{}/sendDocument", self.origin, self.token);
        let response = self
            .agent
            .post(&url)
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
        let limit = limit.min(MAX_IN);
        if file.size.is_some_and(|size| size > limit) {
            return Err(format!("larger than {} MB", limit >> 20));
        }
        let info = self
            .call("getFile", &json!({ "file_id": file.id }))
            .map_err(|failure| failure.message())?;
        let path = info["file_path"]
            .as_str()
            .ok_or("Telegram gave no file path (files over 20 MB cannot be downloaded)")?;
        let url = format!("{}/file/bot{}/{path}", self.origin, self.token);
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
            .map_err(|error| self.scrub(&error.to_string()))?;
        if bytes.len() as u64 > limit {
            return Err(format!("larger than {} MB", limit >> 20));
        }
        Ok(bytes)
    }

    fn ack(&self, press: &str, text: &str) {
        let mut body = json!({ "callback_query_id": press });
        if !text.is_empty() {
            body["text"] = json!(text);
        }
        let _ = self.call("answerCallbackQuery", &body);
    }
}

/// Receives updates until the connection fails or `stop` is set; each one
/// goes to the chat side, and the offset is saved once they are handed on.
pub(crate) fn receive(
    hub: &Hub,
    id: &str,
    api: &Api,
    stop: &AtomicBool,
    events: &Sender<Event>,
    connected: &dyn Fn(),
) -> Result<(), Fault> {
    let mut first = true;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let offset = hub.connectors.offset(id);
        let body = json!({
            "offset": offset,
            "timeout": POLL_SECONDS,
            "allowed_updates": ["message", "callback_query"],
        });
        let updates = match api.call_with(&api.poll, "getUpdates", &body) {
            Ok(updates) => updates,
            Err(Failure::Http(401 | 404, ..)) => {
                return Err(Fault::Rejected(
                    "Telegram no longer accepts this bot token: copy the current one from @BotFather".to_string(),
                ))
            }
            Err(Failure::Http(409, description, _)) => {
                return Err(Fault::Conflict(format!(
                    "another program receives this bot's messages (or the bot has a webhook); use one bot per computer: {description}"
                )))
            }
            Err(Failure::Http(429, _, retry)) => {
                thread::sleep(Duration::from_secs(retry.unwrap_or(5).min(60)));
                continue;
            }
            Err(failure) => return Err(Fault::Retry(failure.message())),
        };
        // Stopped meanwhile: these updates come again to the next poller.
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        connected();
        if first {
            first = false;
            api.set_commands();
        }
        let mut next = offset;
        for update in updates.as_array().into_iter().flatten() {
            if let Some(update_id) = update["update_id"].as_i64() {
                next = next.max(update_id + 1);
            }
            if let Some(inbound) = parse(update) {
                if events.send(Event::Inbound(inbound)).is_err() {
                    return Ok(());
                }
            }
        }
        hub.connectors.set_offset(id, next);
    }
}

/// An id Telegram sends as a number.
fn id_text(value: &Value) -> Option<String> {
    value
        .as_i64()
        .map(|id| id.to_string())
        .or_else(|| value.as_str().map(str::to_string))
}

/// An update as the chat side sees it: a message or a button press. Other
/// updates (edits, channel posts) are dropped.
pub(crate) fn parse(update: &Value) -> Option<Inbound> {
    if let Some(message) = update.get("message") {
        let from = &message["from"];
        let name = [from["first_name"].as_str(), from["last_name"].as_str()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let name = match from["username"].as_str() {
            Some(user) if name.is_empty() => format!("@{user}"),
            Some(user) => format!("{name} (@{user})"),
            None => name,
        };
        let mut files = Vec::new();
        if let Some(photo) = message["photo"].as_array().and_then(|sizes| sizes.last()) {
            files.push(Attachment {
                id: photo["file_id"].as_str().unwrap_or_default().to_string(),
                name: format!(
                    "photo-{}.jpg",
                    id_text(&message["message_id"]).unwrap_or_default()
                ),
                size: photo["file_size"].as_u64(),
            });
        }
        if let Some(document) = message.get("document") {
            files.push(Attachment {
                id: document["file_id"].as_str().unwrap_or_default().to_string(),
                name: document["file_name"].as_str().unwrap_or("file").to_string(),
                size: document["file_size"].as_u64(),
            });
        }
        let text = message["text"]
            .as_str()
            .filter(|text| !text.is_empty())
            .or(message["caption"].as_str())
            .unwrap_or_default()
            .to_string();
        let unsupported = text.is_empty()
            && files.is_empty()
            && [
                "voice",
                "video",
                "video_note",
                "audio",
                "sticker",
                "animation",
                "location",
                "contact",
                "poll",
            ]
            .iter()
            .any(|kind| message.get(*kind).is_some());
        return Some(Inbound::Message(Incoming {
            peer: id_text(&from["id"])?,
            chat: id_text(&message["chat"]["id"])?,
            private: message["chat"]["type"] == "private",
            from_bot: from["is_bot"] == true,
            name,
            lang: from["language_code"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            id: id_text(&message["message_id"])?,
            text,
            forwarded: [
                "forward_origin",
                "forward_from",
                "forward_from_chat",
                "forward_sender_name",
                "forward_date",
            ]
            .iter()
            .any(|key| message.get(*key).is_some()),
            reply_to: id_text(&message["reply_to_message"]["message_id"]),
            files,
            unsupported,
        }));
    }
    let query = update.get("callback_query")?;
    Some(Inbound::Press(Press {
        peer: id_text(&query["from"]["id"])?,
        private: query["message"]["chat"]["type"] == "private",
        id: query["id"].as_str()?.to_string(),
        data: query["data"].as_str().unwrap_or_default().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_tokens_are_checked_for_their_shape() {
        assert!(check_token("123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1").is_ok());
        for bad in [
            "",
            "123456789",
            "abc:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1",
            "123:short",
            "123:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1 ",
        ] {
            assert!(check_token(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn updates_become_messages_and_presses() {
        let message = json!({
            "update_id": 9,
            "message": {
                "message_id": 31,
                "from": { "id": 42, "is_bot": false, "first_name": "Alice", "username": "alice", "language_code": "en" },
                "chat": { "id": 42, "type": "private" },
                "caption": "看看这张图",
                "photo": [
                    { "file_id": "small", "file_size": 100 },
                    { "file_id": "big", "file_size": 5000 }
                ],
                "reply_to_message": { "message_id": 30 }
            }
        });
        let Some(Inbound::Message(incoming)) = parse(&message) else {
            panic!("a message");
        };
        assert_eq!(incoming.peer, "42");
        assert_eq!(incoming.chat, "42");
        assert!(incoming.private && !incoming.from_bot && !incoming.forwarded);
        assert_eq!(incoming.name, "Alice (@alice)");
        assert_eq!(incoming.lang, "en");
        assert_eq!(incoming.text, "看看这张图");
        assert_eq!(incoming.reply_to.as_deref(), Some("30"));
        assert_eq!(incoming.files.len(), 1);
        assert_eq!(incoming.files[0].id, "big");
        assert_eq!(incoming.files[0].name, "photo-31.jpg");

        let group = json!({ "update_id": 10, "message": {
            "message_id": 1, "from": { "id": 7 }, "chat": { "id": -100, "type": "group" },
            "text": "hi", "forward_origin": { "type": "user" }
        }});
        let Some(Inbound::Message(incoming)) = parse(&group) else {
            panic!("a message");
        };
        assert!(!incoming.private && incoming.forwarded);

        let voice = json!({ "update_id": 11, "message": {
            "message_id": 2, "from": { "id": 7 }, "chat": { "id": 7, "type": "private" },
            "voice": { "file_id": "v" }
        }});
        let Some(Inbound::Message(incoming)) = parse(&voice) else {
            panic!("a message");
        };
        assert!(incoming.unsupported);

        let press = json!({ "update_id": 12, "callback_query": {
            "id": "cb1", "from": { "id": 42 }, "data": "a:K7QX2:y",
            "message": { "message_id": 33, "chat": { "id": 42, "type": "private" } }
        }});
        let Some(Inbound::Press(press)) = parse(&press) else {
            panic!("a press");
        };
        assert_eq!(
            (press.peer.as_str(), press.id.as_str(), press.data.as_str()),
            ("42", "cb1", "a:K7QX2:y")
        );
        assert!(press.private);
        assert!(parse(&json!({ "update_id": 13, "edited_message": {} })).is_none());
    }

    #[test]
    fn errors_never_carry_the_token() {
        let connector = Connector {
            id: "tg-1".to_string(),
            platform: "telegram".to_string(),
            name: String::new(),
            username: String::new(),
            bot_id: String::new(),
            enabled: true,
            proxy: Some("direct".to_string()),
            api: Some("http://127.0.0.1:9".to_string()),
            token: "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1".to_string(),
            sealed: String::new(),
            settings: Default::default(),
            created_at: 0,
        };
        let api = Api::new(&connector).unwrap();
        // Port 9 refuses: the error names the URL, with the token scrubbed.
        let error = api.me().unwrap_err();
        assert!(
            !error.contains("AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw1"),
            "{error}"
        );
        assert_eq!(chat_id("42"), json!(42));
        assert_eq!(chat_id("@channel"), json!("@channel"));
    }
}

//! The chat side of a connector, the same for every platform: who is bound,
//! what a bound account's messages do, and what the bot posts back
//! (docs/chat-connectors.md, "In the chat").
//!
//! Each binding is a hub client of its own (its device is the binding), so
//! its ops go through `handle()` and the chat profile like a phone's. Its
//! client receives every broadcast; the binding keeps what concerns it: its
//! current session's turns, approval requests, plans, and the daemon's
//! deferred actions and questions.

use super::{bind, profile, redact, render, Redeemed, MODES};
use crate::{
    hub::{Hub, Outbox},
    session_messages::rank,
    store::Device,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

/// The bot's command menu: command, Chinese and English description.
pub(crate) const COMMANDS: [(&str, &str, &str); 11] = [
    ("new", "在项目里新建会话", "New session in a project"),
    ("chat", "新建聊天会话", "New chat session"),
    ("sessions", "最近的会话", "Recent sessions"),
    ("use", "切换当前会话", "Switch to a session"),
    ("stop", "停止当前任务", "Stop the current task"),
    ("status", "当前会话状态", "Session status"),
    (
        "mode",
        "查看或切换权限模式",
        "Show or set the approval mode",
    ),
    ("diff", "当前项目的改动", "Changes in the project"),
    ("file", "发送项目里的文件", "Send a project file"),
    ("open", "手机远程页地址", "Phone page address"),
    ("help", "帮助", "Help"),
];

/// How often the worker looks for work when nothing happened.
const POLL: Duration = Duration::from_millis(40);
/// The status message changes at most this often.
const EDIT_EVERY: Duration = Duration::from_secs(3);
/// A binding watches its session this long after the account's last message.
const ACTIVE: Duration = Duration::from_secs(30 * 60);
/// An approval card's buttons work this long.
const CARD_TTL: Duration = Duration::from_secs(30 * 60);
/// Where messages cannot be edited, a running turn says so this often.
const NOTE_EVERY: Duration = Duration::from_secs(10 * 60);
/// The same notification goes out at most once in this time.
const NOTICE_EVERY: Duration = Duration::from_secs(60);
/// Messages of one reply; the rest goes as a file.
const MAX_PARTS: usize = 3;
/// Sessions `/sessions` lists.
const LIST: usize = 10;
/// Frames of one binding handled per round, so one busy session cannot
/// hold up the others.
const FRAMES_PER_ROUND: usize = 500;
/// Characters of a card's command or summary.
const CARD_TEXT: usize = 500;
const REMOTE_PAGE: &str = "https://app.lynshen.org/remote";

/// What reaches the chat side: from the platform, or from the daemon.
pub(crate) enum Event {
    Inbound(Inbound),
    Notice {
        title: String,
        body: String,
        tag: String,
    },
}

pub(crate) enum Inbound {
    Message(Incoming),
    Press(Press),
}

/// A message to the bot.
pub(crate) struct Incoming {
    /// The sender's immutable id on the platform.
    pub peer: String,
    /// The chat to answer in.
    pub chat: String,
    /// A private chat with the bot (not a group or a channel).
    pub private: bool,
    pub from_bot: bool,
    /// The sender as people see them.
    pub name: String,
    /// The sender's language as the platform reports it (`zh-hans`, `en`).
    pub lang: String,
    pub text: String,
    /// Forwarded from someone else.
    pub forwarded: bool,
    /// The bot message this one replies to.
    pub reply_to: Option<String>,
    pub files: Vec<Attachment>,
    /// A kind of message the bot does not take (voice, sticker, ...).
    pub unsupported: bool,
}

/// A press on one of the bot's buttons.
pub(crate) struct Press {
    pub peer: String,
    pub private: bool,
    /// The press's id, to answer it.
    pub id: String,
    pub data: String,
}

/// A file sent to the bot.
pub(crate) struct Attachment {
    pub id: String,
    pub name: String,
    pub size: Option<u64>,
    /// Where it downloads from, when the platform gives an address.
    pub url: Option<String>,
}

pub(crate) struct Button {
    pub label: String,
    pub data: String,
}

/// What a platform accepts.
pub(crate) struct Limits {
    /// Characters of Markdown per message.
    pub text: usize,
    /// Bytes of a file the bot may download, and send.
    pub file_in: u64,
    pub file_out: u64,
    /// Whether messages can carry buttons.
    pub buttons: bool,
    /// Whether a sent message can be changed: without, a turn shows no
    /// status message, only a line every `NOTE_EVERY` while it runs.
    pub edits: bool,
}

/// A platform's API, as the chat side uses it. Text is Markdown; the
/// platform renders it.
pub(crate) trait Api: Send {
    fn limits(&self) -> Limits;
    /// Sends a message; returns its id.
    fn send(&self, chat: &str, text: &str, buttons: &[Button]) -> Result<String, String>;
    /// Replaces a message's text and buttons.
    fn edit(&self, chat: &str, message: &str, text: &str, buttons: &[Button])
        -> Result<(), String>;
    fn send_file(&self, chat: &str, name: &str, bytes: &[u8], caption: &str) -> Result<(), String>;
    fn download(&self, file: &Attachment, limit: u64) -> Result<Vec<u8>, String>;
    /// Answers a button press (the platform stops showing it as pending).
    fn ack(&self, press: &str, text: &str);
}

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Zh,
    En,
}

fn lang_of(code: &str) -> Lang {
    if code.is_empty() || code.starts_with("zh") {
        Lang::Zh
    } else {
        Lang::En
    }
}

fn tr(lang: Lang, zh: &str, en: &str) -> String {
    match lang {
        Lang::Zh => zh.to_string(),
        Lang::En => en.to_string(),
    }
}

/// What a reply to one of the bot's messages answers.
#[derive(Clone)]
enum Reply {
    Question(String),
    Plan { session: String, plan: String },
}

/// What a card's buttons decide.
#[derive(Clone)]
enum Target {
    Call(String),
    Action(String),
    Plan(String),
}

struct Card {
    device: String,
    session: String,
    target: Target,
    chat: String,
    message: Option<String>,
    text: String,
    created: Instant,
}

struct Turn {
    session: String,
    /// The status message.
    status: Option<String>,
    started: Instant,
    step: String,
    shown: String,
    edited: Instant,
    /// The text of the turn's latest response.
    reply: String,
    error: Option<String>,
    /// Something happened since it started (Claude Code reports `ready`
    /// once at the start of a turn).
    active: bool,
    /// Where messages cannot be edited: running notes sent so far.
    notes: u32,
}

enum Ending {
    Done,
    Stopped,
    Closed,
}

struct Binding {
    device: Device,
    peer: String,
    chat: String,
    lang: Lang,
    client: u64,
    inbox: Receiver<String>,
    /// Frames read while waiting for a reply, handled next round.
    backlog: VecDeque<Value>,
    current: Option<String>,
    watching: Option<String>,
    active: Instant,
    /// Messages sent to a session and not started yet: session, content.
    sent: VecDeque<(String, String, Instant)>,
    turn: Option<Turn>,
    /// Files received, waiting for the next text.
    files: Vec<(PathBuf, bool)>,
    replies: HashMap<String, Reply>,
    /// What the last `/sessions` and `/new` listed.
    listed: Vec<String>,
    projects: Vec<(String, PathBuf)>,
    /// Deferred actions and questions already announced.
    actions: HashSet<String>,
    questions: HashSet<String>,
    /// Sessions the account chose "本轮都允许" (allow all this turn) for:
    /// the next ordinary approval requests of that session run without a
    /// card until the turn ends. Each is allowed once, like the per-request
    /// allow; never an elevation, an escalation or a rule that must ask.
    allow_turn: HashSet<String>,
    /// Revoked: its client is gone.
    gone: bool,
}

struct Chat {
    hub: Arc<Hub>,
    connector: String,
    api: Box<dyn Api>,
    bindings: HashMap<String, Binding>,
    /// Approval, action and plan cards by their short id.
    cards: HashMap<String, Card>,
    notices: HashMap<String, Instant>,
    next_request: u64,
}

/// The chat side of `connector` until `stop` is set.
pub(crate) fn run(
    hub: Arc<Hub>,
    connector: String,
    api: Box<dyn Api>,
    stop: Arc<AtomicBool>,
    events: Receiver<Event>,
) {
    let mut chat = Chat {
        hub,
        connector,
        api,
        bindings: HashMap::new(),
        cards: HashMap::new(),
        notices: HashMap::new(),
        next_request: 0,
    };
    for device in chat.hub.store.devices() {
        let mine = device
            .chat
            .as_ref()
            .is_some_and(|peer| peer.connector == chat.connector);
        if mine && !device.revoked {
            chat.add_binding(device);
        }
    }
    while !stop.load(Ordering::SeqCst) {
        let mut busy = false;
        loop {
            match events.try_recv() {
                Ok(event) => {
                    busy = true;
                    chat.event(event);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    chat.close();
                    return;
                }
            }
            if stop.load(Ordering::SeqCst) {
                break;
            }
        }
        busy |= chat.pump();
        chat.tick();
        if !busy {
            thread::sleep(POLL);
        }
    }
    chat.close();
}

impl Chat {
    fn add_binding(&mut self, device: Device) {
        let Some(peer) = device.chat.clone() else {
            return;
        };
        let (outbox, inbox) = mpsc::channel();
        let client = self
            .hub
            .add_client(Outbox::local(outbox), Some(device.id.clone()));
        let binding = Binding {
            peer: peer.peer,
            chat: peer.chat,
            lang: lang_of(&peer.lang),
            client,
            inbox,
            backlog: VecDeque::new(),
            current: self.hub.connectors.current(&self.connector, &device.id),
            watching: None,
            active: Instant::now(),
            sent: VecDeque::new(),
            turn: None,
            files: Vec::new(),
            replies: HashMap::new(),
            listed: Vec::new(),
            projects: Vec::new(),
            actions: self
                .hub
                .store
                .open_actions()
                .into_iter()
                .map(|a| a.id)
                .collect(),
            questions: self
                .hub
                .store
                .open_questions()
                .into_iter()
                .map(|q| q.id)
                .collect(),
            allow_turn: HashSet::new(),
            gone: false,
            device,
        };
        self.bindings.insert(binding.device.id.clone(), binding);
    }

    fn drop_binding(&mut self, device: &str) {
        if let Some(binding) = self.bindings.remove(device) {
            // Unwatches its session (revoking only dropped its outbox).
            self.hub.remove_client(binding.client);
        }
        self.cards.retain(|_, card| card.device != device);
    }

    fn close(&mut self) {
        let devices: Vec<String> = self.bindings.keys().cloned().collect();
        for device in devices {
            self.drop_binding(&device);
        }
    }

    fn device_for_peer(&self, peer: &str) -> Option<String> {
        self.bindings
            .values()
            .find(|binding| binding.peer == peer && !binding.gone)
            .map(|binding| binding.device.id.clone())
    }

    fn lang(&self, device: &str) -> Lang {
        self.bindings.get(device).map_or(Lang::Zh, |b| b.lang)
    }

    fn settings(&self) -> super::Settings {
        self.hub
            .connectors
            .settings(&self.connector)
            .unwrap_or_default()
    }

    /// Sends `text` (redacted) to `chat`; the message id, or None when it
    /// could not be sent (logged).
    fn post(&self, chat: &str, text: &str, buttons: &[Button]) -> Option<String> {
        let text = redact::redact(text, &self.hub.connectors.secrets());
        match self.api.send(chat, &text, buttons) {
            Ok(id) => Some(id),
            Err(error) => {
                lynshen_agent_core::log_warn!("daemon", "chat message not sent", error = error);
                None
            }
        }
    }

    fn edit(&self, chat: &str, message: &str, text: &str, buttons: &[Button]) {
        if !self.api.limits().edits {
            return;
        }
        let text = redact::redact(text, &self.hub.connectors.secrets());
        if let Err(error) = self.api.edit(chat, message, &text, buttons) {
            lynshen_agent_core::log_warn!("daemon", "chat message not edited", error = error);
        }
    }

    /// A message to a binding's account.
    fn tell(&self, device: &str, text: &str) -> Option<String> {
        let chat = self.bindings.get(device)?.chat.clone();
        self.post(&chat, text, &[])
    }

    /// A long text in as many messages as it takes, at most `MAX_PARTS`;
    /// the rest as a Markdown file. The ids of the messages sent.
    fn post_long(&self, chat: &str, text: &str, lang: Lang) -> Vec<String> {
        let text = redact::redact(text, &self.hub.connectors.secrets());
        let pieces = render::split(&text, self.api.limits().text);
        let mut sent = Vec::new();
        for (index, piece) in pieces.iter().enumerate() {
            if index == MAX_PARTS && self.api.limits().file_out == 0 {
                // A platform that takes no files from the bot: say what is left.
                let left: usize = pieces[index..].iter().map(|p| p.chars().count()).sum();
                let note = match lang {
                    Lang::Zh => format!("（还有 {left} 字没有发出，完整回复请在电脑或手机远程页查看。）"),
                    Lang::En => format!("({left} more characters: read the whole reply on the desktop or the phone page.)"),
                };
                if let Ok(id) = self.api.send(chat, &note, &[]) {
                    sent.push(id);
                }
                break;
            }
            if index == MAX_PARTS {
                let rest = pieces[index..].join("\n\n");
                let caption = tr(lang, "后面的内容在文件里。", "The rest is in the file.");
                if let Err(error) = self
                    .api
                    .send_file(chat, "reply.md", rest.as_bytes(), &caption)
                {
                    lynshen_agent_core::log_warn!("daemon", "chat file not sent", error = error);
                }
                break;
            }
            match self.api.send(chat, piece, &[]) {
                Ok(id) => sent.push(id),
                Err(error) => {
                    lynshen_agent_core::log_warn!("daemon", "chat message not sent", error = error)
                }
            }
        }
        sent
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Inbound(Inbound::Message(message)) => self.message(message),
            Event::Inbound(Inbound::Press(press)) => self.press(press),
            Event::Notice { title, body, tag } => self.notice(&title, &body, &tag),
        }
    }

    fn message(&mut self, message: Incoming) {
        // Groups, channels and other bots never count, bound or not.
        if !message.private || message.from_bot {
            return;
        }
        match self.device_for_peer(&message.peer) {
            Some(device) => self.owner(&device, message),
            None => self.stranger(message),
        }
    }

    /// A message from someone not bound: only a binding code counts, and
    /// only while one waits. Nothing else is answered or passed on.
    fn stranger(&mut self, message: Incoming) {
        let lang = lang_of(&message.lang);
        let Some(code) = bind_code(&message.text) else {
            if self.hub.connectors.code_pending(&self.connector) {
                let hint = tr(
                    lang,
                    "请发送 /bind 加电脑上显示的 8 位绑定码。",
                    "Send /bind followed by the 8-character code shown on the computer.",
                );
                self.post(&message.chat, &hint, &[]);
            }
            return;
        };
        match self
            .hub
            .connectors
            .redeem(&self.connector, &message.peer, &code)
        {
            Redeemed::Bound => {
                let Some(connector) = self.hub.connectors.get(&self.connector) else {
                    return;
                };
                let code = if lang == Lang::Zh { "zh" } else { "en" };
                match bind(
                    &self.hub,
                    &connector,
                    &message.peer,
                    &message.chat,
                    &message.name,
                    code,
                ) {
                    Ok(device) => {
                        let id = device.id.clone();
                        self.add_binding(device);
                        let welcome = tr(
                            lang,
                            "绑定成功。现在可以在这里给电脑上的 LynShen 发任务。\n\n",
                            "Bound. You can now send tasks to LynShen on your computer from here.\n\n",
                        );
                        self.tell(&id, &format!("{welcome}{}", help(lang)));
                    }
                    Err(error) => {
                        lynshen_agent_core::log_warn!(
                            "daemon",
                            "chat binding failed",
                            error = error
                        )
                    }
                }
            }
            Redeemed::Wrong => {
                self.hub.connectors.audit(
                    &self.connector,
                    "bind_refused",
                    json!({ "peer": message.peer }),
                );
                let text = tr(
                    lang,
                    "绑定码不对或已过期。请在电脑上重新获取。",
                    "The code is wrong or expired. Get a new one on the computer.",
                );
                self.post(&message.chat, &text, &[]);
            }
            Redeemed::NoCode | Redeemed::Locked => {}
        }
    }

    /// A message from a bound account.
    fn owner(&mut self, device: &str, message: Incoming) {
        if self.hub.store.device(device).is_none() {
            self.drop_binding(device);
            return;
        }
        self.hub.touch_device(device);
        let lang = {
            let Some(binding) = self.bindings.get_mut(device) else {
                return;
            };
            if !message.lang.is_empty() {
                binding.lang = lang_of(&message.lang);
            }
            binding.active = Instant::now();
            binding.lang
        };
        if message.forwarded {
            self.refused(device, "forwarded");
            self.tell(
                device,
                &tr(
                    lang,
                    "转发的消息不会交给会话：它的内容来自别人。需要的话，请复制内容后自己发送。",
                    "Forwarded messages are not passed to sessions: their content comes from someone else. Copy the text and send it yourself if you need it.",
                ),
            );
            return;
        }
        if message.unsupported {
            self.tell(
                device,
                &tr(
                    lang,
                    "暂不支持这种消息。可以发文字、图片或文件。",
                    "This kind of message is not supported. Send text, photos or files.",
                ),
            );
            return;
        }
        for file in &message.files {
            if let Err(error) = self.receive_file(device, file) {
                let text = match lang {
                    Lang::Zh => format!("没收到文件 {}：{error}", file.name),
                    Lang::En => format!("Could not take the file {}: {error}", file.name),
                };
                self.tell(device, &text);
            }
        }
        let text = message.text.trim();
        if text.is_empty() {
            let waiting = self.bindings.get(device).map_or(0, |b| b.files.len());
            if waiting > 0 {
                self.tell(
                    device,
                    &tr(
                        lang,
                        "已收到文件。再发一句说明，它们会一起交给会话。",
                        "Got the file. Send a line about it and both go to the session.",
                    ),
                );
            }
            return;
        }
        let reply = message.reply_to.as_ref().and_then(|id| {
            self.bindings
                .get(device)
                .and_then(|binding| binding.replies.get(id).cloned())
        });
        if let Some(reply) = reply {
            self.answer_reply(device, reply, text);
            return;
        }
        if let Some((command, argument)) = command_of(text) {
            self.command(device, command, argument);
            return;
        }
        self.send_text(device, text);
    }

    fn refused(&self, device: &str, what: &str) {
        self.hub.connectors.audit(
            &self.connector,
            "refused",
            json!({ "device": device, "what": what }),
        );
    }

    fn receive_file(&mut self, device: &str, file: &Attachment) -> Result<(), String> {
        let limit = self.api.limits().file_in;
        let bytes = self.api.download(file, limit)?;
        let hub = Arc::clone(&self.hub);
        let path = self
            .hub
            .uploads
            .save(&file.name, &bytes, || hub.new_id("u"))?;
        let image = crate::uploads::is_image(&path);
        if let Some(binding) = self.bindings.get_mut(device) {
            binding.files.push((path, image));
        }
        Ok(())
    }

    /// Sends an op as this binding's client. With `wait`, waits that long
    /// for the reply (frames read meanwhile go to the backlog); without,
    /// takes only a reply that came at once (an error), else Null.
    fn call(&mut self, device: &str, mut op: Value, wait: Duration) -> Result<Value, String> {
        self.next_request += 1;
        let id = format!("chat-{}", self.next_request);
        op["id"] = json!(id);
        let client = self.bindings.get(device).ok_or("not bound")?.client;
        // Revoked meanwhile: nothing more goes out as this binding.
        if self.hub.store.device(device).is_none() {
            if let Some(binding) = self.bindings.get_mut(device) {
                binding.gone = true;
            }
            return Err("this binding was revoked".to_string());
        }
        crate::handle(&self.hub, client, &op.to_string());
        let deadline = Instant::now() + wait;
        let binding = self.bindings.get_mut(device).ok_or("not bound")?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let received = if left.is_zero() {
                binding
                    .inbox
                    .try_recv()
                    .map_err(|error| error == TryRecvError::Disconnected)
            } else {
                binding
                    .inbox
                    .recv_timeout(left)
                    .map_err(|error| error == RecvTimeoutError::Disconnected)
            };
            let text = match received {
                Ok(text) => text,
                Err(true) => {
                    binding.gone = true;
                    return Err("this binding was revoked".to_string());
                }
                Err(false) if wait.is_zero() => return Ok(Value::Null),
                Err(false) => return Err("the daemon did not answer in time".to_string()),
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame["id"] == id.as_str() {
                return if frame["type"] == "error" {
                    Err(frame["message"].as_str().unwrap_or("error").to_string())
                } else {
                    Ok(frame)
                };
            }
            binding.backlog.push_back(frame);
        }
    }

    /// The binding's current session, while it still exists.
    fn current(&mut self, device: &str) -> Option<String> {
        let current = self.bindings.get(device)?.current.clone()?;
        let exists = self.hub.store.sessions().iter().any(|r| r.id == current);
        if exists {
            Some(current)
        } else {
            self.set_current(device, None);
            None
        }
    }

    fn set_current(&mut self, device: &str, session: Option<&str>) {
        if let Some(binding) = self.bindings.get_mut(device) {
            binding.current = session.map(str::to_string);
            binding.listed.clear();
        }
        self.hub
            .connectors
            .set_current(&self.connector, device, session);
    }

    /// Watches `session` for this binding (and stops watching the one
    /// before): while it watches, approvals wait for an answer.
    fn watch(&mut self, device: &str, session: &str) -> Result<(), String> {
        let before = self.bindings.get(device).and_then(|b| b.watching.clone());
        if before.as_deref() == Some(session) {
            return Ok(());
        }
        if let Some(before) = before {
            let _ = self.call(
                device,
                json!({ "op": "unwatch", "session": before }),
                Duration::ZERO,
            );
        }
        if let Some(binding) = self.bindings.get_mut(device) {
            binding.watching = None;
        }
        self.call(
            device,
            json!({ "op": "watch", "session": session }),
            Duration::from_secs(60),
        )?;
        if let Some(binding) = self.bindings.get_mut(device) {
            binding.watching = Some(session.to_string());
        }
        Ok(())
    }

    /// Refused, in the binding's language, unless `session` is a LynShen
    /// session (the chat profile checks the same for every op).
    fn lynshen_only(&self, device: &str, session: &str) -> Result<(), String> {
        if profile::lynshen_session(&self.hub, session) {
            return Ok(());
        }
        self.refused(device, "engine");
        Err(other_engine(self.lang(device)))
    }

    /// Watches `session` and checks that it is a LynShen session in an
    /// approval mode within what this connector lets a chat drive.
    fn drive(&mut self, device: &str, session: &str) -> Result<(), String> {
        let lang = self.lang(device);
        self.lynshen_only(device, session)?;
        self.watch(device, session)?;
        let cap = self.settings().max_mode;
        let mode = self.hub.messages.wait_for_mode(session).ok_or_else(|| {
            tr(
                lang,
                "会话还没准备好，请稍后再试。",
                "The session is not ready yet; try again shortly.",
            )
        })?;
        if rank(&mode) > rank(&cap) {
            self.refused(device, "mode");
            return Err(match lang {
                Lang::Zh => format!(
                    "这个会话是「{}」模式，超过了聊天里允许的最高模式「{}」。请在电脑或手机远程页把它调低，或用 /new 新建一个会话。",
                    mode_name(&mode, lang),
                    mode_name(&cap, lang)
                ),
                Lang::En => format!(
                    "This session runs in {}, looser than {}, the most a chat may drive. Lower it on the desktop or the phone page, or start one with /new.",
                    mode_name(&mode, lang),
                    mode_name(&cap, lang)
                ),
            });
        }
        Ok(())
    }

    /// Starts a session (in `cwd`, or a chat session) and makes it the
    /// current one, in no looser a mode than the connector allows.
    fn create(&mut self, device: &str, cwd: Option<&Path>) -> Result<String, String> {
        let op = match cwd {
            Some(cwd) => json!({ "op": "session_create", "cwd": cwd }),
            None => json!({ "op": "session_create", "chat": true }),
        };
        let reply = self.call(device, op, Duration::from_secs(60))?;
        let session = reply["session"]
            .as_str()
            .ok_or("the daemon did not start a session")?
            .to_string();
        self.set_current(device, Some(&session));
        self.watch(device, &session)?;
        let cap = self.settings().max_mode;
        let mode = self.hub.messages.wait_for_mode(&session);
        if mode.as_deref().is_none_or(|mode| rank(mode) > rank(&cap)) {
            self.call(
                device,
                json!({ "op": "set_approval_mode", "session": session, "mode": cap }),
                Duration::ZERO,
            )?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.hub.messages.mode(&session).as_deref() != Some(cap.as_str())
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(session)
    }

    /// Plain text: the user's next message in the current session (a new
    /// chat session when there is none), with the files that wait.
    fn send_text(&mut self, device: &str, text: &str) {
        let lang = self.lang(device);
        let session = match self.current(device) {
            Some(session) => session,
            None => match self.create(device, None) {
                Ok(session) => session,
                Err(error) => {
                    self.tell(device, &failed(lang, &error));
                    return;
                }
            },
        };
        if let Err(error) = self.drive(device, &session) {
            self.tell(device, &error);
            return;
        }
        let files = self
            .bindings
            .get_mut(device)
            .map(|b| std::mem::take(&mut b.files))
            .unwrap_or_default();
        let (content, images) = compose(text, &files);
        let busy = self.hub.is_busy(&session);
        let op = json!({ "op": "user_message", "session": session, "content": content, "images": images });
        match self.call(device, op, Duration::ZERO) {
            Ok(_) => {
                if let Some(binding) = self.bindings.get_mut(device) {
                    binding
                        .sent
                        .push_back((session.clone(), content, Instant::now()));
                }
                if busy {
                    self.tell(
                        device,
                        &tr(
                            lang,
                            "会话正在忙，这条消息会在当前任务结束后开始。",
                            "The session is busy; this message starts when the current task ends.",
                        ),
                    );
                }
            }
            Err(error) => {
                self.tell(device, &failed(lang, &error));
            }
        }
    }

    fn answer_reply(&mut self, device: &str, reply: Reply, text: &str) {
        let lang = self.lang(device);
        let result = match &reply {
            Reply::Question(question) => {
                let asking = self
                    .hub
                    .store
                    .question(question)
                    .map(|q| q.session)
                    .unwrap_or_default();
                self.lynshen_only(device, &asking)
                    .and_then(|()| {
                        self.call(
                            device,
                            json!({ "op": "question_answer", "question": question, "answer": text }),
                            Duration::from_secs(10),
                        )
                    })
                    .map(|_| tr(lang, "已回答。", "Answered."))
            }
            Reply::Plan { session, plan } => self
                .drive(device, session)
                .and_then(|()| {
                    self.call(
                        device,
                        json!({ "op": "approve_plan", "session": session, "id": plan, "decision": "revise", "feedback": text }),
                        Duration::ZERO,
                    )
                })
                .map(|_| tr(lang, "已请它修改计划。", "Asked for a revised plan.")),
        };
        let text = result.unwrap_or_else(|error| failed(lang, &error));
        self.tell(device, &text);
    }

    fn command(&mut self, device: &str, command: Command, argument: &str) {
        let lang = self.lang(device);
        self.hub.connectors.audit(
            &self.connector,
            "command",
            json!({ "device": device, "command": command.name() }),
        );
        let text = match command {
            Command::Help => Some(help(lang)),
            Command::Bind => Some(tr(lang, "这个账号已经绑定了。", "This account is already bound.")),
            Command::New => self.new_session(device, argument),
            Command::ChatNew => Some(match self.create(device, None) {
                Ok(_) => tr(
                    lang,
                    "已新建聊天会话（不带项目文件）。直接发消息开始。",
                    "Started a chat session (no project files). Send a message to begin.",
                ),
                Err(error) => failed(lang, &error),
            }),
            Command::Sessions => Some(self.sessions(device)),
            Command::Use => Some(self.use_session(device, argument)),
            Command::Stop => Some(match self.current(device) {
                Some(session) => match self.call(
                    device,
                    json!({ "op": "interrupt", "session": session }),
                    Duration::ZERO,
                ) {
                    Ok(_) => tr(lang, "已停止。", "Stopped."),
                    Err(error) => failed(lang, &error),
                },
                None => no_session(lang),
            }),
            Command::Status => Some(self.status(device)),
            Command::Mode => Some(self.mode(device, argument)),
            Command::Diff => self.diff(device),
            Command::File => self.file(device, argument),
            Command::Open => Some(match lang {
                Lang::Zh => format!("手机远程页：{REMOTE_PAGE}\n用电脑上的二维码配对后，可以看到完整内容，内容端到端加密。"),
                Lang::En => format!("Phone page: {REMOTE_PAGE}\nPair it with the QR code on the computer to see everything, end to end encrypted."),
            }),
            Command::Allow | Command::Deny => {
                let id = argument.trim().to_ascii_uppercase();
                Some(self.decide(device, &id, command == Command::Allow))
            }
            Command::AllowTurn => {
                let id = argument.trim().to_ascii_uppercase();
                Some(self.allow_rest_of_turn(device, &id))
            }
            Command::Unbind => {
                self.tell(
                    device,
                    &tr(
                        lang,
                        "已解除绑定。之后这个账号的消息不会再交给电脑。",
                        "Unbound. Messages from this account no longer reach the computer.",
                    ),
                );
                let _ = self.hub.revoke_device(device);
                if let Some(binding) = self.bindings.get_mut(device) {
                    binding.gone = true;
                }
                None
            }
            Command::Terminal => {
                self.refused(device, "terminal");
                Some(tr(
                    lang,
                    "聊天里不能打开终端。请在电脑或手机远程页操作。",
                    "A terminal cannot be opened from chat. Use the desktop or the phone page.",
                ))
            }
            Command::Unknown => Some(tr(
                lang,
                "不认识这个命令。发 /help 查看可用命令。",
                "Unknown command. Send /help for the list.",
            )),
        };
        if let Some(text) = text {
            self.tell(device, &text);
        }
    }

    /// Every project, as `/new` numbers them.
    fn projects(&self) -> Vec<(String, PathBuf)> {
        crate::projects::projects(&self.hub)
            .iter()
            .filter_map(|project| {
                let path = PathBuf::from(project["path"].as_str()?);
                let name = project["name"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| path.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_default();
                Some((name, path))
            })
            .collect()
    }

    fn new_session(&mut self, device: &str, argument: &str) -> Option<String> {
        let lang = self.lang(device);
        let projects = self.projects();
        if projects.is_empty() {
            return Some(tr(
                lang,
                "电脑上还没有项目。请先在桌面端添加项目，或用 /chat 新建聊天会话。",
                "There are no projects on the computer yet. Add one on the desktop, or use /chat.",
            ));
        }
        let argument = argument.trim();
        if argument.is_empty() {
            let mut text = tr(lang, "在哪个项目里新建？\n", "Which project?\n");
            for (index, (name, _)) in projects.iter().enumerate() {
                text.push_str(&format!("{}. {name}\n", index + 1));
            }
            text.push_str(&tr(lang, "\n发 /new 编号。", "\nSend /new and its number."));
            let buttons: Vec<Button> = projects
                .iter()
                .enumerate()
                .take(8)
                .map(|(index, (name, _))| Button {
                    label: render::clip(name, 20),
                    data: format!("n:{}", index + 1),
                })
                .collect();
            let chat = self.bindings.get(device)?.chat.clone();
            if let Some(binding) = self.bindings.get_mut(device) {
                binding.projects = projects;
            }
            let buttons = if self.api.limits().buttons {
                buttons
            } else {
                Vec::new()
            };
            self.post(&chat, &text, &buttons);
            return None;
        }
        let listed = self
            .bindings
            .get(device)
            .map(|b| b.projects.clone())
            .filter(|listed| !listed.is_empty())
            .unwrap_or_else(|| projects.clone());
        let lower = argument.to_lowercase();
        let found = argument
            .parse::<usize>()
            .ok()
            .and_then(|n| listed.get(n.wrapping_sub(1)).cloned())
            .or_else(|| {
                projects
                    .iter()
                    .find(|(name, _)| name.to_lowercase() == lower)
                    .or_else(|| {
                        projects
                            .iter()
                            .find(|(name, _)| name.to_lowercase().starts_with(&lower))
                    })
                    .cloned()
            });
        let Some((name, path)) = found else {
            return Some(tr(
                lang,
                "没有这个项目。发 /new 看看有哪些。",
                "No such project. Send /new to see the list.",
            ));
        };
        Some(match self.create(device, Some(&path)) {
            Ok(_) => match lang {
                Lang::Zh => format!("已在「{name}」新建会话。直接发消息开始。"),
                Lang::En => format!("Started a session in {name}. Send a message to begin."),
            },
            Err(error) => failed(lang, &error),
        })
    }

    /// LynShen sessions, newest first, not archived: (id, title, cwd, open).
    /// Sessions of other engines are left out: a chat cannot drive them.
    fn recent(&self) -> Vec<(String, String, PathBuf, bool)> {
        let mut list: Vec<Value> = self.hub.sessions_json()["sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|session| session["archived"] != true && session["engine"] == "lynshen")
            .collect();
        list.sort_by_key(|session| std::cmp::Reverse(session["updated_at"].as_u64().unwrap_or(0)));
        list.into_iter()
            .take(LIST)
            .filter_map(|session| {
                Some((
                    session["session"].as_str()?.to_string(),
                    session["title"].as_str().unwrap_or_default().to_string(),
                    PathBuf::from(session["cwd"].as_str().unwrap_or_default()),
                    session["open"] == true,
                ))
            })
            .collect()
    }

    fn place(&self, cwd: &Path, lang: Lang) -> String {
        if lynshen_agent_core::chat::is_chat_dir(cwd) {
            return tr(lang, "聊天", "chat");
        }
        crate::projects::project_of_cwd(&self.hub, cwd)
            .and_then(|project| project["name"].as_str().map(str::to_string))
            .or_else(|| {
                cwd.file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_default()
    }

    fn state(&self, session: &str, open: bool, lang: Lang) -> String {
        let waiting = self.hub.connectors.waiting(session)
            || self
                .hub
                .store
                .open_actions()
                .iter()
                .any(|action| action.session_id == session);
        match (self.hub.is_busy(session), waiting, open) {
            (_, true, _) => tr(lang, "待确认", "needs approval"),
            (true, _, _) => tr(lang, "运行中", "running"),
            (_, _, true) => tr(lang, "空闲", "idle"),
            _ => tr(lang, "已关闭", "closed"),
        }
    }

    fn sessions(&mut self, device: &str) -> String {
        let lang = self.lang(device);
        let recent = self.recent();
        if recent.is_empty() {
            return tr(
                lang,
                "还没有会话。直接发消息会新建一个。",
                "No sessions yet. Send a message to start one.",
            );
        }
        let current = self.current(device);
        let mut text = tr(lang, "最近的会话：\n", "Recent sessions:\n");
        for (index, (id, title, cwd, open)) in recent.iter().enumerate() {
            let title = if title.is_empty() {
                tr(lang, "未命名", "untitled")
            } else {
                render::clip(title, 40)
            };
            let mark = if current.as_deref() == Some(id.as_str()) {
                tr(lang, "（当前）", " (current)")
            } else {
                String::new()
            };
            text.push_str(&format!(
                "{}. {title}{mark} · {} · {}\n",
                index + 1,
                self.place(cwd, lang),
                self.state(id, *open, lang)
            ));
        }
        text.push_str(&tr(
            lang,
            "\n发 /use 编号 切换。",
            "\nSend /use and a number to switch.",
        ));
        if let Some(binding) = self.bindings.get_mut(device) {
            binding.listed = recent.into_iter().map(|(id, ..)| id).collect();
        }
        text
    }

    fn use_session(&mut self, device: &str, argument: &str) -> String {
        let lang = self.lang(device);
        let listed = self
            .bindings
            .get(device)
            .map(|b| b.listed.clone())
            .filter(|listed| !listed.is_empty())
            .unwrap_or_else(|| self.recent().into_iter().map(|(id, ..)| id).collect());
        let Some(session) = argument
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| listed.get(n.wrapping_sub(1)).cloned())
        else {
            return tr(
                lang,
                "发 /sessions 看编号，再发 /use 编号。",
                "Send /sessions for the numbers, then /use and a number.",
            );
        };
        if let Err(error) = self.lynshen_only(device, &session) {
            return error;
        }
        self.set_current(device, Some(&session));
        let title = self.title(&session, lang);
        match self.drive(device, &session) {
            Ok(()) => match lang {
                Lang::Zh => format!("已切换到「{title}」。"),
                Lang::En => format!("Switched to \"{title}\"."),
            },
            Err(error) => match lang {
                Lang::Zh => format!("已切换到「{title}」，但：{error}"),
                Lang::En => format!("Switched to \"{title}\", but: {error}"),
            },
        }
    }

    fn title(&self, session: &str, lang: Lang) -> String {
        self.hub
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .and_then(|record| crate::hub::shown_title(&record, None))
            .unwrap_or_else(|| tr(lang, "新会话", "new session"))
    }

    fn status(&mut self, device: &str) -> String {
        let lang = self.lang(device);
        let Some(session) = self.current(device) else {
            return no_session(lang);
        };
        let record = self
            .hub
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session);
        let cwd = record.map(|r| r.cwd).unwrap_or_default();
        let mode = self
            .hub
            .messages
            .mode(&session)
            .map(|mode| mode_name(&mode, lang))
            .unwrap_or_else(|| tr(lang, "未知（会话未打开）", "unknown (not open)"));
        let open = self.hub.is_open(&session);
        let waiting = self
            .hub
            .store
            .open_actions()
            .iter()
            .filter(|action| action.session_id == session)
            .count()
            + usize::from(self.hub.connectors.waiting(&session));
        let title = self.title(&session, lang);
        let place = self.place(&cwd, lang);
        let state = self.state(&session, open, lang);
        match lang {
            Lang::Zh => format!("会话：{title}\n项目：{place}\n状态：{state}\n模式：{mode}\n待确认：{waiting} 项"),
            Lang::En => format!("Session: {title}\nProject: {place}\nState: {state}\nMode: {mode}\nWaiting approvals: {waiting}"),
        }
    }

    fn mode(&mut self, device: &str, argument: &str) -> String {
        let lang = self.lang(device);
        let cap = self.settings().max_mode;
        let allowed: Vec<String> = MODES
            .iter()
            .filter(|mode| rank(mode) <= rank(&cap))
            .map(|mode| format!("{mode}（{}）", mode_name(mode, Lang::Zh)))
            .collect();
        let Some(session) = self.current(device) else {
            return no_session(lang);
        };
        if let Err(error) = self.lynshen_only(device, &session) {
            return error;
        }
        let argument = argument.trim();
        if argument.is_empty() {
            let mode = self
                .hub
                .messages
                .mode(&session)
                .map(|mode| mode_name(&mode, lang))
                .unwrap_or_else(|| tr(lang, "未知", "unknown"));
            return match lang {
                Lang::Zh => format!(
                    "当前模式：{mode}\n可以切换到：{}\n发 /mode 模式名。",
                    allowed.join("、")
                ),
                Lang::En => format!(
                    "Mode: {mode}\nYou may switch to: {}\nSend /mode and a mode.",
                    allowed.join(", ")
                ),
            };
        }
        let Some(mode) = parse_mode(argument) else {
            return tr(
                lang,
                "不认识这个模式。可用：plan、manual、auto-edit、auto。",
                "Unknown mode. Use plan, manual, auto-edit or auto.",
            );
        };
        if mode == "full-access" || rank(mode) > rank(&cap) {
            self.refused(device, "full-access");
            return match lang {
                Lang::Zh => format!(
                    "聊天里最高只能切到「{}」。全自动和完全访问只能在电脑或手机远程页打开。",
                    mode_name(&cap, lang)
                ),
                Lang::En => format!(
                    "From chat the mode goes up to {} at most. Full auto and full access are switched on the desktop or the phone page.",
                    mode_name(&cap, lang)
                ),
            };
        }
        let result = self.watch(device, &session).and_then(|()| {
            self.call(
                device,
                json!({ "op": "set_approval_mode", "session": session, "mode": mode }),
                Duration::ZERO,
            )
        });
        match result {
            Ok(_) => match lang {
                Lang::Zh => format!("已切换为「{}」。", mode_name(mode, lang)),
                Lang::En => format!("Switched to {}.", mode_name(mode, lang)),
            },
            Err(error) => failed(lang, &error),
        }
    }

    fn session_cwd(&mut self, device: &str) -> Option<PathBuf> {
        let session = self.current(device)?;
        self.hub
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .map(|record| record.cwd)
    }

    fn diff(&mut self, device: &str) -> Option<String> {
        let lang = self.lang(device);
        let Some(cwd) = self.session_cwd(device) else {
            return Some(no_session(lang));
        };
        let wait = Duration::from_secs(30);
        let status = match self.call(device, json!({ "op": "git_status", "path": cwd }), wait) {
            Ok(status) => status,
            Err(error) => return Some(failed(lang, &error)),
        };
        if status["repo"] != true {
            return Some(tr(
                lang,
                "这个目录不是 git 仓库。",
                "This directory is not a git repository.",
            ));
        }
        let files = status["files"].as_array().cloned().unwrap_or_default();
        if files.is_empty() {
            return Some(tr(lang, "没有改动。", "No changes."));
        }
        let mut text = match lang {
            Lang::Zh => format!(
                "分支 {} · {} 个文件有改动：\n",
                status["branch"].as_str().unwrap_or("?"),
                files.len()
            ),
            Lang::En => format!(
                "Branch {} · {} changed files:\n",
                status["branch"].as_str().unwrap_or("?"),
                files.len()
            ),
        };
        for file in files.iter().take(20) {
            text.push_str(&format!(
                "{} {}\n",
                file["status"].as_str().unwrap_or_default(),
                file["path"].as_str().unwrap_or_default()
            ));
        }
        if files.len() > 20 {
            text.push_str(&format!("… +{}\n", files.len() - 20));
        }
        let chat = self.bindings.get(device)?.chat.clone();
        self.post(&chat, &format!("```\n{}```", text), &[]);
        let diff = self.call(device, json!({ "op": "git_diff", "path": cwd }), wait);
        match diff {
            Ok(diff) => {
                let body = diff["diff"].as_str().unwrap_or_default();
                if body.trim().is_empty() {
                    return None;
                }
                if self.api.limits().file_out == 0 {
                    return Some(tr(
                        lang,
                        "完整的改动请在电脑或手机远程页查看：这个聊天平台还不能接收文件。",
                        "Read the whole diff on the desktop or the phone page: this chat platform takes no files yet.",
                    ));
                }
                let body = redact::redact(body, &self.hub.connectors.secrets());
                let caption = if diff["truncated"] == true {
                    tr(
                        lang,
                        "改动太多，只含前 1 MB。",
                        "Truncated to the first 1 MB.",
                    )
                } else {
                    String::new()
                };
                match self
                    .api
                    .send_file(&chat, "changes.diff", body.as_bytes(), &caption)
                {
                    Ok(()) => None,
                    Err(error) => Some(failed(lang, &error)),
                }
            }
            Err(error) => Some(failed(lang, &error)),
        }
    }

    /// `/file <path>`: a file from the current session's directories, by the
    /// rules of `fs_read`, never one that usually holds secrets.
    fn file(&mut self, device: &str, argument: &str) -> Option<String> {
        let lang = self.lang(device);
        let argument = argument.trim();
        if argument.is_empty() {
            return Some(tr(
                lang,
                "发 /file 加路径，例如 /file src/main.rs。",
                "Send /file and a path, such as /file src/main.rs.",
            ));
        }
        let Some(cwd) = self.session_cwd(device) else {
            return Some(no_session(lang));
        };
        let path = if Path::new(argument).is_absolute() {
            PathBuf::from(argument)
        } else {
            cwd.join(argument)
        };
        let real = match crate::files::known_dir(&self.hub, &path.to_string_lossy()) {
            Ok(real) => real,
            Err(_) => {
                self.refused(device, "file");
                return Some(tr(
                    lang,
                    "这个文件不在项目目录里，或者受保护，不能发送。",
                    "That file is outside the project directories or protected.",
                ));
            }
        };
        let name = real
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if secret_file(&name) {
            self.refused(device, "secret_file");
            return Some(tr(
                lang,
                "这个文件可能含有密钥，不能发到聊天里。",
                "That file may hold secrets; it is not sent to chat.",
            ));
        }
        let limit = self.api.limits().file_out;
        if limit == 0 {
            return Some(tr(
                lang,
                "这个聊天平台还不能接收机器人发的文件，请在电脑或手机远程页查看。",
                "This chat platform takes no files from the bot yet: open it on the desktop or the phone page.",
            ));
        }
        match std::fs::metadata(&real) {
            Ok(meta) if !meta.is_file() => {
                return Some(tr(lang, "这不是文件。", "That is not a file."))
            }
            Ok(meta) if meta.len() > limit => {
                return Some(match lang {
                    Lang::Zh => format!("文件超过 {} MB，不能发送。", limit >> 20),
                    Lang::En => format!("The file is larger than {} MB.", limit >> 20),
                })
            }
            Ok(_) => {}
            Err(error) => return Some(failed(lang, &error.to_string())),
        }
        let bytes = match std::fs::read(&real) {
            Ok(bytes) => bytes,
            Err(error) => return Some(failed(lang, &error.to_string())),
        };
        let chat = self.bindings.get(device)?.chat.clone();
        match self.api.send_file(&chat, &name, &bytes, "") {
            Ok(()) => None,
            Err(error) => Some(failed(lang, &error)),
        }
    }

    fn press(&mut self, press: Press) {
        if !press.private {
            self.api.ack(&press.id, "");
            return;
        }
        // Only a bound account's presses count, checked on every press.
        let Some(device) = self.device_for_peer(&press.peer) else {
            self.api.ack(&press.id, "");
            return;
        };
        self.hub.touch_device(&device);
        if let Some(binding) = self.bindings.get_mut(&device) {
            binding.active = Instant::now();
        }
        let parts: Vec<&str> = press.data.split(':').collect();
        let answer = match parts.as_slice() {
            ["a", id, "t"] => self.allow_rest_of_turn(&device, id),
            ["a", id, answer] => self.decide(&device, id, *answer == "y"),
            ["p", id, answer] => self.plan_press(&device, id, *answer == "y"),
            ["n", index] => {
                if let Some(text) = self.new_session(&device, index) {
                    self.tell(&device, &text);
                }
                String::new()
            }
            _ => String::new(),
        };
        self.api.ack(&press.id, &render::clip(&answer, 180));
    }

    /// Answers a card: an approval request or a deferred action.
    fn decide(&mut self, device: &str, id: &str, allow: bool) -> String {
        let lang = self.lang(device);
        let Some(card) = self.cards.get(id) else {
            return tr(lang, "这个请求已失效。", "This request is no longer open.");
        };
        if card.device != device {
            return tr(lang, "这不是你的确认请求。", "This request is not yours.");
        }
        let (session, target) = (card.session.clone(), card.target.clone());
        if !matches!(target, Target::Plan(_)) {
            if let Err(error) = self.lynshen_only(device, &session) {
                return error;
            }
        }
        let (op, what) = match &target {
            Target::Call(call) => self.decision_op(&session, call, allow),
            Target::Action(action) => {
                let decision = if allow { "allow" } else { "deny" };
                (
                    json!({ "op": "decide_action", "session": session, "action": action, "decision": decision }),
                    json!({ "session": session, "action": action, "decision": decision }),
                )
            }
            Target::Plan(_) => return self.plan_press(device, id, allow),
        };
        if let Err(error) = self.forward_decision(device, op, what) {
            let text = failed(lang, &error);
            self.tell(device, &text);
            return text;
        }
        let outcome = if allow {
            tr(lang, "已允许", "Allowed")
        } else {
            tr(lang, "已拒绝", "Denied")
        };
        self.close_card(id, &outcome);
        outcome
    }

    /// Allows this request and every ordinary approval of its session for the
    /// rest of the turn (`allow_turn`): the same reach as the per-request
    /// 允许, held only until the turn ends. Never offered for an elevation,
    /// a command that leaves the sandbox, or a rule that must ask, so it can
    /// never approve `sudo` or a boundary crossing — those stay on the
    /// desktop or the phone page.
    fn allow_rest_of_turn(&mut self, device: &str, id: &str) -> String {
        let lang = self.lang(device);
        let Some(card) = self.cards.get(id) else {
            return tr(lang, "这个请求已失效。", "This request is no longer open.");
        };
        if card.device != device {
            return tr(lang, "这不是你的确认请求。", "This request is not yours.");
        }
        let (session, target) = (card.session.clone(), card.target.clone());
        let Target::Call(call) = target else {
            // Only an ordinary approval request has a turn; a plan or a
            // deferred action falls back to a plain allow.
            return self.decide(device, id, true);
        };
        if let Err(error) = self.lynshen_only(device, &session) {
            return error;
        }
        let (op, what) = self.decision_op(&session, &call, true);
        if let Err(error) = self.forward_decision(device, op, what) {
            let text = failed(lang, &error);
            self.tell(device, &text);
            return text;
        }
        if let Some(binding) = self.bindings.get_mut(device) {
            binding.allow_turn.insert(session.clone());
        }
        self.hub
            .connectors
            .audit(&self.connector, "allow_turn", json!({ "session": session }));
        let outcome = tr(lang, "本轮都允许", "Allowed for this turn");
        self.close_card(id, &outcome);
        outcome
    }

    /// The `approve` op for a call and the line to audit.
    fn decision_op(&self, session: &str, call: &str, allow: bool) -> (Value, Value) {
        let decision = if allow { "allow" } else { "deny" };
        (
            json!({ "op": "approve", "session": session, "call_id": call, "decision": decision }),
            json!({ "session": session, "call": call, "decision": decision }),
        )
    }

    /// Sends a decision op to the daemon and audits it; a forwarded op's
    /// silence is the usual answer, not an error.
    fn forward_decision(&mut self, device: &str, op: Value, what: Value) -> Result<(), String> {
        self.call(device, op, Duration::from_millis(200))
            .or_else(|error| {
                if error == "the daemon did not answer in time" {
                    Ok(Value::Null)
                } else {
                    Err(error)
                }
            })?;
        self.hub.connectors.audit(&self.connector, "decide", what);
        Ok(())
    }

    fn plan_press(&mut self, device: &str, id: &str, approve: bool) -> String {
        let lang = self.lang(device);
        let Some(card) = self.cards.get(id) else {
            return tr(lang, "这个计划已处理。", "This plan was already handled.");
        };
        if card.device != device {
            return tr(lang, "这不是你的计划。", "This plan is not yours.");
        }
        let (session, Target::Plan(plan)) = (card.session.clone(), card.target.clone()) else {
            return String::new();
        };
        if !approve {
            return tr(
                lang,
                "回复这条计划消息，写下要修改的地方。",
                "Reply to the plan message with what to change.",
            );
        }
        let cap = self.settings().max_mode;
        let mode = if rank(&cap) >= rank("auto-edit") {
            "auto-edit".to_string()
        } else {
            cap.clone()
        };
        if mode == "plan" {
            return tr(
                lang,
                "这个机器人最高只允许计划模式，不能批准执行。",
                "This bot allows plan mode only; the plan cannot run from chat.",
            );
        }
        let result = self.drive(device, &session).and_then(|()| {
            self.call(
                device,
                json!({ "op": "approve_plan", "session": session, "id": plan, "decision": "approve", "mode": mode }),
                Duration::ZERO,
            )
        });
        match result {
            Ok(_) => {
                self.hub.connectors.audit(
                    &self.connector,
                    "plan",
                    json!({ "session": session, "plan": plan, "decision": "approve" }),
                );
                let outcome = tr(lang, "已批准，开始执行", "Approved, running");
                self.close_card(id, &outcome);
                outcome
            }
            Err(error) => {
                let text = failed(lang, &error);
                self.tell(device, &text);
                text
            }
        }
    }

    /// Takes a card's buttons away and says how it ended.
    fn close_card(&mut self, id: &str, outcome: &str) {
        if let Some(card) = self.cards.remove(id) {
            if let Some(message) = &card.message {
                self.edit(
                    &card.chat,
                    message,
                    &format!("{}\n\n{outcome}", card.text),
                    &[],
                );
            }
        }
    }

    /// A new card for `device`, with buttons as the connector allows.
    /// `turn_ok`: this request may offer "本轮都允许" (an ordinary call of a
    /// session a turn is running in), so the user can clear a burst of
    /// approvals at once.
    #[allow(clippy::too_many_arguments)]
    fn card(
        &mut self,
        device: &str,
        session: &str,
        target: Target,
        heading: &str,
        kind: &str,
        what: &str,
        reason: Option<&str>,
        turn_ok: bool,
    ) {
        let lang = self.lang(device);
        let Some(chat) = self.bindings.get(device).map(|b| b.chat.clone()) else {
            return;
        };
        let Ok(id) = crate::hub::new_code(5) else {
            return;
        };
        let title = self.title(session, lang);
        let mut text = format!(
            "{heading} · {title}\n{}\n{}",
            kind_words(kind, lang),
            render::clip(what, CARD_TEXT)
        );
        if let Some(reason) = reason.filter(|reason| !reason.trim().is_empty()) {
            text.push_str(&format!(
                "\n{}{}",
                tr(lang, "理由：", "Reason: "),
                render::clip(reason, 300)
            ));
        }
        let settings = self.settings();
        let allow = Button {
            label: tr(lang, "允许", "Allow"),
            data: format!("a:{id}:y"),
        };
        let allow_turn = Button {
            label: tr(lang, "本轮都允许", "Allow this turn"),
            data: format!("a:{id}:t"),
        };
        let deny = Button {
            label: tr(lang, "拒绝", "Deny"),
            data: format!("a:{id}:n"),
        };
        let (buttons, footer) = if !settings.approvals {
            (
                Vec::new(),
                tr(
                    lang,
                    "聊天里的审批已关闭，请在电脑或手机远程页处理。",
                    "Approvals from chat are off: decide on the desktop or the phone page.",
                ),
            )
        } else if kind == "elevation" {
            (
                vec![deny],
                match lang {
                    Lang::Zh => format!("提权（sudo 等）不能在聊天里批准，只能拒绝；批准请到电脑或手机远程页（{REMOTE_PAGE}）。编号 {id}。"),
                    Lang::En => format!("An elevation (sudo and the like) cannot be approved from chat, only denied; approve it on the desktop or the phone page ({REMOTE_PAGE}). Id {id}."),
                },
            )
        } else if turn_ok {
            (
                vec![allow, allow_turn, deny],
                match lang {
                    Lang::Zh => format!("编号 {id}：也可以回复「允许 {id}」「本轮 {id}」或「拒绝 {id}」。「本轮都允许」只在这一轮里、对普通命令生效，不含提权。"),
                    Lang::En => format!("Id {id}: reply \"/allow {id}\", \"/allowturn {id}\" or \"/deny {id}\". \"Allow this turn\" covers ordinary commands of this turn only, never an elevation."),
                },
            )
        } else {
            (
                vec![allow, deny],
                match lang {
                    Lang::Zh => format!("编号 {id}：也可以回复「允许 {id}」或「拒绝 {id}」。"),
                    Lang::En => {
                        format!("Id {id}: you can also send \"/allow {id}\" or \"/deny {id}\".")
                    }
                },
            )
        };
        let buttons = if self.api.limits().buttons {
            buttons
        } else {
            Vec::new()
        };
        let shown = format!("{text}\n\n{footer}");
        let message = self.post(&chat, &shown, &buttons);
        if settings.approvals {
            self.cards.insert(
                id,
                Card {
                    device: device.to_string(),
                    session: session.to_string(),
                    target,
                    chat,
                    message,
                    text,
                    created: Instant::now(),
                },
            );
        }
    }

    fn notice(&mut self, title: &str, body: &str, tag: &str) {
        if !self.settings().notify {
            return;
        }
        self.notices.retain(|_, at| at.elapsed() < NOTICE_EVERY);
        if self.notices.contains_key(tag) {
            return;
        }
        self.notices.insert(tag.to_string(), Instant::now());
        let chats: Vec<String> = self
            .bindings
            .values()
            .filter(|b| !b.gone)
            .map(|b| b.chat.clone())
            .collect();
        for chat in chats {
            self.post(&chat, &format!("{title}\n{}", render::clip(body, 600)), &[]);
        }
    }

    /// Frames from the hub for each binding.
    fn pump(&mut self) -> bool {
        let mut busy = false;
        let devices: Vec<String> = self.bindings.keys().cloned().collect();
        for device in devices {
            for _ in 0..FRAMES_PER_ROUND {
                let next = {
                    let Some(binding) = self.bindings.get_mut(&device) else {
                        break;
                    };
                    match binding.backlog.pop_front() {
                        Some(frame) => frame,
                        None => match binding.inbox.try_recv() {
                            Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                binding.gone = true;
                                break;
                            }
                        },
                    }
                };
                busy = true;
                self.frame(&device, next);
            }
        }
        let gone: Vec<String> = self
            .bindings
            .iter()
            .filter(|(_, b)| b.gone)
            .map(|(id, _)| id.clone())
            .collect();
        for device in gone {
            self.drop_binding(&device);
        }
        busy
    }

    fn frame(&mut self, device: &str, frame: Value) {
        let kind = frame["type"].as_str().unwrap_or_default().to_string();
        let session = frame["session"].as_str().unwrap_or_default().to_string();
        let (lang, watching, turn_session) = match self.bindings.get(device) {
            Some(b) => (
                b.lang,
                b.watching.clone(),
                b.turn.as_ref().map(|t| t.session.clone()),
            ),
            None => return,
        };
        let in_turn = !session.is_empty() && turn_session.as_deref() == Some(session.as_str());
        match kind.as_str() {
            // A reply to one of this binding's ops that it did not wait for.
            "error"
                if frame["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("chat-")) =>
            {
                let text = failed(lang, frame["message"].as_str().unwrap_or_default());
                self.tell(device, &text);
            }
            "error" if in_turn => {
                if let Some(turn) = self.turn_mut(device) {
                    turn.error = frame["message"].as_str().map(str::to_string);
                    turn.active = true;
                }
            }
            "user_message" => {
                let content = frame["content"].as_str().unwrap_or_default();
                self.turn_started(device, &session, content);
            }
            "thinking_start" | "reasoning_delta" if in_turn => {
                self.progress(device, tr(lang, "思考中", "Thinking"));
            }
            "assistant_start" if in_turn => {
                if let Some(turn) = self.turn_mut(device) {
                    turn.reply.clear();
                }
                self.progress(device, tr(lang, "正在回复", "Replying"));
            }
            "assistant_delta" if in_turn => {
                if let Some(turn) = self.turn_mut(device) {
                    turn.reply
                        .push_str(frame["delta"].as_str().unwrap_or_default());
                    turn.active = true;
                }
            }
            "response_interrupted" if in_turn => {
                if let Some(turn) = self.turn_mut(device) {
                    turn.reply.clear();
                }
            }
            "tool_start" if in_turn => {
                let step = tool_step(frame["name"].as_str().unwrap_or_default(), lang);
                self.progress(device, step);
            }
            "compaction_start" if in_turn => {
                self.progress(device, tr(lang, "整理上下文", "Compacting the context"));
            }
            "plan" if in_turn => {
                if let Some(step) = plan_step(&frame["plan"], lang) {
                    self.progress(device, step);
                }
            }
            "status" => {
                let message = frame["message"].as_str().unwrap_or_default();
                if matches!(message, "ready" | "interrupted") {
                    let stopped = message == "interrupted";
                    let active = self
                        .bindings
                        .get(device)
                        .and_then(|b| b.turn.as_ref())
                        .is_some_and(|t| t.active);
                    if in_turn && (stopped || active) {
                        self.finish(
                            device,
                            if stopped {
                                Ending::Stopped
                            } else {
                                Ending::Done
                            },
                        );
                    }
                    // The turn is over: "本轮都允许" lapses, so the next turn
                    // asks again.
                    if let Some(binding) = self.bindings.get_mut(device) {
                        binding.allow_turn.remove(&session);
                    }
                    self.resolve_calls(&session, None, &tr(lang, "已结束", "Ended"));
                }
            }
            "approval_request" if watching.as_deref() == Some(session.as_str()) => {
                let call = frame["call_id"].as_str().unwrap_or_default().to_string();
                let what = frame["command"]
                    .as_str()
                    .filter(|command| !command.is_empty())
                    .or(frame["summary"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let kind = frame["kind"].as_str().unwrap_or("tool").to_string();
                // "本轮都允许" earlier this turn: approve this one too, with
                // no card. Only an ordinary approval is covered — an
                // elevation, a boundary crossing or a rule that must ask
                // still gets its own card.
                let covered = self
                    .bindings
                    .get(device)
                    .is_some_and(|b| b.allow_turn.contains(&session))
                    && turn_allowable(&kind, &frame);
                if covered {
                    let (op, audit) = self.decision_op(&session, &call, true);
                    if self.forward_decision(device, op, audit).is_ok() {
                        return;
                    }
                }
                let heading = tr(lang, "需要确认", "Approval needed");
                let turn_ok = turn_allowable(&kind, &frame);
                self.card(
                    device,
                    &session,
                    Target::Call(call),
                    &heading,
                    &kind,
                    &what,
                    frame["justification"].as_str(),
                    turn_ok,
                );
            }
            "tool_output" | "action_deferred" => {
                let call = frame["call_id"].as_str().unwrap_or_default();
                let outcome = if kind == "action_deferred" {
                    tr(lang, "已转为待处理事项", "Became a pending action")
                } else {
                    tr(lang, "已在其他设备处理", "Handled on another device")
                };
                self.resolve_calls(&session, Some(call), &outcome);
            }
            "actions" => self.actions(device, &frame),
            "questions" => self.questions(device, &frame),
            "proposed_plan" if frame["status"] == "pending" => {
                let current = self.bindings.get(device).and_then(|b| b.current.clone());
                if current.as_deref() == Some(session.as_str()) {
                    self.plan(device, &session, &frame);
                }
            }
            "session_closed" => {
                if let Some(binding) = self.bindings.get_mut(device) {
                    if binding.watching.as_deref() == Some(session.as_str()) {
                        binding.watching = None;
                    }
                    binding.allow_turn.remove(&session);
                }
                if turn_session.as_deref() == Some(session.as_str()) {
                    self.finish(device, Ending::Closed);
                }
            }
            _ => {}
        }
    }

    fn turn_mut(&mut self, device: &str) -> Option<&mut Turn> {
        self.bindings.get_mut(device)?.turn.as_mut()
    }

    fn progress(&mut self, device: &str, step: String) {
        if let Some(turn) = self.turn_mut(device) {
            turn.step = step;
            turn.active = true;
        }
    }

    /// A session started a user message: when it is one this binding sent,
    /// its turn is followed in a status message.
    fn turn_started(&mut self, device: &str, session: &str, content: &str) {
        let (chat, lang) = {
            let Some(binding) = self.bindings.get_mut(device) else {
                return;
            };
            let Some(position) = binding
                .sent
                .iter()
                .position(|(s, c, _)| s == session && c == content)
            else {
                return;
            };
            binding.sent.drain(..=position);
            (binding.chat.clone(), binding.lang)
        };
        // Queued messages run back to back: the one before is done.
        self.finish(device, Ending::Done);
        let step = tr(lang, "思考中", "Thinking");
        // Without edits a status message could only grow stale.
        let status = if self.api.limits().edits {
            self.post(&chat, &format!("{step}…"), &[])
        } else {
            None
        };
        if let Some(binding) = self.bindings.get_mut(device) {
            binding.turn = Some(Turn {
                session: session.to_string(),
                status,
                started: Instant::now(),
                shown: step.clone(),
                step,
                edited: Instant::now(),
                reply: String::new(),
                error: None,
                active: false,
                notes: 0,
            });
        }
    }

    /// The end of a turn this binding started: the reply as new messages
    /// (so the phone notifies), then the status message says how it ended.
    fn finish(&mut self, device: &str, ending: Ending) {
        let Some(binding) = self.bindings.get_mut(device) else {
            return;
        };
        let Some(turn) = binding.turn.take() else {
            return;
        };
        let (chat, lang) = (binding.chat.clone(), binding.lang);
        let took = render::duration(turn.started.elapsed().as_secs(), lang == Lang::Zh);
        let reply = turn.reply.trim();
        let status = match ending {
            Ending::Stopped => format!("{} · {took}", tr(lang, "已停止", "Stopped")),
            Ending::Closed => tr(lang, "会话已关闭", "The session closed"),
            Ending::Done if reply.is_empty() && turn.error.is_some() => format!(
                "{}：{}",
                tr(lang, "出错", "Failed"),
                render::clip(turn.error.as_deref().unwrap_or_default(), 300)
            ),
            Ending::Done => format!(
                "{} · {}{took}",
                tr(lang, "完成", "Done"),
                tr(lang, "用时 ", "")
            ),
        };
        if !reply.is_empty() {
            self.post_long(&chat, reply, lang);
        }
        if let Some(message) = &turn.status {
            self.edit(&chat, message, &status, &[]);
        }
    }

    /// Cards of `session` (one call, or all) that ended without this chat.
    fn resolve_calls(&mut self, session: &str, call: Option<&str>, outcome: &str) {
        let ids: Vec<String> = self
            .cards
            .iter()
            .filter(|(_, card)| {
                card.session == session
                    && matches!(&card.target, Target::Call(c) if call.is_none_or(|call| call == c))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.close_card(&id, outcome);
        }
    }

    /// The daemon's deferred actions: new ones are announced with buttons,
    /// cards of the ones decided elsewhere close.
    fn actions(&mut self, device: &str, frame: &Value) {
        let open: Vec<Value> = frame["actions"].as_array().cloned().unwrap_or_default();
        let ids: HashSet<String> = open
            .iter()
            .filter_map(|action| action["id"].as_str().map(str::to_string))
            .collect();
        let stale: Vec<String> = self
            .cards
            .iter()
            .filter(|(_, card)| {
                card.device == device
                    && matches!(&card.target, Target::Action(a) if !ids.contains(a))
            })
            .map(|(id, _)| id.clone())
            .collect();
        let lang = self.lang(device);
        for id in stale {
            self.close_card(&id, &tr(lang, "已处理", "Handled"));
        }
        let fresh: Vec<Value> = {
            let Some(binding) = self.bindings.get_mut(device) else {
                return;
            };
            let fresh = open
                .into_iter()
                .filter(|action| {
                    action["id"]
                        .as_str()
                        .is_some_and(|id| !binding.actions.contains(id))
                })
                .collect();
            binding.actions = ids;
            fresh
        };
        if !self.settings().notify {
            return;
        }
        for action in fresh {
            let session = action["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if !profile::lynshen_session(&self.hub, &session) {
                continue;
            }
            let what = format!(
                "{} {}",
                action["name"].as_str().unwrap_or_default(),
                action["summary"].as_str().unwrap_or_default()
            );
            let heading = tr(lang, "待处理事项", "Pending action");
            // A deferred action has no running turn to scope "本轮都允许" to.
            self.card(
                device,
                &session,
                Target::Action(action["id"].as_str().unwrap_or_default().to_string()),
                &heading,
                action["kind"].as_str().unwrap_or("tool"),
                &what,
                action["justification"].as_str(),
                false,
            );
        }
    }

    /// Questions agents asked: a reply to the message answers one.
    fn questions(&mut self, device: &str, frame: &Value) {
        let open: Vec<Value> = frame["questions"].as_array().cloned().unwrap_or_default();
        let fresh: Vec<Value> = {
            let Some(binding) = self.bindings.get_mut(device) else {
                return;
            };
            let ids: HashSet<String> = open
                .iter()
                .filter_map(|q| q["id"].as_str().map(str::to_string))
                .collect();
            let fresh = open
                .into_iter()
                .filter(|q| {
                    q["id"]
                        .as_str()
                        .is_some_and(|id| !binding.questions.contains(id))
                })
                .collect();
            binding.questions = ids;
            fresh
        };
        if !self.settings().notify {
            return;
        }
        let lang = self.lang(device);
        for question in fresh {
            let asking = question["session"].as_str().unwrap_or_default();
            if !profile::lynshen_session(&self.hub, asking) {
                continue;
            }
            let mut text = format!(
                "{}{}\n{}",
                tr(lang, "问题：", "Question: "),
                question["title"].as_str().unwrap_or_default(),
                question["body"].as_str().unwrap_or_default()
            );
            if let Some(default) = question["default"].as_str().filter(|d| !d.is_empty()) {
                text.push_str(&format!(
                    "\n{}{default}",
                    tr(lang, "不回答时：", "If unanswered: ")
                ));
            }
            text.push_str(&tr(
                lang,
                "\n\n回复这条消息作答。",
                "\n\nReply to this message to answer.",
            ));
            if let Some(message) = self.tell(device, &text) {
                let id = question["id"].as_str().unwrap_or_default().to_string();
                if let Some(binding) = self.bindings.get_mut(device) {
                    binding.replies.insert(message, Reply::Question(id));
                }
            }
        }
    }

    /// A plan of the current session: 批准 / 修改, and a reply revises it.
    fn plan(&mut self, device: &str, session: &str, frame: &Value) {
        let lang = self.lang(device);
        let Some(chat) = self.bindings.get(device).map(|b| b.chat.clone()) else {
            return;
        };
        let plan = frame["id"].as_str().unwrap_or_default().to_string();
        let text = format!(
            "{}{}\n\n{}",
            tr(lang, "计划：", "Plan: "),
            frame["title"].as_str().unwrap_or_default(),
            frame["markdown"].as_str().unwrap_or_default()
        );
        let messages = self.post_long(&chat, &text, lang);
        let Ok(id) = crate::hub::new_code(5) else {
            return;
        };
        let buttons = vec![
            Button {
                label: tr(lang, "批准", "Approve"),
                data: format!("p:{id}:y"),
            },
            Button {
                label: tr(lang, "修改", "Revise"),
                data: format!("p:{id}:n"),
            },
        ];
        let prompt = tr(
            lang,
            "批准后开始执行；要修改，回复计划消息写下意见。",
            "Approve to run it; to change it, reply to the plan message.",
        );
        let buttons = if self.api.limits().buttons {
            buttons
        } else {
            Vec::new()
        };
        let message = self.post(&chat, &prompt, &buttons);
        if let Some(binding) = self.bindings.get_mut(device) {
            for message in messages.iter().chain(message.iter()) {
                binding.replies.insert(
                    message.clone(),
                    Reply::Plan {
                        session: session.to_string(),
                        plan: plan.clone(),
                    },
                );
            }
        }
        self.cards.insert(
            id,
            Card {
                device: device.to_string(),
                session: session.to_string(),
                target: Target::Plan(plan),
                chat,
                message,
                text: prompt,
                created: Instant::now(),
            },
        );
    }

    /// Where messages cannot be edited: a line every `NOTE_EVERY` while a
    /// turn of this binding runs.
    fn note_running(&mut self, device: &str) {
        let note = {
            let Some(binding) = self.bindings.get_mut(device) else {
                return;
            };
            let (chat, lang) = (binding.chat.clone(), binding.lang);
            let Some(turn) = binding.turn.as_mut() else {
                return;
            };
            let due = turn.started.elapsed() >= NOTE_EVERY * (turn.notes + 1);
            if !due {
                return;
            }
            turn.notes += 1;
            let minutes = turn.started.elapsed().as_secs() / 60;
            let text = match lang {
                Lang::Zh => format!("仍在运行（已 {minutes} 分钟）：{}", turn.step),
                Lang::En => format!("Still running ({minutes} min): {}", turn.step),
            };
            (chat, text)
        };
        self.post(&note.0, &note.1, &[]);
    }

    /// Timed work: progress edits, unwatching an idle chat's session,
    /// expiring cards.
    fn tick(&mut self) {
        let devices: Vec<String> = self.bindings.keys().cloned().collect();
        for device in &devices {
            let edit = {
                let Some(binding) = self.bindings.get_mut(device) else {
                    continue;
                };
                binding.sent.retain(|(_, _, at)| at.elapsed() < ACTIVE);
                let lang = binding.lang;
                let chat = binding.chat.clone();
                binding.turn.as_mut().and_then(|turn| {
                    let due = turn.step != turn.shown && turn.edited.elapsed() >= EDIT_EVERY;
                    let message = turn.status.clone().filter(|_| due)?;
                    turn.shown = turn.step.clone();
                    turn.edited = Instant::now();
                    let took = render::duration(turn.started.elapsed().as_secs(), lang == Lang::Zh);
                    Some((chat, message, format!("{}… · {took}", turn.step)))
                })
            };
            if let Some((chat, message, text)) = edit {
                self.edit(&chat, &message, &text, &[]);
            }
            if !self.api.limits().edits {
                self.note_running(device);
            }
            let idle = self.bindings.get(device).and_then(|binding| {
                let idle = binding.turn.is_none()
                    && binding.sent.is_empty()
                    && binding.active.elapsed() >= ACTIVE;
                binding.watching.clone().filter(|_| idle)
            });
            if let Some(session) = idle {
                let _ = self.call(
                    device,
                    json!({ "op": "unwatch", "session": session }),
                    Duration::ZERO,
                );
                if let Some(binding) = self.bindings.get_mut(device) {
                    binding.watching = None;
                }
            }
        }
        let expired: Vec<String> = self
            .cards
            .iter()
            .filter(|(_, card)| card.created.elapsed() >= CARD_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let lang = self
                .cards
                .get(&id)
                .map_or(Lang::Zh, |card| self.lang(&card.device));
            self.close_card(
                &id,
                &tr(
                    lang,
                    "已过期，请在电脑或手机远程页处理。",
                    "Expired: decide on the desktop or the phone page.",
                ),
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Command {
    Help,
    Bind,
    New,
    ChatNew,
    Sessions,
    Use,
    Stop,
    Status,
    Mode,
    Diff,
    File,
    Open,
    Allow,
    AllowTurn,
    Deny,
    Unbind,
    Terminal,
    Unknown,
}

impl Command {
    fn name(self) -> &'static str {
        match self {
            Command::Help => "help",
            Command::Bind => "bind",
            Command::New => "new",
            Command::ChatNew => "chat",
            Command::Sessions => "sessions",
            Command::Use => "use",
            Command::Stop => "stop",
            Command::Status => "status",
            Command::Mode => "mode",
            Command::Diff => "diff",
            Command::File => "file",
            Command::Open => "open",
            Command::Allow => "allow",
            Command::AllowTurn => "allowturn",
            Command::Deny => "deny",
            Command::Unbind => "unbind",
            Command::Terminal => "terminal",
            Command::Unknown => "unknown",
        }
    }
}

/// A command and its argument. `/name` (with `@bot` after it, as group
/// menus send it), or a Chinese word on its own, with at most one word
/// after it, so a sentence that starts with 停止 is still a message.
fn command_of(text: &str) -> Option<(Command, &str)> {
    let (first, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(first, rest)| (first, rest.trim()));
    if let Some(name) = first.strip_prefix('/') {
        let name = name
            .split('@')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let command = match name.as_str() {
            "start" | "help" => Command::Help,
            "bind" => Command::Bind,
            "new" => Command::New,
            "chat" => Command::ChatNew,
            "sessions" => Command::Sessions,
            "use" => Command::Use,
            "stop" => Command::Stop,
            "status" => Command::Status,
            "mode" => Command::Mode,
            "diff" => Command::Diff,
            "file" => Command::File,
            "open" => Command::Open,
            "allow" => Command::Allow,
            "allowturn" | "allow_turn" => Command::AllowTurn,
            "deny" => Command::Deny,
            "unbind" => Command::Unbind,
            "terminal" | "term" | "shell" | "sudo" | "tui" => Command::Terminal,
            _ => Command::Unknown,
        };
        return Some((command, rest));
    }
    let (command, takes_argument) = match first {
        "帮助" => (Command::Help, false),
        "绑定" => (Command::Bind, true),
        "新建" => (Command::New, true),
        "聊天" => (Command::ChatNew, false),
        "会话" => (Command::Sessions, false),
        "切换" => (Command::Use, true),
        "停止" => (Command::Stop, false),
        "状态" => (Command::Status, false),
        "模式" => (Command::Mode, true),
        "允许" => (Command::Allow, true),
        "本轮" => (Command::AllowTurn, true),
        "拒绝" => (Command::Deny, true),
        "解绑" => (Command::Unbind, false),
        "终端" => (Command::Terminal, false),
        _ => return None,
    };
    let fits = if takes_argument {
        !rest.contains(char::is_whitespace)
    } else {
        rest.is_empty()
    };
    fits.then_some((command, rest))
}

/// The code in `/start <code>`, `/bind <code>` or `绑定 <code>`.
fn bind_code(text: &str) -> Option<String> {
    let mut words = text.split_whitespace();
    let first = words.next()?;
    let command = first
        .strip_prefix('/')
        .map(|name| name.split('@').next().unwrap_or_default());
    let binds = matches!(command, Some("start" | "bind")) || first == "绑定";
    if !binds {
        return None;
    }
    words.next().map(|code| code.to_ascii_uppercase())
}

fn parse_mode(text: &str) -> Option<&'static str> {
    Some(match text.trim().to_lowercase().as_str() {
        "plan" | "计划" => "plan",
        "manual" | "谨慎" | "ask" | "手动" => "manual",
        "auto-edit" | "autoedit" | "edits" | "自动编辑" | "编辑" => "auto-edit",
        "auto" | "自动" => "auto",
        "full-access" | "full" | "full-auto" | "yolo" | "bypass" | "bypasspermissions"
        | "全自动" | "完全访问" => "full-access",
        _ => return None,
    })
}

fn mode_name(mode: &str, lang: Lang) -> String {
    let (zh, en) = match mode {
        "plan" => ("计划", "plan"),
        "manual" | "read-only" | "default" => ("谨慎", "careful (manual)"),
        "auto-edit" | "acceptEdits" => ("自动编辑", "auto-edit"),
        "auto" => ("自动", "auto"),
        "full-access" | "full-auto" | "bypassPermissions" => ("全自动", "full access"),
        other => (other, other),
    };
    tr(lang, zh, en)
}

/// Whether "本轮都允许" covers this approval request: an ordinary command
/// or tool, never an elevation, a command that leaves the sandbox, a write
/// outside the workspace, or a request a person must decide (an `ask`
/// rule). This keeps the turn scope to the same reach as the per-request
/// 允许, and well clear of elevation and boundary crossings.
fn turn_allowable(kind: &str, frame: &Value) -> bool {
    if frame["must_ask"].as_bool() == Some(true) {
        return false;
    }
    // Each phone action is decided on its own (approved once at most).
    if frame["name"] == lynshen_agent_core::android::ACT_TOOL {
        return false;
    }
    matches!(kind, "command" | "edit" | "tool")
}

fn kind_words(kind: &str, lang: Lang) -> String {
    let (zh, en) = match kind {
        "edit" => ("修改文件", "Edit files"),
        "command" => ("运行命令", "Run a command"),
        "escalation" => ("越界命令（离开沙箱）", "Command outside the sandbox"),
        "outside_write" => ("写工作区外文件", "Write outside the workspace"),
        "elevation" => ("提权（sudo 等）", "Elevation (sudo and the like)"),
        "rule" => ("规则要求确认", "A rule asks first"),
        _ => ("调用工具", "Use a tool"),
    };
    tr(lang, zh, en)
}

fn tool_step(name: &str, lang: Lang) -> String {
    let (zh, en) = match name {
        "bash" | "shell" | "exec_command" | "write_stdin" | "Bash" => {
            ("运行命令", "Running a command")
        }
        "edit" | "write" | "apply_patch" | "multi_edit" | "Edit" | "Write" => {
            ("修改文件", "Editing files")
        }
        "read" | "view" | "read_file" | "Read" => ("读取文件", "Reading files"),
        "grep" | "glob" | "search" | "list_dir" | "Grep" | "Glob" => {
            ("查找代码", "Searching the code")
        }
        "web_search" | "WebSearch" => ("搜索网页", "Searching the web"),
        "web_fetch" | "WebFetch" => ("读取网页", "Reading a web page"),
        "spawn_agent" | "Task" => ("启动子任务", "Starting a subtask"),
        _ => {
            return match lang {
                Lang::Zh => format!("调用 {name}"),
                Lang::En => format!("Using {name}"),
            }
        }
    };
    tr(lang, zh, en)
}

/// The plan step in progress: `第 2/5 步：…`.
fn plan_step(plan: &Value, lang: Lang) -> Option<String> {
    let steps = plan.as_array()?;
    let (index, step) = steps
        .iter()
        .enumerate()
        .find(|(_, step)| step["status"] == "in_progress")?;
    let text = render::clip(step["step"].as_str().unwrap_or_default(), 60);
    Some(match lang {
        Lang::Zh => format!("第 {}/{} 步：{text}", index + 1, steps.len()),
        Lang::En => format!("Step {}/{}: {text}", index + 1, steps.len()),
    })
}

/// The message and its files as the engine reads them (as the desktop
/// sends attachments): images in `images`, other files listed after it.
fn compose(text: &str, files: &[(PathBuf, bool)]) -> (String, Vec<String>) {
    let images = files
        .iter()
        .filter(|(_, image)| *image)
        .map(|(path, _)| path.display().to_string())
        .collect();
    let others: Vec<String> = files
        .iter()
        .filter(|(_, image)| !*image)
        .map(|(path, _)| path.display().to_string())
        .collect();
    let content = if others.is_empty() {
        text.to_string()
    } else {
        format!(
            "{text}\n\nAttached files (read these):\n{}",
            others.join("\n")
        )
    };
    (content, images)
}

/// Names of files that usually hold secrets.
fn secret_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with(".env")
        || name.starts_with("id_")
        || name.starts_with("credentials")
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"]
            .iter()
            .any(|ext| name.ends_with(ext))
        || matches!(
            name.as_str(),
            ".npmrc" | ".pypirc" | ".netrc" | ".git-credentials"
        )
}

fn help(lang: Lang) -> String {
    tr(
        lang,
        "可用命令：\n/new [编号或项目名]：在项目里新建会话\n/chat：新建聊天会话（不带项目文件）\n/sessions：最近的会话\n/use 编号：切换当前会话\n/stop：停止当前任务\n/status：当前会话状态\n/mode [模式]：查看或切换权限模式\n/diff：当前项目的改动\n/file 路径：发送项目里的文件\n/open：手机远程页地址\n/unbind：解除绑定\n\n直接发文字：交给当前会话。发图片或文件：随下一句话一起交给会话。\n全自动、始终允许、sudo 提权和终端只能在电脑或手机远程页操作。",
        "Commands:\n/new [number or project]: new session in a project\n/chat: new chat session (no project files)\n/sessions: recent sessions\n/use number: switch session\n/stop: stop the current task\n/status: session status\n/mode [mode]: show or set the approval mode\n/diff: changes in the project\n/file path: send a project file\n/open: phone page address\n/unbind: unbind this account\n\nPlain text goes to the current session. Photos and files go with your next message.\nFull auto, always-allow, sudo and terminals are only on the desktop or the phone page.",
    )
}

/// A session of another engine (Claude Code, Codex, an ACP agent): its
/// modes and approvals mean something else, so chat leaves it alone.
fn other_engine(lang: Lang) -> String {
    tr(
        lang,
        "这个会话不是 LynShen 引擎的会话（例如 Claude Code、Codex 或 OpenCode），聊天里不能操作它：不能发消息、切换模式、批准或回答。请在电脑或手机远程页使用它，或发 /sessions 选一个 LynShen 会话，或用 /new 新建一个。",
        "This session does not run on the LynShen engine (it is a Claude Code, Codex or OpenCode session), so chat cannot message it, switch its mode, approve or answer in it. Use it on the desktop or the phone page, pick a LynShen session with /sessions, or start one with /new.",
    )
}

fn no_session(lang: Lang) -> String {
    tr(
        lang,
        "还没有当前会话。发 /sessions 选一个，或直接发消息新建。",
        "No current session. Pick one with /sessions, or send a message to start one.",
    )
}

fn failed(lang: Lang, error: &str) -> String {
    match lang {
        Lang::Zh => format!("出错：{error}"),
        Lang::En => format!("Failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_for_the_turn_never_covers_a_phone_action() {
        let call = |name: &str| json!({ "name": name, "kind": "tool", "must_ask": false });
        assert!(turn_allowable("tool", &call("mcp__files__read")));
        assert!(!turn_allowable("tool", &call("android_act")));
    }

    #[test]
    fn commands_need_a_slash_or_a_whole_chinese_word() {
        assert_eq!(command_of("/new 2"), Some((Command::New, "2")));
        assert_eq!(
            command_of("/sessions@my_bot"),
            Some((Command::Sessions, ""))
        );
        assert_eq!(command_of("/whatever x"), Some((Command::Unknown, "x")));
        assert_eq!(command_of("/sudo rm"), Some((Command::Terminal, "rm")));
        assert_eq!(command_of("停止"), Some((Command::Stop, "")));
        assert_eq!(command_of("切换 3"), Some((Command::Use, "3")));
        assert_eq!(command_of("允许 K7QX2"), Some((Command::Allow, "K7QX2")));
        // A sentence is a message, not a command.
        assert_eq!(command_of("停止 这个方案不好"), None);
        assert_eq!(command_of("模式 改成 自动"), None);
        assert_eq!(command_of("fix the tests"), None);
    }

    #[test]
    fn binding_codes_come_from_start_bind_or_the_chinese_word() {
        assert_eq!(bind_code("/start ab3dk7xq").as_deref(), Some("AB3DK7XQ"));
        assert_eq!(
            bind_code("/start@my_bot AB3DK7XQ").as_deref(),
            Some("AB3DK7XQ")
        );
        assert_eq!(bind_code("/bind AB3DK7XQ").as_deref(), Some("AB3DK7XQ"));
        assert_eq!(bind_code("绑定 AB3DK7XQ").as_deref(), Some("AB3DK7XQ"));
        assert_eq!(bind_code("/start"), None);
        assert_eq!(bind_code("bind AB3DK7XQ"), None);
        assert_eq!(bind_code("hello"), None);
    }

    #[test]
    fn modes_are_named_and_full_access_is_recognized() {
        assert_eq!(parse_mode("自动"), Some("auto"));
        assert_eq!(parse_mode("全自动"), Some("full-access"));
        assert_eq!(parse_mode("YOLO"), Some("full-access"));
        assert_eq!(parse_mode("nonsense"), None);
        assert_eq!(mode_name("auto", Lang::Zh), "自动");
        assert_eq!(mode_name("bypassPermissions", Lang::En), "full access");
    }

    #[test]
    fn files_go_as_the_desktop_sends_them_and_secret_names_stay_home() {
        let files = vec![
            (PathBuf::from("/u/a.png"), true),
            (PathBuf::from("/u/b.pdf"), false),
        ];
        let (content, images) = compose("看看", &files);
        assert_eq!(content, "看看\n\nAttached files (read these):\n/u/b.pdf");
        assert_eq!(images, vec!["/u/a.png".to_string()]);
        assert_eq!(compose("hi", &[]).0, "hi");
        for name in [
            ".env",
            ".env.local",
            "server.pem",
            "id_ed25519",
            "Credentials.json",
            ".npmrc",
        ] {
            assert!(secret_file(name), "{name}");
        }
        for name in ["main.rs", "README.md", "keyboard.ts"] {
            assert!(!secret_file(name), "{name}");
        }
    }

    #[test]
    fn progress_names_the_step() {
        assert_eq!(tool_step("bash", Lang::Zh), "运行命令");
        assert_eq!(tool_step("mcp__x", Lang::En), "Using mcp__x");
        let plan = json!([
            { "step": "读代码", "status": "completed" },
            { "step": "改测试", "status": "in_progress" },
            { "step": "提交", "status": "pending" }
        ]);
        assert_eq!(
            plan_step(&plan, Lang::Zh).as_deref(),
            Some("第 2/3 步：改测试")
        );
        assert_eq!(plan_step(&json!([]), Lang::Zh), None);
    }
}

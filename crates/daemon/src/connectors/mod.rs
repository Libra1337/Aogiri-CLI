//! Chat connectors (docs/chat-connectors.md): bots the user created on a
//! chat platform, connected out from the daemon. An account bound to a bot
//! is a restricted device; `profile` limits what it may do, `chat` turns its
//! messages into ops and the sessions' events into messages, and each
//! platform module speaks its platform's API.
//!
//! This module keeps the connectors (`connectors.json`, credentials sealed
//! like auth.json's), their progress (`connectors-state.json`), the audit
//! log (`connectors.jsonl`), binding codes, and the threads of each
//! connector: one receives from the platform with backoff, one runs the
//! chat side.

mod chat;
mod profile;
mod redact;
mod render;
mod telegram;

pub(crate) use profile::check as check_profile;

use crate::{
    hub::{lock, Hub},
    store::{now, random_hex, write_private, ChatPeer, Device},
};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const FILE: &str = "connectors.json";
const STATE_FILE: &str = "connectors-state.json";
const AUDIT_FILE: &str = "connectors.jsonl";
/// How long a binding code stays valid.
const CODE_TTL_MS: u64 = 5 * 60 * 1000;
/// Wrong codes one sender may try within `WRONG_WINDOW_MS`.
const MAX_WRONG: usize = 5;
const WRONG_WINDOW_MS: u64 = 10 * 60 * 1000;
/// How long a sender who tried too many wrong codes is ignored.
const LOCKOUT_MS: u64 = 60 * 60 * 1000;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A connection this long lived is stable: the next failure starts the
/// backoff over.
const STABLE: Duration = Duration::from_secs(30);
/// The loosest approval modes a binding may use, from the strictest.
pub(crate) const MODES: [&str; 4] = ["plan", "manual", "auto-edit", "auto"];

/// What every binding of a connector may do.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Settings {
    /// Bindings may allow or deny ordinary approval requests.
    pub approvals: bool,
    /// Bindings get notifications.
    pub notify: bool,
    /// The loosest approval mode a binding may use or drive (`MODES`).
    pub max_mode: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            approvals: true,
            notify: true,
            max_mode: "auto".to_string(),
        }
    }
}

impl Settings {
    fn from_json(value: &Value) -> Self {
        let default = Self::default();
        Self {
            approvals: value["approvals"].as_bool().unwrap_or(default.approvals),
            notify: value["notify"].as_bool().unwrap_or(default.notify),
            max_mode: value["max_mode"]
                .as_str()
                .filter(|mode| MODES.contains(mode))
                .map_or(default.max_mode, str::to_string),
        }
    }

    fn to_json(&self) -> Value {
        json!({ "approvals": self.approvals, "notify": self.notify, "max_mode": self.max_mode })
    }
}

/// One bot.
#[derive(Debug, Clone)]
pub(crate) struct Connector {
    pub id: String,
    pub platform: String,
    /// The bot as people see it (`@name`).
    pub name: String,
    /// The bot's user name, for links (`t.me/<username>`).
    pub username: String,
    /// The platform's id of the bot: one connector per bot.
    pub bot_id: String,
    pub enabled: bool,
    /// None: the environment's proxy; `direct`; or an `http://` URL.
    pub proxy: Option<String>,
    /// Another API origin (a local Bot API server, a test server).
    pub api: Option<String>,
    /// The token in memory; empty when it could not be read.
    pub token: String,
    /// The token as saved (plaintext or a `jcenc1:` envelope).
    sealed: String,
    pub settings: Settings,
    pub created_at: u64,
}

impl Connector {
    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
        let sealed = value["credentials"]["token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let token = lynshen_agent_core::open_secret(&sealed).unwrap_or_default();
        Some(Self {
            id: value["id"].as_str()?.to_string(),
            platform: value["platform"].as_str()?.to_string(),
            name: text("name"),
            username: text("username"),
            bot_id: text("bot_id"),
            enabled: value["enabled"].as_bool().unwrap_or(true),
            proxy: value["proxy"].as_str().map(str::to_string),
            api: value["api"].as_str().map(str::to_string),
            token,
            sealed,
            settings: Settings::from_json(&value["settings"]),
            created_at: value["created_at"].as_u64().unwrap_or_default(),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "platform": self.platform,
            "name": self.name,
            "username": self.username,
            "bot_id": self.bot_id,
            "enabled": self.enabled,
            "proxy": self.proxy,
            "api": self.api,
            "credentials": { "token": self.sealed },
            "settings": self.settings.to_json(),
            "created_at": self.created_at,
        })
    }

    /// Where the bot is opened in the app, for the desktop's link and QR.
    fn link(&self) -> Option<String> {
        (self.platform == "telegram" && !self.username.is_empty())
            .then(|| format!("https://t.me/{}", self.username))
    }

    /// The origin requests go to.
    pub(crate) fn api_origin(&self) -> String {
        self.api
            .clone()
            .unwrap_or_else(|| telegram::ORIGIN.to_string())
    }
}

/// A connector's running threads.
struct Running {
    stop: Arc<AtomicBool>,
    events: Sender<chat::Event>,
}

/// Why a connection to the platform ended.
pub(crate) enum Fault {
    /// Try again after the backoff.
    Retry(String),
    /// Another program uses this bot: try again after a minute.
    Conflict(String),
    /// The platform refused the credentials: wait until they change.
    Rejected(String),
}

/// What happened to a binding code.
#[derive(Debug, PartialEq)]
pub(crate) enum Redeemed {
    Bound,
    /// No code waits for this connector: the attempt is ignored.
    NoCode,
    Wrong,
    /// The sender tried too many wrong codes.
    Locked,
}

pub struct Connectors {
    dir: PathBuf,
    list: Mutex<Vec<Connector>>,
    /// `connectors-state.json`: `{<connector>: {offset, current: {<device>: <session>}}}`.
    state: Mutex<Value>,
    /// Each connector's connection state and its last error.
    status: Mutex<HashMap<String, (String, Option<String>)>>,
    running: Mutex<HashMap<String, Running>>,
    /// Binding code → (connector, expiry).
    codes: Mutex<HashMap<String, (String, u64)>>,
    /// `<connector>:<peer>` → when it tried wrong codes.
    wrong: Mutex<HashMap<String, Vec<u64>>>,
    /// `<connector>:<peer>` → until when its attempts are ignored.
    locked: Mutex<HashMap<String, u64>>,
    /// Approval requests waiting now: (session, call id) → kind.
    calls: Mutex<HashMap<(String, String), String>>,
}

impl Connectors {
    pub fn load(dir: &std::path::Path) -> Self {
        let read = |file: &str| {
            fs::read_to_string(dir.join(file))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        };
        let list = read(FILE)
            .and_then(|value| value["connectors"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(Connector::from_json)
            .collect();
        let state = read(STATE_FILE)
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        Self {
            dir: dir.to_path_buf(),
            list: Mutex::new(list),
            state: Mutex::new(state),
            status: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
            wrong: Mutex::new(HashMap::new()),
            locked: Mutex::new(HashMap::new()),
            calls: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<Connector> {
        lock(&self.list).iter().find(|c| c.id == id).cloned()
    }

    pub(crate) fn settings(&self, id: &str) -> Option<Settings> {
        self.get(id).map(|connector| connector.settings)
    }

    /// Every connector's credentials, for the redaction filter.
    pub(crate) fn secrets(&self) -> Vec<String> {
        lock(&self.list)
            .iter()
            .map(|c| c.token.clone())
            .filter(|token| !token.is_empty())
            .collect()
    }

    fn save(&self, list: &[Connector]) -> Result<(), String> {
        let doc = json!({ "connectors": list.iter().map(Connector::to_json).collect::<Vec<_>>() });
        write_private(&self.dir.join(FILE), format!("{doc:#}\n").as_bytes())
            .map_err(|error| error.to_string())
    }

    /// Changes one connector and saves the list.
    fn update(&self, id: &str, change: impl FnOnce(&mut Connector)) -> Result<Connector, String> {
        let mut list = lock(&self.list);
        let connector = list
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or_else(|| format!("unknown connector {id}"))?;
        change(connector);
        let changed = connector.clone();
        self.save(&list)?;
        Ok(changed)
    }

    /// Appends a line to the audit log: what happened, never message content.
    pub(crate) fn audit(&self, connector: &str, event: &str, detail: Value) {
        let mut entry = json!({ "at": now(), "connector": connector, "event": event });
        if let (Some(entry), Value::Object(detail)) = (entry.as_object_mut(), detail) {
            entry.extend(detail);
        }
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(AUDIT_FILE))
            .and_then(|mut file| file.write_all(format!("{entry}\n").as_bytes()));
        if let Err(error) = written {
            lynshen_agent_core::log_warn!(
                "daemon",
                "connector audit not written",
                error = error.to_string()
            );
        }
    }

    fn with_state<T>(
        &self,
        connector: &str,
        change: impl FnOnce(&mut Map<String, Value>) -> T,
    ) -> T {
        let mut state = lock(&self.state);
        if !state[connector].is_object() {
            state[connector] = json!({});
        }
        let result = change(
            state[connector]
                .as_object_mut()
                .expect("an object, set above"),
        );
        let text = format!("{:#}\n", *state);
        if let Err(error) = write_private(&self.dir.join(STATE_FILE), text.as_bytes()) {
            lynshen_agent_core::log_warn!(
                "daemon",
                "connector state not saved",
                error = error.to_string()
            );
        }
        result
    }

    /// Where the connector's update stream continues (Telegram's offset).
    pub(crate) fn offset(&self, connector: &str) -> i64 {
        lock(&self.state)[connector]["offset"].as_i64().unwrap_or(0)
    }

    pub(crate) fn set_offset(&self, connector: &str, offset: i64) {
        if self.offset(connector) != offset {
            self.with_state(connector, |state| {
                state.insert("offset".to_string(), json!(offset));
            });
        }
    }

    /// A binding's current session.
    pub(crate) fn current(&self, connector: &str, device: &str) -> Option<String> {
        lock(&self.state)[connector]["current"][device]
            .as_str()
            .map(str::to_string)
    }

    pub(crate) fn set_current(&self, connector: &str, device: &str, session: Option<&str>) {
        self.with_state(connector, |state| {
            let current = state.entry("current").or_insert_with(|| json!({}));
            match session {
                Some(session) => current[device] = json!(session),
                None => {
                    if let Some(map) = current.as_object_mut() {
                        map.remove(device);
                    }
                }
            }
        });
    }

    /// A new binding code for `connector`: 8 characters, 5 minutes, single
    /// use. An earlier code of the same connector stops working.
    fn new_code(&self, connector: &str) -> Result<(String, u64), String> {
        let code = crate::hub::new_code(8)?;
        let expires_at = now() + CODE_TTL_MS;
        let mut codes = lock(&self.codes);
        codes.retain(|_, (owner, expiry)| *expiry > now() && owner != connector);
        codes.insert(code.clone(), (connector.to_string(), expires_at));
        Ok((code, expires_at))
    }

    /// Whether a binding code waits for `connector`.
    pub(crate) fn code_pending(&self, connector: &str) -> bool {
        lock(&self.codes)
            .values()
            .any(|(owner, expiry)| owner == connector && *expiry > now())
    }

    /// Checks a code a sender typed. A wrong one counts against the sender:
    /// after `MAX_WRONG` within `WRONG_WINDOW_MS` it is ignored for
    /// `LOCKOUT_MS`.
    pub(crate) fn redeem(&self, connector: &str, peer: &str, code: &str) -> Redeemed {
        let key = format!("{connector}:{peer}");
        let at = now();
        if lock(&self.locked)
            .get(&key)
            .is_some_and(|until| *until > at)
        {
            return Redeemed::Locked;
        }
        if !self.code_pending(connector) {
            return Redeemed::NoCode;
        }
        let code = code.trim().to_ascii_uppercase();
        let valid = {
            let mut codes = lock(&self.codes);
            let valid = codes
                .get(&code)
                .is_some_and(|(owner, expiry)| owner == connector && *expiry > at);
            if valid {
                codes.remove(&code);
            }
            valid
        };
        if valid {
            lock(&self.wrong).remove(&key);
            return Redeemed::Bound;
        }
        let mut wrong = lock(&self.wrong);
        let tries = wrong.entry(key.clone()).or_default();
        tries.retain(|when| at.saturating_sub(*when) < WRONG_WINDOW_MS);
        tries.push(at);
        if tries.len() >= MAX_WRONG {
            tries.clear();
            lock(&self.locked).insert(key, at + LOCKOUT_MS);
            self.audit(connector, "locked", json!({ "peer": peer }));
            return Redeemed::Locked;
        }
        Redeemed::Wrong
    }

    /// Every session event: the approval requests that wait now, so a chat
    /// client can only answer one that exists, and never an elevation.
    pub fn observe(&self, session: &str, event: &Value) {
        let call = event["call_id"].as_str().map(str::to_string);
        match event["type"].as_str().unwrap_or_default() {
            "approval_request" => {
                if let Some(call) = call {
                    let kind = event["kind"].as_str().unwrap_or("tool").to_string();
                    let mut calls = lock(&self.calls);
                    // Calls nobody answered pile up only in a session that never
                    // ends a turn: keep the map small.
                    if calls.len() > 1000 {
                        calls.clear();
                    }
                    calls.insert((session.to_string(), call), kind);
                }
            }
            "tool_output" | "action_deferred" => {
                if let Some(call) = call {
                    lock(&self.calls).remove(&(session.to_string(), call));
                }
            }
            "status" if event["message"] == "interrupted" => {
                lock(&self.calls).retain(|(owner, _), _| owner != session);
            }
            _ => {}
        }
    }

    /// The kind of an approval request that waits in `session`.
    pub(crate) fn pending_kind(&self, session: &str, call: &str) -> Option<String> {
        lock(&self.calls)
            .get(&(session.to_string(), call.to_string()))
            .cloned()
    }

    /// Whether an approval request waits in `session`.
    pub(crate) fn waiting(&self, session: &str) -> bool {
        lock(&self.calls).keys().any(|(owner, _)| owner == session)
    }

    pub fn session_closed(&self, session: &str) {
        lock(&self.calls).retain(|(owner, _), _| owner != session);
    }

    /// A notification for every binding that wants them (see `push`).
    pub fn notify(&self, title: &str, body: &str, tag: &str) {
        for running in lock(&self.running).values() {
            let _ = running.events.send(chat::Event::Notice {
                title: title.to_string(),
                body: body.to_string(),
                tag: tag.to_string(),
            });
        }
    }

    /// A binding was revoked (on the desktop, or `/unbind`).
    pub fn revoked(&self, device: &Device, chat: &ChatPeer) {
        self.set_current(&chat.connector, &device.id, None);
        self.audit(
            &chat.connector,
            "revoked",
            json!({ "device": device.id, "peer": chat.peer }),
        );
    }

    fn set_status(&self, hub: &Hub, connector: &str, state: &str, error: Option<String>) {
        let next = (state.to_string(), error);
        let changed = lock(&self.status).insert(connector.to_string(), next.clone()) != Some(next);
        if changed {
            hub.broadcast_local(&self.list_json(hub));
        }
    }

    fn stop(&self, connector: &str) {
        if let Some(running) = lock(&self.running).remove(connector) {
            running.stop.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn list_json(&self, hub: &Hub) -> Value {
        let list = lock(&self.list).clone();
        let status = lock(&self.status).clone();
        let devices = hub.store.devices();
        let connectors: Vec<Value> = list
            .iter()
            .map(|connector| {
                let bindings: Vec<Value> = devices
                    .iter()
                    .filter(|device| {
                        !device.revoked
                            && device
                                .chat
                                .as_ref()
                                .is_some_and(|chat| chat.connector == connector.id)
                    })
                    .map(|device| {
                        json!({
                            "device": device.id,
                            "name": device.name,
                            "paired_at": device.paired_at,
                            "last_seen": hub.last_seen(&device.id),
                        })
                    })
                    .collect();
                let (state, error) = match status.get(&connector.id) {
                    Some((state, error)) => (state.clone(), error.clone()),
                    None if connector.enabled => ("connecting".to_string(), None),
                    None => ("stopped".to_string(), None),
                };
                let (proxy, source) =
                    proxy_for(connector.proxy.as_deref(), &connector.api_origin());
                json!({
                    "id": connector.id,
                    "platform": connector.platform,
                    "name": connector.name,
                    "link": connector.link(),
                    "enabled": connector.enabled,
                    "proxy": connector.proxy,
                    "proxy_source": source,
                    "proxy_shown": proxy.as_deref().map(shown_proxy),
                    "state": state,
                    "error": error,
                    "settings": connector.settings.to_json(),
                    "bindings": bindings,
                    "created_at": connector.created_at,
                })
            })
            .collect();
        json!({ "type": "connectors", "connectors": connectors })
    }
}

/// The proxy a connection to `url` goes through, and where that choice comes
/// from (`setting`, `environment` or `none`). The environment is the
/// engine's proxy setting (HTTPS_PROXY and the like, as web_fetch reads it);
/// a SOCKS proxy there is passed over, as the engine does.
pub(crate) fn proxy_for(setting: Option<&str>, url: &str) -> (Option<String>, &'static str) {
    match setting {
        Some("direct") => (None, "none"),
        Some(proxy) => (Some(proxy.to_string()), "setting"),
        None => match lynshen_agent_core::env_proxy(url) {
            Some(proxy) if !proxy.to_ascii_lowercase().starts_with("socks") => {
                (Some(proxy), "environment")
            }
            _ => (None, "none"),
        },
    }
}

/// A proxy URL without its user name and password.
pub(crate) fn shown_proxy(proxy: &str) -> String {
    match proxy.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
            format!("{scheme}://{host}")
        }
        None => proxy
            .rsplit_once('@')
            .map_or(proxy, |(_, host)| host)
            .to_string(),
    }
}

/// A blocking HTTP agent through `proxy`.
pub(crate) fn http_agent(proxy: Option<&str>, read: Duration) -> Result<ureq::Agent, String> {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(read)
        .timeout_write(Duration::from_secs(120));
    if let Some(proxy) = proxy {
        let parsed = ureq::Proxy::new(proxy)
            .map_err(|_| format!("the proxy {} is not a valid URL", shown_proxy(proxy)))?;
        builder = builder.proxy(parsed);
    }
    Ok(builder.build())
}

/// A `proxy` field: absent or empty (the environment), `direct`, or an
/// `http://host:port` URL.
fn proxy_setting(value: &Value) -> Result<Option<String>, String> {
    let Some(proxy) = value.as_str().map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if proxy == "direct" {
        return Ok(Some(proxy.to_string()));
    }
    if !proxy.starts_with("http://") || ureq::Proxy::new(proxy).is_err() {
        return Err("proxy must be an http://host:port URL or direct (SOCKS proxies are not supported; use the HTTP port of Clash and the like)".to_string());
    }
    Ok(Some(proxy.trim_end_matches('/').to_string()))
}

/// An `api` field: another origin for the platform's API.
fn api_setting(value: &Value) -> Result<Option<String>, String> {
    match value.as_str().map(str::trim).filter(|api| !api.is_empty()) {
        None => Ok(None),
        Some(api) if api.starts_with("https://") || api.starts_with("http://") => {
            Ok(Some(api.trim_end_matches('/').to_string()))
        }
        Some(_) => Err("api must be an http:// or https:// URL".to_string()),
    }
}

/// Saves a token as config.json asks (sealed or plaintext).
fn seal(token: &str) -> Result<String, String> {
    // Unit tests never read the user's config.json or secret key.
    if cfg!(test) {
        return Ok(token.to_string());
    }
    lynshen_agent_core::seal_secret(token)
}

/// The connector ops (local clients only; `lib.rs` checks that).
pub(crate) fn handle(hub: &Arc<Hub>, name: &str, op: &Value) -> Result<Value, String> {
    let connector = || {
        op["connector"]
            .as_str()
            .filter(|id| hub.connectors.get(id).is_some())
            .ok_or_else(|| format!("{name} requires a known connector"))
    };
    match name {
        "connector_list" => Ok(hub.connectors.list_json(hub)),
        "connector_add" => add(hub, op),
        "connector_set" => set(hub, connector()?, op),
        "connector_remove" => remove(hub, connector()?),
        "connector_bind_start" => {
            let id = connector()?;
            let found = hub.connectors.get(id).ok_or("unknown connector")?;
            if !found.enabled {
                return Err("turn the connector on first".to_string());
            }
            let (code, expires_at) = hub.connectors.new_code(id)?;
            let link = found.link().map(|link| format!("{link}?start={code}"));
            Ok(json!({
                "type": "connector_bind_code",
                "connector": id,
                "code": code,
                "expires_at": expires_at,
                "link": link,
            }))
        }
        _ => Err(format!("unknown op {name}")),
    }
}

fn add(hub: &Arc<Hub>, op: &Value) -> Result<Value, String> {
    let platform = op["platform"].as_str().unwrap_or_default();
    if platform != "telegram" {
        return Err(format!("unknown platform {platform}: use telegram"));
    }
    let token = op["token"].as_str().unwrap_or_default().trim().to_string();
    telegram::check_token(&token)?;
    let proxy = proxy_setting(&op["proxy"])?;
    let api = api_setting(&op["api"])?;
    let mut connector = Connector {
        id: format!("tg-{}", random_hex(4).map_err(|error| error.to_string())?),
        platform: platform.to_string(),
        name: String::new(),
        username: String::new(),
        bot_id: String::new(),
        enabled: true,
        proxy,
        api,
        sealed: seal(&token)?,
        token,
        settings: Settings::default(),
        created_at: now(),
    };
    let bot = telegram::Api::new(&connector)?.me()?;
    if lock(&hub.connectors.list)
        .iter()
        .any(|c| c.platform == platform && c.bot_id == bot.id)
    {
        return Err("this bot is already added".to_string());
    }
    connector.name = format!("@{}", bot.username);
    connector.username = bot.username;
    connector.bot_id = bot.id;
    {
        let mut list = lock(&hub.connectors.list);
        list.push(connector.clone());
        hub.connectors.save(&list)?;
    }
    hub.connectors
        .audit(&connector.id, "added", json!({ "platform": platform }));
    start(hub, &connector.id);
    let list = hub.connectors.list_json(hub);
    hub.broadcast_local(&list);
    let item = list["connectors"]
        .as_array()
        .and_then(|all| all.iter().find(|c| c["id"] == connector.id.as_str()))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({ "type": "connector_added", "connector": item }))
}

fn set(hub: &Arc<Hub>, id: &str, op: &Value) -> Result<Value, String> {
    let before = hub.connectors.get(id).ok_or("unknown connector")?;
    let proxy = match op.get("proxy") {
        Some(value) => Some(proxy_setting(value)?),
        None => None,
    };
    let token = match op["token"].as_str().map(str::trim) {
        Some(token) => {
            telegram::check_token(token)?;
            Some((token.to_string(), seal(token)?))
        }
        None => None,
    };
    let max_mode = match op["max_mode"].as_str() {
        Some(mode) if MODES.contains(&mode) => Some(mode.to_string()),
        Some(mode) => {
            return Err(format!(
                "max_mode must be one of {}; {mode} is not allowed from chat",
                MODES.join(", ")
            ))
        }
        None => None,
    };
    if let Some((token, _)) = &token {
        // A new token must be the same bot's.
        let probe = Connector {
            token: token.clone(),
            proxy: proxy.clone().unwrap_or_else(|| before.proxy.clone()),
            ..before.clone()
        };
        let bot = telegram::Api::new(&probe)?.me()?;
        if bot.id != before.bot_id {
            return Err("this token belongs to another bot: add it as a new connector".to_string());
        }
    }
    let after = hub.connectors.update(id, |connector| {
        if let Some(enabled) = op["enabled"].as_bool() {
            connector.enabled = enabled;
        }
        if let Some(proxy) = proxy {
            connector.proxy = proxy;
        }
        if let Some((token, sealed)) = token {
            connector.token = token;
            connector.sealed = sealed;
        }
        if let Some(approvals) = op["approvals"].as_bool() {
            connector.settings.approvals = approvals;
        }
        if let Some(notify) = op["notify"].as_bool() {
            connector.settings.notify = notify;
        }
        if let Some(mode) = max_mode {
            connector.settings.max_mode = mode;
        }
    })?;
    let restart = after.enabled != before.enabled
        || after.proxy != before.proxy
        || after.sealed != before.sealed;
    if after.enabled != before.enabled {
        hub.connectors.audit(
            id,
            if after.enabled { "enabled" } else { "disabled" },
            json!({}),
        );
    }
    if after.settings != before.settings {
        hub.connectors.audit(
            id,
            "settings",
            json!({ "settings": after.settings.to_json() }),
        );
    }
    if restart {
        start(hub, id);
    }
    let list = hub.connectors.list_json(hub);
    hub.broadcast_local(&list);
    Ok(list)
}

fn remove(hub: &Arc<Hub>, id: &str) -> Result<Value, String> {
    hub.connectors.stop(id);
    for device in hub.store.devices() {
        if !device.revoked
            && device
                .chat
                .as_ref()
                .is_some_and(|chat| chat.connector == id)
        {
            let _ = hub.revoke_device(&device.id);
        }
    }
    {
        let mut list = lock(&hub.connectors.list);
        list.retain(|c| c.id != id);
        hub.connectors.save(&list)?;
    }
    if let Some(state) = lock(&hub.connectors.state).as_object_mut() {
        state.remove(id);
    }
    lock(&hub.connectors.status).remove(id);
    lock(&hub.connectors.codes).retain(|_, (owner, _)| owner != id);
    hub.connectors.audit(id, "removed", json!({}));
    hub.broadcast_local(&hub.connectors.list_json(hub));
    Ok(json!({ "type": "connector_removed", "connector": id }))
}

/// Starts every enabled connector (when the daemon starts).
pub(crate) fn start_all(hub: &Arc<Hub>) {
    let ids: Vec<String> = lock(&hub.connectors.list)
        .iter()
        .map(|c| c.id.clone())
        .collect();
    for id in ids {
        start(hub, &id);
    }
}

/// (Re)starts a connector's threads, or stops them when it is off.
fn start(hub: &Arc<Hub>, id: &str) {
    hub.connectors.stop(id);
    let Some(connector) = hub.connectors.get(id) else {
        return;
    };
    if !connector.enabled {
        hub.connectors.set_status(hub, id, "stopped", None);
        return;
    }
    if connector.token.is_empty() {
        let error = "the bot token cannot be read (it was saved encrypted and no secret key opens it): enter it again";
        hub.connectors
            .set_status(hub, id, "error", Some(error.to_string()));
        return;
    }
    let api = match telegram::Api::new(&connector) {
        Ok(api) => api,
        Err(error) => {
            hub.connectors.set_status(hub, id, "error", Some(error));
            return;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (events, inbox) = mpsc::channel();
    lock(&hub.connectors.running).insert(
        id.to_string(),
        Running {
            stop: Arc::clone(&stop),
            events: events.clone(),
        },
    );
    {
        let hub = Arc::clone(hub);
        let stop = Arc::clone(&stop);
        let api = api.clone();
        let id = id.to_string();
        thread::spawn(move || {
            supervise(&hub, &id, &stop, |connected| {
                telegram::receive(&hub, &id, &api, &stop, &events, connected)
            })
        });
    }
    let hub = Arc::clone(hub);
    let id = id.to_string();
    thread::spawn(move || chat::run(hub, id, Box::new(api), stop, inbox));
}

/// Keeps a connector's connection to its platform, reconnecting with
/// backoff (1 s doubling to 60 s, plus up to 1 s of jitter), as the relay
/// does. `receive` returns when the connection ends or `stop` is set.
fn supervise(
    hub: &Arc<Hub>,
    id: &str,
    stop: &AtomicBool,
    mut receive: impl FnMut(&dyn Fn()) -> Result<(), Fault>,
) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        hub.connectors.set_status(hub, id, "connecting", None);
        let since = Instant::now();
        let connected = || hub.connectors.set_status(hub, id, "connected", None);
        let result = receive(&connected);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if since.elapsed() >= STABLE {
            backoff = Duration::from_secs(1);
        }
        let wait = match result {
            Ok(()) => Duration::from_secs(1),
            Err(Fault::Retry(error)) => {
                hub.connectors.set_status(hub, id, "error", Some(error));
                let wait = backoff;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                wait
            }
            Err(Fault::Conflict(error)) => {
                hub.connectors.set_status(hub, id, "error", Some(error));
                MAX_BACKOFF
            }
            Err(Fault::Rejected(error)) => {
                hub.connectors.audit(id, "rejected", json!({}));
                hub.connectors.set_status(hub, id, "error", Some(error));
                // Until `connector_set` gives a new token (which restarts).
                while !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(500));
                }
                break;
            }
        };
        let mut jitter = [0u8; 2];
        let _ = getrandom::getrandom(&mut jitter);
        let wait = wait + Duration::from_millis(u64::from(u16::from_le_bytes(jitter)) % 1000);
        let until = Instant::now() + wait;
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// A new binding: the sender becomes a chat device of `connector`.
pub(crate) fn bind(
    hub: &Hub,
    connector: &Connector,
    peer: &str,
    chat: &str,
    name: &str,
    lang: &str,
) -> Result<Device, String> {
    let device = Device {
        id: hub.new_id("dev"),
        name: name.chars().take(60).collect(),
        // Of a random value nobody keeps: no token opens this device.
        token_hash: crate::store::token_hash(&random_hex(32).map_err(|e| e.to_string())?),
        paired_at: now(),
        revoked: false,
        chat: Some(ChatPeer {
            connector: connector.id.clone(),
            platform: connector.platform.clone(),
            peer: peer.to_string(),
            chat: chat.to_string(),
            lang: lang.to_string(),
        }),
    };
    hub.store
        .record_device(&device)
        .map_err(|error| error.to_string())?;
    hub.connectors.audit(
        &connector.id,
        "bound",
        json!({ "device": device.id, "peer": peer }),
    );
    hub.broadcast_local(&json!({
        "type": "connector_bound",
        "connector": connector.id,
        "device": device.id,
        "name": device.name,
    }));
    hub.broadcast_local(&hub.connectors.list_json(hub));
    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Agents;

    pub(crate) fn hub(label: &str) -> Arc<Hub> {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-connectors-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let _ = fs::remove_dir_all(&dir);
        Hub::new(
            crate::Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        )
    }

    #[test]
    fn binding_codes_are_single_use_and_wrong_ones_lock_the_sender_out() {
        let hub = hub("codes");
        let connectors = &hub.connectors;
        assert_eq!(
            connectors.redeem("tg-1", "42", "ANYTHING"),
            Redeemed::NoCode
        );
        let (code, _) = connectors.new_code("tg-1").unwrap();
        assert_eq!(code.len(), 8);
        // Another connector's code does not bind here.
        let (other, _) = connectors.new_code("tg-2").unwrap();
        assert_eq!(connectors.redeem("tg-1", "42", &other), Redeemed::Wrong);
        assert_eq!(
            connectors.redeem("tg-1", "42", &code.to_lowercase()),
            Redeemed::Bound
        );
        assert_eq!(connectors.redeem("tg-1", "42", &code), Redeemed::NoCode);

        let (code, _) = connectors.new_code("tg-1").unwrap();
        for _ in 0..MAX_WRONG - 1 {
            assert_eq!(connectors.redeem("tg-1", "7", "WRONG123"), Redeemed::Wrong);
        }
        assert_eq!(connectors.redeem("tg-1", "7", "WRONG123"), Redeemed::Locked);
        // Locked out: even the right code is ignored, for that sender only.
        assert_eq!(connectors.redeem("tg-1", "7", &code), Redeemed::Locked);
        assert_eq!(connectors.redeem("tg-1", "8", &code), Redeemed::Bound);
        let audit = fs::read_to_string(hub.store.dir().join(AUDIT_FILE)).unwrap();
        assert!(audit.contains("\"locked\""));
    }

    #[test]
    fn connectors_are_saved_privately_and_read_back() {
        let hub = hub("save");
        let connector = Connector {
            id: "tg-1".to_string(),
            platform: "telegram".to_string(),
            name: "@bot".to_string(),
            username: "bot".to_string(),
            bot_id: "1".to_string(),
            enabled: true,
            proxy: Some("http://u:p@127.0.0.1:7890".to_string()),
            api: None,
            token: "123456:secret-token-value-0000000000000".to_string(),
            sealed: "123456:secret-token-value-0000000000000".to_string(),
            settings: Settings::default(),
            created_at: 1,
        };
        lock(&hub.connectors.list).push(connector.clone());
        hub.connectors.save(&[connector]).unwrap();
        let path = hub.store.dir().join(FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let again = Connectors::load(hub.store.dir());
        let loaded = again.get("tg-1").unwrap();
        assert_eq!(loaded.token, "123456:secret-token-value-0000000000000");
        assert_eq!(loaded.settings, Settings::default());
        // The list clients get never carries the token or the proxy password.
        let list = hub.connectors.list_json(&hub).to_string();
        assert!(!list.contains("secret-token"));
        assert!(list.contains("\"proxy_shown\":\"http://127.0.0.1:7890\""));
        assert!(list.contains("\"proxy_source\":\"setting\""));
        assert_eq!(hub.connectors.secrets().len(), 1);
    }

    #[test]
    fn proxy_and_api_settings_are_checked() {
        assert_eq!(proxy_setting(&json!(null)).unwrap(), None);
        assert_eq!(proxy_setting(&json!(" ")).unwrap(), None);
        assert_eq!(
            proxy_setting(&json!("direct")).unwrap().as_deref(),
            Some("direct")
        );
        assert_eq!(
            proxy_setting(&json!("http://127.0.0.1:7890/"))
                .unwrap()
                .as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert!(proxy_setting(&json!("socks5://127.0.0.1:7891")).is_err());
        assert!(api_setting(&json!("ftp://x")).is_err());
        assert_eq!(
            proxy_for(Some("direct"), "https://api.telegram.org"),
            (None, "none")
        );
        assert_eq!(
            proxy_for(Some("http://127.0.0.1:1"), "https://api.telegram.org").1,
            "setting"
        );
        assert_eq!(
            shown_proxy("http://user:pass@host:8080"),
            "http://host:8080"
        );
        assert_eq!(shown_proxy("host:8080"), "host:8080");
    }

    #[test]
    fn current_sessions_and_offsets_survive_a_restart() {
        let hub = hub("state");
        hub.connectors.set_offset("tg-1", 77);
        hub.connectors.set_current("tg-1", "dev-1", Some("s1"));
        let again = Connectors::load(hub.store.dir());
        assert_eq!(again.offset("tg-1"), 77);
        assert_eq!(again.current("tg-1", "dev-1").as_deref(), Some("s1"));
        again.set_current("tg-1", "dev-1", None);
        assert_eq!(again.current("tg-1", "dev-1"), None);
    }

    #[test]
    fn waiting_approvals_are_tracked_until_answered() {
        let hub = hub("calls");
        let connectors = &hub.connectors;
        connectors.observe(
            "s1",
            &json!({ "type": "approval_request", "call_id": "c1", "kind": "elevation" }),
        );
        connectors.observe(
            "s1",
            &json!({ "type": "approval_request", "call_id": "c2", "kind": "command" }),
        );
        assert_eq!(
            connectors.pending_kind("s1", "c1").as_deref(),
            Some("elevation")
        );
        assert!(connectors.waiting("s1"));
        connectors.observe("s1", &json!({ "type": "tool_output", "call_id": "c2" }));
        assert_eq!(connectors.pending_kind("s1", "c2"), None);
        connectors.observe("s1", &json!({ "type": "status", "message": "interrupted" }));
        assert!(!connectors.waiting("s1"));
    }
}

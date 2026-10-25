//! Shared daemon state: hosted sessions, connected clients and who watches
//! what, plus message routing and timers for long-lived agents. A session is
//! attended while at least one client watches it.

use crate::{
    agents::Agents,
    engines,
    relay::Relay,
    schedules::{self, Schedule},
    session,
    store::{
        now, random_hex, token_hash, Closure, Device, ItemKind, Message, Question, Report,
        SessionRecord, Store, Timer, CLOSED_KEEP_MS,
    },
    titles::{self, Turns},
    usage::{SessionInfo, Usage},
};
use lynshen_agent_core::{HookStopper, KeptState};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, TryRecvError},
        Arc, Mutex, MutexGuard, Weak,
    },
    thread,
    time::{Duration, Instant},
};

/// How long a pairing code shown on the desktop stays valid.
const PAIRING_TTL_MS: u64 = 5 * 60 * 1000;
/// Pairing codes avoid characters that are easy to misread (0/O, 1/I).
const PAIRING_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Runs that may be in progress at once across all sessions; messages that
/// would start another run wait for a free slot.
pub const MAX_RUNNING: usize = 4;

/// An engine session (Claude Code, Codex, an ACP agent) nobody watches,
/// with nothing going on (see `Hub::close_idle_engines`), has its engine
/// closed this long after its last activity; the next open resumes it as
/// it ran. A LynShen session closes on its own thread (`retire_idle`).
pub const ENGINE_IDLE_CLOSE: Duration = Duration::from_secs(30 * 60);
/// Engine processes kept open at most: past this, unwatched idle engine
/// sessions are closed, least recently active first, before their
/// `ENGINE_IDLE_CLOSE` is up.
pub const MAX_LIVE_ENGINES: usize = 16;
/// An engine session idle this briefly is not closed for the cap: it was
/// just opened or used, and closing it would only start it again.
const CAP_MIN_IDLE: Duration = Duration::from_secs(2 * 60);
/// Frames a client may have waiting: a client that falls this far behind
/// (a stalled connection) is dropped rather than queued for without end;
/// it reconnects and gets a fresh snapshot.
pub const OUTBOX_LIMIT: usize = 10_000;
/// How long a pending daemon update waits for held work (an engine
/// session's TUI or background work) to end before it restarts anyway;
/// running turns it always waits for.
const RESTART_HOLD: Duration = Duration::from_secs(10 * 60);

pub struct Hub {
    pub store: Store,
    pub agents: Agents,
    pub dispatch: crate::dispatch::Dispatches,
    pub requirements: crate::requirements::Requirements,
    pub uploads: crate::uploads::Uploads,
    pub push: crate::push::Push,
    /// Terminals (session TUIs, clients' shells) and the clients viewing
    /// them.
    pub terminals: crate::terminal::Terminals,
    pub version: &'static str,
    pub relay: Relay,
    sessions: Mutex<HashMap<String, Hosted>>,
    /// What the engines of sessions closed as idle held only in memory,
    /// until each session opens again (see `retire_idle`).
    kept: Mutex<HashMap<String, KeptState>>,
    /// Engine sessions closed as idle, with the options their engine ran
    /// with, until each opens again (see `close_idle_engines`).
    asleep: Mutex<HashMap<String, engines::Options>>,
    /// Why the daemon closed a session, until its engine thread ends and
    /// `session_ended` tells the clients (`session_closed`'s `reason`).
    close_reasons: Mutex<HashMap<String, &'static str>>,
    /// Sessions whose engine is running a turn or has queued messages.
    busy: Mutex<HashSet<String>>,
    /// Sessions whose latest turn ended on an error, until the next starts.
    failed: Mutex<HashSet<String>>,
    /// A newer daemon waits to replace this one (`restart_when_idle`).
    restart_pending: AtomicBool,
    /// Delivered messages a session thread has not processed yet. They hold
    /// a running slot: the engine still reports "ready" until it has read
    /// the message, and that must not free the slot early.
    claims: Mutex<HashMap<String, usize>>,
    /// Every agent's scheduled tasks (see `schedules`).
    pub(crate) schedules: Mutex<Vec<Schedule>>,
    /// Cancellation and firing must not both succeed for the same timer.
    timer_updates: Mutex<()>,
    /// Serializes message delivery (scheduler ticks, sends, tool calls).
    delivering: Mutex<()>,
    next_id: AtomicU64,
    next_generation: AtomicU64,
    clients: Mutex<HashMap<u64, Client>>,
    /// Pairing code → expiry (ms). Single use.
    pairings: Mutex<HashMap<String, u64>>,
    next_client: AtomicU64,
    /// Sessions created here that have not had a user message yet: the
    /// first one becomes their title.
    untitled: Mutex<HashSet<String>>,
    /// Images sent with a message, by session and the message's text, until
    /// the engine echoes it: no engine's `user_message` carries them.
    sent_images: Mutex<HashMap<String, Vec<(String, Value)>>>,
    /// Each session's conversation so far, for its model-written title.
    turns: Mutex<HashMap<String, Turns>>,
    /// Sessions whose title is being written now.
    titling: Mutex<HashSet<String>>,
    /// Agent sessions whose handoff note is being written now.
    handing_off: Mutex<HashSet<String>>,
    /// This hub, for the threads it starts.
    me: Weak<Hub>,
    /// Each turn's token usage (see `usage`).
    pub usage: Usage,
    /// Messages between sessions (see `session_messages`).
    pub(crate) messages: crate::session_messages::Messages,
    /// Chat bots and what they keep (see `connectors`).
    pub connectors: crate::connectors::Connectors,
    /// Dev servers the sessions' commands started (see `devservers`).
    pub dev_servers: crate::devservers::DevServers,
    /// When each device was last heard from (ms), since the daemon started.
    seen: Mutex<HashMap<String, u64>>,
}

/// Where a client's frames wait. A connection's (`Hub::outbox`) counts
/// them: one that falls `OUTBOX_LIMIT` behind is dropped. An in-process
/// client's (a chat binding, `Outbox::local`) is not limited: it reads its
/// frames on its own thread, and dropping it would unbind its chat.
pub struct Outbox {
    sender: Sender<String>,
    waiting: Option<Arc<AtomicUsize>>,
}

impl Outbox {
    pub fn local(sender: Sender<String>) -> Self {
        Self {
            sender,
            waiting: None,
        }
    }

    /// Queues `text`; false when the client is gone or too far behind.
    fn send(&self, text: String) -> bool {
        if let Some(waiting) = &self.waiting {
            if waiting.fetch_add(1, Ordering::SeqCst) >= OUTBOX_LIMIT {
                return false;
            }
        }
        self.sender.send(text).is_ok()
    }
}

/// A connection's side of its `Outbox`.
pub struct Inbox {
    receiver: Receiver<String>,
    waiting: Arc<AtomicUsize>,
}

impl Inbox {
    pub fn try_recv(&self) -> Result<String, TryRecvError> {
        let frame = self.receiver.try_recv()?;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        Ok(frame)
    }
}

struct Client {
    outbox: Outbox,
    /// The paired device this connection authenticated as; None for a local
    /// client holding the daemon token.
    device: Option<String>,
}

struct Hosted {
    ops: Sender<Value>,
    /// Stops a LynShen engine's hooks that hold its thread (see
    /// `HookStopper`); an interrupt op triggers it before it is queued.
    hook_stopper: Option<HookStopper>,
    cwd: PathBuf,
    watchers: HashSet<u64>,
    /// Which engine thread backs this entry (see `session_ended`).
    generation: u64,
    /// When it was last opened (or found open): a client that opened it
    /// watches it next, so it is not closed as idle meanwhile.
    opened: Instant,
    /// When something last happened in it: an op from a client, a turn
    /// starting or ending, held work ending (engine sessions' idle close).
    last_active: Instant,
    /// Work outside a turn keeps an engine session open: background tasks
    /// or subagents running, an approval waiting, its TUI open.
    held: bool,
}

/// How long a tokenizer nobody counted with stays loaded (see `tick`).
const TOKENIZER_IDLE: Duration = Duration::from_secs(10 * 60);

/// How long after an open an idle session is kept for the client that
/// opened it to start watching (see `Hub::retire_idle`).
const OPEN_GRACE: Duration = Duration::from_secs(30);

/// How often idle engine sessions are looked for (`close_idle_engines`).
const IDLE_SWEEP_EVERY: Duration = Duration::from_secs(30);

/// `ENGINE_IDLE_CLOSE`, which a debug build's end-to-end checks shorten with
/// `LYNSHEN_TEST_ENGINE_IDLE_SECS`. A release build always waits the full time.
fn engine_idle_close() -> Duration {
    if cfg!(debug_assertions) {
        if let Some(secs) = std::env::var("LYNSHEN_TEST_ENGINE_IDLE_SECS")
            .ok()
            .and_then(|secs| secs.parse().ok())
        {
            return Duration::from_secs(secs);
        }
    }
    ENGINE_IDLE_CLOSE
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl Hub {
    /// `relay` is the relay base URL; None turns the relay off whatever the
    /// setting says (`--no-relay`).
    pub fn new(
        store: Store,
        agents: Agents,
        version: &'static str,
        relay: Option<String>,
    ) -> Arc<Self> {
        let schedules = Mutex::new(schedules::load(&agents));
        // `~/.lynshen`: uploads live beside the daemon's state, not in it.
        let lynshen_dir = store.dir().parent().unwrap_or(store.dir()).to_path_buf();
        let uploads = crate::uploads::Uploads::load(&lynshen_dir);
        let requirements = crate::requirements::Requirements::load(
            store.dir(),
            uploads.dir().join("requirements"),
            &store.workspaces(),
        );
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            schedules,
            timer_updates: Mutex::new(()),
            turns: Mutex::new(HashMap::new()),
            titling: Mutex::new(HashSet::new()),
            handing_off: Mutex::new(HashSet::new()),
            relay: Relay::new(relay, &store),
            usage: Usage::new(&store),
            messages: Default::default(),
            dispatch: crate::dispatch::Dispatches::load(store.dir()),
            requirements,
            uploads,
            push: crate::push::Push::load(store.dir(), &store),
            connectors: crate::connectors::Connectors::load(store.dir()),
            dev_servers: Default::default(),
            seen: Mutex::new(HashMap::new()),
            terminals: Default::default(),
            store,
            agents,
            version,
            sessions: Mutex::new(HashMap::new()),
            kept: Mutex::new(HashMap::new()),
            asleep: Mutex::new(HashMap::new()),
            close_reasons: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
            failed: Mutex::new(HashSet::new()),
            restart_pending: AtomicBool::new(false),
            claims: Mutex::new(HashMap::new()),
            delivering: Mutex::new(()),
            next_id: AtomicU64::new(0),
            next_generation: AtomicU64::new(0),
            clients: Mutex::new(HashMap::new()),
            pairings: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            untitled: Mutex::new(HashSet::new()),
            sent_images: Mutex::new(HashMap::new()),
        })
    }

    /// The channel a connection's frames wait in (`OUTBOX_LIMIT`).
    pub fn outbox() -> (Outbox, Inbox) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiting = Arc::new(AtomicUsize::new(0));
        let outbox = Outbox {
            sender,
            waiting: Some(Arc::clone(&waiting)),
        };
        (outbox, Inbox { receiver, waiting })
    }

    /// Queues `text` for a client; false when the client is gone or so far
    /// behind that it is dropped (it reconnects and gets a fresh snapshot).
    fn queue(client: &Client, text: String) -> bool {
        client.outbox.send(text)
    }

    pub fn add_client(&self, outbox: Outbox, device: Option<String>) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::SeqCst);
        if let Some(device) = &device {
            self.touch_device(device);
        }
        lock(&self.clients).insert(id, Client { outbox, device });
        id
    }

    /// Notes that a device was heard from now (`last_seen`).
    pub fn touch_device(&self, device: &str) {
        lock(&self.seen).insert(device.to_string(), now());
    }

    /// Whether the connection is a chat binding's (see `connectors`).
    pub fn is_chat(&self, client: u64) -> bool {
        self.device_of(client)
            .and_then(|device| self.store.device(&device))
            .is_some_and(|device| device.chat.is_some())
    }

    /// The paired device a connection authenticated as (None: local).
    pub fn device_of(&self, client: u64) -> Option<String> {
        lock(&self.clients).get(&client)?.device.clone()
    }

    /// Whether the connection is a local client (not a paired device).
    pub fn is_local(&self, client: u64) -> bool {
        lock(&self.clients)
            .get(&client)
            .is_some_and(|client| client.device.is_none())
    }

    /// A new single-use pairing code and its expiry (ms).
    pub fn start_pairing(&self) -> Result<(String, u64), String> {
        let code = new_code(8)?;
        let expires_at = now() + PAIRING_TTL_MS;
        let mut pairings = lock(&self.pairings);
        pairings.retain(|_, expiry| *expiry > now());
        pairings.insert(code.clone(), expires_at);
        Ok((code, expires_at))
    }

    /// Trades a pairing code for a new device and its token. The token is
    /// returned once and only its hash is stored.
    /// The device waits for the computer to confirm it, as over the relay;
    /// its check code is from the token's SHA-256, which the phone computes
    /// from the token it got.
    pub fn pair(&self, code: &str, name: &str) -> Result<(Device, String), String> {
        use sha2::{Digest, Sha256};
        let token = random_hex(32).map_err(|error| error.to_string())?;
        let check = check_code(&Sha256::digest(token.as_bytes()));
        let device = self.pair_pending(code, name, token_hash(&token), check)?;
        Ok((device, token))
    }

    /// Drops a pairing code shown on the desktop (its dialog closed); every
    /// open code when `code` is None.
    pub fn cancel_pairing(&self, code: Option<&str>) {
        let mut pairings = lock(&self.pairings);
        match code {
            Some(code) => {
                pairings.remove(&code.trim().to_ascii_uppercase());
            }
            None => pairings.clear(),
        }
    }

    /// A relay client paired with a code: a device that waits for the
    /// computer to confirm it (the 6-digit check code both sides show,
    /// `device_confirm`). It cannot connect until then.
    /// `check`: the code from this pairing's channel (`check_code`).
    pub fn pair_pending(
        &self,
        code: &str,
        name: &str,
        token_hash: String,
        check: String,
    ) -> Result<Device, String> {
        let device = self.new_device(code, name, token_hash, Some(check))?;
        self.broadcast_local(&json!({
            "type": "device_pending",
            "device": device.id,
            "name": device.name,
            "check": device.check,
        }));
        Ok(device)
    }

    /// The computer confirms a pending device (its check code matched).
    pub fn confirm_device(&self, id: &str) -> Result<(), String> {
        let device = self
            .store
            .device(id)
            .ok_or_else(|| format!("unknown device {id}"))?;
        if !device.pending {
            return Ok(());
        }
        self.store
            .record_device_confirmed(id)
            .map_err(|error| error.to_string())?;
        self.broadcast_local(&self.devices_json());
        Ok(())
    }

    /// Grants `id` exactly these elevated scopes (`scopes.rs`).
    pub fn set_device_scopes(&self, id: &str, scopes: &[String]) -> Result<(), String> {
        if let Some(bad) = scopes
            .iter()
            .find(|scope| !crate::scopes::ELEVATED.contains(&scope.as_str()))
        {
            return Err(format!("unknown scope {bad}"));
        }
        if self
            .store
            .device(id)
            .is_none_or(|device| device.chat.is_some())
        {
            return Err(format!("unknown device {id}"));
        }
        self.store
            .record_device_scopes(id, scopes)
            .map_err(|error| error.to_string())?;
        self.broadcast_local(&self.devices_json());
        // The device's own connections hear of it (they ask `device_me`).
        let text = json!({ "type": "device_scopes_changed" }).to_string();
        for client in lock(&self.clients).values() {
            if client.device.as_deref() == Some(id) {
                let _ = client.outbox.send(text.clone());
            }
        }
        Ok(())
    }

    /// Trades a pairing code for a new device identified by `token_hash`
    /// (a token's hash, or a relay client's static key hash). `check`:
    /// Some(code) for a device that waits for the computer.
    fn new_device(
        &self,
        code: &str,
        name: &str,
        token_hash: String,
        check: Option<String>,
    ) -> Result<Device, String> {
        let code = code.trim().to_ascii_uppercase();
        let valid = lock(&self.pairings)
            .remove(&code)
            .is_some_and(|expiry| expiry > now());
        if !valid {
            return Err("pairing code is wrong or expired".to_string());
        }
        let name = name.trim();
        let device = Device {
            id: self.new_id("dev"),
            name: if name.is_empty() { "device" } else { name }
                .chars()
                .take(60)
                .collect(),
            token_hash,
            paired_at: now(),
            revoked: false,
            chat: None,
            scopes: Vec::new(),
            pending: check.is_some(),
            check,
        };
        self.store
            .record_device(&device)
            .map_err(|error| error.to_string())?;
        Ok(device)
    }

    /// Revokes a device and drops its open connections (a chat binding's
    /// client too: its connector stops serving it).
    pub fn revoke_device(&self, id: &str) -> Result<(), String> {
        let Some(device) = self.store.device(id) else {
            return Err(format!("unknown device {id}"));
        };
        self.store
            .record_device_revoked(id)
            .map_err(|error| error.to_string())?;
        // Dropping the outbox ends that connection's loop.
        lock(&self.clients).retain(|_, client| client.device.as_deref() != Some(id));
        self.push.forget_device(id);
        if let Some(chat) = &device.chat {
            self.connectors.revoked(&device, chat);
        }
        Ok(())
    }

    pub fn devices_json(&self) -> Value {
        let seen = lock(&self.seen).clone();
        let list: Vec<Value> = self
            .store
            .devices()
            .iter()
            .filter(|device| !device.revoked)
            .map(|device| {
                let mut item = json!({
                    "id": device.id,
                    "name": device.name,
                    "paired_at": device.paired_at,
                    "kind": if device.chat.is_some() { "chat" } else { "device" },
                    "last_seen": seen.get(&device.id),
                    "scopes": device.scopes,
                    "pending": device.pending,
                });
                if device.pending {
                    item["check"] = json!(device.check);
                }
                if let Some(chat) = &device.chat {
                    item["platform"] = json!(chat.platform);
                    item["connector"] = json!(chat.connector);
                }
                item
            })
            .collect();
        json!({ "type": "devices", "devices": list })
    }

    /// When a device was last heard from, since the daemon started.
    pub fn last_seen(&self, device: &str) -> Option<u64> {
        lock(&self.seen).get(device).copied()
    }

    /// Drops the client, its watches, the terminals it viewed and its
    /// shells; sessions it was the last watcher of become unattended.
    pub fn remove_client(&self, client: u64) {
        let removed = lock(&self.clients).remove(&client);
        if let Some(device) = removed.and_then(|client| client.device) {
            let still = lock(&self.clients)
                .values()
                .any(|other| other.device.as_deref() == Some(device.as_str()));
            if !still {
                crate::push::gone(self, &device);
            }
        }
        self.terminals.detach_client(client);
        let watched: Vec<String> = lock(&self.sessions)
            .iter()
            .filter(|(_, hosted)| hosted.watchers.contains(&client))
            .map(|(id, _)| id.clone())
            .collect();
        for session in watched {
            self.set_watch(client, &session, false);
        }
    }

    pub fn broadcast(&self, frame: &Value) {
        let text = frame.to_string();
        lock(&self.clients).retain(|_, client| Self::queue(client, text.clone()));
    }

    /// Sends `frame` to the local clients only (the desktop), never to a
    /// paired phone or a chat binding.
    pub fn broadcast_local(&self, frame: &Value) {
        let text = frame.to_string();
        lock(&self.clients)
            .retain(|_, client| client.device.is_some() || Self::queue(client, text.clone()));
    }

    pub fn send_to(&self, client: u64, frame: &Value) {
        let mut clients = lock(&self.clients);
        if let Some(queued) = clients.get(&client) {
            if !Self::queue(queued, frame.to_string()) {
                clients.remove(&client);
            }
        }
    }

    /// Starts a new session in `cwd`, in the agent's directory for an agent
    /// session, or in the chats directory for a chat.
    pub fn create_session(
        self: &Arc<Self>,
        cwd: Option<PathBuf>,
        agent: Option<&str>,
        chat: bool,
    ) -> Result<String, String> {
        self.create_engine_session(cwd, agent, chat, None, engines::Options::default(), None)
    }

    /// `create_session` with the engine to run (None: lynshen), its start
    /// options and the project it belongs to (`""`: none; None: the one
    /// whose main directory is its folder).
    pub fn create_engine_session(
        self: &Arc<Self>,
        cwd: Option<PathBuf>,
        agent: Option<&str>,
        chat: bool,
        engine: Option<engines::Kind>,
        options: engines::Options,
        project: Option<&str>,
    ) -> Result<String, String> {
        if engine.is_some() && agent.is_some() {
            return Err("agents run on the lynshen engine".to_string());
        }
        let cwd = match agent {
            None if chat => {
                lynshen_agent_core::chat::ensure_chats_dir().map_err(|error| error.to_string())?
            }
            Some(id) => {
                let agent = self
                    .agents
                    .get(id)
                    .ok_or_else(|| format!("unknown agent {id}"))?;
                if !agent.enabled {
                    return Err(format!("agent {id} is disabled"));
                }
                agent.cwd
            }
            None => cwd.ok_or_else(|| "session_create requires cwd or agent".to_string())?,
        };
        if !cwd.is_dir() {
            return Err(format!("not a directory: {}", cwd.display()));
        }
        // A chat belongs to no project.
        let project = match chat {
            true => String::new(),
            false => crate::projects::for_new_session(self, &cwd, project)?,
        };
        let dirs = crate::projects::extra_dirs(self, &cwd, &project);
        let mut options = engines::Options {
            dirs: dirs.clone(),
            ..options
        };
        // A fork goes on from another conversation: where it cuts, and what
        // it shows until its engine replays it (see `fork_start`).
        let (forked, seed) = match options.fork_from.clone() {
            Some(from) => self.fork_start(engine, &cwd, &from, &mut options)?,
            None => (None, Vec::new()),
        };
        let agent = agent.map(str::to_string);
        let gateway = options.gateway == Some(true);
        // Claude Code / Codex off the gateway: the custom provider it starts
        // on, kept with it (`pin_tool_provider`).
        if let Some(kind) = engine {
            engines::pin_tool_provider(&self.store, kind, &mut options);
        }
        let tool_provider = options.tool_provider.clone();
        // Which ACP agent runs it (a built-in one by its id, or the
        // desktop's registry entry): a client can start it again.
        let acp_agent = match &options.agent {
            Some(id) => engines::acp_presets::find(id)
                .map(|preset| json!({ "id": preset.id, "name": preset.name, "builtin": true })),
            None => options.acp_agent.clone(),
        }
        .filter(|_| engine == Some(engines::Kind::Acp));
        let mut hook_stopper = None;
        let (id, ops, generation) = match engine {
            None => {
                // A fork: a copy of the conversation's first turns, resumed.
                let resume = match &options.fork_from {
                    Some(from) => Some(
                        lynshen_agent_core::fork_session(
                            options.fork_cwd.as_deref().unwrap_or(&cwd),
                            &cwd,
                            from,
                            options.fork_turns.map(|turns| turns as usize),
                        )
                        .map_err(|error| format!("cannot fork {from}: {error}"))?,
                    ),
                    None => None,
                };
                let (id, ops, generation, stopper) =
                    session::spawn(Arc::clone(self), cwd.clone(), resume, agent.clone(), dirs)?;
                hook_stopper = Some(stopper);
                (id, ops, generation)
            }
            Some(kind) => {
                // Claude Code takes its conversation id from us; others name it.
                let id = match kind {
                    engines::Kind::Claude | engines::Kind::Acp => Some(engines::new_uuid()?),
                    engines::Kind::Codex => None,
                };
                engines::spawn(Arc::clone(self), kind, id, cwd.clone(), options, seed)?
            }
        };
        self.store
            .record_engine_session(
                &id,
                &cwd,
                agent.as_deref(),
                engine.map(engines::Kind::name),
                gateway,
                Some(&project),
            )
            .map_err(|error| error.to_string())?;
        if let Some(provider) = tool_provider.filter(|_| !gateway) {
            self.store
                .record_session_meta(&id, &json!({ "tool_provider": provider }))
                .map_err(|error| error.to_string())?;
        }
        // Which of the desktop's ACP agents runs it: a client can start it again.
        if let Some(acp_agent) = acp_agent {
            self.store
                .record_session_meta(&id, &json!({ "acp_agent": acp_agent }))
                .map_err(|error| error.to_string())?;
        }
        // Where a fork comes from, kept with it: the list names it, and a
        // Claude fork closed before its first turn is made again from it.
        // A fork starts with its source's title (the daemon's own, which the
        // title model may refine), not one from its next message.
        let mut untitled = true;
        if let Some(source) = forked {
            self.store
                .record_session_meta(&id, &json!({ "forked_from": source }))
                .map_err(|error| error.to_string())?;
            let title = self
                .store
                .sessions()
                .into_iter()
                .find(|record| Some(record.id.as_str()) == source["session"].as_str())
                .and_then(|record| shown_title(&record, None));
            if let Some(title) = title {
                untitled = false;
                self.store
                    .record_session_meta(&id, &json!({ "title": title, "title_auto": true }))
                    .map_err(|error| error.to_string())?;
            }
        }
        if untitled {
            lock(&self.untitled).insert(id.clone());
        }
        self.host(id.clone(), ops, cwd, generation);
        self.set_hook_stopper(&id, hook_stopper);
        self.broadcast(&self.agents_json());
        Ok(id)
    }

    /// A fork of conversation `from` that `engine` starts in `cwd`: its
    /// record (`{session, turns, at}`) and the transcript it shows until its
    /// engine replays it. Claude Code forks at a reply (`resume_at`), found
    /// here from `fork_turns` when the client named none; Codex and LynShen
    /// cut at `fork_turns` themselves. An ACP agent's conversation is copied
    /// whole with `fork` instead.
    fn fork_start(
        &self,
        engine: Option<engines::Kind>,
        cwd: &std::path::Path,
        from: &str,
        options: &mut engines::Options,
    ) -> Result<(Option<Value>, Vec<Value>), String> {
        if options.fork_turns == Some(0) {
            return Err("a fork keeps at least one turn".to_string());
        }
        if let Some(dir) = options.fork_cwd.as_deref().filter(|dir| !dir.is_absolute()) {
            return Err(format!("fork_cwd must be absolute: {}", dir.display()));
        }
        let mut seed = Vec::new();
        match engine {
            Some(engines::Kind::Acp) => {
                return Err(
                    "an ACP agent's conversation is copied whole with options.fork".to_string(),
                )
            }
            Some(engines::Kind::Claude) => {
                let source_cwd = options
                    .fork_cwd
                    .clone()
                    .unwrap_or_else(|| cwd.to_path_buf());
                let config = engines::claude::config_with(options, &source_cwd, from);
                let path = engines::claude::find_conversation(&config, &source_cwd, from)
                    .ok_or_else(|| {
                        format!("cannot fork {from}: no saved Claude Code conversation")
                    })?;
                if options.resume_at.is_none() {
                    if let Some(turns) = options.fork_turns {
                        options.resume_at = engines::claude::fork_point(&path, turns)
                            .map_err(|error| format!("cannot fork {from}: {error}"))?;
                    }
                }
                seed = engines::claude::fork_transcript(
                    &path,
                    options.resume_at.as_deref(),
                    self.uploads.dir(),
                );
            }
            Some(engines::Kind::Codex) | None => {}
        }
        let source =
            json!({ "session": from, "turns": options.fork_turns, "at": options.resume_at });
        Ok((Some(source), seed))
    }

    /// Hosts a session recorded earlier (after a restart or a close). A
    /// session that is already hosted is left as it is.
    /// `cwd` also opens a session the daemon never hosted (one saved by
    /// the TUI or `lynshen serve` in that directory).
    pub fn open_session(self: &Arc<Self>, id: &str, cwd: Option<PathBuf>) -> Result<(), String> {
        self.open_engine_session(id, cwd, None, engines::Options::default())
    }

    /// `open_session` with start options; `engine` names the engine of a
    /// session the daemon never hosted (a Claude Code conversation saved in
    /// `cwd`).
    pub fn open_engine_session(
        self: &Arc<Self>,
        id: &str,
        cwd: Option<PathBuf>,
        engine: Option<engines::Kind>,
        options: engines::Options,
    ) -> Result<(), String> {
        if let Some(hosted) = lock(&self.sessions).get_mut(id) {
            hosted.opened = Instant::now();
            hosted.last_active = Instant::now();
            return Ok(());
        }
        // An engine closed as idle: a client that does not say how (a phone,
        // a watch, a delivery) opens it as it last ran. The entry goes once
        // it is open (a failed open leaves it asleep).
        let kept = lock(&self.asleep).get(id).cloned();
        let options = match kept {
            Some(kept) if options == engines::Options::default() => kept,
            _ => options,
        };
        let known = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == id);
        let record = match (known, cwd) {
            (Some(record), _) => record,
            (None, Some(cwd)) => {
                let saved = match engine {
                    None => lynshen_agent_core::saved_sessions(&cwd)
                        .map_err(|error| error.to_string())?
                        .iter()
                        .any(|summary| summary.id == id),
                    Some(engines::Kind::Claude) => engines::claude::saved(
                        &engines::claude::config_with(&options, &cwd, id),
                        &cwd,
                    )
                    .iter()
                    .any(|(saved, _, _)| saved == id),
                    Some(engines::Kind::Codex) => engines::codex::saved(&cwd)
                        .iter()
                        .any(|(saved, _, _)| saved == id),
                    // The daemon cannot list an ACP agent's conversations.
                    Some(engines::Kind::Acp) => false,
                };
                if !saved {
                    return Err(format!("no session {id} in {}", cwd.display()));
                }
                SessionRecord {
                    id: id.to_string(),
                    cwd,
                    agent: None,
                    created_at: now(),
                    closed: true,
                    title: None,
                    title_auto: false,
                    archived: false,
                    hidden: false,
                    engine: engine.map(|kind| kind.name().to_string()),
                    gateway: false,
                    group: None,
                    project: None,
                    conversation: None,
                    acp_agent: None,
                    forked_from: None,
                    tool_provider: None,
                }
            }
            (None, None) => return Err(format!("unknown session {id}")),
        };
        let kind = engines::Kind::parse(record.engine.as_deref().unwrap_or_default())?;
        let dirs = crate::projects::extra_dirs(
            self,
            &record.cwd,
            &crate::projects::of_record(self, &record),
        );
        let mut gateway = false;
        let mut tool_provider = None;
        let mut hook_stopper = None;
        let (ops, generation) = match kind {
            None => {
                let (_, ops, generation, stopper) = session::spawn(
                    Arc::clone(self),
                    record.cwd.clone(),
                    Some(id.to_string()),
                    record.agent.clone(),
                    dirs,
                )?;
                hook_stopper = Some(stopper);
                (ops, generation)
            }
            Some(kind) => {
                // A conversation with no turn yet was never saved: start it
                // again under the same id instead of resuming it.
                // Codex resumes through its protocol, which also sends the
                // history back. An ACP agent reopens the conversation it
                // opened (its own session id, kept with the record).
                // Where Claude Code saves its conversations (the session's
                // environment or a program of the user's may set that).
                let claude_config = (kind == engines::Kind::Claude)
                    .then(|| engines::claude::config_with(&options, &record.cwd, id));
                let saved = match kind {
                    engines::Kind::Claude => claude_config
                        .as_deref()
                        .is_some_and(|config| engines::claude::is_saved(config, &record.cwd, id)),
                    engines::Kind::Codex => true,
                    engines::Kind::Acp => record.conversation.is_some(),
                };
                let mut transcript = claude_config
                    .as_deref()
                    .map(|config| {
                        engines::claude::transcript(config, &record.cwd, id, self.uploads.dir())
                    })
                    .unwrap_or_default();
                // A Claude fork closed before its first turn was never saved:
                // it is forked again rather than started empty.
                let refork = match (kind, saved, &record.forked_from) {
                    (engines::Kind::Claude, false, Some(source)) => {
                        let from = source["session"].as_str().map(str::to_string);
                        let at = source["at"].as_str().map(str::to_string);
                        if let (Some(config), Some(from)) = (claude_config.as_deref(), &from) {
                            if let Some(path) =
                                engines::claude::find_conversation(config, &record.cwd, from)
                            {
                                transcript = engines::claude::fork_transcript(
                                    &path,
                                    at.as_deref(),
                                    self.uploads.dir(),
                                );
                            }
                        }
                        from.map(|from| (from, at))
                    }
                    _ => None,
                };
                // A built-in ACP agent opens again from its record, whatever
                // the client names; a registered one only from the desktop,
                // which knows its command.
                let builtin = (kind == engines::Kind::Acp)
                    .then_some(record.acp_agent.as_ref())
                    .flatten()
                    .filter(|agent| agent["builtin"] == true)
                    .and_then(|agent| agent["id"].as_str())
                    .map(str::to_string);
                if kind == engines::Kind::Acp
                    && builtin.is_none()
                    && options.command.is_none()
                    && options.agent.is_none()
                {
                    return Err(
                        "this agent's session can only be reopened on the desktop".to_string()
                    );
                }
                let options = match builtin {
                    Some(agent) => engines::Options {
                        agent: Some(agent),
                        command: None,
                        args: Vec::new(),
                        ..options
                    },
                    None => options,
                };
                // A client that does not say (a phone) keeps how it last ran,
                // on the provider it last ran on off the gateway (a session
                // recorded before providers were: the tool's own).
                let mut options = engines::Options {
                    resume: match kind {
                        engines::Kind::Acp => record.conversation.clone(),
                        _ => saved.then(|| id.to_string()),
                    },
                    gateway: Some(options.gateway.unwrap_or(record.gateway)),
                    tool_provider: options.tool_provider.or_else(|| {
                        record
                            .tool_provider
                            .clone()
                            .or_else(|| (!record.gateway).then(String::new))
                    }),
                    // A copy is made once, when the session is created.
                    fork: None,
                    fork_from: refork.as_ref().map(|(from, _)| from.clone()),
                    fork_turns: None,
                    fork_cwd: None,
                    resume_at: match &refork {
                        Some((_, at)) => at.clone(),
                        None => options.resume_at.clone(),
                    },
                    claude_config,
                    dirs,
                    ..options
                };
                gateway = options.gateway == Some(true);
                if gateway {
                    options.tool_provider = None;
                }
                engines::pin_tool_provider(&self.store, kind, &mut options);
                tool_provider = options.tool_provider.clone();
                // A new start keeps the recorded id; a resume opens it. An
                // ACP session keeps the daemon's id either way (`resume` is
                // the agent's).
                let fresh = (kind == engines::Kind::Acp || options.resume.is_none())
                    .then(|| id.to_string());
                let (_, ops, generation) = engines::spawn(
                    Arc::clone(self),
                    kind,
                    fresh,
                    record.cwd.clone(),
                    options,
                    transcript,
                )?;
                (ops, generation)
            }
        };
        if record.closed {
            self.store
                .record_engine_session(
                    id,
                    &record.cwd,
                    record.agent.as_deref(),
                    record.engine.as_deref(),
                    gateway,
                    record.project.as_deref(),
                )
                .map_err(|error| error.to_string())?;
        }
        if let Some(provider) = tool_provider.filter(|p| record.tool_provider.as_ref() != Some(p)) {
            self.store
                .record_session_meta(id, &json!({ "tool_provider": provider }))
                .map_err(|error| error.to_string())?;
        }
        self.host(id.to_string(), ops, record.cwd, generation);
        self.set_hook_stopper(id, hook_stopper);
        lock(&self.asleep).remove(id);
        Ok(())
    }

    /// Whether an engine runs the session now.
    pub fn is_open(&self, id: &str) -> bool {
        lock(&self.sessions).contains_key(id)
    }

    /// An open session's conversation as a client that starts watching
    /// gets it: its `transcript` items.
    pub fn transcript(&self, session: &str) -> Result<Vec<Value>, String> {
        let (outbox, inbox) = std::sync::mpsc::channel();
        let client = self.add_client(Outbox::local(outbox), None);
        let result = self
            .forward(session, json!({ "op": "snapshot", "client": client }))
            .and_then(|()| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let left = deadline
                        .checked_duration_since(std::time::Instant::now())
                        .ok_or("the conversation did not answer in time")?;
                    let text = inbox
                        .recv_timeout(left)
                        .map_err(|_| "the conversation did not answer in time")?;
                    let frame: Value = serde_json::from_str(&text).unwrap_or_default();
                    if frame["type"] == "transcript" && frame["session"] == session {
                        break Ok(frame["items"].as_array().cloned().unwrap_or_default());
                    }
                }
            });
        lock(&self.clients).remove(&client);
        result
    }

    pub(crate) fn host(&self, id: String, ops: Sender<Value>, cwd: PathBuf, generation: u64) {
        lock(&self.sessions).insert(
            id,
            Hosted {
                ops,
                hook_stopper: None,
                cwd,
                watchers: HashSet::new(),
                generation,
                opened: Instant::now(),
                last_active: Instant::now(),
                held: false,
            },
        );
    }

    pub(crate) fn set_hook_stopper(&self, id: &str, stopper: Option<HookStopper>) {
        if let Some(hosted) = lock(&self.sessions).get_mut(id) {
            hosted.hook_stopper = stopper;
        }
    }

    /// Closes a session whose engine thread found it idle with nobody
    /// watching, unless a client watches or opened it since, a delivered
    /// message holds it, it is being titled or handed off, another session
    /// waits on its reply, or an op reached it (`quiet` checks the thread's
    /// channel; ops are sent under the sessions lock, so none arrives once
    /// this has decided). True when closed: the thread then ends, and
    /// `session_ended` tells the clients. Reopening it brings it back, with
    /// what its engine held only in memory: `kept` is stored before an open
    /// can find the session closed (see `take_kept`).
    pub(crate) fn retire_idle(
        &self,
        id: &str,
        generation: u64,
        quiet: impl FnOnce() -> bool,
        kept: impl FnOnce() -> KeptState,
    ) -> bool {
        // Before the sessions lock: a message send holds its state while it
        // forwards (a send that starts meanwhile finds the session claimed).
        if crate::session_messages::awaited(self, id) {
            return false;
        }
        let mut sessions = lock(&self.sessions);
        let Some(hosted) = sessions.get(id) else {
            return false;
        };
        if hosted.generation != generation
            || !hosted.watchers.is_empty()
            || hosted.opened.elapsed() < OPEN_GRACE
            || lock(&self.claims).contains_key(id)
            || lock(&self.busy).contains(id)
            || lock(&self.untitled).contains(id)
            || lock(&self.titling).contains(id)
            || lock(&self.handing_off).contains(id)
            || !quiet()
        {
            return false;
        }
        sessions.remove(id);
        lock(&self.kept).insert(id.to_string(), kept());
        drop(sessions);
        let _ = self.store.record_session_closed(id);
        true
    }

    /// What a session's engine held only in memory when it closed as idle,
    /// for the engine that opens it next.
    pub(crate) fn take_kept(&self, id: &str) -> Option<KeptState> {
        lock(&self.kept).remove(id)
    }

    /// A number unique to one engine thread, tying it to its `Hosted` entry.
    pub fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::SeqCst)
    }

    pub fn close_session(&self, id: &str) -> Result<(), String> {
        let hosted = lock(&self.sessions)
            .remove(id)
            .ok_or_else(|| format!("session {id} is not open"))?;
        self.shut(id, hosted)
    }

    fn shut(&self, id: &str, hosted: Hosted) -> Result<(), String> {
        // An idle close keeps the engine's options for the next open.
        let keep = lock(&self.close_reasons).get(id) == Some(&"idle");
        let _ = hosted.ops.send(json!({ "op": "shutdown", "keep": keep }));
        self.store
            .record_session_closed(id)
            .map_err(|error| error.to_string())
    }

    /// Called by an engine session's thread every tick: whether work
    /// outside a turn keeps its engine open (see `Hosted::held`). Work
    /// ending counts as activity, so the idle time runs from then.
    pub fn set_held(&self, id: &str, held: bool) {
        if let Some(hosted) = lock(&self.sessions).get_mut(id) {
            if hosted.held != held {
                hosted.held = held;
                hosted.last_active = Instant::now();
            }
        }
    }

    /// Something happened in a hosted session: its idle time starts again.
    fn touch(&self, id: &str) {
        if let Some(hosted) = lock(&self.sessions).get_mut(id) {
            hosted.last_active = Instant::now();
        }
    }

    /// Closes the engines of engine sessions nobody watches with nothing
    /// going on: those idle for `ENGINE_IDLE_CLOSE`, then, while more than
    /// `MAX_LIVE_ENGINES` engine processes run, the least recently active
    /// ones idle for `CAP_MIN_IDLE`. Nothing is going on when no turn runs,
    /// no message waits or is being delivered, no work is held
    /// (`Hosted::held`), no title or handoff is being written and no other
    /// session waits on its reply. Clients get `session_closed` with reason
    /// `idle`. `engine_sessions`: the hosted sessions an engine process
    /// runs (not LynShen's own, which close on their thread). Returns the
    /// closed sessions.
    pub(crate) fn close_idle_engines(
        &self,
        at: Instant,
        engine_sessions: &HashSet<String>,
    ) -> Vec<String> {
        // Before the sessions lock (a send holds its state while it forwards).
        let awaited: HashSet<String> = engine_sessions
            .iter()
            .filter(|id| crate::session_messages::awaited(self, id))
            .cloned()
            .collect();
        let idle_close = engine_idle_close();
        let mut closing = Vec::new();
        {
            // Checked and removed under one lock: an op that reaches the
            // session first has made it active (`forward`), and one that
            // comes after finds it asleep (`wake`).
            let mut sessions = lock(&self.sessions);
            let busy = lock(&self.busy).clone();
            let claims = lock(&self.claims).clone();
            let untitled = lock(&self.untitled).clone();
            let titling = lock(&self.titling).clone();
            let mut idle: Vec<(String, Duration)> = sessions
                .iter()
                .filter(|(id, hosted)| {
                    engine_sessions.contains(*id)
                        && hosted.watchers.is_empty()
                        && !hosted.held
                        && hosted.opened.elapsed() >= OPEN_GRACE
                        && !busy.contains(*id)
                        && !claims.contains_key(*id)
                        && !untitled.contains(*id)
                        && !titling.contains(*id)
                        && !awaited.contains(*id)
                })
                .map(|(id, hosted)| {
                    let idle = at.saturating_duration_since(hosted.last_active);
                    (id.clone(), idle)
                })
                .collect();
            // Longest idle first.
            idle.sort_by_key(|entry| std::cmp::Reverse(entry.1));
            let mut live = sessions
                .keys()
                .filter(|id| engine_sessions.contains(*id))
                .count();
            for (id, idle_for) in idle {
                let over = live > MAX_LIVE_ENGINES && idle_for >= CAP_MIN_IDLE;
                if idle_for < idle_close && !over {
                    continue;
                }
                if let Some(hosted) = sessions.remove(&id) {
                    live -= 1;
                    // Before the lock goes: an op right after finds it
                    // asleep (`wake`), never just gone.
                    lock(&self.asleep).insert(id.clone(), engines::Options::default());
                    lock(&self.close_reasons).insert(id.clone(), "idle");
                    closing.push((id, hosted));
                }
            }
        }
        let mut closed = Vec::new();
        for (id, hosted) in closing {
            if self.shut(&id, hosted).is_ok() {
                closed.push(id);
            }
        }
        closed
    }

    /// The options an engine closed as idle ran with (sent by its thread as
    /// it stops), for its next open.
    pub(crate) fn keep_options(&self, id: &str, options: engines::Options) {
        if let Some(kept) = lock(&self.asleep).get_mut(id) {
            *kept = options;
        }
    }

    /// Before an op goes to `session`: an engine session closed as idle is
    /// opened again, as it ran, and `client` (which watched it) watches it
    /// again. Every client then gets `session_reopened`, so one that put it
    /// to sleep can watch it again; clients that open it themselves first
    /// (`session_open`, `watch`) never need this.
    pub fn wake(self: &Arc<Self>, client: u64, session: &str) -> Result<(), String> {
        if lock(&self.sessions).contains_key(session) || !lock(&self.asleep).contains_key(session) {
            return Ok(());
        }
        self.open_session(session, None)?;
        self.set_watch(client, session, true);
        self.broadcast(&json!({ "type": "session_reopened", "session": session }));
        Ok(())
    }

    /// Called by a session thread when its engine stops on its own (`/quit`
    /// or a failed open), so the session no longer counts as hosted.
    ///
    /// `generation` is the ending thread's: when the session was closed and
    /// reopened while that thread was still finishing, the entry now belongs
    /// to the new engine, which must stay hosted.
    pub fn session_ended(&self, id: &str, generation: u64) {
        {
            let mut sessions = lock(&self.sessions);
            match sessions.get(id) {
                Some(hosted) if hosted.generation == generation => {
                    sessions.remove(id);
                    let _ = self.store.record_session_closed(id);
                }
                // Reopened while this engine was stopping: the new one is
                // live, and why the old one closed says nothing about it.
                Some(_) => {
                    lock(&self.close_reasons).remove(id);
                    return;
                }
                None => {}
            }
        }
        crate::session_messages::ended(self, id);
        self.connectors.session_closed(id);
        lock(&self.busy).remove(id);
        lock(&self.claims).remove(id);
        // Images waiting for an echo the closed engine will not send.
        lock(&self.sent_images).remove(id);
        self.usage.close(id);
        self.usage.forget(id);
        let mut closed = json!({ "type": "session_closed", "session": id });
        if let Some(reason) = lock(&self.close_reasons).remove(id) {
            closed["reason"] = json!(reason);
        }
        self.broadcast(&closed);
    }

    /// Sends `op` to every open LynShen session (other engines have no MCP
    /// servers of the daemon's).
    pub fn forward_to_lynshen_sessions(&self, op: &Value) {
        let lynshen: HashSet<String> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.engine.is_none())
            .map(|record| record.id)
            .collect();
        for (id, hosted) in lock(&self.sessions).iter() {
            if lynshen.contains(id) {
                let _ = hosted.ops.send(op.clone());
            }
        }
    }

    pub fn forward(&self, session: &str, mut op: Value) -> Result<(), String> {
        if op["op"] == "set_approval_mode" && crate::requirements::gated(self, session) {
            op["mode"] = json!(crate::requirements::read_only(self, session));
        }
        let mut sessions = lock(&self.sessions);
        let hosted = sessions
            .get_mut(session)
            .ok_or_else(|| format!("session {session} is not open"))?;
        hosted.last_active = Instant::now();
        // The engine's thread may be held by a hook (a long stop hook) and
        // take the op only once it ends.
        if op["op"] == "interrupt" {
            if let Some(stopper) = &hosted.hook_stopper {
                stopper.stop();
            }
        }
        hosted
            .ops
            .send(op)
            .map_err(|_| format!("session {session} has stopped"))
    }

    pub fn set_watch(&self, client: u64, session: &str, watch: bool) {
        let mut sessions = lock(&self.sessions);
        let Some(hosted) = sessions.get_mut(session) else {
            return;
        };
        let before = !hosted.watchers.is_empty();
        // Watched until now: its idle time starts when the last one leaves.
        hosted.last_active = Instant::now();
        if watch {
            hosted.watchers.insert(client);
        } else {
            hosted.watchers.remove(&client);
        }
        let after = !hosted.watchers.is_empty();
        if watch {
            let _ = hosted
                .ops
                .send(json!({ "op": "snapshot", "client": client }));
        }
        if before != after {
            let _ = hosted
                .ops
                .send(json!({ "op": "set_attended", "attended": after }));
        }
    }

    /// Called by a session thread with its engine's state every tick. A
    /// The desktop brought a newer daemon (an app update): this one exits
    /// once no session is running, and the desktop starts the new one, which
    /// reopens the sessions. Running turns are never cut off.
    pub fn restart_when_idle(&self) {
        if self.restart_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(hub) = self.me.upgrade() else {
            return;
        };
        let asked = Instant::now();
        thread::spawn(move || loop {
            // Nothing running, and nothing held (an engine's TUI or
            // background work) or held too long.
            let held = lock(&hub.sessions).values().any(|hosted| hosted.held)
                && asked.elapsed() < RESTART_HOLD;
            if lock(&hub.busy).is_empty() && !held {
                hub.broadcast(&json!({ "type": "daemon_restarting" }));
                thread::sleep(std::time::Duration::from_millis(300));
                // exit runs no destructors: the sessions' locks go first, or
                // the next daemon cannot reopen them where it cannot tell this
                // one is gone (Windows).
                lynshen_agent_core::release_session_locks();
                lynshen_agent_core::sandbox::release_os_sandbox();
                // As on a signal: nothing this daemon started outlives it
                // (engines and what they started, tool commands).
                lynshen_agent_core::terminate_tool_processes();
                std::process::exit(0);
            }
            thread::sleep(std::time::Duration::from_secs(1));
        });
    }

    /// session with an unprocessed delivery stays busy.
    pub fn set_busy(&self, session: &str, busy: bool) {
        let busy = busy
            || lock(&self.claims)
                .get(session)
                .is_some_and(|count| *count > 0);
        let changed = if busy {
            lock(&self.busy).insert(session.to_string())
        } else {
            lock(&self.busy).remove(session)
        };
        if changed {
            self.touch(session);
            self.broadcast(&self.agents_json());
            self.broadcast(&self.sessions_json());
        }
    }

    /// A client watches `session` now (it is attended).
    pub fn watched(&self, session: &str) -> bool {
        lock(&self.sessions)
            .get(session)
            .is_some_and(|hosted| !hosted.watchers.is_empty())
    }

    /// A session is running a turn or has messages waiting.
    pub fn is_busy(&self, session: &str) -> bool {
        lock(&self.busy).contains(session)
    }

    /// The end of a session's latest reply, as far as this daemon saw it.
    pub fn last_reply(&self, session: &str) -> Option<String> {
        let turns = lock(&self.turns);
        let tail = turns.get(session)?.tail().trim();
        (!tail.is_empty()).then(|| tail.to_string())
    }

    /// Sends `content` to a session as the user's next message, holding its
    /// running slot like a delivered message (see `deliver`).
    pub fn send_to_session(&self, session: &str, content: &str) -> Result<(), String> {
        *lock(&self.claims).entry(session.to_string()).or_default() += 1;
        lock(&self.busy).insert(session.to_string());
        self.forward(
            session,
            json!({ "op": "user_message", "content": content, "claimed": true }),
        )
        .inspect_err(|_| self.release_failed_claim(session))
    }

    /// A claimed message did not reach the session: its claim goes, and so
    /// does the running slot of a session that is not hosted (no engine
    /// thread clears it; a hosted one, or one ending, does it itself).
    fn release_failed_claim(&self, session: &str) {
        self.release_claim(session);
        if !self.is_open(session) {
            lock(&self.busy).remove(session);
        }
    }

    /// Notifies the paired phones (see `push`).
    /// This hub, for a thread that outlives the caller's borrow.
    pub fn handle(&self) -> Option<Arc<Hub>> {
        self.me.upgrade()
    }

    pub fn notify(&self, title: &str, body: &str, tag: &str) {
        self.notify_with(crate::push::Notice {
            kind: crate::push::Kind::Update,
            level: crate::push::Level::Normal,
            tag,
            title,
            body,
            url: None,
            session: None,
            answer: None,
        });
    }

    /// `notify` whose notification opens `url` on the remote page.
    pub fn notify_at(&self, title: &str, body: &str, tag: &str, url: &str) {
        self.notify_with(crate::push::Notice {
            kind: crate::push::Kind::Update,
            level: crate::push::Level::Normal,
            tag,
            title,
            body,
            url: Some(url),
            session: None,
            answer: None,
        });
    }

    /// A notice with its kind and level (see `push_policy`); the chat bots
    /// that want notices get it too.
    pub fn notify_with(&self, notice: crate::push::Notice) {
        self.connectors
            .notify(notice.title, notice.body, notice.tag);
        if let Some(hub) = self.me.upgrade() {
            crate::push::notify(&hub, notice);
        }
    }

    /// A session's conversation as the title model sees it for a note: its
    /// first and latest requests and the end of its latest reply.
    pub fn turn_excerpt(&self, session: &str) -> Option<String> {
        let title = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .and_then(|record| record.title)
            .unwrap_or_default();
        lock(&self.turns)
            .get(session)
            .map(|turns| turns.handoff_prompt(&title, None))
    }

    /// The session thread has handed a delivered message to its engine.
    pub fn release_claim(&self, session: &str) {
        let mut claims = lock(&self.claims);
        if let Some(count) = claims.get_mut(session) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                claims.remove(session);
            }
        }
    }

    pub fn agents_json(&self) -> Value {
        let records = self.store.sessions();
        let busy = lock(&self.busy).clone();
        let list: Vec<Value> = self
            .agents
            .list()
            .into_iter()
            .map(|agent| {
                let sessions: Vec<&str> = records
                    .iter()
                    .filter(|record| record.agent.as_deref() == Some(agent.id.as_str()))
                    .map(|record| record.id.as_str())
                    .collect();
                let mut value = agent.to_json();
                value["summary"] = json!(self.agents.summary(&agent.id));
                value["sessions"] = json!(sessions.len());
                let running: Vec<&str> = sessions
                    .iter()
                    .copied()
                    .filter(|id| busy.contains(*id))
                    .collect();
                value["busy"] = json!(!running.is_empty());
                value["running"] = json!(running);
                value
            })
            .collect();
        json!({ "type": "agents", "agents": list })
    }

    /// Deletes an agent none of whose sessions is running: its folder
    /// (brief, memory, schedules), its timers and its open questions. Its
    /// open sessions close; their records stay. Messages still waiting for
    /// it become undeliverable on the next delivery pass.
    pub fn delete_agent(&self, id: &str) -> Result<(), String> {
        // No message reaches the agent (or starts a session of it) meanwhile.
        let _guard = lock(&self.delivering);
        if self.agents.get(id).is_none() {
            return Err(format!("unknown agent {id}"));
        }
        let own: Vec<String> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.agent.as_deref() == Some(id))
            .map(|record| record.id)
            .collect();
        if own.iter().any(|session| lock(&self.busy).contains(session)) {
            return Err(format!(
                "agent {id} is working; delete it once its sessions are idle"
            ));
        }
        // Holding the schedules while the folder goes keeps a run that fires
        // meanwhile from writing schedules.json back into it.
        let mut schedules = lock(&self.schedules);
        self.agents.delete(id)?;
        schedules.retain(|schedule| schedule.agent != id);
        drop(schedules);
        for session in &own {
            let _ = self.close_session(session);
        }
        for timer in self.store.active_timers() {
            if timer.agent == id {
                let _ = self.cancel_timer(&timer.id, None);
            }
        }
        for question in self.store.open_questions() {
            if question.agent == id {
                let _ = self.store.record_answer(&question.id, "", "agent_deleted");
            }
        }
        self.broadcast(&self.agents_json());
        self.broadcast(&self.schedules_json(None));
        self.broadcast(&self.questions_json());
        Ok(())
    }

    /// A fresh id with a readable prefix (`m-…`, `t-…`).
    pub fn new_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}-{}-{}",
            now(),
            self.next_id.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// Records a message and tries to deliver it right away. Returns false
    /// when a message with the same dedupe key was already recorded.
    pub fn send_message(self: &Arc<Self>, message: Message) -> Result<bool, String> {
        if self.agents.get(&message.to).is_none() {
            return Err(format!("unknown agent {}", message.to));
        }
        let fresh = self
            .store
            .record_message(&message)
            .map_err(|error| error.to_string())?;
        if fresh {
            self.deliver_pending();
        }
        Ok(fresh)
    }

    pub fn set_timer(&self, timer: &Timer) -> Result<(), String> {
        self.store
            .record_timer(timer)
            .map_err(|error| error.to_string())
    }

    pub fn cancel_timer(&self, id: &str, agent: Option<&str>) -> Result<(), String> {
        let _guard = lock(&self.timer_updates);
        if !self
            .store
            .active_timers()
            .iter()
            .any(|timer| timer.id == id && agent.is_none_or(|agent| timer.agent == agent))
        {
            return Err(format!("no active timer {id}"));
        }
        self.store
            .record_timer_done(id, "cancelled")
            .map_err(|error| error.to_string())
    }

    /// One scheduler pass: due timers, due schedules and overdue questions
    /// become messages, then everything pending is delivered.
    pub fn tick(self: &Arc<Self>) {
        self.fire_due_timers();
        self.fire_due_schedules();
        self.expire_questions();
        self.deliver_pending();
        crate::push::tick(self);
        self.sweep_idle_engines();
        self.scan_dev_servers();
        // The daemon runs for days: a tokenizer no session used for a while
        // is loaded again by the next count.
        lynshen_agent_core::release_idle_tokenizers(TOKENIZER_IDLE);
    }

    /// Looks for dev servers when a scan is due, off the scheduler thread
    /// (listing sockets takes a moment), and tells the clients of changes.
    fn scan_dev_servers(self: &Arc<Self>) {
        let at = now();
        if !self.dev_servers.due(at) {
            return;
        }
        let sessions: Vec<crate::devservers::SessionDir> = lock(&self.sessions)
            .iter()
            .map(|(id, hosted)| crate::devservers::SessionDir {
                id: id.clone(),
                cwd: hosted.cwd.clone(),
                last_active: at.saturating_sub(hosted.last_active.elapsed().as_millis() as u64),
            })
            .collect();
        let hub = Arc::clone(self);
        thread::spawn(move || {
            if let Some(servers) = hub.dev_servers.scan(&sessions) {
                hub.broadcast(&crate::devservers::servers_json(&servers));
            }
        });
    }

    /// Every `IDLE_SWEEP_EVERY`: idle engine sessions close
    /// (`close_idle_engines`).
    fn sweep_idle_engines(&self) {
        static LAST: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut last = lock(&LAST);
            if last.is_some_and(|at| at.elapsed() < IDLE_SWEEP_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let hosted: HashSet<String> = lock(&self.sessions).keys().cloned().collect();
        if hosted.is_empty() {
            return;
        }
        let engine_sessions: HashSet<String> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.engine.is_some() && hosted.contains(&record.id))
            .map(|record| record.id)
            .collect();
        self.close_idle_engines(Instant::now(), &engine_sessions);
    }

    pub fn ask(&self, question: &Question) -> Result<(), String> {
        self.store
            .record_question(question)
            .map_err(|error| error.to_string())?;
        self.broadcast(&self.questions_json());
        Ok(())
    }

    /// Answers a question: the answer goes back to the session that asked,
    /// waking it. `by` is `user` or `deadline`. Errors when the question is
    /// unknown or already answered.
    pub fn answer_question(
        self: &Arc<Self>,
        id: &str,
        answer: &str,
        by: &str,
    ) -> Result<(), String> {
        let question = self
            .store
            .question(id)
            .ok_or_else(|| format!("unknown question {id}"))?;
        if !self
            .store
            .record_answer(id, answer, by)
            .map_err(|error| error.to_string())?
        {
            return Err(format!("question {id} is already answered"));
        }
        self.broadcast(&self.questions_json());
        let body = if by == "deadline" {
            format!(
                "Q: {}\nNo answer by the deadline. Go ahead with your default: {}",
                question.title, question.default_action
            )
        } else {
            format!("Q: {}\nA: {answer}", question.title)
        };
        // The dispatcher has no `open_items` tool.
        let others = match question.agent.as_str() {
            crate::dispatch::AGENT => 0,
            agent => self.open_items_of(agent).len(),
        };
        let body = if others > 0 {
            format!(
                "{body}\n\nYou have {others} other open item(s) waiting for the user. Check them with `open_items` and close any this answer made unnecessary."
            )
        } else {
            body
        };
        self.store
            .record_message(&Message {
                id: self.new_id("m"),
                to: question.agent,
                from: format!("question:{id}"),
                body,
                session: Some(question.session),
                reply_to: None,
                dedupe_key: Some(format!("question:{id}")),
                at: now(),
            })
            .map_err(|error| error.to_string())?;
        self.deliver_pending();
        Ok(())
    }

    fn expire_questions(self: &Arc<Self>) {
        for question in self.store.open_questions() {
            if question.due_at.is_some_and(|due| due <= now()) {
                let _ = self.answer_question(&question.id, "", "deadline");
            }
        }
    }

    pub fn post_report(&self, report: &Report) -> Result<(), String> {
        self.store
            .record_report(report)
            .map_err(|error| error.to_string())?;
        self.broadcast(&json!({ "type": "report_posted", "report": report_json(report) }));
        Ok(())
    }

    /// Open questions, and the ones closed lately (each with who closed it,
    /// why and when) so a client can offer to reopen them.
    pub fn questions_json(&self) -> Value {
        let list: Vec<Value> = self
            .store
            .open_questions()
            .iter()
            .map(question_json)
            .collect();
        let closed: Vec<Value> = self
            .store
            .closed_questions(now().saturating_sub(CLOSED_KEEP_MS))
            .iter()
            .map(|(question, closure)| with_closure(question_json(question), closure))
            .collect();
        json!({ "type": "questions", "questions": list, "closed": closed })
    }

    pub fn reports_json(&self, limit: usize) -> Value {
        let list: Vec<Value> = self.store.reports(limit).iter().map(report_json).collect();
        json!({ "type": "reports", "reports": list })
    }

    /// Open actions, and the ones closed lately (see `questions_json`).
    pub fn actions_json(&self) -> Value {
        let list: Vec<Value> = self
            .store
            .open_actions()
            .iter()
            .map(|action| action.to_json())
            .collect();
        let closed: Vec<Value> = self
            .store
            .closed_actions(now().saturating_sub(CLOSED_KEEP_MS))
            .iter()
            .map(|(action, closure)| with_closure(action.to_json(), closure))
            .collect();
        json!({ "type": "actions", "actions": list, "closed": closed })
    }

    /// `agent`'s open questions and the open actions of its sessions, as
    /// its `open_items` tool lists them.
    pub fn open_items_of(&self, agent: &str) -> Vec<Value> {
        let own: HashSet<String> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.agent.as_deref() == Some(agent))
            .map(|record| record.id)
            .collect();
        let questions = self
            .store
            .open_questions()
            .into_iter()
            .filter(|question| question.agent == agent)
            .map(|question| {
                json!({
                    "kind": "question", "id": question.id, "session": question.session,
                    "title": question.title, "assumption": question.assumption,
                    "asked_at_unix": question.asked_at / 1000,
                })
            });
        let actions = self
            .store
            .open_actions()
            .into_iter()
            .filter(|action| own.contains(&action.session_id))
            .map(|action| {
                json!({
                    "kind": "action", "id": action.id, "session": action.session_id,
                    "tool": action.name, "summary": action.summary,
                    "asked_at_unix": action.created_at / 1000,
                })
            });
        questions.chain(actions).collect()
    }

    /// Closes an open question or action without answering it. `by` is
    /// `user`, `agent:<id>` or `superseded`; an agent closing one tells the
    /// user's phone, since nobody asked for it.
    pub fn close_item(
        &self,
        kind: ItemKind,
        id: &str,
        by: &str,
        reason: &str,
    ) -> Result<(), String> {
        let title = match kind {
            ItemKind::Question => self.store.question(id).map(|q| q.title),
            ItemKind::Action => self
                .store
                .open_actions()
                .into_iter()
                .find(|action| action.id == id)
                .map(|action| format!("{} {}", action.name, action.summary)),
        };
        if !self
            .store
            .close_item(kind, id, by, reason.trim())
            .map_err(|error| error.to_string())?
        {
            return Err(format!("{id} is not open"));
        }
        self.broadcast_items(kind);
        if by.starts_with("agent:") {
            let title = title.unwrap_or_else(|| id.to_string());
            self.notify(&format!("已关闭：{title}"), reason, &format!("closed:{id}"));
        }
        Ok(())
    }

    /// Reopens a closed item; its session comes back out of the archive.
    pub fn reopen_item(&self, kind: ItemKind, id: &str) -> Result<(), String> {
        let session = match kind {
            ItemKind::Question => self.store.question(id).map(|q| q.session),
            ItemKind::Action => self
                .store
                .closed_actions(0)
                .into_iter()
                .find(|(action, _)| action.id == id)
                .map(|(action, _)| action.session_id),
        };
        if !self
            .store
            .reopen_item(kind, id)
            .map_err(|error| error.to_string())?
        {
            return Err(format!("{id} is not closed"));
        }
        self.broadcast_items(kind);
        let archived = self
            .store
            .sessions()
            .into_iter()
            .any(|record| Some(&record.id) == session.as_ref() && record.archived);
        if let (true, Some(session)) = (archived, session) {
            if let Ok(true) = self
                .store
                .record_session_meta(&session, &json!({ "archived": false }))
            {
                self.broadcast(&self.sessions_json());
            }
        }
        Ok(())
    }

    fn broadcast_items(&self, kind: ItemKind) {
        self.broadcast(&match kind {
            ItemKind::Question => self.questions_json(),
            ItemKind::Action => self.actions_json(),
        });
    }

    /// A scheduled task started a new run in `current`: its earlier runs
    /// are done with. Their open questions and actions close (the new run
    /// asks again if it still needs to) and the runs are archived, so the
    /// agent's page shows the latest run. A run still working is left.
    pub(crate) fn retire_schedule_runs(&self, schedule: &str, name: &str, current: &str) {
        let earlier: Vec<String> = self
            .store
            .sessions_reached_from(&format!("schedule:{schedule}"))
            .into_iter()
            .filter(|session| session != current && !self.is_busy(session))
            .collect();
        if earlier.is_empty() {
            return;
        }
        let reason = format!("定时任务「{name}」已开始新一次运行");
        for question in self.store.open_questions() {
            if earlier.contains(&question.session) {
                let _ = self.close_item(ItemKind::Question, &question.id, "superseded", &reason);
            }
        }
        for action in self.store.open_actions() {
            if earlier.contains(&action.session_id) {
                let _ = self.close_item(ItemKind::Action, &action.id, "superseded", &reason);
            }
        }
        let mut archived = false;
        for record in self.store.sessions() {
            if earlier.contains(&record.id) && !record.archived {
                archived |= self
                    .store
                    .record_session_meta(&record.id, &json!({ "archived": true }))
                    .unwrap_or(false);
            }
        }
        if archived {
            self.broadcast(&self.sessions_json());
        }
    }

    /// Turns due timers into messages. The timer id is the message's dedupe
    /// key, so a timer that fired just before a crash fires only once.
    fn fire_due_timers(self: &Arc<Self>) {
        let _guard = lock(&self.timer_updates);
        let due: Vec<Timer> = self
            .store
            .active_timers()
            .into_iter()
            .filter(|timer| timer.fire_at <= now())
            .collect();
        for timer in due {
            let message = Message {
                id: self.new_id("m"),
                to: timer.agent.clone(),
                from: format!("timer:{}", timer.id),
                body: timer.body.clone(),
                session: timer.session.clone(),
                reply_to: None,
                dedupe_key: Some(format!("timer:{}", timer.id)),
                at: now(),
            };
            match self.store.record_message(&message) {
                Ok(_) => {
                    let _ = self.store.record_timer_done(&timer.id, "fired");
                }
                Err(error) => {
                    lynshen_agent_core::log_warn!(
                        "daemon",
                        "timer not fired",
                        error = error.to_string()
                    );
                }
            }
        }
    }

    /// Delivers every pending message that can go now, oldest first. A
    /// message that would start a new run waits while `MAX_RUNNING` runs are
    /// in progress; one for a busy session joins that session's queue.
    pub fn deliver_pending(self: &Arc<Self>) {
        let _guard = lock(&self.delivering);
        for message in self.store.pending_messages() {
            match self.route(&message) {
                Ok(target) => {
                    let busy = target
                        .as_deref()
                        .is_some_and(|session| lock(&self.busy).contains(session));
                    if !busy && lock(&self.busy).len() >= MAX_RUNNING {
                        continue;
                    }
                    if let Err(error) = self.deliver(&message, target) {
                        lynshen_agent_core::log_warn!("daemon", "delivery failed", error = error);
                    }
                }
                Err(reason) => {
                    let _ = self.store.record_undeliverable(&message.id, &reason);
                }
            }
        }
    }

    /// The session a message goes to (None: a new session). Errors mean it
    /// can never be delivered.
    fn route(&self, message: &Message) -> Result<Option<String>, String> {
        let agent = self
            .agents
            .get(&message.to)
            .ok_or_else(|| format!("unknown agent {}", message.to))?;
        if !agent.enabled {
            return Err(format!("agent {} is disabled", agent.id));
        }
        let own: Vec<_> = self
            .store
            .sessions()
            .into_iter()
            .filter(|record| record.agent.as_deref() == Some(agent.id.as_str()))
            .collect();
        let owned = |session: &str| own.iter().any(|record| record.id == session);
        if let Some(session) = &message.session {
            return if owned(session) {
                Ok(Some(session.clone()))
            } else {
                Err(format!("session {session} does not belong to {}", agent.id))
            };
        }
        if let Some(earlier) = &message.reply_to {
            if let Some(session) = self.store.delivered_session(earlier).filter(|s| owned(s)) {
                return Ok(Some(session));
            }
        }
        // Anything else is a new task, in a new session; what earlier
        // sessions concluded reaches it through their handoff notes.
        Ok(None)
    }

    fn deliver(self: &Arc<Self>, message: &Message, target: Option<String>) -> Result<(), String> {
        let session = match target {
            Some(session) => {
                self.open_session(&session, None)?;
                session
            }
            None => self.create_session(None, Some(&message.to), false)?,
        };
        // A delivered message starts (or queues) a run: claim the slot before
        // forwarding, so the next message sees it taken.
        *lock(&self.claims).entry(session.clone()).or_default() += 1;
        lock(&self.busy).insert(session.clone());
        if let Err(error) = self.forward(
            &session,
            json!({ "op": "user_message", "content": delivery_text(message), "claimed": true }),
        ) {
            self.release_failed_claim(&session);
            return Err(error);
        }
        self.store
            .record_delivered(&message.id, &session)
            .map_err(|error| error.to_string())?;
        if let Some(schedule) = message.from.strip_prefix("schedule:") {
            self.schedule_delivered(schedule, &session);
        }
        self.broadcast(&json!({
            "type": "message_delivered",
            "id": message.id,
            "agent": message.to,
            "from": message.from,
            "session": session,
        }));
        Ok(())
    }

    /// Every recorded session with whether it is hosted right now.
    /// Every event a session publishes: its title follows the conversation
    /// (see `titles`).
    pub fn observe(&self, session: &str, event: &Value) {
        self.dev_servers.observe(event, now());
        crate::session_messages::observe(self, session, event);
        self.connectors.observe(session, event);
        self.usage.observe(session, event, || {
            let record = self
                .store
                .sessions()
                .into_iter()
                .find(|record| record.id == session)?;
            Some(SessionInfo {
                engine: record.engine.unwrap_or_else(|| "lynshen".to_string()),
                gateway: record.gateway,
                tool_provider: record.tool_provider,
                cwd: record.cwd.to_string_lossy().into_owned(),
            })
        });
        if event["type"] == "user_message" {
            self.note_user_message(session, event["content"].as_str().unwrap_or_default());
        }
        // A name the session has in Claude Code (given there, or the one we
        // gave it) is the user's.
        if event["type"] == "session_title" {
            self.adopt_engine_title(session, event["title"].as_str().unwrap_or_default());
        }
        // `ended`: a turn with a reply ended; `started` / `over`: any turn.
        let (due, ended, started, over, failed) = {
            let mut turns = lock(&self.turns);
            let turns = turns.entry(session.to_string()).or_default();
            let before = turns.done();
            let running = turns.running();
            let due = turns.observe(event);
            (
                due,
                turns.done() != before,
                !running && turns.running(),
                running && !turns.running(),
                turns.take_failed(),
            )
        };
        // Every client follows which sessions failed, watched or not (a
        // desktop marks them in the lists it is not showing).
        if started || over {
            let changed = {
                let mut set = lock(&self.failed);
                if over && failed.is_some() {
                    set.insert(session.to_string())
                } else {
                    set.remove(session)
                }
            };
            if changed {
                self.broadcast(&self.sessions_json());
            }
        }
        if due {
            self.retitle(session);
        }
        if let Some(hub) = self.me.upgrade() {
            crate::push::observe(&hub, session, event, ended, failed.as_deref());
        }
        if let Some(error) = failed {
            self.report_failed_turn(session, &error);
        }
        if ended {
            self.write_handoff(session);
        }
        if let Some(hub) = self.me.upgrade() {
            crate::dispatch::observe(&hub, session, event);
            crate::requirements::observe(&hub, session, event, ended);
        }
    }

    /// An agent session's turn ended on an error. Nobody may be watching
    /// (a scheduled run), so it becomes a report and a notification instead
    /// of only a line in the live view.
    fn report_failed_turn(&self, session: &str, error: &str) {
        let Some(record) = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
        else {
            return;
        };
        let Some(agent) = record.agent else {
            return;
        };
        let title = format!("运行中断：{}", record.title.as_deref().unwrap_or(session));
        let report = Report {
            id: self.new_id("r"),
            agent,
            session: session.to_string(),
            body: format!("这一轮因错误中断，没有完成：\n{error}"),
            title,
            at: now(),
            read: false,
        };
        match self.post_report(&report) {
            Ok(()) => self.notify_with(crate::push::Notice {
                kind: crate::push::Kind::Error,
                level: crate::push::Level::Urgent,
                tag: session,
                title: &report.title,
                body: "这一轮出错了，没有做完",
                url: None,
                session: Some(session),
                answer: None,
            }),
            Err(error) => {
                lynshen_agent_core::log_warn!("daemon", "failure report not saved", error = error)
            }
        }
    }

    /// An agent session's turn ended: the title model rewrites its handoff
    /// note in the background (see `titles`). One at a time per session; a
    /// turn that ends meanwhile is covered by the next.
    fn write_handoff(&self, session: &str) {
        let Some(record) = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
        else {
            return;
        };
        let Some(agent) = record.agent.clone() else {
            return;
        };
        let Some(hub) = self.me.upgrade() else {
            return;
        };
        if !lock(&self.handing_off).insert(session.to_string()) {
            return;
        }
        let title = record.title.clone().unwrap_or_default();
        let previous = self.agents.handoff(&agent, session);
        let Some(prompt) = lock(&self.turns)
            .get(session)
            .map(|turns| turns.handoff_prompt(&title, previous.as_deref()))
        else {
            lock(&self.handing_off).remove(session);
            return;
        };
        let id = session.to_string();
        thread::spawn(move || {
            let reply = lynshen_agent_core::title_completion(titles::HANDOFF_SYSTEM, &prompt);
            lock(&hub.handing_off).remove(&id);
            let note = match reply {
                Ok(reply) => titles::clean_handoff(&reply),
                Err(error) => {
                    lynshen_agent_core::log_warn!("daemon", "handoff note failed", error = error);
                    None
                }
            };
            let Some(note) = note else { return };
            // The title as it is now: the model may have renamed it meanwhile.
            let title = hub
                .store
                .sessions()
                .into_iter()
                .find(|record| record.id == id)
                .and_then(|record| record.title)
                .unwrap_or(title);
            if let Err(error) = hub.agents.save_handoff(&agent, &id, &title, &note, now()) {
                lynshen_agent_core::log_warn!("daemon", "handoff note not saved", error = error);
            }
        });
    }

    /// Asks the title model for a title in the background, unless a client
    /// named the session by hand.
    fn retitle(&self, session: &str) {
        let Some(record) = self.auto_titled(session) else {
            return;
        };
        let Some(hub) = self.me.upgrade() else {
            return;
        };
        if !lock(&self.titling).insert(session.to_string()) {
            return;
        }
        let project = record
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(prompt) = lock(&self.turns)
            .get(session)
            .map(|turns| turns.prompt(&project, record.title.as_deref()))
        else {
            lock(&self.titling).remove(session);
            return;
        };
        let id = session.to_string();
        thread::spawn(move || {
            let reply = lynshen_agent_core::title_completion(titles::SYSTEM, &prompt);
            lock(&hub.titling).remove(&id);
            let title = match reply {
                Ok(reply) => titles::clean(&reply),
                Err(error) => {
                    lynshen_agent_core::log_warn!(
                        "daemon",
                        "conversation title failed",
                        error = error
                    );
                    None
                }
            };
            // Renamed by hand meanwhile, or unchanged: nothing to do.
            let Some(title) = title else { return };
            let Some(record) = hub.auto_titled(&id) else {
                return;
            };
            if record.title.as_deref() != Some(title.as_str())
                && hub
                    .store
                    .record_session_meta(&id, &json!({ "title": title, "title_auto": true }))
                    .is_ok()
            {
                hub.broadcast(&hub.sessions_json());
            }
        });
    }

    fn adopt_engine_title(&self, session: &str, title: &str) {
        let title = title.trim();
        let current = self
            .store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .and_then(|record| record.title);
        if title.is_empty() || current.as_deref() == Some(title) {
            return;
        }
        if let Ok(true) = self
            .store
            .record_session_meta(session, &json!({ "title": title }))
        {
            self.broadcast(&self.sessions_json());
        }
    }

    /// The session's record when its title is ours to write (none yet, or
    /// one the daemon wrote).
    fn auto_titled(&self, session: &str) -> Option<SessionRecord> {
        self.store
            .sessions()
            .into_iter()
            .find(|record| record.id == session)
            .filter(|record| record.title.is_none() || record.title_auto)
    }

    /// A `user_message` op with images: its echo will show them.
    pub fn note_sent_images(&self, session: &str, op: &Value) {
        if op["op"] == "user_message" && op["images"].as_array().is_some_and(|i| !i.is_empty()) {
            let text = op["content"].as_str().unwrap_or_default().to_string();
            lock(&self.sent_images)
                .entry(session.to_string())
                .or_default()
                .push((text, op["images"].clone()));
        }
    }

    /// The images of the message a `user_message` event echoes, so every
    /// client (the remote page too) shows them.
    pub fn attach_sent_images(&self, session: &str, event: &mut Value) {
        if event["type"] != "user_message" || !event["images"].is_null() {
            return;
        }
        let text = event["content"].as_str().unwrap_or_default();
        let mut sent = lock(&self.sent_images);
        let Some(list) = sent.get_mut(session) else {
            return;
        };
        if let Some(at) = list.iter().position(|(t, _)| t == text) {
            event["images"] = list.remove(at).1;
        }
    }

    /// A session accepted a user message: a new session is titled after
    /// its first one (first line, 40 characters); clients show the daemon's titles.
    pub fn note_user_message(&self, session: &str, content: &str) {
        if !lock(&self.untitled).remove(session) {
            return;
        }
        let Some(title) = title_from(content) else {
            return;
        };
        let untouched = self
            .store
            .sessions()
            .iter()
            .any(|record| record.id == session && record.title.is_none());
        if untouched
            && self
                .store
                .record_session_meta(session, &json!({ "title": title, "title_auto": true }))
                .is_ok()
        {
            self.broadcast(&self.sessions_json());
        }
    }

    pub fn sessions_json(&self) -> Value {
        let records = self.store.sessions();
        let saved = saved_by_id(records.iter().map(|record| record.cwd.as_path()));
        let projects = crate::projects::projects(self);
        let sessions = lock(&self.sessions);
        let busy = lock(&self.busy).clone();
        let failed = lock(&self.failed).clone();
        let list: Vec<Value> = records
            .into_iter()
            // An agent's session stays on its agent's page even when a
            // client hid it from a session list (desktops listed them there
            // before they moved to the workbench).
            .filter(|record| !record.hidden || record.agent.is_some())
            .map(|record| {
                let hosted = sessions.get(&record.id);
                let cwd = hosted.map_or_else(|| record.cwd.clone(), |h| h.cwd.clone());
                let summary = saved.get(&record.id);
                json!({
                    "session": record.id,
                    "chat": lynshen_agent_core::chat::is_chat_dir(&cwd),
                    "cwd": cwd.display().to_string(),
                    "created_at": record.created_at,
                    "updated_at": summary.map_or(record.created_at, |s| s.updated_at * 1000),
                    // What the engine saved of it (LynShen sessions): 0 for one
                    // opened without a first message, which clients leave out
                    // of their lists.
                    "entries": summary.map(|s| s.entries),
                    "title": shown_title(&record, summary.map(|s| s.label.as_str())),
                    "archived": record.archived,
                    "group": record.group,
                    // The project it belongs to, wherever it works; null: none.
                    "project": Some(crate::projects::member(&projects, &record)).filter(|p| !p.is_empty()),
                    "gateway": record.gateway,
                    "tool_provider": record.tool_provider.as_deref().filter(|p| !p.is_empty() && !record.gateway),
                    "engine": record.engine.as_deref().unwrap_or("lynshen"),
                    "acp_agent": record.acp_agent,
                    // The conversation it was forked from: {session, turns, at}.
                    "forked_from": record.forked_from,
                    "agent": record.agent,
                    "open": hosted.is_some(),
                    "watchers": hosted.map(|h| h.watchers.len()).unwrap_or(0),
                    // A turn runs (or messages wait): 进行中 on the phone.
                    "running": busy.contains(&record.id),
                    // Its latest turn ended on an error.
                    "failed": failed.contains(&record.id),
                })
            })
            .collect();
        json!({ "type": "sessions", "sessions": list })
    }

    /// Every session saved in `cwd`, newest first, whoever ran it (the
    /// daemon, the TUI or `lynshen serve`), with the daemon's title and
    /// archive state for the ones it knows.
    pub fn session_history(&self, cwd: &std::path::Path) -> Result<Value, String> {
        let saved = lynshen_agent_core::saved_sessions(cwd).map_err(|error| error.to_string())?;
        let records: HashMap<String, SessionRecord> = self
            .store
            .sessions()
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect();
        let sessions = lock(&self.sessions);
        let item = |id: String, label: String, updated_at: u64, entries: Value, engine: &str| {
            let record = records.get(&id);
            json!({
                "title": match record {
                    Some(record) => shown_title(record, Some(&label)),
                    None => title_from(&label),
                },
                "updated_at": updated_at,
                "entries": entries,
                "archived": record.is_some_and(|r| r.archived),
                "agent": record.and_then(|r| r.agent.clone()),
                "open": sessions.contains_key(&id),
                "engine": engine,
                "session": id,
            })
        };
        let mut list: Vec<Value> = saved
            .into_iter()
            .map(|s| {
                item(
                    s.id,
                    s.label,
                    s.updated_at * 1000,
                    json!(s.entries),
                    "lynshen",
                )
            })
            .chain(
                engines::claude::saved(&engines::claude::config_dir(), cwd)
                    .into_iter()
                    .map(|(id, title, updated_at)| {
                        item(id, title, updated_at, Value::Null, "claude")
                    }),
            )
            .chain(
                engines::codex::saved(cwd)
                    .into_iter()
                    .map(|(id, title, updated_at)| {
                        item(id, title, updated_at, Value::Null, "codex")
                    }),
            )
            .collect();
        list.retain(|item| {
            !records
                .get(item["session"].as_str().unwrap_or_default())
                .is_some_and(|record| record.hidden)
        });
        list.sort_by_key(|item| std::cmp::Reverse(item["updated_at"].as_u64().unwrap_or(0)));
        Ok(json!({ "type": "session_history", "cwd": cwd, "sessions": list }))
    }
}

/// A random code of `len` characters from `PAIRING_ALPHABET` (pairing and
/// chat binding codes).
/// The 6-digit check code both sides of a pairing show (the user compares
/// them before the computer trusts the device): from the Noise handshake
/// hash of the pairing connection over the relay, which binds both sides'
/// keys; from the device token's SHA-256 over the LAN. The first 8 bytes,
/// big-endian, modulo 10^6.
pub fn check_code(bytes: &[u8]) -> String {
    let mut first = [0u8; 8];
    for (to, from) in first.iter_mut().zip(bytes) {
        *to = *from;
    }
    format!("{:06}", u64::from_be_bytes(first) % 1_000_000)
}

/// The kind of an approval request waiting in `session` (`call` or a
/// deferred action id), for the scope check.
impl Hub {
    pub(crate) fn approval_kind(&self, session: &str, id: &str) -> Option<String> {
        self.connectors.pending_kind(session, id).or_else(|| {
            self.store
                .open_actions()
                .into_iter()
                .find(|action| action.id == id && action.session_id == session)
                .map(|action| action.kind.unwrap_or_else(|| "tool".to_string()))
        })
    }
}

pub(crate) fn new_code(len: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; len];
    getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes
        .iter()
        .map(|byte| PAIRING_ALPHABET[*byte as usize % PAIRING_ALPHABET.len()] as char)
        .collect())
}

/// Saved-session summaries of the given directories, by session id.
fn saved_by_id<'a>(
    dirs: impl Iterator<Item = &'a std::path::Path>,
) -> HashMap<String, lynshen_agent_core::SessionSummary> {
    let dirs: HashSet<&std::path::Path> = dirs.collect();
    dirs.into_iter()
        .flat_map(|dir| lynshen_agent_core::saved_sessions(dir).unwrap_or_default())
        .map(|summary| (summary.id.clone(), summary))
        .collect()
}

/// How a message reads in the receiving session: the user's own words
/// as-is, anything else with a line saying where it came from.
fn report_json(report: &Report) -> Value {
    json!({
        "id": report.id,
        "agent": report.agent,
        "session": report.session,
        "title": report.title,
        "body": report.body,
        "at": report.at,
        "read": report.read,
    })
}

fn question_json(question: &Question) -> Value {
    json!({
        "id": question.id,
        "agent": question.agent,
        "session": question.session,
        "title": question.title,
        "body": question.body,
        "assumption": question.assumption,
        "default": question.default_action,
        "importance": question.importance,
        "due_at": question.due_at,
        "asked_at": question.asked_at,
    })
}

/// A closed item's JSON with who closed it, why and when.
fn with_closure(mut item: Value, closure: &Closure) -> Value {
    item["closed_by"] = json!(closure.by);
    item["closed_reason"] = json!(closure.reason);
    item["closed_at"] = json!(closure.at);
    item
}

/// A session's title as clients see it: its own, else its first prompt; never
/// an id or a delivery header (which older daemons wrote as the title).
pub(crate) fn shown_title(record: &SessionRecord, label: Option<&str>) -> Option<String> {
    record
        .title
        .clone()
        .filter(|title| {
            !(record.title_auto
                && DELIVERY_HEADERS
                    .iter()
                    .any(|header| title.starts_with(header)))
        })
        .or_else(|| label.and_then(title_from))
}

/// How `delivery_text` starts the line naming where a message came from.
/// Clients show a message with one of these headers as a notice, not as
/// something the user wrote (LynShen-Desktop `src/lib/delivery.ts`).
const DELIVERY_HEADERS: [&str; 5] = [
    "[message from ",
    "[timer ",
    "[scheduled task ",
    "[answer to your question ",
    "[task ",
];

/// `text` without the delivery header line `delivery_text` put first: what
/// was actually asked, for titles and handoff notes.
pub(crate) fn without_delivery_header(text: &str) -> &str {
    let text = text.trim_start();
    if let Some(message) = crate::session_messages::unwrapped(text) {
        return message;
    }
    if DELIVERY_HEADERS
        .iter()
        .any(|header| text.starts_with(header))
    {
        return text.split_once('\n').map_or("", |(_, rest)| rest);
    }
    text
}

/// A title from what was asked: its first line, at most 40 characters.
pub(crate) fn title_from(text: &str) -> Option<String> {
    let title: String = without_delivery_header(text)
        .trim()
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(40)
        .collect();
    let title = title
        .trim()
        .trim_end_matches([':', '：'])
        .trim()
        .to_string();
    (!title.is_empty()).then_some(title)
}

fn delivery_text(message: &Message) -> String {
    if message.from == "user" {
        return message.body.clone();
    }
    let origin = match message.from.split_once(':') {
        Some(("agent", id)) => format!("message from agent {id}"),
        Some(("timer", id)) => format!("timer {id} fired"),
        Some(("schedule", id)) => format!("scheduled task {id}"),
        Some(("question", id)) => format!("answer to your question {id}"),
        Some(("task", id)) => format!("task {id} update"),
        _ => format!("message from {}", message.from),
    };
    format!("[{origin} · {}]\n{}", message.id, message.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{store::now, Agents};
    use std::{fs, sync::mpsc};

    #[test]
    fn the_check_code_is_the_first_8_bytes_modulo_a_million() {
        // The phone page computes the same (src/lib/relay/socket.ts).
        let mut bytes = [0u8; 32];
        bytes[5..8].copy_from_slice(&[0x0f, 0x42, 0x41]);
        assert_eq!(check_code(&bytes), "000001");
        assert_eq!(check_code(&[0xff; 32]), "551615");
    }

    #[test]
    fn titles_come_from_what_was_asked_never_a_delivery_header() {
        let scheduled = delivery_text(&Message {
            id: "m-1".into(),
            to: "ops".into(),
            from: "schedule:sch-1".into(),
            body: "定时任务「每日巡检」：\n检查部署".into(),
            session: None,
            reply_to: None,
            dedupe_key: None,
            at: 0,
        });
        assert_eq!(
            title_from(&scheduled).as_deref(),
            Some("定时任务「每日巡检」")
        );
        assert_eq!(
            without_delivery_header(&scheduled),
            "定时任务「每日巡检」：\n检查部署"
        );
        assert_eq!(
            title_from("  修复登录跳转\n细节").as_deref(),
            Some("修复登录跳转")
        );
        assert_eq!(title_from("[timer t-1 fired · m-2]\n"), None);

        // An older daemon's header title gives way to the first prompt.
        let record = SessionRecord {
            id: "s1".into(),
            cwd: "/tmp".into(),
            agent: Some("ops".into()),
            created_at: 0,
            closed: false,
            title: Some("[scheduled task sch-1 · m-".into()),
            title_auto: true,
            archived: false,
            hidden: false,
            engine: None,
            gateway: false,
            group: None,
            project: None,
            conversation: None,
            acp_agent: None,
            forked_from: None,
            tool_provider: None,
        };
        assert_eq!(
            shown_title(&record, Some(&scheduled)).as_deref(),
            Some("定时任务「每日巡检」")
        );
        assert_eq!(shown_title(&record, Some("")), None);
        let named = SessionRecord {
            title: Some("我的标题".into()),
            title_auto: false,
            ..record
        };
        assert_eq!(shown_title(&named, None).as_deref(), Some("我的标题"));
    }

    #[test]
    fn cancelled_timers_stay_cancelled_and_cannot_race_firing() {
        let dir =
            std::env::temp_dir().join(format!("lynshen-cancel-{}-{}", std::process::id(), now()));
        let open = || {
            Hub::new(
                Store::open(dir.join("daemon")).unwrap(),
                Agents::open(dir.join("agents")).unwrap(),
                "test",
                None,
            )
        };
        let timer = |id: &str| Timer {
            id: id.into(),
            agent: "ops".into(),
            session: None,
            fire_at: 0,
            body: "remind".into(),
        };
        let hub = open();
        hub.set_timer(&timer("t1")).unwrap();
        assert!(hub.cancel_timer("t1", Some("other")).is_err());
        hub.cancel_timer("t1", Some("ops")).unwrap();
        drop(hub);
        let hub = open();
        hub.fire_due_timers();
        assert!(hub.store.message_log(None, 100).is_empty());
        assert!(hub.cancel_timer("t1", None).is_err());

        // Exactly one wins, including cancellation during a scheduler tick.
        for n in 0..20 {
            let id = format!("race-{n}");
            hub.set_timer(&timer(&id)).unwrap();
            let gate = Arc::new(std::sync::Barrier::new(2));
            let firing = hub.clone();
            let start = gate.clone();
            let worker = thread::spawn(move || {
                start.wait();
                firing.fire_due_timers();
            });
            gate.wait();
            let cancelled = hub.cancel_timer(&id, None).is_ok();
            worker.join().unwrap();
            let fired = hub
                .store
                .message_log(None, 100)
                .iter()
                .any(|m| m["from"] == format!("timer:{id}"));
            assert_ne!(cancelled, fired);
        }
        drop(hub);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_agent_turn_that_fails_becomes_a_report() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-hub-failed-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        hub.store
            .record_engine_session("a1", &dir, Some("ops"), None, false, None)
            .unwrap();
        hub.store
            .record_engine_session("u1", &dir, None, None, false, None)
            .unwrap();
        for session in ["a1", "u1"] {
            hub.observe(
                session,
                &json!({ "type": "user_message", "content": "巡检" }),
            );
            hub.observe(session, &json!({ "type": "error", "message": "HTTP 502" }));
            hub.observe(session, &json!({ "type": "status", "message": "ready" }));
        }
        let reports = hub.store.reports(10);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].session, "a1");
        assert_eq!(reports[0].agent, "ops");
        assert!(reports[0].body.contains("HTTP 502"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_scheduled_run_retires_the_earlier_ones() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-hub-retire-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        for (message, session) in [("m1", "run1"), ("m2", "run2"), ("m3", "run3")] {
            hub.store
                .record_engine_session(session, &dir, Some("ops"), None, false, None)
                .unwrap();
            hub.store
                .record_message(&Message {
                    id: message.into(),
                    to: "ops".into(),
                    from: "schedule:sch-1".into(),
                    body: "巡检".into(),
                    session: None,
                    reply_to: None,
                    dedupe_key: None,
                    at: 1,
                })
                .unwrap();
            hub.store.record_delivered(message, session).unwrap();
        }
        let ask = |id: &str, session: &str| Question {
            id: id.into(),
            agent: "ops".into(),
            session: session.into(),
            title: id.into(),
            body: String::new(),
            assumption: String::new(),
            default_action: String::new(),
            importance: "normal".into(),
            due_at: None,
            asked_at: 1,
        };
        hub.ask(&ask("q1", "run1")).unwrap();
        hub.ask(&ask("q3", "run3")).unwrap();
        // run2 is still working: left alone.
        lock(&hub.busy).insert("run2".into());

        hub.retire_schedule_runs("sch-1", "巡检", "run3");
        let open: Vec<String> = hub
            .store
            .open_questions()
            .into_iter()
            .map(|q| q.id)
            .collect();
        assert_eq!(open, ["q3"]);
        assert_eq!(hub.store.closed_questions(0)[0].1.by, "superseded");
        let archived = |id: &str| {
            hub.store
                .sessions()
                .iter()
                .any(|r| r.id == id && r.archived)
        };
        assert!(archived("run1") && !archived("run2") && !archived("run3"));
        assert_eq!(hub.open_items_of("ops").len(), 1);

        // Reopening brings its run back out of the archive.
        hub.reopen_item(ItemKind::Question, "q1").unwrap();
        assert!(!archived("run1"));
        assert_eq!(hub.open_items_of("ops").len(), 2);
        assert!(hub.reopen_item(ItemKind::Question, "q1").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_idle_session_closes_only_when_nothing_holds_it() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-hub-idle-close-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        let (ops, _rx) = mpsc::channel();
        let generation = hub.next_generation();
        hub.host("s1".to_string(), ops, dir.clone(), generation);
        let retire = |quiet: bool, generation: u64| {
            hub.retire_idle("s1", generation, || quiet, KeptState::default)
        };
        // Just opened: the client that opened it watches it next.
        assert!(!retire(true, generation));
        let past = Instant::now() - OPEN_GRACE;
        lock(&hub.sessions).get_mut("s1").unwrap().opened = past;
        hub.set_watch(7, "s1", true);
        assert!(!retire(true, generation));
        hub.set_watch(7, "s1", false);
        lock(&hub.busy).insert("s1".into());
        assert!(!retire(true, generation));
        lock(&hub.busy).remove("s1");
        // An op reached it meanwhile, or it belongs to another engine.
        assert!(!retire(false, generation));
        assert!(!retire(true, generation + 1));
        assert!(lock(&hub.sessions).contains_key("s1"));
        assert!(hub.take_kept("s1").is_none());
        assert!(retire(true, generation));
        assert!(!lock(&hub.sessions).contains_key("s1"));
        // What its engine held in memory waits for the next one.
        assert!(hub.take_kept("s1").is_some());
        assert!(hub.take_kept("s1").is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_message_that_cannot_reach_a_closed_session_frees_its_slot() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-hub-failed-send-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        assert!(hub.send_to_session("gone", "hello").is_err());
        assert!(!lock(&hub.claims).contains_key("gone"));
        // Nothing would ever clear it, and restart_when_idle waits for none.
        assert!(!hub.is_busy("gone"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_finishing_engine_leaves_its_successor_hosted() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-hub-generation-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        let (old_ops, _old_rx) = mpsc::channel();
        let old = hub.next_generation();
        hub.host("s1".to_string(), old_ops, dir.clone(), old);
        // Closed and reopened while the old engine thread is still finishing.
        lock(&hub.sessions).remove("s1");
        let (new_ops, _new_rx) = mpsc::channel();
        let new = hub.next_generation();
        hub.host("s1".to_string(), new_ops, dir.clone(), new);

        hub.session_ended("s1", old);
        assert!(lock(&hub.sessions).contains_key("s1"));
        hub.session_ended("s1", new);
        assert!(!lock(&hub.sessions).contains_key("s1"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn every_client_learns_when_a_turn_fails_until_the_next_starts() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-hub-failed-list-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        hub.store
            .record_session("s", std::path::Path::new("/tmp"), None)
            .unwrap();
        let (outbox, frames) = mpsc::channel();
        hub.add_client(Outbox::local(outbox), None);
        let failed = |frames: &mpsc::Receiver<String>| {
            let list: Value = frames
                .try_iter()
                .map(|f| serde_json::from_str::<Value>(&f).unwrap())
                .filter(|f| f["type"] == "sessions")
                .last()
                .unwrap();
            list["sessions"][0]["failed"].as_bool().unwrap()
        };
        let ready = json!({ "type": "status", "message": "ready" });

        hub.observe("s", &json!({ "type": "user_message", "content": "go" }));
        hub.observe("s", &json!({ "type": "error", "message": "rate limited" }));
        hub.observe("s", &ready);
        assert!(failed(&frames));
        hub.observe("s", &json!({ "type": "user_message", "content": "again" }));
        assert!(!failed(&frames));
        hub.observe("s", &ready);
        assert!(
            frames
                .try_iter()
                .all(|f| !f.contains("\"type\":\"sessions\"")),
            "a good turn changes nothing"
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn engine_hub(label: &str) -> (Arc<Hub>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-hub-engines-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            None,
        );
        (hub, dir)
    }

    /// Hosts `id`, opened long enough ago and last active at `active`; the
    /// receiver sees its ops.
    fn host_engine(hub: &Hub, id: &str, active: Instant) -> mpsc::Receiver<Value> {
        let (ops, rx) = mpsc::channel();
        hub.host(id.into(), ops, "/tmp".into(), hub.next_generation());
        let mut sessions = lock(&hub.sessions);
        let hosted = sessions.get_mut(id).unwrap();
        hosted.opened = Instant::now() - OPEN_GRACE;
        hosted.last_active = active;
        rx
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn idle_engine_sessions_close_after_the_idle_time_and_say_why() {
        let (hub, dir) = engine_hub("time");
        let start = Instant::now();
        let at = start + ENGINE_IDLE_CLOSE;
        let old = host_engine(&hub, "old", start);
        let _recent = host_engine(&hub, "recent", start + Duration::from_secs(60));
        let (outbox, frames) = mpsc::channel();
        hub.add_client(Outbox::local(outbox), None);

        assert_eq!(
            hub.close_idle_engines(at, &set(&["old", "recent"])),
            ["old"]
        );
        let shutdown = old.try_recv().unwrap();
        assert_eq!(shutdown["op"], "shutdown");
        assert_eq!(
            shutdown["keep"], true,
            "its options are kept for the next open"
        );
        assert!(lock(&hub.sessions).contains_key("recent"));

        // The engine thread ends: clients learn it closed as idle.
        hub.session_ended("old", 0);
        let closed: Value = frames
            .try_iter()
            .map(|f| serde_json::from_str::<Value>(&f).unwrap())
            .find(|f| f["type"] == "session_closed")
            .unwrap();
        assert_eq!(closed["reason"], "idle");
        assert!(lock(&hub.close_reasons).is_empty());
        assert!(
            lock(&hub.asleep).contains_key("old"),
            "a message opens it again"
        );
        // What its engine ran with waits for the next open.
        let ran = engines::Options {
            model: Some("claude-opus-5-5".into()),
            ..engines::Options::default()
        };
        hub.keep_options("old", ran.clone());
        assert_eq!(lock(&hub.asleep)["old"], ran);
        // An open that fails (no record here) leaves it asleep.
        assert!(hub.open_session("old", None).is_err());
        assert!(lock(&hub.asleep).contains_key("old"));
        // A user close says no reason.
        hub.close_session("recent").unwrap();
        hub.session_ended("recent", 1);
        let closed: Value = frames
            .try_iter()
            .map(|f| serde_json::from_str::<Value>(&f).unwrap())
            .find(|f| f["type"] == "session_closed")
            .unwrap();
        assert!(closed.get("reason").is_none(), "{closed}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn work_a_watcher_or_a_lynshen_engine_keeps_a_session_open() {
        let (hub, dir) = engine_hub("work");
        let start = Instant::now();
        let at = start + ENGINE_IDLE_CLOSE * 4;
        let ids = ["busy", "claimed", "held", "watched", "untitled", "lynshen"];
        let _rx: Vec<_> = ids.iter().map(|id| host_engine(&hub, id, start)).collect();
        lock(&hub.busy).insert("busy".into());
        lock(&hub.claims).insert("claimed".into(), 1);
        lock(&hub.sessions).get_mut("held").unwrap().held = true;
        lock(&hub.sessions)
            .get_mut("watched")
            .unwrap()
            .watchers
            .insert(7);
        lock(&hub.untitled).insert("untitled".into());
        // Every session but `lynshen` runs on an engine process.
        let engines = set(&ids[..5]);

        assert!(hub.close_idle_engines(at, &engines).is_empty());
        assert_eq!(lock(&hub.sessions).len(), ids.len());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn past_the_cap_the_least_recently_active_idle_engines_close() {
        let (hub, dir) = engine_hub("cap");
        let start = Instant::now();
        let at = start + CAP_MIN_IDLE * 3;
        let mut ids = Vec::new();
        let mut rx = Vec::new();
        // Over the cap by three: two idle long enough, the rest just used.
        for i in 0..MAX_LIVE_ENGINES + 3 {
            let active = match i {
                0 => start,
                1 => start + CAP_MIN_IDLE,
                _ => at - CAP_MIN_IDLE + Duration::from_secs(1),
            };
            let id = format!("s{i}");
            rx.push(host_engine(&hub, &id, active));
            ids.push(id);
        }
        let engines: HashSet<String> = ids.into_iter().collect();
        let mut closed = hub.close_idle_engines(at, &engines);
        closed.sort();
        assert_eq!(closed, ["s0", "s1"], "only sessions idle past CAP_MIN_IDLE");
        assert_eq!(lock(&hub.sessions).len(), MAX_LIVE_ENGINES + 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_op_a_turn_or_work_ending_restarts_the_idle_time() {
        let (hub, dir) = engine_hub("touch");
        let start = Instant::now();
        let _sent = host_engine(&hub, "sent", start);
        let _turned = host_engine(&hub, "turned", start);
        let _released = host_engine(&hub, "released", start);
        hub.forward("sent", json!({ "op": "user_message", "content": "hi" }))
            .unwrap();
        hub.set_busy("turned", true);
        hub.set_busy("turned", false);
        lock(&hub.sessions).get_mut("released").unwrap().held = true;
        hub.set_held("released", false);

        let at = Instant::now() + ENGINE_IDLE_CLOSE - Duration::from_secs(5);
        let engines = set(&["sent", "turned", "released"]);
        assert!(hub.close_idle_engines(at, &engines).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_reopened_session_does_not_inherit_the_idle_reason() {
        let (hub, dir) = engine_hub("reopen");
        let start = Instant::now();
        let _old = host_engine(&hub, "s", start);
        let old_generation = lock(&hub.sessions)["s"].generation;
        hub.close_idle_engines(start + ENGINE_IDLE_CLOSE, &set(&["s"]));
        // Opened again before the old engine thread finished.
        let _new = host_engine(&hub, "s", Instant::now());
        hub.session_ended("s", old_generation);
        assert!(lock(&hub.sessions).contains_key("s"));
        assert!(lock(&hub.close_reasons).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_connection_that_falls_too_far_behind_is_dropped() {
        let (hub, dir) = engine_hub("outbox");
        let (outbox, inbox) = Hub::outbox();
        let client = hub.add_client(outbox, None);
        for _ in 0..OUTBOX_LIMIT {
            hub.broadcast(&json!({ "type": "x" }));
        }
        assert!(lock(&hub.clients).contains_key(&client));
        // What it reads makes room again.
        inbox.try_recv().unwrap();
        hub.broadcast(&json!({ "type": "x" }));
        assert!(lock(&hub.clients).contains_key(&client));
        hub.broadcast(&json!({ "type": "x" }));
        assert!(!lock(&hub.clients).contains_key(&client));
        // An in-process client (a chat binding) reads on its own thread and
        // is never dropped for it.
        let (local, _frames) = mpsc::channel();
        let chat = hub.add_client(Outbox::local(local), None);
        for _ in 0..OUTBOX_LIMIT + 5 {
            hub.broadcast(&json!({ "type": "x" }));
        }
        assert!(lock(&hub.clients).contains_key(&chat));
        let _ = fs::remove_dir_all(dir);
    }
}

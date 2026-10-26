//! The host side of the LynShen relay (`docs/relay-protocol.md`): one
//! outbound WebSocket to the relay, authenticated with the daemon's Ed25519
//! identity, carrying client streams. Each stream runs a Noise session
//! (`noise.rs`) and then serves a paired device exactly like a local
//! WebSocket client. The relay only sees ciphertext.
//!
//! Signed in to LynShen, the daemon uses relay v2 (§7): it connects with a
//! relay token minted with the computer's device key, registers as one of
//! the account's computers, and admits clients of that account only (paired
//! devices, pairing codes, or connect requests the desktop allows). Signed
//! out, it uses v1 as before (§1–§6).

use crate::{
    hub::{lock, Hub},
    noise,
    store::{bytes_hash, write_private, Device, DeviceAccount, Store},
    Link, Received,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    net::TcpStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

const IDENTITY_FILE: &str = "relay-identity.json";
/// Signed together with the relay's nonce.
const AUTH_CONTEXT: &[u8] = b"lynshen-relay-v1:";
/// The daemon setting that turns the relay on.
const SETTING: &str = "relay";
/// How long the relay's challenge and a client's first Noise message may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Short reads let one thread both receive and flush, as for local clients.
const POLL: Duration = Duration::from_millis(20);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The relay pings the host every 30 s. A connection this long without a
/// single frame is dead even if the socket never said so: a proxy or a
/// network change can drop it without the close ever reaching us.
const SILENCE: Duration = Duration::from_secs(75);

const OPEN: u8 = 1;
const DATA: u8 = 2;
const CLOSE: u8 = 3;

/// The daemon setting naming the account the computer is registered under.
const USER_SETTING: &str = "relay_user";
/// How long a connect request waits for the desktop's answer (§7.4; unit
/// tests: a second).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 120 });
/// Connect requests open at once, at most.
const MAX_REQUESTS: usize = 3;
/// How often a connected host checks that its mode (v1, v2, off) is still
/// the right one: a sign-in, a sign-out or another account.
const MODE_CHECK: Duration = Duration::from_millis(500);
/// The relay's close code for a server restarting: reconnect soon.
const RESTARTING: u16 = 4503;

/// A binary frame on the host connection (§3.1). `Open` carries the caller
/// the relay vouches for (v2, §7.3).
#[derive(Debug, PartialEq)]
enum Frame {
    Open(u32, Vec<u8>),
    Data(u32, Vec<u8>),
    Close(u32),
}

/// Who opened a stream (§7.3): empty for a v1 client.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Caller {
    pub user: String,
    pub authz: String,
    pub client: String,
    pub name: String,
}

impl Caller {
    fn parse(payload: &[u8]) -> Self {
        let value: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
        Self {
            user: text("user"),
            authz: text("authz"),
            client: text("client"),
            name: text("name"),
        }
    }

    /// A v1 client: no account (an Aogiri release from before v2, or the
    /// v1 relay).
    fn is_v1(&self) -> bool {
        self.user.is_empty()
    }
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let (kind, stream, payload) = match self {
            Frame::Open(stream, payload) => (OPEN, stream, payload.as_slice()),
            Frame::Data(stream, payload) => (DATA, stream, payload.as_slice()),
            Frame::Close(stream) => (CLOSE, stream, &[][..]),
        };
        let mut bytes = Vec::with_capacity(5 + payload.len());
        bytes.push(kind);
        bytes.extend_from_slice(&stream.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let (&kind, rest) = bytes.split_first()?;
        let stream = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
        if stream == 0 {
            return None;
        }
        match kind {
            OPEN => Some(Frame::Open(stream, rest[4..].to_vec())),
            DATA => Some(Frame::Data(stream, rest[4..].to_vec())),
            CLOSE => Some(Frame::Close(stream)),
            _ => None,
        }
    }
}

/// The daemon's relay keys (§1), kept in `relay-identity.json`.
pub struct Identity {
    signing: SigningKey,
    static_private: Vec<u8>,
    static_public: Vec<u8>,
}

impl Identity {
    fn load_or_create(path: &std::path::Path) -> Result<Self, String> {
        if let Ok(text) = fs::read_to_string(path) {
            let saved: Value = serde_json::from_str(&text)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let key = |name: &str| {
                saved[name]
                    .as_str()
                    .and_then(|text| URL_SAFE_NO_PAD.decode(text).ok())
                    .ok_or_else(|| format!("{}: missing {name}", path.display()))
            };
            let seed: [u8; 32] = key("identity")?
                .try_into()
                .map_err(|_| format!("{}: bad identity key", path.display()))?;
            return Ok(Self {
                signing: SigningKey::from_bytes(&seed),
                static_private: key("static")?,
                static_public: key("static_pub")?,
            });
        }
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|error| error.to_string())?;
        let (static_private, static_public) = noise::generate_keypair()?;
        let identity = Self {
            signing: SigningKey::from_bytes(&seed),
            static_private,
            static_public,
        };
        let saved = json!({
            "identity": URL_SAFE_NO_PAD.encode(seed),
            "static": URL_SAFE_NO_PAD.encode(&identity.static_private),
            "static_pub": URL_SAFE_NO_PAD.encode(&identity.static_public),
        });
        write_private(path, format!("{saved:#}\n").as_bytes())
            .map_err(|error| error.to_string())?;
        Ok(identity)
    }

    /// First 16 bytes of SHA-256 of the Ed25519 public key, base64url.
    pub fn host_id(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.signing.verifying_key().as_bytes());
        URL_SAFE_NO_PAD.encode(&digest[..16])
    }
}

/// Drops the relay identity in `dir`; the next connection makes a new one
/// (and phones must pair again).
pub fn forget_identity(dir: &std::path::Path) {
    let _ = fs::remove_file(dir.join(IDENTITY_FILE));
}

/// Which relay protocol a host connection speaks.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    V1,
    V2,
}

/// Relay configuration and state, shared by the relay thread and the ops
/// that report or change it.
pub struct Relay {
    /// v1 base URL (`…/relay/v1`); None when the relay is disabled for this
    /// process. v2 is the same base with `/v2`.
    url: Option<String>,
    /// The `relay` setting (off until a local client turns it on, as in v1).
    enabled: AtomicBool,
    connected: AtomicBool,
    /// The mode of the connection now, while connected.
    mode: Mutex<Option<Mode>>,
    dir: PathBuf,
    identity: Mutex<Option<Arc<Identity>>>,
    /// The account the computer is registered under (`relay_user`).
    user: Mutex<Option<String>>,
    /// SHA-256 of a device key the gateway refused: the computer counts as
    /// signed out until auth.json holds another one.
    refused: Mutex<Option<String>>,
    /// Open connect requests by id (§7.4).
    requests: Mutex<HashMap<String, Request>>,
    /// v2 could not be reached (a gateway or relay without v2): v1 until
    /// then, so remote access keeps working.
    v1_until: Mutex<Option<Instant>>,
    /// Unit tests: the device key instead of auth.json (never the real one).
    #[cfg(test)]
    test_key: Mutex<Option<String>>,
}

/// How long the daemon stays on v1 after v2 could not be reached.
const V2_RETRY: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 10 * 60 });

/// A client asking to connect, waiting for the desktop (§7.4 step 4).
struct Request {
    name: String,
    client: String,
    check: String,
    at: u64,
    decide: Sender<bool>,
}

impl Request {
    fn to_json(&self, id: &str) -> Value {
        json!({ "request": id, "name": self.name, "client": self.client, "check": self.check, "at": self.at })
    }
}

/// How a connect request ended.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    Allowed,
    Denied,
    Expired,
    Cancelled,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Allowed => "allowed",
            Outcome::Denied => "denied",
            Outcome::Expired => "expired",
            Outcome::Cancelled => "cancelled",
        }
    }
}

impl Relay {
    pub fn new(url: Option<String>, store: &Store) -> Self {
        Self {
            url: url.map(|url| url.trim_end_matches('/').to_string()),
            enabled: AtomicBool::new(store.setting(SETTING).as_bool().unwrap_or(false)),
            connected: AtomicBool::new(false),
            mode: Mutex::new(None),
            dir: store.dir().to_path_buf(),
            identity: Mutex::new(None),
            user: Mutex::new(store.setting(USER_SETTING).as_str().map(str::to_string)),
            refused: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
            v1_until: Mutex::new(None),
            #[cfg(test)]
            test_key: Mutex::new(None),
        }
    }

    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// The v2 base: `…/relay/v1` → `…/relay/v2`.
    fn v2_url(&self) -> Option<String> {
        let url = self.url.as_deref()?;
        Some(match url.strip_suffix("/v1") {
            Some(base) => format!("{base}/v2"),
            None => format!("{url}/v2"),
        })
    }

    /// This computer's device key, unless the gateway refused it.
    fn account_key(&self) -> Option<String> {
        #[cfg(test)]
        let key = lock(&self.test_key).clone()?;
        #[cfg(not(test))]
        let (_, key) = lynshen_agent_core::account::computer_key()?;
        let hash = bytes_hash(key.as_bytes());
        (lock(&self.refused).as_deref() != Some(hash.as_str())).then_some(key)
    }

    /// Signed in to LynShen with a login the gateway has not refused.
    fn signed_in(&self) -> bool {
        self.account_key().is_some()
    }

    fn enabled(&self) -> bool {
        self.url.is_some() && self.enabled.load(Ordering::SeqCst)
    }

    /// LAN and Tailscale direct connections are an advanced option of a
    /// computer signed in to LynShen: new pairings over the daemon's own
    /// port (`pair_start`, `POST /api/pair`) need the sign-in. Devices
    /// paired before keep connecting.
    pub fn lan_pairing_allowed(&self) -> Result<(), String> {
        if self.signed_in() {
            Ok(())
        } else {
            Err(
                "sign_in_required: sign in to LynShen to pair a device over the LAN or Tailscale"
                    .to_string(),
            )
        }
    }

    /// The mode a connection should use now: v2 signed in, else v1 (also
    /// for a while after v2 could not be reached).
    fn wanted_mode(&self) -> Mode {
        let fallback = lock(&self.v1_until).is_some_and(|until| Instant::now() < until);
        if self.signed_in() && !fallback {
            Mode::V2
        } else {
            Mode::V1
        }
    }

    fn user(&self) -> Option<String> {
        lock(&self.user).clone()
    }

    /// Connected over relay v2 now.
    pub fn on_v2(&self) -> bool {
        self.connected.load(Ordering::SeqCst) && *lock(&self.mode) == Some(Mode::V2)
    }

    /// The identity, created on first use.
    fn identity(&self) -> Result<Arc<Identity>, String> {
        let mut slot = self
            .identity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(identity) = slot.as_ref() {
            return Ok(Arc::clone(identity));
        }
        let identity = Arc::new(Identity::load_or_create(&self.dir.join(IDENTITY_FILE))?);
        *slot = Some(Arc::clone(&identity));
        Ok(identity)
    }

    /// This computer's host id at the relay, once it has an identity.
    pub fn host_id(&self) -> Option<String> {
        self.url.as_ref()?;
        self.identity().ok().map(|identity| identity.host_id())
    }

    /// Turns the relay on or off; persisted across restarts.
    pub fn set_enabled(&self, store: &Store, enabled: bool) -> Result<(), String> {
        if self.url.is_none() {
            return Err("the relay is disabled (--no-relay)".to_string());
        }
        store
            .set_setting(SETTING, json!(enabled))
            .map_err(|error| error.to_string())?;
        self.enabled.store(enabled, Ordering::SeqCst);
        Ok(())
    }

    pub fn status_json(&self) -> Value {
        let connected = self.connected.load(Ordering::SeqCst);
        json!({
            "type": "relay_status",
            "enabled": self.enabled(),
            "connected": connected,
            "host": self.identity().ok().map(|identity| identity.host_id()),
            "url": self.url,
            "signed_in": self.signed_in(),
            "refused": lock(&self.refused).is_some(),
            "mode": match (connected, *lock(&self.mode)) {
                (true, Some(Mode::V2)) => "v2",
                (true, Some(Mode::V1)) => "v1",
                _ => "off",
            },
            "user": self.user(),
        })
    }

    /// The relay registered the computer under `user` (§7.2). Another
    /// account than before unpairs every remote device first: they were
    /// paired by the previous account's owner. The first account keeps the
    /// devices paired before (over v1 or the LAN, by this computer's user).
    fn register(&self, hub: &Hub, user: &str) -> Result<(), String> {
        let mut current = lock(&self.user);
        if current.as_deref() == Some(user) {
            return Ok(());
        }
        if current.is_some() {
            let revoked = hub.revoke_remote_devices()?;
            lynshen_agent_core::log_info!(
                "daemon",
                "relay account changed; remote devices unpaired",
                revoked = revoked
            );
        }
        hub.store
            .set_setting(USER_SETTING, json!(user))
            .map_err(|error| error.to_string())?;
        *current = Some(user.to_string());
        Ok(())
    }

    /// `connect_requests`: the open requests.
    pub fn requests_json(&self) -> Value {
        let requests: Vec<Value> = lock(&self.requests)
            .iter()
            .map(|(id, request)| request.to_json(id))
            .collect();
        json!({ "type": "connect_requests", "requests": requests })
    }

    /// `connect_decide`: answers an open request.
    pub fn decide(&self, id: &str, allow: bool) -> Result<(), String> {
        // Sent while the request is taken out under the lock, so a waiting
        // stream that finds it gone knows the answer is in its channel.
        let mut requests = lock(&self.requests);
        let request = requests
            .remove(id)
            .ok_or_else(|| format!("no open connect request {id}"))?;
        let _ = request.decide.send(allow);
        Ok(())
    }

    /// Asks the desktop whether a client may connect and waits for the
    /// answer (`REQUEST_TIMEOUT`). `cancelled` tells whether the client has
    /// gone.
    fn request_connect(
        &self,
        hub: &Hub,
        name: &str,
        client: &str,
        check: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Outcome, String> {
        if !hub.has_local_client() {
            return Err("no_desktop".to_string());
        }
        let id = hub.new_id("cr");
        let (decide, decision) = mpsc::channel();
        let request = Request {
            name: name.to_string(),
            client: client.to_string(),
            check: check.to_string(),
            at: crate::store::now(),
            decide,
        };
        let mut announce = request.to_json(&id);
        {
            let mut requests = lock(&self.requests);
            if requests.len() >= MAX_REQUESTS {
                return Err("busy".to_string());
            }
            requests.insert(id.clone(), request);
        }
        announce["type"] = json!("connect_request");
        hub.broadcast_local(&announce);
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let waited = loop {
            match decision.recv_timeout(POLL) {
                Ok(allow) => break Some(allow),
                Err(RecvTimeoutError::Timeout) if cancelled() => break None,
                Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => break None,
                Err(_) => {}
            }
        };
        let decided = |allow| {
            if allow {
                Outcome::Allowed
            } else {
                Outcome::Denied
            }
        };
        let outcome = match waited {
            Some(allow) => decided(allow),
            // Already taken out: decided just after the last look.
            None if lock(&self.requests).remove(&id).is_none() => {
                decided(decision.try_recv().unwrap_or(false))
            }
            None if cancelled() => Outcome::Cancelled,
            None => Outcome::Expired,
        };
        hub.broadcast_local(
            &json!({ "type": "connect_request_closed", "request": id, "outcome": outcome.as_str() }),
        );
        Ok(outcome)
    }

    /// A pairing code plus the link a phone opens to pair over the relay
    /// (§2). The page is served by the relay's host.
    pub fn pair_link(&self, hub: &Hub) -> Result<Value, String> {
        if !self.enabled() {
            return Err("the relay is off".to_string());
        }
        let identity = self.identity()?;
        let (code, expires_at) = hub.start_pairing()?;
        let link = format!(
            "{}/remote#pair={}.{}.{code}",
            app_origin(self.url.as_deref().unwrap_or_default()),
            identity.host_id(),
            URL_SAFE_NO_PAD.encode(&identity.static_public),
        );
        Ok(json!({ "type": "pair_link", "link": link, "code": code, "expires_at": expires_at }))
    }
}

impl Relay {
    /// Asks the relay to deliver a notification (see `push`): signed with
    /// the host key, which the relay only accepts from a host connected to
    /// it. A Web Push message (`payload`) is encrypted here for the browser
    /// (RFC 8291), so the relay passes on ciphertext; APNs, FCM and Getui
    /// take `native`, generic text and ids only. Returns the relay's HTTP
    /// status (410: the subscription is gone).
    pub fn push(
        &self,
        subscription: &Value,
        payload: &Value,
        native: &Value,
        delivery: &Delivery,
    ) -> Result<u16, String> {
        let url = self.url.as_deref().ok_or("the relay is disabled")?;
        let identity = self.identity()?;
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).map_err(|error| error.to_string())?;
        let mut body = json!({
            "pub": URL_SAFE_NO_PAD.encode(identity.signing.verifying_key().as_bytes()),
            "ts": crate::store::now(),
            "nonce": URL_SAFE_NO_PAD.encode(nonce),
            "urgency": delivery.urgency,
            "ttl": delivery.ttl,
        });
        if let Some(topic) = &delivery.topic {
            body["topic"] = json!(topic);
        }
        match subscription["provider"].as_str() {
            Some("getui") => {
                body["subscription"] =
                    json!({ "provider": "getui", "client_id": subscription["client_id"] });
                body["payload"] = native.clone();
            }
            Some("apns") => {
                body["subscription"] = json!({
                    "provider": "apns",
                    "token": subscription["token"],
                    "environment": subscription["environment"],
                });
                body["payload"] = native.clone();
            }
            Some("fcm") => {
                body["subscription"] = json!({ "provider": "fcm", "token": subscription["token"] });
                body["payload"] = native.clone();
            }
            _ => {
                let key = |name: &str| {
                    subscription["keys"][name]
                        .as_str()
                        .and_then(|text| URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).ok())
                        .ok_or_else(|| format!("the subscription has no {name} key"))
                };
                let message = crate::webpush::encrypt(
                    payload.to_string().as_bytes(),
                    &key("p256dh")?,
                    &key("auth")?,
                )?;
                body["subscription"] = json!({ "endpoint": subscription["endpoint"] });
                body["encrypted"] = json!(URL_SAFE_NO_PAD.encode(message));
            }
        }
        let body = body.to_string();
        let mut signed = PUSH_CONTEXT.to_vec();
        signed.extend_from_slice(body.as_bytes());
        let signature = identity.signing.sign(&signed);
        let response = ureq::post(&format!("{}/relay/v1/push", app_origin(url)))
            .timeout(Duration::from_secs(20))
            .set("Content-Type", "application/json")
            .set(
                "X-LynShen-Signature",
                &URL_SAFE_NO_PAD.encode(signature.to_bytes()),
            )
            .send_string(&body);
        match response {
            Ok(response) => Ok(response.status()),
            Err(ureq::Error::Status(code, _)) => Ok(code),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// How the push service treats one notification (RFC 8030 5).
#[derive(Debug, Clone)]
pub struct Delivery {
    /// `low`, `normal` or `high`.
    pub urgency: &'static str,
    /// Seconds the push service keeps it for a phone that is offline.
    pub ttl: u64,
    /// A newer notification with the same topic replaces one not yet
    /// delivered.
    pub topic: Option<String>,
}

/// What a push request's signature covers, before its body.
const PUSH_CONTEXT: &[u8] = b"lynshen-relay-push-v1:";

/// `wss://host[:port]/relay/v1` → `https://host[:port]`.
fn app_origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("wss", url));
    let authority = rest.split('/').next().unwrap_or_default();
    let scheme = if scheme == "ws" { "http" } else { "https" };
    format!("{scheme}://{authority}")
}

/// How a host connection ended, besides a clean stop (relay turned off, a
/// sign-in or sign-out that changes the mode).
#[derive(Debug)]
enum Ended {
    /// The gateway refused this computer's login (HTTP 401 on relay-token).
    Refused,
    /// Close 4503: the server is restarting; reconnect soon.
    Restarting,
    /// v2 could not be reached (no relay-token, the relay has no v2): use v1
    /// for a while.
    Unavailable(String),
    Failed(String),
}

impl From<String> for Ended {
    fn from(error: String) -> Self {
        Ended::Failed(error)
    }
}

/// The relay thread: keeps a host connection while the relay is enabled,
/// v2 while signed in and v1 otherwise, reconnecting with backoff (1 s
/// doubling to 60 s, plus jitter; 1 s flat after a server restart).
pub fn run(hub: &Arc<Hub>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if !hub.relay.enabled() {
            backoff = Duration::from_secs(1);
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let mode = hub.relay.wanted_mode();
        let result = hub
            .relay
            .identity()
            .map_err(Ended::Failed)
            .and_then(|identity| match mode {
                Mode::V2 => signed_in_connection(hub, &identity, &mut backoff),
                Mode::V1 => host_connection(hub, &identity, Mode::V1, None, &mut backoff),
            });
        hub.relay.connected.store(false, Ordering::SeqCst);
        *lock(&hub.relay.mode) = None;
        let wait = match result {
            Ok(()) => continue,
            Err(Ended::Refused) => {
                lynshen_agent_core::log_warn!(
                    "daemon",
                    "the gateway refused this computer's login"
                );
                continue;
            }
            Err(Ended::Restarting) => {
                lynshen_agent_core::log_info!("daemon", "relay server restarting");
                Duration::from_secs(1)
            }
            Err(Ended::Unavailable(error)) => {
                lynshen_agent_core::log_warn!(
                    "daemon",
                    "relay v2 unavailable; using v1",
                    error = error
                );
                *lock(&hub.relay.v1_until) = Some(Instant::now() + V2_RETRY);
                continue;
            }
            Err(Ended::Failed(error)) => {
                lynshen_agent_core::log_warn!("daemon", "relay connection ended", error = error);
                let mut jitter = [0u8; 2];
                let _ = getrandom::getrandom(&mut jitter);
                let jitter = Duration::from_millis(u64::from(u16::from_le_bytes(jitter)) % 1000);
                let wait = backoff + jitter;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                wait
            }
        };
        if hub.relay.enabled() {
            thread::sleep(wait);
        }
    }
}

/// A v2 host connection on a relay token minted with the device key. A key
/// the gateway refuses is remembered (`Relay::refused`): the daemon goes
/// back to v1 until auth.json holds another login.
fn signed_in_connection(
    hub: &Arc<Hub>,
    identity: &Arc<Identity>,
    backoff: &mut Duration,
) -> Result<(), Ended> {
    use lynshen_agent_core::account::{self, AccountError};
    let key = hub.relay.account_key();
    match account::relay_token() {
        Ok((token, _)) => host_connection(hub, identity, Mode::V2, Some(&token), backoff),
        // The gateway refused the device key: it was removed from the
        // account. The computer is signed out (the key leaves auth.json), so
        // every client shows sign-in instead of retrying a dead login.
        Err(AccountError::Revoked(_)) => {
            if let Some(key) = key {
                *lock(&hub.relay.refused) = Some(bytes_hash(key.as_bytes()));
                if let Err(error) = account::forget_refused_key(&key) {
                    lynshen_agent_core::log_warn!("daemon", "sign-out failed", error = error);
                }
            }
            Err(Ended::Refused)
        }
        Err(AccountError::Failed(error)) => {
            Err(Ended::Unavailable(format!("relay token: {error}")))
        }
    }
}

type HostSocket = WebSocket<MaybeTlsStream<TcpStream>>;

/// One host connection: authenticate, then route stream frames until the
/// socket drops, the relay is turned off, or the mode changes (a sign-in, a
/// sign-out, another account). Dropping the stream table ends every stream
/// thread.
fn host_connection(
    hub: &Arc<Hub>,
    identity: &Arc<Identity>,
    mode: Mode,
    token: Option<&str>,
    backoff: &mut Duration,
) -> Result<(), Ended> {
    let host_id = identity.host_id();
    let url = match mode {
        Mode::V1 => format!("{}/host", hub.relay.url().unwrap_or_default()),
        Mode::V2 => format!(
            "{}/host?host={host_id}",
            hub.relay.v2_url().unwrap_or_default()
        ),
    };
    let account = hub.relay.account_key();
    let mut socket = connect_host(&url, token)?;
    let challenge = read_json(&mut socket)?;
    if challenge["t"] != "challenge" {
        return Err(Ended::Failed(format!(
            "expected a challenge, got {challenge}"
        )));
    }
    let nonce = challenge["nonce"]
        .as_str()
        .and_then(|nonce| URL_SAFE_NO_PAD.decode(nonce).ok())
        .ok_or("challenge without a nonce".to_string())?;
    let signature = identity
        .signing
        .sign(&[AUTH_CONTEXT, nonce.as_slice()].concat());
    let mut auth = json!({
        "t": "auth",
        "pub": URL_SAFE_NO_PAD.encode(identity.signing.verifying_key().as_bytes()),
        "sig": URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        "v": hub.version,
    });
    if mode == Mode::V2 {
        auth["static"] = json!(URL_SAFE_NO_PAD.encode(&identity.static_public));
        auth["name"] = json!(lynshen_agent_core::account::hostname().unwrap_or_default());
        auth["platform"] = json!(match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            _ => "linux",
        });
    }
    socket
        .send(Message::text(auth.to_string()))
        .map_err(|error| error.to_string())?;
    let ready = read_json(&mut socket)?;
    if ready["t"] != "ready" {
        return Err(Ended::Failed(format!("relay refused the host: {ready}")));
    }
    if mode == Mode::V2 {
        let user = ready["user"]
            .as_str()
            .filter(|user| !user.is_empty())
            .ok_or_else(|| format!("relay ready without a user: {ready}"))?;
        hub.relay.register(hub, user)?;
    }
    set_read_timeout(&socket, POLL)?;
    hub.relay.connected.store(true, Ordering::SeqCst);
    *lock(&hub.relay.mode) = Some(mode);
    // Back to the short backoff only once this connection proved stable: a
    // relay that accepts the host and drops it right away (another daemon
    // with this identity, say) would otherwise be retried every second.
    let _reset = ResetWhenStable {
        backoff,
        since: Instant::now(),
    };
    lynshen_agent_core::log_info!(
        "daemon",
        "relay connected",
        host = host_id,
        mode = format!("{mode:?}")
    );

    // Stream threads send finished frames here; this thread writes them.
    let (out, outgoing) = mpsc::channel::<Vec<u8>>();
    let mut streams: HashMap<u32, Sender<Vec<u8>>> = HashMap::new();
    let mut heard = Instant::now();
    let mut mode_checked = Instant::now();
    loop {
        // Signing in or out, or in to another account, reconnects in the
        // right mode; the next v2 connection registers the new account.
        let changed = mode_checked.elapsed() >= MODE_CHECK && {
            mode_checked = Instant::now();
            hub.relay.wanted_mode() != mode || hub.relay.account_key() != account
        };
        if !hub.relay.enabled() || changed {
            let _ = socket.close(None);
            let _ = socket.flush();
            return Ok(());
        }
        if heard.elapsed() > SILENCE {
            return Err(Ended::Failed(format!(
                "no frame from the relay for {} s",
                SILENCE.as_secs()
            )));
        }
        let read = socket.read();
        if read.is_ok() {
            heard = Instant::now();
        }
        match read {
            Ok(Message::Binary(bytes)) => match Frame::decode(&bytes) {
                Some(Frame::Open(id, payload)) => {
                    let (incoming, receiver) = mpsc::channel();
                    streams.insert(id, incoming);
                    let hub = Arc::clone(hub);
                    let identity = Arc::clone(identity);
                    let out = out.clone();
                    let caller = Caller::parse(&payload);
                    thread::spawn(move || {
                        if let Err(error) = stream(&hub, &identity, id, &caller, receiver, out) {
                            lynshen_agent_core::log_warn!(
                                "daemon",
                                "relay stream ended",
                                error = error
                            );
                        }
                    });
                }
                Some(Frame::Data(id, payload)) => {
                    if streams
                        .get(&id)
                        .is_some_and(|stream| stream.send(payload).is_err())
                    {
                        streams.remove(&id);
                    }
                }
                Some(Frame::Close(id)) => {
                    streams.remove(&id);
                }
                None => return Err(Ended::Failed("malformed relay frame".to_string())),
            },
            Ok(Message::Close(frame)) => return Err(closed(frame)),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(Ended::Failed(error.to_string())),
        }
        while let Ok(frame) = outgoing.try_recv() {
            if let [CLOSE, a, b, c, d] = frame[..] {
                streams.remove(&u32::from_be_bytes([a, b, c, d]));
            }
            socket
                .send(Message::binary(frame))
                .map_err(|error| error.to_string())?;
        }
    }
}

/// What the relay's close frame means for reconnecting.
fn closed(frame: Option<tungstenite::protocol::CloseFrame>) -> Ended {
    match frame {
        Some(frame) if u16::from(frame.code) == RESTARTING => Ended::Restarting,
        frame => Ended::Failed(format!("the relay closed the connection: {frame:?}")),
    }
}

/// How long a host connection must last before the backoff starts over.
const STABLE: Duration = Duration::from_secs(30);

struct ResetWhenStable<'a> {
    backoff: &'a mut Duration,
    since: Instant,
}

impl Drop for ResetWhenStable<'_> {
    fn drop(&mut self) {
        if self.since.elapsed() >= STABLE {
            *self.backoff = Duration::from_secs(1);
        }
    }
}

/// Opens the host WebSocket with every step bounded: TCP connect, TLS and the
/// HTTP upgrade (a proxy that accepts the connection and then says nothing
/// would otherwise block this thread for good, `relay_set(false)` included).
fn connect_host(url: &str, token: Option<&str>) -> Result<HostSocket, Ended> {
    use std::net::ToSocketAddrs;
    use tungstenite::client::IntoClientRequest;
    let mut request = url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    if let Some(token) = token {
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|_| "the relay token is not a header value".to_string())?,
        );
    }
    let uri = request.uri();
    let host = uri
        .host()
        .ok_or("relay url without a host".to_string())?
        .to_string();
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    let mut last = format!("could not resolve {host}");
    let mut tcp = None;
    for addr in (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
    {
        match TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(error) => last = error.to_string(),
        }
    }
    let tcp = tcp.ok_or(last)?;
    tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .and_then(|_| tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT)))
        .map_err(|error| error.to_string())?;
    match tungstenite::client_tls(request, tcp) {
        Ok((socket, _)) => Ok(socket),
        // A relay without v2 (404), or one that refuses the token (401, a
        // gateway key it does not know yet).
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response)))
            if token.is_some() =>
        {
            Err(Ended::Unavailable(format!(
                "relay v2 answered {}",
                response.status()
            )))
        }
        Err(error) => Err(Ended::Failed(error.to_string())),
    }
}

fn set_read_timeout(socket: &HostSocket, timeout: Duration) -> Result<(), String> {
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => tls.get_ref(),
        _ => return Err("unsupported relay stream".to_string()),
    };
    tcp.set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())
}

/// The next text frame as JSON, during the host handshake.
fn read_json(socket: &mut HostSocket) -> Result<Value, Ended> {
    loop {
        match socket.read().map_err(|error| error.to_string())? {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str())
                    .map_err(|error| Ended::Failed(error.to_string()))
            }
            Message::Close(frame) => return Err(closed(frame)),
            _ => {}
        }
    }
}

/// One client stream: the Noise handshake, then the client protocol. A
/// refused client gets its reason in msg 2 and then `close`.
fn stream(
    hub: &Arc<Hub>,
    identity: &Identity,
    id: u32,
    caller: &Caller,
    incoming: Receiver<Vec<u8>>,
    out: Sender<Vec<u8>>,
) -> Result<(), String> {
    let first = match incoming.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(message) => message,
        Err(RecvTimeoutError::Disconnected) => return Ok(()),
        Err(RecvTimeoutError::Timeout) => {
            let _ = out.send(Frame::Close(id).encode());
            return Err("no handshake from the client".to_string());
        }
    };
    // Nothing more comes before msg 2; the channel closing means the client
    // (or the whole host connection) went away.
    let gone = || matches!(incoming.try_recv(), Err(TryRecvError::Disconnected));
    let (transport, device) = match handshake(hub, identity, id, caller, &first, &out, &gone) {
        Ok(accepted) => accepted,
        Err(error) => {
            let _ = out.send(Frame::Close(id).encode());
            return Err(error);
        }
    };
    let mut link = Stream {
        id,
        incoming,
        out,
        transport,
        open: true,
    };
    crate::attach(hub, &mut link, Some(device.id))
}

/// Reads msg 1, authorizes the client and sends msg 2.
fn handshake(
    hub: &Hub,
    identity: &Identity,
    id: u32,
    caller: &Caller,
    first: &[u8],
    out: &Sender<Vec<u8>>,
    gone: &dyn Fn() -> bool,
) -> Result<(noise::Transport, Device), String> {
    let mut noise = noise::responder(&identity.static_private)?;
    let mut buffer = vec![0u8; noise::MAX_MESSAGE];
    let read = noise
        .read_message(first, &mut buffer)
        .map_err(|error| format!("bad handshake: {error}"))?;
    let hello: Value = serde_json::from_slice(&buffer[..read]).unwrap_or(Value::Null);
    let key = noise
        .get_remote_static()
        .ok_or("handshake without a client key")?
        .to_vec();
    // The check code both sides show: from this connection's handshake hash
    // after msg 1, which binds both static keys and the ephemeral one.
    let check = crate::hub::check_code(noise.get_handshake_hash());
    let verdict = if caller.is_v1() {
        authorize(hub, &key, &hello, check)
    } else {
        authorize_account(hub, &key, &hello, caller, check, gone)
    };
    let reply = match &verdict {
        Ok(Verdict::Accepted(device)) => {
            json!({ "ok": true, "device": device.id, "name": device.name })
        }
        // Paired, not trusted yet: the phone shows the same code and waits.
        Ok(Verdict::Pending(device)) => json!({
            "ok": false,
            "pending": true,
            "device": device.id,
            "check": device.check,
            "error": "confirm this device on the computer: compare the check code",
        }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    let written = noise
        .write_message(reply.to_string().as_bytes(), &mut buffer)
        .map_err(|error| error.to_string())?;
    out.send(Frame::Data(id, buffer[..written].to_vec()).encode())
        .map_err(|_| "relay connection closed".to_string())?;
    match verdict? {
        Verdict::Accepted(device) => Ok((noise::Transport::new(noise)?, device)),
        Verdict::Pending(_) => Err("waiting for the computer to confirm the device".to_string()),
    }
}

enum Verdict {
    Accepted(Device),
    /// Paired now; the computer has not confirmed it (§4 step 2).
    Pending(Device),
}

/// The device a client static key belongs to: an already paired one, or a
/// new one, pending until the computer confirms it, when msg 1 carries a
/// valid pairing code (§4 step 2). A pending device that connects again is
/// told it still waits, with its first check code. v1 clients (no account)
/// on either relay version.
fn authorize(hub: &Hub, key: &[u8], hello: &Value, check: String) -> Result<Verdict, String> {
    let hash = bytes_hash(key);
    if let Some(device) = hub.store.device_for_hash(&hash) {
        return Ok(Verdict::Accepted(device));
    }
    if let Some(device) = hub
        .store
        .devices()
        .into_iter()
        .find(|d| !d.revoked && d.pending && d.chat.is_none() && d.token_hash == hash)
    {
        return Ok(Verdict::Pending(device));
    }
    match hello["pair"].as_str() {
        Some(code) => hub
            .pair_pending(
                code,
                hello["name"].as_str().unwrap_or_default(),
                hash,
                check,
            )
            .map(Verdict::Pending),
        None => Err("this device is not paired".to_string()),
    }
}

/// §7.4: a client of a LynShen account, in this order: another account than
/// the computer's is refused; a paired device is allowed; a pairing code
/// pairs as in v1 (pending until the computer confirms the check code); a
/// connect request asks the desktop, showing the same check code, and waits
/// up to 120 s; anything else is refused.
fn authorize_account(
    hub: &Hub,
    key: &[u8],
    hello: &Value,
    caller: &Caller,
    check: String,
    gone: &dyn Fn() -> bool,
) -> Result<Verdict, String> {
    if hub.relay.user().as_deref() != Some(caller.user.as_str()) {
        return Err("wrong account".to_string());
    }
    let account = DeviceAccount {
        user: caller.user.clone(),
        authz: caller.authz.clone(),
        client: caller.client.clone(),
    };
    let name = hello["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&caller.name)
        .to_string();
    if hello["pair"].is_null() && hello["request"] == true {
        let hash = bytes_hash(key);
        if hub.store.device_for_hash(&hash).is_none() {
            return match hub
                .relay
                .request_connect(hub, &name, &caller.client, &check, gone)?
            {
                Outcome::Allowed => hub.add_device(&name, hash, &account).map(Verdict::Accepted),
                outcome => Err(outcome.as_str().to_string()),
            };
        }
    }
    let verdict = authorize(
        hub,
        key,
        &json!({ "name": name, "pair": hello["pair"] }),
        check,
    )
    .map_err(|_| "not paired".to_string())?;
    let device = match &verdict {
        Verdict::Accepted(device) | Verdict::Pending(device) => device,
    };
    // A device paired over v1 or the LAN (no account) is bound to the
    // account on its first v2 connection: the computer's owner paired it,
    // and it now comes signed in to the same account. A new sign-in on the
    // device (another authorization) is recorded too.
    match hub.store.device_accounts().get(&device.id) {
        Some(known) if known.user != account.user => return Err("wrong account".to_string()),
        Some(known) if *known == account => {}
        _ => hub
            .store
            .record_device_account(&device.id, &account)
            .map_err(|error| error.to_string())?,
    }
    Ok(verdict)
}

/// A relay stream after its handshake, as a client link. Frames travel as
/// chunked Noise transport messages.
struct Stream {
    id: u32,
    incoming: Receiver<Vec<u8>>,
    out: Sender<Vec<u8>>,
    transport: noise::Transport,
    /// False once either side closed, so `close` goes out at most once.
    open: bool,
}

impl Link for Stream {
    fn send(&mut self, frame: &str) -> Result<(), String> {
        for message in self.transport.seal(frame.as_bytes())? {
            self.out
                .send(Frame::Data(self.id, message).encode())
                .map_err(|_| "relay connection closed".to_string())?;
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<String>, Received> {
        match self.incoming.recv_timeout(POLL) {
            Ok(message) => match self.transport.open(&message) {
                Ok(Some(frame)) => String::from_utf8(frame)
                    .map(Some)
                    .map_err(|_| Received::Failed("frame is not UTF-8".to_string())),
                Ok(None) => Ok(None),
                Err(error) => Err(Received::Failed(error)),
            },
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                self.open = false;
                Err(Received::Closed)
            }
        }
    }

    fn close(&mut self) {
        if std::mem::replace(&mut self.open, false) {
            let _ = self.out.send(Frame::Close(self.id).encode());
        }
    }
}

/// A stream that ends on the daemon's side (an error, a revoked device)
/// tells the relay.
impl Drop for Stream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{store::now, Agents};

    fn hub(label: &str) -> Arc<Hub> {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-relay-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let _ = fs::remove_dir_all(&dir);
        Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            Agents::open(dir.join("agents")).unwrap(),
            "test",
            Some("wss://relay.example/relay/v1/".to_string()),
        )
    }

    #[test]
    fn frames_encode_and_decode() {
        for frame in [
            Frame::Open(1, Vec::new()),
            Frame::Open(9, br#"{"user":"u1"}"#.to_vec()),
            Frame::Data(0xdead_beef, b"payload".to_vec()),
            Frame::Data(7, Vec::new()),
            Frame::Close(u32::MAX),
        ] {
            assert_eq!(Frame::decode(&frame.encode()), Some(frame));
        }
        assert_eq!(Frame::Data(258, vec![9]).encode(), [2, 0, 0, 1, 2, 9]);
        assert_eq!(Frame::decode(&[1, 0, 0, 0, 0]), None, "stream 0");
        assert_eq!(
            Caller::parse(br#"{"user":"u1","authz":"a1","client":"aogiri-phone","name":"iPhone"}"#),
            Caller {
                user: "u1".to_string(),
                authz: "a1".to_string(),
                client: "aogiri-phone".to_string(),
                name: "iPhone".to_string()
            }
        );
        assert!(Caller::parse(b"").is_v1());
        assert!(Caller::parse(br#"{"v":1}"#).is_v1());
        assert_eq!(Frame::decode(&[4, 0, 0, 0, 1]), None, "unknown kind");
        assert_eq!(Frame::decode(&[2, 0, 0]), None, "short");
    }

    #[test]
    fn authorization_pairs_with_a_code_waits_for_the_computer_and_honours_revocation() {
        let hub = hub("authorize");
        let (_, phone) = noise::generate_keypair().unwrap();
        let (_, stranger) = noise::generate_keypair().unwrap();
        let check = || "123456".to_string();
        let accepted = |verdict: Result<Verdict, String>| match verdict {
            Ok(Verdict::Accepted(device)) => Some(device),
            _ => None,
        };

        // Unknown key without a code, or with a wrong one.
        assert!(authorize(&hub, &phone, &json!({ "name": "phone" }), check()).is_err());
        assert!(authorize(&hub, &phone, &json!({ "pair": "WRONG123" }), check()).is_err());

        // A valid code pairs the key, pending; the code is single use.
        let (code, _) = hub.start_pairing().unwrap();
        let Ok(Verdict::Pending(device)) = authorize(
            &hub,
            &phone,
            &json!({ "name": "phone", "pair": code }),
            check(),
        ) else {
            panic!("a new device waits for the computer");
        };
        assert_eq!(
            (device.name.as_str(), device.check.as_deref()),
            ("phone", Some("123456"))
        );
        assert_eq!(device.token_hash, bytes_hash(&phone));
        assert!(authorize(&hub, &stranger, &json!({ "pair": code }), check()).is_err());
        // Back before the computer confirmed it: still waiting, same code.
        let Ok(Verdict::Pending(again)) = authorize(&hub, &phone, &json!({}), "999999".to_string())
        else {
            panic!("still pending");
        };
        assert_eq!(again.check.as_deref(), Some("123456"));
        assert_eq!(hub.devices_json()["devices"][0]["pending"], true);

        // Confirmed on the computer: the key comes back as the same device.
        hub.confirm_device(&device.id).unwrap();
        assert_eq!(
            accepted(authorize(&hub, &phone, &json!({}), check()))
                .unwrap()
                .id,
            device.id
        );
        assert_eq!(hub.devices_json()["devices"][0]["id"], device.id.as_str());

        hub.revoke_device(&device.id).unwrap();
        assert!(authorize(&hub, &phone, &json!({}), check()).is_err());
    }

    #[test]
    fn a_closed_pairing_dialog_takes_its_code_back() {
        let hub = hub("cancel");
        let (_, phone) = noise::generate_keypair().unwrap();
        let (code, _) = hub.start_pairing().unwrap();
        hub.cancel_pairing(Some(&code.to_lowercase()));
        assert!(authorize(&hub, &phone, &json!({ "pair": code }), "1".to_string()).is_err());
        let (code, _) = hub.start_pairing().unwrap();
        hub.cancel_pairing(None);
        assert!(authorize(&hub, &phone, &json!({ "pair": code }), "1".to_string()).is_err());
    }

    #[test]
    fn the_identity_is_kept_and_private() {
        let hub = hub("identity");
        let first = hub.relay.identity().unwrap();
        let path = hub.store.dir().join(IDENTITY_FILE);
        let again = Identity::load_or_create(&path).unwrap();
        assert_eq!(again.host_id(), first.host_id());
        assert_eq!(again.static_public, first.static_public);
        assert_eq!(first.host_id().len(), 22);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_pair_link_needs_the_relay_on_and_names_the_app() {
        let hub = hub("link");
        assert!(hub.relay.pair_link(&hub).is_err());
        hub.relay.set_enabled(&hub.store, true).unwrap();
        assert_eq!(hub.store.setting(SETTING), json!(true));
        let reply = hub.relay.pair_link(&hub).unwrap();
        let identity = hub.relay.identity().unwrap();
        assert_eq!(
            reply["link"],
            format!(
                "https://relay.example/remote#pair={}.{}.{}",
                identity.host_id(),
                URL_SAFE_NO_PAD.encode(&identity.static_public),
                reply["code"].as_str().unwrap(),
            )
        );
        assert_eq!(
            app_origin("ws://127.0.0.1:9/relay/v1"),
            "http://127.0.0.1:9"
        );
    }

    fn caller(user: &str, authz: &str) -> Caller {
        Caller {
            user: user.to_string(),
            authz: authz.to_string(),
            client: "aogiri-phone".to_string(),
            name: "iPhone".to_string(),
        }
    }

    /// §7.4: wrong account → paired → code → connect request → refused.
    #[test]
    fn account_clients_are_authorized_in_order() {
        let hub = hub("account");
        hub.relay.register(&hub, "u1").unwrap();
        let (_, phone) = noise::generate_keypair().unwrap();
        let never = || false;
        let accepted = |verdict: Result<Verdict, String>| match verdict {
            Ok(Verdict::Accepted(device)) => Ok(device),
            Ok(Verdict::Pending(device)) => Err(format!("pending {}", device.id)),
            Err(error) => Err(error),
        };

        // 1. Another account: refused before anything else, even with a code.
        let (code, _) = hub.start_pairing().unwrap();
        let other = authorize_account(
            &hub,
            &phone,
            &json!({ "pair": code }),
            &caller("u2", "a9"),
            "1".to_string(),
            &never,
        );
        assert_eq!(accepted(other).unwrap_err(), "wrong account");

        // 5. Unknown key, no code, no request: not paired.
        let none = authorize_account(
            &hub,
            &phone,
            &json!({}),
            &caller("u1", "a1"),
            "1".to_string(),
            &never,
        );
        assert_eq!(accepted(none).unwrap_err(), "not paired");

        // 4. A connect request with nobody at the computer: no_desktop.
        let request = json!({ "request": true, "name": "我的 iPhone" });
        let asked = authorize_account(
            &hub,
            &phone,
            &request,
            &caller("u1", "a1"),
            "123456".to_string(),
            &never,
        );
        assert_eq!(accepted(asked).unwrap_err(), "no_desktop");

        // With a desktop: it hears connect_request with the check code and
        // allows; the device is paired under the account.
        let (outbox, inbox) = mpsc::channel();
        hub.add_client(crate::hub::Outbox::local(outbox), None);
        let answer = {
            let hub = Arc::clone(&hub);
            thread::spawn(move || loop {
                let frame: Value = serde_json::from_str(&inbox.recv().unwrap()).unwrap();
                if frame["type"] == "connect_request" {
                    assert_eq!(
                        (frame["check"].as_str(), frame["name"].as_str()),
                        (Some("123456"), Some("我的 iPhone"))
                    );
                    assert_eq!(
                        hub.relay.requests_json()["requests"][0]["request"],
                        frame["request"]
                    );
                    hub.relay
                        .decide(frame["request"].as_str().unwrap(), true)
                        .unwrap();
                    return frame["request"].as_str().unwrap().to_string();
                }
            })
        };
        let allowed = authorize_account(
            &hub,
            &phone,
            &request,
            &caller("u1", "a1"),
            "123456".to_string(),
            &never,
        );
        let device = accepted(allowed).unwrap();
        answer.join().unwrap();
        assert_eq!(device.name, "我的 iPhone");
        assert_eq!(hub.store.device_accounts()[&device.id].authz, "a1");
        assert_eq!(hub.devices_json()["devices"][0]["client"], "aogiri-phone");

        // 2. Paired now: allowed without a request; another account still not.
        let again = authorize_account(
            &hub,
            &phone,
            &json!({}),
            &caller("u1", "a1"),
            "1".to_string(),
            &never,
        );
        assert_eq!(accepted(again).unwrap().id, device.id);
        let stolen = authorize_account(
            &hub,
            &phone,
            &json!({}),
            &caller("u2", "a1"),
            "1".to_string(),
            &never,
        );
        assert_eq!(accepted(stolen).unwrap_err(), "wrong account");

        // A denied request and one that runs out (a second in tests).
        let (_, tablet) = noise::generate_keypair().unwrap();
        let (outbox, inbox) = mpsc::channel();
        hub.add_client(crate::hub::Outbox::local(outbox), None);
        let deny = {
            let hub = Arc::clone(&hub);
            thread::spawn(move || loop {
                let frame: Value = serde_json::from_str(&inbox.recv().unwrap()).unwrap();
                if frame["type"] == "connect_request" {
                    hub.relay
                        .decide(frame["request"].as_str().unwrap(), false)
                        .unwrap();
                    return;
                }
            })
        };
        let denied = authorize_account(
            &hub,
            &tablet,
            &json!({ "request": true }),
            &caller("u1", "a2"),
            "2".to_string(),
            &never,
        );
        deny.join().unwrap();
        assert_eq!(accepted(denied).unwrap_err(), "denied");
        // Someone stays at the computer and does not answer.
        let (outbox, _kept) = mpsc::channel();
        hub.add_client(crate::hub::Outbox::local(outbox), None);
        let expired = authorize_account(
            &hub,
            &tablet,
            &json!({ "request": true }),
            &caller("u1", "a2"),
            "2".to_string(),
            &never,
        );
        assert_eq!(accepted(expired).unwrap_err(), "expired");
        let gone = || true;
        let cancelled = authorize_account(
            &hub,
            &tablet,
            &json!({ "request": true }),
            &caller("u1", "a2"),
            "2".to_string(),
            &gone,
        );
        assert_eq!(accepted(cancelled).unwrap_err(), "cancelled");

        // 3. A pairing code pairs as in v1: pending until the check code is
        // confirmed, under the account.
        let (code, _) = hub.start_pairing().unwrap();
        let paired = authorize_account(
            &hub,
            &tablet,
            &json!({ "pair": code, "name": "iPad" }),
            &caller("u1", "a2"),
            "654321".to_string(),
            &never,
        );
        let Ok(Verdict::Pending(pending)) = paired else {
            panic!("a code pairs pending");
        };
        assert_eq!(pending.check.as_deref(), Some("654321"));
        assert_eq!(hub.store.device_accounts()[&pending.id].user, "u1");
        hub.confirm_device(&pending.id).unwrap();
        let confirmed = authorize_account(
            &hub,
            &tablet,
            &json!({}),
            &caller("u1", "a2"),
            "1".to_string(),
            &never,
        );
        assert_eq!(accepted(confirmed).unwrap().id, pending.id);
    }

    /// §7.4: at most three connect requests wait at once.
    #[test]
    fn a_fourth_connect_request_is_busy() {
        let hub = hub("busy");
        hub.relay.register(&hub, "u1").unwrap();
        let (outbox, _inbox) = mpsc::channel();
        hub.add_client(crate::hub::Outbox::local(outbox), None);
        let waiting: Vec<_> = (0..3)
            .map(|_| {
                let hub = Arc::clone(&hub);
                thread::spawn(move || {
                    hub.relay
                        .request_connect(&hub, "x", "aogiri-phone", "1", &|| false)
                })
            })
            .collect();
        while lock(&hub.relay.requests).len() < 3 {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            hub.relay
                .request_connect(&hub, "y", "aogiri-phone", "1", &|| false)
                .unwrap_err(),
            "busy"
        );
        for request in waiting {
            assert_eq!(request.join().unwrap().unwrap(), Outcome::Expired);
        }
    }

    /// A v1 device (paired over the LAN or relay v1) is bound to the account
    /// on its first v2 connection; a computer moving to another account
    /// unpairs every remote device.
    #[test]
    fn v1_devices_join_the_account_and_another_account_unpairs_them() {
        let hub = hub("switch");
        let (_, phone) = noise::generate_keypair().unwrap();
        let (code, _) = hub.start_pairing().unwrap();
        let Ok(Verdict::Pending(device)) = authorize(
            &hub,
            &phone,
            &json!({ "pair": code, "name": "old phone" }),
            "1".to_string(),
        ) else {
            panic!("v1 pairing");
        };
        hub.confirm_device(&device.id).unwrap();
        // The first account keeps devices paired before it.
        hub.relay.register(&hub, "u1").unwrap();
        assert!(hub.store.device(&device.id).is_some());
        let joined = authorize_account(
            &hub,
            &phone,
            &json!({}),
            &caller("u1", "a1"),
            "1".to_string(),
            &|| false,
        );
        assert!(matches!(joined, Ok(Verdict::Accepted(_))));
        assert_eq!(hub.store.device_accounts()[&device.id].user, "u1");
        // A v1 client keeps working on a v2 host while it is paired.
        assert!(matches!(
            authorize(&hub, &phone, &json!({}), "1".to_string()),
            Ok(Verdict::Accepted(_))
        ));

        // Another account: every remote device is revoked, a chat binding is not.
        hub.relay.register(&hub, "u1").unwrap();
        assert!(
            hub.store.device(&device.id).is_some(),
            "the same account changes nothing"
        );
        hub.relay.register(&hub, "u2").unwrap();
        assert!(hub.store.device(&device.id).is_none());
        assert_eq!(hub.store.setting(USER_SETTING), json!("u2"));
        assert_eq!(hub.relay.status_json()["user"], "u2");
    }

    #[test]
    fn the_v2_url_sits_next_to_v1_and_lan_pairing_needs_a_sign_in() {
        let hub = hub("urls");
        assert_eq!(
            hub.relay.v2_url().as_deref(),
            Some("wss://relay.example/relay/v2")
        );
        // No device key: signed out.
        assert!(!hub.relay.signed_in());
        assert_eq!(hub.relay.wanted_mode(), Mode::V1);
        assert!(hub
            .relay
            .lan_pairing_allowed()
            .unwrap_err()
            .starts_with("sign_in_required"));
        let status = hub.relay.status_json();
        assert_eq!(
            (status["signed_in"].clone(), status["mode"].clone()),
            (json!(false), json!("off"))
        );
        // Signed in: v2 and LAN pairing; a refused key counts as signed out.
        *lock(&hub.relay.test_key) = Some("sk-device".to_string());
        assert_eq!(hub.relay.wanted_mode(), Mode::V2);
        assert!(hub.relay.lan_pairing_allowed().is_ok());
        *lock(&hub.relay.refused) = Some(bytes_hash(b"sk-device"));
        assert!(!hub.relay.signed_in());
        assert_eq!(hub.relay.wanted_mode(), Mode::V1);
        // A new login (another key) is signed in again.
        *lock(&hub.relay.test_key) = Some("sk-new".to_string());
        assert!(hub.relay.signed_in());
        // v2 unreachable: v1 for a while.
        *lock(&hub.relay.v1_until) = Some(Instant::now() + V2_RETRY);
        assert_eq!(hub.relay.wanted_mode(), Mode::V1);
    }
}

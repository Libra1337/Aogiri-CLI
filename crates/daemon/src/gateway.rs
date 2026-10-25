//! The local gateway. Claude Code and Codex sessions on the LynShen gateway
//! send their requests here (`http://127.0.0.1:<port>/gw/v1/…`), not to the
//! gateway itself, and the daemon forwards them:
//!
//! - with a LynShen access token valid at that moment: the engines would keep
//!   the one they started with, which expires after an hour (every request
//!   then fails with 401);
//! - with the session's group (`X-LynShen-Group`), when that group serves
//!   the requested model; else the model's default group from `lynshen_groups`
//!   in config.json; else none, and the gateway routes on its own.
//!
//! A session on a custom provider (crate::tool_providers) passes here too,
//! with a key of its own: its requests go to the provider's URL with the
//! provider's key alone. They never carry the LynShen token, the session's
//! group, its turn or any other `X-LynShen-*` header, and are not LynShen
//! gateway use (crate::usage keeps them local).
//!
//! Only engines the daemon started pass here: native tools keep their own
//! settings. An engine holds a per-session local key the daemon swaps for the
//! real token, so the token never reaches its environment or disk. A request
//! must name a loopback host (no DNS rebinding) and carry a key the daemon
//! issued (a web page cannot set that header on a cross-site request).

use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

const PREFIX: &str = "/gw";
/// Requests carry whole conversations, images included.
const MAX_BODY: usize = 64 * 1024 * 1024;
/// Which group serves which model: refetched after this long.
const CATALOG_TTL: Duration = Duration::from_secs(600);
/// A streamed reply may pause this long between chunks (long thinking).
const READ_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Default)]
struct State {
    /// Local key → the session it was issued to ("" until the engine names
    /// a new conversation, see `bind`).
    keys: HashMap<String, String>,
    /// Session → the group it routes to.
    groups: HashMap<String, String>,
    /// Local key → the custom provider its requests go to (`base_url`,
    /// `format`, `key`, `tool`, `id`), instead of the LynShen gateway.
    custom: HashMap<String, Value>,
    /// Session → the turn its requests are charged to (`X-LynShen-Turn`, see
    /// crate::usage): the running one, else the last until the next opens.
    turns: HashMap<String, String>,
    /// The gateway's groups (`/v1/open/groups`), and when they were read.
    catalog: Option<(Instant, Vec<Value>)>,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

static PORT: OnceLock<u16> = OnceLock::new();

/// The daemon listens on `port`; engines reach the gateway there.
pub fn set_port(port: u16) {
    let _ = PORT.set(port);
}

/// Base URL an engine is given (append `/v1/…`).
pub fn base_url() -> Result<String, String> {
    let port = PORT.get().ok_or("the daemon is not listening")?;
    Ok(format!("http://127.0.0.1:{port}{PREFIX}"))
}

/// A new local key for `session` ("" while the engine has not named it).
pub fn issue(session: &str) -> Result<String, String> {
    let mut bytes = [0u8; 24];
    getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
    let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let key = format!("jgw-{key}");
    state().keys.insert(key.clone(), session.to_string());
    Ok(key)
}

/// The engine named its conversation: requests with `key` are `session`'s.
pub fn bind(key: &str, session: &str) {
    if let Some(owner) = state().keys.get_mut(key) {
        *owner = session.to_string();
    }
}

/// The engine holding `key` is gone.
pub fn revoke(key: &str) {
    let mut state = state();
    state.keys.remove(key);
    state.custom.remove(key);
}

/// Requests with `key` go to `provider` (`base_url`, `format`, `key`) and
/// nowhere else.
pub fn set_custom(key: &str, provider: Value) {
    state().custom.insert(key.to_string(), provider);
}

/// `session` routes to `group` (None or empty: no choice).
pub fn set_group(session: &str, group: Option<&str>) {
    let mut state = state();
    match group.map(str::trim).filter(|g| !g.is_empty()) {
        Some(group) => state.groups.insert(session.to_string(), group.to_string()),
        None => state.groups.remove(session),
    };
}

/// `session`'s requests belong to `turn` (None: its engine stopped).
pub fn set_turn(session: &str, turn: Option<&str>) {
    let mut state = state();
    match turn {
        Some(turn) => state.turns.insert(session.to_string(), turn.to_string()),
        None => state.turns.remove(session),
    };
}

/// Whether a request path is the gateway's.
pub fn is_gateway(path: &str) -> bool {
    path == PREFIX || path.starts_with("/gw/")
}

/// Where requests go and with which token, read per request.
pub struct Upstream {
    pub api: String,
    pub token: String,
}

/// Answers one gateway request on `stream` (`head` is its request head,
/// peeked, still unread).
pub fn serve(stream: TcpStream, head: &str) -> Result<(), String> {
    serve_with(
        stream,
        head,
        live_upstream,
        live_catalog,
        read_default_groups,
    )
}

fn live_upstream() -> Result<Upstream, String> {
    let (api, token) = lynshen_agent_core::lynshen_gateway_token()?;
    Ok(Upstream { api, token })
}

/// `lynshen_groups` in `~/.lynshen/config.json`: the LynShen engine's per-model
/// group choice, the default here too.
fn read_default_groups() -> BTreeMap<String, String> {
    let path = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| {
            std::path::PathBuf::from(home)
                .join(".lynshen")
                .join("config.json")
        });
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|config| serde_json::from_value(config["lynshen_groups"].clone()).ok())
        .unwrap_or_default()
}

/// Group id → models, cached for `CATALOG_TTL`. None when unreadable: no
/// group is then sent, and the gateway routes on its own.
fn live_catalog(upstream: &Upstream) -> Option<HashMap<String, HashSet<String>>> {
    let groups = live_groups(upstream)?;
    Some(
        groups
            .iter()
            .filter_map(|group| {
                let id = group["id"].as_str()?.to_string();
                let models = group["models"]
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                Some((id, models))
            })
            .collect(),
    )
}

/// The gateway's groups, cached for `CATALOG_TTL`.
fn live_groups(upstream: &Upstream) -> Option<Vec<Value>> {
    if let Some((at, groups)) = &state().catalog {
        if at.elapsed() < CATALOG_TTL {
            return Some(groups.clone());
        }
    }
    let url = format!("{}/v1/open/groups", upstream.api.trim_end_matches('/'));
    let value: Value = agent()
        .get(&url)
        .set("Authorization", &format!("Bearer {}", upstream.token))
        .timeout(Duration::from_secs(15))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let groups = value["groups"].as_array()?.clone();
    state().catalog = Some((Instant::now(), groups.clone()));
    Some(groups)
}

/// What a client that cannot read this machine's LynShen login (the remote
/// page) needs to offer the gateway: the models the user chose to show
/// (`lynshen_models` in config.json) and the gateway's groups. Empty lists
/// when not signed in or the gateway cannot be reached.
pub fn catalog_json() -> Value {
    // The window the engine budgets with: through the pinned group, and a
    // hand-set override (capped at the gateway's largest) over the gateway's.
    let models: Vec<Value> = lynshen_agent_core::lynshen_visible_models()
        .into_iter()
        .map(|model| {
            let mut entry = json!({
                "name": model.name,
                "display_name": model.display_name,
                "context_window": model.context_window,
            });
            // Kept callable by name; pickers leave it out.
            if model.hidden {
                entry["hidden"] = json!(true);
            }
            entry
        })
        .collect();
    let groups = live_upstream()
        .ok()
        .and_then(|upstream| live_groups(&upstream))
        .unwrap_or_default();
    json!({ "type": "gateway_catalog", "models": models, "groups": groups })
}

/// The desktop plan of the account this computer is signed in with (the
/// gateway's `GET /api/desktop/oauth/plan`), for the phone's 今天用量: the
/// plan's name and its windows with the share used. Cached a minute.
/// `plan: null` when signed in without a plan; an error when not signed in.
pub fn plan_quota_json() -> Result<Value, String> {
    static CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);
    if let Some((at, value)) = CACHE.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        if at.elapsed() < Duration::from_secs(60) {
            return Ok(value.clone());
        }
    }
    let upstream = live_upstream()?;
    let url = format!(
        "{}/api/desktop/oauth/plan",
        upstream.api.trim_end_matches('/')
    );
    let reply: Value = agent()
        .get(&url)
        .set("Authorization", &format!("Bearer {}", upstream.token))
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|error| error.to_string())?
        .into_json()
        .map_err(|error| error.to_string())?;
    let value = plan_quota_from(&reply);
    *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), value.clone()));
    Ok(value)
}

/// `plan_quota` from the gateway's plan status: each window's share used.
fn plan_quota_from(reply: &Value) -> Value {
    let plan = &reply["plan"];
    if plan.is_null() || plan["active"] == false {
        return json!({ "type": "plan_quota", "plan": null, "windows": [] });
    }
    let number = |v: &Value| {
        v.as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or(v.as_f64())
    };
    let windows: Vec<Value> = plan["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let quota = number(&w["quota_cny"])?;
            let used = number(&w["used_cny"]).unwrap_or(0.0);
            let pct = if quota > 0.0 {
                (used / quota * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            Some(json!({ "kind": w["kind"], "used_pct": pct.round(), "resets_at": w["resets_at"] }))
        })
        .collect();
    json!({ "type": "plan_quota", "plan": plan["name"], "windows": windows })
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(READ_TIMEOUT)
            .redirects(0)
            // Direct, like the LynShen engine's own requests: ureq would read
            // ALL_PROXY first and has no SOCKS support built in.
            .build()
    })
}

/// The group a request for `model` goes to: the session's, else the model's
/// default — each only if it serves the model (a pin it does not serve would
/// fail the request, e.g. Claude Code's small background model).
fn pick_group(
    chosen: Option<&str>,
    default: Option<&str>,
    model: &str,
    catalog: Option<&HashMap<String, HashSet<String>>>,
) -> Option<String> {
    let catalog = catalog?;
    [chosen, default]
        .into_iter()
        .flatten()
        .find(|group| {
            catalog
                .get(*group)
                .is_some_and(|models| models.contains(model))
        })
        .map(str::to_string)
}

struct Request {
    method: String,
    /// Path and query after `/gw`.
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Request headers not passed on: hop-by-hop, the client's credential and
/// framing (the daemon sets its own), encodings (the reply is relayed as
/// is, so it must come uncompressed), and any of LynShen's own: the daemon
/// sets those it means to, and only towards the LynShen gateway.
fn dropped_request_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("x-lynshen-")
        || matches!(
            name.as_str(),
            "host"
                | "authorization"
                | "x-api-key"
                | "content-length"
                | "transfer-encoding"
                | "connection"
                | "keep-alive"
                | "proxy-connection"
                | "proxy-authorization"
                | "te"
                | "upgrade"
                | "expect"
                | "accept-encoding"
        )
}

/// Reply headers not passed back: hop-by-hop and framing (the daemon closes
/// the connection after the body).
fn dropped_reply_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "content-encoding"
            | "upgrade"
    )
}

fn serve_with(
    stream: TcpStream,
    head: &str,
    upstream: impl Fn() -> Result<Upstream, String>,
    catalog: impl Fn(&Upstream) -> Option<HashMap<String, HashSet<String>>>,
    defaults: impl Fn() -> BTreeMap<String, String>,
) -> Result<(), String> {
    let mut out = stream.try_clone().map_err(|error| error.to_string())?;
    let _ = out.set_nodelay(true);
    // Who is asking is in the head: refuse before reading a body.
    let (method, target, headers) = parse_head(head);
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    if !loopback_host(header("host").unwrap_or_default()) {
        return reply_error(
            &mut out,
            403,
            "the local gateway only answers on the loopback address",
        );
    }
    let presented = header("authorization")
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .or_else(|| header("x-api-key"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let session = match state().keys.get(&presented) {
        Some(session) if !presented.is_empty() => session.clone(),
        _ => return reply_error(&mut out, 401, "unknown local gateway key"),
    };
    if !target.starts_with("/v1/") {
        return reply_error(&mut out, 404, "the local gateway serves /v1/ only");
    }
    let request = match read_body(stream, &headers) {
        Ok(body) => Request {
            method,
            target,
            headers,
            body,
        },
        Err((status, message)) => return reply_error(&mut out, status, &message),
    };
    // A custom provider's session: its own URL and key, nothing of LynShen's
    // (no login needed, no group, no turn).
    let custom = state().custom.get(&presented).cloned();
    let upstream = match &custom {
        Some(provider) => Ok(Upstream {
            api: provider["base_url"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            token: provider["key"].as_str().unwrap_or_default().to_string(),
        }),
        None => upstream(),
    };
    let upstream = match upstream {
        Ok(upstream) => upstream,
        // "not logged in to LynShen …": the desktop reads it as a sign-in error.
        Err(message) => return reply_error(&mut out, 401, &message),
    };
    let model = serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|body| body["model"].as_str().map(str::to_string))
        .unwrap_or_default();
    let (chosen, default) = match &custom {
        Some(_) => (None, None),
        None => (
            state().groups.get(&session).cloned(),
            Some(&model)
                .filter(|model| !model.is_empty())
                .and_then(|model| defaults().get(model).cloned()),
        ),
    };
    // Nothing chosen: no catalog lookup on the way.
    let group = if model.is_empty() || (chosen.is_none() && default.is_none()) {
        None
    } else {
        pick_group(
            chosen.as_deref(),
            default.as_deref(),
            &model,
            catalog(&upstream).as_ref(),
        )
    };

    let url = match &custom {
        Some(_) => custom_url(&upstream.api, &request.target),
        None => format!("{}{}", upstream.api.trim_end_matches('/'), request.target),
    };
    let mut call = match &custom {
        Some(_) => custom_agent(&url).request(&request.method, &url),
        None => agent().request(&request.method, &url),
    }
    .set("Accept-Encoding", "identity");
    // An Anthropic-format provider takes its key as Claude Code's own client
    // sends an API key; the others a bearer token.
    call = match &custom {
        Some(provider) if provider["format"] == "anthropic" => {
            call.set("x-api-key", &upstream.token)
        }
        _ => call.set("Authorization", &format!("Bearer {}", upstream.token)),
    };
    for (name, value) in &request.headers {
        if !dropped_request_header(name) {
            call = call.set(name, value);
        }
    }
    if custom.is_none() {
        if let Some(group) = &group {
            call = call.set("X-LynShen-Group", group);
        }
        if let Some(turn) = state().turns.get(&session).cloned() {
            call = call.set("X-LynShen-Turn", &turn);
        }
    }
    let result = if request.body.is_empty() && request.method.eq_ignore_ascii_case("GET") {
        call.call()
    } else {
        call.send_bytes(&request.body)
    };
    let response = match result {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(error)) => {
            let target = match &custom {
                Some(provider) => format!(
                    "the custom provider {}",
                    provider["name"].as_str().unwrap_or_default()
                ),
                None => "the LynShen gateway".to_string(),
            };
            return reply_error(&mut out, 502, &format!("cannot reach {target}: {error}"));
        }
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status(),
        response.status_text()
    );
    for name in response.headers_names() {
        if dropped_reply_header(&name) {
            continue;
        }
        for value in response.all(&name) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    head.push_str("Connection: close\r\n\r\n");
    // A custom Anthropic-format API that refuses Claude Code's
    // mid-conversation system messages: told as Claude Code's gateway
    // contract names it (same status), so it resends without them.
    if custom
        .as_ref()
        .is_some_and(|p| p["format"] == "anthropic" && matches!(response.status(), 400 | 422))
    {
        let mut body = Vec::new();
        let _ = response.into_reader().take(1 << 20).read_to_end(&mut body);
        if let Some(class) = capability_rejected(&request.body, &body) {
            body = json!({ "type": "error", "error": {
                "type": "invalid_request_error",
                "message": format!("capability_rejected: {class}"),
            } })
            .to_string()
            .into_bytes();
        }
        return out
            .write_all(head.as_bytes())
            .and_then(|()| out.write_all(&body))
            .map_err(|error| error.to_string());
    }
    out.write_all(head.as_bytes())
        .map_err(|error| error.to_string())?;
    // Relayed as it arrives (server-sent events); a client that hangs up
    // ends the copy, and dropping the reader closes the upstream request.
    let mut body = response.into_reader();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => return Err(format!("upstream read failed: {error}")),
        };
        if out
            .write_all(&buffer[..read])
            .and_then(|()| out.flush())
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

/// A custom provider's URL for `target` (`/v1/…` and its query): its base
/// may name the version (`…/v1`) or not.
fn custom_url(api: &str, target: &str) -> String {
    let api = api.trim_end_matches('/');
    match api.ends_with("/v1") {
        true => format!("{api}{}", target.strip_prefix("/v1").unwrap_or(target)),
        false => format!("{api}{target}"),
    }
}

/// The client for a custom provider: through the user's proxy, as the tool
/// would have called it, never following a redirect (the key goes to the
/// configured host only).
fn custom_agent(url: &str) -> ureq::Agent {
    let builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(READ_TIMEOUT)
        .redirects(0);
    match lynshen_agent_core::env_proxy(url).and_then(|proxy| ureq::Proxy::new(proxy).ok()) {
        Some(proxy) => builder.proxy(proxy).build(),
        None => builder.build(),
    }
}

/// The capability class (Claude Code's `capability_rejected: <class>`) an
/// upstream's 400 refuses, when it is one the client can recover from. Only
/// `mid_conv_system`: the request has a `{role: "system"}` message and the
/// error names that message's path or a system role.
fn capability_rejected(request: &[u8], reply: &[u8]) -> Option<&'static str> {
    let body: Value = serde_json::from_slice(request).ok()?;
    let system: Vec<usize> = body["messages"]
        .as_array()?
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "system")
        .map(|(i, _)| i)
        .collect();
    if system.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(reply).to_ascii_lowercase();
    // `messages.1` / `messages[1]`, not `messages.12`.
    let names = |path: &str| {
        text.match_indices(path)
            .any(|(at, _)| !text[at + path.len()..].starts_with(|c: char| c.is_ascii_digit()))
    };
    let at_path = system
        .iter()
        .any(|i| names(&format!("messages.{i}")) || names(&format!("messages[{i}]")));
    (at_path || (text.contains("role") && text.contains("system"))).then_some("mid_conv_system")
}

fn loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

/// The peeked request head: method, target after `/gw`, headers.
fn parse_head(head: &str) -> (String, String, Vec<(String, String)>) {
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_string();
    let target = first.next().unwrap_or_default();
    let target = target.strip_prefix(PREFIX).unwrap_or(target).to_string();
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    (method, target, headers)
}

/// Reads past the peeked head, then the body (by Content-Length or chunked).
/// Errors carry the status to answer with.
fn read_body(stream: TcpStream, headers: &[(String, String)]) -> Result<Vec<u8>, (u16, String)> {
    let mut reader = BufReader::new(stream);
    // Consume the head (peeked, so still in the socket).
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Err((400, "incomplete request".into())),
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {}
            Err(error) => return Err((400, error.to_string())),
        }
    }
    let find = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    if find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        return read_chunked(&mut reader);
    }
    let length: usize = find("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err((413, "request body too large".into()));
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| (400, error.to_string()))?;
    Ok(body)
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, (u16, String)> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader
            .read_line(&mut line)
            .map_err(|error| (400, error.to_string()))?;
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or_default(), 16)
            .map_err(|_| (400, "bad chunk size".to_string()))?;
        if size == 0 {
            // Trailers, then the blank line.
            loop {
                line.clear();
                if reader
                    .read_line(&mut line)
                    .map_err(|error| (400, error.to_string()))?
                    == 0
                    || line.trim().is_empty()
                {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > MAX_BODY {
            return Err((413, "request body too large".into()));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader
            .read_exact(&mut body[start..])
            .map_err(|error| (400, error.to_string()))?;
        line.clear();
        reader
            .read_line(&mut line)
            .map_err(|error| (400, error.to_string()))?;
    }
}

fn reply_error(out: &mut TcpStream, status: u16, message: &str) -> Result<(), String> {
    let reason = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Bad Gateway",
    };
    let body =
        json!({ "error": { "type": "local_gateway_error", "message": message } }).to_string();
    let reply = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    out.write_all(reply.as_bytes())
        .map_err(|error| error.to_string())?;
    // Closing with the request still unread would reset the connection and
    // lose this reply: finish writing, then drop what already arrived (a
    // little, briefly — a refused body is never read in full).
    let _ = out.shutdown(std::net::Shutdown::Write);
    let _ = out.set_read_timeout(Some(Duration::from_millis(500)));
    let mut sink = [0u8; 16 * 1024];
    let mut drained = 0;
    while drained < 1024 * 1024 {
        match out.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(read) => drained += read,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    #[test]
    fn the_plan_quota_is_the_share_of_each_window_used() {
        let reply = json!({ "plan": { "name": "Sprout", "windows": [
            { "kind": "5h", "quota_cny": "10", "used_cny": "2.5", "resets_at": "2026-10-19T12:00:00Z" },
            { "kind": "7d", "quota_cny": "50", "used_cny": "60", "resets_at": null },
        ] } });
        let quota = super::plan_quota_from(&reply);
        assert_eq!(quota["plan"], "Sprout");
        assert_eq!(quota["windows"][0]["used_pct"], 25.0);
        assert_eq!(quota["windows"][1]["used_pct"], 100.0);
        assert_eq!(
            super::plan_quota_from(&json!({ "plan": null }))["windows"],
            json!([])
        );
        assert_eq!(
            super::plan_quota_from(&json!({ "plan": { "active": false } }))["plan"],
            Value::Null
        );
    }

    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn hosts_must_be_loopback() {
        assert!(loopback_host("127.0.0.1:7788"));
        assert!(loopback_host("localhost:7788"));
        assert!(loopback_host("[::1]:7788"));
        assert!(!loopback_host("evil.example:7788"));
        assert!(!loopback_host("127.0.0.1.evil.example"));
        assert!(!loopback_host(""));
    }

    #[test]
    fn a_group_is_sent_only_where_it_serves_the_model() {
        let catalog: HashMap<String, HashSet<String>> = HashMap::from([
            (
                "g-claude".into(),
                HashSet::from(["claude-opus-5-5".to_string()]),
            ),
            (
                "g-all".into(),
                HashSet::from([
                    "claude-opus-5-5".to_string(),
                    "claude-haiku-4-5".to_string(),
                ]),
            ),
        ]);
        let pick = |chosen, default, model| pick_group(chosen, default, model, Some(&catalog));
        assert_eq!(
            pick(Some("g-claude"), None, "claude-opus-5-5").as_deref(),
            Some("g-claude")
        );
        // The session's group lacks the background model: the default, then none.
        assert_eq!(
            pick(Some("g-claude"), Some("g-all"), "claude-haiku-4-5").as_deref(),
            Some("g-all")
        );
        assert_eq!(pick(Some("g-claude"), None, "claude-haiku-4-5"), None);
        assert_eq!(pick(None, None, "claude-opus-5-5"), None);
        // Unknown catalog: never a group that might not serve the model.
        assert_eq!(
            pick_group(Some("g-claude"), None, "claude-opus-5-5", None),
            None
        );
    }

    #[test]
    fn keys_bind_to_the_session_once_named_and_revoke() {
        let key = issue("").unwrap();
        assert!(key.starts_with("jgw-") && key.len() == 52);
        bind(&key, "s-named");
        assert_eq!(state().keys.get(&key).map(String::as_str), Some("s-named"));
        revoke(&key);
        assert!(!state().keys.contains_key(&key));
    }

    /// A fake LynShen gateway: answers one request with a streamed body and
    /// reports what it received.
    fn fake_upstream() -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut received = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                received.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            // A local dev-server scanner may probe the test's port.
            if !received.starts_with("POST ") {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            received.push_str(&String::from_utf8_lossy(&body));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
            for chunk in ["data: one\n\n", "data: two\n\n"] {
                stream
                    .write_all(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes())
                    .unwrap();
                stream.flush().unwrap();
            }
            stream.write_all(b"0\r\n\r\n").unwrap();
            break received;
        });
        (format!("http://{addr}"), handle)
    }

    /// Sends `request` through the gateway against `api`; returns the reply.
    fn through_gateway(request: &str, api: String, group: Option<&str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let group = group.map(str::to_string);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let head = crate::http::peek_head(&stream).unwrap();
            let catalog = HashMap::from([("g1".to_string(), HashSet::from(["m1".to_string()]))]);
            serve_with(
                stream,
                &head,
                || {
                    Ok(Upstream {
                        api: api.clone(),
                        token: "real-token".into(),
                    })
                },
                |_| Some(catalog.clone()),
                || {
                    group
                        .iter()
                        .map(|g| ("m1".to_string(), g.clone()))
                        .collect()
                },
            )
            .unwrap();
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        server.join().unwrap();
        reply
    }

    #[test]
    fn requests_go_up_with_the_real_token_and_group_and_stream_back() {
        let key = issue("s-stream").unwrap();
        let (api, upstream) = fake_upstream();
        let body = r#"{"model":"m1","stream":true}"#;
        let request = format!(
            "POST /gw/v1/responses HTTP/1.1\r\nHost: 127.0.0.1:7788\r\nAuthorization: Bearer {key}\r\nAccept-Encoding: br\r\nX-Custom: kept\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let reply = through_gateway(&request, api, Some("g1"));
        let received = upstream.join().unwrap().to_ascii_lowercase();
        assert!(received.starts_with("post /v1/responses "), "{received}");
        assert!(received.contains("authorization: bearer real-token"));
        assert!(!received.contains(&key), "the local key stays local");
        assert!(received.contains("x-lynshen-group: g1"));
        assert!(received.contains("x-custom: kept"));
        assert!(received.contains("accept-encoding: identity"));
        assert!(received.ends_with(body), "{received}");
        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
        assert!(!reply.to_ascii_lowercase().contains("transfer-encoding"));
        assert!(reply.ends_with("data: one\n\ndata: two\n\n"), "{reply}");
        revoke(&key);
    }

    #[test]
    fn unknown_keys_and_foreign_hosts_are_refused() {
        let request = "POST /gw/v1/responses HTTP/1.1\r\nHost: 127.0.0.1:7788\r\nAuthorization: Bearer nope\r\nContent-Length: 0\r\n\r\n";
        let reply = through_gateway(request, "http://127.0.0.1:9".into(), None);
        assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
        let key = issue("s-host").unwrap();
        let request = format!("POST /gw/v1/responses HTTP/1.1\r\nHost: evil.example:7788\r\nAuthorization: Bearer {key}\r\nContent-Length: 0\r\n\r\n");
        let reply = through_gateway(&request, "http://127.0.0.1:9".into(), None);
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        revoke(&key);
    }

    #[test]
    fn a_stranger_is_refused_before_its_body_is_read() {
        // Claims 60 MB but sends none: answered at once, not after reading it.
        let request = "POST /gw/v1/responses HTTP/1.1\r\nHost: 127.0.0.1:7788\r\nAuthorization: Bearer nope\r\nContent-Length: 60000000\r\n\r\n";
        let reply = through_gateway(request, "http://127.0.0.1:9".into(), None);
        assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    }

    #[test]
    fn no_group_chosen_means_no_catalog_lookup() {
        let key = issue("s-nogroup").unwrap();
        let (api, upstream) = fake_upstream();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let head = crate::http::peek_head(&stream).unwrap();
            serve_with(
                stream,
                &head,
                || {
                    Ok(Upstream {
                        api: api.clone(),
                        token: "t".into(),
                    })
                },
                |_| panic!("the catalog was read with no group chosen"),
                BTreeMap::new,
            )
            .unwrap();
        });
        let body = r#"{"model":"m1"}"#;
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .write_all(format!("POST /gw/v1/responses HTTP/1.1\r\nHost: localhost:1\r\nx-api-key: {key}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
            .unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        server.join().unwrap();
        assert!(!upstream
            .join()
            .unwrap()
            .to_ascii_lowercase()
            .contains("x-lynshen-group"));
        assert!(reply.starts_with("HTTP/1.1 200"));
        revoke(&key);
    }

    #[test]
    fn a_custom_provider_gets_its_own_key_and_nothing_of_lynshen() {
        let key = issue("s-custom").unwrap();
        let (api, upstream) = fake_upstream();
        set_custom(
            &key,
            json!({"tool":"claude","id":"private","name":"Private","base_url":format!("{api}/v1"),"key":"own-secret","format":"anthropic"}),
        );
        // The session has a group and a turn: neither goes to the provider.
        set_turn("s-custom", Some("t-private"));
        set_group("s-custom", Some("g1"));
        let body = r#"{"model":"m1","stream":true}"#;
        let request = format!("POST /gw/v1/messages?beta=true HTTP/1.1\r\nHost: localhost:7788\r\nAuthorization: Bearer {key}\r\nX-LynShen-Group: forged\r\nX-LynShen-Device: v1;hw=forged\r\nX-LynShen-Turn: forged\r\nanthropic-version: 2023-06-01\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        // `through_gateway` offers a LynShen login ("real-token") and a group
        // that serves m1: a custom route must use neither.
        let reply = through_gateway(&request, "http://127.0.0.1:9".into(), Some("g1"));
        let received = upstream.join().unwrap().to_ascii_lowercase();
        assert!(reply.ends_with("data: one\n\ndata: two\n\n"), "{reply}");
        assert!(
            received.starts_with("post /v1/messages?beta=true "),
            "{received}"
        );
        assert!(received.contains("x-api-key: own-secret"), "{received}");
        assert!(received.contains("anthropic-version: 2023-06-01"));
        assert!(!received.contains("authorization:"), "{received}");
        assert!(!received.contains("real-token"), "{received}");
        assert!(!received.contains(&key), "{received}");
        assert!(!received.contains("x-lynshen-"), "{received}");
        assert!(!received.contains("forged"), "{received}");
        revoke(&key);
        set_group("s-custom", None);
        set_turn("s-custom", None);
        assert!(!state().custom.contains_key(&key));
    }

    #[test]
    fn a_responses_provider_gets_a_bearer_key_on_its_own_base() {
        let key = issue("s-custom-codex").unwrap();
        let (api, upstream) = fake_upstream();
        set_custom(
            &key,
            json!({"tool":"codex","id":"relay","name":"Relay","base_url":format!("{api}/openai"),"key":"relay-key","format":"responses"}),
        );
        let body = r#"{"model":"gpt-x"}"#;
        let request = format!("POST /gw/v1/responses HTTP/1.1\r\nHost: 127.0.0.1:7788\r\nAuthorization: Bearer {key}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        through_gateway(&request, "http://127.0.0.1:9".into(), None);
        let received = upstream.join().unwrap().to_ascii_lowercase();
        assert!(
            received.starts_with("post /openai/v1/responses "),
            "{received}"
        );
        assert!(received.contains("authorization: bearer relay-key"));
        assert!(!received.contains("x-api-key") && !received.contains("x-lynshen-"));
        revoke(&key);
        assert_eq!(
            custom_url("https://h/v1/", "/v1/models"),
            "https://h/v1/models"
        );
        assert_eq!(
            custom_url("https://h/api", "/v1/models"),
            "https://h/api/v1/models"
        );
    }

    #[test]
    fn a_refused_mid_conversation_system_message_is_named_for_claude_code() {
        let request =
            br#"{"messages":[{"role":"user","content":"hi"},{"role":"system","content":"env"}]}"#;
        let refused = br#"{"type":"error","error":{"message":"Invalid input at messages.1.role"}}"#;
        assert_eq!(
            capability_rejected(request, refused),
            Some("mid_conv_system")
        );
        let worded = br#"{"error":{"message":"Unexpected role 'system' in messages"}}"#;
        assert_eq!(
            capability_rejected(request, worded),
            Some("mid_conv_system")
        );
        let other = br#"{"error":{"message":"Invalid input at messages.12.content"}}"#;
        assert_eq!(capability_rejected(request, other), None);
        let plain = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(capability_rejected(plain, refused), None);
    }

    #[test]
    fn chunked_request_bodies_are_read_whole() {
        let mut input = "4\r\nabcd\r\n3;x=1\r\nefg\r\n0\r\n\r\n".as_bytes();
        assert_eq!(read_chunked(&mut input).unwrap(), b"abcdefg");
    }
}

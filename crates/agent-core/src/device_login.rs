//! `lynshen login --device`: signs a computer without the desktop in to the
//! LynShen account with the gateway's device flow (monoize
//! `desktop-authorization.spec.md` DA-2, DA-4, DA-14) as the client
//! `lynshen-cli`. The key the gateway issues is this computer's device key
//! (`providers.lynshen-cli` in auth.json); the daemon reads it
//! (`account::computer_key`) and connects to relay v2 with it
//! (`docs/relay-protocol.md` §7). The desktop's own sign-in
//! (`providers.monoize`) is used first when both exist; both take one seat.

use crate::config::{AuthStore, Config};
use serde_json::{json, Value};
use std::{path::Path, path::PathBuf, time::Duration};

/// The device-flow client and the auth.json provider of its key.
pub const CLIENT_ID: &str = "lynshen-cli";
pub const PROVIDER: &str = "lynshen-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REVOKE_TIMEOUT: Duration = Duration::from_secs(5);
/// DA-2: a grant lives 600 s; polled every 5 s.
const DEFAULT_EXPIRES: u64 = 600;
const DEFAULT_INTERVAL: u64 = 5;

/// The account a sign-in is for, as the token reply names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub username: String,
    pub email: Option<String>,
}

impl Account {
    fn shown(&self) -> String {
        match &self.email {
            Some(email) if !email.is_empty() && *email != self.username => {
                format!("{} ({email})", self.username)
            }
            _ => self.username.clone(),
        }
    }
}

/// What the device flow needs from outside: where the gateway is, the
/// computer's identifiers, and how to talk to the user, wait and open the
/// browser (tests replace the last three).
pub struct Flow<'a> {
    /// The gateway's web base (`lynshen_web_url`).
    pub web_url: &'a str,
    /// `X-LynShen-Device` (AD-H1), sent on every request.
    pub device_header: Option<&'a str>,
    /// DA-4a: this installation's id, when it has one.
    pub installation_id: Option<String>,
    pub say: &'a mut dyn FnMut(&str),
    pub sleep: &'a mut dyn FnMut(Duration),
    pub open_browser: &'a mut dyn FnMut(&str) -> bool,
}

/// Runs the device flow until the browser approves it, and returns the
/// device key and its account. The key is never passed to `say`.
pub fn authorize(flow: Flow<'_>) -> Result<(String, Account), String> {
    let Flow {
        web_url,
        device_header,
        installation_id,
        say,
        sleep,
        open_browser,
    } = flow;
    let base = web_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("lynshen_web_url is empty in ~/.lynshen/config.json".to_string());
    }
    let start = post(
        base,
        "/api/desktop/oauth/device",
        &json!({ "client_id": CLIENT_ID }),
        device_header,
    )?;
    if start.status != 200 {
        return Err(refusal(&start));
    }
    let device_code = start.body["device_code"].as_str().unwrap_or_default();
    let user_code = start.body["user_code"].as_str().unwrap_or_default();
    if device_code.is_empty() || user_code.is_empty() {
        return Err("the gateway answered without a device code".to_string());
    }
    let mut interval = Duration::from_secs(
        start.body["interval"]
            .as_u64()
            .unwrap_or(DEFAULT_INTERVAL)
            .max(1),
    );
    let lifetime =
        Duration::from_secs(start.body["expires_in"].as_u64().unwrap_or(DEFAULT_EXPIRES));
    let url = format!(
        "{base}/oauth/authorize?user_code={}",
        llm_provider_kit::oauth::url_encode(user_code)
    );
    let shown_code = if user_code.len() == 12 {
        format!("{}-{}", &user_code[..6], &user_code[6..])
    } else {
        user_code.to_string()
    };
    say(&format!(
        "Open this page to sign this computer in to LynShen (code {shown_code}):\n  {url}"
    ));
    if !open_browser(&url) {
        say("Could not open a browser here: open the page on any device.");
    }
    say("Waiting for the approval…");

    let mut body = json!({ "client_id": CLIENT_ID, "device_code": device_code });
    if let Some(id) = installation_id {
        body["installation_id"] = json!(id);
    }
    // Time is counted by the waits, which make up nearly all of it.
    let mut waited = Duration::ZERO;
    loop {
        if waited >= lifetime {
            return Err(expired());
        }
        sleep(interval);
        waited += interval;
        let reply = post(base, "/api/desktop/oauth/token", &body, device_header)?;
        if let Some(key) = reply.body["access_token"]
            .as_str()
            .filter(|_| reply.status == 200)
        {
            let user = &reply.body["user"];
            let account = Account {
                username: user["username"].as_str().unwrap_or_default().to_string(),
                email: user["email"].as_str().map(str::to_string),
            };
            return Ok((key.to_string(), account));
        }
        let code = error_code(&reply.body);
        match (reply.status, code.as_str()) {
            (200, "authorization_pending") => {}
            // DA-4b, and the public IP limiter (HTTP 429).
            (200 | 429, "slow_down") | (429, _) => interval += Duration::from_secs(5),
            (_, "expired_token") => return Err(expired()),
            (_, "access_denied") => {
                return Err("the sign-in was refused in the browser".to_string())
            }
            _ => return Err(refusal(&reply)),
        }
    }
}

fn expired() -> String {
    "the code expired before it was approved: run `lynshen login --device` again".to_string()
}

/// Saves `key` as this computer's device key (`providers.lynshen-cli`),
/// encrypted as the rest of auth.json is.
pub fn store_key(key: &str) -> Result<(), String> {
    let config = Config::load_or_create().map_err(|error| error.to_string())?;
    let _lock = crate::oauth::lock_auth_refresh()?;
    let path = crate::config::profile_dir()
        .map_err(|error| error.to_string())?
        .join("auth.json");
    store_key_at(path, config.encrypt_secrets, key)
}

pub(crate) fn store_key_at(path: PathBuf, encrypt: bool, key: &str) -> Result<(), String> {
    let mut auth =
        AuthStore::load_or_create_at(path, encrypt).map_err(|error| error.to_string())?;
    auth.set_key(PROVIDER, key.to_string());
    auth.save().map_err(|error| error.to_string())
}

/// `lynshen login --device`: the whole sign-in, talking through `say`.
/// Returns the account.
pub fn login(say: &mut dyn FnMut(&str)) -> Result<Account, String> {
    let config = Config::load_or_create().map_err(|error| error.to_string())?;
    let desktop = AuthStore::load_or_create(config.encrypt_secrets)
        .ok()
        .and_then(|auth| auth.key_for("monoize").map(|key| !key.trim().is_empty()))
        .unwrap_or(false);
    if desktop {
        say("This computer is signed in through Aogiri desktop, which remote access uses first; this sign-in is kept as a second one.");
    }
    let installation = installation_id(&crate::config::profile_dir().map_err(|e| e.to_string())?);
    let (key, account) = authorize(Flow {
        web_url: &config.lynshen_web_url,
        device_header: crate::device_id::header_value(),
        installation_id: installation,
        say,
        sleep: &mut std::thread::sleep,
        open_browser: &mut |url| llm_provider_kit::oauth::open_browser(url).is_ok(),
    })?;
    store_key(&key)?;
    crate::log_info!("account", "signed in with the device flow (lynshen-cli)");
    say(&format!(
        "Signed in to LynShen as {}. Remote access through the account is on once the relay is on (`lynshen daemon relay on`); a running daemon picks up the sign-in by itself.",
        account.shown()
    ));
    Ok(account)
}

/// `lynshen logout --device`: forgets `providers.lynshen-cli`. The gateway
/// deletes the key only while it is fresh and unused (AD-R2); otherwise the
/// computer stays in the account's devices until removed there.
pub fn logout(say: &mut dyn FnMut(&str)) -> Result<(), String> {
    let config = Config::load_or_create().map_err(|error| error.to_string())?;
    let _lock = crate::oauth::lock_auth_refresh()?;
    let mut auth =
        AuthStore::load_or_create(config.encrypt_secrets).map_err(|error| error.to_string())?;
    let Some(key) = auth
        .key_for(PROVIDER)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
    else {
        say("This computer is not signed in with `lynshen login --device`.");
        return Ok(());
    };
    revoke(&config.lynshen_web_url, &key);
    auth.remove_key(PROVIDER);
    auth.save().map_err(|error| error.to_string())?;
    say(&format!(
        "Signed out. The computer stays in the account's device list until you remove it at {}/dashboard.",
        config.lynshen_web_url.trim_end_matches('/')
    ));
    Ok(())
}

/// Best effort (DA-5 revoke: only a fresh, unused key is deleted).
fn revoke(web_url: &str, key: &str) {
    let mut request = ureq::post(&format!(
        "{}/api/desktop/oauth/revoke",
        web_url.trim().trim_end_matches('/')
    ))
    .timeout(REVOKE_TIMEOUT)
    .set("Authorization", &format!("Bearer {key}"));
    if let Some(device) = crate::device_id::header_value() {
        request = request.set(crate::device_id::HEADER, device);
    }
    let _ = request.call();
}

/// DA-4a: the installation id this computer's LynShen apps keep
/// (`~/.lynshen/installation-id`, written by the desktop), when there is a
/// valid one. The CLI does not create it.
fn installation_id(dir: &Path) -> Option<String> {
    let stored = std::fs::read_to_string(dir.join("installation-id")).ok()?;
    let stored = stored.trim();
    ((8..=64).contains(&stored.len())
        && stored
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    .then(|| stored.to_string())
}

struct Reply {
    status: u16,
    body: Value,
}

fn post(base: &str, path: &str, body: &Value, device: Option<&str>) -> Result<Reply, String> {
    let mut request = ureq::post(&format!("{base}{path}"))
        .timeout(REQUEST_TIMEOUT)
        .set("Content-Type", "application/json");
    if let Some(device) = device {
        request = request.set(crate::device_id::HEADER, device);
    }
    match request.send_json(body.clone()) {
        Ok(response) => Ok(Reply {
            status: response.status(),
            body: response.into_json().unwrap_or(Value::Null),
        }),
        Err(ureq::Error::Status(status, response)) => Ok(Reply {
            status,
            body: response.into_json().unwrap_or(Value::Null),
        }),
        Err(error) => Err(format!("could not reach the LynShen gateway: {error}")),
    }
}

/// `{"error": "<code>"}` (the token poll) or `{"error": {"code"}}`.
fn error_code(body: &Value) -> String {
    match &body["error"] {
        Value::String(code) => code.clone(),
        error => error["code"]
            .as_str()
            .or(body["code"].as_str())
            .unwrap_or_default()
            .to_string(),
    }
}

/// The gateway's own words for a refusal (device limits, an unverified
/// email, a disabled account), else its status.
fn refusal(reply: &Reply) -> String {
    let message = reply.body["error"]["message"]
        .as_str()
        .or(reply.body["message"].as_str())
        .filter(|message| !message.is_empty());
    match (message, error_code(&reply.body)) {
        (Some(message), _) => format!("the gateway refused the sign-in: {message}"),
        (None, code) if !code.is_empty() => format!("the gateway refused the sign-in: {code}"),
        _ => format!("the gateway answered HTTP {}", reply.status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
    };

    const KEY: &str = "sk-device-key-0123456789abcdef";
    const HEADER: &str = "v1;hw=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA;os=linux;cli=0.7.9";

    #[derive(Debug, Clone)]
    struct Seen {
        path: String,
        device: Option<String>,
        body: Value,
    }

    /// A gateway that answers each request with the next of `answers`
    /// (status, body) and records what it got.
    fn gateway(answers: Vec<(u16, Value)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let mut answers: VecDeque<_> = answers.into();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut reader = BufReader::new(stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap_or(0);
                let path = first.split(' ').nth(1).unwrap_or_default().to_string();
                let (mut length, mut device) = (0, None);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("x-lynshen-device:") {
                        device = line.split_once(':').map(|(_, v)| v.trim().to_string());
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                log.lock().unwrap().push(Seen {
                    path,
                    device,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                let (status, answer) = answers
                    .pop_front()
                    .unwrap_or((500, json!({"error": "no more answers"})));
                let answer = answer.to_string();
                let _ = reader.get_mut().write_all(
                    format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len()).as_bytes(),
                );
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn start() -> (u16, Value) {
        (
            200,
            json!({ "device_code": "d".repeat(64), "user_code": "ABCDEF123456", "expires_in": 600, "interval": 5 }),
        )
    }

    fn pending(code: &str) -> (u16, Value) {
        (200, json!({ "error": code }))
    }

    fn granted() -> (u16, Value) {
        (
            200,
            json!({ "access_token": KEY, "token_type": "Bearer", "key_id": "k1",
                    "user": { "id": "u1", "username": "chad", "email": "chad@example.com" } }),
        )
    }

    /// Runs the flow against `base`; returns the result, what the user was
    /// told and the waits.
    fn run(base: &str) -> (Result<(String, Account), String>, Vec<String>, Vec<u64>) {
        let mut said = Vec::new();
        let mut waits = Vec::new();
        let result = authorize(Flow {
            web_url: base,
            device_header: Some(HEADER),
            installation_id: Some("inst-0123456789".to_string()),
            say: &mut |line| said.push(line.to_string()),
            sleep: &mut |wait| waits.push(wait.as_secs()),
            open_browser: &mut |_| false,
        });
        (result, said, waits)
    }

    #[test]
    fn pending_then_slow_down_then_the_key() {
        let (base, seen) = gateway(vec![
            start(),
            pending("authorization_pending"),
            pending("slow_down"),
            (
                429,
                json!({ "error": { "code": "slow_down", "message": "Try again later" } }),
            ),
            granted(),
        ]);
        let (result, said, waits) = run(&base);
        let (key, account) = result.unwrap();
        assert_eq!(key, KEY);
        assert_eq!(
            account,
            Account {
                username: "chad".into(),
                email: Some("chad@example.com".into())
            }
        );
        // 5 s, then 5 s more after each slow_down.
        assert_eq!(waits, vec![5, 5, 10, 15]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].path, "/api/desktop/oauth/device");
        assert_eq!(seen[0].body, json!({ "client_id": "lynshen-cli" }));
        assert!(seen[1..]
            .iter()
            .all(|s| s.path == "/api/desktop/oauth/token"));
        assert_eq!(seen[1].body["client_id"], "lynshen-cli");
        assert_eq!(seen[1].body["device_code"], "d".repeat(64));
        assert_eq!(seen[1].body["installation_id"], "inst-0123456789");
        // The computer's header on every request.
        assert!(
            seen.iter().all(|s| s.device.as_deref() == Some(HEADER)),
            "{seen:?}"
        );
        // The page and the code, never the key.
        let text = said.join("\n");
        assert!(text.contains(&format!("{base}/oauth/authorize?user_code=ABCDEF123456")));
        assert!(text.contains("ABCDEF-123456"));
        assert!(!text.contains(KEY));
    }

    #[test]
    fn a_refused_sign_in_and_an_expired_code_end_the_flow() {
        let (base, _) = gateway(vec![
            start(),
            pending("authorization_pending"),
            pending("access_denied"),
        ]);
        assert!(run(&base).0.unwrap_err().contains("refused in the browser"));
        let (base, _) = gateway(vec![start(), pending("expired_token")]);
        assert!(run(&base).0.unwrap_err().contains("expired"));
    }

    #[test]
    fn the_flow_stops_when_the_grant_runs_out_unanswered() {
        let (base, seen) = gateway(
            std::iter::once((
                200,
                json!({ "device_code": "d".repeat(64), "user_code": "ABCDEF123456", "expires_in": 20, "interval": 5 }),
            ))
            .chain(std::iter::repeat_with(|| pending("authorization_pending")).take(10))
            .collect(),
        );
        let (result, _, waits) = run(&base);
        assert!(result.unwrap_err().contains("expired"));
        assert_eq!(waits, vec![5, 5, 5, 5]);
        assert_eq!(seen.lock().unwrap().len(), 5);
    }

    #[test]
    fn a_gateway_refusal_shows_its_message() {
        let (base, _) = gateway(vec![
            start(),
            (
                403,
                json!({ "error": { "code": "device_limit_reached", "message": "设备数已满" } }),
            ),
        ]);
        assert_eq!(
            run(&base).0.unwrap_err(),
            "the gateway refused the sign-in: 设备数已满"
        );
        let (base, _) = gateway(vec![(
            400,
            json!({ "error": { "code": "invalid_client", "message": "Unknown desktop client" } }),
        )]);
        assert!(run(&base).0.unwrap_err().contains("Unknown desktop client"));
    }

    #[test]
    fn the_key_goes_to_providers_lynshen_cli() {
        let dir = std::env::temp_dir().join(format!("lynshen-device-login-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, "{\"providers\":{\"openai\":\"sk-own\"}}\n").unwrap();
        store_key_at(path.clone(), false, KEY).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["providers"]["lynshen-cli"], KEY);
        assert_eq!(saved["providers"]["openai"], "sk-own");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_installation_id_is_read_only_when_valid() {
        let dir = std::env::temp_dir().join(format!("lynshen-device-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(installation_id(&dir), None);
        std::fs::write(dir.join("installation-id"), "0123abcd-ef\n").unwrap();
        assert_eq!(installation_id(&dir).as_deref(), Some("0123abcd-ef"));
        std::fs::write(dir.join("installation-id"), "bad id!").unwrap();
        assert_eq!(installation_id(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

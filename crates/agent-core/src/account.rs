//! This computer's LynShen account as relay v2 needs it
//! (`docs/relay-protocol.md` §7): the computer's device key, relay tokens
//! minted with it, and the account's push targets. The gateway side is
//! monoize `account-computers.spec.md`.
//!
//! The device key is the one the desktop's sign-in stored
//! (`providers.monoize` in auth.json, client `lynshen-desktop`), else the
//! CLI's own (`providers.lynshen-cli`, client `lynshen-cli`). Both clients
//! of one computer hold one device seat.

use serde_json::Value;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

/// The providers in auth.json that hold a computer's device key, in order.
pub const DEVICE_KEY_PROVIDERS: [&str; 2] = ["monoize", "lynshen-cli"];

/// The gateway's web base (`lynshen_web_url`) and this computer's device
/// key, None when the computer is not signed in to the account.
pub fn computer_key() -> Option<(String, String)> {
    let config = crate::config::Config::load_existing().ok()?;
    let auth = crate::config::AuthStore::load_or_create(config.encrypt_secrets).ok()?;
    DEVICE_KEY_PROVIDERS.iter().find_map(|provider| {
        auth.key_for(provider)
            .map(str::trim)
            .filter(|key| !key.is_empty() && !key.starts_with("jcenc1:"))
            .map(|key| (config.lynshen_web_url.clone(), key.to_string()))
    })
}

/// Why a gateway call with the device key failed.
#[derive(Debug)]
pub enum AccountError {
    /// HTTP 401: the key is revoked (the computer was removed from the
    /// account, or signed out elsewhere).
    Revoked(String),
    /// Anything else: retry later.
    Failed(String),
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountError::Revoked(message) | AccountError::Failed(message) => f.write_str(message),
        }
    }
}

fn call(method: &str, path: &str) -> Result<Value, AccountError> {
    let (base, key) =
        computer_key().ok_or_else(|| AccountError::Failed("not signed in".to_string()))?;
    let mut request = ureq::AgentBuilder::new()
        .timeout(TIMEOUT)
        .redirects(0)
        .build()
        .request(method, &format!("{}{path}", base.trim_end_matches('/')))
        .set("Authorization", &format!("Bearer {key}"));
    if let Some(device) = crate::device_id::header_value() {
        request = request.set(crate::device_id::HEADER, device);
    }
    match request.call() {
        Ok(response) => response
            .into_json()
            .map_err(|error| AccountError::Failed(error.to_string())),
        Err(ureq::Error::Status(401, _)) => Err(AccountError::Revoked(
            "the gateway refused this computer's login".to_string(),
        )),
        Err(ureq::Error::Status(status, response)) => {
            let body: Value = response.into_json().unwrap_or_default();
            let message = body["error"]["message"].as_str().unwrap_or("gateway error");
            Err(AccountError::Failed(format!("{status}: {message}")))
        }
        Err(error) => Err(AccountError::Failed(error.to_string())),
    }
}

/// A relay token (10 minutes) for this computer: `(token, user)`.
pub fn relay_token() -> Result<(String, String), AccountError> {
    let reply = call("POST", "/api/desktop/oauth/relay-token")?;
    let token = reply["token"].as_str().unwrap_or_default().to_string();
    let user = reply["user"].as_str().unwrap_or_default().to_string();
    if token.is_empty() || user.is_empty() {
        return Err(AccountError::Failed(format!(
            "relay-token answered without a token: {}",
            reply["error"]
        )));
    }
    Ok((token, user))
}

/// The account's push registrations other than this computer's
/// (`[{id, provider, subscription, device_name, client}]`).
pub fn push_targets() -> Result<Vec<Value>, AccountError> {
    Ok(call("GET", "/api/desktop/oauth/push/targets")?["targets"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// Forgets a push target the push service dropped.
pub fn forget_push_target(id: &str) -> Result<(), AccountError> {
    call("DELETE", &format!("/api/desktop/oauth/push/targets/{id}")).map(|_| ())
}

/// Signs the computer out after the gateway refused `key`: the key leaves
/// auth.json (whichever provider held it), unless another sign-in replaced
/// it meanwhile.
pub fn forget_refused_key(key: &str) -> Result<(), String> {
    let config = crate::config::Config::load_existing().map_err(|error| error.to_string())?;
    let _lock = crate::oauth::lock_auth_refresh()?;
    let mut auth = crate::config::AuthStore::load_or_create(config.encrypt_secrets)
        .map_err(|error| error.to_string())?;
    let mut changed = false;
    for provider in DEVICE_KEY_PROVIDERS {
        if auth.key_for(provider).map(str::trim) == Some(key) {
            auth.remove_key(provider);
            changed = true;
        }
    }
    if changed {
        auth.save().map_err(|error| error.to_string())?;
        crate::log_warn!(
            "account",
            "the gateway refused this computer's login; signed out"
        );
    }
    Ok(())
}

/// This computer's host name, None when it cannot be read.
pub fn hostname() -> Option<String> {
    crate::oauth::hostname()
}

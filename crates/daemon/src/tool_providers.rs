//! Custom providers for Claude Code and Codex sessions: an API (an
//! Anthropic Messages one for Claude Code, a Responses one for Codex) the
//! user brings, with its own key. A session on one runs through the local
//! gateway (crate::gateway) like a LynShen gateway session, so the key never
//! reaches the engine's environment, argv or disk, but its requests go to
//! the provider exactly as configured: never with the LynShen token, device
//! or group headers, and never into the LynShen usage ledger as gateway use.
//!
//! The list (`selected`, `providers` with id, name, base_url, format, model,
//! models) is a daemon setting (`tool_providers_<tool>` in settings.json);
//! each key is in auth.json's `tool_providers` block as `<tool>:<id>`,
//! encrypted when `encrypt_secrets` is on (docs/secrets.md). Clients never
//! get a key back: a provider row says `has_key`.

use crate::store::Store;
use serde_json::{json, Value};
use std::time::Duration;

pub fn format(tool: &str) -> Result<&'static str, String> {
    match tool {
        "claude" => Ok("anthropic"),
        "codex" => Ok("responses"),
        _ => Err("custom providers are for Claude Code and Codex".into()),
    }
}

fn setting(tool: &str) -> String {
    format!("tool_providers_{tool}")
}

fn config(store: &Store, tool: &str) -> Value {
    let mut config = store.setting(&setting(tool));
    if !config.is_object() {
        config = json!({ "selected": "", "providers": [] });
    }
    if !config["providers"].is_array() {
        config["providers"] = json!([]);
    }
    config
}

/// Where keys are kept: auth.json in the daemon, a map in unit tests (which
/// never read or write the user's auth.json).
mod keys {
    #[cfg(not(test))]
    pub fn get(tool: &str, id: &str) -> Option<String> {
        lynshen_agent_core::tool_provider_key(tool, id)
    }

    #[cfg(not(test))]
    pub fn set(tool: &str, id: &str, key: Option<&str>) -> Result<(), String> {
        lynshen_agent_core::set_tool_provider_key(tool, id, key)
    }

    #[cfg(test)]
    fn map() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, String>> {
        static KEYS: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> =
            std::sync::Mutex::new(None);
        std::sync::MutexGuard::map(KEYS.lock().unwrap_or_else(|p| p.into_inner()), |keys| {
            keys.get_or_insert_with(Default::default)
        })
    }

    #[cfg(test)]
    pub fn get(tool: &str, id: &str) -> Option<String> {
        map().get(&format!("{tool}:{id}")).cloned()
    }

    #[cfg(test)]
    pub fn set(tool: &str, id: &str, key: Option<&str>) -> Result<(), String> {
        let name = format!("{tool}:{id}");
        match key {
            Some(key) => map().insert(name, key.to_string()),
            None => map().remove(&name),
        };
        Ok(())
    }
}

/// `tool_providers`: the providers as clients see them (no key, no secret).
pub fn list(store: &Store, tool: &str) -> Result<Value, String> {
    format(tool)?;
    let mut config = config(store, tool);
    if let Some(providers) = config["providers"].as_array_mut() {
        for provider in providers {
            with_models(provider);
            let id = provider["id"].as_str().unwrap_or_default().to_string();
            provider["has_key"] = json!(keys::get(tool, &id).is_some());
            if let Some(fields) = provider.as_object_mut() {
                fields.remove("key");
            }
        }
    }
    Ok(json!({ "type": "tool_providers", "tool": tool, "config": config }))
}

/// An http(s) API base URL with no credentials, query or fragment.
fn check_url(url: &str) -> Result<(), String> {
    let parsed: tungstenite::http::Uri = url.parse().map_err(|_| "invalid API URL")?;
    if !matches!(parsed.scheme_str(), Some("http" | "https"))
        || parsed.authority().is_none()
        || url.contains(['?', '#', '@'])
        || url.chars().any(char::is_whitespace)
    {
        return Err(
            "API URL must be an http(s) base URL without credentials, query or fragment".into(),
        );
    }
    Ok(())
}

/// `tool_providers` (list), `tool_provider_save`, `tool_provider_remove`,
/// `tool_provider_select`. Desktop only (`lib.rs` checks it).
pub fn handle(store: &Store, op: &Value) -> Result<Value, String> {
    static EDIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = EDIT.lock().unwrap_or_else(|p| p.into_inner());
    let tool = op["tool"].as_str().ok_or("tool is required")?;
    let protocol = format(tool)?;
    if op["op"] == "tool_providers" {
        return list(store, tool);
    }
    let mut config = config(store, tool);
    let id = op["provider"]["id"]
        .as_str()
        .or(op["provider_id"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    match op["op"].as_str() {
        Some("tool_provider_save") => {
            let provider = &op["provider"];
            if id.is_empty()
                || id.len() > 100
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err(
                    "provider ID must contain 1–100 letters, digits, hyphens or underscores".into(),
                );
            }
            let url = provider["base_url"]
                .as_str()
                .unwrap_or("")
                .trim()
                .trim_end_matches('/')
                .to_string();
            check_url(&url)?;
            if provider["format"].as_str().unwrap_or(protocol) != protocol {
                return Err(format!("{tool} requires the {protocol} format"));
            }
            // The models it serves, the first the default unless one is named.
            let mut models: Vec<String> = Vec::new();
            for model in provider["models"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(std::iter::once(&provider["model"]))
                .filter_map(|m| m.as_str().map(str::trim))
                .filter(|m| !m.is_empty())
            {
                if model.len() > 200 || model.chars().any(char::is_control) {
                    return Err(format!("invalid model ID: {model}"));
                }
                if !models.iter().any(|m| m == model) {
                    models.push(model.to_string());
                }
            }
            if models.is_empty() {
                return Err("model is required".into());
            }
            if models.len() > 500 {
                return Err("at most 500 models".into());
            }
            let model = provider["model"]
                .as_str()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or(&models[0])
                .to_string();
            // An empty key keeps the saved one (an edit).
            let typed = provider["key"].as_str().unwrap_or("").trim();
            if typed.contains(['\r', '\n']) {
                return Err("a valid API key is required".into());
            }
            if typed.is_empty() && keys::get(tool, &id).is_none() {
                return Err("a valid API key is required".into());
            }
            let name = provider["name"]
                .as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(&id);
            let entry = json!({
                "id": id, "name": name, "base_url": url, "format": protocol,
                "model": model, "models": models,
            });
            if !typed.is_empty() {
                keys::set(tool, &id, Some(typed))?;
            }
            let providers = config["providers"].as_array_mut().unwrap();
            providers.retain(|p| p["id"] != id.as_str());
            providers.push(entry);
        }
        Some("tool_provider_remove") => {
            if let Some(providers) = config["providers"].as_array_mut() {
                providers.retain(|p| p["id"] != id.as_str());
            }
            if config["selected"] == id.as_str() {
                config["selected"] = json!("");
            }
            keys::set(tool, &id, None)?;
        }
        Some("tool_provider_select") => {
            if !id.is_empty()
                && !config["providers"]
                    .as_array()
                    .is_some_and(|ps| ps.iter().any(|p| p["id"] == id.as_str()))
            {
                return Err("unknown provider for this tool".into());
            }
            config["selected"] = json!(id);
        }
        _ => return Err("unknown tool provider operation".into()),
    }
    store
        .set_setting(&setting(tool), config)
        .map_err(|e| e.to_string())?;
    list(store, tool)
}

/// The provider `tool`'s new sessions start on (None: the tool's own
/// configuration).
pub fn selected(store: &Store, tool: &str) -> Option<Value> {
    let config = config(store, tool);
    let id = config["selected"].as_str().filter(|id| !id.is_empty())?;
    find(store, tool, id)
}

/// `tool`'s provider `id`, without its key.
pub fn find(store: &Store, tool: &str, id: &str) -> Option<Value> {
    let mut provider = config(store, tool)["providers"]
        .as_array()?
        .iter()
        .find(|p| p["id"] == id)
        .cloned()?;
    with_models(&mut provider);
    Some(provider)
}

/// The key of `tool`'s provider `id`, for the local gateway only.
pub fn key(tool: &str, id: &str) -> Option<String> {
    keys::get(tool, id)
}

/// A provider saved with one `model` (before `models`) serves that one.
fn with_models(provider: &mut Value) {
    let listed = provider["models"]
        .as_array()
        .is_some_and(|models| !models.is_empty());
    if !listed {
        provider["models"] = json!([provider["model"].as_str().unwrap_or_default()]);
    }
}

/// What clients are told about the provider a session runs on: never its
/// key or URL.
pub fn view(provider: &Value) -> Value {
    json!({
        "id": provider["id"],
        "name": provider["name"],
        "model": provider["model"],
        "models": provider["models"],
    })
}

/// The URL of `base`'s model list: the base with its own `/v1`, or `/v1`
/// added, as the local gateway sends `/v1/messages` and `/v1/responses`.
fn models_url(base: &str) -> String {
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

/// `tool_provider_models`: the models an API serves, from its model list
/// (`GET …/models`, in the OpenAI or Anthropic shape), for the form that
/// adds or edits a provider. The key is the one typed, else the saved one of
/// `provider_id`. Models the list says take other protocols than `tool`'s
/// are counted (`hidden`), not listed. The request carries the key and
/// nothing of LynShen's.
pub fn models(store: &Store, op: &Value) -> Result<Value, String> {
    let tool = op["tool"].as_str().ok_or("tool is required")?;
    let protocol = format(tool)?;
    let base = op["base_url"]
        .as_str()
        .unwrap_or("")
        .trim()
        .trim_end_matches('/');
    check_url(base)?;
    let typed = op["key"].as_str().unwrap_or("").trim().to_string();
    let key = if typed.is_empty() {
        op["provider_id"]
            .as_str()
            .filter(|id| find(store, tool, id).is_some())
            .and_then(|id| keys::get(tool, id))
            .unwrap_or_default()
    } else {
        typed
    };
    if key.is_empty() || key.contains(['\r', '\n']) {
        return Err("a valid API key is required".into());
    }
    let url = models_url(base);
    let mut builder = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .redirects(0);
    if let Some(proxy) =
        lynshen_agent_core::env_proxy(&url).and_then(|proxy| ureq::Proxy::new(proxy).ok())
    {
        builder = builder.proxy(proxy);
    }
    let agent = builder.build();
    let get = |page: Option<&str>, bearer: bool| {
        let mut call = agent.get(&url);
        if let Some(after) = page {
            call = call.query("limit", "1000").query("after_id", after);
        }
        if protocol == "anthropic" && !bearer {
            call = call
                .set("x-api-key", &key)
                .set("anthropic-version", "2023-06-01");
        } else {
            call = call.set("Authorization", &format!("Bearer {key}"));
        }
        match call.call() {
            Ok(response) => {
                let mut text = String::new();
                std::io::Read::read_to_string(
                    &mut std::io::Read::take(response.into_reader(), 8 << 20),
                    &mut text,
                )
                .map_err(|e| (0, format!("cannot read the model list: {e}")))?;
                serde_json::from_str::<Value>(&text)
                    .map_err(|e| (0, format!("the model list is not JSON: {e}")))
            }
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err((
                    status,
                    format!(
                        "HTTP {status}: {}",
                        body.chars().take(300).collect::<String>()
                    ),
                ))
            }
            Err(ureq::Error::Transport(error)) => Err((0, format!("cannot reach {url}: {error}"))),
        }
    };
    // Anthropic-format APIs take x-api-key; many relays only a bearer token.
    let mut bearer = protocol != "anthropic";
    let mut page = match get(None, bearer) {
        Err((401 | 403, _)) if !bearer => {
            bearer = true;
            get(None, true)
        }
        other => other,
    }
    .map_err(|(_, message)| message)?;
    let mut listed: Vec<Value> = Vec::new();
    let mut hidden = 0;
    // Anthropic's list comes in pages.
    for _ in 0..20 {
        for model in page["data"].as_array().into_iter().flatten() {
            let Some(id) = model["id"].as_str().filter(|id| !id.trim().is_empty()) else {
                continue;
            };
            if !serves(model, protocol) {
                hidden += 1;
                continue;
            }
            if listed.iter().any(|m| m["id"] == id) {
                continue;
            }
            let name = model["display_name"]
                .as_str()
                .or(model["name"].as_str())
                .filter(|name| *name != id);
            let window = model["context_length"]
                .as_u64()
                .or(model["context_window"].as_u64())
                .or(model["max_input_tokens"].as_u64());
            listed.push(json!({ "id": id, "name": name, "context_window": window }));
        }
        let next = page["last_id"]
            .as_str()
            .filter(|_| page["has_more"] == true);
        let Some(after) = next.map(str::to_string) else {
            break;
        };
        page = get(Some(&after), bearer).map_err(|(_, message)| message)?;
    }
    if page["data"].as_array().is_none() && listed.is_empty() {
        return Err(format!("{url} did not return a model list"));
    }
    Ok(json!({ "type": "tool_provider_models", "tool": tool, "models": listed, "hidden": hidden }))
}

/// Whether a listed model takes `protocol`, when the list says which
/// endpoints it serves (`supported_endpoints`: paths; new-api's
/// `supported_endpoint_types`: names). Unsaid: assumed.
fn serves(model: &Value, protocol: &str) -> bool {
    let named = |key: &str| {
        model[key].as_array().map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
    };
    if let Some(paths) = named("supported_endpoints") {
        let suffix = if protocol == "anthropic" {
            "/messages"
        } else {
            "/responses"
        };
        return paths
            .iter()
            .any(|p| p.trim_end_matches('/').ends_with(suffix));
    }
    if let Some(types) = named("supported_endpoint_types") {
        let wanted = if protocol == "anthropic" {
            "anthropic"
        } else {
            "openai-response"
        };
        return types.iter().any(|t| t == wanted);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_belong_to_their_tool_and_keys_never_reach_a_client() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-provider-test-{}-{}",
            std::process::id(),
            crate::store::now()
        ));
        let store = Store::open(dir.clone()).unwrap();
        let mut op = json!({"op":"tool_provider_save","tool":"claude","provider":{"id":"custom","name":"My API","base_url":"https://example.com/v1/","format":"anthropic","model":"claude-test","key":"secret-1"}});
        let reply = handle(&store, &op).unwrap();
        assert_eq!(reply["config"]["providers"][0]["has_key"], true);
        assert_eq!(
            reply["config"]["providers"][0]["base_url"],
            "https://example.com/v1"
        );
        assert!(reply["config"]["providers"][0].get("key").is_none());
        assert!(!reply.to_string().contains("secret-1"));
        // The key is not in the daemon's settings either.
        let settings = std::fs::read_to_string(dir.join("settings.json")).unwrap();
        assert!(!settings.contains("secret-1"), "{settings}");
        assert_eq!(key("claude", "custom").as_deref(), Some("secret-1"));
        assert_eq!(
            list(&store, "codex").unwrap()["config"]["providers"],
            json!([])
        );
        assert_eq!(
            reply["config"]["providers"][0]["models"],
            json!(["claude-test"])
        );
        // An edit with no key keeps the saved one.
        op["provider"]["key"] = json!("");
        op["provider"]["model"] = json!("new-model");
        op["provider"]["models"] = json!([" a ", "new-model", "a", ""]);
        let reply = handle(&store, &op).unwrap();
        assert_eq!(
            reply["config"]["providers"][0]["models"],
            json!(["a", "new-model"])
        );
        assert_eq!(reply["config"]["providers"][0]["model"], "new-model");
        assert_eq!(key("claude", "custom").as_deref(), Some("secret-1"));
        assert_eq!(
            view(&find(&store, "claude", "custom").unwrap()),
            json!({"id":"custom","name":"My API","model":"new-model","models":["a","new-model"]})
        );
        assert!(handle(
            &store,
            &json!({"op":"tool_provider_select","tool":"codex","provider_id":"custom"})
        )
        .is_err());
        handle(
            &store,
            &json!({"op":"tool_provider_select","tool":"claude","provider_id":"custom"}),
        )
        .unwrap();
        assert_eq!(selected(&store, "claude").unwrap()["id"], "custom");
        handle(
            &store,
            &json!({"op":"tool_provider_remove","tool":"claude","provider_id":"custom"}),
        )
        .unwrap();
        assert_eq!(list(&store, "claude").unwrap()["config"]["selected"], "");
        assert_eq!(key("claude", "custom"), None);
        // A new provider needs a key; Claude Code takes the anthropic format.
        op["provider"]["key"] = json!("");
        assert!(handle(&store, &op).is_err());
        op["provider"]["key"] = json!("secret-2");
        op["provider"]["format"] = json!("responses");
        assert!(handle(&store, &op).is_err());
        op["provider"]["format"] = json!("anthropic");
        for url in [
            "file:///etc/passwd",
            "https://user:pass@example.com",
            "https://example.com?token=secret",
            "https://example.com#frag",
            "invalid",
        ] {
            op["provider"]["base_url"] = json!(url);
            assert!(handle(&store, &op).is_err(), "{url}");
        }
        assert!(handle(
            &store,
            &json!({"op":"tool_provider_save","tool":"acp","provider":{}})
        )
        .is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("settings.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn listed_models_keep_the_ones_that_take_the_tool_s_protocol() {
        let claude = json!({"id":"c","supported_endpoints":["/messages"]});
        let gpt = json!({"id":"g","supported_endpoints":["/chat/completions","/responses"]});
        let relay = json!({"id":"r","supported_endpoint_types":["anthropic","openai"]});
        let plain = json!({"id":"p"});
        assert!(serves(&claude, "anthropic") && !serves(&claude, "responses"));
        assert!(!serves(&gpt, "anthropic") && serves(&gpt, "responses"));
        assert!(serves(&relay, "anthropic") && !serves(&relay, "responses"));
        assert!(serves(&plain, "anthropic") && serves(&plain, "responses"));
        assert_eq!(models_url("https://x/v1"), "https://x/v1/models");
        assert_eq!(models_url("https://x/api"), "https://x/api/v1/models");
    }
}

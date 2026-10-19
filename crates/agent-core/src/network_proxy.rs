//! The proxy the user set for the agent's own network use (Aogiri ticket
//! #50): `network_proxy` in config.json, an HTTP proxy such as
//! `http://127.0.0.1:7890` (empty or absent: none). It applies to the web
//! tools' own requests (web_fetch, the local search fallback; `web_fetch.rs`)
//! and to shell commands (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and
//! `NO_PROXY` for the loopback, `shell.rs`). Model requests are not affected.
//!
//! Desktop changes it while engines run, so it is read from config.json
//! when used, at most once a second.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What `NO_PROXY` holds for commands: this machine.
pub const NO_PROXY: &str = "localhost,127.0.0.1,::1";

/// `value` as a proxy URL ureq takes, or None when it is not one: an
/// `http://` (or bare `host:port`, read as http) URL with a host and a port.
pub fn parse(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let rest = match value.split_once("://") {
        Some(("http", rest)) => rest,
        Some(_) => return None,
        None => value,
    };
    let authority = rest.trim_end_matches('/');
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let (host, port) = host_port.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() || port == 0 || authority.contains('/') {
        return None;
    }
    Some(format!("http://{authority}"))
}

/// The proxy config.json names now, if any.
pub fn configured() -> Option<String> {
    // Unit tests never read this machine's config.json.
    if cfg!(test) {
        return None;
    }
    static CACHE: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, value)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(1) {
            return value.clone();
        }
    }
    let value = crate::config::lynshen_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|config| config.get("network_proxy")?.as_str().and_then(parse));
    *cache = Some((Instant::now(), value.clone()));
    value
}

/// The variables a command runs with for `proxy`.
pub fn command_env(proxy: &str) -> Vec<(&'static str, String)> {
    let mut vars = Vec::new();
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        vars.push((name, proxy.to_string()));
        vars.push((lower(name), proxy.to_string()));
    }
    vars.push(("NO_PROXY", NO_PROXY.to_string()));
    vars.push(("no_proxy", NO_PROXY.to_string()));
    vars
}

fn lower(name: &'static str) -> &'static str {
    match name {
        "HTTP_PROXY" => "http_proxy",
        "HTTPS_PROXY" => "https_proxy",
        _ => "all_proxy",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_http_proxy_and_refuses_the_rest() {
        assert_eq!(
            parse("http://127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            parse(" 127.0.0.1:7890 ").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            parse("http://user:pw@proxy.lan:8080/").as_deref(),
            Some("http://user:pw@proxy.lan:8080")
        );
        assert_eq!(parse(""), None);
        assert_eq!(parse("socks5://127.0.0.1:7891"), None);
        assert_eq!(parse("127.0.0.1"), None);
        assert_eq!(parse("http://127.0.0.1:0"), None);
        assert_eq!(parse("http://h:1/path"), None);
    }

    #[test]
    fn commands_get_both_spellings_and_no_proxy_for_this_machine() {
        let env = command_env("http://127.0.0.1:7890");
        for name in ["HTTP_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
            assert!(
                env.iter()
                    .any(|(k, v)| *k == name && v == "http://127.0.0.1:7890"),
                "{name}"
            );
        }
        assert!(env.iter().any(|(k, v)| *k == "NO_PROXY" && v == NO_PROXY));
    }
}

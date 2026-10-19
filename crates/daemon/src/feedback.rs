//! A bug report or suggestion sent from the phone page (Aogiri ticket #45):
//! the phone has no LynShen device key, so it asks the computer it is paired
//! with, which sends the ticket with the desktop's device key and hardware
//! header, as Aogiri's own 反馈问题 does (`POST /api/desktop/feedback`). With
//! the phone's consent the end of this computer's logs goes along, secrets
//! and user names taken out, gzipped.

use serde_json::{json, Value};
use std::{io::Write, time::Duration};

/// The ticket fields the gateway takes from a client (its FT-7 limits).
const TITLE_MAX: usize = 200;
const BODY_MAX: usize = 10_000;
const PHOTOS_MAX: usize = 2;
const PHOTO_MAX_BYTES: usize = 3 * 1024 * 1024;
/// The end of each log that goes along.
const LOG_TAIL: u64 = 256 * 1024;

/// `feedback_send`: `title`, `body`, `category` (`bug` | `feature`),
/// `app_version`, `os`, `attachments` (`{name, type, data_url}`, images
/// only), `with_logs`. Answers `{type: "feedback_sent", ticket_no}`.
pub fn send(op: &Value) -> Result<Value, String> {
    let ticket = ticket(op)?;
    let (base, key) = lynshen_agent_core::desktop_gateway_key()?;
    let mut request = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .redirects(0)
        .build()
        .post(&format!(
            "{}/api/desktop/feedback",
            base.trim_end_matches('/')
        ))
        .set("Authorization", &format!("Bearer {key}"));
    if let Some(device) = lynshen_agent_core::device_id::header_value() {
        request = request.set(lynshen_agent_core::device_id::HEADER, device);
    }
    let reply: Value = match request.send_json(ticket) {
        Ok(response) => response.into_json().map_err(|error| error.to_string())?,
        Err(ureq::Error::Status(status, response)) => {
            let body: Value = response.into_json().unwrap_or_default();
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("the gateway refused the report");
            return Err(format!("{status}: {message}"));
        }
        Err(error) => return Err(error.to_string()),
    };
    Ok(json!({ "type": "feedback_sent", "ticket_no": reply["ticket_no"] }))
}

/// The ticket as the gateway takes it, from the phone's op; the logs added
/// here when asked for.
fn ticket(op: &Value) -> Result<Value, String> {
    let text = |key: &str| op[key].as_str().unwrap_or_default().trim().to_string();
    let title: String = text("title").chars().take(TITLE_MAX).collect();
    let body: String = text("body").chars().take(BODY_MAX).collect();
    if title.is_empty() || body.is_empty() {
        return Err("a report needs a title and a description".to_string());
    }
    let category = if op["category"] == "feature" {
        "feature"
    } else {
        "bug"
    };
    let mut attachments: Vec<Value> = op["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            let url = item["data_url"].as_str().unwrap_or_default();
            url.starts_with("data:image/") && url.len() <= PHOTO_MAX_BYTES * 4 / 3 + 64
        })
        .take(PHOTOS_MAX)
        .map(|item| {
            json!({
                "name": item["name"].as_str().unwrap_or("photo").chars().take(120).collect::<String>(),
                "type": item["type"].as_str().unwrap_or("image/png"),
                "data_url": item["data_url"],
            })
        })
        .collect();
    if op["with_logs"] == true {
        if let Some(logs) = logs_attachment() {
            attachments.push(logs);
        }
    }
    let short = |key: &str| text(key).chars().take(64).collect::<String>();
    Ok(json!({
        "title": title,
        "body": body,
        "category": category,
        "app_version": short("app_version"),
        "os": short("os"),
        "attachments": attachments,
    }))
}

/// The end of the engine's and the daemon's logs, redacted and gzipped, as
/// Aogiri attaches them (`lynshen-logs.txt.gz`). None when there are none.
fn logs_attachment() -> Option<Value> {
    use base64::Engine as _;
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let dir = std::path::PathBuf::from(home).join(".lynshen");
    let mut bundle = String::new();
    for (name, path) in [
        ("lynshen.log", dir.join("logs").join("lynshen.log")),
        ("daemon.log", dir.join("daemon").join("daemon.log")),
        ("desktop-ui.log", dir.join("logs").join("desktop-ui.log")),
    ] {
        let Some(text) = tail(&path) else { continue };
        bundle.push_str(&format!("===== {name} =====\n{}\n\n", redact(&text)));
    }
    if bundle.is_empty() {
        return None;
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(bundle.as_bytes()).ok()?;
    let bytes = gz.finish().ok()?;
    Some(json!({
        "name": "lynshen-logs.txt.gz",
        "type": "application/gzip",
        "data_url": format!("data:application/gzip;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)),
    }))
}

fn tail(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(LOG_TAIL)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `text` with what looks like a secret, and the user's name in home
/// paths, taken out (the patterns of Aogiri's `feedback.ts`).
fn redact(text: &str) -> String {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            (r"(?i)\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{8,}", "$1 [redacted]"),
            (r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", "[redacted-jwt]"),
            (r"\b(sk|jcp|ghp|gho|xox[abp]|pk)[-_][A-Za-z0-9_-]{12,}", "[redacted-key]"),
            (
                r#"(?i)("?(?:access_token|refresh_token|id_token|api_key|apikey|api-key|authorization|password|secret|token|key)"?\s*[:=]\s*)("[^"]*"|[^\s,&}]+)"#,
                "${1}[redacted]",
            ),
            (r#"(/Users/|/home/)[^/\s"']+"#, "${1}~"),
            (r#"([A-Za-z]:\\\\?Users\\\\?)[^\\\s"']+"#, "${1}~"),
        ]
        .into_iter()
        .map(|(pattern, to)| (regex::Regex::new(pattern).expect("valid pattern"), to))
        .collect()
    });
    patterns
        .iter()
        .fold(text.to_string(), |out, (pattern, to)| {
            pattern.replace_all(&out, *to).into_owned()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_keeps_what_the_gateway_takes_and_only_images() {
        let op = json!({
            "op": "feedback_send",
            "title": "  web 端想提工单  ",
            "body": "在 app.lynshen.org 上提交",
            "category": "feature",
            "app_version": "web",
            "os": "iPad",
            "attachments": [
                { "name": "a.png", "type": "image/png", "data_url": "data:image/png;base64,AAAA" },
                { "name": "x.sh", "type": "text/plain", "data_url": "data:text/plain;base64,AAAA" },
                { "name": "b.png", "type": "image/png", "data_url": "data:image/png;base64,BBBB" },
                { "name": "c.png", "type": "image/png", "data_url": "data:image/png;base64,CCCC" }
            ],
            "with_logs": false
        });
        let made = ticket(&op).unwrap();
        assert_eq!(made["title"], "web 端想提工单");
        assert_eq!(made["category"], "feature");
        let names: Vec<&str> = made["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["a.png", "b.png"]);
        assert!(ticket(&json!({ "title": "t", "body": " " })).is_err());
        assert_eq!(
            ticket(&json!({ "title": "t", "body": "b", "category": "x" })).unwrap()["category"],
            "bug"
        );
    }

    #[test]
    fn logs_lose_secrets_and_user_names() {
        let line = r#"GET with Authorization: Bearer abcdefgh12345678 key="sk-live-0123456789abcd" at /Users/alice/code C:\Users\bob\x"#;
        let out = redact(line);
        assert!(!out.contains("abcdefgh12345678"), "{out}");
        assert!(!out.contains("sk-live-0123456789abcd"), "{out}");
        assert!(!out.contains("alice") && !out.contains("bob"), "{out}");
    }
}

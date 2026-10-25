//! The app's own tools, which the daemon gives the sessions it hosts (see
//! the daemon's `app_tools`): `present_files` (the task's deliverables, shown
//! as cards), `propose_schedule` (a scheduled task the user creates from a
//! card, or not) and the tools for the user's other conversations
//! (`list_sessions`, `read_session`, `start_session`, `send_to_session`,
//! `wait_session`). This module holds what the model sees of them and runs
//! the first two; the daemon runs the session tools. Their output is JSON
//! the clients draw cards from, so a replayed conversation shows the same
//! cards.

use serde_json::{json, Value};
use std::path::Path;

pub const PRESENT_FILES: &str = "present_files";
pub const PROPOSE_SCHEDULE: &str = "propose_schedule";
pub const LIST_SESSIONS: &str = "list_sessions";
pub const READ_SESSION: &str = "read_session";
pub const START_SESSION: &str = "start_session";
pub const SEND_TO_SESSION: &str = "send_to_session";
pub const WAIT_SESSION: &str = "wait_session";
/// The tools for the user's other conversations, in the order offered.
pub const SESSION_TOOLS: [&str; 5] = [
    LIST_SESSIONS,
    READ_SESSION,
    START_SESSION,
    SEND_TO_SESSION,
    WAIT_SESSION,
];
/// Every app tool.
pub const NAMES: [&str; 7] = [
    PRESENT_FILES,
    PROPOSE_SCHEDULE,
    LIST_SESSIONS,
    READ_SESSION,
    START_SESSION,
    SEND_TO_SESSION,
    WAIT_SESSION,
];

/// Files one call may present.
const MAX_FILES: usize = 12;

/// The definitions of `names` (app tools, in the order given), shaped as the
/// built-in function tools.
pub fn definitions(names: &[&str]) -> Vec<Value> {
    names.iter().filter_map(|name| definition(name)).collect()
}

fn definition(name: &str) -> Option<Value> {
    let (description, parameters) = match name {
        PRESENT_FILES => (
            "Show the user the deliverables you made (report, slides, image, video, page, build) as cards. Once, at the end, most important first; not edited source or logs.",
            json!({
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "description": { "type": "string" }
                            },
                            "required": ["path"]
                        }
                    }
                },
                "required": ["files"]
            }),
        ),
        PROPOSE_SCHEDULE => (
            "Propose a task run at set local times in this project. The user decides from a card; nothing runs before. The prompt must stand alone.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "prompt": { "type": "string" },
                    "repeat": { "type": "string", "enum": ["once", "hourly", "daily", "weekdays", "weekly"] },
                    "time": { "type": "string", "description": "HH:MM" },
                    "days": { "type": "array", "items": { "type": "integer" }, "description": "weekly; 0=Sunday" },
                    "date": { "type": "string", "description": "once; YYYY-MM-DD" }
                },
                "required": ["name", "prompt", "repeat", "time"]
            }),
        ),
        LIST_SESSIONS => (
            "List the user's other conversations in this app: id, title, project directory, engine, state (idle, busy, closed), last activity.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Filter by title or directory." }
                },
                "additionalProperties": false
            }),
        ),
        READ_SESSION => (
            "Read another conversation's latest requests and replies (no tool details).",
            json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "turns": { "type": "integer", "minimum": 1, "maximum": 10, "description": "Default 3." }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
        ),
        START_SESSION => (
            "Start a helper conversation in this project, on another engine or model, and send it a task (it arrives as its first message): e.g. a review on Codex, a check on Claude Code, a second opinion on another model. It runs in this conversation's approval mode or a stricter one, and shows in the user's list. With wait_reply, wait (up to 10 minutes) for its reply.",
            json!({
                "type": "object",
                "properties": {
                    "engine": { "type": "string", "enum": ["lynshen", "claude", "codex"], "description": "Default: lynshen." },
                    "model": { "type": "string", "description": "The engine's model name; default: the engine's own." },
                    "effort": { "type": "string", "description": "Reasoning effort the engine offers (e.g. low, medium, high, xhigh, max)." },
                    "approval_mode": { "type": "string", "enum": ["plan", "manual", "auto-edit", "auto"], "description": "Default: this conversation's." },
                    "title": { "type": "string" },
                    "message": { "type": "string", "description": "Self-contained: the helper does not see this conversation." },
                    "wait_reply": { "type": "boolean" }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        ),
        SEND_TO_SESSION => (
            "Send a message to another conversation's AI; it arrives there as its next message. With wait_reply, wait (up to 10 minutes) and get its reply.",
            json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "message": { "type": "string", "description": "Self-contained: the receiver does not see this conversation." },
                    "wait_reply": { "type": "boolean" }
                },
                "required": ["session", "message"],
                "additionalProperties": false
            }),
        ),
        WAIT_SESSION => (
            "Wait for a conversation's reply to your last message. Returns early if it awaits the user's approval.",
            json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "timeout_seconds": { "type": "integer", "description": "Default 600, max 1800" }
                },
                "required": ["session"]
            }),
        ),
        _ => return None,
    };
    Some(json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": parameters,
    }))
}

/// Runs `present_files` or `propose_schedule` for a session in `cwd`: Ok
/// with the JSON output, Err with what the model should fix. None for any
/// other tool.
pub fn run(name: &str, args: &Value, cwd: &Path) -> Option<Result<Value, String>> {
    match name {
        PRESENT_FILES => Some(present_files(args, cwd)),
        PROPOSE_SCHEDULE => Some(propose_schedule(args)),
        _ => None,
    }
}

fn text<'a>(args: &'a Value, key: &str) -> &'a str {
    args[key].as_str().map(str::trim).unwrap_or_default()
}

/// What a client shows a presented file as, by its extension: `image` and
/// `video` play inline (the daemon's `fs_image` reads them), anything else
/// is a `file` card.
pub fn media_kind(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => "image",
        "mp4" | "m4v" | "webm" | "mov" => "video",
        _ => "file",
    }
}

fn present_files(args: &Value, cwd: &Path) -> Result<Value, String> {
    let entries = args["files"]
        .as_array()
        .filter(|files| !files.is_empty())
        .ok_or("files must list at least one file")?;
    if entries.len() > MAX_FILES {
        return Err(format!(
            "present at most {MAX_FILES} files: the deliverables, not everything you touched"
        ));
    }
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for entry in entries {
        // A bare path is accepted too.
        let (path, description) = match entry {
            Value::String(path) => (path.trim(), ""),
            entry => (text(entry, "path"), text(entry, "description")),
        };
        if path.is_empty() {
            continue;
        }
        let resolved = crate::tools::resolve_path(cwd, path);
        match std::fs::metadata(&resolved) {
            Ok(meta) if meta.is_file() => files.push(json!({
                "path": resolved.display().to_string(),
                "description": description,
                "size": meta.len(),
                "kind": media_kind(&resolved),
            })),
            _ => missing.push(path.to_string()),
        }
    }
    if files.is_empty() {
        return Err(format!("no such file: {}", missing.join(", ")));
    }
    let mut output = json!({ "files": files });
    if !missing.is_empty() {
        output["missing"] = json!(missing);
    }
    Ok(output)
}

fn propose_schedule(args: &Value) -> Result<Value, String> {
    let (name, prompt) = (text(args, "name"), text(args, "prompt"));
    let (repeat, time) = (text(args, "repeat"), text(args, "time"));
    if name.is_empty() || prompt.is_empty() {
        return Err("name and prompt must not be empty".to_string());
    }
    if !matches!(repeat, "once" | "hourly" | "daily" | "weekdays" | "weekly") {
        return Err("repeat must be once, hourly, daily, weekdays or weekly".to_string());
    }
    let clock = time.split_once(':').and_then(|(h, m)| {
        let (h, m) = (h.parse::<u8>().ok()?, m.parse::<u8>().ok()?);
        (h < 24 && m < 60 && time.len() == 5).then_some(())
    });
    if clock.is_none() {
        return Err(format!("time must be HH:MM (24-hour), got '{time}'"));
    }
    let mut proposal = json!({
        "id": proposal_id(),
        "name": name,
        "prompt": prompt,
        "repeat": repeat,
        "time": time,
    });
    if repeat == "weekly" {
        let mut days: Vec<u64> = args["days"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .filter(|day| *day <= 6)
            .collect();
        days.sort_unstable();
        days.dedup();
        if days.is_empty() {
            return Err("a weekly task needs days (0 is Sunday … 6 is Saturday)".to_string());
        }
        proposal["days"] = json!(days);
    }
    if repeat == "once" {
        let date = text(args, "date");
        let valid = date.len() == 10
            && date.char_indices().all(|(i, c)| match i {
                4 | 7 => c == '-',
                _ => c.is_ascii_digit(),
            });
        if !valid {
            return Err("a one-off task needs date as YYYY-MM-DD".to_string());
        }
        proposal["date"] = json!(date);
    }
    proposal["note"] =
        json!("Shown to the user as a card; nothing is scheduled until they create it.");
    Ok(proposal)
}

/// Ties the card to the task created from it (the schedule's `origin`).
fn proposal_id() -> String {
    let mut bytes = [0u8; 6];
    let _ = getrandom::getrandom(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("prop-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_app_tool_has_a_definition() {
        let defined = definitions(&NAMES);
        let names: Vec<&str> = defined
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, NAMES);
        assert!(definitions(&["bash"]).is_empty());
        let start = &definitions(&[START_SESSION])[0]["parameters"]["properties"];
        assert_eq!(
            start["engine"]["enum"],
            json!(["lynshen", "claude", "codex"])
        );
    }

    #[test]
    fn present_files_keeps_existing_files_and_names_missing_ones() {
        let dir = std::env::temp_dir().join(format!("lynshen-present-{}", proposal_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("out.html"), "<p>").unwrap();
        std::fs::write(dir.join("demo.MP4"), "v").unwrap();
        let output = run(
            PRESENT_FILES,
            &json!({ "files": [{ "path": "out.html", "description": "page" }, { "path": "gone.mp4" }, "demo.MP4"] }),
            &dir,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            output["files"][0]["path"],
            dir.join("out.html").display().to_string()
        );
        assert_eq!(output["files"][0]["description"], "page");
        assert_eq!(output["files"][0]["size"], 3);
        assert_eq!(output["files"][0]["kind"], "file");
        assert_eq!(output["files"][1]["kind"], "video");
        assert_eq!(output["missing"], json!(["gone.mp4"]));
        let none = run(PRESENT_FILES, &json!({ "files": ["gone.mp4"] }), &dir).unwrap();
        assert!(none.unwrap_err().contains("gone.mp4"));
        let many: Vec<String> = (0..13).map(|i| format!("f{i}")).collect();
        assert!(run(PRESENT_FILES, &json!({ "files": many }), &dir)
            .unwrap()
            .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn propose_schedule_checks_when_it_runs() {
        let ok = propose_schedule(&json!({ "name": "Tests", "prompt": "run tests", "repeat": "weekly", "time": "09:30", "days": [5, 1, 5] })).unwrap();
        assert!(ok["id"].as_str().unwrap().starts_with("prop-"));
        assert_eq!(ok["days"], json!([1, 5]));
        let once = propose_schedule(&json!({ "name": "a", "prompt": "b", "repeat": "once", "time": "07:05", "date": "2026-11-02" })).unwrap();
        assert_eq!(once["date"], "2026-11-02");
        for bad in [
            json!({ "name": "a", "prompt": "b", "repeat": "daily", "time": "9:30" }),
            json!({ "name": "a", "prompt": "b", "repeat": "daily", "time": "24:00" }),
            json!({ "name": "a", "prompt": "b", "repeat": "monthly", "time": "09:00" }),
            json!({ "name": "a", "prompt": "b", "repeat": "weekly", "time": "09:00" }),
            json!({ "name": "a", "prompt": "b", "repeat": "once", "time": "09:00", "date": "tomorrow" }),
            json!({ "name": "", "prompt": "b", "repeat": "daily", "time": "09:00" }),
        ] {
            assert!(propose_schedule(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn presented_media_is_told_by_its_extension() {
        assert_eq!(media_kind(Path::new("a.PNG")), "image");
        assert_eq!(media_kind(Path::new("clip.webm")), "video");
        assert_eq!(media_kind(Path::new("deck.pptx")), "file");
        assert_eq!(media_kind(Path::new("README")), "file");
    }
}

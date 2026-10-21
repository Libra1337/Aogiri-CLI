use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 60;
const MAX_IMAGE_READ_BYTES: u64 = 1024 * 1024;
const LARGE_TEXT_READ_SOFT_BYTES: u64 = 256 * 1024;
const LARGE_RIPGREP_OUTPUT_SOFT_LINES: usize = 200;
const LARGE_RIPGREP_OUTPUT_SOFT_BYTES: usize = 128 * 1024;
const COMMAND_OUTPUT_MAX_LINES: usize = 3_000;
const COMMAND_OUTPUT_MAX_BYTES: usize = 128 * 1024;
const COMMAND_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
const HASHLINE_ALPHABET: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";
const READ_MODEL_CONTENT_OMIT_THRESHOLD: usize = 1024;
const READ_MODEL_HASHLINES_LIMIT: usize = 8 * 1024;
const DIFF_MODEL_OUTPUT_INLINE_LIMIT: usize = 8 * 1024;
const MODEL_OUTPUT_INLINE_LIMIT: usize = 16 * 1024;
const MODEL_OUTPUT_FIELD_LIMIT: usize = 4 * 1024;

pub struct ToolExecutionResult {
    pub output: String,
    pub model_output: String,
    pub is_error: bool,
}

pub enum ToolExecutionEvent {
    Update(String),
}

/// Built-in tool definitions. Descriptions say only what a call cannot show:
/// the parameter names and types carry the rest. `exec_command`, `execute`
/// and `shell_command` are accepted as aliases of `bash` but not offered.
pub fn definitions() -> Vec<Value> {
    with_function_tool_defaults(vec![
        json!({
            "type": "function",
            "name": "read",
            "description": "Read a file: text with LINE#HASH line anchors, an image, or binary metadata. Use offset and limit for large files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "number", "description": "1-indexed start line." },
                    "limit": { "type": "number", "description": "Max lines." }
                },
                "required": ["path"]
            }
        }),
        json!({
            "type": "function",
            "name": "str_replace",
            "description": "Replace exact text in a file you have read. Each oldText must match exactly once in the file before the call; put all edits to one file in one call.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": { "type": "string" },
                                "newText": { "type": "string" }
                            },
                            "required": ["oldText", "newText"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }
        }),
        json!({
            "type": "function",
            "name": "hashline_edit",
            "description": "Edit a file by the LINE#HASH anchors of your latest read of it. Put all edits to one file in one call.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": { "type": "string", "enum": ["replace", "append", "prepend"], "description": "replace pos..end, append after pos, prepend before pos. Without pos, append/prepend go to the end/start of the file." },
                                "pos": { "type": "string", "description": "LINE#HASH anchor; required for replace." },
                                "end": { "type": "string", "description": "Last LINE#HASH of a replaced range." },
                                "lines": { "type": "array", "items": { "type": "string" }, "description": "New lines, without anchors or diff markers." }
                            },
                            "required": ["op", "lines"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }
        }),
        json!({
            "type": "function",
            "name": "write",
            "description": "Write a whole file. A new file needs no read; read an existing file before overwriting it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "type": "function",
            "name": "apply_patch",
            "description": "Apply a unified diff to the workspace, as git apply does. Suits edits across several files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "patch": { "type": "string" }
                },
                "required": ["patch"]
            }
        }),
        json!({
            "type": "function",
            "name": "bash",
            "description": "Run a shell command in the workspace and return its exit code and output. With yield_time_ms, a command still running then returns a session_id for write_stdin.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "workdir": { "type": "string" },
                    "timeout": { "type": "number", "description": "Seconds, default 60." },
                    "yield_time_ms": { "type": "number", "description": "Return early after this many ms if still running (servers, watchers, long jobs)." }
                },
                "required": ["command"]
            }
        }),
        json!({
            "type": "function",
            "name": "write_stdin",
            "description": "Send input to a running bash session, or poll it for output.",
            "parameters": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "number" },
                    "text": { "type": "string", "description": "Omit to only poll." },
                    "yield_time_ms": { "type": "number", "description": "How long to wait for output, default 1000." }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "type": "function",
            "name": "ls",
            "description": "List a directory, sorted; directories end with /.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "limit": { "type": "number", "description": "Max entries." }
                }
            }
        }),
        json!({
            "type": "function",
            "name": "ripgrep",
            "description": "Search file contents with ripgrep (respects .gitignore). Returns matching lines with paths and line numbers.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex, or plain text with literal: true." },
                    "path": { "type": "string", "description": "File or directory." },
                    "glob": { "type": "string", "description": "File filter, e.g. *.rs." },
                    "ignoreCase": { "type": "boolean" },
                    "literal": { "type": "boolean" },
                    "contextLines": { "type": "number", "description": "Lines shown around each match." },
                    "limit": { "type": "number", "description": "Max output lines." }
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "type": "function",
            "name": "outline",
            "description": "List a source file's symbols with line numbers, without reading the whole file.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "limit": { "type": "number", "description": "Max symbols, default 200." }
                },
                "required": ["path"]
            }
        }),
        json!({
            "type": "function",
            "name": "checkpoint",
            "description": "Snapshot files under .lynshen/checkpoints, list snapshots, or restore one (local rollback, not git).",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["create", "list", "restore"] },
                    "name": { "type": "string", "description": "For create." },
                    "id": { "type": "string", "description": "For restore." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Files to snapshot, for create." }
                },
                "required": ["action"]
            }
        }),
        crate::web_fetch::definition(),
        crate::web::search_definition(),
        crate::images::definition(),
    ]
    .into_iter()
    .chain(crate::android::definitions())
    .collect())
}

fn with_function_tool_defaults(mut definitions: Vec<Value>) -> Vec<Value> {
    for definition in &mut definitions {
        if definition.get("type").and_then(Value::as_str) == Some("function") {
            if let Some(map) = definition.as_object_mut() {
                map.entry("strict").or_insert(json!(false));
            }
        }
    }
    definitions
}

#[cfg(test)]
thread_local! {
    /// One tracker per test thread, standing in for an engine's (in a
    /// session of its own).
    static TEST_STATE: ToolState = {
        let state = ToolState::default();
        state.set_session_id("test-session");
        state
    };
}

#[cfg(test)]
fn test_state() -> ToolState {
    TEST_STATE.with(ToolState::clone)
}

#[cfg(test)]
fn run_tool(name: &str, arguments: &str, cwd: &Path) -> String {
    run_tool_with_events(name, arguments, cwd, &[], &test_state(), |_| Ok(())).output
}

/// Runs a built-in tool. The read tools read anywhere but credentials
/// (`readable_path`), so `_extra_read_roots` (the skills' directories) no
/// longer widens anything. The write tools write inside the boundary, and
/// outside it only where `state` allows this call (`write_target`).
pub fn run_tool_with_events(
    name: &str,
    arguments: &str,
    cwd: &Path,
    _extra_read_roots: &[PathBuf],
    state: &ToolState,
    emit: impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> ToolExecutionResult {
    // A bug inside one tool must come back to the model as a failed call:
    // a panic here would otherwise leave the turn waiting for a result forever.
    let run = std::panic::AssertUnwindSafe(|| run_tool_inner(name, arguments, cwd, state, emit));
    std::panic::catch_unwind(run).unwrap_or_else(|panic| {
        let reason = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown error".to_string());
        tool_result(
            name,
            json!({ "error": format!("internal error in {name}: {reason}. The file may be unchanged; read it again before retrying.") }),
            cwd,
        )
    })
}

fn run_tool_inner(
    name: &str,
    arguments: &str,
    cwd: &Path,
    state: &ToolState,
    mut emit: impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> ToolExecutionResult {
    let parsed = serde_json::from_str::<Value>(arguments);
    let mut args = match parsed {
        Ok(args) => args,
        Err(error) => {
            return tool_result(
                name,
                json!({ "error": format!("invalid JSON arguments: {error}") }),
                cwd,
            )
        }
    };
    // A subagent in its own worktree reads and writes the parent's paths
    // where they sit in the worktree.
    map_confined_paths(name, &mut args, state);

    let sandbox = state.sandbox();
    let mut result = match name {
        "read" => read_file(&args, cwd, state),
        "str_replace" | "edit" => str_replace_file(&args, cwd, state),
        "hashline_edit" => hashline_edit_file(&args, cwd, state),
        "write" => write_file(&args, cwd, state),
        "bash" | "execute" | "exec_command" | "shell_command" => {
            let confine = state
                .confine
                .as_ref()
                .map(|confine| (confine.0.as_path(), confine.1.as_path()));
            let shell = state.shell();
            let value = bash(&args, cwd, &shell, sandbox.as_ref(), confine, &mut emit);
            if let Some(session_id) = value.get("session_id").and_then(Value::as_u64) {
                state.own_shell(session_id);
            }
            value
        }
        "write_stdin" => write_stdin(&args, state),
        "apply_patch" => apply_patch(&args, cwd, sandbox.as_ref(), state, &mut emit),
        "ls" => list_dir(&args, cwd),
        "ripgrep" => ripgrep(&args, cwd),
        "outline" => outline_file(&args, cwd),
        "checkpoint" => checkpoint_tool(&args, cwd, state),
        "web_fetch" => crate::web::run_fetch(&args, state.web().as_ref()),
        "web_search" => crate::web::run_search(&args, state.web().as_ref()),
        crate::images::TOOL_NAME => crate::images::run(&args, cwd, state, &mut emit),
        name if crate::android::is_tool(name) => match state.android() {
            Ok(au) => crate::android::run(name, &args, &au),
            Err(reason) => json!({ "error": reason }),
        },
        _ => json!({ "error": format!("unknown tool: {name}") }),
    };
    if state.outside_writes.is_some() && result.get("error").is_none() {
        if let Value::Object(map) = &mut result {
            map.insert(
                "outside_workspace".to_string(),
                json!("Written outside the workspace: checkpoints and /rewind do not cover this file."),
            );
        }
    }
    tool_result(name, result, cwd)
}

fn tool_result(name: &str, value: Value, cwd: &Path) -> ToolExecutionResult {
    let output = value.to_string();
    let model_output = project_model_output(name, &output, cwd);
    // Log the tool name and a brief error only; arguments and file contents
    // must never reach the log, nor what the phone shows (its errors quote
    // the screen).
    if let Some(error) = value
        .get("error")
        .and_then(Value::as_str)
        .filter(|_| !crate::android::is_tool(name))
    {
        let brief = error.chars().take(200).collect::<String>();
        crate::log_warn!("tools", "tool error", tool = name, error = brief);
    }
    ToolExecutionResult {
        is_error: value.get("error").is_some()
            || value
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
            || value
                .get("timed_out")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        output,
        model_output,
    }
}

fn add_soft_hint(value: &mut Value, warning: &str, suggestion: &str) {
    if let Value::Object(map) = value {
        map.entry("warning".to_string())
            .or_insert_with(|| json!(warning));
        map.entry("suggestion".to_string())
            .or_insert_with(|| json!(suggestion));
    }
}

fn read_file(args: &Value, cwd: &Path, state: &ToolState) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };

    let path = match readable_path(cwd, path) {
        Ok(path) => path,
        Err(error) => return json!({ "error": error }),
    };
    let offset = match optional_usize(args, "offset") {
        Ok(offset) => offset.unwrap_or(1).max(1),
        Err(error) => return json!({ "error": error }),
    };
    let limit = match optional_usize(args, "limit") {
        Ok(limit) => limit.map(|limit| limit.max(1)),
        Err(error) => return json!({ "error": error }),
    };

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };
    if metadata.is_dir() {
        // "Is a directory (os error 21)" says nothing about what to do.
        return json!({
            "path": path.display().to_string(),
            "kind": "directory",
            "error": "this is a directory, not a file: list it with ls, or search it with ripgrep",
        });
    }
    if let Some(mime) = image_mime(&path) {
        if metadata.len() > MAX_IMAGE_READ_BYTES {
            return json!({
                "path": path.display().to_string(),
                "kind": "image",
                "mime": mime,
                "bytes": metadata.len(),
                "truncated": true,
                "error": "image is too large to inline; inspect it with an external viewer or a narrower tool"
            });
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return json!({ "path": path.display().to_string(), "error": error.to_string() })
            }
        };
        state.mark_read(&path);
        return json!({
            "path": path.display().to_string(),
            "kind": "image",
            "mime": mime,
            "bytes": bytes.len(),
            "base64": BASE64_STANDARD.encode(bytes),
        });
    }

    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };
    let Some((text, encoding)) = decode_text_bytes(&bytes) else {
        return json!({
            "path": path.display().to_string(),
            "kind": "binary",
            "bytes": bytes.len(),
            "error": "this file is not text in a supported encoding (UTF-8, UTF-16 or Latin-1); inspect it with a shell command such as file, strings or xxd",
        });
    };

    let mut content = String::new();
    let mut hashlines = String::new();
    let mut lines_read = 0usize;
    let mut truncated = false;
    let mut line_number = 0usize;

    for line in text.split_inclusive('\n') {
        line_number += 1;
        if line_number < offset {
            continue;
        }
        if limit.is_some_and(|limit| lines_read >= limit) {
            truncated = true;
            break;
        }

        content.push_str(line);
        let display_line = line.strip_suffix('\n').unwrap_or(line);
        hashlines.push_str(&format_hashline(line_number, display_line));
        if line.ends_with('\n') {
            hashlines.push('\n');
        }
        lines_read += 1;
    }
    state.mark_read(&path);

    let mut value = json!({
        "path": path.display().to_string(),
        "kind": "text",
        "encoding": encoding,
        "offset": offset,
        "lines_read": lines_read,
        "truncated": truncated,
        "content": content,
        "hashlines": hashlines,
    });
    if lines_read == 0 && offset > 1 {
        // An empty read with no word about why sends the model round again.
        value["note"] = json!(format!(
            "offset {offset} is past the end of the file, which has {line_number} lines"
        ));
    }
    if limit.is_none() && metadata.len() > LARGE_TEXT_READ_SOFT_BYTES {
        add_soft_hint(
            &mut value,
            "large file read without a line limit",
            "Use read with offset/limit, outline, or ripgrep to narrow the next read unless the full file is required.",
        );
    }
    value
}

fn str_replace_file(args: &Value, cwd: &Path, state: &ToolState) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let Some(edits) = args.get("edits").and_then(Value::as_array) else {
        return json!({ "error": "missing edits" });
    };
    if edits.is_empty() {
        return json!({ "error": "edits must not be empty" });
    }

    let path = match write_target(cwd, path, state) {
        Ok(path) => path,
        Err(error) => return json!({ "error": error }),
    };
    if let Some(error) = unread_or_stale_error(
        state,
        &path,
        "edit requires reading this file first so oldText matches bytes on disk",
    ) {
        return error;
    }
    let original = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };

    // Matched and edited with LF line breaks; see LfText for the line
    // endings written back.
    let text = LfText::new(&original);
    let mut replacements = Vec::new();
    for edit in edits {
        let Some(old_text) = edit.get("oldText").and_then(Value::as_str) else {
            return json!({ "path": path.display().to_string(), "error": "each edit requires oldText" });
        };
        let Some(new_text) = edit.get("newText").and_then(Value::as_str) else {
            return json!({ "path": path.display().to_string(), "error": "each edit requires newText" });
        };
        if old_text.is_empty() {
            return json!({ "path": path.display().to_string(), "error": "oldText must not be empty" });
        }
        let find = if text.has_crlf() {
            old_text.replace("\r\n", "\n")
        } else {
            old_text.to_string()
        };

        let matches = text.lf.match_indices(find.as_str()).collect::<Vec<_>>();
        if matches.len() != 1 {
            return json!({
                "path": path.display().to_string(),
                "error": format!("oldText must match exactly once; found {}", matches.len()),
                "oldText": old_text,
            });
        }
        let start = matches[0].0;
        replacements.push((start, start + find.len(), new_text));
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    for pair in replacements.windows(2) {
        if pair[0].1 > pair[1].0 {
            return json!({ "path": path.display().to_string(), "error": "edits must not overlap" });
        }
    }

    let output = text.apply(
        &original,
        replacements
            .iter()
            .map(|(start, end, new_text)| (*start, *end, *new_text)),
    );

    let checkpoint = begin_checkpoint(cwd, "auto-edit", std::slice::from_ref(&path), state);
    let written = fs::write(&path, &output);
    let _ = checkpoint.and_then(PendingCheckpoint::finish);
    match written {
        Ok(()) => {
            state.mark_read(&path);
            let diff = unified_diff_for_file(cwd, &path, &original, &output);
            json!({
                "path": path.display().to_string(),
                "edits": replacements.len(),
                "written_bytes": output.len(),
                "diff": diff.unwrap_or_default(),
            })
        }
        Err(error) => json!({ "path": path.display().to_string(), "error": error.to_string() }),
    }
}

#[derive(Clone)]
struct HashlineAnchor {
    line: usize,
    /// Empty when the reference named only a line ("329"): the line's
    /// current hash is taken, which is safe because an edit runs only on a
    /// file read and unchanged since (see `unread_or_stale_error`).
    hash: String,
}

struct HashlineEdit {
    op: String,
    pos: Option<HashlineAnchor>,
    end: Option<HashlineAnchor>,
    lines: Vec<String>,
}

struct HashlineSpan {
    start: usize,
    end: usize,
    replacement: String,
}

fn hashline_edit_file(args: &Value, cwd: &Path, state: &ToolState) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let Some(raw_edits) = args.get("edits").and_then(Value::as_array) else {
        return json!({ "error": "missing edits" });
    };
    if raw_edits.is_empty() {
        return json!({ "error": "edits must not be empty" });
    }

    let path = match write_target(cwd, path, state) {
        Ok(path) => path,
        Err(error) => return json!({ "error": error }),
    };
    if let Some(error) = unread_or_stale_error(
        state,
        &path,
        "hashline_edit requires reading this file first and copying LINE#HASH anchors from read().hashlines",
    ) {
        return error;
    }

    let original = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };

    let output = match apply_hashline_edits_preview(&original, raw_edits) {
        Ok(output) => output,
        Err(error) => {
            let error = explain_bad_ref(&error, raw_edits, &original);
            return json!({ "path": path.display().to_string(), "error": error });
        }
    };

    let checkpoint = begin_checkpoint(
        cwd,
        "auto-hashline-edit",
        std::slice::from_ref(&path),
        state,
    );
    let written = fs::write(&path, &output);
    let _ = checkpoint.and_then(PendingCheckpoint::finish);
    match written {
        Ok(()) => {
            state.mark_read(&path);
            let diff = unified_diff_for_file(cwd, &path, &original, &output);
            let changed = changed_line_range(&original, &output);
            let anchors =
                changed.and_then(|(first, last)| post_edit_anchor_block(&output, first, last));
            json!({
                "path": path.display().to_string(),
                "edits": raw_edits.len(),
                "written_bytes": output.len(),
                "diff": diff.unwrap_or_default(),
                "anchors": anchors.unwrap_or_default(),
            })
        }
        Err(error) => json!({ "path": path.display().to_string(), "error": error.to_string() }),
    }
}

fn write_file(args: &Value, cwd: &Path, state: &ToolState) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let Some(content) = args.get("content").and_then(Value::as_str) else {
        return json!({ "error": "missing content" });
    };

    let path = match write_target(cwd, path, state) {
        Ok(path) => path,
        Err(error) => return json!({ "error": error }),
    };
    let exists = path.exists();
    if exists {
        if let Some(error) = unread_or_stale_error(
            state,
            &path,
            "write requires reading an existing file first before overwriting it; new files can be written without a prior read",
        ) {
            return error;
        }
    }
    let original = if exists {
        fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };
    // Overwriting a CRLF file keeps its line endings.
    let content = if uses_crlf(&original) {
        to_crlf(content)
    } else {
        content.to_string()
    };
    let content = content.as_str();
    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return json!({ "path": path.display().to_string(), "error": error.to_string() });
        }
    }

    let checkpoint = begin_checkpoint(cwd, "auto-write", std::slice::from_ref(&path), state);
    let written = fs::write(&path, content);
    let _ = checkpoint.and_then(PendingCheckpoint::finish);
    match written {
        Ok(()) => {
            state.mark_read(&path);
            let diff = unified_diff_for_file(cwd, &path, &original, content);
            json!({
                "path": path.display().to_string(),
                "written_bytes": content.len(),
                "diff": diff.unwrap_or_default(),
            })
        }
        Err(error) => json!({ "path": path.display().to_string(), "error": error.to_string() }),
    }
}

/// Whether a file's text uses CRLF line breaks: most of its line breaks
/// are CRLF. The lines an edit inserts into such a file get CRLF.
fn uses_crlf(text: &str) -> bool {
    let crlf = text.matches("\r\n").count();
    crlf > 0 && crlf * 2 > text.matches('\n').count()
}

fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// A file's text with its CRLF line breaks read as LF, so an edit is
/// matched and made with LF line breaks and then applied to the original:
/// text outside the edit keeps its bytes, and the text it inserts gets the
/// file's main line ending (`uses_crlf`). A file without CRLF is edited as
/// it is.
struct LfText {
    lf: String,
    /// Offsets in `lf` of the line feeds that were CRLF.
    crlf_at: Vec<usize>,
    crlf: bool,
}

impl LfText {
    fn new(original: &str) -> Self {
        let mut lf = String::with_capacity(original.len());
        let mut crlf_at = Vec::new();
        let mut rest = original;
        while let Some(at) = rest.find("\r\n") {
            lf.push_str(&rest[..at]);
            crlf_at.push(lf.len());
            lf.push('\n');
            rest = &rest[at + 2..];
        }
        lf.push_str(rest);
        Self {
            lf,
            crlf_at,
            crlf: uses_crlf(original),
        }
    }

    fn has_crlf(&self) -> bool {
        !self.crlf_at.is_empty()
    }

    /// `original` with each `(start, end, replacement)` applied: offsets in
    /// `lf`, in order and not overlapping.
    fn apply<'a>(
        &self,
        original: &str,
        edits: impl IntoIterator<Item = (usize, usize, &'a str)>,
    ) -> String {
        let to_original = |offset: usize| offset + self.crlf_at.partition_point(|&at| at < offset);
        let mut output = String::with_capacity(original.len());
        let mut cursor = 0;
        for (start, end, replacement) in edits {
            output.push_str(&original[cursor..to_original(start)]);
            if !self.has_crlf() {
                output.push_str(replacement);
            } else if self.crlf {
                output.push_str(&to_crlf(replacement));
            } else {
                output.push_str(&replacement.replace("\r\n", "\n"));
            }
            cursor = to_original(end);
        }
        output.push_str(&original[cursor..]);
        output
    }
}

/// Applies hashline edits to `original` without touching the filesystem.
/// Shared by `hashline_edit` itself and hunk planning for selective approval
/// (which previews each edit in isolation).
pub(crate) fn apply_hashline_edits_preview(
    original: &str,
    raw_edits: &[Value],
) -> Result<String, String> {
    // Made with LF line breaks (line numbers and anchors are the same
    // either way); see LfText for the line endings written back.
    let text = LfText::new(original);
    let edits = parse_hashline_edits(raw_edits)?;
    let line_index = LineIndex::new(&text.lf);
    validate_hashline_anchors(&edits, &line_index)?;
    let spans = resolve_hashline_spans(&edits, &text.lf, &line_index)?;
    Ok(text.apply(
        original,
        spans
            .iter()
            .map(|span| (span.start, span.end, span.replacement.as_str())),
    ))
}

fn parse_hashline_edits(raw_edits: &[Value]) -> Result<Vec<HashlineEdit>, String> {
    let mut edits = Vec::new();
    for edit in raw_edits {
        let op = edit
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| "each hashline edit requires op".to_string())?;
        if !matches!(op, "replace" | "append" | "prepend") {
            return Err(format!(
                "[E_BAD_OP] Unknown edit op \"{op}\". Expected replace, append, or prepend."
            ));
        }

        let pos = parse_anchor_argument(edit, "pos")?;
        let end = parse_anchor_argument(edit, "end")?;
        let lines = parse_hashline_lines(edit.get("lines"))?;

        if op == "replace" && pos.is_none() {
            return Err("[E_BAD_OP] Replace requires a pos anchor.".to_string());
        }
        if op != "replace" && end.is_some() {
            return Err(format!("[E_BAD_OP] {op} does not support an end anchor."));
        }
        if op != "replace" && lines.is_empty() {
            return Err(format!("[E_BAD_OP] {op} requires at least one line."));
        }

        edits.push(HashlineEdit {
            op: op.to_string(),
            pos,
            end,
            lines,
        });
    }
    Ok(edits)
}

fn parse_hashline_lines(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Err("each hashline edit requires lines".to_string());
    };
    let lines = if let Some(text) = value.as_str() {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        normalized
            .strip_suffix('\n')
            .unwrap_or(&normalized)
            .split('\n')
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else if let Some(values) = value.as_array() {
        values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "lines array must contain only strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        return Err("lines must be a string or an array of strings".to_string());
    };
    for line in &lines {
        if is_hashline_display_prefix(line) || is_diff_payload_prefix(line) {
            return Err(format!(
                "[E_INVALID_PATCH] lines must contain literal file content, not LINE#HASH or diff prefixes. Offending line: {line:?}"
            ));
        }
    }
    Ok(lines)
}

/// `pos`/`end` as the model wrote it: the LINE#HASH string copied from
/// read(), or a bare line number sent as a JSON number instead of a string
/// (which used to be dropped and reported as a missing anchor).
fn parse_anchor_argument(edit: &Value, key: &str) -> Result<Option<HashlineAnchor>, String> {
    match edit.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => parse_hashline_anchor(text).map(Some),
        Some(Value::Number(number)) => match number.as_u64() {
            Some(line) if line >= 1 => Ok(Some(HashlineAnchor {
                line: line as usize,
                hash: String::new(),
            })),
            _ => Err(bad_ref_message(&number.to_string())),
        },
        Some(other) => Err(bad_ref_message(&other.to_string())),
    }
}

fn bad_ref_message(ref_text: &str) -> String {
    format!(
        "[E_BAD_REF] Invalid line reference {ref_text:?}. Expected LINE#HASH, e.g. \"12#AB\" copied from read()."
    )
}

fn parse_hashline_anchor(ref_text: &str) -> Result<HashlineAnchor, String> {
    let core = ref_text
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '>' || ch == '+' || ch == '-')
        .trim_end();
    let Some(hash_pos) = core.find('#') else {
        // A bare line number: models often drop the hash they copied.
        let line = core.split(':').next().unwrap_or_default().trim();
        return match line.parse::<usize>() {
            Ok(0) => Err(format!(
                "[E_BAD_REF] Line number must be >= 1 in {ref_text:?}."
            )),
            Ok(line) => Ok(HashlineAnchor {
                line,
                hash: String::new(),
            }),
            Err(_) => Err(bad_ref_message(ref_text)),
        };
    };
    let line = core[..hash_pos].trim().parse::<usize>().map_err(|_| {
        format!("[E_BAD_REF] Invalid line reference {ref_text:?}. Expected numeric LINE#HASH.")
    })?;
    if line == 0 {
        return Err(format!(
            "[E_BAD_REF] Line number must be >= 1 in {ref_text:?}."
        ));
    }
    let hash_part = core[hash_pos + 1..]
        .split_once(':')
        .map(|(hash, _)| hash)
        .unwrap_or(&core[hash_pos + 1..])
        .trim();
    if hash_part.len() != 2
        || !hash_part
            .as_bytes()
            .iter()
            .all(|byte| HASHLINE_ALPHABET.contains(byte))
    {
        return Err(format!(
            "[E_BAD_REF] Invalid line reference {ref_text:?}: hash must be exactly 2 characters from {}.",
            String::from_utf8_lossy(HASHLINE_ALPHABET)
        ));
    }
    Ok(HashlineAnchor {
        line,
        hash: hash_part.to_string(),
    })
}

struct LineIndex {
    lines: Vec<String>,
    starts: Vec<usize>,
    has_terminal_newline: bool,
}

impl LineIndex {
    fn new(content: &str) -> Self {
        let lines = content.split('\n').map(str::to_string).collect::<Vec<_>>();
        let mut starts = Vec::with_capacity(lines.len());
        let mut offset = 0usize;
        for (index, line) in lines.iter().enumerate() {
            starts.push(offset);
            offset += line.len();
            if index < lines.len() - 1 {
                offset += 1;
            }
        }
        Self {
            lines,
            starts,
            has_terminal_newline: content.ends_with('\n'),
        }
    }
}

fn validate_hashline_anchors(edits: &[HashlineEdit], line_index: &LineIndex) -> Result<(), String> {
    let mut mismatches = Vec::new();
    for edit in edits {
        for anchor in [&edit.pos, &edit.end].into_iter().flatten() {
            if anchor.line == 0 || anchor.line > line_index.lines.len() {
                return Err(format!(
                    "[E_RANGE_OOB] Line {} does not exist (file has {} lines).",
                    anchor.line,
                    line_index.lines.len()
                ));
            }
            let actual = compute_line_hash(anchor.line, &line_index.lines[anchor.line - 1]);
            if !anchor.hash.is_empty() && actual != anchor.hash {
                mismatches.push((anchor.line, anchor.hash.clone(), actual));
            }
        }
    }
    if mismatches.is_empty() {
        return Ok(());
    }
    Err(format_hashline_mismatch(&mismatches, &line_index.lines))
}

fn resolve_hashline_spans(
    edits: &[HashlineEdit],
    content: &str,
    line_index: &LineIndex,
) -> Result<Vec<HashlineSpan>, String> {
    let mut spans = Vec::new();
    for edit in edits {
        let span = match edit.op.as_str() {
            "replace" => resolve_hashline_replace(edit, content, line_index)?,
            "append" => resolve_hashline_append(edit, content, line_index)?,
            "prepend" => resolve_hashline_prepend(edit, content, line_index)?,
            _ => return Err(format!("[E_BAD_OP] Unknown edit op {}.", edit.op)),
        };
        spans.push(span);
    }
    spans.sort_by_key(|span| (span.start, span.end));
    for pair in spans.windows(2) {
        if pair[0].end > pair[1].start {
            return Err("[E_EDIT_CONFLICT] hashline edits must not overlap.".to_string());
        }
        if pair[0].start == pair[1].start && pair[0].end == pair[1].end {
            return Err(
                "[E_EDIT_CONFLICT] hashline edits target the same insertion boundary.".to_string(),
            );
        }
    }
    Ok(spans)
}

fn resolve_hashline_replace(
    edit: &HashlineEdit,
    content: &str,
    line_index: &LineIndex,
) -> Result<HashlineSpan, String> {
    let pos = edit.pos.as_ref().expect("replace pos validated");
    let end = edit.end.as_ref().unwrap_or(pos);
    if pos.line > end.line {
        return Err(format!(
            "[E_BAD_OP] Range start line {} must be <= end line {}.",
            pos.line, end.line
        ));
    }
    let replacement = edit.lines.join("\n");
    let (start, end_offset) = if edit.lines.is_empty() {
        if pos.line == 1 && end.line == line_index.lines.len() {
            (0, content.len())
        } else if end.line < line_index.lines.len() {
            (line_index.starts[pos.line - 1], line_index.starts[end.line])
        } else {
            // Deleting through the final line of a file without a trailing
            // newline: consume the preceding newline instead of a trailing one.
            (
                line_index.starts[pos.line - 1].saturating_sub(1),
                content.len(),
            )
        }
    } else {
        (
            line_index.starts[pos.line - 1],
            line_index.starts[end.line - 1] + line_index.lines[end.line - 1].len(),
        )
    };
    Ok(HashlineSpan {
        start,
        end: end_offset,
        replacement,
    })
}

fn resolve_hashline_append(
    edit: &HashlineEdit,
    content: &str,
    line_index: &LineIndex,
) -> Result<HashlineSpan, String> {
    let inserted = edit.lines.join("\n");
    if content.is_empty() {
        return Ok(HashlineSpan {
            start: 0,
            end: 0,
            replacement: inserted,
        });
    }
    if let Some(pos) = &edit.pos {
        let sentinel_append = line_index.has_terminal_newline && pos.line == line_index.lines.len();
        let offset = if sentinel_append {
            content.len()
        } else {
            line_index.starts[pos.line - 1] + line_index.lines[pos.line - 1].len()
        };
        Ok(HashlineSpan {
            start: offset,
            end: offset,
            replacement: if sentinel_append {
                format!("{inserted}\n")
            } else {
                format!("\n{inserted}")
            },
        })
    } else {
        Ok(HashlineSpan {
            start: content.len(),
            end: content.len(),
            replacement: if line_index.has_terminal_newline {
                format!("{inserted}\n")
            } else {
                format!("\n{inserted}")
            },
        })
    }
}

fn resolve_hashline_prepend(
    edit: &HashlineEdit,
    content: &str,
    line_index: &LineIndex,
) -> Result<HashlineSpan, String> {
    let inserted = edit.lines.join("\n");
    let start = edit
        .pos
        .as_ref()
        .map(|pos| line_index.starts[pos.line - 1])
        .unwrap_or(0);
    Ok(HashlineSpan {
        start,
        end: start,
        replacement: if content.is_empty() {
            inserted
        } else {
            format!("{inserted}\n")
        },
    })
}

fn format_hashline_mismatch(mismatches: &[(usize, String, String)], lines: &[String]) -> String {
    let around = mismatches
        .iter()
        .map(|(line, _, _)| *line)
        .collect::<Vec<_>>();
    let mut out = vec![format!(
        "[E_STALE_ANCHOR] {} stale anchor{}. Retry with the >>> LINE#HASH lines below.",
        mismatches.len(),
        if mismatches.len() == 1 { "" } else { "s" }
    )];
    out.extend(retry_anchor_lines(&around, lines));
    out.join("\n")
}

/// The `>>> LINE#HASH:content` lines for each line in `around` and its two
/// neighbours: what to copy into the next call.
fn retry_anchor_lines(around: &[usize], lines: &[String]) -> Vec<String> {
    let mut wanted = HashSet::new();
    for line in around {
        let start = line.saturating_sub(2).max(1);
        let end = (*line + 2).min(lines.len());
        for retry in start..=end {
            wanted.insert(retry);
        }
    }
    let mut sorted = wanted.into_iter().collect::<Vec<_>>();
    sorted.sort_unstable();
    sorted
        .into_iter()
        .map(|line| {
            let content = &lines[line - 1];
            format!(
                ">>> {}#{}:{}",
                line,
                compute_line_hash(line, content),
                content
            )
        })
        .collect()
}

/// A rejected anchor is easier to correct with the file's own anchors in
/// hand. When the reference named a line that exists, that line and its
/// neighbours are appended as a retry block, as for a stale anchor;
/// otherwise the error says what an anchor looks like and where it comes
/// from (a placeholder such as `${fetch lines}` reaches here).
fn explain_bad_ref(error: &str, raw_edits: &[Value], original: &str) -> String {
    if !error.starts_with("[E_BAD_REF]") {
        return error.to_string();
    }
    let line_index = LineIndex::new(original);
    let visible = if line_index.has_terminal_newline {
        line_index.lines.len().saturating_sub(1)
    } else {
        line_index.lines.len()
    };
    let named: Vec<usize> = raw_edits
        .iter()
        .flat_map(|edit| ["pos", "end"].map(|key| edit.get(key).cloned()))
        .flatten()
        .filter_map(|value| leading_line_number(&value))
        .filter(|line| (1..=visible).contains(line))
        .collect();
    if named.is_empty() {
        return format!(
            "{error} Copy the anchor from the hashlines of a read() of this file; a placeholder or a line of your own text is not an anchor."
        );
    }
    let mut out = vec![format!("{error} Retry with the >>> LINE#HASH lines below.")];
    out.extend(retry_anchor_lines(&named, &line_index.lines));
    out.join("\n")
}

/// The line number a `pos`/`end` value starts with, however it was written.
fn leading_line_number(value: &Value) -> Option<usize> {
    if let Some(number) = value.as_u64() {
        return usize::try_from(number).ok();
    }
    let text = value.as_str()?;
    let digits: String = text
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '>' || ch == '+' || ch == '-')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn is_hashline_display_prefix(line: &str) -> bool {
    let trimmed = line
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '>' || ch == '+')
        .trim_start();
    let Some((line_part, rest)) = trimmed.split_once('#') else {
        return false;
    };
    if !line_part.trim().chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let Some((hash, _)) = rest.split_once(':') else {
        return false;
    };
    hash.trim().len() == 2
        && hash
            .trim()
            .as_bytes()
            .iter()
            .all(|byte| HASHLINE_ALPHABET.contains(byte))
}

fn is_diff_payload_prefix(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('-') else {
        return false;
    };
    let trimmed = rest.trim_start();
    let digit_count = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digit_count > 0 && trimmed[digit_count..].starts_with("    ")
}

fn format_hashline(line_number: usize, line: &str) -> String {
    format!(
        "{line_number}#{}:{line}",
        compute_line_hash(line_number, line)
    )
}

pub(crate) fn compute_line_hash(line_number: usize, line: &str) -> String {
    let normalized = line.trim_end_matches('\r').trim_end();
    let seed = if normalized.chars().any(|ch| ch.is_alphanumeric()) {
        0
    } else {
        line_number as u32
    };
    let value = xxh32(normalized.as_bytes(), seed) & 0xff;
    let high = ((value >> 4) & 0x0f) as usize;
    let low = (value & 0x0f) as usize;
    let mut hash = String::with_capacity(2);
    hash.push(HASHLINE_ALPHABET[high] as char);
    hash.push(HASHLINE_ALPHABET[low] as char);
    hash
}

fn xxh32(input: &[u8], seed: u32) -> u32 {
    const PRIME32_1: u32 = 0x9E3779B1;
    const PRIME32_2: u32 = 0x85EBCA77;
    const PRIME32_3: u32 = 0xC2B2AE3D;
    const PRIME32_4: u32 = 0x27D4EB2F;
    const PRIME32_5: u32 = 0x165667B1;

    let mut index = 0usize;
    let mut hash;
    if input.len() >= 16 {
        let mut v1 = seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
        let mut v2 = seed.wrapping_add(PRIME32_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME32_1);
        while index <= input.len() - 16 {
            v1 = xxh32_round(v1, read_u32_le(input, index));
            index += 4;
            v2 = xxh32_round(v2, read_u32_le(input, index));
            index += 4;
            v3 = xxh32_round(v3, read_u32_le(input, index));
            index += 4;
            v4 = xxh32_round(v4, read_u32_le(input, index));
            index += 4;
        }
        hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
    } else {
        hash = seed.wrapping_add(PRIME32_5);
    }

    hash = hash.wrapping_add(input.len() as u32);
    while index + 4 <= input.len() {
        hash = hash
            .wrapping_add(read_u32_le(input, index).wrapping_mul(PRIME32_3))
            .rotate_left(17)
            .wrapping_mul(PRIME32_4);
        index += 4;
    }
    while index < input.len() {
        hash = hash
            .wrapping_add(u32::from(input[index]).wrapping_mul(PRIME32_5))
            .rotate_left(11)
            .wrapping_mul(PRIME32_1);
        index += 1;
    }

    hash ^= hash >> 15;
    hash = hash.wrapping_mul(PRIME32_2);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(PRIME32_3);
    hash ^= hash >> 16;
    hash
}

fn xxh32_round(acc: u32, lane: u32) -> u32 {
    const PRIME32_1: u32 = 0x9E3779B1;
    const PRIME32_2: u32 = 0x85EBCA77;
    acc.wrapping_add(lane.wrapping_mul(PRIME32_2))
        .rotate_left(13)
        .wrapping_mul(PRIME32_1)
}

fn read_u32_le(input: &[u8], index: usize) -> u32 {
    u32::from_le_bytes([
        input[index],
        input[index + 1],
        input[index + 2],
        input[index + 3],
    ])
}

fn changed_line_range(original: &str, updated: &str) -> Option<(usize, usize)> {
    if original == updated {
        return None;
    }
    let original_bytes = original.as_bytes();
    let updated_bytes = updated.as_bytes();
    let min_len = original_bytes.len().min(updated_bytes.len());
    let mut first_diff = 0usize;
    while first_diff < min_len && original_bytes[first_diff] == updated_bytes[first_diff] {
        first_diff += 1;
    }

    let mut original_tail = original_bytes.len();
    let mut updated_tail = updated_bytes.len();
    while original_tail > first_diff
        && updated_tail > first_diff
        && original_bytes[original_tail - 1] == updated_bytes[updated_tail - 1]
    {
        original_tail -= 1;
        updated_tail -= 1;
    }

    let first = byte_index_to_line(updated, first_diff);
    let last = if updated_tail <= first_diff {
        first
    } else {
        byte_index_to_line(updated, updated_tail.saturating_sub(1))
    };
    Some((first, last.max(first)))
}

fn byte_index_to_line(text: &str, byte_index: usize) -> usize {
    // Counting newlines over bytes needs no char boundary: the byte-wise
    // prefix/suffix scan above can stop inside a multi-byte character.
    let end = byte_index.min(text.len());
    text.as_bytes()[..end]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn post_edit_anchor_block(content: &str, first: usize, last: usize) -> Option<String> {
    let lines = content.split('\n').map(str::to_string).collect::<Vec<_>>();
    let visible_line_count = if content.ends_with('\n') {
        lines.len().saturating_sub(1)
    } else {
        lines.len()
    };
    if visible_line_count == 0 {
        return None;
    }
    let start = first.saturating_sub(2).max(1);
    let end = (last + 2).min(visible_line_count);
    if end < start || end - start + 1 > 12 {
        return None;
    }
    let mut out = Vec::new();
    out.push(format!("--- Anchors {start}-{end} ---"));
    for line_number in start..=end {
        out.push(format_hashline(line_number, &lines[line_number - 1]));
    }
    Some(out.join("\n"))
}

/// Runs a shell command. With a sandbox it runs inside it, unless the call
/// carries `escalate: true` (the approval gate has already let it through).
/// A confined agent's `(worktree, project)`: the command starts inside the
/// worktree, and in the sandbox writes only there.
fn bash(
    args: &Value,
    cwd: &Path,
    shell: &crate::shell::CommandShell,
    sandbox: Option<&crate::sandbox::SandboxPolicy>,
    confine: Option<(&Path, &Path)>,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Value {
    let Some(command) = args
        .get("command")
        .or_else(|| args.get("cmd"))
        .and_then(Value::as_str)
    else {
        return json!({ "error": "missing command; use `command` for bash or `cmd` for exec_command" });
    };
    let workdir = match command_workdir(args, cwd) {
        Ok(workdir) => workdir,
        Err(error) => return json!({ "command": command, "error": error }),
    };
    if let Some((root, _)) = confine {
        if !normalize_lexically(&workdir).starts_with(normalize_lexically(root)) {
            return json!({
                "command": command,
                "error": format!("workdir {} is outside your worktree {}; run commands there", workdir.display(), root.display())
            });
        }
    }
    let timeout = match optional_u64(args, "timeout") {
        Ok(timeout) => Duration::from_secs(timeout.unwrap_or(DEFAULT_BASH_TIMEOUT_SECS).max(1)),
        Err(error) => return json!({ "command": command, "error": error }),
    };
    let yield_time = match optional_u64(args, "yield_time_ms") {
        Ok(yield_time) => yield_time.map(Duration::from_millis),
        Err(error) => return json!({ "command": command, "error": error }),
    };

    let invocation = shell.invocation(command);
    let program = invocation.program.as_str();
    let shell_args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
    let escalated = args.get("escalate").and_then(Value::as_bool) == Some(true);
    let wrapped = sandbox.filter(|_| !escalated).map(|sandbox| match confine {
        Some((root, project)) => {
            sandbox.wrap_confined(program, &shell_args, root, project, &workdir)
        }
        None => sandbox.wrap(program, &shell_args, &workdir),
    });
    let (program, shell_args) = match wrapped {
        Some(Ok(wrapped)) => wrapped,
        Some(Err(error)) => return json!({ "command": command, "error": error }),
        None => (program.to_string(), invocation.args.clone()),
    };
    let program = program.as_str();
    let shell_args: Vec<&str> = shell_args.iter().map(String::as_str).collect();
    let env = &invocation.env;
    let result = if let Some(yield_time) = yield_time {
        run_command_session(
            program,
            &shell_args,
            env,
            command,
            &workdir,
            timeout,
            yield_time,
            emit,
        )
    } else {
        run_command_events(program, &shell_args, env, None, &workdir, timeout, emit)
            .map(|result| command_result_json(command, Ok(result)))
    };
    let sandboxed = !escalated && sandbox.is_some_and(|sandbox| sandbox.enforced());
    match result {
        Ok(mut value) => {
            add_command_soft_hint(&mut value);
            if sandboxed {
                add_sandbox_hint(&mut value);
            }
            value
        }
        Err(error) => json!({ "command": command, "error": error }),
    }
}

/// A failed command whose output reads like the sandbox stopped it: tell
/// the model how to get out.
fn add_sandbox_hint(value: &mut Value) {
    let failed = value.get("error").is_some()
        || value
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0);
    let output = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
    // Seatbelt and bwrap; on Windows the restricted-token sandbox, as cmd,
    // PowerShell and Git Bash report it.
    let signs: &[&str] = if cfg!(windows) {
        &["Access is denied", "is denied.", "Permission denied"]
    } else {
        &["Operation not permitted", "EPERM", "Read-only file system"]
    };
    let blocked = signs
        .iter()
        .any(|sign| output("stderr").contains(sign) || output("stdout").contains(sign));
    if failed && blocked {
        value["sandbox_hint"] = json!(
            "This looks like the sandbox stopped it. If the command must write outside the writable directories, run it again with escalate: true and a one-line justification."
        );
    }
}

fn command_workdir(args: &Value, cwd: &Path) -> Result<PathBuf, String> {
    let Some(workdir) = args.get("workdir").and_then(Value::as_str) else {
        return Ok(cwd.to_path_buf());
    };
    let path = resolve_path(cwd, workdir);
    if !path.exists() {
        return Err(format!("workdir does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("workdir is not a directory: {}", path.display()));
    }
    Ok(path)
}

/// What `write_stdin` types: `text`, else `chars` (the gate checks the
/// same text the tool sends).
pub(crate) fn stdin_text(args: &Value) -> &str {
    args.get("text")
        .filter(|value| !value.is_null())
        .or_else(|| args.get("chars"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn write_stdin(args: &Value, state: &ToolState) -> Value {
    let session_id = match optional_u64(args, "session_id") {
        Ok(Some(session_id)) => session_id,
        Ok(None) => return json!({ "error": "missing session_id" }),
        Err(error) => return json!({ "error": error }),
    };
    let text = stdin_text(args);
    let yield_time = match optional_u64(args, "yield_time_ms") {
        Ok(yield_time) => Duration::from_millis(yield_time.unwrap_or(1000)),
        Err(error) => return json!({ "session_id": session_id, "error": error }),
    };
    // Not one of this engine's sessions: it may still be a command that ran
    // in this process and finished, whose result is repeated rather than
    // reported as a missing session.
    let result = if state.owns_shell(session_id) {
        poll_or_write_session(session_id, text, yield_time)
    } else {
        finished_or_unknown(session_id, !text.is_empty())
    };
    match result {
        Ok(value) => value,
        Err(error) => json!({ "session_id": session_id, "error": error }),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_command_session(
    program: &str,
    args: &[&str],
    env: &crate::shell::Vars,
    display_command: &str,
    cwd: &Path,
    timeout: Duration,
    yield_time: Duration,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Result<Value, String> {
    let stdout_path = temp_output_path("stdout");
    let stderr_path = temp_output_path("stderr");
    let stdout_file = File::create(&stdout_path).map_err(|error| error.to_string())?;
    let stderr_file = File::create(&stderr_path).map_err(|error| error.to_string())?;

    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().cloned())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start {program}: {error}"))?;
    track_tool_group(child.id());

    let stdin = child.stdin.take();
    let started = SystemTime::now();
    if let Err(error) = emit(ToolExecutionEvent::Update(format!(
        "started: {}",
        command_display(program, args)
    ))) {
        abort_command(&mut child, &stdout_path, &stderr_path);
        return Err(error);
    }

    let wait_until = yield_time.min(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let result = collect_command_result(
                    status.code(),
                    status_signal(&status),
                    false,
                    &stdout_path,
                    &stderr_path,
                );
                return Ok(command_result_json(display_command, Ok(result)));
            }
            Ok(None) => {}
            Err(error) => {
                abort_command(&mut child, &stdout_path, &stderr_path);
                return Err(error.to_string());
            }
        }
        if started.elapsed().unwrap_or_default() >= wait_until {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let (stdout, stdout_truncated) = read_output_file(&stdout_path);
    let (stderr, stderr_truncated) = read_output_file(&stderr_path);
    shell_sessions()
        .lock()
        .map_err(|_| "shell session lock is poisoned".to_string())?
        .insert(
            session_id,
            ShellSession {
                child,
                stdin,
                stdout_path,
                stderr_path,
                command: display_command.to_string(),
                started,
                timeout,
            },
        );

    Ok(json!({
        "command": display_command,
        "session_id": session_id,
        "running": true,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": stdout_truncated || stderr_truncated,
        "note": "command is still running; use write_stdin with this session_id to poll or send input"
    }))
}

fn poll_or_write_session(
    session_id: u64,
    text: &str,
    yield_time: Duration,
) -> Result<Value, String> {
    let started_poll = SystemTime::now();
    let mut wrote_stdin = false;
    loop {
        let mut finished = None;
        {
            let mut sessions = shell_sessions()
                .lock()
                .map_err(|_| "shell session lock is poisoned".to_string())?;
            let Some(session) = sessions.get_mut(&session_id) else {
                drop(sessions);
                return finished_or_unknown(session_id, !text.is_empty());
            };
            if session.started.elapsed().unwrap_or_default() >= session.timeout {
                kill_child(&mut session.child);
                finished = Some((true, None, None));
            } else if let Some(status) = session
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
            {
                finished = Some((false, status.code(), status_signal(&status)));
            }
            if let Some((timed_out, code, signal)) = finished {
                let session = sessions.remove(&session_id).expect("session exists");
                let result = collect_command_result(
                    code,
                    signal,
                    timed_out,
                    &session.stdout_path,
                    &session.stderr_path,
                );
                return Ok(remember_finished(
                    session_id,
                    command_result_json(&session.command, Ok(result)),
                ));
            }
            if !text.is_empty() && !wrote_stdin {
                if let Some(stdin) = session.stdin.as_mut() {
                    if let Err(error) = stdin.write_all(text.as_bytes()).and_then(|_| stdin.flush())
                    {
                        if let Ok(Some(status)) = session.child.try_wait() {
                            let signal = status_signal(&status);
                            let session = sessions.remove(&session_id).expect("session exists");
                            let result = collect_command_result(
                                status.code(),
                                signal,
                                false,
                                &session.stdout_path,
                                &session.stderr_path,
                            );
                            return Ok(remember_finished(
                                session_id,
                                command_result_json(&session.command, Ok(result)),
                            ));
                        }
                        return Err(error.to_string());
                    }
                    wrote_stdin = true;
                }
            }
        }

        if started_poll.elapsed().unwrap_or_default() >= yield_time {
            let sessions = shell_sessions()
                .lock()
                .map_err(|_| "shell session lock is poisoned".to_string())?;
            let Some(session) = sessions.get(&session_id) else {
                drop(sessions);
                return finished_or_unknown(session_id, !text.is_empty());
            };
            let (stdout, stdout_truncated) = read_output_file(&session.stdout_path);
            let (stderr, stderr_truncated) = read_output_file(&session.stderr_path);
            return Ok(json!({
                "command": session.command,
                "session_id": session_id,
                "running": true,
                "stdout": stdout,
                "stderr": stderr,
                "truncated": stdout_truncated || stderr_truncated,
            }));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn collect_command_result(
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    stdout_path: &Path,
    stderr_path: &Path,
) -> CommandResult {
    let (stdout, stdout_truncated) = read_output_file(stdout_path);
    let (stderr, stderr_truncated) = read_output_file(stderr_path);
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
    CommandResult {
        exit_code,
        signal,
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    }
}

/// Kill a still-running child and drop its capture files; used on early
/// returns (user interrupt, wait errors) so no orphan process is left behind.
fn abort_command(child: &mut Child, stdout_path: &Path, stderr_path: &Path) {
    kill_child(child);
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
}

fn status_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

pub(crate) fn kill_child(child: &mut Child) {
    #[cfg(windows)]
    {
        let id = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/pid", &id, "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // The child is spawned as a process-group leader (process_group(0)),
        // so a group signal takes descendants down with it. Fall back to
        // killing just the child if the group is already gone.
        let pid = child.id() as i32;
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Process groups of the tool commands and hooks this process started (each
/// child is a group leader). Entries are never removed;
/// `terminate_tool_processes` signals only leaders that are still unreaped
/// children of this process, so a stale entry whose pid was reused is
/// skipped.
static TOOL_GROUPS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

pub(crate) fn track_tool_group(pid: u32) {
    if let Ok(mut groups) = TOOL_GROUPS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        groups.push(pid);
    }
}

#[cfg(test)]
pub(crate) fn is_tracked_tool_group(pid: u32) -> bool {
    TOOL_GROUPS
        .get()
        .and_then(|groups| groups.lock().ok().map(|groups| groups.contains(&pid)))
        .unwrap_or(false)
}

/// Ends every tool command this process started that is still running —
/// foreground commands and background shells (dev servers, watchers) alike —
/// with their descendants: SIGTERM to each group, SIGKILL to what is left
/// after half a second. Call when the engine process is about to exit, so
/// nothing it started outlives it.
pub fn terminate_tool_processes() {
    let pids = TOOL_GROUPS
        .get()
        .and_then(|groups| groups.lock().ok().map(|mut g| std::mem::take(&mut *g)))
        .unwrap_or_default();
    terminate_groups(pids);
}

fn terminate_groups(pids: Vec<u32>) {
    #[cfg(not(unix))]
    let _ = pids;
    #[cfg(unix)]
    {
        // Still our unreaped child: its pid (and group id) cannot have been reused.
        let running = |pid: u32| {
            let mut status = 0;
            // SAFETY: WNOHANG never blocks; the pid is one this process spawned.
            unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) == 0 }
        };
        let mut live: Vec<u32> = pids.into_iter().filter(|&pid| running(pid)).collect();
        for &pid in &live {
            // SAFETY: signals the group this process created for that child.
            unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
        }
        for _ in 0..10 {
            live.retain(|&pid| running(pid));
            if live.is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        for &pid in &live {
            // SAFETY: as above.
            unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0) };
        }
    }
}

struct ShellSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    command: String,
    started: SystemTime,
    timeout: Duration,
}

static SHELL_SESSIONS: OnceLock<Mutex<HashMap<u64, ShellSession>>> = OnceLock::new();
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
/// Final results of sessions that ended, so a later poll of the same id (a
/// parallel call, or a model that polls once more) gets the result again
/// instead of "session not found". Bounded: the oldest are dropped.
static FINISHED_SESSIONS: OnceLock<Mutex<VecDeque<(u64, Value)>>> = OnceLock::new();
const FINISHED_SESSIONS_KEPT: usize = 64;

fn finished_sessions() -> &'static Mutex<VecDeque<(u64, Value)>> {
    FINISHED_SESSIONS.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Records a session's final result; returns it unchanged.
fn remember_finished(session_id: u64, result: Value) -> Value {
    if let Ok(mut done) = finished_sessions().lock() {
        done.retain(|(id, _)| *id != session_id);
        done.push_back((session_id, result.clone()));
        while done.len() > FINISHED_SESSIONS_KEPT {
            done.pop_front();
        }
    }
    result
}

/// A session that is no longer running: its final result again, marked as
/// already reported, or a clear note when it is too old to be kept.
fn finished_or_unknown(session_id: u64, wrote_input: bool) -> Result<Value, String> {
    let kept = finished_sessions().lock().ok().and_then(|done| {
        done.iter()
            .find(|(id, _)| *id == session_id)
            .map(|(_, v)| v.clone())
    });
    match kept {
        Some(mut result) => {
            result["session_id"] = json!(session_id);
            result["running"] = json!(false);
            result["note"] = json!(if wrote_input {
                "This command had already finished; the input was not delivered. Its final result is repeated here."
            } else {
                "This command had already finished; its final result is repeated here."
            });
            Ok(result)
        }
        None => Err(unknown_session_error(session_id)),
    }
}

/// What to say about a session_id this process has no result for: a bash
/// command that finishes already carries its whole output, so there is
/// nothing to poll for.
fn unknown_session_error(session_id: u64) -> String {
    format!(
        "no running shell session {session_id}: a bash command returns its whole output in its own result, and write_stdin is only for a session_id that a still-running bash command returned. Run the command again with bash if you need it."
    )
}

fn shell_sessions() -> &'static Mutex<HashMap<u64, ShellSession>> {
    SHELL_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn project_model_output(name: &str, output: &str, cwd: &Path) -> String {
    if let Some(projected) = project_image_model_output(name, output) {
        return projected;
    }
    if let Some(projected) = project_read_model_output(name, output, cwd) {
        return projected;
    }
    if let Some(projected) = project_diff_model_output(name, output, cwd) {
        return projected;
    }
    if output.len() <= MODEL_OUTPUT_INLINE_LIMIT {
        return output.to_string();
    }

    let full_output_path = write_full_tool_output(name, output, cwd);
    let mut projected =
        serde_json::from_str::<Value>(output).unwrap_or_else(|_| json!({ "output": output }));
    let mut truncated = false;
    truncate_large_strings(&mut projected, &mut truncated);

    match &mut projected {
        Value::Object(map) => {
            map.insert("model_output_truncated".to_string(), json!(true));
            map.insert(
                "full_output_path".to_string(),
                json!(full_output_path.display().to_string()),
            );
            map.insert(
                "note".to_string(),
                json!("Large tool output was projected for the model. Inspect full_output_path if exact full output is needed."),
            );
        }
        _ => {
            projected = json!({
                "tool": name,
                "model_output_truncated": true,
                "full_output_path": full_output_path.display().to_string(),
                "output": truncate_text(output, MODEL_OUTPUT_FIELD_LIMIT),
            });
        }
    }

    let serialized = projected.to_string();
    if serialized.len() <= MODEL_OUTPUT_INLINE_LIMIT {
        return serialized;
    }

    json!({
        "tool": name,
        "model_output_truncated": true,
        "full_output_path": full_output_path.display().to_string(),
        "output": truncate_text(&serialized, MODEL_OUTPUT_INLINE_LIMIT / 2),
    })
    .to_string()
}

/// A read's text reaches the model once: as hashlines when hashline_edit
/// edits files (it needs their anchors), else as the plain content. Small
/// reads carried both, the same text twice.
pub(crate) fn read_output_for_editing(model_output: String, hashline_edit: bool) -> String {
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&model_output) else {
        return model_output;
    };
    let has = |key: &str| map.get(key).is_some_and(Value::is_string);
    if !(has("content") && has("hashlines")) {
        return model_output;
    }
    map.remove(if hashline_edit {
        "content"
    } else {
        "hashlines"
    });
    Value::Object(map).to_string()
}

fn project_read_model_output(name: &str, output: &str, cwd: &Path) -> Option<String> {
    if env::var("LYNSHEN_PROJECT_READ_MODEL_OUTPUT")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "0" | "false" | "FALSE" | "off" | "OFF"))
    {
        return None;
    }
    project_read_model_output_inner(name, output, cwd)
}

fn project_read_model_output_inner(name: &str, output: &str, cwd: &Path) -> Option<String> {
    if name != "read" {
        return None;
    }
    let mut value = serde_json::from_str::<Value>(output).ok()?;
    let content_len = value
        .get("content")
        .and_then(Value::as_str)
        .map(|text| text.len())
        .unwrap_or(0);
    let hashlines_len = value
        .get("hashlines")
        .and_then(Value::as_str)
        .map(|text| text.len())
        .unwrap_or(0);
    if content_len == 0 || hashlines_len == 0 {
        return None;
    }
    let omit_content = content_len >= READ_MODEL_CONTENT_OMIT_THRESHOLD;
    let truncate_hashlines = hashlines_len > READ_MODEL_HASHLINES_LIMIT;
    if !omit_content && !truncate_hashlines {
        return None;
    }
    let full_output_path = truncate_hashlines.then(|| write_full_tool_output(name, output, cwd));
    if let Value::Object(map) = &mut value {
        if omit_content {
            map.remove("content");
        }
        if truncate_hashlines {
            if let Some(Value::String(hashlines)) = map.get_mut("hashlines") {
                *hashlines = truncate_text(hashlines, MODEL_OUTPUT_FIELD_LIMIT);
            }
        }
        map.insert("model_output_truncated".to_string(), json!(true));
        if let Some(path) = full_output_path {
            map.insert(
                "full_output_path".to_string(),
                json!(path.display().to_string()),
            );
        }
        map.insert(
            "note".to_string(),
            json!("Large read output was projected for the model. Re-read with offset/limit when exact nearby lines or anchors are needed."),
        );
        return Some(value.to_string());
    }
    None
}

/// Replaces the inline base64 payload of an image read with a short note. The
/// actual pixels are delivered to the model as a separate image message via
/// [`image_content_item`], so keeping the base64 in the function output would
/// only waste tokens.
fn project_image_model_output(name: &str, output: &str) -> Option<String> {
    if name != "read" && !crate::android::is_tool(name) {
        return None;
    }
    let mut value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("kind").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let map = value.as_object_mut()?;
    map.remove("base64")?;
    map.insert(
        "note".to_string(),
        json!("Image content is attached as a separate message; view it directly."),
    );
    Some(value.to_string())
}

/// Builds a user image message from a `read` tool output that contains an inline
/// base64 image, so the model can actually see the pixels. Returns `None` for
/// non-image outputs.
pub fn image_content_item(output: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("kind").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let mime = value.get("mime").and_then(Value::as_str)?;
    let base64 = value.get("base64").and_then(Value::as_str)?;
    Some(json!({
        "role": "user",
        "content": [{
            "type": "input_image",
            "image_url": format!("data:{mime};base64,{base64}"),
        }],
    }))
}

/// Reads a local image file and returns an `input_image` content part for a user
/// message, or an error describing why it cannot be attached.
pub fn image_attachment_part(path: &Path) -> Result<Value, String> {
    let Some(mime) = image_mime(path) else {
        return Err(format!("{}: not a supported image", path.display()));
    };
    let metadata = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if metadata.len() > MAX_IMAGE_READ_BYTES {
        return Err(format!("{}: image is too large to attach", path.display()));
    }
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(json!({
        "type": "input_image",
        "image_url": format!("data:{mime};base64,{}", BASE64_STANDARD.encode(bytes)),
    }))
}

/// Cheap validation (no read/encode) of an image attachment path. Returns an
/// error message if the path is not an attachable image, else `None`.
pub fn image_attachment_error(path: &Path) -> Option<String> {
    if image_mime(path).is_none() {
        return Some(format!("{}: not a supported image", path.display()));
    }
    match fs::metadata(path) {
        Err(error) => Some(format!("{}: {error}", path.display())),
        Ok(metadata) if metadata.len() > MAX_IMAGE_READ_BYTES => {
            Some(format!("{}: image is too large to attach", path.display()))
        }
        Ok(_) => None,
    }
}

fn project_diff_model_output(name: &str, output: &str, cwd: &Path) -> Option<String> {
    if env::var("LYNSHEN_PROJECT_DIFF_MODEL_OUTPUT")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "0" | "false" | "FALSE" | "off" | "OFF"))
    {
        return None;
    }
    let mut value = serde_json::from_str::<Value>(output).ok()?;
    let diff = value.get("diff").and_then(Value::as_str)?.to_string();
    if diff.len() <= DIFF_MODEL_OUTPUT_INLINE_LIMIT && output.len() <= MODEL_OUTPUT_INLINE_LIMIT {
        return None;
    }
    let full_output_path = write_full_tool_output(name, output, cwd);
    let summary = summarize_unified_diff(&diff);
    if let Value::Object(map) = &mut value {
        map.insert(
            "diff".to_string(),
            json!(truncate_text(&diff, MODEL_OUTPUT_FIELD_LIMIT)),
        );
        map.insert("diff_summary".to_string(), summary);
        map.insert("model_output_truncated".to_string(), json!(true));
        map.insert(
            "full_output_path".to_string(),
            json!(full_output_path.display().to_string()),
        );
        map.insert(
            "note".to_string(),
            json!("Large diff was summarized for the model. Inspect full_output_path only if exact omitted hunks are necessary; otherwise use path-specific diff/read commands."),
        );
        return Some(value.to_string());
    }
    None
}

fn summarize_unified_diff(diff: &str) -> Value {
    let mut files = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let mut hunks = 0usize;
    for line in diff.lines() {
        if let Some(file) = diff_file_from_header(line) {
            if !files.iter().any(|existing| existing == &file) {
                files.push(file);
            }
        } else if line.starts_with("@@") {
            hunks += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    let total_files = files.len();
    if files.len() > 20 {
        files.truncate(20);
    }
    json!({
        "files_changed": total_files,
        "files_sample": files,
        "hunks": hunks,
        "additions": additions,
        "deletions": deletions,
        "diff_bytes": diff.len(),
        "diff_lines": diff.lines().count(),
    })
}

fn diff_file_from_header(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let mut parts = rest.split_whitespace();
        let _old = parts.next()?;
        let new = parts.next()?;
        return Some(strip_diff_prefix(new).to_string());
    }
    if let Some(path) = line.strip_prefix("+++ ") {
        let path = path.trim();
        if path != "/dev/null" {
            return Some(strip_diff_prefix(path).to_string());
        }
    }
    None
}

fn strip_diff_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn truncate_large_strings(value: &mut Value, truncated: &mut bool) {
    match value {
        Value::String(text) if text.len() > MODEL_OUTPUT_FIELD_LIMIT => {
            *text = truncate_text(text, MODEL_OUTPUT_FIELD_LIMIT);
            *truncated = true;
        }
        Value::Array(items) => {
            for item in items {
                truncate_large_strings(item, truncated);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                truncate_large_strings(value, truncated);
            }
        }
        _ => {}
    }
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let half = limit / 2;
    let head_end = safe_boundary(text, half);
    let tail_start = text.len() - safe_boundary_rev(text, half);
    format!(
        "{}\n\n[...model projection omitted {} bytes...]\n\n{}",
        &text[..head_end],
        text.len()
            .saturating_sub(head_end + (text.len() - tail_start)),
        &text[tail_start..]
    )
}

fn safe_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn safe_boundary_rev(text: &str, mut width: usize) -> usize {
    width = width.min(text.len());
    while width > 0 && !text.is_char_boundary(text.len() - width) {
        width -= 1;
    }
    width
}

fn write_full_tool_output(name: &str, output: &str, cwd: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(output.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let safe_name = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let dir = cwd.join(".lynshen").join("truncated-results");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{safe_name}-{}.json", &hash[..16]));
    let _ = fs::write(&path, output);
    path
}

fn apply_patch(
    args: &Value,
    cwd: &Path,
    sandbox: Option<&crate::sandbox::SandboxPolicy>,
    state: &ToolState,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Value {
    let Some(patch) = args.get("patch").and_then(Value::as_str) else {
        return json!({ "error": "missing patch" });
    };
    if patch.trim().is_empty() {
        return json!({ "error": "patch must not be empty" });
    }
    // Workspace path policy: reject the whole patch when any target escapes
    // the workspace, before anything is checked or applied.
    let targets = patch_target_paths(patch, cwd);
    for target in &targets {
        if let Err(error) = ensure_in_workspace(cwd, target)
            .and_then(|()| sandbox.map_or(Ok(()), |sandbox| sandbox.check_write(cwd, target)))
        {
            return json!({ "error": error });
        }
        // Credentials and LynShen's settings, in a workspace that holds them
        // (at home): only a write the user approves for that path, which a
        // patch never has.
        if is_guarded_write(target) {
            return json!({
                "error": format!(
                    "{} holds credentials or LynShen's settings: apply_patch does not change it; use write or edit, which ask the user",
                    target.display()
                )
            });
        }
    }
    // As for str_replace: every existing file the patch changes must have
    // been read, and be unchanged since. Files it creates need no read.
    for target in targets.iter().filter(|target| target.exists()) {
        if let Some(error) = unread_or_stale_error(
            state,
            target,
            "apply_patch requires reading each file it changes first so the patch matches bytes on disk",
        ) {
            return error;
        }
    }
    let patch = patch_for_line_endings(patch, cwd);
    let patch = patch.as_str();

    let check = run_command_events(
        "git",
        &["apply", "--check", "--whitespace=nowarn", "-"],
        &[],
        Some(patch),
        cwd,
        Duration::from_secs(30),
        emit,
    );
    if let Ok(check) = &check {
        if check.exit_code != Some(0) {
            return command_result_json("git apply --check", Ok(check.clone()));
        }
    }

    match check {
        Ok(_) => {
            // Snapshot the patch's target files (pre-apply) so /rewind can undo it.
            let checkpoint = begin_checkpoint(cwd, "auto-patch", &targets, state);
            let result = run_command_events(
                "git",
                &["apply", "--whitespace=nowarn", "-"],
                &[],
                Some(patch),
                cwd,
                Duration::from_secs(30),
                emit,
            );
            let _ = checkpoint.and_then(PendingCheckpoint::finish);
            let mut value = command_result_json("git apply", result);
            value["applied"] = json!(value.get("exit_code").and_then(Value::as_i64) == Some(0));
            if value["applied"].as_bool().unwrap_or(false) {
                for target in &targets {
                    state.mark_read(target);
                }
                // Diff only the files this patch touched, not the whole worktree.
                let mut chunks = Vec::new();
                for target in &targets {
                    if let Ok(diff) = git_diff(cwd, Some(target)) {
                        if !diff.trim().is_empty() {
                            chunks.push(diff);
                        }
                    }
                }
                value["diff"] = json!(chunks.join("\n"));
            }
            value
        }
        Err(error) => json!({ "command": "git apply --check", "error": error }),
    }
}

fn list_dir(args: &Value, cwd: &Path) -> Value {
    let path = match args.get("path").and_then(Value::as_str) {
        Some(path) => match readable_path(cwd, path) {
            Ok(path) => path,
            Err(error) => return json!({ "error": error }),
        },
        None => cwd.to_path_buf(),
    };
    let limit = match optional_usize(args, "limit") {
        Ok(limit) => limit.map(|limit| limit.max(1)),
        Err(error) => return json!({ "error": error }),
    };

    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let mut name = entry.file_name().to_string_lossy().to_string();
        if entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false)
        {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    let truncated = limit.is_some_and(|limit| names.len() > limit);
    if let Some(limit) = limit {
        names.truncate(limit);
    }

    json!({
        "path": path.display().to_string(),
        "entries": names,
        "truncated": truncated,
    })
}

fn ripgrep(args: &Value, cwd: &Path) -> Value {
    let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
        return json!({ "error": "missing pattern" });
    };
    let search_path = match args.get("path").and_then(Value::as_str) {
        Some(path) => match readable_path(cwd, path) {
            Ok(path) => path,
            Err(error) => return json!({ "error": error }),
        },
        None => cwd.to_path_buf(),
    };
    let limit = match optional_usize(args, "limit") {
        Ok(limit) => limit.map(|limit| limit.max(1)),
        Err(error) => return json!({ "error": error }),
    };
    let context_lines = match optional_usize(args, "contextLines") {
        Ok(context_lines) => context_lines.unwrap_or(0),
        Err(error) => return json!({ "error": error }),
    };

    let mut command_args = vec![
        "--line-number".to_string(),
        "--no-heading".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ];
    if args
        .get("ignoreCase")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        command_args.push("--ignore-case".to_string());
    }
    if args
        .get("literal")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        command_args.push("--fixed-strings".to_string());
    }
    if context_lines > 0 {
        command_args.push("--context".to_string());
        command_args.push(context_lines.to_string());
    }
    if let Some(glob) = args.get("glob").and_then(Value::as_str) {
        command_args.push("--glob".to_string());
        command_args.push(glob.to_string());
    }
    // -e keeps patterns that start with `-` from being parsed as flags.
    command_args.push("-e".to_string());
    command_args.push(pattern.to_string());
    command_args.push(search_path.display().to_string());

    let arg_refs = command_args.iter().map(String::as_str).collect::<Vec<_>>();
    let mut value = match run_command("rg", &arg_refs, cwd, Duration::from_secs(30)) {
        // No rg on this machine (a stock Windows has none): search in-process.
        Err(error) if error.starts_with("failed to start rg") => {
            let options = crate::search::Options {
                ignore_case: args
                    .get("ignoreCase")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                literal: args
                    .get("literal")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                context: context_lines,
                glob: args.get("glob").and_then(Value::as_str),
                timeout: Duration::from_secs(30),
            };
            let out = crate::search::search(pattern, &search_path, &options);
            command_result_json(
                "rg",
                Ok(CommandResult {
                    exit_code: Some(out.exit_code),
                    signal: None,
                    stdout: out.stdout,
                    stderr: out.stderr,
                    timed_out: out.timed_out,
                    truncated: out.truncated,
                }),
            )
        }
        result => command_result_json("rg", result),
    };
    // rg exit code 1 means "no matches", which is a valid result; 2+ is an error.
    if value.get("exit_code").and_then(Value::as_i64) == Some(1) {
        value["exit_code"] = json!(0);
        if value
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty()
        {
            value["stdout"] = json!("no matches found");
        }
    }
    if let (Some(stdout), Some(limit)) = (value.get("stdout").and_then(Value::as_str), limit) {
        let lines = stdout.lines().take(limit).collect::<Vec<_>>();
        let truncated = stdout.lines().count() > limit;
        value["stdout"] = json!(lines.join("\n"));
        value["truncated"] = json!(value["truncated"].as_bool().unwrap_or(false) || truncated);
    }
    value["path"] = json!(search_path.display().to_string());
    add_ripgrep_soft_hint(&mut value, limit.is_none());
    value
}

fn add_ripgrep_soft_hint(value: &mut Value, no_limit: bool) {
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        add_soft_hint(
            value,
            "ripgrep output was truncated",
            "Use a narrower path, glob, more specific pattern, or a limit before reading more.",
        );
    } else if no_limit
        && (stdout.lines().count() > LARGE_RIPGREP_OUTPUT_SOFT_LINES
            || stdout.len() > LARGE_RIPGREP_OUTPUT_SOFT_BYTES)
    {
        add_soft_hint(
            value,
            "large ripgrep output without a line limit",
            "Use a narrower path, glob, more specific pattern, or limit to reduce context before inspecting matches.",
        );
    }
}

fn outline_file(args: &Value, cwd: &Path) -> Value {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return json!({ "error": "missing path" });
    };
    let path = match readable_path(cwd, path) {
        Ok(path) => path,
        Err(error) => return json!({ "error": error }),
    };
    let limit = match optional_usize(args, "limit") {
        Ok(limit) => limit.unwrap_or(200).max(1),
        Err(error) => return json!({ "error": error }),
    };
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return json!({ "path": path.display().to_string(), "error": error.to_string() })
        }
    };
    let mut symbols = Vec::new();
    let mut truncated = false;
    for (index, line) in content.lines().enumerate() {
        if let Some(symbol) = symbol_from_line(line) {
            if symbols.len() >= limit {
                truncated = true;
                break;
            }
            symbols.push(json!({ "line": index + 1, "symbol": symbol }));
        }
    }
    json!({
        "path": path.display().to_string(),
        "symbols": symbols,
        "truncated": truncated,
    })
}

fn checkpoint_tool(args: &Value, cwd: &Path, state: &ToolState) -> Value {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match action {
        "create" => {
            let mut paths = Vec::new();
            for path in args
                .get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                // A copy is a read: credentials stay unreadable (a
                // workspace at home holds them).
                match workspace_path(cwd, path)
                    .and_then(|path| readable_path(cwd, &path.display().to_string()).map(|_| path))
                {
                    Ok(path) => paths.push(path),
                    Err(error) => return json!({ "error": error }),
                }
            }
            if paths.is_empty() {
                return json!({ "error": "checkpoint create requires paths" });
            }
            let name = args.get("name").and_then(Value::as_str).unwrap_or("manual");
            match create_checkpoint(cwd, name, &paths, state) {
                Ok(meta) => meta,
                Err(error) => json!({ "error": error.to_string() }),
            }
        }
        "list" => match list_checkpoints(cwd) {
            Ok(items) => json!({ "checkpoints": items }),
            Err(error) => json!({ "error": error.to_string() }),
        },
        "restore" => {
            let Some(id) = args.get("id").and_then(Value::as_str) else {
                return json!({ "error": "checkpoint restore requires id" });
            };
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            match restore_checkpoint(cwd, id, force, state) {
                Ok(value) => value,
                Err(error) => json!({ "error": error.to_string() }),
            }
        }
        _ => json!({ "error": "checkpoint action must be create, list, or restore" }),
    }
}

pub(crate) fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

fn decode_text_bytes(bytes: &[u8]) -> Option<(String, &'static str)> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16(&bytes[2..], true).map(|text| (text, "utf-16le"));
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(&bytes[2..], false).map(|text| (text, "utf-16be"));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    String::from_utf8(bytes.to_vec())
        .ok()
        .map(|text| (text, "utf-8"))
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes(*chunk)
            } else {
                u16::from_be_bytes(*chunk)
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).ok()
}

fn symbol_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let prefixes = [
        "fn ",
        "pub fn ",
        "struct ",
        "pub struct ",
        "enum ",
        "pub enum ",
        "trait ",
        "pub trait ",
        "impl ",
        "func ",
        "type ",
        "class ",
        "export function ",
        "function ",
        "export class ",
    ];
    prefixes
        .iter()
        .find(|prefix| trimmed.starts_with(**prefix))
        .map(|_| trimmed.trim_end().to_string())
}

fn normalize_path_key(path: &Path) -> String {
    let value = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn file_fingerprint(path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let bytes = fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// The error for an edit to a file this agent has not read, or whose content
/// changed since it last read it; None when the edit may proceed.
fn unread_or_stale_error(state: &ToolState, path: &Path, not_read: &str) -> Option<Value> {
    let error = match state.read_check(path) {
        ReadCheck::Fresh => return None,
        ReadCheck::NotRead => not_read,
        ReadCheck::Stale => "file changed on disk since you last read it (another agent, the user, or a command modified it); read it again and redo the edit against the current content",
    };
    Some(json!({ "path": path.display().to_string(), "error": error }))
}

/// Per-engine tool state. One per engine (shared with its subagents), so
/// several engines in one process never see each other's background shell
/// sessions started by `bash`: `write_stdin` may only reach the engine's own.
///
/// `reads` is per agent (`for_subagent` starts a fresh one): the content
/// fingerprint of each file as this agent last read or wrote it. Editing an
/// existing file requires having read it, and fails once anyone else (another
/// agent, the user, a command) has changed it since — so parallel agents in
/// one tree cannot silently overwrite each other.
#[derive(Clone, Default)]
pub struct ToolState {
    inner: Arc<Mutex<ToolStateInner>>,
    reads: Arc<Mutex<HashMap<String, u64>>>,
    /// A worktree subagent's (worktree, project): its shell commands start
    /// inside the worktree and write nowhere else (see `wrap_confined`).
    confine: Option<Arc<(PathBuf, PathBuf)>>,
    /// Paths outside the workspace this one call may write: approved by the
    /// user, or run unasked in full-access (`allowing_writes`).
    outside_writes: Option<Arc<Vec<PathBuf>>>,
}

enum ReadCheck {
    Fresh,
    NotRead,
    Stale,
}

#[derive(Default)]
struct ToolStateInner {
    shells: HashSet<u64>,
    /// When set, shell commands run in this OS sandbox and file writes are
    /// checked against it.
    sandbox: Option<crate::sandbox::SandboxPolicy>,
    /// The shell commands run in (`shell` in config.json); None runs them
    /// as before the setting (`CommandShell::legacy`).
    shell: Option<Arc<crate::shell::CommandShell>>,
    /// Gateway settings for the web tools; None fetches locally and leaves
    /// web_search unavailable.
    web: Option<crate::web::WebTools>,
    /// Endpoint and model for generate_image, or why it is unavailable;
    /// None until the engine configures it.
    images: Option<Result<crate::images::ImageTools, String>>,
    /// The android-use binary the phone tools run, or why they are off;
    /// None until the engine configures it.
    android: Option<Result<PathBuf, String>>,
    /// Messages for the session from work that outlived its tool call (an
    /// interrupted generate_image saving its result), drained into Info events.
    notices: Vec<String>,
    /// The session the engine is in, recorded in each checkpoint so a rewind
    /// restores only this session's edits.
    session_id: Option<String>,
}

impl Drop for ToolStateInner {
    /// The engine owning these background shells is gone (a daemon session
    /// closed, say): end them rather than leave them running unattended.
    fn drop(&mut self) {
        if self.shells.is_empty() {
            return;
        }
        let Ok(mut sessions) = shell_sessions().lock() else {
            return;
        };
        for id in self.shells.drain() {
            if let Some(mut session) = sessions.remove(&id) {
                kill_child(&mut session.child);
                let _ = fs::remove_file(&session.stdout_path);
                let _ = fs::remove_file(&session.stderr_path);
            }
        }
    }
}

impl ToolState {
    pub fn set_sandbox(&self, sandbox: Option<crate::sandbox::SandboxPolicy>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.sandbox = sandbox;
        }
    }

    pub fn set_shell(&self, shell: Option<Arc<crate::shell::CommandShell>>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.shell = shell;
        }
    }

    /// The shell this agent's commands run in.
    pub fn shell(&self) -> Arc<crate::shell::CommandShell> {
        static LEGACY: OnceLock<Arc<crate::shell::CommandShell>> = OnceLock::new();
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.shell.clone())
            .unwrap_or_else(|| {
                Arc::clone(LEGACY.get_or_init(|| Arc::new(crate::shell::CommandShell::legacy())))
            })
    }

    /// The sandbox this agent runs in. A confined agent's has no extra
    /// read-write directories: only its worktree (and temp and package
    /// caches) are writable.
    pub fn sandbox(&self) -> Option<crate::sandbox::SandboxPolicy> {
        let sandbox = self
            .inner
            .lock()
            .ok()
            .and_then(|inner| inner.sandbox.clone());
        match (sandbox, &self.confine) {
            (Some(sandbox), Some(_)) => Some(crate::sandbox::SandboxPolicy {
                writable_dirs: Vec::new(),
                ..sandbox
            }),
            (sandbox, _) => sandbox,
        }
    }

    /// The worktree a confined agent works in.
    pub(crate) fn confine_root(&self) -> Option<&Path> {
        self.confine.as_ref().map(|confine| confine.0.as_path())
    }

    /// This state for one call that may write `paths` outside the
    /// workspace: the user approved it (or full-access ran it unasked).
    pub(crate) fn allowing_writes(&self, paths: Vec<PathBuf>) -> Self {
        Self {
            outside_writes: Some(Arc::new(
                paths
                    .iter()
                    .map(|path| normalize_for_policy(path))
                    .collect(),
            )),
            ..self.clone()
        }
    }

    /// Whether this call may write `path` outside the workspace: an
    /// approved file, or a new file right in an approved directory (where
    /// `generate_image` saves the images it names itself).
    fn may_write_outside(&self, path: &Path) -> bool {
        let path = normalize_for_policy(path);
        self.outside_writes.as_ref().is_some_and(|paths| {
            paths.contains(&path)
                || path
                    .parent()
                    .is_some_and(|parent| paths.iter().any(|dir| dir == parent && dir.is_dir()))
                    && !path.exists()
        })
    }

    pub fn set_web(&self, web: Option<crate::web::WebTools>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.web = web;
        }
    }

    pub fn web(&self) -> Option<crate::web::WebTools> {
        self.inner.lock().ok().and_then(|inner| inner.web.clone())
    }

    pub fn set_session_id(&self, session_id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.session_id = Some(session_id.to_string());
        }
    }

    pub(crate) fn session_id(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.session_id.clone())
    }

    /// web_search is offered whenever the web tools are configured: the
    /// local engine needs nothing, only a gateway engine needs a login.
    pub fn web_search_enabled(&self) -> bool {
        self.web().is_some_and(|web| web.search_available())
    }

    pub(crate) fn set_images(&self, images: Result<crate::images::ImageTools, String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.images = Some(images);
        }
    }

    /// generate_image's settings, or the reason it is not offered.
    pub(crate) fn images(&self) -> Result<crate::images::ImageTools, String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.images.clone())
            .unwrap_or_else(|| Err(crate::images::UNAVAILABLE.to_string()))
    }

    pub(crate) fn set_android(&self, android: Result<PathBuf, String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.android = Some(android);
        }
    }

    /// The au the phone tools run, or why they are not offered.
    pub(crate) fn android(&self) -> Result<PathBuf, String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.android.clone())
            .unwrap_or_else(|| crate::android::availability(false))
    }

    pub(crate) fn push_notice(&self, notice: String) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.notices.push(notice);
        }
    }

    pub(crate) fn take_notices(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.notices))
            .unwrap_or_default()
    }

    /// The same engine state with an empty read record, for a subagent: it
    /// must read files itself before editing them.
    pub(crate) fn for_subagent(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            reads: Arc::default(),
            confine: self.confine.clone(),
            outside_writes: None,
        }
    }

    /// `for_subagent` for an agent in its own worktree `root` of `project`:
    /// its shell is confined to the worktree.
    pub(crate) fn confined_to(&self, root: PathBuf, project: PathBuf) -> Self {
        Self {
            inner: self.inner.clone(),
            reads: Arc::default(),
            confine: Some(Arc::new((root, project))),
            outside_writes: None,
        }
    }

    fn mark_read(&self, path: &Path) {
        let key = normalize_path_key(path);
        let fingerprint = file_fingerprint(path);
        if let Ok(mut reads) = self.reads.lock() {
            match fingerprint {
                Some(fingerprint) => reads.insert(key, fingerprint),
                None => reads.remove(&key),
            };
        }
    }

    fn read_check(&self, path: &Path) -> ReadCheck {
        let recorded = self
            .reads
            .lock()
            .ok()
            .and_then(|reads| reads.get(&normalize_path_key(path)).copied());
        match recorded {
            None => ReadCheck::NotRead,
            Some(recorded) if file_fingerprint(path) == Some(recorded) => ReadCheck::Fresh,
            Some(_) => ReadCheck::Stale,
        }
    }

    /// A background shell this engine started is still there: dropping the
    /// engine would end it (see `ToolStateInner`'s `Drop`).
    pub fn has_live_shells(&self) -> bool {
        let ids: Vec<u64> = match self.inner.lock() {
            Ok(inner) => inner.shells.iter().copied().collect(),
            Err(_) => return true,
        };
        !ids.is_empty()
            && shell_sessions()
                .lock()
                .map(|sessions| ids.iter().any(|id| sessions.contains_key(id)))
                .unwrap_or(true)
    }

    fn own_shell(&self, session_id: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.shells.insert(session_id);
        }
    }

    fn owns_shell(&self, session_id: u64) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.shells.contains(&session_id))
            .unwrap_or(false)
    }
}

/// Files a unified diff changes, creates or deletes, for the workspace and
/// read checks and the pre-apply snapshot (see `patch_file_names`).
fn patch_target_paths(patch: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for name in patch_file_names(patch) {
        let abs = resolve_path(cwd, &name);
        if !paths.contains(&abs) {
            paths.push(abs);
        }
    }
    paths
}

/// The files a unified diff changes, creates or deletes, as it names them
/// (`a/`/`b/` dropped), each once: the paths of its `---`/`+++` headers
/// (`/dev/null` aside), of git's `rename from`, `rename to` and `copy to`
/// lines, and of a `diff --git a/x b/x` header (a mode change or an empty
/// file has nothing else). A copy's source does not change and is left out.
/// Hunk lines are skipped by their header's line counts, as git reads them,
/// so a removed line that starts like a header is not taken for one.
pub(crate) fn patch_file_names(patch: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let (mut old_left, mut new_left) = (0usize, 0usize);
    for line in patch.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if old_left > 0 || new_left > 0 {
            count_hunk_line(line, &mut old_left, &mut new_left);
            continue;
        }
        let name = if line.starts_with("@@") {
            (_, old_left, new_left) = hunk_header(line).unwrap_or_default();
            None
        } else if let Some(rest) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            patch_header_name(rest)
        } else if let Some(rest) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("rename to "))
            .or_else(|| line.strip_prefix("copy to "))
        {
            Some(rest.trim())
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            git_header_path(rest)
        } else {
            None
        };
        if let Some(name) = name.filter(|name| !names.iter().any(|known| known == name)) {
            names.push(name.to_string());
        }
    }
    names
}

/// The file a `---`/`+++` header names; None for `/dev/null`.
fn patch_header_path(rest: &str, cwd: &Path) -> Option<PathBuf> {
    patch_header_name(rest).map(|name| resolve_path(cwd, name))
}

/// The name in a `---`/`+++` header, `a/`/`b/` dropped; None for
/// `/dev/null`. `diff -u` puts a tab and a timestamp after it.
fn patch_header_name(rest: &str) -> Option<&str> {
    let rest = rest.split('\t').next().unwrap_or(rest).trim();
    if rest == "/dev/null" {
        return None;
    }
    Some(
        rest.strip_prefix("a/")
            .or_else(|| rest.strip_prefix("b/"))
            .unwrap_or(rest),
    )
}

/// The path of a `diff --git a/x b/x` header whose two sides are the same
/// file; a rename or a copy names its paths on lines of their own.
fn git_header_path(rest: &str) -> Option<&str> {
    let rest = rest.strip_prefix("a/")?;
    let half = rest.len().checked_sub(3)? / 2;
    let (old, new) = (rest.get(..half)?, rest.get(half..)?);
    (new.strip_prefix(" b/")? == old && !old.is_empty()).then_some(old)
}

/// Counts one line of a hunk against the old and new lines it has left.
fn count_hunk_line(line: &str, old_left: &mut usize, new_left: &mut usize) {
    match line.as_bytes().first() {
        Some(b'+') => *new_left = new_left.saturating_sub(1),
        Some(b'-') => *old_left = old_left.saturating_sub(1),
        Some(b'\\') => {}
        _ => {
            *old_left = old_left.saturating_sub(1);
            *new_left = new_left.saturating_sub(1);
        }
    }
}

/// `patch` with the hunk lines for each file that has CRLF line breaks
/// given the endings git apply has to see: a context or removed line the
/// ending of the file line it stands for, an added line the file's main
/// ending (`uses_crlf`). A line followed by a "\ No newline" marker keeps its
/// ending, and an empty context line gets the space git expects before a
/// CR. A file without CRLF gets its hunks as they are.
fn patch_for_line_endings(patch: &str, cwd: &Path) -> String {
    let lines = patch.split_inclusive('\n').collect::<Vec<_>>();
    let mut out = String::with_capacity(patch.len() + patch.len() / 16);
    // Per line of the current file whether it ends with CRLF, and its main
    // ending; None for a file without CRLF.
    let mut endings: Option<(Vec<bool>, bool)> = None;
    let mut old_line = 0usize;
    let (mut old_left, mut new_left) = (0usize, 0usize);
    for (index, line) in lines.iter().enumerate() {
        let text = line.strip_suffix('\n').unwrap_or(line);
        let text = text.strip_suffix('\r').unwrap_or(text);
        if old_left == 0 && new_left == 0 {
            if let Some(rest) = text.strip_prefix("--- ") {
                endings = patch_header_path(rest, cwd)
                    .and_then(|path| fs::read_to_string(path).ok())
                    .filter(|content| content.contains("\r\n"))
                    .map(|content| {
                        let lines = content
                            .split_inclusive('\n')
                            .map(|line| line.ends_with("\r\n"))
                            .collect();
                        (lines, uses_crlf(&content))
                    });
            } else if text.starts_with("@@") {
                let (old_start, old_count, new_count) = hunk_header(text).unwrap_or_default();
                (old_left, new_left) = (old_count, new_count);
                old_line = old_start.saturating_sub(1);
            }
            out.push_str(line);
            continue;
        }
        let kind = text.as_bytes().first().copied();
        count_hunk_line(text, &mut old_left, &mut new_left);
        let Some((file_lines, main_crlf)) = &endings else {
            out.push_str(line);
            continue;
        };
        let crlf = match kind {
            Some(b'+') => *main_crlf,
            Some(b'\\') => false,
            _ => {
                old_line += 1;
                file_lines.get(old_line - 1).copied().unwrap_or(*main_crlf)
            }
        };
        let keeps_ending = kind == Some(b'\\')
            || !line.ends_with('\n')
            || lines
                .get(index + 1)
                .is_some_and(|next| next.starts_with('\\'));
        if keeps_ending {
            out.push_str(line);
            continue;
        }
        out.push_str(if text.is_empty() { " " } else { text });
        out.push_str(if crlf { "\r\n" } else { "\n" });
    }
    out
}

/// The old start line and the old and new line counts of a hunk header
/// `@@ -a,b +c,d @@`; a range without a count has one line.
fn hunk_header(header: &str) -> Option<(usize, usize, usize)> {
    let mut ranges = header.split_whitespace().skip(1);
    let range = |range: &str, sign: char| -> Option<(usize, usize)> {
        let range = range.strip_prefix(sign)?;
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(ranges.next()?, '-')?;
    let (_, new_count) = range(ranges.next()?, '+')?;
    Some((old_start, old_count, new_count))
}

/// A checkpoint whose snapshot is on disk: `begin_checkpoint` writes the
/// files as they are before an edit, with the session that took it, so a
/// crash during the edit loses nothing. `finish` adds, once the edit has
/// run, a hash of what it left in each file (`after`), so a rewind can tell
/// whether anyone changed a file after this session did.
pub(crate) struct PendingCheckpoint {
    saved: SavedCheckpoint,
    checkpoint: Value,
}

/// A checkpoint file and the files it captured, with the `after` each one
/// got (see `after_value`).
struct SavedCheckpoint {
    path: PathBuf,
    files: Vec<(PathBuf, Option<Value>)>,
}

pub(crate) fn begin_checkpoint(
    cwd: &Path,
    name: &str,
    paths: &[PathBuf],
    state: &ToolState,
) -> io::Result<PendingCheckpoint> {
    let id = format!("cp-{}", now_nanos());
    let mut files = Vec::new();
    let mut captured = Vec::new();
    let mut bytes = 0usize;
    for path in paths {
        let abs = resolve_existing_or_future(cwd, path);
        if !is_inside(cwd, &abs) {
            continue;
        }
        let rel = diff_label(cwd, &abs);
        let content = match fs::read_to_string(&abs) {
            Ok(content) => {
                bytes += content.len();
                Value::String(content)
            }
            // Existing but unreadable (e.g. non-UTF-8): mark it so restore
            // skips it instead of misreading null as "did not exist yet".
            Err(_) if abs.exists() => json!({ "unreadable": true }),
            Err(_) => Value::Null,
        };
        files.push(json!({ "path": rel, "content": content }));
        captured.push((abs, None));
    }
    let mut checkpoint = json!({
        "id": id,
        "name": name,
        "created_at": now_secs(),
        "files": files,
        "bytes": bytes,
    });
    if let Some(session) = state.session_id() {
        checkpoint["session_id"] = json!(session);
    }
    let dir = checkpoint_dir(cwd);
    fs::create_dir_all(&dir)?;
    // Keep .lynshen out of the user's git history.
    let ignore = cwd.join(".lynshen").join(".gitignore");
    if !ignore.exists() {
        let _ = fs::write(&ignore, "*\n");
    }
    let path = dir.join(format!("{id}.json"));
    write_checkpoint(&path, &checkpoint)?;
    prune_checkpoints(&dir);
    Ok(PendingCheckpoint {
        saved: SavedCheckpoint {
            path,
            files: captured,
        },
        checkpoint,
    })
}

impl PendingCheckpoint {
    /// Records `after` for each file; returns what the checkpoint tool
    /// reports. The checkpoint goes to the thread's collector, if any, so
    /// the tool call's post_tool_use hooks can update `after`
    /// (`EditCheckpoints::around_hooks`).
    pub(crate) fn finish(mut self) -> io::Result<Value> {
        let files = self.checkpoint["files"].as_array_mut();
        for (file, (abs, after)) in files.into_iter().flatten().zip(&mut self.saved.files) {
            *after = after_value(abs);
            if let Some(after) = after {
                file["after"] = after.clone();
            }
        }
        write_checkpoint(&self.saved.path, &self.checkpoint)?;
        let report = json!({
            "id": self.checkpoint["id"],
            "name": self.checkpoint["name"],
            "files": self.saved.files.len(),
            "bytes": self.checkpoint["bytes"],
        });
        COLLECTED.with(|collected| {
            if let Some(collected) = collected.borrow_mut().as_mut() {
                collected.push(self.saved);
            }
        });
        Ok(report)
    }
}

impl SavedCheckpoint {
    /// The files that still hold what the edit left (`after`).
    fn unchanged_files(&self) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, (abs, after))| after.is_some() && after_value(abs) == *after)
            .map(|(index, _)| index)
            .collect()
    }

    /// Stores what `files` hold now as their `after`, where that changed.
    /// A checkpoint pruned meanwhile is left gone.
    fn record_after(self, files: &[usize]) -> io::Result<()> {
        let changed = files
            .iter()
            .map(|&index| (index, after_value(&self.files[index].0)))
            .filter(|(index, now)| *now != self.files[*index].1)
            .collect::<Vec<_>>();
        if changed.is_empty() {
            return Ok(());
        }
        let mut checkpoint = serde_json::from_str::<Value>(&fs::read_to_string(&self.path)?)
            .map_err(io::Error::other)?;
        for (index, now) in changed {
            let Some(file) = checkpoint["files"].get_mut(index) else {
                continue;
            };
            match now {
                Some(after) => file["after"] = after,
                None => {
                    if let Some(file) = file.as_object_mut() {
                        file.remove("after");
                    }
                }
            }
        }
        write_checkpoint(&self.path, &checkpoint)
    }
}

thread_local! {
    /// The checkpoints finished on this thread while a `CheckpointCollector`
    /// collects; None when none does.
    static COLLECTED: RefCell<Option<Vec<SavedCheckpoint>>> = const { RefCell::new(None) };
}

/// Collects the checkpoints the edits on this thread finish until `take`:
/// those of one tool call, whose post_tool_use hooks run after it. Each
/// thread collects its own, so a deferred action running next to a turn
/// never takes the turn's.
pub(crate) struct CheckpointCollector {
    /// What an enclosing collector on this thread had, put back at the
    /// end; None once ended.
    outer: Option<Option<Vec<SavedCheckpoint>>>,
}

pub(crate) fn collect_checkpoints() -> CheckpointCollector {
    CheckpointCollector {
        outer: Some(COLLECTED.with(|collected| collected.replace(Some(Vec::new())))),
    }
}

impl CheckpointCollector {
    pub(crate) fn take(mut self) -> EditCheckpoints {
        EditCheckpoints(self.end())
    }

    fn end(&mut self) -> Vec<SavedCheckpoint> {
        match self.outer.take() {
            Some(outer) => COLLECTED
                .with(|collected| collected.replace(outer))
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }
}

impl Drop for CheckpointCollector {
    fn drop(&mut self) {
        self.end();
    }
}

/// The checkpoints one tool call's edits finished (see
/// `collect_checkpoints`).
#[derive(Default)]
pub(crate) struct EditCheckpoints(Vec<SavedCheckpoint>);

impl EditCheckpoints {
    /// Runs `hooks`, post_tool_use hooks of the call that may rewrite what
    /// its edits wrote (a formatter), and records what they leave as each
    /// file's `after`: only for a file that still holds what its edit left
    /// when they start, so a change someone else made meanwhile (the user
    /// saving during a long command) is never taken for this session's.
    pub(crate) fn around_hooks(self, hooks: impl FnOnce()) {
        let unchanged = self
            .0
            .iter()
            .map(SavedCheckpoint::unchanged_files)
            .collect::<Vec<_>>();
        hooks();
        for (checkpoint, files) in self.0.into_iter().zip(unchanged) {
            let _ = checkpoint.record_after(&files);
        }
    }
}

/// A checkpoint file's `after` for `abs`: its content hash, null when it
/// does not exist, None when it cannot be read (a rewind then treats any
/// content as a later change).
fn after_value(abs: &Path) -> Option<Value> {
    content_hash(abs).ok().map(|hash| json!(hash))
}

/// Writes a checkpoint file whole or not at all: through a temporary file
/// (not `.json`, so nothing reads it as a checkpoint) and a rename.
fn write_checkpoint(path: &Path, checkpoint: &Value) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        format!(
            "{}\n",
            serde_json::to_string_pretty(checkpoint).map_err(io::Error::other)?
        ),
    )?;
    fs::rename(&temporary, path)
}

/// A checkpoint of `paths` as they are now, with nothing edited after it.
pub(crate) fn create_checkpoint(
    cwd: &Path,
    name: &str,
    paths: &[PathBuf],
    state: &ToolState,
) -> io::Result<Value> {
    begin_checkpoint(cwd, name, paths, state)?.finish()
}

/// A stable hash of a file's bytes, stored in checkpoints; None when the
/// file does not exist.
fn content_hash(path: &Path) -> io::Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes_hash(&bytes))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn bytes_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Bound checkpoint disk use: keep the newest checkpoints within both a count
/// and a total-size budget, deleting the oldest beyond either limit.
const MAX_CHECKPOINTS: usize = 200;
const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;

fn prune_checkpoints(dir: &Path) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(u128, PathBuf, u64)> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let nanos = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("cp-"))
            .and_then(|n| n.parse::<u128>().ok())
            .unwrap_or(0);
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push((nanos, path, size));
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.0)); // newest first
    let mut kept = 0usize;
    let mut bytes = 0u64;
    for (_, path, size) in files {
        kept += 1;
        bytes = bytes.saturating_add(size);
        if kept > MAX_CHECKPOINTS || bytes > MAX_CHECKPOINT_BYTES {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn list_checkpoints(cwd: &Path) -> io::Result<Vec<Value>> {
    let dir = checkpoint_dir(cwd);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        items.push(json!({
            "id": value.get("id").cloned().unwrap_or_default(),
            "name": value.get("name").cloned().unwrap_or_default(),
            "created_at": value.get("created_at").cloned().unwrap_or_default(),
            "files": value.get("files").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "bytes": value.get("bytes").cloned().unwrap_or_default(),
        }));
    }
    items.sort_by_key(|item| item.get("created_at").and_then(Value::as_u64).unwrap_or(0));
    items.reverse();
    Ok(items)
}

/// A stored checkpoint, as a restore reads it.
struct StoredCheckpoint {
    id: String,
    nanos: u128,
    created_at: u64,
    /// The session that took it; None for one taken outside a session or
    /// by a version that did not record sessions.
    session: Option<String>,
    files: Vec<Value>,
}

/// Every checkpoint of the workspace, oldest first (by nanosecond id).
fn load_checkpoints(cwd: &Path) -> io::Result<Vec<StoredCheckpoint>> {
    let dir = checkpoint_dir(cwd);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut checkpoints = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        checkpoints.push(StoredCheckpoint {
            nanos: id
                .strip_prefix("cp-")
                .and_then(|n| n.parse::<u128>().ok())
                .unwrap_or(0),
            created_at: value.get("created_at").and_then(Value::as_u64).unwrap_or(0),
            session: value
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            files: value
                .get("files")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            id,
        });
    }
    checkpoints.sort_by_key(|checkpoint| checkpoint.nanos);
    Ok(checkpoints)
}

/// A checkpoint this session took. One without a session (from an older
/// version, or taken outside a session) never counts: it cannot be told
/// apart from another session's.
fn is_own_checkpoint(checkpoint: &StoredCheckpoint, session: Option<&str>) -> bool {
    checkpoint.session.is_some() && checkpoint.session.as_deref() == session
}

/// One file a restore would put back, from the checkpoints in its range.
struct RestoreFile {
    rel: String,
    /// Its content in the earliest of this session's checkpoints: text,
    /// null for "did not exist", or the unreadable marker. None when only
    /// checkpoints without a session captured it: there is no content known
    /// to be from before this session's edits.
    content: Option<Value>,
    /// `after` of this session's latest checkpoint of it: what that edit
    /// left there. None when the checkpoint records none.
    after: Option<Value>,
    /// A checkpoint of it by another session, or without a session,
    /// follows this session's first.
    edited_by_other: bool,
}

/// The files this session's checkpoints in `range` (oldest first) captured,
/// and those only checkpoints without a session captured (reported, never
/// restored), limited to `only` when given. Edits other sessions made
/// before this session's first checkpoint of a file are in its content.
fn restore_files(
    range: &[StoredCheckpoint],
    session: Option<&str>,
    only: Option<&HashSet<String>>,
) -> Vec<RestoreFile> {
    let mut files: Vec<RestoreFile> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for checkpoint in range {
        let own = is_own_checkpoint(checkpoint, session);
        for file in &checkpoint.files {
            let Some(rel) = file.get("path").and_then(Value::as_str) else {
                continue;
            };
            if only.is_some_and(|only| !only.contains(rel)) {
                continue;
            }
            let content = || file.get("content").cloned().unwrap_or(Value::Null);
            match index.get(rel).map(|&at| &mut files[at]) {
                Some(found) if found.content.is_none() => {
                    if own {
                        found.content = Some(content());
                        found.after = file.get("after").cloned();
                    }
                }
                Some(found) if own => found.after = file.get("after").cloned(),
                Some(found) => found.edited_by_other = true,
                None if own || checkpoint.session.is_none() => {
                    index.insert(rel.to_string(), files.len());
                    files.push(RestoreFile {
                        rel: rel.to_string(),
                        content: own.then(content),
                        after: if own {
                            file.get("after").cloned()
                        } else {
                            None
                        },
                        edited_by_other: false,
                    });
                }
                None => {}
            }
        }
    }
    files
}

/// Why restoring `file` to `content` would overwrite someone else's
/// change, if it would. `force` accepts a change since this session's last
/// edit (one of its own commands, say), never another session's
/// checkpointed edit.
fn restore_conflict(
    abs: &Path,
    content: &Value,
    file: &RestoreFile,
    force: bool,
) -> Option<&'static str> {
    let Ok(current) = content_hash(abs) else {
        return Some("it cannot be read to check for later changes");
    };
    if current == content.as_str().map(|text| bytes_hash(text.as_bytes())) {
        return None;
    }
    if file.edited_by_other {
        return Some("another session edited it after this session did");
    }
    if force {
        return None;
    }
    match &file.after {
        Some(after) if after.as_str() == current.as_deref() => None,
        Some(_) => Some(
            "changed since this session last edited it (by another session, the user, or a command)",
        ),
        None => Some(
            "its checkpoint does not record what the edit left (the edit did not finish, or the file could not be read), so a later change cannot be ruled out",
        ),
    }
}

/// Puts `files` back, leaving alone (as `conflicts`) any whose restore
/// would overwrite a later change by someone else.
fn apply_restore(
    cwd: &Path,
    files: Vec<RestoreFile>,
    force: bool,
    state: &ToolState,
) -> io::Result<Value> {
    let mut skipped = Vec::new();
    let mut conflicts = Vec::new();
    let mut changes = Vec::new();
    for file in files {
        let abs = resolve_path(cwd, &file.rel);
        if !is_inside(cwd, &abs) {
            continue;
        }
        let Some(content) = &file.content else {
            conflicts.push(json!({
                "path": file.rel,
                "reason": "only a checkpoint without a session (made by an older version) captured it, so it cannot be told which edits were this session's",
            }));
            continue;
        };
        if !content.is_string() && !content.is_null() {
            // Unreadable at checkpoint time: never delete, just report it.
            skipped.push(file.rel);
        } else if let Some(reason) = restore_conflict(&abs, content, &file, force) {
            conflicts.push(json!({ "path": file.rel, "reason": reason }));
        } else {
            changes.push((file.rel, abs, content.clone()));
        }
    }
    // The restore is this session's edit too: a later rewind compares with
    // what it wrote.
    let paths = changes
        .iter()
        .map(|(_, abs, _)| abs.clone())
        .collect::<Vec<_>>();
    let checkpoint = if paths.is_empty() {
        None
    } else {
        begin_checkpoint(cwd, "auto-restore", &paths, state).ok()
    };
    let mut restored = Vec::new();
    let mut removed = Vec::new();
    let mut outcome = Ok(());
    for (rel, abs, content) in changes {
        let step = match content.as_str() {
            Some(text) => abs
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::write(&abs, text))
                .map(|()| {
                    state.mark_read(&abs);
                    restored.push(rel);
                }),
            None if abs.exists() => fs::remove_file(&abs).map(|()| removed.push(rel)),
            None => Ok(()),
        };
        if step.is_err() {
            outcome = step;
            break;
        }
    }
    if let Some(checkpoint) = checkpoint {
        let _ = checkpoint.finish();
    }
    outcome?;
    let mut warnings = Vec::new();
    if !skipped.is_empty() {
        warnings
            .push("some files could not be captured at checkpoint time and were left untouched");
    }
    if !conflicts.is_empty() {
        warnings.push("some files were left as they are so as not to overwrite changes that may not be this session's; see conflicts");
    }
    let mut result = json!({ "restored": restored, "removed": removed, "conflicts": conflicts });
    if !skipped.is_empty() {
        result["skipped"] = json!(skipped);
    }
    if !warnings.is_empty() {
        result["warning"] = json!(warnings.join("; "));
    }
    Ok(result)
}

/// Whether a call is a `checkpoint` restore with `force`, which asks for
/// approval in more modes than a plain restore
/// (`ApprovalMode::requires_approval_for_forced_restore`).
pub(crate) fn is_forced_restore(name: &str, arguments: &str) -> bool {
    name == "checkpoint"
        && serde_json::from_str::<Value>(arguments).is_ok_and(|args| {
            args.get("action").and_then(Value::as_str) == Some("restore")
                && args.get("force").and_then(Value::as_bool) == Some(true)
        })
}

/// Restore the files of checkpoint `id`, which must be this session's, to
/// their content then. `force` also overwrites files changed since this
/// session last edited them (see `restore_conflict`).
pub(crate) fn restore_checkpoint(
    cwd: &Path,
    id: &str,
    force: bool,
    state: &ToolState,
) -> io::Result<Value> {
    let checkpoints = load_checkpoints(cwd)?;
    let Some(start) = checkpoints
        .iter()
        .position(|checkpoint| checkpoint.id == id)
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no checkpoint {id}"),
        ));
    };
    let session = state.session_id();
    if !is_own_checkpoint(&checkpoints[start], session.as_deref()) {
        return Err(io::Error::other(format!(
            "checkpoint {id} was taken by another session, or by an older version that did not record sessions; only this session's checkpoints can be restored"
        )));
    }
    let only = checkpoints[start]
        .files
        .iter()
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let files = restore_files(&checkpoints[start..], session.as_deref(), Some(&only));
    let mut result = apply_restore(cwd, files, force, state)?;
    result["id"] = json!(id);
    // `force` is offered here rather than in the tool definition: only a
    // conflict makes it relevant.
    if !force
        && result["conflicts"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
    {
        result["suggestion"] = json!(
            "If a listed change is your own (a command you ran after your last edit), restore again with \"force\": true to overwrite it. Another session's edits are never overwritten."
        );
    }
    Ok(result)
}

/// Reconstruct the working tree as of `t` (a turn's creation time) for this
/// session's edits: every file one of its checkpoints at or after `t`
/// captured gets the content of the earliest such checkpoint (its pre-edit
/// state), or is deleted when it did not exist yet. Files only other
/// sessions touched are left alone, and so is a file changed since this
/// session last edited it (listed under `conflicts`).
pub(crate) fn restore_to_timestamp(cwd: &Path, t: u64, state: &ToolState) -> io::Result<Value> {
    let range = load_checkpoints(cwd)?
        .into_iter()
        .filter(|checkpoint| checkpoint.created_at >= t)
        .collect::<Vec<_>>();
    let files = restore_files(&range, state.session_id().as_deref(), None);
    apply_restore(cwd, files, false, state)
}

/// The files a restore left alone and why, for the session's message; None
/// when there were none.
pub(crate) fn restore_conflicts_note(result: &Value) -> Option<String> {
    let conflicts = result
        .get("conflicts")
        .and_then(Value::as_array)
        .filter(|conflicts| !conflicts.is_empty())?;
    let items = conflicts
        .iter()
        .map(|conflict| {
            format!(
                "{} ({})",
                conflict["path"].as_str().unwrap_or_default(),
                conflict["reason"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    Some(format!(
        "left {} file(s) as they are: {}",
        conflicts.len(),
        items.join("; ")
    ))
}

fn checkpoint_dir(cwd: &Path) -> PathBuf {
    cwd.join(".lynshen").join("checkpoints")
}

fn resolve_existing_or_future(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Whether `path` lies in `root`. A file that does not exist (yet, or any
/// more) is judged by its deepest existing directory, so a workspace reached
/// through a symlink still holds its new and deleted files.
fn is_inside(root: &Path, path: &Path) -> bool {
    let root = normalize_for_policy(root);
    let path = normalize_for_policy(path);
    path == root || path.starts_with(root)
}

#[derive(Clone)]
struct CommandResult {
    exit_code: Option<i32>,
    signal: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    truncated: bool,
}

fn run_command(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandResult, String> {
    run_command_events(program, args, &[], None, cwd, timeout, &mut |_| Ok(()))
}

/// Runs `program` with `env` added to this process's environment.
fn run_command_events(
    program: &str,
    args: &[&str],
    env: &[(std::ffi::OsString, std::ffi::OsString)],
    stdin: Option<&str>,
    cwd: &Path,
    timeout: Duration,
    emit: &mut impl FnMut(ToolExecutionEvent) -> Result<(), String>,
) -> Result<CommandResult, String> {
    let stdout_path = temp_output_path("stdout");
    let stderr_path = temp_output_path("stderr");
    let stdout_file = File::create(&stdout_path).map_err(|error| error.to_string())?;
    let stderr_file = File::create(&stderr_path).map_err(|error| error.to_string())?;

    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().cloned())
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start {program}: {error}"))?;
    track_tool_group(child.id());

    if let Some(input) = stdin {
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin
                .write_all(input.as_bytes())
                .map_err(|error| error.to_string())?;
        }
    }

    let started = SystemTime::now();
    let mut last_update = started;
    let mut timed_out = false;
    if let Err(error) = emit(ToolExecutionEvent::Update(format!(
        "started: {}",
        command_display(program, args)
    ))) {
        abort_command(&mut child, &stdout_path, &stderr_path);
        return Err(error);
    }
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => {
                abort_command(&mut child, &stdout_path, &stderr_path);
                return Err(error.to_string());
            }
        }

        if started.elapsed().unwrap_or_default() >= timeout {
            timed_out = true;
            kill_child(&mut child);
            break;
        }
        if last_update.elapsed().unwrap_or_default() >= COMMAND_UPDATE_INTERVAL {
            let (stdout, stdout_truncated) = read_output_file(&stdout_path);
            let (stderr, stderr_truncated) = read_output_file(&stderr_path);
            if let Err(error) = emit(ToolExecutionEvent::Update(command_update_text(
                &stdout,
                &stderr,
                stdout_truncated || stderr_truncated,
                started.elapsed().unwrap_or_default(),
            ))) {
                abort_command(&mut child, &stdout_path, &stderr_path);
                return Err(error);
            }
            last_update = SystemTime::now();
        }
        thread::sleep(Duration::from_millis(50));
    }

    let status = child.wait().ok();
    let (stdout, stdout_truncated) = read_output_file(&stdout_path);
    let (stderr, stderr_truncated) = read_output_file(&stderr_path);
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);

    Ok(CommandResult {
        exit_code: status.as_ref().and_then(|status| status.code()),
        signal: status.as_ref().and_then(status_signal),
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    })
}

fn command_result_json(command: &str, result: Result<CommandResult, String>) -> Value {
    let mut value = match result {
        Ok(result) => {
            let mut value = json!({
                "command": command,
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "timed_out": result.timed_out,
                "truncated": result.truncated,
            });
            // A missing exit code outside the timeout path means the command
            // was killed; report it as an error instead of a silent success.
            if result.exit_code.is_none() && !result.timed_out {
                value["error"] = json!(match result.signal {
                    Some(signal) => format!("command terminated by signal {signal}"),
                    None => "command terminated without an exit code".to_string(),
                });
            }
            value
        }
        Err(error) => json!({ "command": command, "error": error }),
    };
    add_command_soft_hint(&mut value);
    value
}

fn add_command_soft_hint(value: &mut Value) {
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let stderr = value
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let output_bytes = stdout.len().saturating_add(stderr.len());
    let output_lines = stdout
        .lines()
        .count()
        .saturating_add(stderr.lines().count());
    if value
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        add_soft_hint(
            value,
            "command timed out",
            "Inspect partial output, narrow the command, or rerun with a longer timeout only if the full command is necessary. In the sandbox, a program that writes outside the writable directories (a browser's profile under ~/Library, for example) can hang: rerun it with escalate.",
        );
    } else if value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        add_soft_hint(
            value,
            "command output was truncated",
            "Rerun with a narrower command, redirect detailed output to a file, or inspect the referenced full output if provided.",
        );
    } else if output_bytes > COMMAND_OUTPUT_MAX_BYTES || output_lines > COMMAND_OUTPUT_MAX_LINES {
        add_soft_hint(
            value,
            "large command output",
            "Prefer a narrower command with paths, globs, grep/head/tail, or tool-native limits before requesting more output.",
        );
    }
}

pub(crate) fn unified_diff_for_file(
    cwd: &Path,
    path: &Path,
    original: &str,
    updated: &str,
) -> Option<String> {
    if original == updated {
        return None;
    }

    let old_path = temp_output_path("diff-old");
    let new_path = temp_output_path("diff-new");
    if fs::write(&old_path, original).is_err() || fs::write(&new_path, updated).is_err() {
        let _ = fs::remove_file(old_path);
        let _ = fs::remove_file(new_path);
        return None;
    }

    let label = diff_label(cwd, path);
    let old_arg = old_path.display().to_string();
    let new_arg = new_path.display().to_string();
    let result = run_command(
        "git",
        &[
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--",
            &old_arg,
            &new_arg,
        ],
        cwd,
        Duration::from_secs(30),
    );
    let _ = fs::remove_file(old_path);
    let _ = fs::remove_file(new_path);

    if let Ok(result) = result {
        if !result.stdout.trim().is_empty() {
            return Some(relabel_no_index_diff(&result.stdout, &label));
        }
    }
    Some(simple_unified_diff(&label, original, updated))
}

/// The working tree's changes against the index as a unified diff,
/// untracked files included; `path` limits it to one file or directory.
pub fn git_diff(cwd: &Path, path: Option<&Path>) -> Result<String, String> {
    if path.is_some_and(|path| is_internal_tool_path(cwd, path)) {
        return Ok(String::new());
    }
    let result = if let Some(path) = path {
        let path_arg = diff_path_arg(cwd, path);
        run_command(
            "git",
            &["diff", "--no-ext-diff", "--", &path_arg],
            cwd,
            Duration::from_secs(30),
        )
    } else {
        run_command(
            "git",
            &["diff", "--no-ext-diff"],
            cwd,
            Duration::from_secs(30),
        )
    };

    let result = result?;
    if result.exit_code != Some(0) {
        return Err(command_failure_message("git diff", &result));
    }

    let mut chunks = Vec::new();
    let tracked_diff = filter_internal_diff_sections(&result.stdout);
    if !tracked_diff.trim().is_empty() {
        chunks.push(tracked_diff);
    }
    for path in git_untracked_paths(cwd, path)? {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Some(diff) = unified_diff_for_file(cwd, &path, "", &content) {
                chunks.push(diff);
            }
        }
    }
    Ok(chunks.join("\n"))
}

fn git_untracked_paths(cwd: &Path, path: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    if path.is_some_and(|path| is_internal_tool_path(cwd, path)) {
        return Ok(Vec::new());
    }
    let path_arg = path.map(|path| diff_path_arg(cwd, path));
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "--"];
    if let Some(path_arg) = path_arg.as_deref() {
        args.push(path_arg);
    }
    let result = run_command("git", &args, cwd, Duration::from_secs(30))?;
    if result.exit_code != Some(0) {
        return Err(command_failure_message("git ls-files", &result));
    }
    Ok(result
        .stdout
        .lines()
        .map(|line| cwd.join(line))
        .filter(|path| path.is_file())
        .filter(|path| !is_internal_tool_path(cwd, path))
        .collect())
}

fn filter_internal_diff_sections(diff: &str) -> String {
    let mut output = String::new();
    let mut section = String::new();
    let mut keep_section = true;
    let mut saw_header = false;

    for line in diff.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        if bare.starts_with("diff --git ") {
            if keep_section {
                output.push_str(&section);
            }
            section.clear();
            saw_header = true;
            keep_section = !diff_header_is_internal(bare);
        }
        section.push_str(line);
    }
    if keep_section {
        output.push_str(&section);
    }
    if saw_header {
        output
    } else {
        diff.to_string()
    }
}

fn diff_header_is_internal(line: &str) -> bool {
    line.split_whitespace().skip(2).any(|path| {
        let path = strip_diff_prefix(path);
        path == ".lynshen" || path.starts_with(".lynshen/")
    })
}

fn is_internal_tool_path(cwd: &Path, path: &Path) -> bool {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let relative = path.strip_prefix(cwd).unwrap_or(path.as_path());
    relative
        .components()
        .next()
        .is_some_and(|component| component.as_os_str() == ".lynshen")
}

fn command_failure_message(command: &str, result: &CommandResult) -> String {
    [result.stderr.trim(), result.stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{command} failed"))
}

pub(crate) fn diff_label(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn diff_path_arg(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

fn relabel_no_index_diff(diff: &str, label: &str) -> String {
    let mut output = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            output.push_str(&format!("diff --git a/{label} b/{label}\n"));
        } else if line.starts_with("--- ") {
            output.push_str(&format!("--- a/{label}\n"));
        } else if line.starts_with("+++ ") {
            output.push_str(&format!("+++ b/{label}\n"));
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn simple_unified_diff(label: &str, original: &str, updated: &str) -> String {
    let old_lines = original.lines().collect::<Vec<_>>();
    let new_lines = updated.lines().collect::<Vec<_>>();
    let mut diff = format!(
        "diff --git a/{label} b/{label}\n--- a/{label}\n+++ b/{label}\n@@ -1,{} +1,{} @@\n",
        old_lines.len(),
        new_lines.len()
    );
    for op in line_diff_ops(&old_lines, &new_lines) {
        let (prefix, line) = match op {
            DiffOp::Context(line) => (' ', line),
            DiffOp::Remove(line) => ('-', line),
            DiffOp::Add(line) => ('+', line),
        };
        diff.push(prefix);
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

enum DiffOp<'a> {
    Context(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

fn line_diff_ops<'a>(old_lines: &[&'a str], new_lines: &[&'a str]) -> Vec<DiffOp<'a>> {
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];
    let mut ops = Vec::new();
    ops.extend(old_lines[..prefix].iter().copied().map(DiffOp::Context));
    ops.extend(line_diff_middle(old_mid, new_mid));
    ops.extend(
        old_lines[old_lines.len() - suffix..]
            .iter()
            .copied()
            .map(DiffOp::Context),
    );
    ops
}

fn line_diff_middle<'a>(old_lines: &[&'a str], new_lines: &[&'a str]) -> Vec<DiffOp<'a>> {
    if old_lines.is_empty() {
        return new_lines.iter().copied().map(DiffOp::Add).collect();
    }
    if new_lines.is_empty() {
        return old_lines.iter().copied().map(DiffOp::Remove).collect();
    }

    let cols = new_lines.len() + 1;
    let mut lcs = vec![0usize; (old_lines.len() + 1) * cols];
    for old_index in (0..old_lines.len()).rev() {
        for new_index in (0..new_lines.len()).rev() {
            let index = old_index * cols + new_index;
            lcs[index] = if old_lines[old_index] == new_lines[new_index] {
                lcs[(old_index + 1) * cols + new_index + 1] + 1
            } else {
                lcs[(old_index + 1) * cols + new_index].max(lcs[old_index * cols + new_index + 1])
            };
        }
    }

    let mut ops = Vec::new();
    let mut old_index = 0usize;
    let mut new_index = 0usize;
    while old_index < old_lines.len() && new_index < new_lines.len() {
        if old_lines[old_index] == new_lines[new_index] {
            ops.push(DiffOp::Context(old_lines[old_index]));
            old_index += 1;
            new_index += 1;
        } else if lcs[(old_index + 1) * cols + new_index] >= lcs[old_index * cols + new_index + 1] {
            ops.push(DiffOp::Remove(old_lines[old_index]));
            old_index += 1;
        } else {
            ops.push(DiffOp::Add(new_lines[new_index]));
            new_index += 1;
        }
    }
    ops.extend(old_lines[old_index..].iter().copied().map(DiffOp::Remove));
    ops.extend(new_lines[new_index..].iter().copied().map(DiffOp::Add));
    ops
}

fn command_display(program: &str, args: &[&str]) -> String {
    let mut parts = vec![program.to_string()];
    parts.extend(args.iter().map(|arg| arg.to_string()));
    parts.join(" ")
}

fn command_update_text(stdout: &str, stderr: &str, truncated: bool, elapsed: Duration) -> String {
    let mut lines = vec![format!("running {:.1}s", elapsed.as_secs_f32())];
    if !stdout.is_empty() {
        lines.push(format!("stdout:\n{}", tail_lines(stdout, 8)));
    }
    if !stderr.is_empty() {
        lines.push(format!("stderr:\n{}", tail_lines(stderr, 8)));
    }
    if truncated {
        lines.push("output truncated".to_string());
    }
    lines.join("\n")
}

fn tail_lines(text: &str, limit: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

fn read_output_file(path: &Path) -> (String, bool) {
    let Ok(bytes) = fs::read(path) else {
        return (String::new(), false);
    };
    truncate_command_output(&String::from_utf8_lossy(&bytes))
}

fn truncate_command_output(text: &str) -> (String, bool) {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let line_start = lines.len().saturating_sub(COMMAND_OUTPUT_MAX_LINES);
    let mut output = lines[line_start..].concat();

    let mut omitted_lines = line_start;
    let mut omitted_bytes = text.len().saturating_sub(output.len());
    if output.len() > COMMAND_OUTPUT_MAX_BYTES {
        let keep_width = safe_boundary_rev(&output, COMMAND_OUTPUT_MAX_BYTES);
        let keep_start = output.len() - keep_width;
        omitted_bytes += keep_start;
        omitted_lines += output[..keep_start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        output = output[keep_start..].to_string();
    }

    if omitted_lines == 0 && omitted_bytes == 0 {
        return (output, false);
    }

    (
        format!(
            "[...command output truncated: omitted {omitted_lines} earlier lines and {omitted_bytes} bytes...]\n{output}"
        ),
        true,
    )
}

fn temp_output_path(label: &str) -> PathBuf {
    // The counter keeps names unique across threads: the clock alone repeats
    // when two tool calls (or two hosted sessions) ask in the same tick.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    env::temp_dir().join(format!(
        "lynshen-tool-{label}-{}-{}-{}.log",
        std::process::id(),
        now_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>, String> {
    Ok(optional_u64(args, key)?.and_then(|value| usize::try_from(value).ok()))
}

/// `Ok(None)` when the key is absent or null. Accepts integer-valued floats
/// (e.g. 30.0); anything else is a descriptive error instead of a silent default.
pub(crate) fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    if let Some(number) = value.as_u64() {
        return Ok(Some(number));
    }
    if let Some(number) = value.as_f64() {
        if number >= 0.0 && number.fract() == 0.0 && number <= u64::MAX as f64 {
            return Ok(Some(number as u64));
        }
    }
    Err(format!("{key} must be a non-negative integer, got {value}"))
}

pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = expand_tilde(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Where a mutating file tool may write: inside the boundary (the
/// workspace, the sandbox's read-write directories, temp and the package
/// caches) where the sandbox does not forbid it, or a path outside it that
/// this call was approved to write (`ToolState::allowing_writes`). Symlinks
/// in the existing part of the path are resolved before the check, so a
/// symlink pointing outside the workspace counts as outside.
pub(crate) fn write_target(cwd: &Path, path: &str, state: &ToolState) -> Result<PathBuf, String> {
    let resolved = resolve_path(cwd, path);
    if state.may_write_outside(&resolved) {
        // The path as it was approved, not through a link changed since.
        return Ok(normalize_for_policy(&resolved));
    }
    // Credentials and LynShen's settings are written only with an approval
    // for that very path, wherever they are (a workspace at home, a
    // writable directory around them).
    if is_guarded_write(&resolved) {
        return Err(format!(
            "{} holds credentials or LynShen's settings: writing it needs the user's approval, which this call does not have",
            resolved.display()
        ));
    }
    let sandbox = state.sandbox();
    let inside = ensure_in_workspace(cwd, &resolved).is_ok()
        || sandbox
            .as_ref()
            .is_some_and(|sandbox| sandbox.within_boundary(cwd, &resolved));
    if !inside {
        return Err(format!(
            "{} is outside the workspace and the writable directories: writing it needs the user's approval, which this call does not have",
            resolved.display()
        ));
    }
    if let Some(sandbox) = sandbox {
        sandbox.check_write(cwd, &resolved)?;
    }
    Ok(resolved)
}

/// The file a write tool (`write`, `edit`, `str_replace`, `hashline_edit`,
/// `generate_image`) writes, when it is outside the workspace and the
/// writable directories: the call then needs approval (`outside_write`).
pub(crate) fn outside_write_target(
    name: &str,
    arguments: &str,
    cwd: &Path,
    sandbox: Option<&crate::sandbox::SandboxPolicy>,
) -> Option<PathBuf> {
    if !matches!(
        name,
        "write" | "edit" | "str_replace" | "hashline_edit" | crate::images::TOOL_NAME
    ) {
        return None;
    }
    let args = serde_json::from_str::<Value>(arguments).ok()?;
    let path = args.get("path").and_then(Value::as_str)?;
    let mut resolved = resolve_path(cwd, path);
    // generate_image names its files (`fox.png`, `fox-2.png`, a timestamp
    // in a directory): what it asks for is the directory they go into.
    if name == crate::images::TOOL_NAME && !(path.ends_with(['/', '\\']) || resolved.is_dir()) {
        resolved = resolved.parent()?.to_path_buf();
    }
    let inside = ensure_in_workspace(cwd, &resolved).is_ok()
        || sandbox.is_some_and(|sandbox| sandbox.within_boundary(cwd, &resolved));
    (!inside || is_guarded_write(&resolved)).then(|| normalize_for_policy(&resolved))
}

/// Whether `path` is (in) credentials or LynShen's settings
/// (`sandbox::guarded_writes`).
fn is_guarded_write(path: &Path) -> bool {
    let normalized = normalize_for_policy(path);
    crate::sandbox::guarded_writes()
        .iter()
        .any(|guarded| normalized.starts_with(guarded))
}

pub(crate) fn workspace_path(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let resolved = resolve_path(cwd, path);
    ensure_in_workspace(cwd, &resolved)?;
    Ok(resolved)
}

/// Path policy for the read-only file tools: anywhere but credentials
/// (`sandbox::denied_reads`), which stay unreadable as they are for a
/// sandboxed command.
pub(crate) fn readable_path(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let resolved = resolve_path(cwd, path);
    // Checked first: a workspace at home (or around a credentials
    // directory) does not make them readable.
    let normalized = normalize_for_policy(&resolved);
    if crate::sandbox::denied_reads()
        .iter()
        .any(|secret| normalized == *secret || normalized.starts_with(secret))
    {
        return Err(format!(
            "{} holds credentials and is not readable",
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// Errors when `resolved` escapes the workspace root after resolving
/// symlinks in its existing prefix and `.`/`..` components lexically in the
/// (necessarily symlink-free) non-existent remainder.
pub(crate) fn ensure_in_workspace(cwd: &Path, resolved: &Path) -> Result<(), String> {
    let workspace = cwd.canonicalize().map_err(|error| {
        format!(
            "cannot resolve the workspace root {}: {error}",
            cwd.display()
        )
    })?;
    let normalized = normalize_for_policy(resolved);
    if normalized == workspace || normalized.starts_with(&workspace) {
        Ok(())
    } else {
        Err(format!(
            "path escapes the workspace: {} resolves outside {}. File tools only operate on paths inside the workspace; write files you need to read back (screenshots, logs, generated output) under the workspace, or copy one in with bash first.",
            resolved.display(),
            workspace.display()
        ))
    }
}

/// Canonicalizes the deepest existing ancestor of `path` (resolving symlinks
/// and `..`), then applies the remaining non-existent components lexically.
fn normalize_for_policy(path: &Path) -> PathBuf {
    let (base, remainder) = deepest_canonical_ancestor(path);
    let mut normalized = base;
    for component in remainder.components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            // CurDir is dropped; RootDir/Prefix cannot appear in a stripped
            // remainder.
            _ => {}
        }
    }
    normalized
}

fn deepest_canonical_ancestor(path: &Path) -> (PathBuf, PathBuf) {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = ancestor.canonicalize() {
            let remainder = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
            return (canonical, remainder.to_path_buf());
        }
    }
    (path.to_path_buf(), PathBuf::new())
}

/// The path arguments of each file tool, as single strings and as arrays.
fn path_argument_keys(name: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match name {
        "read" | "ls" | "ripgrep" | "outline" | "write" | "str_replace" | "edit"
        | "hashline_edit" => (&["path"], &[]),
        crate::images::TOOL_NAME => (&["path"], &["images"]),
        "checkpoint" => (&[], &["paths"]),
        _ => (&[], &[]),
    }
}

/// Rewrites a file tool's path arguments for a subagent confined to its own
/// worktree, in place. Returns whether anything changed.
pub(crate) fn map_confined_paths(name: &str, args: &mut Value, state: &ToolState) -> bool {
    let Some(confine) = state.confine.as_ref() else {
        return false;
    };
    let (root, project) = (confine.0.as_path(), confine.1.as_path());
    let (strings, arrays) = path_argument_keys(name);
    let Some(map) = args.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for key in strings {
        let Some(path) = map.get(*key).and_then(Value::as_str) else {
            continue;
        };
        if let Some(mapped) = map_project_path(path, root, project) {
            map.insert((*key).to_string(), json!(mapped.display().to_string()));
            changed = true;
        }
    }
    for key in arrays {
        let Some(values) = map.get(*key).and_then(Value::as_array).cloned() else {
            continue;
        };
        let mut array_changed = false;
        let mapped = values
            .iter()
            .map(|value| match value.as_str() {
                Some(path) => match map_project_path(path, root, project) {
                    Some(mapped) => {
                        array_changed = true;
                        json!(mapped.display().to_string())
                    }
                    None => value.clone(),
                },
                None => value.clone(),
            })
            .collect::<Vec<_>>();
        if array_changed {
            map.insert((*key).to_string(), json!(mapped));
            changed = true;
        }
    }
    changed
}

/// `map_confined_paths` on a tool call's raw arguments; None when nothing
/// changed. Used by the write-isolation gate, which runs before the tool.
pub(crate) fn map_confined_arguments(
    name: &str,
    arguments: &str,
    state: &ToolState,
) -> Option<String> {
    let mut args = serde_json::from_str::<Value>(arguments).ok()?;
    map_confined_paths(name, &mut args, state).then(|| args.to_string())
}

/// A subagent's worktree holds the parent repository's tree, so the same
/// file sits at the same relative path in both. Its task names the parent's
/// paths and the model repeats them, so an absolute path under the parent's
/// root is read and written at that relative path inside the worktree
/// instead of being refused. Paths under `.lynshen/agents` (this workspace
/// or a sibling's) are left exactly as they are, as is anything outside the
/// parent's root.
fn map_project_path(path: &str, root: &Path, project: &Path) -> Option<PathBuf> {
    let candidate = expand_tilde(path);
    if !candidate.is_absolute() {
        return None;
    }
    let relative = normalize_for_policy(&candidate)
        .strip_prefix(normalize_for_policy(project))
        .ok()?
        .to_path_buf();
    if relative.starts_with(Path::new(".lynshen").join("agents")) {
        return None;
    }
    Some(root.join(relative))
}

/// Checks whether a mutating tool call targets a path outside `root` (used to
/// confine subagent writes to their isolated workspace). Returns a description
/// of the violation, or None when the call is allowed. Read-only tools and
/// tools without static path arguments (bash) always pass; bash isolation
/// relies on the workspace being the child's cwd plus approval gating.
pub(crate) fn write_target_escapes_root(
    name: &str,
    arguments: &str,
    cwd: &Path,
    root: &Path,
) -> Option<String> {
    let args = serde_json::from_str::<Value>(arguments).ok()?;
    match name {
        "write" | "str_replace" | "edit" | "hashline_edit" | crate::images::TOOL_NAME => {
            let path = args.get("path").and_then(Value::as_str)?;
            let resolved = normalize_lexically(&resolve_path(cwd, path));
            if resolved.starts_with(normalize_lexically(root)) {
                None
            } else {
                Some(format!("{name} targets {} outside the workspace", path))
            }
        }
        "apply_patch" => {
            let patch = args.get("patch").and_then(Value::as_str)?;
            patch_escapes_workdir(patch)
                .map(|path| format!("apply_patch touches {path} outside the workspace"))
        }
        _ => None,
    }
}

/// Lexically resolves `.` and `..` components without touching the filesystem
/// (targets may not exist yet). `..` at the root is kept, which can only make
/// containment checks stricter.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// A patch applied with `git apply` in the workspace cwd can only escape via
/// absolute paths or `..` traversal in its file headers; flag either.
fn patch_escapes_workdir(patch: &str) -> Option<String> {
    for line in patch.lines() {
        let path = if let Some(rest) = line.strip_prefix("+++ ") {
            rest
        } else if let Some(rest) = line.strip_prefix("--- ") {
            rest
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            rest
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            rest
        } else if let Some(rest) = line.strip_prefix("copy to ") {
            rest
        } else {
            continue;
        };
        let path = path
            .trim()
            .trim_start_matches("a/")
            .trim_start_matches("b/");
        if path == "/dev/null" {
            continue;
        }
        if Path::new(path).has_root()
            || Path::new(path).components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
        {
            return Some(path.to_string());
        }
    }
    None
}

fn expand_tilde(path: &str) -> PathBuf {
    if path != "~" && !path.starts_with("~/") {
        return PathBuf::from(path);
    }
    let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let Some(home) = env::var_os(home_key) else {
        return PathBuf::from(path);
    };
    let mut expanded = PathBuf::from(home);
    if let Some(rest) = path.strip_prefix("~/") {
        expanded.push(rest);
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn terminating_a_tool_group_ends_the_command_and_its_children() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let grandchild: i32 = line.trim().parse().unwrap();

        // Only this test's group: the suite runs other tool commands in
        // parallel in this process.
        terminate_groups(vec![child.id()]);

        // SAFETY: signal 0 only probes the pid.
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(grandchild), "the command's child outlived it");
        assert!(matches!(child.try_wait(), Err(_) | Ok(Some(_))));
    }

    #[test]
    fn write_root_guard_blocks_targets_outside_workspace() {
        let root = Path::new("/work/.lynshen/agents/worker-1");

        // Relative writes inside the workspace pass.
        assert!(write_target_escapes_root(
            "write",
            &json!({ "path": "src/new.rs", "content": "x" }).to_string(),
            root,
            root
        )
        .is_none());

        // Absolute writes into the parent tree are blocked.
        let violation = write_target_escapes_root(
            "write",
            &json!({ "path": "/work/src/main.rs", "content": "x" }).to_string(),
            root,
            root,
        );
        assert!(violation.unwrap().contains("outside the workspace"));

        // `..` traversal out of the workspace is blocked lexically.
        let violation = write_target_escapes_root(
            "str_replace",
            &json!({ "path": "../../../src/main.rs", "edits": [] }).to_string(),
            root,
            root,
        );
        assert!(violation.unwrap().contains("outside the workspace"));

        // Read-only tools are never gated.
        assert!(write_target_escapes_root(
            "read",
            &json!({ "path": "/work/src/main.rs" }).to_string(),
            root,
            root
        )
        .is_none());
    }

    #[test]
    fn write_root_guard_blocks_escaping_patches() {
        let root = Path::new("/work/.lynshen/agents/worker-1");
        let safe = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(write_target_escapes_root(
            "apply_patch",
            &json!({ "patch": safe }).to_string(),
            root,
            root
        )
        .is_none());

        let absolute = "--- /work/src/lib.rs\n+++ /work/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(write_target_escapes_root(
            "apply_patch",
            &json!({ "patch": absolute }).to_string(),
            root,
            root
        )
        .is_some());

        let traversal = "--- a/../escape.rs\n+++ b/../escape.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(write_target_escapes_root(
            "apply_patch",
            &json!({ "patch": traversal }).to_string(),
            root,
            root
        )
        .is_some());

        let new_file = "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n+a\n";
        assert!(write_target_escapes_root(
            "apply_patch",
            &json!({ "patch": new_file }).to_string(),
            root,
            root
        )
        .is_none());
    }

    #[test]
    fn image_read_projects_note_and_builds_image_message() {
        let output = json!({
            "path": "/tmp/pic.png",
            "kind": "image",
            "mime": "image/png",
            "bytes": 3,
            "base64": "AAEC",
        })
        .to_string();

        let projected = project_model_output("read", &output, Path::new("."));
        let projected_value = serde_json::from_str::<Value>(&projected).unwrap();
        assert!(projected_value.get("base64").is_none());
        assert!(projected_value.get("note").is_some());

        let image = image_content_item(&output).unwrap();
        assert_eq!(image["role"], "user");
        assert_eq!(image["content"][0]["type"], "input_image");
        assert_eq!(
            image["content"][0]["image_url"],
            "data:image/png;base64,AAEC"
        );
    }

    #[test]
    fn a_read_reaches_the_model_in_the_form_its_edit_tool_uses() {
        let output = json!({ "path": "a.txt", "content": "hello\n", "hashlines": "1#ab:hello\n" })
            .to_string();
        let hashline: Value =
            serde_json::from_str(&read_output_for_editing(output.clone(), true)).unwrap();
        assert!(hashline.get("content").is_none() && hashline["hashlines"] == "1#ab:hello\n");
        let plain: Value = serde_json::from_str(&read_output_for_editing(output, false)).unwrap();
        assert!(plain.get("hashlines").is_none() && plain["content"] == "hello\n");
        // A large read already lost its content: the hashlines stay.
        let projected = json!({ "path": "a.txt", "hashlines": "1#ab:hello" }).to_string();
        assert_eq!(read_output_for_editing(projected.clone(), false), projected);
        assert_eq!(
            read_output_for_editing("not json".to_string(), true),
            "not json"
        );
    }

    #[test]
    fn non_image_read_has_no_image_message() {
        let output = json!({ "content": "hello", "hashlines": "1#ab hello" }).to_string();
        assert!(image_content_item(&output).is_none());
    }

    #[test]
    fn read_supports_offset_and_limit() {
        let dir = test_dir("read");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let result = run_tool(
            "read",
            &json!({ "path": path, "offset": 2, "limit": 1 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["content"], "two\n");
        assert_eq!(value["lines_read"], 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_defaults_to_no_line_limit() {
        let dir = test_dir("read-no-limit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        let content = (1..=550)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        fs::write(&path, &content).unwrap();

        let result = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["lines_read"], 550);
        assert_eq!(value["truncated"], false);
        assert_eq!(value["content"], content);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_warns_when_large_file_has_no_limit() {
        let dir = test_dir("read-large-warning");
        let path = dir.join("large.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "x".repeat(300 * 1024)).unwrap();

        let result = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["warning"]
            .as_str()
            .unwrap()
            .contains("large file read"));
        assert!(value["suggestion"]
            .as_str()
            .unwrap()
            .contains("offset/limit"));
        assert_eq!(value["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn bash_truncates_large_stdout() {
        let dir = test_dir("bash-truncate");
        fs::create_dir_all(&dir).unwrap();

        let result = run_tool(
            "bash",
            &json!({
                "command": "i=0; while [ $i -lt 3505 ]; do echo line-$i; i=$((i+1)); done"
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let stdout = value["stdout"].as_str().unwrap();

        assert_eq!(value["truncated"], true);
        assert!(stdout.contains("command output truncated"));
        assert!(stdout.contains("line-3504"));
        assert!(!stdout.contains("line-0\n"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_requires_unique_old_text() {
        let dir = test_dir("edit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "same\nsame\n").unwrap();
        let _ = run_tool("read", &json!({ "path": path }).to_string(), &dir);

        let result = run_tool(
            "edit",
            &json!({
                "path": path,
                "edits": [{ "oldText": "same", "newText": "next" }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["error"].as_str().unwrap().contains("exactly once"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "same\nsame\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_applies_targeted_replacement() {
        let dir = test_dir("edit-ok");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\nbeta\n").unwrap();
        let _ = run_tool("read", &json!({ "path": path }).to_string(), &dir);

        let result = run_tool(
            "edit",
            &json!({
                "path": path,
                "edits": [{ "oldText": "beta", "newText": "gamma" }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["edits"], 1);
        assert_eq!(
            fs::read_to_string(&path).unwrap().replace("\r\n", "\n"),
            "alpha\ngamma\n"
        );
        let diff = value["diff"].as_str().unwrap();
        assert!(diff.contains("-beta"));
        assert!(diff.contains("+gamma"));
        assert!(diff.contains(" alpha"));
        assert!(!diff.contains("-alpha"));
        assert!(!diff.contains("+alpha"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_returns_git_style_diff() {
        let dir = test_dir("write-diff");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "before\n").unwrap();
        let _ = run_tool("read", &json!({ "path": path }).to_string(), &dir);

        let result = run_tool(
            "write",
            &json!({ "path": path, "content": "after\n" }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["diff"].as_str().unwrap().contains("--- a/"));
        assert!(value["diff"].as_str().unwrap().contains("+++ b/"));
        assert!(value["diff"].as_str().unwrap().contains("-before"));
        assert!(value["diff"].as_str().unwrap().contains("+after"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_creates_new_file_without_prior_read() {
        let dir = test_dir("write-create");
        let path = dir.join("src").join("main.rs");
        fs::create_dir_all(&dir).unwrap();

        let result = run_tool(
            "write",
            &json!({ "path": path, "content": "fn main() {}\n" }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "fn main() {}\n");
        assert!(value.get("error").is_none());
        assert!(value["diff"].as_str().unwrap().contains("+fn main() {}"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_still_requires_read_before_overwriting_existing() {
        let dir = test_dir("write-read-first");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "before\n").unwrap();

        let result = run_tool(
            "write",
            &json!({ "path": path, "content": "after\n" }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("requires reading"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn git_diff_returns_empty_success_for_clean_workspace() {
        let dir = test_dir("diff-clean");
        fs::create_dir_all(&dir).unwrap();
        run_command("git", &["init"], &dir, Duration::from_secs(30)).unwrap();

        let diff = git_diff(&dir, None).unwrap();

        assert_eq!(diff, "");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn git_diff_includes_untracked_text_file() {
        let dir = test_dir("diff-untracked");
        fs::create_dir_all(&dir).unwrap();
        run_command("git", &["init"], &dir, Duration::from_secs(30)).unwrap();
        fs::write(dir.join("new.txt"), "hello\n").unwrap();

        let diff = git_diff(&dir, Some(&dir.join("new.txt"))).unwrap();

        assert!(diff.contains("new.txt"));
        assert!(diff.contains("+hello"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn git_diff_excludes_lynshen_internal_files() {
        let dir = test_dir("diff-internal");
        fs::create_dir_all(dir.join(".lynshen").join("checkpoints")).unwrap();
        run_command("git", &["init"], &dir, Duration::from_secs(30)).unwrap();
        fs::write(
            dir.join(".lynshen").join("checkpoints").join("cp.json"),
            "{}\n",
        )
        .unwrap();
        fs::write(dir.join("new.txt"), "hello\n").unwrap();

        let diff = git_diff(&dir, None).unwrap();

        assert!(diff.contains("new.txt"));
        assert!(!diff.contains(".lynshen"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hashline_edit_applies_anchor_replacement() {
        let dir = test_dir("hashline-edit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let read = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let read = serde_json::from_str::<Value>(&read).unwrap();
        let beta_anchor = read["hashlines"]
            .as_str()
            .unwrap()
            .lines()
            .find(|line| line.ends_with(":beta"))
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();

        let result = run_tool(
            "hashline_edit",
            &json!({
                "path": path,
                "edits": [{ "op": "replace", "pos": beta_anchor, "lines": ["BETA"] }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap().replace("\r\n", "\n"),
            "alpha\nBETA\ngamma\n"
        );
        assert!(value["anchors"].as_str().unwrap().contains("2#"));
        assert!(value["diff"].as_str().unwrap().contains("-beta"));
        assert!(value["diff"].as_str().unwrap().contains("+BETA"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hashline_hash_matches_reference_encoding() {
        assert_eq!(compute_line_hash(1, "alpha"), "JN");
        assert_eq!(compute_line_hash(2, "beta"), "NK");
        assert_eq!(compute_line_hash(3, "gamma"), "WB");
        assert_eq!(compute_line_hash(4, ""), "RW");
        assert_eq!(compute_line_hash(5, "  "), "BT");
        assert_eq!(compute_line_hash(6, "{"), "KM");
    }

    #[test]
    fn hashline_edit_rejects_stale_anchor() {
        let dir = test_dir("hashline-stale");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\nbeta\n").unwrap();

        let read = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let read = serde_json::from_str::<Value>(&read).unwrap();
        let beta_anchor = read["hashlines"]
            .as_str()
            .unwrap()
            .lines()
            .find(|line| line.ends_with(":beta"))
            .unwrap()
            .split_once(':')
            .unwrap()
            .0
            .to_string();
        fs::write(&path, "alpha\nchanged\n").unwrap();

        let result = run_tool(
            "hashline_edit",
            &json!({
                "path": path,
                "edits": [{ "op": "replace", "pos": beta_anchor, "lines": ["BETA"] }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        // The file changed after the read, so the stale-read gate rejects the
        // edit before the anchors are even checked.
        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("changed on disk since you last read it"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\nchanged\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_rejects_file_changed_since_read() {
        let dir = test_dir("write-stale");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "original\n").unwrap();
        run_tool("read", &json!({ "path": path }).to_string(), &dir);
        fs::write(&path, "someone else's change\n").unwrap();

        let result = run_tool(
            "write",
            &json!({ "path": path, "content": "mine\n" }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("changed on disk since you last read it"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "someone else's change\n"
        );

        // Reading again picks up the current content and unblocks the write.
        run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let result = run_tool(
            "write",
            &json!({ "path": path, "content": "mine\n" }).to_string(),
            &dir,
        );
        assert!(serde_json::from_str::<Value>(&result).unwrap()["error"].is_null());
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn subagent_tool_state_must_read_before_editing() {
        let dir = test_dir("subagent-reads");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "original\n").unwrap();
        let parent = ToolState::default();
        run_tool_with_events(
            "read",
            &json!({ "path": path }).to_string(),
            &dir,
            &[],
            &parent,
            |_| Ok(()),
        );

        let child = parent.for_subagent();
        let result = run_tool_with_events(
            "write",
            &json!({ "path": path, "content": "child\n" }).to_string(),
            &dir,
            &[],
            &child,
            |_| Ok(()),
        );
        assert!(
            serde_json::from_str::<Value>(&result.output).unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("requires reading an existing file first")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "original\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ls_sorts_and_marks_directories() {
        let dir = test_dir("ls");
        fs::create_dir_all(dir.join("b_dir")).unwrap();
        fs::write(dir.join("a.txt"), "").unwrap();

        let result = run_tool("ls", &json!({ "path": dir }).to_string(), &env::temp_dir());
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["entries"][0], "a.txt");
        assert_eq!(value["entries"][1], "b_dir/");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ls_defaults_to_no_entry_limit() {
        let dir = test_dir("ls-no-limit");
        fs::create_dir_all(&dir).unwrap();
        for index in 0..550 {
            fs::write(dir.join(format!("{index:03}.txt")), "").unwrap();
        }

        let result = run_tool("ls", &json!({ "path": dir }).to_string(), &env::temp_dir());
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["entries"].as_array().unwrap().len(), 550);
        assert_eq!(value["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ripgrep_defaults_to_no_output_line_limit() {
        let dir = test_dir("rg-no-limit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "needle\n".repeat(150)).unwrap();

        let result = run_tool(
            "ripgrep",
            &json!({ "pattern": "needle", "path": path }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["stdout"].as_str().unwrap().lines().count(), 150);
        assert_eq!(value["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ripgrep_warns_when_broad_output_has_no_limit() {
        let dir = test_dir("rg-large-warning");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "needle\n".repeat(250)).unwrap();

        let result = run_tool(
            "ripgrep",
            &json!({ "pattern": "needle", "path": path }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["warning"]
            .as_str()
            .unwrap()
            .contains("large ripgrep output"));
        assert!(value["suggestion"].as_str().unwrap().contains("limit"));
        assert_eq!(value["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn definitions_expose_expected_tools() {
        let tools = definitions();
        let names = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "read",
                "str_replace",
                "hashline_edit",
                "write",
                "apply_patch",
                "bash",
                "write_stdin",
                "ls",
                "ripgrep",
                "outline",
                "checkpoint",
                "web_fetch",
                "web_search",
                "generate_image",
                "android_read",
                "android_act"
            ]
        );
        assert!(tools
            .iter()
            .all(|tool| tool.get("strict") == Some(&json!(false))));
    }

    #[test]
    fn shell_aliases_run_as_bash_without_being_offered() {
        let names = definitions()
            .into_iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        let dir = std::env::temp_dir();
        for alias in ["exec_command", "execute", "shell_command"] {
            assert!(
                !names.iter().any(|name| name == alias),
                "{alias} is offered"
            );
            let output = run_tool(alias, &json!({ "cmd": "echo alias-ok" }).to_string(), &dir);
            assert!(output.contains("alias-ok"), "{alias}: {output}");
        }
    }

    #[test]
    fn exec_command_accepts_codex_style_cmd_and_workdir() {
        let dir = test_dir("exec-command");
        let subdir = dir.join("sub");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(subdir.join("marker.txt"), "ok").unwrap();
        let command = if cfg!(windows) {
            "Get-ChildItem marker.txt | Select-Object -ExpandProperty Name"
        } else {
            "pwd; ls marker.txt"
        };

        let result = run_tool(
            "exec_command",
            &json!({ "cmd": command, "workdir": "sub", "timeout": 5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("marker.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn exec_command_truncates_large_output() {
        let dir = test_dir("exec-large-truncate");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "1..3500 | ForEach-Object { 'line' }"
        } else {
            "yes line | head -n 3500"
        };

        let result = run_tool(
            "exec_command",
            &json!({ "cmd": command, "timeout": 5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["truncated"], true);
        assert!(value["stdout"]
            .as_str()
            .unwrap()
            .contains("command output truncated"));
        assert!(value["warning"]
            .as_str()
            .unwrap()
            .contains("command output was truncated"));
        assert!(value["suggestion"]
            .as_str()
            .unwrap()
            .contains("narrower command"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_accepts_workdir() {
        let dir = test_dir("bash-workdir");
        let subdir = dir.join("sub");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(subdir.join("marker.txt"), "ok").unwrap();
        let command = if cfg!(windows) {
            "Get-ChildItem marker.txt | Select-Object -ExpandProperty Name"
        } else {
            "ls marker.txt"
        };

        let result = run_tool(
            "bash",
            &json!({ "command": command, "workdir": "sub", "timeout": 5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("marker.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn apply_patch_applies_unified_diff() {
        let dir = test_dir("apply-patch");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\nbeta\n").unwrap();

        let patch = r#"diff --git a/sample.txt b/sample.txt
--- a/sample.txt
+++ b/sample.txt
@@ -1,2 +1,2 @@
 alpha
-beta
+gamma
"#;
        run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let result = run_tool("apply_patch", &json!({ "patch": patch }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["applied"], true);
        assert_eq!(
            fs::read_to_string(&path).unwrap().replace("\r\n", "\n"),
            "alpha\ngamma\n"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_emits_lifecycle_updates() {
        let dir = test_dir("bash-updates");
        fs::create_dir_all(&dir).unwrap();
        let mut updates = Vec::new();

        let result = run_tool_with_events(
            "bash",
            &json!({ "command": "echo hello", "timeout": 5 }).to_string(),
            &dir,
            &[],
            &test_state(),
            |event| {
                let ToolExecutionEvent::Update(output) = event;
                updates.push(output);
                Ok(())
            },
        );
        let value = serde_json::from_str::<Value>(&result.output).unwrap();

        assert!(!result.is_error);
        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("hello"));
        assert!(updates.iter().any(|update| update.contains("started:")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_can_yield_running_session_and_poll_it() {
        let dir = test_dir("bash-session");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 300; Write-Output done"
        } else {
            "sleep 0.3; echo done"
        };

        let result = run_tool(
            "bash",
            &json!({ "command": command, "timeout": 5, "yield_time_ms": 1 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let session_id = value["session_id"].as_u64().unwrap();
        assert_eq!(value["running"], true);

        let mut value = json!({ "running": true });
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            let result = run_tool(
                "write_stdin",
                &json!({ "session_id": session_id, "yield_time_ms": 100 }).to_string(),
                &dir,
            );
            value = serde_json::from_str::<Value>(&result).unwrap();
            if value["running"] != true {
                break;
            }
        }

        assert_eq!(value["exit_code"], 0, "{value}");
        assert!(value["stdout"].as_str().unwrap().contains("done"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_stdin_accepts_codex_style_chars() {
        let dir = test_dir("write-stdin-chars");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "$line = [Console]::In.ReadLine(); Write-Output $line"
        } else {
            "head -n 1"
        };

        let result = run_tool(
            "exec_command",
            &json!({ "cmd": command, "timeout": 5, "yield_time_ms": 1 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let session_id = value["session_id"].as_u64().unwrap();

        let mut value = json!({ "running": true });
        for index in 0..20 {
            let args = if index == 0 {
                json!({ "session_id": session_id, "chars": "hello\n", "yield_time_ms": 250 })
            } else {
                json!({ "session_id": session_id, "yield_time_ms": 250 })
            };
            let result = run_tool("write_stdin", &args.to_string(), &dir);
            value = serde_json::from_str::<Value>(&result).unwrap();
            if value["running"] != true {
                break;
            }
        }

        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"].as_str().unwrap().contains("hello"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_truncates_output_instead_of_returning_full_stdout() {
        let dir = test_dir("bash-tail-limit");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "1..9000 | ForEach-Object { 'line' }"
        } else {
            "yes line | head -n 9000"
        };

        let result = run_tool(
            "bash",
            &json!({ "command": command, "timeout": 5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["truncated"], true);
        assert!(value["stdout"].as_str().unwrap().lines().count() < 9000);
        assert!(value["stdout"]
            .as_str()
            .unwrap()
            .contains("command output truncated"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn long_tool_result_is_projected_for_model() {
        let dir = test_dir("tool-projection");
        fs::create_dir_all(&dir).unwrap();
        let content = "x".repeat(128 * 1024);
        let result = tool_result("test_tool", json!({ "content": content }), &dir);
        let projected = serde_json::from_str::<Value>(&result.model_output).unwrap();
        let full_output_path = projected["full_output_path"].as_str().unwrap();

        assert_ne!(result.output, result.model_output);
        assert!(result.model_output.contains("model_output_truncated"));
        assert_eq!(fs::read_to_string(full_output_path).unwrap(), result.output);
        assert!(dir.join(".lynshen").join("truncated-results").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn large_edit_diff_is_summarized_for_model_output() {
        let dir = test_dir("diff-projection");
        fs::create_dir_all(&dir).unwrap();
        let diff_body = (0..700)
            .map(|index| format!("+added line {index}\n"))
            .collect::<String>();
        let diff = format!(
            "diff --git a/src/app.rs b/src/app.rs\n--- a/src/app.rs\n+++ b/src/app.rs\n@@ -0,0 +1,700 @@\n{diff_body}"
        );

        let result = tool_result(
            "write",
            json!({
                "path": dir.display().to_string(),
                "has_changes": true,
                "diff": diff
            }),
            &dir,
        );
        let projected = serde_json::from_str::<Value>(&result.model_output).unwrap();
        let full_output_path = projected["full_output_path"].as_str().unwrap();

        assert_ne!(result.output, result.model_output);
        assert_eq!(projected["model_output_truncated"], true);
        assert_eq!(projected["diff_summary"]["files_changed"], 1);
        assert_eq!(projected["diff_summary"]["additions"], 700);
        assert!(projected["diff"].as_str().unwrap().len() < 6000);
        assert_eq!(fs::read_to_string(full_output_path).unwrap(), result.output);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_model_output_omits_duplicate_content_but_keeps_hashlines() {
        let dir = test_dir("read-projection");
        fs::create_dir_all(&dir).unwrap();
        let content = format!("{}\n{}", "a".repeat(2048), "b".repeat(2048));
        let hashlines = format!("1#AA:{}\n2#BB:{}", "a".repeat(2048), "b".repeat(2048));
        let output = json!({
            "path": "/tmp/example.py",
            "kind": "text",
            "content": content,
            "hashlines": hashlines,
            "lines_read": 2,
            "truncated": false
        })
        .to_string();

        let projected = serde_json::from_str::<Value>(
            &project_read_model_output_inner("read", &output, &dir).unwrap(),
        )
        .unwrap();

        assert!(projected.get("content").is_none());
        assert!(projected["hashlines"].as_str().unwrap().contains("1#AA:"));
        assert_eq!(projected["model_output_truncated"], true);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_model_output_truncates_large_hashlines_and_keeps_full_output_path() {
        let dir = test_dir("read-hashlines-projection");
        fs::create_dir_all(&dir).unwrap();
        let content = (0..600)
            .map(|index| format!("line {index}: {}\n", "x".repeat(32)))
            .collect::<String>();
        let hashlines = content
            .lines()
            .enumerate()
            .map(|(index, line)| format!("{}#AA:{line}\n", index + 1))
            .collect::<String>();
        let output = json!({
            "path": "/tmp/example.py",
            "kind": "text",
            "content": content,
            "hashlines": hashlines,
            "lines_read": 600,
            "truncated": false
        })
        .to_string();

        let projected = serde_json::from_str::<Value>(
            &project_read_model_output_inner("read", &output, &dir).unwrap(),
        )
        .unwrap();
        let full_output_path = projected["full_output_path"].as_str().unwrap();

        assert!(projected.get("content").is_none());
        assert!(projected["hashlines"]
            .as_str()
            .unwrap()
            .contains("model projection omitted"));
        assert_eq!(fs::read_to_string(full_output_path).unwrap(), output);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_reports_image_payloads() {
        let dir = test_dir("read-image");
        let path = dir.join("tiny.png");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, [137, 80, 78, 71]).unwrap();

        let result = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["kind"], "image");
        assert_eq!(value["mime"], "image/png");
        assert_eq!(value["base64"], "iVBORw==");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_supports_utf16_bom_text() {
        let dir = test_dir("read-utf16");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, [0xff, 0xfe, b'h', 0, b'i', 0, b'\n', 0]).unwrap();

        let result = run_tool("read", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["kind"], "text");
        assert_eq!(value["encoding"], "utf-16le");
        assert_eq!(value["content"], "hi\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_requires_read_first() {
        let dir = test_dir("read-before-edit");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "alpha\n").unwrap();

        let result = run_tool(
            "edit",
            &json!({ "path": path, "edits": [{ "oldText": "alpha", "newText": "beta" }] })
                .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("requires reading"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_restore_leaves_a_later_change_unless_forced() {
        let dir = test_dir("checkpoint");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "before\n").unwrap();

        let created = run_tool(
            "checkpoint",
            &json!({ "action": "create", "name": "manual", "paths": [path] }).to_string(),
            &dir,
        );
        let created = serde_json::from_str::<Value>(&created).unwrap();
        fs::write(dir.join("sample.txt"), "after\n").unwrap();
        let restore = |force: bool| {
            let restored = run_tool(
                "checkpoint",
                &json!({ "action": "restore", "id": created["id"], "force": force }).to_string(),
                &dir,
            );
            serde_json::from_str::<Value>(&restored).unwrap()
        };

        let refused = restore(false);
        assert_eq!(refused["restored"], json!([]), "{refused}");
        assert_eq!(refused["conflicts"][0]["path"], "sample.txt");
        assert!(refused["conflicts"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("changed since this session last edited it"));
        assert!(refused["suggestion"].as_str().unwrap().contains("force"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");

        let forced = restore(true);
        assert_eq!(forced["restored"][0], "sample.txt", "{forced}");
        assert_eq!(forced["conflicts"], json!([]));
        assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");
        let _ = fs::remove_dir_all(dir);
    }

    fn session_state(id: &str) -> ToolState {
        let state = ToolState::default();
        state.set_session_id(id);
        state
    }

    fn call_as(state: &ToolState, dir: &Path, name: &str, args: Value) -> Value {
        let output =
            run_tool_with_events(name, &args.to_string(), dir, &[], state, |_| Ok(())).output;
        serde_json::from_str(&output).unwrap()
    }

    /// `state` writes `content` to `path` with the write tool, reading an
    /// existing file first as an agent must.
    fn write_as(state: &ToolState, dir: &Path, path: &str, content: &str) {
        if dir.join(path).exists() {
            call_as(state, dir, "read", json!({ "path": path }));
        }
        let value = call_as(
            state,
            dir,
            "write",
            json!({ "path": path, "content": content }),
        );
        assert!(value.get("error").is_none(), "{value}");
    }

    fn conflict_paths(result: &Value) -> Vec<&str> {
        result["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|conflict| conflict["path"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn rewind_restores_only_this_sessions_edits() {
        let dir = test_dir("rewind-two-sessions");
        fs::create_dir_all(&dir).unwrap();
        for (name, content) in [("a.txt", "a0"), ("b.txt", "b0"), ("shared.txt", "s0")] {
            fs::write(dir.join(name), content).unwrap();
        }
        let (a, b) = (session_state("session-a"), session_state("session-b"));
        let t = now_secs();
        write_as(&a, &dir, "a.txt", "a1");
        write_as(&a, &dir, "new.txt", "created by a");
        write_as(&a, &dir, "shared.txt", "s1");
        // Later, another session works in the same project.
        write_as(&b, &dir, "b.txt", "b1");
        write_as(&b, &dir, "shared.txt", "s2");

        let rewound = restore_to_timestamp(&dir, t, &a).unwrap();
        assert_eq!(rewound["restored"], json!(["a.txt"]), "{rewound}");
        assert_eq!(rewound["removed"], json!(["new.txt"]));
        assert_eq!(conflict_paths(&rewound), vec!["shared.txt"]);
        assert!(rewound["conflicts"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("another session edited it"));
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "a0");
        assert!(!dir.join("new.txt").exists());
        assert_eq!(fs::read_to_string(dir.join("b.txt")).unwrap(), "b1");
        assert_eq!(fs::read_to_string(dir.join("shared.txt")).unwrap(), "s2");

        // B's own rewind undoes only B's edits, back to what A had left.
        let rewound = restore_to_timestamp(&dir, t, &b).unwrap();
        assert_eq!(rewound["conflicts"], json!([]), "{rewound}");
        assert_eq!(fs::read_to_string(dir.join("b.txt")).unwrap(), "b0");
        assert_eq!(fs::read_to_string(dir.join("shared.txt")).unwrap(), "s1");
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "a0");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rewind_leaves_a_file_changed_after_this_session_as_is() {
        let dir = test_dir("rewind-user-change");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("edited.txt"), "v0").unwrap();
        fs::write(dir.join("kept.txt"), "k0").unwrap();
        let a = session_state("session-a");
        let t = now_secs();
        write_as(&a, &dir, "edited.txt", "v1");
        write_as(&a, &dir, "kept.txt", "k1");
        // The user changes the file afterwards, outside any session.
        fs::write(dir.join("edited.txt"), "user's change").unwrap();

        let rewound = restore_to_timestamp(&dir, t, &a).unwrap();
        assert_eq!(rewound["restored"], json!(["kept.txt"]), "{rewound}");
        assert_eq!(conflict_paths(&rewound), vec!["edited.txt"]);
        assert!(rewound["conflicts"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("changed since this session last edited it"));
        assert!(rewound["warning"].as_str().unwrap().contains("conflicts"));
        assert_eq!(
            fs::read_to_string(dir.join("edited.txt")).unwrap(),
            "user's change"
        );
        assert_eq!(fs::read_to_string(dir.join("kept.txt")).unwrap(), "k0");
        let note = restore_conflicts_note(&rewound).unwrap();
        assert!(
            note.starts_with("left 1 file(s) as they are: edited.txt ("),
            "{note}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rewind_never_restores_from_a_checkpoint_without_a_session() {
        let dir = test_dir("rewind-legacy");
        let cp_dir = dir.join(".lynshen").join("checkpoints");
        fs::create_dir_all(&cp_dir).unwrap();
        let t = now_secs();
        // An older version, in another session, edited x.txt and only.txt:
        // its checkpoints record neither a session nor `after`.
        let legacy = json!({
            "id": format!("cp-{}", now_nanos()),
            "name": "auto-edit",
            "created_at": t,
            "files": [
                { "path": "x.txt", "content": "before b" },
                { "path": "only.txt", "content": "o0" }
            ],
            "bytes": 10,
        });
        fs::write(
            cp_dir.join(format!("{}.json", legacy["id"].as_str().unwrap())),
            legacy.to_string(),
        )
        .unwrap();
        fs::write(dir.join("x.txt"), "b's change").unwrap();
        fs::write(dir.join("only.txt"), "o1").unwrap();
        // Then this session edits x.txt, and later.txt, which a checkpoint
        // without a session (taken outside any session) captures after it.
        fs::write(dir.join("later.txt"), "l0").unwrap();
        let a = session_state("session-a");
        write_as(&a, &dir, "x.txt", "a's change");
        write_as(&a, &dir, "later.txt", "l1");
        write_as(&ToolState::default(), &dir, "later.txt", "l2");

        let rewound = restore_to_timestamp(&dir, t, &a).unwrap();
        // x.txt goes back to before this session's edit, keeping B's.
        assert_eq!(rewound["restored"], json!(["x.txt"]), "{rewound}");
        assert_eq!(fs::read_to_string(dir.join("x.txt")).unwrap(), "b's change");
        let mut conflicts = conflict_paths(&rewound);
        conflicts.sort_unstable();
        assert_eq!(conflicts, vec!["later.txt", "only.txt"]);
        assert_eq!(fs::read_to_string(dir.join("only.txt")).unwrap(), "o1");
        assert_eq!(fs::read_to_string(dir.join("later.txt")).unwrap(), "l2");

        // Nor can the model restore such a checkpoint by its id, forced or not.
        let refused = call_as(
            &a,
            &dir,
            "checkpoint",
            json!({ "action": "restore", "id": legacy["id"], "force": true }),
        );
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("older version that did not record sessions"),
            "{refused}"
        );
        assert_eq!(fs::read_to_string(dir.join("x.txt")).unwrap(), "b's change");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_post_tool_hook_counts_as_the_edit_only_where_it_found_what_the_edit_left() {
        let dir = test_dir("rewind-hook-call");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hooked.txt"), "h0").unwrap();
        fs::write(dir.join("raced.txt"), "r0").unwrap();
        let a = session_state("session-a");
        let t = now_secs();
        let collector = collect_checkpoints();
        write_as(&a, &dir, "hooked.txt", "h1");
        write_as(&a, &dir, "raced.txt", "r1");
        let edits = collector.take();
        // The user saves raced.txt before the call's hooks start.
        fs::write(dir.join("raced.txt"), "the user's change").unwrap();
        edits.around_hooks(|| fs::write(dir.join("hooked.txt"), "h1 formatted").unwrap());

        let rewound = restore_to_timestamp(&dir, t, &a).unwrap();
        assert_eq!(rewound["restored"], json!(["hooked.txt"]), "{rewound}");
        assert_eq!(fs::read_to_string(dir.join("hooked.txt")).unwrap(), "h0");
        assert_eq!(conflict_paths(&rewound), vec!["raced.txt"]);
        assert_eq!(
            fs::read_to_string(dir.join("raced.txt")).unwrap(),
            "the user's change"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_collector_takes_only_the_checkpoints_of_its_own_thread() {
        let dir = test_dir("collector-threads");
        fs::create_dir_all(&dir).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (edited_tx, edited_rx) = std::sync::mpsc::channel::<()>();
        // A deferred action collecting on its thread while a turn edits.
        let action = thread::spawn(move || {
            let collector = collect_checkpoints();
            started_tx.send(()).unwrap();
            edited_rx.recv().unwrap();
            collector.take().0.len()
        });
        started_rx.recv().unwrap();
        let collector = collect_checkpoints();
        write_as(&session_state("session-a"), &dir, "turn.txt", "edited");
        edited_tx.send(()).unwrap();
        assert_eq!(action.join().unwrap(), 0);
        assert_eq!(collector.take().0.len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_snapshot_is_on_disk_before_the_edit_and_survives_an_edit_cut_off() {
        let dir = test_dir("checkpoint-before-write");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        fs::write(&path, "before").unwrap();
        let a = session_state("session-a");
        let t = now_secs();
        let pending = begin_checkpoint(&dir, "auto-edit", std::slice::from_ref(&path), &a).unwrap();
        let listed = list_checkpoints(&dir).unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        let id = listed[0]["id"].as_str().unwrap().to_string();
        // The engine dies after writing, before the checkpoint is finished.
        fs::write(&path, "half-written").unwrap();
        drop(pending);

        // Not knowing what the edit left, a rewind leaves the file...
        let rewound = restore_to_timestamp(&dir, t, &a).unwrap();
        assert_eq!(conflict_paths(&rewound), vec!["notes.txt"], "{rewound}");
        assert!(rewound["conflicts"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("did not finish"));
        // ...and the content from before the edit is still there to restore.
        let restored = restore_checkpoint(&dir, &id, true, &a).unwrap();
        assert_eq!(restored["restored"], json!(["notes.txt"]), "{restored}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rewinding_again_goes_back_further() {
        let dir = test_dir("rewind-twice");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "v0").unwrap();
        let a = session_state("session-a");
        // Right after a second starts, so the first edit is taken within it.
        let start = now_secs();
        while now_secs() == start {
            thread::sleep(Duration::from_millis(5));
        }
        let first = now_secs();
        write_as(&a, &dir, "notes.txt", "v1");
        while now_secs() == first {
            thread::sleep(Duration::from_millis(20));
        }
        let second = now_secs();
        write_as(&a, &dir, "notes.txt", "v2");

        let rewound = restore_to_timestamp(&dir, second, &a).unwrap();
        assert_eq!(rewound["restored"], json!(["notes.txt"]), "{rewound}");
        assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "v1");
        // The first rewind counts as this session's edit, so the second one
        // does not see "v1" as someone else's change.
        let rewound = restore_to_timestamp(&dir, first, &a).unwrap();
        assert_eq!(rewound["conflicts"], json!([]), "{rewound}");
        assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "v0");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_restore_refuses_another_sessions_checkpoint() {
        let dir = test_dir("checkpoint-other-session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "v0").unwrap();
        let b = session_state("session-b");
        let created = call_as(
            &b,
            &dir,
            "checkpoint",
            json!({ "action": "create", "paths": ["notes.txt"] }),
        );
        let restored = call_as(
            &session_state("session-a"),
            &dir,
            "checkpoint",
            json!({ "action": "restore", "id": created["id"], "force": true }),
        );
        assert!(restored["error"]
            .as_str()
            .unwrap()
            .contains("another session"));
        let missing = call_as(
            &b,
            &dir,
            "checkpoint",
            json!({ "action": "restore", "id": "../../outside" }),
        );
        assert!(missing["error"].as_str().unwrap().contains("no checkpoint"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn outline_extracts_lightweight_symbols() {
        let dir = test_dir("outline");
        let path = dir.join("lib.rs");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "pub struct App {}\nimpl App {}\npub fn run() {}\n").unwrap();

        let result = run_tool("outline", &json!({ "path": path }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["symbols"][0]["symbol"], "pub struct App {}");
        assert_eq!(value["symbols"][2]["symbol"], "pub fn run() {}");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn write_stdin_sends_input_only_once_per_call() {
        let dir = test_dir("write-stdin-once");
        fs::create_dir_all(&dir).unwrap();

        let result = run_tool(
            "bash",
            &json!({
                "command": "while read line; do echo \"got:$line\"; done",
                "timeout": 10,
                "yield_time_ms": 1
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let session_id = value["session_id"].as_u64().unwrap();

        let _ = run_tool(
            "write_stdin",
            &json!({ "session_id": session_id, "text": "hello\n", "yield_time_ms": 500 })
                .to_string(),
            &dir,
        );
        let result = run_tool(
            "write_stdin",
            &json!({ "session_id": session_id, "yield_time_ms": 300 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        let stdout = value["stdout"].as_str().unwrap();
        assert_eq!(stdout.matches("got:hello").count(), 1, "{stdout:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn polling_a_finished_shell_session_repeats_its_result() {
        let dir = test_dir("write-stdin-finished");
        fs::create_dir_all(&dir).unwrap();
        let result = run_tool(
            "bash",
            &json!({ "command": "sleep 0.3; echo done-once", "timeout": 10, "yield_time_ms": 1 })
                .to_string(),
            &dir,
        );
        let session_id = serde_json::from_str::<Value>(&result).unwrap()["session_id"]
            .as_u64()
            .unwrap();
        let poll = |text: &str| {
            let out = run_tool(
                "write_stdin",
                &json!({ "session_id": session_id, "text": text, "yield_time_ms": 2000 })
                    .to_string(),
                &dir,
            );
            serde_json::from_str::<Value>(&out).unwrap()
        };
        let first = poll("");
        assert_eq!(first["exit_code"], 0, "{first}");
        // The session is gone now; polling again (or writing to it) repeats the
        // result instead of failing with "shell session not found".
        for text in ["", "late input\n"] {
            let again = poll(text);
            assert!(again.get("error").is_none(), "{again}");
            assert_eq!(again["exit_code"], 0, "{again}");
            assert!(
                again["stdout"].as_str().unwrap().contains("done-once"),
                "{again}"
            );
            assert_eq!(again["running"], false);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hashline_edit_accepts_a_bare_line_number_on_a_fresh_read() {
        let dir = test_dir("hashline-bare-line");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "a\nb\nc\n").unwrap();
        let state = ToolState::default();
        let run = |args: Value| {
            serde_json::from_str::<Value>(
                &run_tool_with_events(
                    "hashline_edit",
                    &args.to_string(),
                    &dir,
                    &[],
                    &state,
                    |_| Ok(()),
                )
                .output,
            )
            .unwrap()
        };
        // Not read yet: still refused, so a bare number can never hit a
        // line the model has not seen.
        let refused = run(
            json!({ "path": path, "edits": [{ "op": "replace", "pos": "2", "lines": ["B"] }] }),
        );
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("reading this file first"),
            "{refused}"
        );

        let _ = run_tool_with_events(
            "read",
            &json!({ "path": path }).to_string(),
            &dir,
            &[],
            &state,
            |_| Ok(()),
        );
        let edited = run(
            json!({ "path": path, "edits": [{ "op": "replace", "pos": "2", "lines": ["B"] }] }),
        );
        assert!(edited.get("error").is_none(), "{edited}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nB\nc\n");

        let bad = run(
            json!({ "path": path, "edits": [{ "op": "replace", "pos": "two", "lines": ["x"] }] }),
        );
        assert!(
            bad["error"].as_str().unwrap().contains("E_BAD_REF"),
            "{bad}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_says_what_a_path_is_when_it_cannot_return_text() {
        let dir = test_dir("read-kinds");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("binary.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        fs::write(dir.join("three.txt"), "a\nb\nc\n").unwrap();
        let run = |args: Value| -> Value {
            serde_json::from_str(&run_tool("read", &args.to_string(), &dir)).unwrap()
        };

        let directory = run(json!({ "path": "sub" }));
        let error = directory["error"].as_str().unwrap();
        assert!(error.contains("directory, not a file"), "{error}");
        assert!(error.contains("ls"), "{error}");

        let binary = run(json!({ "path": "binary.bin" }));
        let error = binary["error"].as_str().unwrap();
        assert!(error.contains("not text"), "{error}");
        assert_eq!(binary["kind"], "binary");

        // An offset past the end returns nothing; it now says so.
        let past = run(json!({ "path": "three.txt", "offset": 9 }));
        assert!(past.get("error").is_none(), "{past}");
        assert_eq!(past["lines_read"], 0);
        assert!(
            past["note"]
                .as_str()
                .unwrap()
                .contains("past the end of the file"),
            "{past}"
        );
        assert!(run(json!({ "path": "three.txt" })).get("note").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_anchor_comes_back_with_the_files_current_anchors() {
        let dir = test_dir("hashline-bad-ref");
        let path = dir.join("sample.py");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let state = ToolState::default();
        let run = |args: Value| {
            serde_json::from_str::<Value>(
                &run_tool_with_events(
                    "hashline_edit",
                    &args.to_string(),
                    &dir,
                    &[],
                    &state,
                    |_| Ok(()),
                )
                .output,
            )
            .unwrap()
        };
        let _ = run_tool_with_events(
            "read",
            &json!({ "path": path }).to_string(),
            &dir,
            &[],
            &state,
            |_| Ok(()),
        );

        // A line number with a broken hash: the line's real anchor and its
        // neighbours come back, so the next call can work.
        let bad = run(json!({
            "path": path,
            "edits": [{ "op": "replace", "pos": "3#", "lines": ["THREE"] }]
        }));
        let error = bad["error"].as_str().unwrap();
        assert!(error.starts_with("[E_BAD_REF]"), "{error}");
        assert!(error.contains(">>> 3#"), "{error}");
        assert!(error.contains(":three"), "{error}");
        assert!(
            error.contains(">>> 1#") && error.contains(">>> 5#"),
            "{error}"
        );
        // The anchor it offers is the one the next call needs.
        let anchor = error
            .lines()
            .find_map(|line| line.strip_prefix(">>> "))
            .and_then(|line| line.split(':').next())
            .unwrap()
            .to_string();
        let retry = run(json!({
            "path": path,
            "edits": [{ "op": "replace", "pos": anchor, "lines": ["ONE"] }]
        }));
        assert!(retry.get("error").is_none(), "{retry}");

        // A placeholder names no line: say what an anchor is and where it
        // comes from instead of printing the file.
        let placeholder = run(json!({
            "path": path,
            "edits": [{ "op": "replace", "pos": "${fetch lines}", "lines": ["x"] }]
        }));
        let error = placeholder["error"].as_str().unwrap();
        assert!(error.contains("Expected LINE#HASH"), "{error}");
        assert!(error.contains("read()"), "{error}");
        assert!(!error.contains(">>>"), "{error}");

        // A line number sent as a JSON number is a bare line reference, not
        // a missing anchor.
        let numeric = run(json!({
            "path": path,
            "edits": [{ "op": "replace", "pos": 2, "lines": ["TWO"] }]
        }));
        assert!(numeric.get("error").is_none(), "{numeric}");
        assert!(fs::read_to_string(&path).unwrap().contains("TWO"));
        let zero = run(json!({
            "path": path,
            "edits": [{ "op": "replace", "pos": 0, "lines": ["x"] }]
        }));
        assert!(
            zero["error"].as_str().unwrap().contains("E_BAD_REF"),
            "{zero}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hashline_edit_deletes_last_line_of_file_without_trailing_newline() {
        let dir = test_dir("hashline-delete-last");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "a\nb\nc").unwrap();
        let _ = run_tool("read", &json!({ "path": path }).to_string(), &dir);

        let anchor = format!("3#{}", compute_line_hash(3, "c"));
        let result = run_tool(
            "hashline_edit",
            &json!({
                "path": path,
                "edits": [{ "op": "replace", "pos": anchor, "lines": [] }]
            })
            .to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert!(value.get("error").is_none(), "{value}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_restore_skips_unreadable_files_instead_of_deleting() {
        let dir = test_dir("checkpoint-unreadable");
        let path = dir.join("binary.bin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, [0xff, 0x00, 0x9f]).unwrap();

        let created =
            create_checkpoint(&dir, "manual", std::slice::from_ref(&path), &test_state()).unwrap();
        let id = created["id"].as_str().unwrap();
        let restored = restore_checkpoint(&dir, id, false, &test_state()).unwrap();

        assert_eq!(fs::read(&path).unwrap(), vec![0xff, 0x00, 0x9f]);
        assert_eq!(restored["skipped"][0], "binary.bin");
        assert_eq!(restored["removed"].as_array().unwrap().len(), 0);
        assert!(restored["warning"].as_str().unwrap().contains("untouched"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_to_timestamp_skips_unreadable_files_instead_of_deleting() {
        let dir = test_dir("restore-ts-unreadable");
        let path = dir.join("binary.bin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, [0xff, 0x00]).unwrap();

        create_checkpoint(&dir, "manual", std::slice::from_ref(&path), &test_state()).unwrap();
        let restored = restore_to_timestamp(&dir, 0, &test_state()).unwrap();

        assert_eq!(fs::read(&path).unwrap(), vec![0xff, 0x00]);
        assert_eq!(restored["skipped"][0], "binary.bin");
        assert_eq!(restored["removed"].as_array().unwrap().len(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ripgrep_accepts_pattern_starting_with_dash() {
        let dir = test_dir("rg-dash-pattern");
        let path = dir.join("sample.rs");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "value->unwrap()\n").unwrap();

        let result = run_tool(
            "ripgrep",
            &json!({ "pattern": "->unwrap", "path": path, "literal": true }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["exit_code"], 0, "{value}");
        assert!(value["stdout"].as_str().unwrap().contains("->unwrap"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ripgrep_no_matches_is_success_not_error() {
        let dir = test_dir("rg-no-match");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "haystack\n").unwrap();

        let result = run_tool_with_events(
            "ripgrep",
            &json!({ "pattern": "definitely_not_present_xyz", "path": path }).to_string(),
            &dir,
            &[],
            &test_state(),
            |_| Ok(()),
        );
        let value = serde_json::from_str::<Value>(&result.output).unwrap();

        assert!(!result.is_error, "{value}");
        assert_eq!(value["exit_code"], 0);
        assert!(value["stdout"]
            .as_str()
            .unwrap()
            .contains("no matches found"));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn interrupted_bash_kills_child_process() {
        let dir = test_dir("bash-interrupt-kill");
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker.txt");

        let result = run_tool_with_events(
            "bash",
            &json!({ "command": "sleep 0.4; echo done > marker.txt", "timeout": 10 }).to_string(),
            &dir,
            &[],
            &test_state(),
            |_| Err("interrupted".to_string()),
        );

        assert!(result.is_error);
        std::thread::sleep(Duration::from_millis(900));
        assert!(!marker.exists(), "child kept running after interrupt");
        let _ = fs::remove_dir_all(dir);
    }

    /// Waits for `pid` to stop running. SIGKILL is delivered asynchronously, and a
    /// process that has exited but not been reaped still answers `kill(pid, 0)`, so
    /// a single check right after the kill can observe a survivor that is on its
    /// way out (or already a zombie).
    #[cfg(unix)]
    fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if process_exited(pid) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// True when `pid` is gone. A zombie counts as exited — it holds no CPU and
    /// is only waiting to be reaped, which is the kernel's business — so on
    /// Linux the `/proc` state is checked first. Everywhere else (no `/proc`)
    /// the existence probe alone decides.
    #[cfg(unix)]
    fn process_exited(pid: i32) -> bool {
        let state = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(')').map(|(_, rest)| rest.to_string()))
            .and_then(|rest| rest.split_whitespace().next().map(str::to_string));
        if matches!(state.as_deref(), Some("Z" | "X")) {
            return true;
        }
        unsafe { libc::kill(pid, 0) != 0 }
    }

    #[test]
    fn changed_line_range_handles_edits_inside_multibyte_text() {
        // The common prefix ends inside "着" / "解": the old slice panicked and
        // the edit never returned, leaving the turn waiting forever.
        let original = "第一行\n跳跃数学模型现在自洽着\n第三行\n";
        let updated = "第一行\n跳跃数学模型现在自洽解\n第三行\n";
        assert_eq!(changed_line_range(original, updated), Some((2, 2)));
        let original = "保证任何速度下都有解\n";
        let updated = "保证任何速度下都够跳\n";
        assert_eq!(changed_line_range(original, updated), Some((1, 1)));
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_bash_kills_process_group_descendants() {
        let dir = test_dir("bash-interrupt-group");
        fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("grandchild.pid");

        // The shell backgrounds a grandchild that keeps running after the
        // interrupt; the group kill must take it down too. Fail on the second
        // update so the grandchild has been spawned before the interrupt.
        let emits = std::cell::Cell::new(0);
        let result = run_tool_with_events(
            "bash",
            &json!({ "command": "sleep 30 & echo $! > grandchild.pid; wait", "timeout": 60 })
                .to_string(),
            &dir,
            &[],
            &test_state(),
            |_| {
                emits.set(emits.get() + 1);
                if emits.get() == 1 {
                    Ok(())
                } else {
                    Err("interrupted".to_string())
                }
            },
        );

        assert!(result.is_error);
        let pid: i32 = fs::read_to_string(&pid_file)
            .expect("grandchild pid file")
            .trim()
            .parse()
            .expect("pid");
        assert!(
            wait_for_process_exit(pid, Duration::from_secs(5)),
            "grandchild survived interrupt"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_bash_kills_process_group_descendants() {
        let dir = test_dir("bash-timeout-group");
        fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("grandchild.pid");

        let output = run_tool(
            "bash",
            &json!({ "command": "sleep 30 & echo $! > grandchild.pid; wait", "timeout": 1 })
                .to_string(),
            &dir,
        );
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["timed_out"], true);

        let pid: i32 = fs::read_to_string(&pid_file)
            .expect("grandchild pid file")
            .trim()
            .parse()
            .expect("pid");
        assert!(
            wait_for_process_exit(pid, Duration::from_secs(5)),
            "grandchild survived timeout"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn numeric_params_accept_integer_valued_floats_and_reject_fractions() {
        assert_eq!(optional_u64(&json!({ "x": 30.0 }), "x"), Ok(Some(30)));
        assert_eq!(optional_u64(&json!({ "x": 7 }), "x"), Ok(Some(7)));
        assert_eq!(optional_u64(&json!({}), "x"), Ok(None));
        assert_eq!(optional_u64(&json!({ "x": null }), "x"), Ok(None));
        assert!(optional_u64(&json!({ "x": 1.5 }), "x").is_err());
        assert!(optional_u64(&json!({ "x": -1 }), "x").is_err());
        assert!(optional_u64(&json!({ "x": "3" }), "x").is_err());
    }

    #[test]
    fn read_accepts_float_offset_and_limit() {
        let dir = test_dir("read-float-params");
        let path = dir.join("sample.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let result = run_tool(
            "read",
            &json!({ "path": path, "offset": 2.0, "limit": 1.0 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        assert_eq!(value["content"], "two\n");

        let result = run_tool(
            "read",
            &json!({ "path": path, "limit": 1.5 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        assert!(value["error"].as_str().unwrap().contains("limit"));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn signal_killed_command_is_reported_as_error() {
        let dir = test_dir("bash-signal");
        fs::create_dir_all(&dir).unwrap();

        let result = run_tool_with_events(
            "bash",
            &json!({ "command": "kill -9 $$", "timeout": 10 }).to_string(),
            &dir,
            &[],
            &test_state(),
            |_| Ok(()),
        );
        let value = serde_json::from_str::<Value>(&result.output).unwrap();

        assert!(result.is_error, "{value}");
        assert!(value["exit_code"].is_null());
        assert!(value["error"].as_str().unwrap().contains("signal 9"));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn workspace_path_allows_inside_and_rejects_outside() {
        let dir = env::temp_dir().join(format!("lynshen-policy-basic-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("inside.txt"), "ok").unwrap();

        assert!(workspace_path(&dir, "inside.txt").is_ok());
        assert!(workspace_path(&dir, "sub/../inside.txt").is_ok());
        assert!(workspace_path(&dir, "new/dir/file.txt").is_ok());
        assert!(workspace_path(&dir, &dir.join("inside.txt").display().to_string()).is_ok());

        let error = workspace_path(&dir, "../escape.txt").unwrap_err();
        assert!(error.contains("escapes the workspace"), "{error}");
        assert!(workspace_path(&dir, "/etc/passwd").is_err());
        assert!(workspace_path(&dir, "sub/../../escape.txt").is_err());
        // `..` inside a not-yet-existing prefix must not escape either.
        assert!(workspace_path(&dir, "missing/../../escape.txt").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn an_approved_image_directory_takes_the_images_named_in_it() {
        let pid = std::process::id();
        let workspace = env::temp_dir().join(format!("lynshen-image-out-ws-{pid}"));
        let outside = PathBuf::from(if cfg!(target_os = "macos") {
            "/Users/Shared"
        } else {
            "/dev/shm"
        })
        .join(format!("lynshen-image-out-{pid}"));
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let outside = outside.canonicalize().unwrap();
        // The approval names the directory the images go into.
        let base = outside.join("fox").display().to_string();
        let args = json!({ "prompt": "a fox", "path": base }).to_string();
        let target =
            outside_write_target(crate::images::TOOL_NAME, &args, &workspace, None).unwrap();
        assert_eq!(target, outside);
        let state = test_state().allowing_writes(vec![target]);
        for name in ["fox.png", "fox-2.png"] {
            assert!(write_target(
                &workspace,
                &outside.join(name).display().to_string(),
                &state
            )
            .is_ok());
        }
        // Not an existing file there, nor anything deeper or elsewhere.
        fs::write(outside.join("old.png"), "x").unwrap();
        assert!(write_target(
            &workspace,
            &outside.join("old.png").display().to_string(),
            &state
        )
        .is_err());
        assert!(write_target(
            &workspace,
            &outside.join("sub/x.png").display().to_string(),
            &state
        )
        .is_err());
        // A file tool's approval is for its file only.
        let notes = outside.join("notes.md");
        let state = test_state().allowing_writes(vec![notes.clone()]);
        assert!(write_target(&workspace, &notes.display().to_string(), &state).is_ok());
        assert!(write_target(
            &workspace,
            &outside.join("other.md").display().to_string(),
            &state
        )
        .is_err());
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[cfg(not(windows))]
    #[test]
    fn read_tools_read_outside_the_workspace_and_writes_need_approval() {
        let pid = std::process::id();
        let workspace = env::temp_dir().join(format!("lynshen-read-roots-ws-{pid}"));
        let outside = env::temp_dir().join(format!("lynshen-read-roots-out-{pid}"));
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(outside.join("refs")).unwrap();
        fs::write(outside.join("notes.txt"), "outside notes").unwrap();
        let run = |name: &str, args: Value, state: &ToolState| {
            run_tool_with_events(name, &args.to_string(), &workspace, &[], state, |_| Ok(()))
        };
        let state = test_state();

        // Reads go anywhere but credentials.
        let result = run(
            "read",
            json!({ "path": outside.join("notes.txt").display().to_string() }),
            &state,
        );
        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains("outside notes"));
        let result = run(
            "ls",
            json!({ "path": outside.display().to_string() }),
            &state,
        );
        assert!(!result.is_error, "{}", result.output);

        // A write outside needs this call's approval.
        let target = outside.join("new.txt");
        let args = json!({ "path": target.display().to_string(), "content": "x" });
        let result = run("write", args.clone(), &state);
        assert!(result.is_error);
        assert!(
            result.output.contains("needs the user's approval"),
            "{}",
            result.output
        );
        assert!(!target.exists());
        let allowed = state.allowing_writes(vec![target.clone()]);
        let result = run("write", args, &allowed);
        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains("checkpoints"), "{}", result.output);
        assert_eq!(fs::read_to_string(&target).unwrap(), "x");
        // The approval covers that path only.
        let other =
            json!({ "path": outside.join("other.txt").display().to_string(), "content": "x" });
        assert!(run("write", other, &allowed).is_error);

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn file_tools_write_outside_the_workspace_only_when_allowed() {
        let dir = env::temp_dir().join(format!("lynshen-policy-tools-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let writes: [(&str, Value); 3] = [
            ("write", json!({ "path": "../escape.txt", "content": "x" })),
            (
                "str_replace",
                json!({ "path": "/etc/passwd", "edits": [{ "oldText": "a", "newText": "b" }] }),
            ),
            (
                "hashline_edit",
                json!({ "path": "/etc/passwd", "edits": [{ "op": "append", "lines": "x" }] }),
            ),
        ];
        for (tool, args) in writes {
            let result = run_tool(tool, &args.to_string(), &dir);
            let value = serde_json::from_str::<Value>(&result).unwrap();
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                error.contains("needs the user's approval"),
                "{tool}: {result}"
            );
        }
        assert!(!dir.parent().unwrap().join("escape.txt").exists());
        // Reads are open; checkpoints stay inside the workspace.
        for (tool, args) in [
            ("read", json!({ "path": "/etc/hosts" })),
            ("ls", json!({ "path": ".." })),
        ] {
            let result = run_tool(tool, &args.to_string(), &dir);
            assert!(
                !result.contains("escapes the workspace"),
                "{tool}: {result}"
            );
        }
        let result = run_tool(
            "checkpoint",
            &json!({ "action": "create", "name": "cp", "paths": ["../escape.txt"] }).to_string(),
            &dir,
        );
        assert!(result.contains("escapes the workspace"), "{result}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_rejects_targets_outside_the_workspace() {
        let dir = env::temp_dir().join(format!("lynshen-policy-patch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let patch = "--- /dev/null\n+++ b/../evil.txt\n@@ -0,0 +1 @@\n+evil\n";
        let result = run_tool("apply_patch", &json!({ "patch": patch }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(error.contains("escapes the workspace"), "{result}");
        assert!(!dir.parent().unwrap().join("evil.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_tools_treat_symlinks_out_of_the_workspace_as_outside() {
        let outside =
            env::temp_dir().join(format!("lynshen-policy-outside-{}", std::process::id()));
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        let dir = env::temp_dir().join(format!("lynshen-policy-symlink-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();

        // Reading through it is a read outside: allowed. Writing through it
        // is a write outside: it needs approval.
        let result = run_tool(
            "read",
            &json!({ "path": "link/secret.txt" }).to_string(),
            &dir,
        );
        assert!(result.contains("secret"), "{result}");
        let result = run_tool(
            "write",
            &json!({ "path": "link/new.txt", "content": "x" }).to_string(),
            &dir,
        );
        assert!(result.contains("needs the user's approval"), "{result}");
        assert!(!outside.join("new.txt").exists());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn resolve_path_expands_tilde_to_home() {
        let home = env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap();
        assert_eq!(
            resolve_path(Path::new("/cwd"), "~/x"),
            Path::new(&home).join("x")
        );
        assert_eq!(resolve_path(Path::new("/cwd"), "~"), PathBuf::from(&home));
        assert_eq!(
            resolve_path(Path::new("/cwd"), "~x"),
            Path::new("/cwd").join("~x")
        );
    }

    #[test]
    fn decode_text_bytes_strips_utf8_bom() {
        let (text, encoding) = decode_text_bytes(b"\xef\xbb\xbfhi\n").unwrap();
        assert_eq!(text, "hi\n");
        assert_eq!(encoding, "utf-8");
    }

    #[test]
    fn outline_truncated_only_when_symbols_are_dropped() {
        let dir = test_dir("outline-truncated");
        let path = dir.join("lib.rs");
        fs::create_dir_all(&dir).unwrap();
        let filler = "// filler\n".repeat(300);
        fs::write(&path, format!("fn one() {{}}\nfn two() {{}}\n{filler}")).unwrap();

        let result = run_tool(
            "outline",
            &json!({ "path": path, "limit": 2 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        assert_eq!(value["truncated"], false, "{value}");

        fs::write(&path, "fn one() {}\nfn two() {}\nfn three() {}\n").unwrap();
        let result = run_tool(
            "outline",
            &json!({ "path": path, "limit": 2 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(value["symbols"].as_array().unwrap().len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn apply_patch_diff_excludes_unrelated_workspace_changes() {
        let dir = test_dir("apply-patch-scoped-diff");
        fs::create_dir_all(&dir).unwrap();
        run_command("git", &["init"], &dir, Duration::from_secs(30)).unwrap();
        fs::write(dir.join("sample.txt"), "alpha\nbeta\n").unwrap();
        fs::write(dir.join("unrelated.txt"), "junk\n").unwrap();

        let patch = r#"diff --git a/sample.txt b/sample.txt
--- a/sample.txt
+++ b/sample.txt
@@ -1,2 +1,2 @@
 alpha
-beta
+gamma
"#;
        run_tool("read", &json!({ "path": "sample.txt" }).to_string(), &dir);
        let result = run_tool("apply_patch", &json!({ "patch": patch }).to_string(), &dir);
        let value = serde_json::from_str::<Value>(&result).unwrap();

        assert_eq!(value["applied"], true, "{value}");
        let diff = value["diff"].as_str().unwrap();
        assert!(diff.contains("sample.txt"));
        assert!(!diff.contains("unrelated.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edits_keep_a_crlf_files_line_endings() {
        let dir = test_dir("crlf-edits");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("win.txt");
        let call = |name: &str, args: Value| -> Value {
            serde_json::from_str(&run_tool(name, &args.to_string(), &dir)).unwrap()
        };
        let assert_crlf = |expected: &str| {
            let content = fs::read_to_string(&path).unwrap();
            assert_eq!(content, expected.replace('\n', "\r\n"));
            assert!(!content.replace("\r\n", "").contains('\n'), "{content:?}");
        };
        fs::write(&path, "alpha\r\nbeta\r\n-- note\r\ngamma\r\n").unwrap();
        call("read", json!({ "path": "win.txt" }));

        // The model writes LF line breaks, as it usually does.
        let edited = call(
            "str_replace",
            json!({ "path": "win.txt", "edits": [{ "oldText": "beta\n-- note", "newText": "beta\nBETA" }] }),
        );
        assert_eq!(edited["edits"], 1, "{edited}");
        assert_crlf("alpha\nbeta\nBETA\ngamma\n");

        let edited = call(
            "hashline_edit",
            json!({ "path": "win.txt", "edits": [
                { "op": "replace", "pos": 1, "lines": ["one", "two"] },
                { "op": "append", "lines": "tail\n" }
            ] }),
        );
        assert!(edited.get("error").is_none(), "{edited}");
        assert_crlf("one\ntwo\nbeta\nBETA\ngamma\ntail\n");

        let patch =
            "--- a/win.txt\n+++ b/win.txt\n@@ -3,3 +3,3 @@\n beta\n-BETA\n+-- back\n gamma\n";
        let patched = call("apply_patch", json!({ "patch": patch }));
        assert_eq!(patched["applied"], true, "{patched}");
        assert_crlf("one\ntwo\nbeta\n-- back\ngamma\ntail\n");

        // A removed line that starts like a header stays in its hunk.
        let patch = "--- a/win.txt\n+++ b/win.txt\n@@ -3,3 +3,2 @@\n beta\n--- back\n gamma\n";
        let patched = call("apply_patch", json!({ "patch": patch }));
        assert_eq!(patched["applied"], true, "{patched}");
        assert_crlf("one\ntwo\nbeta\ngamma\ntail\n");

        let written = call("write", json!({ "path": "win.txt", "content": "x\ny\n" }));
        assert!(written.get("error").is_none(), "{written}");
        assert_crlf("x\ny\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lf_files_and_mostly_lf_files_count_as_lf() {
        assert!(uses_crlf("a\r\nb\r\nc\n"));
        assert!(!uses_crlf("a\nb\r\nc\n"));
        assert!(!uses_crlf("a\nb\n"));
        assert!(!uses_crlf("no line break"));
    }

    #[test]
    fn apply_patch_requires_reading_files_it_changes_but_not_ones_it_creates() {
        let dir = test_dir("apply-patch-read");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        fs::write(&path, "alpha\nbeta\n").unwrap();
        let call = |name: &str, args: Value| -> Value {
            serde_json::from_str(&run_tool(name, &args.to_string(), &dir)).unwrap()
        };
        let edit = |from: &str, to: &str| {
            format!("--- a/sample.txt\n+++ b/sample.txt\n@@ -1,2 +1,2 @@\n alpha\n-{from}\n+{to}\n")
        };

        let blind = call("apply_patch", json!({ "patch": edit("beta", "gamma") }));
        assert!(blind["error"]
            .as_str()
            .unwrap()
            .contains("apply_patch requires reading"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\nbeta\n");

        let created = call(
            "apply_patch",
            json!({ "patch": "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+fresh\n" }),
        );
        assert_eq!(created["applied"], true, "{created}");
        assert_eq!(fs::read_to_string(dir.join("new.txt")).unwrap(), "fresh\n");

        call("read", json!({ "path": "sample.txt" }));
        let first = call("apply_patch", json!({ "patch": edit("beta", "gamma") }));
        assert_eq!(first["applied"], true, "{first}");
        // The patch marks the file read: the next edit needs no new read.
        let second = call("apply_patch", json!({ "patch": edit("gamma", "delta") }));
        assert_eq!(second["applied"], true, "{second}");

        fs::write(&path, "alpha\nchanged by someone else\n").unwrap();
        let stale = call("apply_patch", json!({ "patch": edit("delta", "epsilon") }));
        assert!(stale["error"]
            .as_str()
            .unwrap()
            .contains("changed on disk since you last read it"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn edits_to_a_file_with_mixed_line_endings_touch_only_the_lines_they_insert() {
        let dir = test_dir("mixed-endings");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mixed.txt");
        let call = |name: &str, args: Value| -> Value {
            serde_json::from_str(&run_tool(name, &args.to_string(), &dir)).unwrap()
        };
        let reset = |content: &str| {
            fs::write(&path, content).unwrap();
            call("read", json!({ "path": "mixed.txt" }));
        };
        let content = || fs::read_to_string(&path).unwrap();

        // Mostly CRLF: the untouched LF line stays LF, new lines get CRLF.
        reset("l1\r\nl2\r\nl3\nl4\r\n");
        let edited = call(
            "str_replace",
            json!({ "path": "mixed.txt", "edits": [{ "oldText": "l1\nl2", "newText": "one\nl2" }] }),
        );
        assert_eq!(edited["edits"], 1, "{edited}");
        assert_eq!(content(), "one\r\nl2\r\nl3\nl4\r\n");

        reset("l1\r\nl2\r\nl3\nl4\r\n");
        let edited = call(
            "hashline_edit",
            json!({ "path": "mixed.txt", "edits": [{ "op": "replace", "pos": 1, "lines": ["one", "two"] }] }),
        );
        assert!(edited.get("error").is_none(), "{edited}");
        assert_eq!(content(), "one\r\ntwo\r\nl2\r\nl3\nl4\r\n");

        reset("l1\r\nl2\r\nl3\nl4\r\n");
        let patch =
            "--- a/mixed.txt\n+++ b/mixed.txt\n@@ -1,4 +1,5 @@\n-l1\n+one\n+two\n l2\n l3\n l4\n";
        let patched = call("apply_patch", json!({ "patch": patch }));
        assert_eq!(patched["applied"], true, "{patched}");
        assert_eq!(content(), "one\r\ntwo\r\nl2\r\nl3\nl4\r\n");

        // Mostly LF: the one CRLF line stays CRLF, new lines get LF.
        reset("a\nb\r\nc\nd\n");
        let edited = call(
            "str_replace",
            json!({ "path": "mixed.txt", "edits": [{ "oldText": "c", "newText": "C\r\nC2" }] }),
        );
        assert_eq!(edited["edits"], 1, "{edited}");
        assert_eq!(content(), "a\nb\r\nC\nC2\nd\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_crlf_patch_keeps_an_empty_context_line_written_without_its_space() {
        let dir = test_dir("crlf-empty-context");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("win.txt");
        fs::write(&path, "a\r\n\r\nb\r\n").unwrap();
        run_tool("read", &json!({ "path": "win.txt" }).to_string(), &dir);
        // Models often drop the space of an empty context line.
        let patch = "--- a/win.txt\n+++ b/win.txt\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
        let patched = serde_json::from_str::<Value>(&run_tool(
            "apply_patch",
            &json!({ "patch": patch }).to_string(),
            &dir,
        ))
        .unwrap();
        assert_eq!(patched["applied"], true, "{patched}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\r\n\r\nc\r\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn patch_targets_include_renames_copies_and_bare_git_headers_but_not_hunk_lines() {
        let dir = Path::new("/work");
        let patch = "\
diff --git a/old.rs b/new.rs
similarity index 100%
rename from old.rs
rename to new.rs
diff --git a/src.rs b/copy.rs
similarity index 100%
copy from src.rs
copy to copy.rs
diff --git a/empty.txt b/empty.txt
new file mode 100644
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
--- a/notes.md
+++ b/notes.md
@@ -1,2 +1,1 @@
 keep
--- not-a-header.md
";
        let targets = patch_target_paths(patch, dir);
        let names = targets
            .iter()
            .map(|path| {
                path.strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "old.rs",
                "new.rs",
                "copy.rs",
                "empty.txt",
                "run.sh",
                "notes.md"
            ]
        );
    }

    #[test]
    fn a_rename_patch_needs_a_read_and_rewind_undoes_it() {
        let dir = test_dir("rename-patch");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("old.txt"), "content\n").unwrap();
        let a = session_state("session-a");
        let rename = |from: &str, to: &str| {
            format!(
                "diff --git a/{from} b/{to}\nsimilarity index 100%\nrename from {from}\nrename to {to}\n"
            )
        };
        let t = now_secs();
        let blind = call_as(
            &a,
            &dir,
            "apply_patch",
            json!({ "patch": rename("old.txt", "new.txt") }),
        );
        assert!(
            blind["error"]
                .as_str()
                .unwrap()
                .contains("apply_patch requires reading"),
            "{blind}"
        );
        assert!(dir.join("old.txt").exists());

        call_as(&a, &dir, "read", json!({ "path": "old.txt" }));
        let renamed = call_as(
            &a,
            &dir,
            "apply_patch",
            json!({ "patch": rename("old.txt", "new.txt") }),
        );
        assert_eq!(renamed["applied"], true, "{renamed}");
        assert!(!dir.join("old.txt").exists());

        let rewound = restore_to_timestamp(&dir, t, &a).unwrap();
        assert_eq!(rewound["restored"], json!(["old.txt"]), "{rewound}");
        assert_eq!(rewound["removed"], json!(["new.txt"]));
        assert_eq!(
            fs::read_to_string(dir.join("old.txt")).unwrap(),
            "content\n"
        );
        assert!(!dir.join("new.txt").exists());

        // A renamed file changed since is left in place.
        // Checkpoints have one-second resolution: start the second rewind
        // point after the first part's checkpoints.
        let first_part = now_secs();
        while now_secs() == first_part {
            thread::sleep(Duration::from_millis(20));
        }
        let t = now_secs();
        call_as(&a, &dir, "read", json!({ "path": "old.txt" }));
        let renamed = call_as(
            &a,
            &dir,
            "apply_patch",
            json!({ "patch": rename("old.txt", "moved.txt") }),
        );
        assert_eq!(renamed["applied"], true, "{renamed}");
        fs::write(dir.join("moved.txt"), "the user's change\n").unwrap();
        let rewound = restore_to_timestamp(&dir, t, &a).unwrap();
        assert_eq!(rewound["restored"], json!(["old.txt"]), "{rewound}");
        assert_eq!(conflict_paths(&rewound), vec!["moved.txt"]);
        assert_eq!(
            fs::read_to_string(dir.join("moved.txt")).unwrap(),
            "the user's change\n"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_stdin_null_text_falls_back_to_chars() {
        let dir = test_dir("write-stdin-null-text");
        fs::create_dir_all(&dir).unwrap();
        let command = if cfg!(windows) {
            "$line = [Console]::In.ReadLine(); Write-Output $line"
        } else {
            "head -n 1"
        };

        let result = run_tool(
            "exec_command",
            &json!({ "cmd": command, "timeout": 5, "yield_time_ms": 1 }).to_string(),
            &dir,
        );
        let value = serde_json::from_str::<Value>(&result).unwrap();
        let session_id = value["session_id"].as_u64().unwrap();

        let mut value = json!({ "running": true });
        for index in 0..20 {
            let args = if index == 0 {
                json!({ "session_id": session_id, "text": null, "chars": "hello\n", "yield_time_ms": 250 })
            } else {
                json!({ "session_id": session_id, "yield_time_ms": 250 })
            };
            let result = run_tool("write_stdin", &args.to_string(), &dir);
            value = serde_json::from_str::<Value>(&result).unwrap();
            if value["running"] != true {
                break;
            }
        }

        assert_eq!(value["exit_code"], 0, "{value}");
        assert!(value["stdout"].as_str().unwrap().contains("hello"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_read_licenses_edits_only_for_the_engine_that_read() {
        let dir = test_dir("read-tracker-scope");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "old").unwrap();
        let reader = ToolState::default();
        let other = ToolState::default();
        let call = |name: &str, args: Value, state: &ToolState| {
            run_tool_with_events(name, &args.to_string(), &dir, &[], state, |_| Ok(())).output
        };
        call("read", json!({ "path": "notes.txt" }), &reader);

        let blind = call(
            "write",
            json!({ "path": "notes.txt", "content": "x" }),
            &other,
        );
        assert!(blind.contains("requires reading"), "{blind}");
        assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "old");

        let informed = call(
            "write",
            json!({ "path": "notes.txt", "content": "new" }),
            &reader,
        );
        assert!(!informed.contains("error"), "{informed}");
        assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_sessions_result_is_repeated_to_any_engine() {
        let dir = test_dir("shell-finished-foreign");
        fs::create_dir_all(&dir).unwrap();
        let owner = ToolState::default();
        let call = |name: &str, args: Value, state: &ToolState| -> Value {
            serde_json::from_str(
                &run_tool_with_events(name, &args.to_string(), &dir, &[], state, |_| Ok(())).output,
            )
            .unwrap()
        };
        let started = call(
            "bash",
            json!({ "command": "sleep 0.3; echo finished-here", "timeout": 10, "yield_time_ms": 1 }),
            &owner,
        );
        let session_id = started["session_id"].as_u64().expect("a session");
        let done = call(
            "write_stdin",
            json!({ "session_id": session_id, "text": "", "yield_time_ms": 3000 }),
            &owner,
        );
        assert_eq!(done["exit_code"], 0, "{done}");

        // Another engine in this process polls the same id: the command's
        // own result, not "no running shell session".
        let other = ToolState::default();
        let repeated = call(
            "write_stdin",
            json!({ "session_id": session_id, "text": "", "yield_time_ms": 10 }),
            &other,
        );
        assert!(repeated.get("error").is_none(), "{repeated}");
        assert!(repeated["stdout"]
            .as_str()
            .unwrap()
            .contains("finished-here"));
        assert_eq!(repeated["running"], false);

        // An id this process never issued stays an error, and says why
        // nothing is there to poll.
        let unknown = call(
            "write_stdin",
            json!({ "session_id": session_id + 10_000, "text": "" }),
            &other,
        );
        let error = unknown["error"].as_str().unwrap();
        assert!(error.contains("no running shell session"), "{error}");
        assert!(error.contains("returns its whole output"), "{error}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_stdin_reaches_only_the_engines_own_shells() {
        let dir = test_dir("shell-owner");
        fs::create_dir_all(&dir).unwrap();
        let owner = ToolState::default();
        let other = ToolState::default();
        let call = |name: &str, args: Value, state: &ToolState| -> Value {
            let output =
                run_tool_with_events(name, &args.to_string(), &dir, &[], state, |_| Ok(())).output;
            serde_json::from_str(&output).unwrap()
        };
        let started = call(
            "bash",
            json!({ "command": "sleep 5", "timeout": 10, "yield_time_ms": 100 }),
            &owner,
        );
        let session_id = started["session_id"].as_u64().expect("still running");

        let foreign = call(
            "write_stdin",
            json!({ "session_id": session_id, "text": "" }),
            &other,
        );
        assert!(
            foreign["error"]
                .as_str()
                .unwrap()
                .contains("no running shell session"),
            "{foreign}"
        );
        let own = call(
            "write_stdin",
            json!({ "session_id": session_id, "text": "", "yield_time_ms": 10 }),
            &owner,
        );
        assert!(own.get("error").is_none(), "{own}");
    }

    #[cfg(not(windows))]
    #[test]
    fn file_writes_inside_the_boundary_need_no_approval() {
        use crate::sandbox::{SandboxMode, SandboxPolicy};
        let dir = test_dir("sandbox-temp-read");
        fs::create_dir_all(&dir).unwrap();
        let shot = test_dir("sandbox-temp-shot");
        fs::create_dir_all(&shot).unwrap();
        fs::write(shot.join("screenshot.txt"), "image bytes").unwrap();
        let file = shot.join("screenshot.txt").display().to_string();
        let run = |tool: &str, args: Value, state: &ToolState| -> Value {
            serde_json::from_str(
                &run_tool_with_events(tool, &args.to_string(), &dir, &[], state, |_| Ok(())).output,
            )
            .unwrap()
        };
        let write = json!({ "path": shot.join("new.txt").display().to_string(), "content": "x" });

        // No sandbox: reads go anywhere, writes stay in the workspace.
        let plain = ToolState::default();
        let read = run("read", json!({ "path": file }), &plain);
        assert!(read["content"].as_str().unwrap().contains("image bytes"));
        let refused = run("write", write.clone(), &plain);
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("needs the user's approval"),
            "{refused}"
        );

        // workspace-write: temp is inside the boundary, as for commands.
        let sandboxed = ToolState::default();
        sandboxed.set_sandbox(Some(SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            writable_dirs: Vec::new(),
            readable_dirs: Vec::new(),
            network: true,
            rules: Vec::new(),
            sandboxed_allow: Vec::new(),
        }));
        for (tool, args) in [
            ("ls", json!({ "path": shot.display().to_string() })),
            ("outline", json!({ "path": file })),
            (
                "ripgrep",
                json!({ "path": shot.display().to_string(), "pattern": "image" }),
            ),
        ] {
            let result = run(tool, args, &sandboxed);
            assert!(result.get("error").is_none(), "{tool}: {result}");
        }
        let written = run("write", write.clone(), &sandboxed);
        assert!(written.get("error").is_none(), "{written}");
        assert!(shot.join("new.txt").exists());

        // read-only: nothing is written, inside or out.
        let read_only = ToolState::default();
        read_only.set_sandbox(Some(SandboxPolicy {
            mode: SandboxMode::ReadOnly,
            ..SandboxPolicy::default_for_platform()
        }));
        let refused = run("write", write, &read_only);
        assert!(
            refused["error"].as_str().unwrap().contains("read-only"),
            "{refused}"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&shot);
    }

    #[test]
    fn reads_never_open_credentials() {
        let dir = test_dir("read-roots-secret");
        fs::create_dir_all(&dir).unwrap();
        let secrets = crate::sandbox::denied_reads();
        let Some(ssh) = secrets.iter().find(|path| path.ends_with(".ssh")) else {
            return;
        };
        // The home directory is readable; ~/.ssh stays closed.
        let error = readable_path(&dir, &ssh.join("id_rsa").display().to_string())
            .expect_err("credentials must not be readable");
        assert!(error.contains("credentials"), "{error}");
        assert!(readable_path(&dir, &ssh.parent().unwrap().display().to_string()).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worktree_subagent_works_on_the_parents_paths_in_its_worktree() {
        let project = test_dir("confined-paths");
        let worktree = project.join(".lynshen/agents/task-1");
        fs::create_dir_all(worktree.join("src")).unwrap();
        fs::create_dir_all(project.join(".lynshen/agents/sibling")).unwrap();
        fs::write(worktree.join("src/text.py"), "print(1)\n").unwrap();
        fs::write(project.join(".lynshen/agents/sibling/notes.md"), "theirs").unwrap();
        let state = ToolState::default().confined_to(worktree.clone(), project.clone());
        let run = |tool: &str, args: Value| -> Value {
            serde_json::from_str(
                &run_tool_with_events(tool, &args.to_string(), &worktree, &[], &state, |_| Ok(()))
                    .output,
            )
            .unwrap()
        };

        // The task named the parent's path; the same file in the worktree is
        // what the agent gets.
        let parent_file = project.join("src/text.py").display().to_string();
        let read = run("read", json!({ "path": parent_file }));
        assert!(read.get("error").is_none(), "{read}");
        assert!(read["content"].as_str().unwrap().contains("print(1)"));
        assert_eq!(
            read["path"].as_str().unwrap(),
            worktree.join("src/text.py").display().to_string()
        );
        let list = run(
            "ls",
            json!({ "path": project.join("src").display().to_string() }),
        );
        assert_eq!(list["entries"][0], "text.py", "{list}");

        // Writes land in the worktree too, and the write-isolation gate sees
        // the mapped path, so it lets the call through.
        let arguments = json!({ "path": parent_file, "content": "print(2)\n" }).to_string();
        let mapped = map_confined_arguments("write", &arguments, &state).expect("mapped");
        assert!(
            write_target_escapes_root("write", &mapped, &worktree, &worktree).is_none(),
            "{mapped}"
        );
        assert!(write_target_escapes_root("write", &arguments, &worktree, &worktree).is_some());
        let write = run(
            "write",
            json!({ "path": parent_file, "content": "print(2)\n" }),
        );
        assert!(write.get("error").is_none(), "{write}");
        assert_eq!(
            fs::read_to_string(worktree.join("src/text.py")).unwrap(),
            "print(2)\n"
        );
        assert!(!project.join("src/text.py").exists());

        // The other direction: .lynshen/agents is never mapped, so a sibling
        // agent's workspace stays out of reach and a path already inside the
        // worktree is used as it is.
        let sibling = project
            .join(".lynshen/agents/sibling/notes.md")
            .display()
            .to_string();
        assert_eq!(map_project_path(&sibling, &worktree, &project), None);
        // Reads are not confined: the path is read as it is, not mapped.
        let read = run("read", json!({ "path": sibling }));
        assert!(
            !read.to_string().contains("escapes the workspace"),
            "{read}"
        );
        let own = worktree.join("src/text.py").display().to_string();
        assert_eq!(map_project_path(&own, &worktree, &project), None);
        // A path outside the parent's root is left alone as well.
        assert_eq!(map_project_path("/etc/hosts", &worktree, &project), None);
        assert_eq!(map_project_path("src/text.py", &worktree, &project), None);
        // The parent root itself is the worktree root.
        assert_eq!(
            map_project_path(&project.display().to_string(), &worktree, &project),
            Some(worktree.clone())
        );
        let _ = fs::remove_dir_all(&project);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_command_the_sandbox_stops_is_told_to_escalate() {
        use crate::sandbox::{SandboxMode, SandboxPolicy};
        let dir = test_dir("sandbox-hint");
        fs::create_dir_all(dir.join(".git")).unwrap();
        let state = ToolState::default();
        state.set_sandbox(Some(SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            ..SandboxPolicy::default_for_platform()
        }));
        let run = |command: &str| -> Value {
            serde_json::from_str(
                &run_tool_with_events(
                    "bash",
                    &json!({ "command": command }).to_string(),
                    &dir,
                    &[],
                    &state,
                    |_| Ok(()),
                )
                .output,
            )
            .unwrap()
        };
        // .git is read-only in the sandbox.
        let blocked = run("echo x > .git/config");
        assert!(
            blocked["sandbox_hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("escalate")),
            "{blocked}"
        );
        assert!(!dir.join(".git/config").exists());
        // A failure of its own gets no hint.
        let failed = run("ls missing-file");
        assert!(failed.get("sandbox_hint").is_none(), "{failed}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_writes_follow_the_sandbox() {
        use crate::sandbox::{SandboxMode, SandboxPolicy};
        let dir = test_dir("sandbox-writes");
        let extra = test_dir("sandbox-extra");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(&extra).unwrap();
        let state = ToolState::default();
        let policy = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            writable_dirs: vec![extra.clone()],
            readable_dirs: Vec::new(),
            network: true,
            rules: Vec::new(),
            sandboxed_allow: Vec::new(),
        };
        state.set_sandbox(Some(policy.clone()));
        let write = |state: &ToolState, path: String| -> Value {
            let args = json!({ "path": path, "content": "x" }).to_string();
            serde_json::from_str(
                &run_tool_with_events("write", &args, &dir, &[], state, |_| Ok(())).output,
            )
            .unwrap()
        };
        assert!(write(&state, "src/a.txt".into()).get("error").is_none());
        let blocked = write(&state, ".git/config".into());
        assert!(
            blocked["error"]
                .as_str()
                .unwrap()
                .contains("read-only in the sandbox"),
            "{blocked}"
        );
        // A read-write directory outside the workspace is writable.
        let outside = extra.join("deploy.sh").display().to_string();
        assert!(write(&state, outside).get("error").is_none());
        assert!(extra.join("deploy.sh").exists());

        let read_only = ToolState::default();
        read_only.set_sandbox(Some(SandboxPolicy {
            mode: SandboxMode::ReadOnly,
            ..policy
        }));
        assert!(write(&read_only, "src/b.txt".into())["error"]
            .as_str()
            .unwrap()
            .contains("read-only"));
        assert!(!dir.join("src/b.txt").exists());
    }

    #[test]
    fn a_confined_agents_commands_start_in_its_worktree() {
        let project = test_dir("confined");
        let worktree = project.join(".lynshen/agents/w");
        fs::create_dir_all(worktree.join("sub")).unwrap();
        let state = ToolState::default().confined_to(worktree.clone(), project.clone());
        let run = |args: Value| -> Value {
            serde_json::from_str(
                &run_tool_with_events(
                    "bash",
                    &args.to_string(),
                    &worktree,
                    &[],
                    &state,
                    |_| Ok(()),
                )
                .output,
            )
            .unwrap()
        };
        let outside =
            run(json!({ "command": "echo hi", "workdir": project.display().to_string() }));
        assert!(
            outside["error"]
                .as_str()
                .unwrap()
                .contains("outside your worktree"),
            "{outside}"
        );
        let escaped = run(json!({ "command": "echo hi", "workdir": "../.." }));
        assert!(escaped["error"].as_str().is_some(), "{escaped}");
        let inside = run(json!({ "command": "echo hi", "workdir": "sub" }));
        assert!(inside.get("error").is_none(), "{inside}");
        // Its sandbox has no read-write directories of the parent's.
        state.set_sandbox(Some(crate::sandbox::SandboxPolicy {
            writable_dirs: vec![project.clone()],
            ..crate::sandbox::SandboxPolicy::default_for_platform()
        }));
        assert!(state.sandbox().unwrap().writable_dirs.is_empty());
        assert_eq!(
            state.for_subagent().confine_root(),
            Some(worktree.as_path())
        );
        let _ = fs::remove_dir_all(project);
    }

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("lynshen-tools-test-{name}-{nanos}"))
    }
}

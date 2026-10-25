//! Agent Client Protocol agents (JSON-RPC 2.0 over stdio lines, protocol v1,
//! https://agentclientprotocol.com): `lynshen acp`, `gemini --experimental-acp`
//! and the like, started with the command the desktop's agent registry names.
//!
//! - Handshake: `initialize` → `session/new` (the agent's session id).
//! - A turn is `session/prompt`, answered with its `stopReason` once it
//!   settles; `session/update` notifications stream the reply, thoughts, tool
//!   calls and plans meanwhile. There is no turn-started notification.
//! - Permission prompts are `session/request_permission` requests answered
//!   with one of the options the agent offered.
//! - One prompt at a time: messages sent during a turn wait here
//!   (`pending_messages`) and run as the next turns; `unqueue` takes one back
//!   (or drops it), `queue_move` reorders them, and `steer` (or `interrupt`
//!   with `index`) cancels the turn so the chosen one runs as soon as it
//!   settles.
//! - Reopening: the daemon keeps the agent's session id (`conversation`), and
//!   a reopened session asks for it again: `session/load` when the agent
//!   offers it (`loadSession`; it replays the conversation, which becomes the
//!   session's transcript), else `session/resume` (`sessionCapabilities.resume`,
//!   no replay). An agent that offers neither, or no longer has the
//!   conversation, starts a new one, and clients are told.

use super::{find_program, op_files, opencode, Adapter, Line, Options, Output};
use lynshen_agent_core::{
    attachments,
    queue::{queue_move_target, QueuePick},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Command,
};

pub const PROTOCOL_VERSION: u64 = 1;

/// A message waiting for the session or the running turn: what the user
/// sent, made into prompt blocks when it goes.
struct Waiting {
    text: String,
    images: Vec<String>,
    /// Other files, sent as `resource_link`s.
    files: Vec<String>,
}

pub fn command(options: &Options) -> Command {
    let program = options.command.as_deref().unwrap_or_default();
    let path = Path::new(program);
    let program = if path.components().count() > 1 || path.is_absolute() {
        path.to_path_buf()
    } else {
        find_program(program, &[])
    };
    let mut command = Command::new(program);
    command.args(&options.args);
    command
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn block_text(block: &Value) -> &str {
    match text(&block["type"]) {
        "text" => text(&block["text"]),
        "resource_link" => text(&block["uri"]),
        "resource" => text(&block["resource"]["uri"]),
        _ => "",
    }
}

/// A tool call's `diff` content as a unified-diff-ish string.
fn diff_text(item: &Value) -> String {
    let path = text(&item["path"]);
    let mut lines = vec![format!("--- {path}"), format!("+++ {path}")];
    if let Some(old) = item["oldText"].as_str().filter(|old| !old.is_empty()) {
        lines.extend(old.split('\n').map(|line| format!("-{line}")));
    }
    lines.extend(
        text(&item["newText"])
            .split('\n')
            .map(|line| format!("+{line}")),
    );
    lines.join("\n")
}

fn image_mime(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

#[derive(Default)]
struct Tool {
    name: String,
    kind: String,
    path: String,
    text: String,
    diff: String,
}

impl Tool {
    fn output(&self) -> String {
        let mut out = json!({ "kind": self.kind });
        for (key, value) in [
            ("path", &self.path),
            ("content", &self.text),
            ("diff", &self.diff),
        ] {
            if !value.is_empty() {
                out[key] = json!(value);
            }
        }
        out.to_string()
    }

    fn absorb(&mut self, content: &Value) {
        for item in content.as_array().into_iter().flatten() {
            match text(&item["type"]) {
                "content" => {
                    let chunk = block_text(&item["content"]);
                    if !chunk.is_empty() {
                        if !self.text.is_empty() {
                            self.text.push('\n');
                        }
                        self.text.push_str(chunk);
                    }
                }
                "diff" => {
                    if self.path.is_empty() {
                        self.path = text(&item["path"]).to_string();
                    }
                    if !self.diff.is_empty() {
                        self.diff.push('\n');
                    }
                    self.diff.push_str(&diff_text(item));
                }
                _ => {}
            }
        }
    }
}

/// The model an agent reports when a session opens or changes: the
/// `models` field (`currentModelId`, shown by its name), else the session
/// config option of category `model` (its current value, shown by its name).
fn current_model(result: &Value) -> String {
    let current = &result["models"]["currentModelId"];
    if !text(current).is_empty() {
        return result["models"]["availableModels"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| &m["modelId"] == current)
            .map(|m| text(&m["name"]).to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| text(current).to_string());
    }
    let Some(option) = result["configOptions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["category"] == "model" || (o["category"].is_null() && o["id"] == "model"))
    else {
        return String::new();
    };
    let value = &option["currentValue"];
    // Options are a list, or groups of lists.
    option["options"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|o| match o["options"].as_array() {
            Some(group) => group.iter().collect::<Vec<_>>(),
            None => vec![o],
        })
        .find(|o| &o["value"] == value)
        .map(|o| text(&o["name"]).to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| text(value).to_string())
}

/// The conversation an agent replays while `session/load` runs, as
/// transcript rows.
#[derive(Default)]
struct Replay {
    rows: Vec<Value>,
    /// The message the last text row belongs to (`messageId`), when the
    /// agent names them: a new message starts a new row.
    message: Option<String>,
}

impl Replay {
    fn text(&mut self, role: &str, update: &Value) {
        let chunk = block_text(&update["content"]);
        if chunk.is_empty() {
            return;
        }
        let message = update["messageId"].as_str().map(str::to_string);
        let same = self.rows.last().is_some_and(|row| row["role"] == role)
            && (message.is_none() || message == self.message);
        if same {
            if let Some(Value::String(content)) =
                self.rows.last_mut().map(|row| &mut row["content"])
            {
                content.push_str(chunk);
            }
        } else {
            self.rows.push(json!({ "role": role, "content": chunk }));
        }
        self.message = message;
    }

    /// A file attached to the user's message being replayed (a new user row
    /// when the files come first).
    fn file(&mut self, path: String) {
        if !self.rows.last().is_some_and(|row| row["role"] == "user") {
            self.rows.push(json!({ "role": "user", "content": "" }));
        }
        if let Some(last) = self.rows.last_mut() {
            let mut files = last["files"].as_array().cloned().unwrap_or_default();
            files.push(json!(path));
            last["files"] = json!(files);
        }
    }

    fn tool(&mut self, tool: &Tool) {
        self.message = None;
        self.rows
            .push(json!({ "role": "tool", "name": tool.name, "output": tool.output() }));
    }
}

pub struct Acp {
    cwd: PathBuf,
    next_id: u64,
    pending: HashMap<u64, String>,
    session: Option<String>,
    image_prompts: bool,
    prompt_in_flight: bool,
    busy_announced: bool,
    /// Messages waiting for the session or the running turn.
    queued: VecDeque<Waiting>,
    session_failed: bool,
    /// Synthetic call id → (request id, the options offered).
    approvals: HashMap<String, (Value, Vec<Value>)>,
    approval_seq: u64,
    tools: HashMap<String, Tool>,
    /// The agent's conversation to reopen, until it is open.
    reopen: Option<String>,
    /// A conversation of the agent's to copy into this session, until the
    /// copy is open.
    fork: Option<String>,
    /// Reopen without a replay when the agent can (a restart).
    quiet: bool,
    /// What `session/load` or `session/fork` replays, until it answers.
    replay: Option<Replay>,
    /// The agent is OpenCode: approval modes are its agents (`opencode`).
    opencode: bool,
    /// OpenCode: the client's approval mode, and the OpenCode agent the
    /// session runs on.
    mode: Option<String>,
    agent: Option<&'static str>,
    /// A mode switch is on its way to the agent: prompts wait for it.
    configuring: bool,
    /// OpenCode: the switch on its way (request id, the agent it asks for).
    switch: Option<(u64, &'static str)>,
    /// OpenCode has no session config options: switch with `session/set_mode`.
    set_mode: bool,
    /// OpenCode did not switch to the agent of a mode other than 全自动:
    /// why. Messages are refused until a switch succeeds, since the agent
    /// it runs on may run tools the mode would ask for.
    mode_failed: Option<String>,
    /// Why this agent cannot run here (OpenCode 2.x).
    refused: Option<String>,
}

impl Acp {
    /// `options.resume`: the agent's session id of a conversation to reopen;
    /// `options.fork`: one to copy instead.
    pub fn new(cwd: &Path, options: &Options) -> Self {
        let reopen = options.resume.clone().filter(|id| !id.is_empty());
        let opencode = opencode::is_opencode(options);
        Self {
            cwd: cwd.to_path_buf(),
            next_id: 0,
            pending: HashMap::new(),
            session: None,
            image_prompts: false,
            prompt_in_flight: false,
            busy_announced: false,
            queued: VecDeque::new(),
            session_failed: false,
            approvals: HashMap::new(),
            approval_seq: 0,
            tools: HashMap::new(),
            fork: options
                .fork
                .clone()
                .filter(|id| !id.is_empty() && reopen.is_none()),
            reopen,
            quiet: options.quiet,
            replay: None,
            opencode,
            mode: options.approval_mode.clone().filter(|_| opencode),
            agent: None,
            configuring: false,
            switch: None,
            set_mode: false,
            mode_failed: None,
            refused: None,
        }
    }

    /// OpenCode: switches the session to the agent `mode` runs on, unless it
    /// runs there already. Prompts wait until the agent has switched. One
    /// switch is on its way at a time, so OpenCode cannot apply them out of
    /// order; a mode picked meanwhile is switched to once it is answered.
    fn switch_agent(&mut self) -> Vec<String> {
        let (Some(mode), Some(session)) = (self.mode.clone(), self.session.clone()) else {
            return Vec::new();
        };
        let agent = opencode::agent_for(&mode);
        if self.agent == Some(agent) {
            return Vec::new();
        }
        self.agent = Some(agent);
        // Messages wait for this switch, and go if it succeeds.
        self.mode_failed = None;
        self.configuring = true;
        if self.switch.is_some() {
            return Vec::new();
        }
        vec![self.switch_request(&session, agent)]
    }

    /// Asks OpenCode to run the session on `agent`.
    fn switch_request(&mut self, session: &str, agent: &'static str) -> String {
        self.configuring = true;
        let frame = if self.set_mode {
            self.request(
                "session/set_mode",
                json!({ "sessionId": session, "modeId": agent }),
            )
        } else {
            self.request(
                "session/set_config_option",
                json!({ "sessionId": session, "configId": "mode", "value": agent }),
            )
        };
        self.switch = Some((self.next_id, agent));
        frame
    }

    /// OpenCode answered a mode switch (`switch_agent`).
    fn switched(&mut self, id: u64, method: &str, result: &Value, error: &Value) -> Output {
        let Some((_, asked)) = self.switch.filter(|(switch, _)| *switch == id) else {
            return Output::default();
        };
        self.switch = None;
        let failure = (!error.is_null()).then(|| match text(&error["message"]) {
            "" => format!("JSON-RPC error {}", error["code"]),
            message => message.to_string(),
        });
        // An OpenCode without session config options (1.0 has
        // `session/set_mode` only): the same switch as a session mode.
        let unknown = failure.as_deref().is_some_and(|message| {
            error["code"] == -32601 || message.contains("unknown config option")
        });
        let wanted = self.agent;
        if method == "session/set_config_option" && unknown {
            self.set_mode = true;
        }
        // A mode picked meanwhile, or the same switch the other way.
        let again = wanted != Some(asked) || (method == "session/set_config_option" && unknown);
        if let (true, Some(agent), Some(session)) = (again, wanted, self.session.clone()) {
            return Output {
                events: vec![],
                frames: vec![self.switch_request(&session, agent)],
            };
        }
        if let Some(message) = failure {
            return self.switch_failed(asked, &message);
        }
        // OpenCode answers with its options as they are now.
        let current = result["configOptions"]
            .as_array()
            .and_then(|options| options.iter().find(|option| option["id"] == "mode"))
            .and_then(|option| option["currentValue"].as_str());
        if let Some(current) = current.filter(|current| *current != asked) {
            return self.switch_failed(asked, &format!("it still runs on {current}"));
        }
        self.configuring = false;
        self.mode_failed = None;
        self.next_prompt().unwrap_or_default()
    }

    /// OpenCode did not switch to `agent`. 全自动 runs anyway: it is OpenCode
    /// as the user configured it, and the session's agent is no looser.
    /// Any other mode fails closed: waiting messages are dropped and new ones
    /// refused until a switch succeeds.
    fn switch_failed(&mut self, agent: &'static str, message: &str) -> Output {
        self.agent = None;
        self.configuring = false;
        let mode = opencode::mode_label(self.mode.as_deref().unwrap_or_default()).to_string();
        if agent == "build" {
            let mut output = Output::events(vec![
                json!({ "type": "error", "message": format!("[opencode] OpenCode did not switch to {mode} ({message}); it runs on its current agent") }),
            ]);
            if let Some(next) = self.next_prompt() {
                output.frames.extend(next.frames);
                output.events.extend(next.events);
            }
            return output;
        }
        let reason = format!(
            "[opencode] OpenCode did not switch to {mode} ({message}). Messages are not sent until it does: on its current agent, tools could run without asking. Pick the mode again, or update OpenCode (1.x)."
        );
        self.mode_failed = Some(reason.clone());
        let mut events = vec![json!({ "type": "error", "message": reason })];
        if !self.queued.is_empty() {
            self.queued.clear();
            events.push(self.pending_event());
            events.push(json!({ "type": "error", "message": "[opencode] The waiting message was not sent." }));
            if !self.prompt_in_flight {
                events.push(json!({ "type": "status", "message": "ready" }));
            }
        }
        Output::events(events)
    }

    /// The next queued message, when the session can take one: its prompt
    /// and the events that say it started (the queue without it).
    fn next_prompt(&mut self) -> Option<Output> {
        if self.session.is_none()
            || self.prompt_in_flight
            || self.configuring
            || self.mode_failed.is_some()
        {
            return None;
        }
        let next = self.queued.pop_front()?;
        let blocks = self.blocks(&next);
        let prompt = self.prompt(blocks);
        Some(Output {
            events: vec![self.pending_event(), json!({ "type": "connecting" })],
            frames: vec![prompt],
        })
    }

    /// The prompt blocks of a message: its text, then its images when the
    /// agent takes them, then a `resource_link` per other file.
    fn blocks(&self, message: &Waiting) -> Vec<Value> {
        let mut blocks = vec![json!({ "type": "text", "text": message.text })];
        if self.image_prompts {
            for image in &message.images {
                blocks.push(json!({ "type": "image", "mimeType": image_mime(image), "data": "", "uri": format!("file://{image}") }));
            }
        }
        blocks.extend(message.files.iter().map(|path| {
            json!({ "type": "resource_link", "uri": attachments::file_uri(path), "name": attachments::file_name(path) })
        }));
        blocks
    }

    fn waiting_texts(&self) -> Vec<&str> {
        self.queued.iter().map(|m| m.text.as_str()).collect()
    }

    fn pending_event(&self) -> Value {
        json!({ "type": "pending_messages", "messages": self.waiting_texts() })
    }

    fn request(&mut self, method: &str, params: Value) -> String {
        self.next_id += 1;
        self.pending.insert(self.next_id, method.to_string());
        json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params })
            .to_string()
    }

    fn prompt(&mut self, mut blocks: Vec<Value>) -> String {
        // OpenCode reminds its own plan agent by its name; ours by a block
        // only the model reads.
        if self.agent == Some(opencode::PLAN_AGENT) {
            blocks.push(opencode::plan_reminder());
        }
        self.prompt_in_flight = true;
        self.busy_announced = false;
        let session = self.session.clone();
        self.request(
            "session/prompt",
            json!({ "sessionId": session, "prompt": blocks }),
        )
    }

    fn new_session(&mut self) -> String {
        let cwd = self.cwd.clone();
        self.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
    }

    /// The first request after `initialize`: the conversation to reopen, as
    /// the agent can (see the module docs), else a new one. The events say
    /// when a conversation cannot be reopened.
    fn open_session(&mut self, capabilities: &Value) -> Output {
        let cwd = self.cwd.clone();
        if let Some(source) = self.fork.take() {
            if capabilities["sessionCapabilities"]["fork"].is_object() {
                self.replay = Some(Replay::default());
                let frame = self.request(
                    "session/fork",
                    json!({ "sessionId": source, "cwd": cwd, "mcpServers": [] }),
                );
                return Output {
                    events: vec![],
                    frames: vec![frame],
                };
            }
            return Output {
                events: vec![
                    json!({ "type": "error", "message": "[acp] this agent cannot copy a conversation; a new one starts" }),
                ],
                frames: vec![self.new_session()],
            };
        }
        let Some(id) = self.reopen.clone() else {
            return Output {
                events: vec![],
                frames: vec![self.new_session()],
            };
        };
        let resumes = capabilities["sessionCapabilities"]["resume"].is_object();
        if self.quiet && resumes {
            let frame = self.request(
                "session/resume",
                json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] }),
            );
            return Output {
                events: vec![],
                frames: vec![frame],
            };
        }
        if capabilities["loadSession"] == true {
            self.replay = Some(Replay::default());
            let frame = self.request(
                "session/load",
                json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] }),
            );
            return Output {
                events: vec![],
                frames: vec![frame],
            };
        }
        if resumes {
            let frame = self.request(
                "session/resume",
                json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] }),
            );
            return Output {
                events: vec![],
                frames: vec![frame],
            };
        }
        self.reopen = None;
        Output {
            events: vec![
                json!({ "type": "info", "message": "[acp] this agent cannot reopen a conversation; a new one starts" }),
            ],
            frames: vec![self.new_session()],
        }
    }

    /// The agent opened a session (`session/new`, or a reopened one): its
    /// model, ready, and the first queued message.
    fn opened(&mut self, result: &Value) -> Output {
        let model = current_model(result);
        let mut events = vec![
            json!({ "type": "startup", "model": model, "cwd": self.cwd, "session_id": "", "context_window": 0 }),
        ];
        if !model.is_empty() {
            events.push(json!({ "type": "model_status", "provider": "acp", "model": model, "reasoning_effort": "", "reasoning_efforts": [], "context_window": 0, "context_limit": 0 }));
        }
        events.push(json!({ "type": "status", "message": "ready" }));
        // OpenCode: the agent of the client's mode, before the first prompt.
        let mut frames = self.switch_agent();
        if let Some(mode) = &self.mode {
            events.push(json!({ "type": "approval_mode", "mode": mode }));
        }
        if let Some(next) = self.next_prompt() {
            frames.extend(next.frames);
            events.extend(next.events);
        }
        Output { events, frames }
    }

    /// The turn settled: start the next queued prompt, or go ready.
    fn settle(&mut self) -> Output {
        self.prompt_in_flight = false;
        self.busy_announced = false;
        self.next_prompt().unwrap_or_else(|| {
            Output::events(vec![json!({ "type": "status", "message": "ready" })])
        })
    }

    fn on_update(&mut self, update: &Value) -> Vec<Value> {
        // While `session/load` runs, the agent replays the conversation: it
        // becomes the transcript, sent once the load answers.
        if self.replay.is_some() {
            self.replayed(update);
            return vec![];
        }
        match text(&update["sessionUpdate"]) {
            "agent_message_chunk" => match block_text(&update["content"]) {
                "" => vec![],
                chunk => vec![json!({ "type": "assistant_delta", "delta": chunk })],
            },
            "agent_thought_chunk" => match block_text(&update["content"]) {
                "" => vec![],
                chunk => vec![json!({ "type": "reasoning_delta", "delta": chunk })],
            },
            "tool_call" => {
                let id = text(&update["toolCallId"]).to_string();
                if id.is_empty() {
                    return vec![];
                }
                let name = [text(&update["title"]), text(&update["kind"])]
                    .into_iter()
                    .find(|n| !n.is_empty())
                    .unwrap_or("tool");
                let mut tool = Tool {
                    name: name.to_string(),
                    kind: match text(&update["kind"]) {
                        "" => "other",
                        kind => kind,
                    }
                    .to_string(),
                    path: text(&update["locations"][0]["path"]).to_string(),
                    ..Tool::default()
                };
                tool.absorb(&update["content"]);
                let mut events = vec![
                    json!({ "type": "tool_start", "call_id": id, "name": tool.name }),
                    json!({ "type": "tool_update", "call_id": id, "output": tool.output() }),
                ];
                let status = text(&update["status"]);
                if matches!(status, "completed" | "failed") {
                    events.push(json!({ "type": "tool_output", "call_id": id, "name": tool.name, "output": tool.output(), "is_error": status == "failed" }));
                } else {
                    self.tools.insert(id, tool);
                }
                events
            }
            "tool_call_update" => {
                let id = text(&update["toolCallId"]).to_string();
                let Some(tool) = self.tools.get_mut(&id) else {
                    return vec![];
                };
                if !text(&update["title"]).is_empty() {
                    tool.name = text(&update["title"]).to_string();
                }
                if !text(&update["locations"][0]["path"]).is_empty() {
                    tool.path = text(&update["locations"][0]["path"]).to_string();
                }
                tool.absorb(&update["content"]);
                let status = text(&update["status"]);
                if matches!(status, "completed" | "failed") {
                    let tool = self.tools.remove(&id).unwrap_or_default();
                    vec![
                        json!({ "type": "tool_output", "call_id": id, "name": tool.name, "output": tool.output(), "is_error": status == "failed" }),
                    ]
                } else {
                    vec![json!({ "type": "tool_update", "call_id": id, "output": tool.output() })]
                }
            }
            "plan" => {
                let plan: Vec<Value> = update["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|entry| json!({ "step": text(&entry["content"]), "status": match text(&entry["status"]) { "" => "pending", s => s } }))
                    .collect();
                vec![json!({ "type": "plan", "plan": plan })]
            }
            "config_option_update" => match current_model(update).as_str() {
                "" => vec![],
                model => vec![
                    json!({ "type": "model_status", "provider": "acp", "model": model, "reasoning_effort": "", "reasoning_efforts": [], "context_window": 0, "context_limit": 0 }),
                ],
            },
            _ => vec![],
        }
    }

    /// One update of a conversation being replayed: its text and finished
    /// tool calls as transcript rows.
    fn replayed(&mut self, update: &Value) {
        let Some(replay) = self.replay.as_mut() else {
            return;
        };
        match text(&update["sessionUpdate"]) {
            // The plan reminder is not what the user wrote.
            "user_message_chunk" if opencode::for_the_model_only(&update["content"]) => {}
            // A file the user attached (as `blocks` links it).
            "user_message_chunk" if update["content"]["type"] == "resource_link" => {
                if let Some(path) = attachments::uri_path(text(&update["content"]["uri"])) {
                    replay.file(path);
                }
            }
            "user_message_chunk" => replay.text("user", update),
            "agent_message_chunk" => replay.text("assistant", update),
            "agent_thought_chunk" => replay.text("reasoning", update),
            "tool_call" | "tool_call_update" => {
                let id = text(&update["toolCallId"]).to_string();
                if id.is_empty() {
                    return;
                }
                let tool = self.tools.entry(id.clone()).or_default();
                if let Some(name) = [text(&update["title"]), text(&update["kind"])]
                    .into_iter()
                    .find(|name| !name.is_empty())
                {
                    tool.name = name.to_string();
                }
                if !text(&update["kind"]).is_empty() {
                    tool.kind = text(&update["kind"]).to_string();
                }
                if !text(&update["locations"][0]["path"]).is_empty() {
                    tool.path = text(&update["locations"][0]["path"]).to_string();
                }
                tool.absorb(&update["content"]);
                if matches!(text(&update["status"]), "completed" | "failed") {
                    let mut tool = self.tools.remove(&id).unwrap_or_default();
                    if tool.name.is_empty() {
                        tool.name = "tool".to_string();
                    }
                    if tool.kind.is_empty() {
                        tool.kind = "other".to_string();
                    }
                    replay.tool(&tool);
                }
            }
            _ => {}
        }
    }

    fn on_response(&mut self, id: u64, result: &Value, error: &Value) -> Output {
        let Some(method) = self.pending.remove(&id) else {
            return Output::default();
        };
        if matches!(
            method.as_str(),
            "session/set_config_option" | "session/set_mode"
        ) {
            return self.switched(id, &method, result, error);
        }
        if !error.is_null() {
            let message = match text(&error["message"]) {
                "" => format!("JSON-RPC error {}", error["code"]),
                message => message.to_string(),
            };
            // The conversation to reopen or copy is gone (or the agent
            // refused it): a new one starts, and clients are told.
            if matches!(
                method.as_str(),
                "session/load" | "session/resume" | "session/fork"
            ) {
                self.replay = None;
                self.tools.clear();
                self.reopen = None;
                let event = if method == "session/fork" {
                    json!({ "type": "error", "message": format!("[acp] the agent could not copy this conversation ({message}); a new one starts") })
                } else {
                    json!({ "type": "info", "message": format!("[acp] the agent could not reopen this conversation ({message}); a new one starts") })
                };
                return Output {
                    events: vec![event],
                    frames: vec![self.new_session()],
                };
            }
            let mut output = Output::events(vec![
                json!({ "type": "error", "message": format!("[acp] {message}") }),
            ]);
            if method == "session/prompt" {
                let settled = self.settle();
                output.events.extend(settled.events);
                output.frames.extend(settled.frames);
            }
            if method == "session/new" || method == "initialize" {
                if !self.queued.is_empty() {
                    self.queued.clear();
                    output.events.push(self.pending_event());
                    output.events.push(json!({ "type": "error", "message": "The agent could not open a session; the messages waiting for it were not sent" }));
                }
                self.session_failed = method == "session/new";
                output
                    .events
                    .push(json!({ "type": "status", "message": "ready" }));
            }
            return output;
        }
        match method.as_str() {
            "initialize" => {
                if let Some(reason) = opencode::unsupported(&result["agentInfo"]) {
                    self.queued.clear();
                    self.refused = Some(reason.clone());
                    return Output::events(vec![
                        self.pending_event(),
                        json!({ "type": "error", "message": reason }),
                        json!({ "type": "status", "message": "ready" }),
                    ]);
                }
                let capabilities = &result["agentCapabilities"];
                self.image_prompts = capabilities["promptCapabilities"]["image"] == true;
                self.open_session(capabilities)
            }
            "session/new" => {
                self.session = result["sessionId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                self.opened(result)
            }
            // The reopened conversation keeps its id; a load replayed it. A
            // copy has an id of its own and replayed too.
            "session/load" | "session/resume" | "session/fork" => {
                self.session = match method.as_str() {
                    "session/fork" => result["sessionId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_string),
                    _ => self.reopen.take(),
                };
                self.tools.clear();
                let mut events = Vec::new();
                if let Some(replay) = self.replay.take() {
                    events.push(json!({ "type": "transcript", "items": replay.rows }));
                }
                let mut output = self.opened(result);
                events.append(&mut output.events);
                output.events = events;
                output
            }
            "session/prompt" => {
                let mut output = Output::default();
                match text(&result["stopReason"]) {
                    "refusal" => output.events.push(json!({ "type": "info", "message": "[acp] the agent refused this request" })),
                    reason @ ("max_tokens" | "max_turn_requests") => {
                        output.events.push(json!({ "type": "info", "message": format!("[acp] the turn stopped at a limit ({reason})") }))
                    }
                    _ => {}
                }
                let settled = self.settle();
                output.events.extend(settled.events);
                output.frames.extend(settled.frames);
                output
            }
            _ => Output::default(),
        }
    }

    fn on_request(&mut self, id: &Value, method: &str, params: &Value) -> Output {
        if method != "session/request_permission" {
            // The client offers no file system or terminal (`initialize`):
            // an agent that asks anyway (opencode mirrors each approved edit
            // to `fs/write_text_file`) has done the work itself, so only
            // other requests are worth a notice.
            let not_offered = method.starts_with("fs/") || method.starts_with("terminal/");
            return Output {
                frames: vec![json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unsupported by client: {method}") } }).to_string()],
                events: if not_offered {
                    vec![]
                } else {
                    vec![json!({ "type": "info", "message": format!("[acp] unsupported request: {method}") })]
                },
            };
        }
        let options = params["options"].as_array().cloned().unwrap_or_default();
        // OpenCode in 自动编辑: edits go ahead without asking.
        let edits = self.opencode && self.mode.as_deref().is_some_and(opencode::allows_edits);
        if edits && params["toolCall"]["kind"] == "edit" {
            let allow = options
                .iter()
                .find(|o| o["kind"] == "allow_once" && !text(&o["optionId"]).is_empty());
            if let Some(allow) = allow {
                return Output {
                    frames: vec![json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": "selected", "optionId": allow["optionId"] } } }).to_string()],
                    events: vec![],
                };
            }
        }
        self.approval_seq += 1;
        let call = format!("acp-approval-{}", self.approval_seq);
        self.approvals.insert(call.clone(), (id.clone(), options));
        let tool = &params["toolCall"];
        let mut summary = text(&tool["title"]).to_string();
        for item in tool["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "diff")
        {
            summary.push('\n');
            summary.push_str(&diff_text(item));
        }
        let kind = match text(&tool["kind"]) {
            "" => "tool",
            kind => kind,
        };
        if summary.is_empty() {
            summary = kind.to_string();
        }
        Output::events(vec![
            json!({ "type": "approval_request", "call_id": call, "name": kind, "summary": summary, "subagent_id": null, "hunks": null }),
        ])
    }
}

impl Adapter for Acp {
    fn start(&mut self) -> Vec<String> {
        vec![self.request(
            "initialize",
            json!({ "protocolVersion": PROTOCOL_VERSION, "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false } }),
        )]
    }

    fn translate(&mut self, line: Line) -> Output {
        let frame = match line {
            // Diagnostics only; kept for an exit message (Session::note_stderr).
            Line::Stderr(_) => return Output::default(),
            Line::Frame(frame) => frame,
        };
        // No turn-started notification: the first frame after a prompt
        // means the turn is running.
        let announce = self.prompt_in_flight && !self.busy_announced;
        if announce {
            self.busy_announced = true;
        }
        let id = &frame["id"];
        let has_id = id.is_u64() || id.is_string();
        let mut output = match frame["method"].as_str() {
            Some(method) if has_id => self.on_request(id, method, &frame["params"]),
            Some("session/update") => Output::events(self.on_update(&frame["params"]["update"])),
            Some(_) => Output::default(),
            None => match id.as_u64() {
                Some(id) => self.on_response(id, &frame["result"], &frame["error"]),
                None => Output::default(),
            },
        };
        if announce {
            output.events.insert(0, json!({ "type": "connecting" }));
        }
        output
    }

    fn encode(&mut self, op: &Value) -> Result<Output, String> {
        if let Some(reason) = &self.refused {
            if op["op"] != "shutdown" {
                return Err(reason.clone());
            }
        }
        let frames = match text(&op["op"]) {
            "user_message" => {
                if let Some(reason) = &self.mode_failed {
                    return Err(reason.clone());
                }
                let message = Waiting {
                    text: text(&op["content"]).to_string(),
                    images: op["images"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|image| text(image).to_string())
                        .collect(),
                    files: op_files(op),
                };
                if self.session.is_none() || self.prompt_in_flight || self.configuring {
                    self.queued.push_back(message);
                    let frames = if self.session.is_none() && self.session_failed {
                        self.session_failed = false;
                        vec![self.new_session()]
                    } else {
                        vec![]
                    };
                    return Ok(Output {
                        events: vec![self.pending_event()],
                        frames,
                    });
                } else {
                    let blocks = self.blocks(&message);
                    vec![self.prompt(blocks)]
                }
            }
            // A waiting message taken back (dropped with `discard`).
            "unqueue" => {
                let Some(message) = QueuePick::from_op(op)
                    .find(&self.waiting_texts())
                    .and_then(|at| self.queued.remove(at))
                else {
                    return Ok(Output::events(vec![self.pending_event()]));
                };
                let mut events = vec![self.pending_event()];
                if op["discard"] != true {
                    let mut unqueued = json!({ "type": "unqueued", "content": message.text, "text": message.text, "images": message.images });
                    super::set_files(&mut unqueued, &message.files);
                    events.push(unqueued);
                }
                return Ok(Output::events(events));
            }
            // A waiting message to another place: `index` to `to`.
            "queue_move" => {
                let Some((from, to)) = queue_move_target(
                    &QueuePick::from_op(op),
                    op["to"].as_i64(),
                    &self.waiting_texts(),
                ) else {
                    return Ok(Output::default());
                };
                if let Some(item) = self.queued.remove(from) {
                    self.queued.insert(to, item);
                }
                return Ok(Output::events(vec![self.pending_event()]));
            }
            "approve" => {
                let call = text(&op["call_id"]);
                let Some((request, options)) = self.approvals.remove(call) else {
                    return Err(format!("no open approval {call}"));
                };
                let kinds: &[&str] = if op["decision"] == "deny" {
                    &["reject_once", "reject_always"]
                } else if op["always"] == true {
                    &["allow_always", "allow_once"]
                } else {
                    &["allow_once", "allow_always"]
                };
                let option = kinds.iter().find_map(|kind| {
                    options
                        .iter()
                        .find(|o| o["kind"] == *kind && !text(&o["optionId"]).is_empty())
                });
                let outcome = match option {
                    Some(option) => {
                        json!({ "outcome": "selected", "optionId": option["optionId"] })
                    }
                    None => json!({ "outcome": "cancelled" }),
                };
                vec![
                    json!({ "jsonrpc": "2.0", "id": request, "result": { "outcome": outcome } })
                        .to_string(),
                ]
            }
            // `steer` (or `interrupt` with `index`): an agent cannot take a
            // message mid-turn, so the turn is cancelled and, once it
            // settles, the chosen waiting message starts the next one, ahead
            // of the others. A plain `interrupt` drops what waits.
            "steer" | "interrupt" => {
                let Some(session) = self.session.clone() else {
                    return Ok(Output::default());
                };
                let send_now = op["op"] == "steer" || op["index"].is_i64();
                let mut events = Vec::new();
                if send_now {
                    let Some(at) = QueuePick::from_op(op).find(&self.waiting_texts()) else {
                        return Ok(Output::default());
                    };
                    if !self.prompt_in_flight {
                        return Ok(Output::default());
                    }
                    if let Some(chosen) = self.queued.remove(at) {
                        self.queued.push_front(chosen);
                    }
                    events.push(self.pending_event());
                } else if !self.queued.is_empty() {
                    self.queued.clear();
                    events.push(self.pending_event());
                }
                let mut frames: Vec<String> = self
                    .approvals
                    .drain()
                    .map(|(_, (request, _))| json!({ "jsonrpc": "2.0", "id": request, "result": { "outcome": { "outcome": "cancelled" } } }).to_string())
                    .collect();
                frames.push(json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": session } }).to_string());
                return Ok(Output { events, frames });
            }
            // OpenCode: the mode is the agent the session runs on (and in
            // 自动编辑 the daemon answers edit requests itself).
            "set_approval_mode" if self.opencode => {
                let mode = text(&op["mode"]).to_string();
                if mode.is_empty() {
                    return Err("set_approval_mode requires mode".to_string());
                }
                self.mode = Some(mode.clone());
                return Ok(Output {
                    frames: self.switch_agent(),
                    events: vec![json!({ "type": "approval_mode", "mode": mode })],
                });
            }
            "shutdown" => vec![],
            other => return Err(format!("ACP agents do not support {other}")),
        };
        Ok(Output {
            events: Vec::new(),
            frames,
        })
    }

    /// OpenCode: the mode a restart keeps.
    fn approval_mode(&self) -> Option<String> {
        self.mode.clone()
    }

    /// OpenCode switches agents for the next turn: a switch during one waits
    /// for it (a switch to 全自动 answers its open requests at once).
    fn mode_applies_live(&self) -> bool {
        !self.opencode
    }

    fn busy(&self) -> bool {
        self.prompt_in_flight
    }

    fn restart_for(&self, _op: &Value) -> Option<Options> {
        None
    }

    /// The agent's own session id (the daemon names the session itself).
    fn conversation(&self) -> Option<String> {
        self.session.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(acp: &mut Acp, value: Value) -> Output {
        acp.translate(Line::Frame(value))
    }

    fn sent(frames: &[String]) -> Vec<Value> {
        frames
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect()
    }

    #[test]
    fn prompts_queue_behind_the_handshake_and_the_running_turn() {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        assert_eq!(sent(&a.start())[0]["method"], "initialize");
        assert!(a
            .encode(&json!({ "op": "user_message", "content": "one" }))
            .unwrap()
            .frames
            .is_empty());
        let new = sent(&frame(&mut a, json!({ "id": 1, "result": {} })).frames);
        assert_eq!(new[0]["method"], "session/new");
        let opened = frame(&mut a, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        assert_eq!(
            sent(&opened.frames)[0]["params"]["prompt"][0]["text"],
            "one"
        );
        assert!(a.busy());
        assert!(a
            .encode(&json!({ "op": "user_message", "content": "two" }))
            .unwrap()
            .frames
            .is_empty());

        let chunk = frame(
            &mut a,
            json!({ "method": "session/update", "params": { "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } } } }),
        );
        assert_eq!(chunk.events[0]["type"], "connecting");
        assert_eq!(chunk.events[1]["delta"], "hi");
        // The turn settles and the queued message starts.
        let settled = frame(
            &mut a,
            json!({ "id": 3, "result": { "stopReason": "end_turn" } }),
        );
        assert_eq!(
            sent(&settled.frames)[0]["params"]["prompt"][0]["text"],
            "two"
        );
        frame(
            &mut a,
            json!({ "id": 4, "result": { "stopReason": "end_turn" } }),
        );
        assert!(!a.busy());
    }

    /// An open session with "one" running and `texts` waiting.
    fn running_with(texts: &[&str]) -> Acp {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        a.start();
        frame(&mut a, json!({ "id": 1, "result": {} }));
        frame(&mut a, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        a.encode(&json!({ "op": "user_message", "content": "one" }))
            .unwrap();
        for text in texts {
            a.encode(&json!({ "op": "user_message", "content": text }))
                .unwrap();
        }
        a
    }

    fn pending(messages: &[&str]) -> Value {
        json!({ "type": "pending_messages", "messages": messages })
    }

    #[test]
    fn waiting_messages_are_listed_taken_back_moved_and_dropped() {
        let mut a = running_with(&["two"]);
        let queued = a
            .encode(&json!({ "op": "user_message", "content": "three", "images": ["/tmp/a.png"] }))
            .unwrap();
        assert_eq!(queued.events, [pending(&["two", "three"])]);
        let moved = a
            .encode(&json!({ "op": "queue_move", "index": -1, "to": 0 }))
            .unwrap();
        assert_eq!(moved.events, [pending(&["three", "two"])]);
        let back = a
            .encode(&json!({ "op": "unqueue", "index": 0, "text": "three" }))
            .unwrap();
        assert_eq!(
            back.events,
            [
                pending(&["two"]),
                json!({ "type": "unqueued", "content": "three", "text": "three", "images": ["/tmp/a.png"] }),
            ]
        );
        let dropped = a
            .encode(&json!({ "op": "unqueue", "index": 0, "discard": true }))
            .unwrap();
        assert_eq!(dropped.events, [pending(&[])]);
        // Nothing waits: the turn settles and goes ready.
        let settled = frame(
            &mut a,
            json!({ "id": 3, "result": { "stopReason": "end_turn" } }),
        );
        assert!(settled.frames.is_empty());
    }

    #[test]
    fn steering_cancels_the_turn_and_the_chosen_message_runs_next() {
        for op in [
            json!({ "op": "steer", "index": -1 }),
            json!({ "op": "interrupt", "index": -1 }),
        ] {
            let mut a = running_with(&["two", "three"]);
            let out = a.encode(&op).unwrap();
            assert_eq!(
                sent(&out.frames).last().unwrap()["method"],
                "session/cancel"
            );
            assert_eq!(out.events, [pending(&["three", "two"])]);
            let done = frame(
                &mut a,
                json!({ "id": 3, "result": { "stopReason": "cancelled" } }),
            );
            let next = sent(&done.frames);
            assert_eq!(next[0]["method"], "session/prompt");
            assert_eq!(next[0]["params"]["prompt"][0]["text"], "three");
            assert!(
                done.events.contains(&pending(&["two"])),
                "{:?}",
                done.events
            );
        }
        // A plain interrupt drops what waits.
        let mut a = running_with(&["two"]);
        let out = a.encode(&json!({ "op": "interrupt" })).unwrap();
        assert_eq!(out.events, [pending(&[])]);
    }

    #[cfg(not(windows))]
    #[test]
    fn attached_files_go_as_resource_links_and_replay_as_files() {
        let mut a = running_with(&[]);
        a.encode(&json!({ "op": "user_message", "content": "oops", "files": ["/tmp/x.zip"] }))
            .unwrap();
        let back = a.encode(&json!({ "op": "unqueue", "index": 0 })).unwrap();
        assert_eq!(
            back.events[1],
            json!({ "type": "unqueued", "content": "oops", "text": "oops", "images": [], "files": ["/tmp/x.zip"] })
        );
        a.encode(
            &json!({ "op": "user_message", "content": "read", "files": ["/tmp/my report.pdf"] }),
        )
        .unwrap();
        let next = frame(
            &mut a,
            json!({ "id": 3, "result": { "stopReason": "end_turn" } }),
        );
        assert_eq!(
            sent(&next.frames)[0]["params"]["prompt"],
            json!([
                { "type": "text", "text": "read" },
                { "type": "resource_link", "uri": "file:///tmp/my%20report.pdf", "name": "my report.pdf" },
            ])
        );

        let mut a = Acp::new(Path::new("/p"), &reopen("ses_1"));
        a.start();
        frame(
            &mut a,
            json!({ "id": 1, "result": { "agentCapabilities": { "loadSession": true } } }),
        );
        for u in [
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "read" } }),
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "resource_link", "uri": "file:///tmp/my%20report.pdf", "name": "my report.pdf" } }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "ok" } }),
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "resource_link", "uri": "file:///tmp/x.zip", "name": "x.zip" } }),
        ] {
            frame(&mut a, update(u));
        }
        let loaded = frame(&mut a, json!({ "id": 2, "result": {} }));
        assert_eq!(
            loaded.events[0]["items"],
            json!([
                { "role": "user", "content": "read", "files": ["/tmp/my report.pdf"] },
                { "role": "assistant", "content": "ok" },
                { "role": "user", "content": "", "files": ["/tmp/x.zip"] },
            ])
        );
    }

    #[test]
    fn permissions_pick_the_offered_option_and_tools_fill_their_cards() {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        let ask = frame(
            &mut a,
            json!({ "id": 9, "method": "session/request_permission", "params": {
            "toolCall": { "title": "Edit a.rs", "kind": "edit", "content": [{ "type": "diff", "path": "a.rs", "oldText": "x", "newText": "y" }] },
            "options": [{ "optionId": "no", "kind": "reject_once" }, { "optionId": "yes", "kind": "allow_once" }, { "optionId": "all", "kind": "allow_always" }],
        } }),
        );
        let request = &ask.events[0];
        assert!(text(&request["summary"]).contains("-x\n+y"));
        let answer = sent(&a.encode(&json!({ "op": "approve", "call_id": request["call_id"], "decision": "allow", "always": true })).unwrap().frames);
        assert_eq!(answer[0]["result"]["outcome"]["optionId"], "all");

        let start = frame(
            &mut a,
            json!({ "method": "session/update", "params": { "update": { "sessionUpdate": "tool_call", "toolCallId": "t", "title": "ls", "kind": "execute" } } }),
        );
        assert_eq!(start.events[0]["type"], "tool_start");
        let done = frame(
            &mut a,
            json!({ "method": "session/update", "params": { "update": { "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": "a.rs" } }] } } }),
        );
        assert_eq!(done.events[0]["type"], "tool_output");
        assert!(text(&done.events[0]["output"]).contains("a.rs"));
    }

    fn reopen(id: &str) -> Options {
        Options {
            resume: Some(id.to_string()),
            ..Options::default()
        }
    }

    fn update(update: Value) -> Value {
        json!({ "method": "session/update", "params": { "sessionId": "ses_1", "update": update } })
    }

    #[test]
    fn a_reopened_session_loads_the_agents_conversation_as_its_transcript() {
        let mut a = Acp::new(Path::new("/p"), &reopen("ses_1"));
        start_and_queue(&mut a, "next");
        let load = sent(
            &frame(
                &mut a,
                json!({ "id": 1, "result": { "agentCapabilities": { "loadSession": true } } }),
            )
            .frames,
        );
        assert_eq!(load[0]["method"], "session/load");
        assert_eq!(load[0]["params"]["sessionId"], "ses_1");
        assert_eq!(load[0]["params"]["cwd"], "/p");
        assert_eq!(load[0]["params"]["mcpServers"], json!([]));

        // The replay streams nothing live: it becomes the transcript.
        for (kind, id, text) in [
            ("user_message_chunk", "m1", "fix "),
            ("user_message_chunk", "m1", "it"),
            ("agent_thought_chunk", "r1", "hm"),
            ("agent_message_chunk", "m2", "done"),
            ("user_message_chunk", "m3", "thanks"),
        ] {
            let replayed = frame(
                &mut a,
                update(
                    json!({ "sessionUpdate": kind, "messageId": id, "content": { "type": "text", "text": text } }),
                ),
            );
            assert!(replayed.events.is_empty(), "{:?}", replayed.events);
        }
        frame(
            &mut a,
            update(
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": "a.rs" } }] }),
            ),
        );
        let loaded = frame(
            &mut a,
            json!({ "id": 2, "result": { "configOptions": [{ "id": "model", "category": "model", "type": "select", "currentValue": "zen/big", "options": [{ "value": "zen/big", "name": "Zen/Big" }] }] } }),
        );
        let transcript = &loaded.events[0];
        assert_eq!(transcript["type"], "transcript");
        let rows = transcript["items"].as_array().unwrap();
        assert_eq!(rows[0], json!({ "role": "user", "content": "fix it" }));
        assert_eq!(rows[1], json!({ "role": "reasoning", "content": "hm" }));
        assert_eq!(rows[2], json!({ "role": "assistant", "content": "done" }));
        assert_eq!(rows[3], json!({ "role": "user", "content": "thanks" }));
        assert_eq!(rows[4]["role"], "tool");
        assert!(text(&rows[4]["output"]).contains("a.rs"));
        assert_eq!(loaded.events[1]["type"], "startup");
        assert_eq!(loaded.events[1]["model"], "Zen/Big");
        assert_eq!(a.conversation().as_deref(), Some("ses_1"));
        // The message sent while it opened goes to the same conversation.
        let prompt = sent(&loaded.frames);
        assert_eq!(prompt[0]["method"], "session/prompt");
        assert_eq!(prompt[0]["params"]["sessionId"], "ses_1");
        assert_eq!(prompt[0]["params"]["prompt"][0]["text"], "next");

        // Live updates stream again.
        let live = frame(
            &mut a,
            update(
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "ok" } }),
            ),
        );
        assert!(live
            .events
            .iter()
            .any(|e| e["type"] == "assistant_delta" && e["delta"] == "ok"));
    }

    /// Starts the handshake and queues `message` behind it.
    fn start_and_queue(a: &mut Acp, message: &str) {
        assert_eq!(sent(&a.start())[0]["method"], "initialize");
        let queued = a
            .encode(&json!({ "op": "user_message", "content": message }))
            .unwrap();
        assert!(queued.frames.is_empty());
    }

    #[test]
    fn a_reopened_session_resumes_without_a_replay_or_starts_anew() {
        // `session/resume`: the conversation, without its history.
        let mut a = Acp::new(Path::new("/p"), &reopen("ses_1"));
        start_and_queue(&mut a, "next");
        let resume = sent(
            &frame(
                &mut a,
                json!({ "id": 1, "result": { "agentCapabilities": { "sessionCapabilities": { "resume": {} } } } }),
            )
            .frames,
        );
        assert_eq!(resume[0]["method"], "session/resume");
        let resumed = frame(&mut a, json!({ "id": 2, "result": {} }));
        assert!(resumed.events.iter().all(|e| e["type"] != "transcript"));
        assert_eq!(sent(&resumed.frames)[0]["params"]["sessionId"], "ses_1");

        // Neither: a new conversation, and clients are told.
        let mut a = Acp::new(Path::new("/p"), &reopen("ses_1"));
        start_and_queue(&mut a, "next");
        let fresh = frame(
            &mut a,
            json!({ "id": 1, "result": { "agentCapabilities": {} } }),
        );
        assert_eq!(sent(&fresh.frames)[0]["method"], "session/new");
        assert_eq!(fresh.events[0]["type"], "info");
        frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "ses_2" } }),
        );
        assert_eq!(a.conversation().as_deref(), Some("ses_2"));
    }

    #[test]
    fn a_conversation_the_agent_no_longer_has_starts_anew() {
        let mut a = Acp::new(Path::new("/p"), &reopen("ses_gone"));
        start_and_queue(&mut a, "next");
        frame(
            &mut a,
            json!({ "id": 1, "result": { "agentCapabilities": { "loadSession": true } } }),
        );
        let failed = frame(
            &mut a,
            json!({ "id": 2, "error": { "code": -32603, "message": "session not found" } }),
        );
        assert_eq!(failed.events[0]["type"], "info");
        assert!(text(&failed.events[0]["message"]).contains("session not found"));
        assert_eq!(sent(&failed.frames)[0]["method"], "session/new");
        let opened = frame(
            &mut a,
            json!({ "id": 3, "result": { "sessionId": "ses_new" } }),
        );
        assert!(opened.events.iter().all(|e| e["type"] != "transcript"));
        assert_eq!(sent(&opened.frames)[0]["params"]["sessionId"], "ses_new");
        assert_eq!(a.conversation().as_deref(), Some("ses_new"));
    }

    #[test]
    fn the_model_label_follows_the_agents_config_options() {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        a.start();
        frame(&mut a, json!({ "id": 1, "result": {} }));
        let opened = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s", "configOptions": [
                { "id": "mode", "category": "mode", "type": "select", "currentValue": "build", "options": [] },
                { "id": "model", "category": "model", "type": "select", "currentValue": "b/two", "options": [
                    { "group": "a", "name": "A", "options": [{ "value": "a/one", "name": "A/One" }] },
                    { "group": "b", "name": "B", "options": [{ "value": "b/two", "name": "B/Two" }] },
                ] },
            ] } }),
        );
        assert_eq!(opened.events[0]["model"], "B/Two");
        let changed = frame(
            &mut a,
            update(
                json!({ "sessionUpdate": "config_option_update", "configOptions": [{ "id": "model", "category": "model", "type": "select", "currentValue": "x/y", "options": [] }] }),
            ),
        );
        assert_eq!(changed.events[0]["type"], "model_status");
        assert_eq!(changed.events[0]["model"], "x/y");
    }

    #[test]
    fn file_requests_the_client_never_offered_are_declined_quietly() {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        let write = frame(
            &mut a,
            json!({ "id": 7, "method": "fs/write_text_file", "params": { "path": "/p/a", "content": "x" } }),
        );
        assert!(write.events.is_empty());
        assert_eq!(sent(&write.frames)[0]["error"]["code"], -32601);
        let other = frame(
            &mut a,
            json!({ "id": 8, "method": "x/unknown", "params": {} }),
        );
        assert_eq!(other.events[0]["type"], "info");
    }

    fn opencode(mode: &str) -> Options {
        Options {
            command: Some("/usr/local/bin/opencode".to_string()),
            args: vec!["acp".to_string()],
            approval_mode: Some(mode.to_string()),
            ..Options::default()
        }
    }

    /// Starts an OpenCode session to the point where it runs on its first
    /// agent; returns the frames that switched it.
    fn open_opencode(a: &mut Acp) -> Vec<Value> {
        start_and_queue(a, "hi");
        frame(
            a,
            json!({ "id": 1, "result": { "agentInfo": { "name": "OpenCode", "version": "1.19.0" }, "agentCapabilities": { "loadSession": true } } }),
        );
        let opened = frame(a, json!({ "id": 2, "result": { "sessionId": "ses_1" } }));
        assert!(
            opened.events.iter().any(|e| e["type"] == "approval_mode"),
            "{:?}",
            opened.events
        );
        sent(&opened.frames)
    }

    #[test]
    fn opencode_modes_are_its_agents_switched_before_the_prompt() {
        let mut a = Acp::new(Path::new("/p"), &opencode("auto-edit"));
        let switched = open_opencode(&mut a);
        assert_eq!(switched.len(), 1, "the prompt waits for the switch");
        assert_eq!(switched[0]["method"], "session/set_config_option");
        assert_eq!(switched[0]["params"]["configId"], "mode");
        assert_eq!(switched[0]["params"]["value"], opencode::ASK_AGENT);
        let id = switched[0]["id"].clone();
        let prompted = sent(
            &frame(
                &mut a,
                json!({ "id": id, "result": { "configOptions": [] } }),
            )
            .frames,
        );
        assert_eq!(prompted[0]["method"], "session/prompt");

        // 自动编辑: edits go ahead, commands ask.
        let edit = frame(
            &mut a,
            json!({ "id": 70, "method": "session/request_permission", "params": {
                "toolCall": { "title": "a.rs", "kind": "edit" },
                "options": [{ "optionId": "once", "kind": "allow_once" }, { "optionId": "reject", "kind": "reject_once" }],
            } }),
        );
        assert!(edit.events.iter().all(|e| e["type"] != "approval_request"));
        let answer = sent(&edit.frames);
        assert_eq!(answer[0]["id"], 70);
        assert_eq!(answer[0]["result"]["outcome"]["optionId"], "once");
        let command = frame(
            &mut a,
            json!({ "id": 71, "method": "session/request_permission", "params": {
                "toolCall": { "title": "ls", "kind": "execute" },
                "options": [{ "optionId": "once", "kind": "allow_once" }],
            } }),
        );
        assert!(command
            .events
            .iter()
            .any(|e| e["type"] == "approval_request"));

        // Live switches: 全自动 is OpenCode's build agent, 计划 its plan agent;
        // 谨慎 asks for edits too.
        for (mode, agent) in [
            ("full-auto", "build"),
            ("plan", opencode::PLAN_AGENT),
            ("read-only", opencode::ASK_AGENT),
        ] {
            let out = a
                .encode(&json!({ "op": "set_approval_mode", "mode": mode }))
                .unwrap();
            assert_eq!(out.events[0]["mode"], mode);
            assert_eq!(sent(&out.frames)[0]["params"]["value"], agent, "{mode}");
            let id = sent(&out.frames)[0]["id"].clone();
            frame(&mut a, json!({ "id": id, "result": {} }));
        }
        // 谨慎 → 自动编辑 keeps the agent: nothing to send.
        let same = a
            .encode(&json!({ "op": "set_approval_mode", "mode": "auto-edit" }))
            .unwrap();
        assert!(same.frames.is_empty());
        assert_eq!(a.approval_mode().as_deref(), Some("auto-edit"));
        assert!(!a.mode_applies_live());
    }

    /// Answers the request `frames` sent with `answer` (a `result` or an
    /// `error`).
    fn answer(a: &mut Acp, frames: &[Value], mut reply: Value) -> Output {
        reply["id"] = frames[0]["id"].clone();
        frame(a, reply)
    }

    fn errors(output: &Output) -> String {
        output
            .events
            .iter()
            .filter(|e| e["type"] == "error")
            .map(|e| text(&e["message"]).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_failed_mode_switch_sends_nothing_until_a_switch_succeeds() {
        let mut a = Acp::new(Path::new("/p"), &opencode("read-only"));
        let switched = open_opencode(&mut a);
        // OpenCode does not know the agent (a config that dropped it).
        let failed = answer(
            &mut a,
            &switched,
            json!({ "error": { "code": -32602, "message": "Invalid params: mode not found: aogiri" } }),
        );
        assert!(failed.frames.is_empty(), "the waiting message is not sent");
        let message = errors(&failed);
        assert!(message.contains("did not switch to 谨慎"), "{message}");
        assert!(
            message.contains("waiting message was not sent"),
            "{message}"
        );
        assert!(failed
            .events
            .iter()
            .any(|e| e["type"] == "status" && e["message"] == "ready"));
        // New messages are refused, not queued.
        let refused = a.encode(&json!({ "op": "user_message", "content": "again" }));
        assert!(refused.unwrap_err().contains("did not switch"));
        // Picking the mode again switches again; once it did, messages go.
        let retry = a
            .encode(&json!({ "op": "set_approval_mode", "mode": "read-only" }))
            .unwrap();
        let retry = sent(&retry.frames);
        assert_eq!(retry[0]["params"]["value"], opencode::ASK_AGENT);
        let ok = answer(&mut a, &retry, json!({ "result": {} }));
        assert!(ok.frames.is_empty() && errors(&ok).is_empty());
        let prompt = a
            .encode(&json!({ "op": "user_message", "content": "now" }))
            .unwrap();
        assert_eq!(sent(&prompt.frames)[0]["method"], "session/prompt");
    }

    #[test]
    fn a_switch_opencode_reports_as_not_done_fails_closed() {
        let mut a = Acp::new(Path::new("/p"), &opencode("plan"));
        let switched = open_opencode(&mut a);
        assert_eq!(switched[0]["params"]["value"], opencode::PLAN_AGENT);
        let kept = answer(
            &mut a,
            &switched,
            json!({ "result": { "configOptions": [{ "id": "mode", "currentValue": "build" }] } }),
        );
        assert!(kept.frames.is_empty());
        assert!(errors(&kept).contains("计划"), "{}", errors(&kept));
        assert!(a
            .encode(&json!({ "op": "user_message", "content": "x" }))
            .is_err());
    }

    #[test]
    fn set_mode_switches_an_opencode_without_session_config_options() {
        let mut a = Acp::new(Path::new("/p"), &opencode("auto-edit"));
        let switched = open_opencode(&mut a);
        let fallback = answer(
            &mut a,
            &switched,
            json!({ "error": { "code": -32601, "message": "Method not found" } }),
        );
        assert!(errors(&fallback).is_empty());
        let fallback = sent(&fallback.frames);
        assert_eq!(fallback[0]["method"], "session/set_mode");
        assert_eq!(fallback[0]["params"]["sessionId"], "ses_1");
        assert_eq!(fallback[0]["params"]["modeId"], opencode::ASK_AGENT);
        let prompted = sent(&answer(&mut a, &fallback, json!({ "result": {} })).frames);
        assert_eq!(prompted[0]["method"], "session/prompt");
        // No plan reminder outside 计划.
        assert_eq!(prompted[0]["params"]["prompt"].as_array().unwrap().len(), 1);
        frame(
            &mut a,
            json!({ "id": prompted[0]["id"], "result": { "stopReason": "end_turn" } }),
        );
        // Later switches use set_mode at once; 计划 adds its reminder.
        let plan = a
            .encode(&json!({ "op": "set_approval_mode", "mode": "plan" }))
            .unwrap();
        let plan = sent(&plan.frames);
        assert_eq!(plan[0]["method"], "session/set_mode");
        assert_eq!(plan[0]["params"]["modeId"], opencode::PLAN_AGENT);
        answer(&mut a, &plan, json!({ "result": {} }));
        let prompt = a
            .encode(&json!({ "op": "user_message", "content": "plan it" }))
            .unwrap();
        let blocks = sent(&prompt.frames)[0]["params"]["prompt"].clone();
        assert_eq!(blocks[0]["text"], "plan it");
        assert_eq!(blocks[1]["annotations"]["audience"], json!(["assistant"]));
        assert!(text(&blocks[1]["text"]).contains("Plan mode"));
        // Neither method: nothing is sent.
        let mut old = Acp::new(Path::new("/p"), &opencode("read-only"));
        let switched = open_opencode(&mut old);
        let fallback = answer(
            &mut old,
            &switched,
            json!({ "error": { "code": -32601, "message": "Method not found" } }),
        );
        let failed = answer(
            &mut old,
            &sent(&fallback.frames),
            json!({ "error": { "code": -32601, "message": "Method not found" } }),
        );
        assert!(failed.frames.is_empty());
        assert!(errors(&failed).contains("did not switch"));
    }

    #[test]
    fn full_auto_runs_when_its_switch_fails() {
        let mut a = Acp::new(Path::new("/p"), &opencode("full-auto"));
        let switched = open_opencode(&mut a);
        assert_eq!(switched[0]["params"]["value"], "build");
        let output = answer(
            &mut a,
            &switched,
            json!({ "error": { "code": -32603, "message": "boom" } }),
        );
        assert!(errors(&output).contains("全自动"));
        assert_eq!(sent(&output.frames)[0]["method"], "session/prompt");
    }

    #[test]
    fn a_mode_picked_during_a_switch_is_switched_to_after_it() {
        let mut a = Acp::new(Path::new("/p"), &opencode("full-auto"));
        let first = open_opencode(&mut a);
        // 谨慎 while the switch to 全自动 is on its way: one switch at a time.
        let picked = a
            .encode(&json!({ "op": "set_approval_mode", "mode": "read-only" }))
            .unwrap();
        assert!(picked.frames.is_empty());
        let next = sent(&answer(&mut a, &first, json!({ "result": {} })).frames);
        assert_eq!(next.len(), 1, "no prompt before the second switch");
        assert_eq!(next[0]["params"]["value"], opencode::ASK_AGENT);
        let prompted = sent(&answer(&mut a, &next, json!({ "result": {} })).frames);
        assert_eq!(prompted[0]["method"], "session/prompt");
    }

    #[test]
    fn the_plan_reminder_stays_out_of_a_replayed_transcript() {
        let mut a = Acp::new(Path::new("/p"), &reopen("ses_1"));
        start_and_queue(&mut a, "next");
        frame(
            &mut a,
            json!({ "id": 1, "result": { "agentCapabilities": { "loadSession": true } } }),
        );
        let mut reminder = opencode::plan_reminder();
        reminder["text"] = json!("<system-reminder>plan</system-reminder>");
        for content in [json!({ "type": "text", "text": "look" }), reminder] {
            frame(
                &mut a,
                update(
                    json!({ "sessionUpdate": "user_message_chunk", "messageId": "m1", "content": content }),
                ),
            );
        }
        let loaded = frame(&mut a, json!({ "id": 2, "result": {} }));
        assert_eq!(
            loaded.events[0]["items"],
            json!([{ "role": "user", "content": "look" }])
        );
    }

    #[test]
    fn other_agents_keep_refusing_approval_modes() {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        assert!(a
            .encode(&json!({ "op": "set_approval_mode", "mode": "full-auto" }))
            .is_err());
        let mut gemini = opencode("read-only");
        gemini.command = Some("gemini".to_string());
        let a = Acp::new(Path::new("/p"), &gemini);
        assert_eq!(a.approval_mode(), None);
    }

    #[test]
    fn opencode_2_is_refused_before_a_session_opens() {
        let mut a = Acp::new(Path::new("/p"), &opencode("read-only"));
        start_and_queue(&mut a, "hi");
        let refused = frame(
            &mut a,
            json!({ "id": 1, "result": { "agentInfo": { "name": "OpenCode", "version": "2.0.29" }, "agentCapabilities": {} } }),
        );
        assert!(refused.frames.is_empty());
        // The waiting message goes: the queue empties, then the refusal.
        assert_eq!(
            refused.events[0],
            json!({ "type": "pending_messages", "messages": [] })
        );
        assert_eq!(refused.events[1]["type"], "error");
        assert!(text(&refused.events[1]["message"]).contains("OpenCode 1.x"));
        let next = a.encode(&json!({ "op": "user_message", "content": "again" }));
        assert!(next.unwrap_err().contains("OpenCode 1.x"));
    }

    #[test]
    fn a_fork_copies_the_conversation_into_a_session_of_its_own() {
        let options = Options {
            fork: Some("ses_original".to_string()),
            ..Options::default()
        };
        let mut a = Acp::new(Path::new("/p"), &options);
        start_and_queue(&mut a, "next");
        let fork = sent(
            &frame(
                &mut a,
                json!({ "id": 1, "result": { "agentCapabilities": { "loadSession": true, "sessionCapabilities": { "fork": {} } } } }),
            )
            .frames,
        );
        assert_eq!(fork[0]["method"], "session/fork");
        assert_eq!(fork[0]["params"]["sessionId"], "ses_original");
        frame(
            &mut a,
            update(
                json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "old" } }),
            ),
        );
        let copied = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "ses_copy" } }),
        );
        assert_eq!(copied.events[0]["type"], "transcript");
        assert_eq!(copied.events[0]["items"][0]["content"], "old");
        assert_eq!(a.conversation().as_deref(), Some("ses_copy"));
        assert_eq!(sent(&copied.frames)[0]["params"]["sessionId"], "ses_copy");

        // An agent that cannot copy: a new conversation, and an error.
        let mut a = Acp::new(Path::new("/p"), &options);
        start_and_queue(&mut a, "next");
        let fresh = frame(
            &mut a,
            json!({ "id": 1, "result": { "agentCapabilities": {} } }),
        );
        assert_eq!(fresh.events[0]["type"], "error");
        assert_eq!(sent(&fresh.frames)[0]["method"], "session/new");
    }

    #[test]
    fn a_restart_reopens_without_a_replay() {
        let options = Options {
            quiet: true,
            ..reopen("ses_1")
        };
        let mut a = Acp::new(Path::new("/p"), &options);
        start_and_queue(&mut a, "next");
        let resume = sent(
            &frame(
                &mut a,
                json!({ "id": 1, "result": { "agentCapabilities": { "loadSession": true, "sessionCapabilities": { "resume": {} } } } }),
            )
            .frames,
        );
        assert_eq!(resume[0]["method"], "session/resume");
    }
}

//! Agent Client Protocol agents (JSON-RPC 2.0 over stdio lines, protocol v1,
//! https://agentclientprotocol.com), stable schema only: the built-in
//! presets (`acp_presets`, by `options.agent`) and agents the desktop's
//! registry names by command.
//!
//! - Handshake: `initialize`, then `session/load` (its replay becomes the
//!   `transcript`) or `session/resume` to reopen the agent's conversation,
//!   `session/fork` to copy one, else `session/new`. An agent that needs a
//!   sign-in (`-32000` on opening) is asked to sign in its own way once
//!   (`authenticate`), then the session opens again.
//! - The user's enabled MCP servers go to the agent with each session
//!   (`mcpServers`): stdio ones always, http ones when it takes them.
//! - A turn is `session/prompt`, answered with its `stopReason` once it
//!   settles; `session/update` notifications stream the reply, thoughts, tool
//!   calls, plans, usage, notices and the agent's commands and config options
//!   meanwhile. There is no turn-started notification.
//! - Permission prompts are `session/request_permission` requests answered
//!   with one of the options the agent offered, and form elicitations
//!   (`elicitation/create`, oh-my-pi's approvals) answered with the form.
//! - Model, effort and the agent's other settings are its config options
//!   (legacy `modes`, or its `models`, when it has none); clients see them as
//!   `model_status`, `model_view` and `session_options`
//!   (`set_session_option` changes one), and what the session can do as
//!   `engine_caps`.
//! - One prompt at a time: messages sent during a turn wait here
//!   (`pending_messages`) and run as the next turns; `unqueue` takes one back
//!   (or drops it), `queue_move` reorders them, and `steer` (or `interrupt`
//!   with `index`) cancels the turn so the chosen one runs as soon as it
//!   settles.
//! - OpenCode's approval modes are agents of its own (`opencode`), switched
//!   live; oh-my-pi's are a launch flag (a mode change restarts it, which
//!   reopens the conversation); Pi asks nothing and runs in 全自动 only.

use super::{
    acp_presets::{self, Mode, Preset},
    claude, find_program, op_files, opencode, set_files, Adapter, Line, Options, Output,
};
use base64::{engine::general_purpose::STANDARD, Engine};
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
/// Larger images are not sent.
const IMAGE_CAP: u64 = 20 * 1024 * 1024;

/// A message waiting for the session or the running turn: what the user
/// sent, made into prompt blocks when it goes.
struct Waiting {
    text: String,
    images: Vec<String>,
    /// Other files, sent as `resource_link`s.
    files: Vec<String>,
}

/// The built-in agent `options` names, if any.
pub fn preset(options: &Options) -> Result<Option<&'static Preset>, String> {
    let Some(id) = options.agent.as_deref() else {
        return Ok(None);
    };
    if options.command.is_some() || !options.args.is_empty() {
        return Err("options.agent and options.command are exclusive".to_string());
    }
    acp_presets::find(id)
        .map(Some)
        .ok_or_else(|| format!("unknown ACP agent {id}"))
}

pub fn command(options: &Options) -> Result<Command, String> {
    if let Some(preset) = preset(options)? {
        return acp_presets::command(preset, options);
    }
    let mut command = Command::new(program(options));
    command.args(&options.args);
    Ok(command)
}

/// The program a desktop-registered agent's command names, found on PATH
/// when it is a bare name.
pub fn program(options: &Options) -> PathBuf {
    let program = options.command.as_deref().unwrap_or_default();
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        path.to_path_buf()
    } else {
        find_program(program, &[])
    }
}

/// The agent's own TUI resuming `conversation`: a built-in agent's, or
/// OpenCode's when the desktop registered it (on the agent of the session's
/// approval mode). Other agents have none here.
pub fn tui(options: &Options, conversation: &str) -> Result<Command, String> {
    if let Some(preset) = preset(options)? {
        return acp_presets::tui(preset, options, conversation);
    }
    if !opencode::is_opencode(options) {
        return Err("a registered ACP agent has no TUI here".to_string());
    }
    let mut command = Command::new(program(options));
    command.args(acp_presets::opencode_tui_args(
        options.approval_mode.as_deref(),
        conversation,
    ));
    Ok(command)
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

/// An image file as a prompt block; None when it is unreadable or too big.
fn image_block(path: &str) -> Option<Value> {
    let size = std::fs::metadata(path).ok()?.len();
    if size > IMAGE_CAP {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    Some(json!({ "type": "image", "mimeType": image_mime(path), "data": STANDARD.encode(data) }))
}

/// The id of the model select made of an agent's `models` (it sets them with
/// `session/set_model`), for one with no model config option.
const MODEL_STATE: &str = "_models";

/// A select option's choices, groups flattened.
fn choices(option: &Value) -> Vec<&Value> {
    option["options"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|choice| match choice["options"].as_array() {
            Some(group) => group.iter().collect(),
            None => vec![choice],
        })
        .collect()
}

fn choice_values(option: Option<&Value>) -> Vec<Value> {
    option
        .map(|option| {
            choices(option)
                .into_iter()
                .map(|c| c["value"].clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Effort values that leave the level to the agent (OpenCode's "default",
/// oh-my-pi's "auto"): the unset state, not a level of their own.
const UNSET_EFFORTS: &[&str] = &["default", "auto"];

/// An effort option's levels, weakest first as agents list them, and its
/// unset value if it has one.
fn effort_levels(option: Option<&Value>) -> (Vec<Value>, &'static str) {
    let mut unset = "";
    let mut levels = Vec::new();
    for value in choice_values(option) {
        match UNSET_EFFORTS.iter().find(|u| value == **u) {
            Some(u) => unset = u,
            None => levels.push(value),
        }
    }
    (levels, unset)
}

/// A model's name without the provider prefix of its value, as the other
/// engines name models ("deepseek/DeepSeek V4 Pro" for
/// "deepseek/deepseek-v4-pro" is "DeepSeek V4 Pro").
fn model_name<'a>(value: &str, name: &'a str) -> &'a str {
    value
        .split_once('/')
        .and_then(|(provider, _)| name.strip_prefix(provider)?.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(name)
}

#[derive(Default)]
struct Tool {
    name: String,
    kind: String,
    path: String,
    /// A terminal call's command (`rawInput.command`, as OpenCode and
    /// oh-my-pi send it; the title may only name the tool).
    command: String,
    text: String,
    diff: String,
    /// The call ended `failed` (a replayed one keeps it).
    failed: bool,
}

impl Tool {
    /// The name clients render it by: a terminal call is a `bash` card (its
    /// command and output), whatever the agent titles it.
    fn display_name(&self) -> &str {
        if self.kind == "execute" {
            "bash"
        } else {
            &self.name
        }
    }

    /// The input an update reports (`rawInput`, sent whole each time).
    fn take_input(&mut self, input: &Value) {
        if let Some(command) = [&input["command"], &input["cmd"]]
            .into_iter()
            .map(text)
            .find(|c| !c.is_empty())
        {
            self.command = command.to_string();
        }
    }

    fn output(&self) -> String {
        let mut out = json!({ "kind": self.kind });
        let text_key = if self.kind == "execute" {
            "stdout"
        } else {
            "content"
        };
        for (key, value) in [
            ("path", &self.path),
            ("command", &self.command),
            (text_key, &self.text),
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

    /// One update of a call (its first or a later one).
    fn update(&mut self, update: &Value) {
        let set = |field: &mut String, value: &str| {
            if !value.is_empty() {
                *field = value.to_string();
            }
        };
        set(&mut self.kind, text(&update["kind"]));
        if self.name.is_empty() || self.name == "tool" {
            set(&mut self.name, text(&update["kind"]));
        }
        set(&mut self.name, text(&update["title"]));
        set(&mut self.path, text(&update["locations"][0]["path"]));
        self.take_input(&update["rawInput"]);
        self.absorb(&update["content"]);
        match text(&update["status"]) {
            "failed" => self.failed = true,
            "completed" => self.failed = false,
            _ => {}
        }
    }
}

/// The conversation an agent replays while `session/load` (or a copy's
/// `session/fork`) runs, as transcript rows.
#[derive(Default)]
struct Replay {
    rows: Vec<Value>,
    /// The message the last text row belongs to (`messageId`), when the
    /// agent names them: a new message starts a new row.
    message: Option<String>,
    /// Its last plan.
    plan: Option<Value>,
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

    /// A tool call as it stands, in its place (the row of its first update).
    fn tool(&mut self, id: &str, tool: &Tool) {
        self.message = None;
        let mut item = json!({ "role": "tool", "name": tool.display_name(), "output": tool.output(), "call_id": id });
        if tool.failed {
            item["is_error"] = json!(true);
        }
        match self.rows.iter_mut().find(|row| row["call_id"] == id) {
            Some(known) => *known = item,
            None => self.rows.push(item),
        }
    }

    /// The rows as a transcript (call ids dropped).
    fn items(mut self) -> Vec<Value> {
        for row in &mut self.rows {
            if let Some(row) = row.as_object_mut() {
                row.remove("call_id");
            }
        }
        self.rows
    }
}

/// An open question from the agent, by the reply it needs.
enum Ask {
    /// `session/request_permission`: the options offered.
    Permission(Vec<Value>),
    /// `elicitation/create`: the form as shown.
    Form(Value),
}

pub struct Acp {
    cwd: PathBuf,
    next_id: u64,
    /// Request id → method and params.
    pending: HashMap<u64, (String, Value)>,
    session: Option<String>,
    /// The model to switch to once open.
    model: Option<String>,
    /// The thinking effort to switch to once open (after the model: its
    /// levels can depend on it).
    effort: Option<String>,
    preset: Option<&'static Preset>,
    /// A built-in agent's approval mode as launched (oh-my-pi's is a flag).
    launched: Mode,
    agent: Value,
    /// Every way it signs in (`authMethods`), and whether one was tried.
    auths: Vec<Value>,
    authenticated: bool,
    /// A probe's run: no sign-in starts from it.
    probing: bool,
    /// The user's MCP servers (lynshen_agent_core::acp_mcp_servers), and
    /// whether the agent takes http ones.
    mcp: Vec<Value>,
    mcp_http: bool,
    /// The agent's `models` (SessionModelState), for one with no model
    /// config option: a select of them is kept among `config` (MODEL_STATE).
    models: Value,
    capabilities: Value,
    image_prompts: bool,
    /// The agent's config options; legacy `modes` when it sends none.
    config: Vec<Value>,
    modes: Value,
    commands: Vec<Value>,
    usage_size: u64,
    usage_seen: bool,
    /// The last `engine_caps` sent.
    caps_sent: Value,
    prompt_in_flight: bool,
    busy_announced: bool,
    /// The running turn produced a reply, thought or tool call.
    replied: bool,
    /// Messages waiting for the session or the running turn.
    queued: VecDeque<Waiting>,
    session_failed: bool,
    /// Synthetic call id → (request id, what was asked).
    approvals: HashMap<String, (Value, Ask)>,
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
    oc_agent: Option<&'static str>,
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
        let preset = options.agent.as_deref().and_then(acp_presets::find);
        let opencode = opencode::is_opencode(options) || preset.is_some_and(|p| p.id == "opencode");
        Self {
            cwd: cwd.to_path_buf(),
            next_id: 0,
            pending: HashMap::new(),
            session: None,
            model: options.model.clone(),
            effort: options.effort.clone().filter(|e| !e.is_empty()),
            preset,
            launched: Mode::of(options.approval_mode.as_deref()),
            agent: Value::Null,
            auths: Vec::new(),
            authenticated: false,
            probing: options.probe,
            mcp: Vec::new(),
            mcp_http: false,
            models: Value::Null,
            capabilities: Value::Null,
            image_prompts: false,
            config: Vec::new(),
            modes: Value::Null,
            commands: Vec::new(),
            usage_size: 0,
            usage_seen: false,
            caps_sent: Value::Null,
            prompt_in_flight: false,
            busy_announced: false,
            replied: false,
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
            oc_agent: None,
            configuring: false,
            switch: None,
            set_mode: false,
            mode_failed: None,
            refused: None,
        }
    }

    /// The MCP servers it is given: every stdio one, http ones if it takes them.
    pub fn with_mcp(mut self, servers: Vec<Value>) -> Self {
        self.mcp = servers;
        self
    }

    fn mcp_servers(&self) -> Vec<Value> {
        self.mcp
            .iter()
            .filter(|server| server["type"] != "http" || self.mcp_http)
            .cloned()
            .collect()
    }

    fn request(&mut self, method: &str, params: Value) -> String {
        self.next_id += 1;
        self.pending
            .insert(self.next_id, (method.to_string(), params.clone()));
        json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params })
            .to_string()
    }

    fn agent_name(&self) -> String {
        [&self.agent["title"], &self.agent["name"]]
            .into_iter()
            .map(text)
            .find(|name| !name.is_empty())
            .or(self.preset.map(|preset| preset.name))
            .unwrap_or("the agent")
            .to_string()
    }

    /// A built-in agent whose modes are a launch flag (oh-my-pi).
    fn launch_modes(&self) -> bool {
        self.preset
            .is_some_and(|preset| preset.approvals && preset.id != "opencode")
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
        if self.oc_agent == Some(agent) {
            return Vec::new();
        }
        self.oc_agent = Some(agent);
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
        let wanted = self.oc_agent;
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
        let mut events = Vec::new();
        if let Some(options) = result["configOptions"].as_array() {
            let current = options
                .iter()
                .find(|option| option["id"] == "mode")
                .and_then(|option| option["currentValue"].as_str());
            if let Some(current) = current.filter(|current| *current != asked) {
                return self.switch_failed(asked, &format!("it still runs on {current}"));
            }
            if !options.is_empty() {
                events = self.config_changed(&result["configOptions"]);
            }
        }
        self.configuring = false;
        self.mode_failed = None;
        let mut output = Output::events(events);
        if let Some(next) = self.next_queued() {
            output.events.extend(next.events);
            output.frames.extend(next.frames);
        }
        output
    }

    /// OpenCode did not switch to `agent`. 全自动 runs anyway: it is OpenCode
    /// as the user configured it, and the session's agent is no looser.
    /// Any other mode fails closed: waiting messages are dropped and new ones
    /// refused until a switch succeeds.
    fn switch_failed(&mut self, agent: &'static str, message: &str) -> Output {
        self.oc_agent = None;
        self.configuring = false;
        let mode = opencode::mode_label(self.mode.as_deref().unwrap_or_default()).to_string();
        if agent == "build" {
            let mut output = Output::events(vec![
                json!({ "type": "error", "message": format!("[opencode] OpenCode did not switch to {mode} ({message}); it runs on its current agent") }),
            ]);
            if let Some(next) = self.next_queued() {
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

    /// A prompt for the agent, now: its blocks (images as data when it takes
    /// them, other files as `resource_link`s) and the `user_message` clients
    /// see.
    fn send(&mut self, message: Waiting) -> Output {
        let Waiting {
            text: content,
            images,
            files,
        } = message;
        let mut blocks = vec![json!({ "type": "text", "text": content })];
        let mut events = Vec::new();
        if !images.is_empty() && !self.image_prompts {
            events.push(json!({ "type": "info", "message": format!("[acp] this agent does not take images; {} not sent", images.len()) }));
        } else if !images.is_empty() {
            let sent: Vec<Value> = images.iter().filter_map(|path| image_block(path)).collect();
            if sent.len() < images.len() {
                events.push(json!({ "type": "info", "message": format!("[acp] {} images unreadable or over 20 MB; not sent", images.len() - sent.len()) }));
            }
            blocks.extend(sent);
        }
        blocks.extend(files.iter().map(|path| {
            json!({ "type": "resource_link", "uri": attachments::file_uri(path), "name": attachments::file_name(path) })
        }));
        // OpenCode reminds its own plan agent by its name; ours by a block
        // only the model reads.
        if self.oc_agent == Some(opencode::PLAN_AGENT) {
            blocks.push(opencode::plan_reminder());
        }
        let mut echo = json!({ "type": "user_message", "content": content });
        if !images.is_empty() {
            echo["images"] = json!(images);
        }
        set_files(&mut echo, &files);
        events.push(echo);
        self.prompt_in_flight = true;
        self.busy_announced = false;
        self.replied = false;
        let session = self.session.clone();
        Output {
            frames: vec![self.request(
                "session/prompt",
                json!({ "sessionId": session, "prompt": blocks }),
            )],
            events,
        }
    }

    /// Whether a message can go to the agent now.
    fn can_send(&self) -> bool {
        self.session.is_some()
            && !self.prompt_in_flight
            && !self.configuring
            && self.mode_failed.is_none()
    }

    /// A prompt now, or once the session is open and the running turn ended.
    fn submit(&mut self, message: Waiting) -> Output {
        if self.can_send() {
            return self.send(message);
        }
        self.queued.push_back(message);
        let mut output = Output::events(vec![self.pending_event()]);
        if self.session.is_none() && self.session_failed {
            self.session_failed = false;
            output.frames.push(self.new_session());
        }
        output
    }

    fn waiting_texts(&self) -> Vec<&str> {
        self.queued.iter().map(|m| m.text.as_str()).collect()
    }

    fn pending_event(&self) -> Value {
        json!({ "type": "pending_messages", "messages": self.waiting_texts() })
    }

    /// The next queued message starts, when the session can take one: its
    /// prompt and the events that say it started (the queue without it).
    fn next_queued(&mut self) -> Option<Output> {
        if !self.can_send() {
            return None;
        }
        let next = self.queued.pop_front()?;
        let mut output = self.send(next);
        output.events.insert(0, self.pending_event());
        output.events.push(json!({ "type": "connecting" }));
        Some(output)
    }

    fn new_session(&mut self) -> String {
        let cwd = self.cwd.clone();
        let mcp = self.mcp_servers();
        self.request("session/new", json!({ "cwd": cwd, "mcpServers": mcp }))
    }

    /// The first request after `initialize` (and after a sign-in): the
    /// conversation to copy or reopen, as the agent can (see the module
    /// docs), else a new one. The events say when one cannot be reopened.
    fn open_session(&mut self) -> Output {
        let cwd = self.cwd.clone();
        let mcp = self.mcp_servers();
        let capabilities = self.capabilities.clone();
        let params = |id: &str| json!({ "sessionId": id, "cwd": cwd, "mcpServers": mcp });
        if let Some(source) = self.fork.clone() {
            if capabilities["sessionCapabilities"]["fork"].is_object() {
                self.replay = Some(Replay::default());
                return Output {
                    frames: vec![self.request("session/fork", params(&source))],
                    events: vec![],
                };
            }
            self.fork = None;
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
        let loads = capabilities["loadSession"] == true;
        if resumes && (self.quiet || !loads) {
            return Output {
                frames: vec![self.request("session/resume", params(&id))],
                events: vec![],
            };
        }
        if loads {
            self.replay = Some(Replay::default());
            return Output {
                frames: vec![self.request("session/load", params(&id))],
                events: vec![],
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

    /// Whether the agent has config options of its own (not only the model
    /// select made of its `models`).
    fn has_config(&self) -> bool {
        self.config.iter().any(|option| option["id"] != MODEL_STATE)
    }

    /// The model select made of the agent's `models`, kept among `config`
    /// while it has no model config option of its own.
    fn with_model_state(&mut self) {
        self.config.retain(|option| option["id"] != MODEL_STATE);
        let models = self.models["availableModels"]
            .as_array()
            .filter(|m| !m.is_empty());
        let (Some(models), None) = (models, self.select("model")) else {
            return;
        };
        let choices: Vec<Value> = models
            .iter()
            .map(|m| json!({ "value": m["modelId"], "name": m["name"], "description": m["description"] }))
            .collect();
        self.config.push(json!({
            "id": MODEL_STATE, "name": "Model", "category": "model", "type": "select",
            "currentValue": self.models["currentModelId"], "options": choices,
        }));
    }

    /// A way to sign in the agent runs itself (`authenticate`): not one that
    /// needs a terminal or an environment variable set.
    fn auth_method(&self) -> Option<Value> {
        self.auths
            .iter()
            .find(|m| {
                !matches!(text(&m["type"]), "terminal" | "env_var")
                    && m["_meta"]["terminal-auth"].is_null()
            })
            .cloned()
    }

    /// The first config option of a category that is a select (an option
    /// with no category but the id names its kind: OpenCode 1.x's).
    fn select(&self, category: &str) -> Option<&Value> {
        self.config.iter().find(|option| {
            (option["category"] == category
                || (option["category"].is_null() && option["id"] == category))
                && option["type"] == "select"
        })
    }

    fn model_status(&self) -> Option<Value> {
        let model = self.select("model")?;
        let current = &model["currentValue"];
        let label = choices(model)
            .into_iter()
            .find(|choice| &choice["value"] == current)
            .map_or("", |choice| {
                model_name(text(current), text(&choice["name"]))
            });
        let effort = self.select("thought_level");
        let (efforts, unset) = effort_levels(effort);
        Some(json!({
            "type": "model_status", "provider": "acp", "model": text(current), "model_label": label,
            "reasoning_effort": effort.map_or("", |e| text(&e["currentValue"])),
            "reasoning_efforts": efforts, "default_effort": unset,
            "context_window": self.usage_size, "context_limit": 0,
            "state": if self.prompt_in_flight { "streaming" } else { "ready" },
        }))
    }

    fn model_view(&self) -> Option<Value> {
        let model = self.select("model")?;
        let current = &model["currentValue"];
        let effort = self.select("thought_level");
        let (efforts, _) = effort_levels(effort);
        let row = |value: &Value, label: &Value| json!({ "model": value, "label": label, "active": value == current, "context_window": 0, "max_output_tokens": 0, "reasoning_efforts": efforts });
        let mut rows: Vec<Value> = choices(model)
            .into_iter()
            .map(|choice| {
                let value = &choice["value"];
                row(
                    value,
                    &json!(model_name(text(value), text(&choice["name"]))),
                )
            })
            .collect();
        if !rows.iter().any(|row| row["active"] == true) {
            let mut unlisted = row(current, current);
            unlisted["listed"] = json!(false);
            rows.insert(0, unlisted);
        }
        Some(json!({
            "type": "model_view", "models": rows,
            "active_effort": effort.map_or("", |e| text(&e["currentValue"])),
        }))
    }

    /// The agent's settings besides model and effort (and, for OpenCode,
    /// besides the mode, which is the approval mode).
    fn session_options(&self) -> Value {
        let main: Vec<&Value> = [self.select("model"), self.select("thought_level")]
            .into_iter()
            .flatten()
            .map(|option| &option["id"])
            .collect();
        let options: Vec<Value> = if !self.has_config() {
            self.modes["availableModes"]
                .as_array()
                .filter(|_| !self.opencode)
                .map(|modes| {
                    let choices: Vec<Value> = modes
                        .iter()
                        .map(|m| json!({ "value": m["id"], "name": m["name"], "description": m["description"] }))
                        .collect();
                    vec![json!({
                        "id": "mode", "name": "Mode", "description": null, "category": "mode",
                        "type": "select", "value": self.modes["currentModeId"], "choices": choices,
                    })]
                })
                .unwrap_or_default()
        } else {
            self.config
                .iter()
                .filter(|option| !main.contains(&&option["id"]))
                .filter(|option| !(self.opencode && option["id"] == "mode"))
                .map(|option| {
                    let mut entry = json!({
                        "id": option["id"], "name": option["name"], "description": option["description"],
                        "category": option["category"], "type": option["type"], "value": option["currentValue"],
                    });
                    if option["type"] == "select" {
                        entry["choices"] = choices(option)
                            .into_iter()
                            .map(|c| json!({ "value": c["value"], "name": c["name"], "description": c["description"] }))
                            .collect();
                    }
                    entry
                })
                .collect()
        };
        json!({ "type": "session_options", "options": options })
    }

    fn command_list(&self) -> Value {
        let commands: Vec<Value> = self
            .commands
            .iter()
            .map(|c| json!({ "command": format!("/{}", text(&c["name"])), "marker": null, "args": text(&c["input"]["hint"]), "description": c["description"] }))
            .collect();
        json!({ "type": "command_list", "commands": commands })
    }

    /// `engine_caps` when it changed since last sent.
    fn caps_changed(&mut self) -> Option<Value> {
        let approvals = self
            .preset
            .map(|p| p.approvals)
            .or(self.opencode.then_some(true));
        let caps = json!({
            "type": "engine_caps",
            "agent": { "id": self.preset.map(|p| p.id), "name": self.agent_name(), "version": self.agent["version"] },
            "approvals": approvals,
            "gateway": self.preset.is_some_and(|p| p.gateway) || self.opencode,
            "caps": {
                "interrupt": true,
                "approvalModes": approvals == Some(true),
                "modelPicker": self.select("model").is_some(),
                "contextUsage": self.usage_seen,
                "slashCommands": !self.commands.is_empty(),
                "transcriptReplay": self.capabilities["loadSession"] == true,
                "fork": self.capabilities["sessionCapabilities"]["fork"].is_object(),
            },
        });
        (caps != self.caps_sent).then(|| {
            self.caps_sent = caps.clone();
            caps
        })
    }

    /// The config options changed: what clients see of them.
    fn config_changed(&mut self, options: &Value) -> Vec<Value> {
        if let Some(options) = options.as_array() {
            self.config = options.clone();
        }
        self.with_model_state();
        let mut events: Vec<Value> = self.model_status().into_iter().collect();
        events.push(self.session_options());
        events.extend(self.caps_changed());
        events
    }

    /// `session/set_config_option` for option `id` (a select's value or a
    /// boolean).
    fn set_option(&mut self, id: &Value, value: &Value) -> String {
        let session = self.session.clone();
        // A model of the agent's `models`: `session/set_model`.
        if id == MODEL_STATE {
            return self.request(
                "session/set_model",
                json!({ "sessionId": session, "modelId": value }),
            );
        }
        let mut params = json!({ "sessionId": session, "configId": id, "value": value });
        if value.is_boolean() {
            params["type"] = json!("boolean");
        }
        self.request("session/set_config_option", params)
    }

    /// The conversation is open: its state, then the model and effort asked
    /// for, OpenCode's agent, and the first queued message.
    fn opened(&mut self, result: &Value) -> Output {
        self.config = result["configOptions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        self.modes = result["modes"].clone();
        self.models = result["models"].clone();
        self.with_model_state();
        let mut events = Vec::new();
        if let Some(replay) = self.replay.take() {
            self.tools.clear();
            let plan = replay.plan.clone();
            events.push(json!({ "type": "transcript", "items": replay.items() }));
            events.extend(plan);
        }
        let current = self.select("model").map(|m| {
            let value = text(&m["currentValue"]);
            choices(m)
                .into_iter()
                .find(|c| c["value"] == value)
                .map_or(value, |c| model_name(value, text(&c["name"])))
                .to_string()
        });
        events.push(json!({
            "type": "startup", "model": current.unwrap_or_default(),
            "cwd": self.cwd, "session_id": "", "context_window": self.usage_size,
        }));
        events.extend(self.model_status());
        events.push(self.session_options());
        if !self.commands.is_empty() {
            events.push(self.command_list());
        }
        if let Some(mode) = self.approval_mode() {
            events.push(json!({ "type": "approval_mode", "mode": mode }));
        }
        events.extend(self.caps_changed());
        events.push(json!({ "type": "status", "message": "ready" }));
        let mut frames = Vec::new();
        // The model asked for, as the agent names it (`provider/model`).
        let wanted = self.model.clone().and_then(|model| {
            let option = self.select("model")?;
            let choices = choices(option);
            let value = choices
                .iter()
                .map(|c| &c["value"])
                .find(|v| text(v) == model)
                .or_else(|| {
                    choices
                        .iter()
                        .map(|c| &c["value"])
                        .find(|v| text(v).ends_with(&format!("/{model}")))
                })
                .cloned()
                .unwrap_or(json!(model));
            (value != option["currentValue"]).then(|| (option["id"].clone(), value))
        });
        if let Some((id, value)) = wanted {
            frames.push(self.set_option(&id, &value));
        }
        let effort = self.effort.clone().and_then(|effort| {
            let option = self.select("thought_level")?;
            (option["currentValue"] != effort.as_str())
                .then(|| option["id"].clone())
                .map(|id| (id, effort))
        });
        if let Some((id, effort)) = effort {
            frames.push(self.set_option(&id, &json!(effort)));
        }
        // OpenCode: the agent of the client's mode, before the first prompt.
        frames.extend(self.switch_agent());
        let mut output = Output { events, frames };
        if let Some(next) = self.next_queued() {
            output.events.extend(next.events);
            output.frames.extend(next.frames);
        }
        output
    }

    /// The turn settled: start the next queued prompt, or go ready.
    fn settle(&mut self) -> Output {
        self.prompt_in_flight = false;
        self.busy_announced = false;
        self.next_queued().unwrap_or_else(|| {
            Output::events(vec![json!({ "type": "status", "message": "ready" })])
        })
    }

    /// One update of a conversation being replayed (`session/load` or a
    /// copy): its text, files, finished tool calls and plan as transcript
    /// rows.
    fn on_replay(&mut self, update: &Value) {
        let Some(replay) = self.replay.as_mut() else {
            return;
        };
        match text(&update["sessionUpdate"]) {
            // The plan reminder is not what the user wrote.
            "user_message_chunk" if opencode::for_the_model_only(&update["content"]) => {}
            // A file the user attached (as `send` links it).
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
                let tool = self.tools.entry(id.clone()).or_insert_with(|| Tool {
                    name: "tool".to_string(),
                    kind: "other".to_string(),
                    ..Tool::default()
                });
                tool.update(update);
                replay.tool(&id, tool);
            }
            "plan" => replay.plan = Some(plan(update)),
            _ => {}
        }
    }

    fn on_update(&mut self, update: &Value) -> Vec<Value> {
        let kind = text(&update["sessionUpdate"]);
        if self.replay.is_some()
            && matches!(
                kind,
                "user_message_chunk"
                    | "agent_message_chunk"
                    | "agent_thought_chunk"
                    | "tool_call"
                    | "tool_call_update"
                    | "plan"
            )
        {
            self.on_replay(update);
            return vec![];
        }
        match kind {
            // Outside a turn (pi-acp's startup banner): not a reply.
            "agent_message_chunk" | "agent_thought_chunk" if !self.prompt_in_flight => vec![],
            "agent_message_chunk" => match block_text(&update["content"]) {
                "" => vec![],
                chunk => {
                    self.replied = true;
                    vec![json!({ "type": "assistant_delta", "delta": chunk })]
                }
            },
            "agent_thought_chunk" => match block_text(&update["content"]) {
                "" => vec![],
                chunk => {
                    self.replied = true;
                    vec![json!({ "type": "reasoning_delta", "delta": chunk })]
                }
            },
            "tool_call" => {
                let id = text(&update["toolCallId"]).to_string();
                if id.is_empty() {
                    return vec![];
                }
                self.replied = true;
                let mut tool = Tool {
                    name: "tool".to_string(),
                    kind: "other".to_string(),
                    ..Tool::default()
                };
                tool.update(update);
                let mut events = vec![
                    json!({ "type": "tool_start", "call_id": id, "name": tool.display_name() }),
                    json!({ "type": "tool_update", "call_id": id, "output": tool.output() }),
                ];
                let status = text(&update["status"]);
                if matches!(status, "completed" | "failed") {
                    events.push(json!({ "type": "tool_output", "call_id": id, "name": tool.display_name(), "output": tool.output(), "is_error": status == "failed" }));
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
                tool.update(update);
                let status = text(&update["status"]);
                if matches!(status, "completed" | "failed") {
                    let tool = self.tools.remove(&id).unwrap_or_default();
                    vec![
                        json!({ "type": "tool_output", "call_id": id, "name": tool.display_name(), "output": tool.output(), "is_error": status == "failed" }),
                    ]
                } else {
                    vec![json!({ "type": "tool_update", "call_id": id, "output": tool.output() })]
                }
            }
            "plan" => vec![plan(update)],
            "config_option_update" => self.config_changed(&update["configOptions"]),
            "current_mode_update" if !self.has_config() => {
                self.modes["currentModeId"] = update["currentModeId"].clone();
                vec![self.session_options()]
            }
            "usage_update" => {
                let mut usage = json!({ "type": "context_usage", "tokens": update["used"] });
                if update["cost"]["currency"] == "USD" {
                    usage["cost"] = update["cost"]["amount"].clone();
                }
                let mut events = vec![usage];
                self.usage_seen = true;
                let size = update["size"].as_u64().unwrap_or(0);
                if size != self.usage_size {
                    self.usage_size = size;
                    events.extend(self.model_status());
                }
                events.extend(self.caps_changed());
                events
            }
            "available_commands_update" => {
                self.commands = update["availableCommands"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let mut events = vec![self.command_list()];
                events.extend(self.caps_changed());
                events
            }
            "notice" => {
                let mut message = format!("[acp] {}", text(&update["title"]));
                if !text(&update["description"]).is_empty() {
                    message.push('\n');
                    message.push_str(text(&update["description"]));
                }
                let kind = if update["severity"] == "error" {
                    "error"
                } else {
                    "info"
                };
                vec![json!({ "type": kind, "message": message })]
            }
            _ => vec![],
        }
    }

    fn on_response(&mut self, id: u64, result: &Value, error: &Value) -> Output {
        let Some((method, params)) = self.pending.remove(&id) else {
            return Output::default();
        };
        // OpenCode's switch to the agent of a mode.
        if self.switch.is_some_and(|(switch, _)| switch == id) {
            return self.switched(id, &method, result, error);
        }
        if !error.is_null() {
            return self.on_error(&method, error);
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
                self.capabilities = result["agentCapabilities"].clone();
                self.image_prompts = self.capabilities["promptCapabilities"]["image"] == true;
                self.mcp_http = self.capabilities["mcpCapabilities"]["http"] == true;
                self.agent = result["agentInfo"].clone();
                self.auths = result["authMethods"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                self.open_session()
            }
            // Signed in: the session it could not open before.
            "authenticate" => {
                let mut output = Output::events(vec![
                    json!({ "type": "info", "message": format!("[acp] signed in to {}", self.agent_name()) }),
                ]);
                let open = self.open_session();
                output.events.extend(open.events);
                output.frames.extend(open.frames);
                output
            }
            "session/set_model" => {
                self.models["currentModelId"] = params["modelId"].clone();
                self.with_model_state();
                Output::events(self.model_status().into_iter().collect())
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
                self.fork = None;
                self.session = match method.as_str() {
                    "session/fork" => result["sessionId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_string),
                    _ => self.reopen.take(),
                };
                self.opened(result)
            }
            "session/set_config_option" => {
                Output::events(self.config_changed(&result["configOptions"]))
            }
            "session/set_mode" => {
                self.modes["currentModeId"] = params["modeId"].clone();
                Output::events(vec![self.session_options()])
            }
            "session/prompt" => {
                let mut output = Output::default();
                match text(&result["stopReason"]) {
                    "refusal" => output.events.push(json!({ "type": "info", "message": "[acp] the agent refused this request" })),
                    reason @ ("max_tokens" | "max_turn_requests") => {
                        output.events.push(json!({ "type": "info", "message": format!("[acp] the turn stopped at a limit ({reason})") }))
                    }
                    "cancelled" => output.events.push(json!({ "type": "status", "message": "interrupted" })),
                    "end_turn" if !self.replied => output.events.push(json!({ "type": "info", "message": "[acp] the agent ended the turn without a reply" })),
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

    fn on_error(&mut self, method: &str, error: &Value) -> Output {
        let message = match text(&error["message"]) {
            "" => format!("JSON-RPC error {}", error["code"]),
            message => message.to_string(),
        };
        let opening = matches!(
            method,
            "session/new" | "session/load" | "session/resume" | "session/fork"
        );
        // Not signed in: once, the way the agent signs in by itself (it may
        // open the browser); then the session again (`authenticate` above).
        if opening && error["code"] == -32000 && !self.authenticated && !self.probing {
            if let Some(how) = self.auth_method() {
                self.authenticated = true;
                // The conversation to reopen or copy is asked for again once
                // signed in (`open_session`).
                self.replay = None;
                let name = [&how["name"], &how["id"]]
                    .into_iter()
                    .map(text)
                    .find(|n| !n.is_empty())
                    .unwrap_or_default()
                    .to_string();
                return Output {
                    frames: vec![self.request("authenticate", json!({ "methodId": how["id"] }))],
                    events: vec![
                        json!({ "type": "info", "message": format!("[acp] signing in to {} ({name})…", self.agent_name()) }),
                    ],
                };
            }
        }
        let signin = (opening && error["code"] == -32000) || method == "authenticate";
        let message = if signin {
            let auth = self.auths.first().cloned().unwrap_or_default();
            let how = [&auth["description"], &auth["name"]]
                .into_iter()
                .map(text)
                .find(|how| !how.is_empty())
                .unwrap_or(&message)
                .to_string();
            format!("[acp] {} needs sign-in: {how}", self.agent_name())
        } else {
            format!("[acp] {message}")
        };
        match method {
            // The conversation to reopen or copy is gone (or the agent
            // refused it): a new one starts, and clients are told.
            "session/load" | "session/resume" | "session/fork" if !signin => {
                self.replay = None;
                self.tools.clear();
                self.reopen = None;
                self.fork = None;
                let event = if method == "session/fork" {
                    json!({ "type": "error", "message": format!("[acp] the agent could not copy this conversation ({}); a new one starts", text(&error["message"])) })
                } else {
                    json!({ "type": "info", "message": format!("[acp] the agent could not reopen this conversation ({}); a new one starts", text(&error["message"])) })
                };
                Output {
                    events: vec![event],
                    frames: vec![self.new_session()],
                }
            }
            "session/prompt" => {
                let mut output =
                    Output::events(vec![json!({ "type": "error", "message": message })]);
                let settled = self.settle();
                output.events.extend(settled.events);
                output.frames.extend(settled.frames);
                output
            }
            "session/new" | "session/load" | "session/resume" | "session/fork" | "initialize"
            | "authenticate" => {
                let mut output =
                    Output::events(vec![json!({ "type": "error", "message": message })]);
                if !self.queued.is_empty() {
                    self.queued.clear();
                    output.events.push(self.pending_event());
                    output.events.push(json!({ "type": "error", "message": "The agent could not open a session; the messages waiting for it were not sent" }));
                }
                self.replay = None;
                self.reopen = None;
                self.fork = None;
                self.session_failed = method != "initialize";
                output
                    .events
                    .push(json!({ "type": "status", "message": "ready" }));
                output
            }
            _ => Output::events(vec![json!({ "type": "error", "message": message })]),
        }
    }

    fn on_request(&mut self, id: &Value, method: &str, params: &Value) -> Output {
        let form = method == "elicitation/create"
            && matches!(params["mode"].as_str(), None | Some("form"));
        if method != "session/request_permission" && !form {
            // The client offers no file system or terminal (`initialize`):
            // an agent that asks anyway (OpenCode mirrors each approved edit
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
        if form {
            self.approval_seq += 1;
            let call = format!("acp-approval-{}", self.approval_seq);
            // A lone unnamed field is asked by the message itself.
            let message = text(&params["message"]);
            let mut schema = params["requestedSchema"].clone();
            if let Some(fields) = schema["properties"].as_object_mut() {
                if fields.len() == 1 {
                    if let Some(field) = fields.values_mut().next() {
                        if text(&field["title"]).is_empty() {
                            field["title"] = json!(message);
                        }
                    }
                }
            }
            let event = json!({
                "type": "approval_request", "call_id": call, "name": "mcp_elicitation",
                "summary": message, "subagent_id": null, "hunks": null,
                "questions": claude::elicitation_questions(&schema),
            });
            self.approvals.insert(call, (id.clone(), Ask::Form(schema)));
            return Output::events(vec![event]);
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
        self.approvals
            .insert(call.clone(), (id.clone(), Ask::Permission(options)));
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

    /// `/model` (the picker, or a switch to a model and effort) and the
    /// agent's own commands, which run as prompts.
    fn command(&mut self, input: &str) -> Result<Output, String> {
        let mut words = input.split_whitespace();
        let name = words.next().unwrap_or_default();
        if name == "/model" {
            let unavailable = || format!("{input} is not available in this agent session");
            let (Some(value), effort) = (words.next(), words.next()) else {
                return self
                    .model_view()
                    .map(|view| Output::events(vec![view]))
                    .ok_or_else(unavailable);
            };
            let model = self.select("model").cloned().ok_or_else(unavailable)?;
            let mut frames = Vec::new();
            if model["currentValue"] != value {
                frames.push(self.set_option(&model["id"], &json!(value)));
            }
            let effort = effort.zip(self.select("thought_level").cloned());
            if let Some((effort, option)) = effort {
                if option["currentValue"] != effort {
                    frames.push(self.set_option(&option["id"], &json!(effort)));
                }
            }
            return Ok(Output {
                frames,
                events: vec![],
            });
        }
        let known = self
            .commands
            .iter()
            .any(|c| name.strip_prefix('/') == Some(text(&c["name"])));
        if !known {
            return Err(format!("{name} is not available in this agent session"));
        }
        if let Some(reason) = &self.mode_failed {
            return Err(reason.clone());
        }
        Ok(self.submit(Waiting {
            text: input.trim().to_string(),
            images: Vec::new(),
            files: Vec::new(),
        }))
    }

    fn set_session_option(&mut self, op: &Value) -> Result<Output, String> {
        let id = text(&op["id"]);
        let value = &op["value"];
        // OpenCode's mode is the approval mode (`set_approval_mode`).
        if self.opencode && id == "mode" {
            return Err("OpenCode's mode follows the approval mode".to_string());
        }
        if !self.has_config() && id == "mode" && self.modes.is_object() {
            let session = self.session.clone();
            let frame = self.request(
                "session/set_mode",
                json!({ "sessionId": session, "modeId": value }),
            );
            return Ok(Output {
                frames: vec![frame],
                events: vec![],
            });
        }
        if !self.config.iter().any(|option| option["id"] == id) {
            return Err(format!("this agent has no option {id}"));
        }
        Ok(Output {
            frames: vec![self.set_option(&op["id"], value)],
            events: vec![],
        })
    }
}

fn plan(update: &Value) -> Value {
    let plan: Vec<Value> = update["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| json!({ "step": text(&entry["content"]), "status": match text(&entry["status"]) { "" => "pending", s => s } }))
        .collect();
    json!({ "type": "plan", "plan": plan })
}

impl Adapter for Acp {
    fn start(&mut self) -> Vec<String> {
        vec![self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientInfo": { "name": "lynshen", "title": "LynShen", "version": env!("CARGO_PKG_VERSION") },
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                    "elicitation": { "form": {} },
                    "session": { "configOptions": { "boolean": {} }, "notices": {} },
                },
            }),
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
            // The agent withdrew a question: it no longer waits for the answer.
            Some("$/cancel_request") => {
                let request = &frame["params"]["requestId"];
                self.approvals.retain(|_, (asked, _)| asked != request);
                Output::default()
            }
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
                let images = op["images"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|image| text(image).to_string())
                    .collect();
                return Ok(self.submit(Waiting {
                    text: text(&op["content"]).to_string(),
                    images,
                    files: op_files(op),
                }));
            }
            "command" => return self.command(text(&op["input"])),
            "set_session_option" => return self.set_session_option(op),
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
                    set_files(&mut unqueued, &message.files);
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
                let Some((request, ask)) = self.approvals.remove(call) else {
                    return Err(format!("no open approval {call}"));
                };
                let deny = op["decision"] == "deny";
                let result = match ask {
                    Ask::Form(_) if deny => json!({ "action": "decline" }),
                    Ask::Form(schema) => {
                        json!({ "action": "accept", "content": claude::elicitation_content(&schema, &op["answers"]) })
                    }
                    Ask::Permission(options) => {
                        let kinds: &[&str] = if deny {
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
                        json!({ "outcome": outcome })
                    }
                };
                vec![json!({ "jsonrpc": "2.0", "id": request, "result": result }).to_string()]
            }
            // `steer` (or `interrupt` with `index`): an agent cannot take a
            // message mid-turn, so the turn is cancelled and, once it
            // settles, the chosen waiting message starts the next one, ahead
            // of the others. A plain `interrupt` drops what waits. Open
            // questions are answered as cancelled either way.
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
                } else {
                    self.queued.clear();
                    events.push(self.pending_event());
                }
                let mut frames: Vec<String> = self
                    .approvals
                    .drain()
                    .map(|(_, (request, ask))| {
                        let result = match ask {
                            Ask::Permission(_) => json!({ "outcome": { "outcome": "cancelled" } }),
                            Ask::Form(_) => json!({ "action": "cancel" }),
                        };
                        json!({ "jsonrpc": "2.0", "id": request, "result": result }).to_string()
                    })
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
            // oh-my-pi: a different mode restarts it (`restart_for`); the
            // same one is acknowledged.
            "set_approval_mode" if self.launch_modes() => {
                return Ok(Output::events(vec![
                    json!({ "type": "approval_mode", "mode": self.launched.name() }),
                ]));
            }
            "shutdown" => vec![],
            other => return Err(format!("ACP agents do not support {other}")),
        };
        Ok(Output {
            events: Vec::new(),
            frames,
        })
    }

    fn busy(&self) -> bool {
        self.prompt_in_flight
    }

    /// oh-my-pi takes its approval mode as a launch flag: another mode
    /// restarts the agent, which reopens the conversation.
    fn restart_for(&self, op: &Value) -> Option<Options> {
        let switch = op["op"] == "set_approval_mode"
            && self.launch_modes()
            && Mode::of(op["mode"].as_str()) != self.launched;
        switch.then(|| Options {
            approval_mode: op["mode"].as_str().map(str::to_string),
            ..Options::default()
        })
    }

    /// The agent's own session id (the daemon names the session itself).
    fn conversation(&self) -> Option<String> {
        self.session.clone()
    }

    /// The agent restores its model when it reopens a conversation; this
    /// keeps an idle close from switching back to the model it started on.
    fn model(&self) -> Option<String> {
        self.select("model")
            .and_then(|option| option["currentValue"].as_str())
            .map(str::to_string)
    }

    /// The mode a restart keeps: OpenCode's as the client set it, a preset's
    /// as launched.
    fn approval_mode(&self) -> Option<String> {
        if self.opencode {
            return self.mode.clone();
        }
        self.preset.map(|_| self.launched.name().to_string())
    }

    /// OpenCode switches agents for the next turn, oh-my-pi restarts: a
    /// switch during a turn waits for it (a switch to 全自动 answers its
    /// open requests at once).
    fn mode_applies_live(&self) -> bool {
        !self.opencode && !self.launch_modes()
    }

    /// OpenCode and the built-in agents that can run on the LynShen gateway.
    fn has_gateway(&self) -> bool {
        self.opencode || self.preset.is_some_and(|preset| preset.gateway)
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

    // Upstream-derived: the full client (presets, config options, forms,
    // sign-in, MCP, usage, notices).

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|e| text(&e["type"])).collect()
    }

    fn event<'a>(events: &'a [Value], kind: &str) -> &'a Value {
        events
            .iter()
            .find(|e| e["type"] == kind)
            .unwrap_or_else(|| panic!("no {kind} in {events:#?}"))
    }

    fn up(acp: &mut Acp, update: Value) -> Output {
        frame(
            acp,
            json!({ "method": "session/update", "params": { "sessionId": "s1", "update": update } }),
        )
    }

    fn config() -> Value {
        json!([
            { "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "lynshen/gpt-6",
              "options": [{ "group": "lynshen", "name": "LynShen", "options": [
                  { "value": "lynshen/gpt-6", "name": "GPT-6" }, { "value": "lynshen/claude-5", "name": "Claude 5" }] }] },
            { "id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "low",
              "options": [{ "value": "low", "name": "Low" }, { "value": "high", "name": "High" }] },
            { "id": "mode", "name": "Session Mode", "category": "mode", "type": "select", "currentValue": "build",
              "options": [{ "value": "build", "name": "build" }, { "value": "plan", "name": "plan" }] },
            { "id": "x", "name": "Auto compact", "type": "boolean", "currentValue": true },
        ])
    }

    /// An adapter through `initialize` with `caps`, before the open answers.
    fn started(options: Options, caps: Value) -> (Acp, Vec<Value>) {
        let mut a = Acp::new(Path::new("/p"), &options);
        a.start();
        let out = frame(
            &mut a,
            json!({ "id": 1, "result": { "protocolVersion": 1, "agentCapabilities": caps,
                "agentInfo": { "name": "omp", "title": "Oh My Pi", "version": "18.1.21" },
                "authMethods": [{ "id": "agent", "name": "Agent login", "description": "Run omp login" }] } }),
        );
        (a, sent(&out.frames))
    }

    fn omp() -> Options {
        Options {
            agent: Some("omp".into()),
            approval_mode: Some("read-only".into()),
            ..Options::default()
        }
    }

    /// An open omp session with `config()`.
    fn opened() -> Acp {
        let (mut a, _) = started(omp(), json!({ "loadSession": true }));
        frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "configOptions": config() } }),
        );
        a
    }

    #[test]
    fn the_handshake_offers_form_elicitation_boolean_options_and_notices() {
        let mut a = Acp::new(Path::new("/p"), &Options::default());
        let init = &sent(&a.start())[0];
        assert_eq!(init["method"], "initialize");
        let caps = &init["params"]["clientCapabilities"];
        assert_eq!(caps["elicitation"], json!({ "form": {} }));
        assert_eq!(caps["session"]["configOptions"]["boolean"], json!({}));
        assert_eq!(caps["session"]["notices"], json!({}));
        assert_eq!(caps["terminal"], false);
        assert_eq!(init["params"]["clientInfo"]["name"], "lynshen");
    }

    #[test]
    fn the_model_and_effort_to_start_with_are_set_once_open() {
        // A new session's picks made before it started (the desktop's draft).
        let (mut a, _) = started(
            Options {
                model: Some("lynshen/claude-5".into()),
                effort: Some("high".into()),
                ..omp()
            },
            json!({}),
        );
        let open = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "configOptions": config() } }),
        );
        let frames = sent(&open.frames);
        let set: Vec<(&Value, &Value)> = frames
            .iter()
            .map(|f| (&f["params"]["configId"], &f["params"]["value"]))
            .collect();
        assert_eq!(
            set,
            [
                (&json!("model"), &json!("lynshen/claude-5")),
                (&json!("effort"), &json!("high"))
            ]
        );
        // What it already runs asks for nothing.
        let (mut a, _) = started(
            Options {
                effort: Some("low".into()),
                ..omp()
            },
            json!({}),
        );
        let open = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "configOptions": config() } }),
        );
        assert!(open.frames.is_empty());
    }

    #[test]
    fn an_open_session_reports_model_options_mode_and_caps() {
        let (mut a, frames) = started(
            Options {
                model: Some("claude-5".into()),
                ..omp()
            },
            json!({ "loadSession": true }),
        );
        assert_eq!(frames[0]["method"], "session/new");
        assert_eq!(frames[0]["params"]["mcpServers"], json!([]));
        let open = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "configOptions": config() } }),
        );
        assert_eq!(
            types(&open.events),
            [
                "startup",
                "model_status",
                "session_options",
                "approval_mode",
                "engine_caps",
                "status"
            ]
        );
        assert_eq!(open.events[0]["model"], "GPT-6");
        let status = &open.events[1];
        assert_eq!(status["model"], "lynshen/gpt-6");
        assert_eq!(status["model_label"], "GPT-6");
        assert_eq!(status["reasoning_effort"], "low");
        assert_eq!(status["reasoning_efforts"], json!(["low", "high"]));
        let options = &open.events[2]["options"];
        assert_eq!(
            options.as_array().unwrap().len(),
            2,
            "model and effort left out"
        );
        assert_eq!(options[0]["choices"][1]["value"], "plan");
        assert_eq!(options[1]["type"], "boolean");
        assert_eq!(options[1]["value"], true);
        assert!(options[1].get("choices").is_none());
        assert_eq!(open.events[3]["mode"], "read-only");
        assert_eq!(
            open.events[4],
            json!({ "type": "engine_caps",
                "agent": { "id": "omp", "name": "Oh My Pi", "version": "18.1.21" },
                "approvals": true, "gateway": true,
                "caps": { "interrupt": true, "approvalModes": true, "modelPicker": true, "contextUsage": false,
                          "slashCommands": false, "transcriptReplay": true, "fork": false } })
        );
        // The asked-for model, as the agent names it.
        let set = &sent(&open.frames)[0];
        assert_eq!(set["method"], "session/set_config_option");
        assert_eq!(set["params"]["configId"], "model");
        assert_eq!(set["params"]["value"], "lynshen/claude-5");
        assert_eq!(a.conversation().as_deref(), Some("s1"));
    }

    #[test]
    fn a_custom_agent_has_no_preset_metadata() {
        let (mut a, _) = started(
            Options {
                command: Some("agent".into()),
                ..Options::default()
            },
            json!({}),
        );
        let open = frame(&mut a, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        assert!(!types(&open.events).contains(&"model_status"));
        assert!(!types(&open.events).contains(&"approval_mode"));
        let caps = event(&open.events, "engine_caps");
        assert_eq!(caps["agent"]["id"], Value::Null);
        assert_eq!(caps["agent"]["name"], "Oh My Pi");
        assert_eq!(caps["approvals"], Value::Null);
        assert_eq!(caps["gateway"], false);
        assert_eq!(caps["caps"]["modelPicker"], false);
        assert!(a
            .encode(&json!({ "op": "set_approval_mode", "mode": "full-access" }))
            .is_err());
        assert!(a
            .restart_for(&json!({ "op": "set_approval_mode", "mode": "full-access" }))
            .is_none());
        assert!(a
            .encode(&json!({ "op": "command", "input": "/model" }))
            .is_err());
        assert!(!a.has_gateway());
    }

    #[test]
    fn config_updates_show_the_model_unlisted_values_and_flips() {
        let mut a = opened();
        let mut options = config();
        options[0]["currentValue"] = json!("other/model");
        options.as_array_mut().unwrap().remove(1);
        let out = up(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": options }),
        );
        assert_eq!(types(&out.events), ["model_status", "session_options"]);
        assert_eq!(out.events[0]["model"], "other/model");
        assert_eq!(out.events[0]["model_label"], "");
        assert_eq!(out.events[0]["reasoning_efforts"], json!([]));
        let view = a
            .encode(&json!({ "op": "command", "input": "/model" }))
            .unwrap();
        let rows = &view.events[0]["models"];
        assert_eq!(rows[0]["model"], "other/model");
        assert_eq!(rows[0]["listed"], false);
        assert_eq!(rows[1]["label"], "GPT-6");
        assert_eq!(rows.as_array().unwrap().len(), 3);
        // No model option any more: the picker goes.
        let out = up(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": [] }),
        );
        assert_eq!(types(&out.events), ["session_options", "engine_caps"]);
        assert_eq!(out.events[1]["caps"]["modelPicker"], false);
    }

    #[test]
    fn model_and_effort_switches_are_config_option_requests() {
        let mut a = opened();
        let out = a
            .encode(&json!({ "op": "command", "input": "/model lynshen/claude-5 high" }))
            .unwrap();
        let frames = sent(&out.frames);
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0]["params"],
            json!({ "sessionId": "s1", "configId": "model", "value": "lynshen/claude-5" })
        );
        assert_eq!(frames[1]["params"]["configId"], "effort");
        assert_eq!(frames[1]["params"]["value"], "high");
        // The answer carries the options as they are now.
        let mut options = config();
        options[0]["currentValue"] = json!("lynshen/claude-5");
        let id = frames[0]["id"].clone();
        let done = frame(
            &mut a,
            json!({ "id": id, "result": { "configOptions": options } }),
        );
        assert_eq!(done.events[0]["model"], "lynshen/claude-5");
        // The same model and effort again ask for nothing.
        let same = a
            .encode(&json!({ "op": "command", "input": "/model lynshen/claude-5 low" }))
            .unwrap();
        assert!(same.frames.is_empty());
    }

    #[test]
    fn an_agent_default_effort_is_the_unset_state_and_names_drop_the_provider() {
        let mut a = opened();
        let options = json!([
            { "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "deepseek/deepseek-flash",
              "options": [{ "value": "deepseek/deepseek-flash", "name": "deepseek/DeepSeek V4.1 Flash" },
                          { "value": "openai/gpt-5", "name": "GPT-5" }] },
            { "id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "default",
              "options": [{ "value": "none", "name": "None" }, { "value": "low", "name": "Low" },
                          { "value": "high", "name": "High" }, { "value": "max", "name": "Max" },
                          { "value": "default", "name": "Default" }] },
        ]);
        let out = up(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": options }),
        );
        let status = event(&out.events, "model_status");
        assert_eq!(status["model_label"], "DeepSeek V4.1 Flash");
        assert_eq!(status["reasoning_effort"], "default");
        assert_eq!(
            status["reasoning_efforts"],
            json!(["none", "low", "high", "max"])
        );
        assert_eq!(status["default_effort"], "default");
        let view = a
            .encode(&json!({ "op": "command", "input": "/model" }))
            .unwrap();
        let rows = &view.events[0]["models"];
        assert_eq!(rows[0]["label"], "DeepSeek V4.1 Flash");
        assert_eq!(rows[1]["label"], "GPT-5");
        let out = up(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": config() }),
        );
        assert_eq!(event(&out.events, "model_status")["default_effort"], "");
    }

    #[test]
    fn session_options_set_selects_booleans_and_legacy_modes() {
        let mut a = opened();
        let select = sent(
            &a.encode(&json!({ "op": "set_session_option", "id": "mode", "value": "plan" }))
                .unwrap()
                .frames,
        );
        assert_eq!(select[0]["method"], "session/set_config_option");
        assert!(select[0]["params"].get("type").is_none());
        let boolean = sent(
            &a.encode(&json!({ "op": "set_session_option", "id": "x", "value": false }))
                .unwrap()
                .frames,
        );
        assert_eq!(boolean[0]["params"]["type"], "boolean");
        assert_eq!(boolean[0]["params"]["value"], false);
        assert!(a
            .encode(&json!({ "op": "set_session_option", "id": "nope", "value": 1 }))
            .is_err());

        // An agent with only legacy modes.
        let (mut b, _) = started(omp(), json!({}));
        let open = frame(
            &mut b,
            json!({ "id": 2, "result": { "sessionId": "s2", "modes": { "currentModeId": "ask",
                "availableModes": [{ "id": "ask", "name": "Ask" }, { "id": "code", "name": "Code" }] } } }),
        );
        let options = &event(&open.events, "session_options")["options"];
        assert_eq!(options[0]["id"], "mode");
        assert_eq!(options[0]["value"], "ask");
        let set = sent(
            &b.encode(&json!({ "op": "set_session_option", "id": "mode", "value": "code" }))
                .unwrap()
                .frames,
        );
        assert_eq!(set[0]["method"], "session/set_mode");
        assert_eq!(set[0]["params"]["modeId"], "code");
        let done = frame(&mut b, json!({ "id": set[0]["id"], "result": {} }));
        assert_eq!(done.events[0]["options"][0]["value"], "code");
        let changed = up(
            &mut b,
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "ask" }),
        );
        assert_eq!(changed.events[0]["options"][0]["value"], "ask");
    }

    #[test]
    fn opencodes_mode_option_is_its_approval_mode_not_a_setting() {
        let mut a = Acp::new(Path::new("/p"), &opencode("read-only"));
        open_opencode(&mut a);
        assert!(a
            .encode(&json!({ "op": "set_session_option", "id": "mode", "value": "build" }))
            .is_err());
        let out = up(
            &mut a,
            json!({ "sessionUpdate": "config_option_update", "configOptions": [
                { "id": "mode", "category": "mode", "type": "select", "currentValue": "aogiri", "options": [] },
                { "id": "x", "name": "Auto compact", "type": "boolean", "currentValue": true },
            ] }),
        );
        let options = &event(&out.events, "session_options")["options"];
        assert_eq!(options.as_array().unwrap().len(), 1, "{options}");
        assert_eq!(options[0]["id"], "x");
    }

    #[test]
    fn usage_updates_feed_context_usage_and_the_window() {
        let mut a = opened();
        let first = up(
            &mut a,
            json!({ "sessionUpdate": "usage_update", "used": 1200, "size": 200000, "cost": { "amount": 0.25, "currency": "USD" } }),
        );
        assert_eq!(
            types(&first.events),
            ["context_usage", "model_status", "engine_caps"]
        );
        assert_eq!(
            first.events[0],
            json!({ "type": "context_usage", "tokens": 1200, "cost": 0.25 })
        );
        assert_eq!(first.events[1]["context_window"], 200000);
        assert_eq!(first.events[2]["caps"]["contextUsage"], true);
        let next = up(
            &mut a,
            json!({ "sessionUpdate": "usage_update", "used": 1500, "size": 200000, "cost": { "amount": 9, "currency": "EUR" } }),
        );
        assert_eq!(
            next.events,
            [json!({ "type": "context_usage", "tokens": 1500 })]
        );
    }

    #[test]
    fn agent_commands_are_listed_and_run_as_prompts() {
        let mut a = opened();
        let out = up(
            &mut a,
            json!({ "sessionUpdate": "available_commands_update", "availableCommands": [
                { "name": "review", "description": "Review changes", "input": { "hint": "path" } }] }),
        );
        assert_eq!(
            out.events[0]["commands"],
            json!([{ "command": "/review", "marker": null, "args": "path", "description": "Review changes" }])
        );
        assert_eq!(out.events[1]["caps"]["slashCommands"], true);
        let run = a
            .encode(&json!({ "op": "command", "input": "/review src" }))
            .unwrap();
        assert_eq!(
            sent(&run.frames)[0]["params"]["prompt"][0]["text"],
            "/review src"
        );
        assert_eq!(event(&run.events, "user_message")["content"], "/review src");
        assert!(a
            .encode(&json!({ "op": "command", "input": "/compact" }))
            .is_err());
    }

    #[test]
    fn replies_outside_a_turn_are_dropped() {
        let mut a = opened();
        let banner = up(
            &mut a,
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "pi v1.2" } }),
        );
        assert!(banner.events.is_empty());
    }

    #[test]
    fn a_reopened_conversation_replays_tool_calls_and_its_plan() {
        let (mut a, frames) = started(
            Options {
                resume: Some("s1".into()),
                ..omp()
            },
            json!({ "loadSession": true, "sessionCapabilities": { "resume": {} } }),
        );
        assert_eq!(frames[0]["method"], "session/load");
        let mut live = Vec::new();
        for u in [
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "fix " } }),
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "it" } }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "on " } }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "it" } }),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.rs", "kind": "edit", "status": "pending" }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed", "content": [{ "type": "content", "content": { "type": "text", "text": "denied" } }] }),
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "fixed" } }),
            json!({ "sessionUpdate": "plan", "entries": [{ "content": "step", "status": "completed" }] }),
        ] {
            live.extend(up(&mut a, u).events);
        }
        assert!(live.is_empty(), "{live:#?}");
        let open = frame(
            &mut a,
            json!({ "id": 2, "result": { "configOptions": config() } }),
        );
        assert_eq!(types(&open.events)[..3], ["transcript", "plan", "startup"]);
        assert_eq!(
            open.events[0]["items"],
            json!([
                { "role": "user", "content": "fix it" },
                { "role": "assistant", "content": "on it" },
                { "role": "tool", "name": "Edit a.rs", "output": r#"{"content":"denied","kind":"edit"}"#, "is_error": true },
                { "role": "assistant", "content": "fixed" },
            ])
        );
    }

    #[test]
    fn an_agent_that_needs_sign_in_signs_in_once_then_says_how() {
        let (mut a, _) = started(omp(), json!({}));
        // Not signed in: the agent's own way, then the session again.
        let out = frame(
            &mut a,
            json!({ "id": 2, "error": { "code": -32000, "message": "Authentication required" } }),
        );
        let frames = sent(&out.frames);
        assert_eq!(
            (
                frames[0]["method"].as_str(),
                &frames[0]["params"]["methodId"]
            ),
            (Some("authenticate"), &json!("agent"))
        );
        assert!(text(&out.events[0]["message"]).contains("signing in to Oh My Pi"));
        let out = frame(&mut a, json!({ "id": 3, "result": {} }));
        assert_eq!(sent(&out.frames)[0]["method"], "session/new");
        // Still not: it says how, and does not try again.
        let out = frame(
            &mut a,
            json!({ "id": 4, "error": { "code": -32000, "message": "Authentication required" } }),
        );
        assert!(out.frames.is_empty());
        assert_eq!(
            out.events[0]["message"],
            "[acp] Oh My Pi needs sign-in: Run omp login"
        );
        // A reopened conversation is asked for again once signed in.
        let (mut b, _) = started(
            Options {
                resume: Some("s1".into()),
                ..omp()
            },
            json!({ "loadSession": true }),
        );
        let out = frame(
            &mut b,
            json!({ "id": 2, "error": { "code": -32000, "message": "Authentication required" } }),
        );
        assert_eq!(sent(&out.frames)[0]["method"], "authenticate");
        let out = frame(&mut b, json!({ "id": 3, "result": {} }));
        let load = &sent(&out.frames)[0];
        assert_eq!(load["method"], "session/load");
        assert_eq!(load["params"]["sessionId"], "s1");
    }

    #[test]
    fn a_probe_does_not_sign_in() {
        let mut a = Acp::new(
            Path::new("/p"),
            &Options {
                probe: true,
                ..omp()
            },
        );
        a.start();
        frame(
            &mut a,
            json!({ "id": 1, "result": { "protocolVersion": 1, "agentCapabilities": {}, "authMethods": [{ "id": "agent", "name": "Agent login" }] } }),
        );
        let out = frame(
            &mut a,
            json!({ "id": 2, "error": { "code": -32000, "message": "Authentication required" } }),
        );
        assert!(out.frames.is_empty());
    }

    #[test]
    fn mcp_servers_go_to_the_agent_http_ones_only_if_it_takes_them() {
        let servers = vec![
            json!({ "name": "files", "command": "mcp-files", "args": [], "env": [] }),
            json!({ "type": "http", "name": "web", "url": "https://x/mcp", "headers": [] }),
        ];
        for (http, count) in [(false, 1), (true, 2)] {
            let mut a = Acp::new(Path::new("/p"), &omp()).with_mcp(servers.clone());
            a.start();
            let out = frame(
                &mut a,
                json!({ "id": 1, "result": { "protocolVersion": 1, "agentCapabilities": { "mcpCapabilities": { "http": http } } } }),
            );
            let new = &sent(&out.frames)[0];
            assert_eq!(new["method"], "session/new");
            assert_eq!(new["params"]["mcpServers"].as_array().unwrap().len(), count);
        }
    }

    #[test]
    fn an_agents_models_without_a_config_option_are_picked_with_set_model() {
        let (mut a, _) = started(omp(), json!({}));
        let out = frame(
            &mut a,
            json!({ "id": 2, "result": { "sessionId": "s1", "models": { "currentModelId": "flash", "availableModels": [
                { "modelId": "flash", "name": "Gemini Flash" }, { "modelId": "pro", "name": "Gemini Pro" }] } } }),
        );
        assert_eq!(event(&out.events, "model_status")["model"], "flash");
        let view = a
            .encode(&json!({ "op": "command", "input": "/model" }))
            .unwrap();
        assert_eq!(view.events[0]["models"].as_array().unwrap().len(), 2);
        let set = a
            .encode(&json!({ "op": "command", "input": "/model pro" }))
            .unwrap();
        let frames = sent(&set.frames);
        assert_eq!(
            (
                frames[0]["method"].as_str(),
                &frames[0]["params"]["modelId"]
            ),
            (Some("session/set_model"), &json!("pro"))
        );
        let id = frames[0]["id"].as_u64().unwrap();
        let out = frame(&mut a, json!({ "id": id, "result": {} }));
        assert_eq!(out.events[0]["model"], "pro");
        assert_eq!(
            a.session_options()["options"].as_array().map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn a_form_elicitation_is_an_approval_card_answered_with_the_form() {
        let mut a = opened();
        let ask = |a: &mut Acp, id: u64| {
            frame(
                a,
                json!({ "id": id, "method": "elicitation/create", "params": { "sessionId": "s1", "mode": "form",
                    "message": "Allow tool: edit\nFile: b.txt",
                    "requestedSchema": { "type": "object", "properties": { "value": { "type": "string", "enum": ["Approve", "Deny"] } } } } }),
            )
            .events
            .remove(0)
        };
        let card = ask(&mut a, 40);
        assert_eq!(card["name"], "mcp_elicitation");
        assert_eq!(card["summary"], "Allow tool: edit\nFile: b.txt");
        assert_eq!(
            card["questions"][0]["question"],
            "Allow tool: edit\nFile: b.txt"
        );
        assert_eq!(card["questions"][0]["options"][0]["label"], "Approve");
        let accept = sent(
            &a.encode(
                &json!({ "op": "approve", "call_id": card["call_id"], "decision": "allow",
                "answers": { "Allow tool: edit\nFile: b.txt": "Approve" } }),
            )
            .unwrap()
            .frames,
        );
        assert_eq!(accept[0]["id"], 40);
        assert_eq!(
            accept[0]["result"],
            json!({ "action": "accept", "content": { "value": "Approve" } })
        );
        let card = ask(&mut a, 41);
        let decline = sent(
            &a.encode(&json!({ "op": "approve", "call_id": card["call_id"], "decision": "deny" }))
                .unwrap()
                .frames,
        );
        assert_eq!(decline[0]["result"], json!({ "action": "decline" }));
        // Other modes are not supported.
        let url = frame(
            &mut a,
            json!({ "id": 42, "method": "elicitation/create", "params": { "mode": "url", "url": "https://x" } }),
        );
        assert_eq!(sent(&url.frames)[0]["error"]["code"], -32601);
        // A withdrawn question is dropped.
        let card = ask(&mut a, 43);
        frame(
            &mut a,
            json!({ "method": "$/cancel_request", "params": { "requestId": 43 } }),
        );
        assert!(a
            .encode(&json!({ "op": "approve", "call_id": card["call_id"], "decision": "allow" }))
            .is_err());
    }

    #[test]
    fn interrupt_answers_open_questions_clears_the_queue_and_cancels() {
        let mut a = opened();
        a.encode(&json!({ "op": "user_message", "content": "one" }))
            .unwrap();
        let queued = a
            .encode(&json!({ "op": "user_message", "content": "two" }))
            .unwrap();
        assert_eq!(queued.events, [pending(&["two"])]);
        frame(
            &mut a,
            json!({ "id": 9, "method": "session/request_permission", "params": { "toolCall": { "title": "ls" }, "options": [] } }),
        );
        frame(
            &mut a,
            json!({ "id": 10, "method": "elicitation/create", "params": { "message": "ok?", "requestedSchema": { "properties": {} } } }),
        );
        let out = a.encode(&json!({ "op": "interrupt" })).unwrap();
        let frames = sent(&out.frames);
        let answer = |id: u64| frames.iter().find(|f| f["id"] == id).unwrap()["result"].clone();
        assert_eq!(answer(9), json!({ "outcome": { "outcome": "cancelled" } }));
        assert_eq!(answer(10), json!({ "action": "cancel" }));
        assert_eq!(frames.last().unwrap()["method"], "session/cancel");
        assert_eq!(out.events, [pending(&[])]);
        let prompt = a
            .pending
            .iter()
            .find(|(_, (m, _))| m == "session/prompt")
            .map(|(id, _)| *id)
            .unwrap();
        let done = frame(
            &mut a,
            json!({ "id": prompt, "result": { "stopReason": "cancelled" } }),
        );
        assert_eq!(
            done.events,
            [
                json!({ "type": "status", "message": "interrupted" }),
                json!({ "type": "status", "message": "ready" })
            ]
        );
        assert!(!a.busy());
    }

    #[test]
    fn a_turn_without_a_reply_says_so() {
        let mut a = opened();
        let prompt = sent(
            &a.encode(&json!({ "op": "user_message", "content": "hi" }))
                .unwrap()
                .frames,
        );
        let done = frame(
            &mut a,
            json!({ "id": prompt[0]["id"], "result": { "stopReason": "end_turn" } }),
        );
        assert_eq!(
            done.events[1]["message"],
            "[acp] the agent ended the turn without a reply"
        );
    }

    #[test]
    fn images_go_as_base64_blocks_when_the_agent_takes_them() {
        let dir = std::env::temp_dir().join(format!("lynshen-acp-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("a.png");
        std::fs::write(&image, [1u8, 2, 3]).unwrap();
        let image = image.display().to_string();
        let (mut a, _) = started(omp(), json!({ "promptCapabilities": { "image": true } }));
        frame(&mut a, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        let out = a
            .encode(&json!({ "op": "user_message", "content": "look", "images": [image, "/no/such.png"] }))
            .unwrap();
        let blocks = &sent(&out.frames)[0]["params"]["prompt"];
        assert_eq!(
            blocks[1],
            json!({ "type": "image", "mimeType": "image/png", "data": "AQID" })
        );
        assert_eq!(blocks.as_array().unwrap().len(), 2);
        assert_eq!(types(&out.events), ["info", "user_message"]);

        let (mut b, _) = started(omp(), json!({}));
        frame(&mut b, json!({ "id": 2, "result": { "sessionId": "s1" } }));
        let out = b
            .encode(&json!({ "op": "user_message", "content": "look", "images": [image] }))
            .unwrap();
        assert_eq!(
            sent(&out.frames)[0]["params"]["prompt"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            out.events[0]["message"],
            "[acp] this agent does not take images; 1 not sent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_approval_mode_restarts_omp_and_the_same_one_is_acknowledged() {
        let mut a = opened();
        assert_eq!(a.approval_mode().as_deref(), Some("read-only"));
        assert!(!a.mode_applies_live());
        let same = json!({ "op": "set_approval_mode", "mode": "manual" });
        assert!(a.restart_for(&same).is_none());
        assert_eq!(
            a.encode(&same).unwrap().events,
            [json!({ "type": "approval_mode", "mode": "read-only" })]
        );
        for mode in ["auto-edit", "full-access"] {
            let restart = a
                .restart_for(&json!({ "op": "set_approval_mode", "mode": mode }))
                .unwrap();
            assert_eq!(restart.approval_mode.as_deref(), Some(mode));
        }
        // Pi never asks: it has no modes to switch.
        let pi = Acp::new(
            Path::new("/p"),
            &Options {
                agent: Some("pi".into()),
                approval_mode: Some("full-access".into()),
                ..Options::default()
            },
        );
        assert!(pi
            .restart_for(&json!({ "op": "set_approval_mode", "mode": "read-only" }))
            .is_none());
        assert!(pi.has_gateway());
        // A built-in OpenCode is OpenCode: its modes are agents, switched live.
        let oc = Acp::new(
            Path::new("/p"),
            &Options {
                agent: Some("opencode".into()),
                approval_mode: Some("plan".into()),
                ..Options::default()
            },
        );
        assert!(oc
            .restart_for(&json!({ "op": "set_approval_mode", "mode": "full-access" }))
            .is_none());
        assert_eq!(oc.approval_mode().as_deref(), Some("plan"));
    }

    #[test]
    fn terminal_calls_are_bash_cards_with_their_command() {
        let mut a = opened();
        let start = up(
            &mut a,
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "shell", "kind": "execute", "status": "pending", "rawInput": { "cwd": "/p" }, "locations": [{ "path": "/p" }] }),
        );
        assert_eq!(start.events[0]["name"], "bash");
        up(
            &mut a,
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "in_progress", "kind": "execute", "title": "ls -a", "rawInput": { "command": "ls -a", "description": "List", "cwd": "/p" } }),
        );
        let done = up(
            &mut a,
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": "a.rs" } }] }),
        );
        assert_eq!(done.events[0]["name"], "bash");
        let output: Value = serde_json::from_str(text(&done.events[0]["output"])).unwrap();
        assert_eq!(output["command"], "ls -a");
        assert_eq!(output["stdout"], "a.rs");
    }

    #[test]
    fn notices_are_info_or_errors() {
        let mut a = opened();
        let out = up(
            &mut a,
            json!({ "sessionUpdate": "notice", "severity": "warning", "title": "Low credit", "description": "Top up" }),
        );
        assert_eq!(
            out.events,
            [json!({ "type": "info", "message": "[acp] Low credit\nTop up" })]
        );
        let out = up(
            &mut a,
            json!({ "sessionUpdate": "notice", "severity": "error", "title": "Down" }),
        );
        assert_eq!(out.events[0]["type"], "error");
    }

    #[test]
    fn preset_options_never_carry_a_command() {
        let both = Options {
            agent: Some("omp".into()),
            command: Some("/bin/sh".into()),
            ..Options::default()
        };
        assert_eq!(
            preset(&both).err().as_deref(),
            Some("options.agent and options.command are exclusive")
        );
        let unknown = Options {
            agent: Some("deepseek".into()),
            ..Options::default()
        };
        assert_eq!(
            preset(&unknown).err().as_deref(),
            Some("unknown ACP agent deepseek")
        );
        // A registered OpenCode has a TUI; another registered agent none.
        let tui = tui(&opencode("plan"), "ses_1").unwrap();
        let args: Vec<String> = tui
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            ["--session", "ses_1", "--agent", opencode::PLAN_AGENT]
        );
        let other = Options {
            command: Some("gemini".into()),
            ..Options::default()
        };
        assert!(super::tui(&other, "x").is_err());
    }
}

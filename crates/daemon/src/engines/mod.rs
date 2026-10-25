//! Sessions run by another agent CLI (Claude Code, Codex, ACP agents) as a
//! child process. An adapter translates the engine's own wire format into the
//! lynshen event dialect and client ops into engine frames, so every client
//! sees the same events it gets from a lynshen session. The daemon keeps a
//! snapshot of each session (state events, transcript, pending approvals) for
//! clients that start watching mid-session.

pub mod acp;
pub mod claude;
pub mod codex;
pub mod opencode;

use crate::hub::Hub;
use lynshen_agent_core::process_group::{self, ProcessGroup};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

/// How long a retired engine gets to exit on its own after its stdin closes.
const EXIT_GRACE: Duration = Duration::from_millis(1500);
const POLL: Duration = Duration::from_millis(20);
/// How long an engine may take to open its conversation.
const START_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
    Acp,
}

impl Kind {
    pub fn parse(name: &str) -> Result<Option<Self>, String> {
        match name {
            "" | "lynshen" => Ok(None),
            "claude" => Ok(Some(Kind::Claude)),
            "codex" => Ok(Some(Kind::Codex)),
            "acp" => Ok(Some(Kind::Acp)),
            other => Err(format!("unknown engine {other}")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
            Kind::Acp => "acp",
        }
    }
}

/// How an engine is started. `resume` names the engine's own conversation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// The client's approval mode (lynshen or desktop names).
    pub approval_mode: Option<String>,
    pub model: Option<String>,
    pub resume: Option<String>,
    /// Claude: resume the conversation as it was at this message (a rewind,
    /// or with `fork_from` the reply a fork goes on after).
    pub resume_at: Option<String>,
    /// A new session that forks this conversation (the engine's id: a
    /// LynShen session, a Claude Code conversation, a Codex thread) instead
    /// of starting empty; it goes on apart from it, which stays as it is.
    /// Only `session_create` reads it.
    pub fork_from: Option<String>,
    /// Codex / LynShen: how many of `fork_from`'s user turns the fork keeps
    /// (None: all). Claude Code forks at `resume_at` instead.
    pub fork_turns: Option<u64>,
    /// LynShen: the directory `fork_from` was saved for, when the fork works
    /// elsewhere (its own worktree). None: the fork's own `cwd`.
    pub fork_cwd: Option<PathBuf>,
    /// A run of `engine_probe`: nothing is saved and nothing waits on the
    /// user (set by the daemon, never by a client).
    pub probe: bool,
    /// ACP: the agent's command line.
    pub command: Option<String>,
    pub args: Vec<String>,
    /// Claude / Codex: the engine binary instead of the one on PATH.
    pub bin: Option<String>,
    /// Extra environment for the engine process.
    pub env: Vec<(String, String)>,
    /// Claude / Codex: talk to the LynShen gateway on the user's LynShen login
    /// instead of the provider in their own config (which stays untouched).
    /// None: as the session last ran.
    pub gateway: Option<bool>,
    /// Claude: start with ultracode on (standing Workflow orchestration).
    pub ultracode: bool,
    /// Claude: the thinking effort to start with.
    pub effort: Option<String>,
    /// Claude: start in fast mode.
    pub fast: bool,
    /// Claude: show thinking summaries (Some(false) hides them).
    pub thinking: Option<bool>,
    /// Claude / Codex: directories besides `cwd` the engine may work in (the
    /// session's project's; set by the hub, never by a client).
    pub dirs: Vec<PathBuf>,
    /// ACP: a conversation of the agent's to copy into this new session
    /// (`session/fork`); the original stays as it is.
    pub fork: Option<String>,
    /// ACP: the engine restarts under a conversation its clients show: it
    /// reopens it without a replay (`session/resume`) when the agent can.
    pub quiet: bool,
    /// ACP: the desktop's registry entry for the agent (`{id, name}`), kept
    /// with the session so a client can start it again.
    pub acp_agent: Option<Value>,
    /// Claude: the config directory the session's Claude Code keeps its
    /// state in, once found (`claude::config_with`, or its `initialize`
    /// reply; set by the daemon, never by a client).
    pub claude_config: Option<PathBuf>,
    /// Claude / Codex: the app's tools the engine is offered (the ones the
    /// user left on when it started; set by the daemon, never by a client).
    pub app_tools: Vec<&'static str>,
}

impl Options {
    pub fn from_json(value: &Value) -> Self {
        let text = |key: &str| {
            value[key]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Self {
            approval_mode: text("approval_mode"),
            model: text("model"),
            resume: None,
            resume_at: text("resume_at"),
            fork_from: text("fork_from"),
            fork_turns: value["fork_turns"].as_u64(),
            fork_cwd: text("fork_cwd").map(PathBuf::from),
            probe: false,
            command: text("command"),
            bin: text("bin"),
            args: value["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|arg| arg.as_str().map(str::to_string))
                .collect(),
            env: value["env"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
                .collect(),
            gateway: value["lynshen_gateway"].as_bool(),
            ultracode: value["ultracode"] == true,
            effort: text("effort"),
            fast: value["fast"] == true,
            thinking: value["thinking"].as_bool(),
            dirs: Vec::new(),
            fork: text("fork"),
            quiet: false,
            acp_agent: Some(value["acp_agent"].clone())
                .filter(|agent| agent["id"].is_string() && agent["name"].is_string()),
            claude_config: None,
            app_tools: Vec::new(),
        }
    }

    /// Whether these options name a program or environment to run, which
    /// only the desktop may do.
    pub fn runs_programs(&self) -> bool {
        self.command.is_some() || self.bin.is_some() || !self.env.is_empty()
    }

    /// Environment names an agent may be given: plain names, never ones that
    /// change how programs load.
    pub fn check_env(&self) -> Result<(), String> {
        for (name, _) in &self.env {
            let plain =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !plain || name.starts_with("DYLD_") || name.starts_with("LD_") {
                return Err(format!("environment variable {name} is not allowed"));
            }
        }
        Ok(())
    }
}

/// One line from the engine.
#[derive(Debug)]
pub enum Line {
    Frame(Value),
    Stderr(String),
}

/// What an adapter makes of a line or an op: events for clients and frames
/// for the engine's stdin.
#[derive(Debug, Default)]
pub struct Output {
    pub events: Vec<Value>,
    pub frames: Vec<String>,
}

impl Output {
    fn events(events: Vec<Value>) -> Self {
        Self {
            events,
            frames: Vec::new(),
        }
    }
}

/// The engine called one of the app's tools (`app_tools`); `token` is how
/// the adapter finds the request to answer.
#[derive(Debug)]
pub struct HostCall {
    pub token: Value,
    pub name: String,
    pub args: Value,
}

/// The app tools an engine of `kind` starting now is offered: none for an
/// ACP agent.
fn app_tools_for(kind: Kind) -> Vec<&'static str> {
    match kind {
        Kind::Acp => Vec::new(),
        Kind::Claude | Kind::Codex => crate::app_tools::enabled(&crate::app_tools::config()),
    }
}

/// A session tool's result on its way back from its thread: (which engine
/// process asked, token, result).
type Answer = (u64, Value, Result<Value, String>);

pub trait Adapter: Send {
    /// Frames to send once the engine started.
    fn start(&mut self) -> Vec<String>;
    fn translate(&mut self, line: Line) -> Output;
    /// Frames for a client op; Err when the engine cannot do it.
    fn encode(&mut self, op: &Value) -> Result<Output, String>;
    /// A turn is running.
    fn busy(&self) -> bool;
    /// Options for restarting the engine to apply `op`, when the op needs a
    /// new process (Claude's full-access mode is a start flag).
    fn restart_for(&self, op: &Value) -> Option<Options>;
    /// The engine's conversation id, for resuming it.
    fn conversation(&self) -> Option<String>;
    /// The session settings a restart keeps (Claude: effort, ultracode,
    /// fast mode, thinking display).
    fn keep(&self, options: Options) -> Options {
        options
    }
    /// The approval mode it runs in now (client names), kept across a restart.
    fn approval_mode(&self) -> Option<String> {
        None
    }
    /// The model it runs now (a switch mid-session included).
    fn model(&self) -> Option<String> {
        None
    }
    /// A mode switch reaches the running turn (else it applies to the next).
    fn mode_applies_live(&self) -> bool {
        true
    }
    /// The app-tool calls the engine made since last asked.
    fn take_calls(&mut self) -> Vec<HostCall> {
        Vec::new()
    }
    /// Frames answering a `HostCall` with the tool's result.
    fn answer(&mut self, _token: &Value, _result: Result<Value, String>) -> Vec<String> {
        Vec::new()
    }
}

fn adapter(kind: Kind, cwd: &Path, options: &Options) -> Box<dyn Adapter> {
    match kind {
        Kind::Claude => Box::new(claude::Claude::new(options).at(cwd)),
        Kind::Codex => Box::new(codex::Codex::new(cwd, options)),
        Kind::Acp => Box::new(acp::Acp::new(cwd, options)),
    }
}

/// An engine's start: its command, the local gateway key it holds when it
/// runs on the LynShen gateway (see crate::gateway), and notices for the
/// session's clients.
struct Start {
    command: Command,
    key: Option<String>,
    notices: Vec<String>,
}

/// How the engine starts in `cwd`.
fn command(kind: Kind, id: &str, options: &Options, cwd: &Path) -> Result<Start, String> {
    let command = match kind {
        Kind::Claude => claude::command(id, options),
        Kind::Codex => codex::command(options),
        Kind::Acp => acp::command(options),
    };
    with_gateway(kind, id, options, cwd, command)
}

/// The engine's own TUI resuming conversation `id` (Claude Code, Codex),
/// configured as its GUI process is: environment and gateway.
fn tui_command(
    kind: Kind,
    id: &str,
    options: &Options,
    cwd: &Path,
    saved: bool,
) -> Result<Start, String> {
    let command = match kind {
        Kind::Claude => claude::tui(id, options, saved),
        Kind::Codex => codex::tui(saved.then_some(id), options),
        Kind::Acp => return Err("an ACP agent has no TUI here".to_string()),
    };
    with_gateway(kind, id, options, cwd, command)
}

/// `command` with the session's environment, and on the LynShen gateway when
/// it runs there: with the local gateway key it holds. OpenCode also gets
/// the agent its approval modes run on (see `opencode`).
fn with_gateway(
    kind: Kind,
    id: &str,
    options: &Options,
    cwd: &Path,
    mut command: Command,
) -> Result<Start, String> {
    command.envs(options.env.iter().map(|(name, value)| (name, value)));
    let opencode = kind == Kind::Acp && opencode::is_opencode(options);
    if options.gateway != Some(true) {
        let notices = if opencode {
            opencode::configure(&mut command, options, cwd, None)?
        } else {
            Vec::new()
        };
        return Ok(Start {
            command,
            key: None,
            notices,
        });
    }
    if kind == Kind::Acp && !opencode {
        return Err("of the ACP agents only OpenCode has a LynShen gateway mode".to_string());
    }
    // Not signed in fails the start, not the first request.
    lynshen_agent_core::lynshen_gateway_credentials()?;
    let base = crate::gateway::base_url()?;
    let key = crate::gateway::issue(id)?;
    let configured = match kind {
        Kind::Claude => claude::use_gateway(&mut command, id, &base, &key).map(|()| Vec::new()),
        Kind::Codex => codex::use_gateway(&mut command, &base, &key).map(|()| Vec::new()),
        Kind::Acp => opencode::configure(&mut command, options, cwd, Some((&base, &key))),
    };
    match configured {
        Ok(notices) => Ok(Start {
            command,
            key: Some(key),
            notices,
        }),
        Err(error) => {
            crate::gateway::revoke(&key);
            Err(error)
        }
    }
}

/// Notices as `info` events.
fn notices(notices: Vec<String>) -> Vec<Value> {
    notices
        .into_iter()
        .map(|message| json!({ "type": "info", "message": message }))
        .collect()
}

/// One window of an official plan's usage for the `plan_usage` event: its
/// share used (0-100), when it resets (unix ms) and its length.
pub(crate) fn plan_window(
    key: &str,
    used_percent: f64,
    resets_secs: &Value,
    minutes: Option<u64>,
) -> Value {
    json!({
        "key": key,
        "used": (used_percent * 10.0).round() / 10.0,
        "resets_at": resets_secs.as_f64().map(|s| (s * 1000.0) as u64),
        "minutes": minutes,
    })
}

/// The events a probe keeps (the last of each kind): what a client shows of
/// an engine before any conversation of it.
const PROBE_KEEPS: &[&str] = &[
    "model_status",
    "model_view",
    "engine_caps",
    "command_list",
    "session_options",
];
/// A probe gives up after this long, and waits this long for the rest once
/// the engine has said something (and again once its models were asked for).
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const PROBE_SETTLE: Duration = Duration::from_secs(4);
/// A probe's answer is reused for this long (`force` asks again): models
/// and commands change with a release or a setting, not from one minute to
/// the next, and each probe starts a process.
const PROBE_FRESH: Duration = Duration::from_secs(10 * 60);

/// One probe answer per engine setup (`probe_key`): when it was made and
/// the events. Its own lock also makes a second probe of the same setup
/// wait for the first and take its answer.
type ProbeSlot = Arc<std::sync::Mutex<Option<(Instant, u64, Vec<Value>)>>>;
static PROBES: std::sync::Mutex<Option<HashMap<String, ProbeSlot>>> = std::sync::Mutex::new(None);

/// What makes two probes the same: the engine, where it runs and what runs.
fn probe_key(kind: Kind, cwd: &Path, options: &Options) -> String {
    json!([
        kind.name(),
        cwd,
        options.bin,
        options.env,
        options.gateway,
        options.command,
        options.args,
        options.model,
    ])
    .to_string()
}

/// `probe`, answered from the cache while it is fresh (`force`: run it
/// anyway): the events, when they were found (ms) and whether they came
/// from the cache.
pub fn probe_cached(
    kind: Kind,
    cwd: &Path,
    options: &Options,
    force: bool,
) -> Result<(Vec<Value>, u64, bool), String> {
    let slot = {
        let mut probes = PROBES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            probes
                .get_or_insert_with(HashMap::new)
                .entry(probe_key(kind, cwd, options))
                .or_default(),
        )
    };
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, ms, events)) = slot.as_ref() {
        if !force && at.elapsed() < PROBE_FRESH {
            return Ok((events.clone(), *ms, true));
        }
    }
    let events = probe(kind, cwd, options)?;
    let ms = now_ms();
    *slot = Some((Instant::now(), ms, events.clone()));
    Ok((events, ms, false))
}

/// What an engine says of itself before any conversation: started in `cwd`
/// as a session of it would be, but out of every list and saving nothing
/// (Claude Code `--no-session-persistence`, a Codex `ephemeral` thread),
/// asked for its models, then stopped. Returns the last event of each
/// PROBE_KEEPS kind it sent.
pub fn probe(kind: Kind, cwd: &Path, options: &Options) -> Result<Vec<Value>, String> {
    let options = Options {
        probe: true,
        resume: None,
        resume_at: None,
        fork: None,
        fork_from: None,
        fork_turns: None,
        fork_cwd: None,
        app_tools: Vec::new(),
        ..options.clone()
    };
    let id = new_uuid()?;
    let Start { command, key, .. } = command(kind, &id, &options, cwd)?;
    let release = |key: &Option<String>| {
        if let Some(key) = key {
            release_key(kind, key);
        }
    };
    let process = match Process::spawn(command, cwd) {
        Ok(process) => process,
        Err(error) => {
            release(&key);
            return Err(error);
        }
    };
    let mut engine = adapter(kind, cwd, &options);
    process.write(engine.start());
    let mut kept: HashMap<String, Value> = HashMap::new();
    let keep = |events: Vec<Value>, kept: &mut HashMap<String, Value>| {
        for event in events {
            if let Some(kind) = event["type"].as_str().filter(|t| PROBE_KEEPS.contains(t)) {
                kept.insert(kind.to_string(), event);
            }
        }
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut settle: Option<Instant> = None;
    let mut asked = false;
    let mut said = None;
    loop {
        let until = settle.map_or(deadline, |at| at.min(deadline));
        let Some(wait) = until.checked_duration_since(Instant::now()) else {
            break;
        };
        match process.lines.recv_timeout(wait) {
            Ok(Ok(line)) => {
                if let Line::Stderr(text) = &line {
                    said = Some(text.clone()).filter(|text| !text.trim().is_empty());
                }
                let out = engine.translate(line);
                process.write(out.frames);
                keep(out.events, &mut kept);
            }
            Ok(Err(_)) | Err(_) => break,
        }
        // Talking: it has a little longer to say the rest. Its conversation
        // open (it said which model it is on): its models, asked for.
        if settle.is_none() && !kept.is_empty() {
            settle = Some(Instant::now() + PROBE_SETTLE);
        }
        if !asked && kept.contains_key("model_status") {
            asked = true;
            settle = Some(Instant::now() + PROBE_SETTLE);
            if let Ok(out) = engine.encode(&json!({ "op": "command", "input": "/model" })) {
                process.write(out.frames);
                keep(out.events, &mut kept);
            }
        }
        if kept.contains_key("model_view") && kept.contains_key("command_list") {
            break;
        }
    }
    process.kill();
    release(&key);
    if kept.is_empty() {
        return Err(match said {
            Some(line) => format!("{} said nothing about itself: {line}", kind.name()),
            None => format!("{} said nothing about itself", kind.name()),
        });
    }
    // In a steady order, as a session sends them.
    Ok(PROBE_KEEPS
        .iter()
        .filter_map(|name| kept.remove(*name))
        .collect())
}

/// A local gateway key is done with: no request may use it again.
fn release_key(kind: Kind, key: &str) {
    crate::gateway::revoke(key);
    if kind == Kind::Claude {
        claude::forget_gateway(key);
    }
}

/// A running engine process: its stdin writer and merged output.
struct Process {
    child: Child,
    /// The engine and what it starts (MCP servers, shells): they end with
    /// it, and with the daemon. None where the system refused one.
    group: Option<ProcessGroup>,
    stdin: Option<Sender<String>>,
    lines: Receiver<Result<Line, String>>,
}

impl Process {
    fn spawn(mut command: Command, cwd: &Path) -> Result<Self, String> {
        let program = command.get_program().to_string_lossy().to_string();
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process_group::prepare(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start {program}: {error}"))?;
        let group = ProcessGroup::adopt(&child);
        let (lines_tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;
        let mut stdin_pipe = child.stdin.take().ok_or("no stdin")?;
        let (stdin, stdin_rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            for frame in stdin_rx {
                if writeln!(stdin_pipe, "{frame}")
                    .and_then(|()| stdin_pipe.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let errors = lines_tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if errors.send(Ok(Line::Stderr(line))).is_err() {
                    break;
                }
            }
        });
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // Anything that is not a JSON object is engine noise.
                if let Ok(frame @ Value::Object(_)) = serde_json::from_str::<Value>(&line) {
                    if lines_tx.send(Ok(Line::Frame(frame))).is_err() {
                        return;
                    }
                }
            }
            let _ = lines_tx.send(Err("the engine closed its output".to_string()));
        });
        Ok(Self {
            child,
            group,
            stdin: Some(stdin),
            lines,
        })
    }

    fn write(&self, frames: Vec<String>) {
        if let Some(stdin) = &self.stdin {
            for frame in frames {
                let _ = stdin.send(frame);
            }
        }
    }

    /// Closes stdin and kills the process if it has not exited after a grace
    /// period; what it started goes with it (`end_group`).
    fn stop(mut self) {
        self.stdin = None;
        thread::spawn(move || {
            let deadline = std::time::Instant::now() + EXIT_GRACE;
            while std::time::Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    self.end_group(false);
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            self.end_group(true);
            let _ = self.child.kill();
            let _ = self.child.wait();
        });
    }

    /// Ends the engine now, without the grace `stop` gives it.
    fn kill(mut self) {
        self.end_group(true);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Ends what the engine started and left running (MCP servers,
    /// shells). `now`: at once, else asking them to stop.
    fn end_group(&self, now: bool) {
        if let Some(group) = &self.group {
            group.end(now);
        }
    }

    fn exit_reason(&mut self) -> String {
        match self.child.wait() {
            Ok(status) => match status.code() {
                Some(code) => format!("exit code {code}"),
                None => "killed by a signal".to_string(),
            },
            Err(error) => error.to_string(),
        }
    }
}

/// How an engine closed as idle opens again: as it runs now (model and
/// approval mode switched mid-session, the settings `keep` carries), on its
/// conversation as a whole. The per-open fields stay behind: a rewind point
/// carried over would silently cut the conversation back, and the project's
/// folders and the conversation to resume are found again at the open.
fn asleep_options(adapter: &dyn Adapter, current: &Options) -> Options {
    Options {
        model: adapter.model().or(current.model.clone()),
        approval_mode: adapter.approval_mode().or(current.approval_mode.clone()),
        resume: None,
        resume_at: None,
        dirs: Vec::new(),
        fork: None,
        fork_from: None,
        fork_turns: None,
        fork_cwd: None,
        quiet: false,
        ..adapter.keep(current.clone())
    }
}

/// Whether a subagent or workflow in this state is working or about to.
fn running(state: &str) -> bool {
    matches!(
        state,
        "running" | "pending" | "queued" | "start" | "started" | "async_launched" | "in_progress"
    )
}

/// The state a client needs when it starts watching.
#[derive(Default)]
pub struct Snapshot {
    /// Latest event of each state type, by type.
    state: BTreeMap<&'static str, Value>,
    transcript: Vec<Value>,
    /// Open approval requests, by call id, oldest first.
    approvals: Vec<(String, Value)>,
    /// The transcript's last item is an assistant reply still streaming.
    in_reply: bool,
}

const STATE_EVENTS: &[&str] = &[
    "startup",
    "model_status",
    "command_list",
    "approval_mode",
    "approval_mode_pending",
    "mcp_servers",
    "background_tasks",
    "agent_runs",
    "plan",
    "rate_limit",
    "plan_usage",
    // Messages waiting for the turn: a client that attaches (another device,
    // a desktop back from elsewhere) shows them too.
    "pending_messages",
];

/// The non-image files a `user_message` op attaches (`files`, absolute
/// paths). The model is told where each is (`attachments`) and opens it
/// with its own tools.
pub(crate) fn op_files(op: &Value) -> Vec<String> {
    op["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// `files` on a user message event or item, when there are any.
pub(crate) fn set_files(value: &mut Value, files: &[String]) {
    if !files.is_empty() {
        value["files"] = json!(files);
    }
}

/// Now, ms since the epoch: when a snapshot item happened.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Snapshot {
    pub fn seed(&mut self, transcript: Vec<Value>) {
        self.transcript = transcript;
    }

    fn apply(&mut self, event: &Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        if let Some(name) = STATE_EVENTS.iter().find(|name| **name == kind) {
            self.state.insert(name, event.clone());
            return;
        }
        match kind {
            "transcript" => {
                self.in_reply = false;
                self.transcript = event["items"].as_array().cloned().unwrap_or_default();
            }
            "user_message" => {
                self.in_reply = false;
                let mut item =
                    json!({ "role": "user", "content": event["content"], "at": now_ms() });
                if event["images"].is_array() {
                    item["images"] = event["images"].clone();
                }
                if event["files"].is_array() {
                    item["files"] = event["files"].clone();
                }
                self.transcript.push(item);
            }
            "assistant_start" => {
                self.in_reply = true;
                self.transcript
                    .push(json!({ "role": "assistant", "content": "", "at": now_ms() }));
            }
            "assistant_delta" => {
                if !self.in_reply {
                    self.in_reply = true;
                    self.transcript
                        .push(json!({ "role": "assistant", "content": "", "at": now_ms() }));
                }
                // Appended in place: rebuilding the reply on every delta
                // copies it once per token.
                if let Some(last) = self.transcript.last_mut() {
                    let delta = event["delta"].as_str().unwrap_or_default();
                    match &mut last["content"] {
                        Value::String(text) => text.push_str(delta),
                        content => *content = json!(delta),
                    }
                }
            }
            "tool_start" => {
                self.in_reply = false;
                self.transcript.push(json!({
                    "role": "tool", "name": event["name"], "output": "", "call_id": event["call_id"], "at": now_ms(),
                }));
            }
            "tool_output" => {
                if let Some(item) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|item| item["call_id"] == event["call_id"])
                {
                    item["output"] = event["output"].clone();
                    if event["is_error"] == true {
                        item["is_error"] = json!(true);
                    }
                }
            }
            // A finished turn's figures, as a reopened transcript has them.
            "turn_stats" => {
                self.in_reply = false;
                let mut item = event.clone();
                if let Some(map) = item.as_object_mut() {
                    map.remove("type");
                    map.remove("session");
                    map.insert("role".to_string(), json!("turn_stats"));
                }
                self.transcript.push(item);
            }
            "approval_request" => {
                if let Some(call) = event["call_id"].as_str() {
                    self.approvals.push((call.to_string(), event.clone()));
                }
            }
            "status" if event["message"] == "ready" => {
                self.in_reply = false;
                self.approvals.clear();
            }
            _ => {}
        }
    }

    /// Work outside a turn keeps the engine open (`Hub::set_held`):
    /// background tasks, subagents or workflows running, approvals waiting.
    /// Claude Code's run list is rebuilt from its transcript (a run that
    /// never ended stays "running" there); its live background work is in
    /// `background_tasks`, so only other engines' runs count.
    fn holds(&self, kind: Kind) -> bool {
        let tasks = self
            .state
            .get("background_tasks")
            .and_then(|event| event["tasks"].as_array())
            .is_some_and(|tasks| !tasks.is_empty());
        let runs = kind != Kind::Claude
            && self.state.get("agent_runs").is_some_and(|event| {
                let workflows = event["workflows"].as_array().into_iter().flatten();
                let nested = workflows
                    .clone()
                    .flat_map(|workflow| workflow["agents"].as_array().into_iter().flatten());
                event["agents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(workflows)
                    .chain(nested)
                    .any(|run| {
                        // Runs say `status`; a workflow's agents say `state`.
                        let state = run["status"].as_str().or(run["state"].as_str());
                        running(state.unwrap_or_default())
                    })
            });
        tasks || runs || !self.approvals.is_empty()
    }

    fn answered(&mut self, call: &str) {
        self.approvals.retain(|(id, _)| id != call);
    }

    /// What a client that starts watching gets; `busy`: a turn runs now.
    fn events(&self, busy: bool) -> Vec<Value> {
        let mut events: Vec<Value> = STATE_EVENTS
            .iter()
            .filter_map(|name| self.state.get(name).cloned())
            .collect();
        // A `model_status` names the engine's state when it was sent. One
        // sent mid-turn (Codex sends one when its first usage update names
        // the context window) still says "streaming" once the turn has
        // ended, and a client would show that turn as running for good. The
        // state is the engine's now: a running turn adds `connecting` below.
        if !busy {
            for event in &mut events {
                if event["type"] == "model_status" && event.get("state").is_some() {
                    event["state"] = json!("ready");
                }
            }
        }
        let items: Vec<Value> = self
            .transcript
            .iter()
            .map(|item| {
                let mut item = item.clone();
                if let Some(map) = item.as_object_mut() {
                    map.remove("call_id");
                }
                item
            })
            .collect();
        events.push(json!({ "type": "transcript", "items": items }));
        if busy {
            events.push(json!({ "type": "connecting" }));
        }
        events.extend(self.approvals.iter().map(|(_, event)| event.clone()));
        events.push(json!({ "type": "attended", "attended": true }));
        events
    }
}

/// Starts a `kind` engine session in `cwd` on its own thread: a new one
/// named `id` when the daemon picks the id (Claude Code), or `options.resume`,
/// or a new one the engine names (Codex). Returns the session id once the
/// engine has opened its conversation, with the thread's ops channel and
/// generation, or the start error.
pub fn spawn(
    hub: Arc<Hub>,
    kind: Kind,
    id: Option<String>,
    cwd: PathBuf,
    options: Options,
    transcript: Vec<Value>,
) -> Result<(String, Sender<Value>, u64), String> {
    let gated = id
        .as_deref()
        .or(options.resume.as_deref())
        .is_some_and(|session| crate::requirements::gated(&hub, session));
    let options = Options {
        approval_mode: if gated {
            Some("read-only".to_string())
        } else {
            options.approval_mode
        },
        app_tools: app_tools_for(kind),
        ..options
    };
    let Start {
        command,
        key: gateway_key,
        notices: started,
    } = command(kind, id.as_deref().unwrap_or_default(), &options, &cwd)?;
    let process = match Process::spawn(command, &cwd) {
        Ok(process) => process,
        Err(error) => {
            if let Some(key) = &gateway_key {
                release_key(kind, key);
            }
            return Err(error);
        }
    };
    let (ops_tx, ops) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let generation = hub.next_generation();
    // An ACP agent reopening its conversation keeps it under the same id.
    let conversation = options.resume.clone().filter(|_| kind == Kind::Acp);
    let opencode = kind == Kind::Acp && opencode::is_opencode(&options);
    thread::spawn(move || {
        let mut session = Session {
            hub: &hub,
            id: None,
            ready: Some(ready_tx),
            early: Vec::new(),
            last_error: None,
            stderr: Default::default(),
            kind,
            cwd,
            snapshot: Snapshot::default(),
            restart: None,
            pending_mode: None,
            gateway_key,
            tui_request: None,
            conversation,
            opencode,
            app_approvals: HashMap::new(),
            app_approval_seq: 0,
            app_stops: Vec::new(),
            process_seq: 0,
            app_answers: None,
        };
        session.snapshot.seed(transcript);
        // A resumed conversation is ready once the engine has opened it.
        if let Some(id) = id {
            session.named(id);
        }
        session.publish(notices(started));
        session.run(process, options, ops);
        match (session.id.clone(), session.ready.take()) {
            (Some(id), _) => hub.session_ended(&id, generation),
            (None, Some(ready)) => {
                let reason = session.last_error.take().unwrap_or_else(|| {
                    session.stopped_message(&format!(
                        "{} stopped before opening a conversation",
                        kind.name()
                    ))
                });
                let _ = ready.send(Err(reason));
            }
            (None, None) => {}
        }
    });
    // A timeout drops the ops channel, which stops the engine.
    let id = ready_rx
        .recv_timeout(START_TIMEOUT)
        .map_err(|_| format!("{} did not open a conversation in time", kind.name()))??;
    Ok((id, ops_tx, generation))
}

struct Session<'a> {
    hub: &'a Hub,
    /// The session id: the engine's conversation id, known once it opened.
    id: Option<String>,
    ready: Option<Sender<Result<String, String>>>,
    /// Events from before the id was known.
    early: Vec<Value>,
    last_error: Option<String>,
    /// The engine's latest stderr lines. Stderr is diagnostics (codex echoes
    /// code it is working on there), so it stays out of the conversation and
    /// only explains an exit (`stopped_message`).
    stderr: std::collections::VecDeque<String>,
    kind: Kind,
    cwd: PathBuf,
    snapshot: Snapshot,
    /// Options to restart with once the running turn ends.
    restart: Option<Options>,
    /// A mode the user picked that waits for the running turn to end (the
    /// engine cannot switch it mid-turn); clients say so, and can interrupt.
    pending_mode: Option<String>,
    /// The engine's local gateway key (gateway sessions).
    gateway_key: Option<String>,
    /// A client asked for the conversation in the engine's own TUI.
    tui_request: Option<Value>,
    /// ACP: the agent's own session id, as recorded for the session.
    conversation: Option<String>,
    /// The ACP agent is OpenCode (approval modes, LynShen gateway).
    opencode: bool,
    /// Session-tool calls waiting for the user's approval, by the call id
    /// of the `approval_request` shown for them.
    app_approvals: HashMap<String, HostCall>,
    app_approval_seq: u64,
    /// Turns true for the session-tool calls running now when the turn is
    /// interrupted or the engine stops (`send_to_session` may wait).
    app_stops: Vec<Arc<AtomicBool>>,
    /// Counts engine processes: an answer for an earlier one is dropped.
    process_seq: u64,
    /// Where session tools' threads send their answers (set by `run`).
    app_answers: Option<Sender<Answer>>,
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.gateway_key.take() {
            release_key(self.kind, &key);
        }
        self.stop_app_calls();
    }
}

impl Session<'_> {
    const STDERR_KEPT: usize = 8;

    fn note_stderr(&mut self, line: &str) {
        let line = strip_ansi(line);
        let line = line.trim();
        if line.is_empty() || matches!(log_level(line), Some("INFO" | "DEBUG" | "TRACE")) {
            return;
        }
        if self.stderr.len() == Self::STDERR_KEPT {
            self.stderr.pop_front();
        }
        self.stderr.push_back(line.to_string());
    }

    /// `head`, then the engine's last stderr lines, which usually say why it
    /// stopped.
    fn stopped_message(&self, head: &str) -> String {
        if self.stderr.is_empty() {
            return head.to_string();
        }
        let tail: Vec<&str> = self.stderr.iter().map(String::as_str).collect();
        format!("{head}\n{}", tail.join("\n"))
    }

    fn run(&mut self, mut process: Process, options: Options, ops: Receiver<Value>) {
        let mut adapter = adapter(self.kind, &self.cwd, &options);
        process.write(adapter.start());
        // A new process runs no background task yet (the last one's went with
        // it); Claude Code reports none at startup. Nothing waits for it.
        self.publish(vec![
            json!({ "type": "background_tasks", "tasks": [] }),
            json!({ "type": "pending_messages", "messages": [] }),
        ]);
        // What the process runs as now (a restart changes it). A fork's
        // fields stay until it is saved: the engine only forks while it has
        // no conversation of its own (`claude::command`, `Codex::new`).
        let mut current = options.clone();
        // Session tools answer from their own threads (they may wait).
        let (answers_tx, answers) = mpsc::channel::<Answer>();
        self.app_answers = Some(answers_tx);
        loop {
            while let Ok((seq, token, result)) = answers.try_recv() {
                if seq == self.process_seq {
                    process.write(adapter.answer(&token, result));
                }
            }
            loop {
                match ops.try_recv() {
                    Ok(op) => {
                        if self.apply(&mut process, adapter.as_mut(), &op) {
                            // Closed as idle: the next open starts it as it
                            // runs now (model, effort, mode).
                            if op["keep"] == true {
                                if let Some(id) = &self.id {
                                    self.hub.keep_options(
                                        id,
                                        asleep_options(adapter.as_ref(), &current),
                                    );
                                }
                            }
                            process.stop();
                            return;
                        }
                        // The ops after a mode or gateway switch go to the
                        // new process (a message right after "full access"
                        // must not run in the old mode).
                        if self.restart.is_some() && !adapter.busy() {
                            break;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        process.stop();
                        return;
                    }
                }
            }
            let mut first = true;
            loop {
                let next = if first {
                    process.lines.recv_timeout(POLL)
                } else {
                    process.lines.try_recv().map_err(|error| match error {
                        mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                        mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                    })
                };
                first = false;
                match next {
                    Ok(Ok(line)) => {
                        if let Line::Stderr(text) = &line {
                            self.note_stderr(text);
                        }
                        let output = adapter.translate(line);
                        process.write(output.frames);
                        self.host_calls(&process, adapter.as_mut(), &current);
                        if self.id.is_none() {
                            if let Some(id) = adapter.conversation() {
                                self.named(id);
                            }
                        }
                        if self.kind == Kind::Acp {
                            self.note_conversation(adapter.as_ref());
                        }
                        let events =
                            self.allow_while_opening(&mut process, adapter.as_mut(), output.events);
                        // The conversation cannot open (a fork that cannot
                        // be made): the start fails now, with that error.
                        let failed = self.id.is_none() && events.iter().any(|e| e["fatal"] == true);
                        self.publish(events);
                        if failed {
                            process.stop();
                            return;
                        }
                    }
                    Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
                        let reason = process.exit_reason();
                        // What it started does not outlive it.
                        process.end_group(false);
                        let message = self
                            .stopped_message(&format!("{} stopped ({reason})", self.kind.name()));
                        self.publish(vec![json!({ "type": "error", "message": message })]);
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                }
            }
            let busy = adapter.busy();
            if let Some(id) = &self.id {
                self.hub.set_busy(id, busy);
                // A switch or a TUI waiting for the turn to end is work too.
                let held = self.snapshot.holds(self.kind)
                    || self.restart.is_some()
                    || self.tui_request.is_some();
                self.hub.set_held(id, held);
            }
            if !busy && self.pending_mode.take().is_some() {
                self.publish(vec![
                    json!({ "type": "approval_mode_pending", "mode": null }),
                ]);
            }
            if !busy {
                if let Some(mut next) = self.restart.take() {
                    next.resume = adapter.conversation().or(next.resume);
                    // A fork restarted before its first turn saved it (a
                    // mode switch): it is forked again, not started empty.
                    if next.resume.is_none() && options.fork_from.is_some() {
                        next.fork_from = options.fork_from.clone();
                        next.fork_turns = options.fork_turns;
                        next.resume_at = options.resume_at.clone();
                    }
                    // What the process runs as stays: its binary, environment
                    // and gateway.
                    next.bin = options.bin.clone();
                    next.env = options.env.clone();
                    next.claude_config = next.claude_config.or(options.claude_config.clone());
                    next.gateway = next.gateway.or(options.gateway);
                    next.approval_mode = next.approval_mode.or(options.approval_mode.clone());
                    next.dirs = crate::projects::extra_dirs(
                        self.hub,
                        &self.cwd,
                        &crate::projects::of_session(
                            self.hub,
                            self.id.as_deref().unwrap_or_default(),
                        ),
                    );
                    // An ACP agent: the same command, reopened without a replay.
                    next.command = options.command.clone();
                    next.args = options.args.clone();
                    next.quiet = true;
                    next.app_tools = app_tools_for(self.kind);
                    self.new_process();
                    current = next.clone();
                    process.stop();
                    let id = self.id.clone().unwrap_or_default();
                    if let Some(key) = self.gateway_key.take() {
                        release_key(self.kind, &key);
                    }
                    let mut said = Vec::new();
                    match command(self.kind, &id, &next, &self.cwd).and_then(|start| {
                        self.gateway_key = start.key;
                        said = start.notices;
                        Process::spawn(start.command, &self.cwd)
                    }) {
                        Ok(started) => {
                            process = started;
                            adapter = self::adapter(self.kind, &self.cwd, &next);
                            process.write(adapter.start());
                            // The new process runs nothing yet and nothing
                            // waits for it.
                            let mut events = notices(said);
                            events.push(json!({ "type": "background_tasks", "tasks": [] }));
                            events.push(json!({ "type": "pending_messages", "messages": [] }));
                            self.publish(events);
                        }
                        Err(error) => {
                            self.publish(vec![json!({ "type": "error", "message": error })]);
                            return;
                        }
                    }
                }
            }
            if let Some(request) = self.tui_request.take() {
                // `force`: the user agreed to cut the running turn short.
                if busy && request["force"] != true {
                    self.refuse(&request, "the running turn must end first".to_string());
                } else {
                    let keep = adapter.keep(current.clone());
                    if let Some(tui) = self.tui_for(adapter.as_ref(), &keep, &request) {
                        // The TUI resumes the conversation file at once: no
                        // grace period in which a cut-off turn still writes it.
                        process.kill();
                        // Its background tasks and the messages waiting
                        // for it went with the process.
                        let mut ended = vec![
                            json!({ "type": "background_tasks", "tasks": [] }),
                            json!({ "type": "pending_messages", "messages": [] }),
                        ];
                        if busy {
                            ended.push(json!({ "type": "status", "message": "interrupted" }));
                        }
                        self.publish(ended);
                        self.new_process();
                        match self.terminal(tui, keep, &request, &ops) {
                            Some((started, next, options)) => {
                                process = started;
                                adapter = next;
                                current = options;
                            }
                            None => return,
                        }
                    }
                }
            }
        }
    }

    /// Whether the engine run with `options` has saved conversation `id`
    /// (only then can its TUI resume it).
    fn saved(&self, id: &str, options: &Options) -> bool {
        match self.kind {
            Kind::Claude => !claude::transcript(
                &claude::config_with(options, &self.cwd, id),
                &self.cwd,
                id,
                self.hub.uploads.dir(),
            )
            .is_empty(),
            Kind::Codex => codex::saved(&self.cwd)
                .iter()
                .any(|(saved, ..)| saved == id),
            Kind::Acp => true,
        }
    }

    /// Answers a TUI request with an error, to the client that made it.
    fn refuse(&self, request: &Value, message: String) {
        let mut error = json!({ "type": "error", "message": message });
        if !request["id"].is_null() {
            error["id"] = request["id"].clone();
        }
        self.hub
            .send_to(request["client"].as_u64().unwrap_or(0), &error);
    }

    /// The TUI command for a request: the conversation and the engine's own
    /// TUI resuming it, with the local gateway key it holds. None (the
    /// client told why) leaves the engine running.
    fn tui_for(
        &self,
        adapter: &dyn Adapter,
        options: &Options,
        request: &Value,
    ) -> Option<(String, Command, Option<String>)> {
        let Some(id) = adapter.conversation().or(self.id.clone()) else {
            self.refuse(request, "the conversation has not started yet".to_string());
            return None;
        };
        // Claude Code writes a fork with its first turn; its TUI would start
        // the conversation empty.
        if self.kind == Kind::Claude && options.fork_from.is_some() && !self.saved(&id, options) {
            self.refuse(
                request,
                "send the fork its first message before opening it in its terminal".to_string(),
            );
            return None;
        }
        match tui_command(self.kind, &id, options, &self.cwd, self.saved(&id, options)) {
            Ok(start) => Some((id, start.command, start.key)),
            Err(error) => {
                self.refuse(request, error);
                None
            }
        }
    }

    /// The conversation in its engine's own TUI, on a terminal for the
    /// client that asked, its engine stopped (one process per conversation);
    /// the engine starts again, resuming, once the TUI exits, and clients see
    /// `surface` change and the conversation as the TUI left it. None when
    /// the session should stop.
    fn terminal(
        &mut self,
        (id, tui, tui_key): (String, Command, Option<String>),
        options: Options,
        request: &Value,
        ops: &Receiver<Value>,
    ) -> Option<(Process, Box<dyn Adapter>, Options)> {
        let client = request["client"].as_u64().unwrap_or(0);
        let hub = self.hub.handle()?;
        if let Some(key) = self.gateway_key.take() {
            release_key(self.kind, &key);
        }
        let session = self.id.clone().unwrap_or_default();
        // A new Codex conversation gets its id from the TUI: the one saved
        // for this directory while it ran.
        let earlier: Vec<String> = match self.kind {
            Kind::Codex => codex::saved(&self.cwd)
                .into_iter()
                .map(|(id, ..)| id)
                .collect(),
            _ => Vec::new(),
        };
        let (exit_hub, exit_session) = (Arc::clone(&hub), session.clone());
        let on_exit: Box<dyn FnOnce() + Send> = Box::new(move || {
            let _ = exit_hub.forward(&exit_session, json!({ "op": "tui_exit" }));
        });
        let opened = crate::terminal::open_command(
            &hub,
            client,
            request,
            crate::terminal::tui(&tui, &self.cwd),
            Some(on_exit),
        );
        match opened {
            Ok(term) => {
                self.hub.set_busy(&session, false);
                // The conversation is in the TUI until it exits: never idle.
                self.hub.set_held(&session, true);
                self.publish(vec![
                    json!({ "type": "surface", "surface": "tui", "term": term, "client": client }),
                ]);
                loop {
                    let Ok(op) = ops.recv() else {
                        crate::terminal::kill(self.hub, &term);
                        return None;
                    };
                    match op["op"].as_str().unwrap_or_default() {
                        "tui_exit" => break,
                        "shutdown" => {
                            crate::terminal::kill(self.hub, &term);
                            if let Some(key) = &tui_key {
                                release_key(self.kind, key);
                            }
                            return None;
                        }
                        "snapshot" => {
                            if let Some(watcher) = op["client"].as_u64() {
                                let mut events = self.snapshot.events(false);
                                events.push(json!({ "type": "surface", "surface": "tui", "term": term, "client": client }));
                                for event in events {
                                    self.hub.send_to(watcher, &self.tagged(event));
                                }
                            }
                        }
                        "set_attended" => {}
                        // Another client asks for it: it attaches to the
                        // TUI that runs.
                        "tui" => {
                            let viewer = op["client"].as_u64().unwrap_or(0);
                            if let Err(error) =
                                crate::terminal::attach_to(&hub, viewer, &term, &op)
                            {
                                self.refuse(&op, error);
                            }
                        }
                        _ => self.publish(vec![json!({ "type": "error", "message": "the conversation is open in its terminal: exit the TUI to continue here" })]),
                    }
                }
            }
            Err(error) => self.refuse(request, error),
        }
        if let Some(key) = tui_key {
            release_key(self.kind, &key);
        }
        // Back to the engine, on the conversation as the TUI left it (none
        // when the TUI saved nothing: the engine starts it afresh).
        let id = match self.kind {
            Kind::Codex if !self.saved(&id, &options) => codex::saved(&self.cwd)
                .into_iter()
                .map(|(id, ..)| id)
                .find(|saved| !earlier.contains(saved))
                .unwrap_or(id),
            _ => id,
        };
        let next = Options {
            resume: self.saved(&id, &options).then(|| id.clone()),
            resume_at: None,
            ..options
        };
        let started = self::command(self.kind, &id, &next, &self.cwd).and_then(|start| {
            self.gateway_key = start.key;
            Process::spawn(start.command, &self.cwd)
        });
        match started {
            Ok(started) => {
                let mut engine = self::adapter(self.kind, &self.cwd, &next);
                started.write(engine.start());
                let mut events = vec![json!({ "type": "surface", "surface": "gui" })];
                // Codex replays its thread when it resumes; Claude Code's
                // conversation is read from its file.
                if self.kind == Kind::Claude {
                    let config = claude::config_with(&next, &self.cwd, &id);
                    events.push(json!({ "type": "transcript", "items": claude::transcript(&config, &self.cwd, &id, self.hub.uploads.dir()) }));
                }
                self.publish(events);
                Some((started, engine, next))
            }
            Err(error) => {
                self.publish(vec![json!({ "type": "error", "message": error })]);
                None
            }
        }
    }

    /// Applies one op; returns true when the session should stop.
    fn apply(&mut self, process: &mut Process, adapter: &mut dyn Adapter, op: &Value) -> bool {
        if op["claimed"] == true {
            if let Some(id) = &self.id {
                self.hub.release_claim(id);
            }
        }
        match op["op"].as_str().unwrap_or_default() {
            "tui" => {
                self.tui_request = Some(op.clone());
                false
            }
            "snapshot" => {
                if let Some(client) = op["client"].as_u64() {
                    for event in self.snapshot.events(adapter.busy()) {
                        let event = self.tagged(event);
                        self.hub.send_to(client, &event);
                    }
                }
                false
            }
            "shutdown" => true,
            // A client watching or not changes nothing: approvals wait for
            // whoever answers them next.
            "set_attended" => false,
            // Claude Code / Codex move between this machine's own login and
            // the LynShen gateway (a new process; the conversation resumes),
            // once the running turn ends.
            "set_gateway" => {
                let Some(gateway) = op["gateway"].as_bool() else {
                    self.publish(vec![
                        json!({ "type": "error", "message": "set_gateway requires gateway" }),
                    ]);
                    return false;
                };
                if self.kind == Kind::Acp && !self.opencode {
                    self.publish(vec![json!({ "type": "error", "message": "of the ACP agents only OpenCode has a LynShen gateway mode" })]);
                    return false;
                }
                self.restart = Some(
                    adapter.keep(Options {
                        gateway: Some(gateway),
                        model: op["model"]
                            .as_str()
                            .filter(|m| !m.is_empty())
                            .map(str::to_string),
                        approval_mode: adapter.approval_mode(),
                        ..Options::default()
                    }),
                );
                if let Some(id) = &self.id {
                    let _ = self
                        .hub
                        .store
                        .record_session_meta(id, &json!({ "gateway": gateway }));
                    self.hub.broadcast(&self.hub.sessions_json());
                }
                if adapter.busy() {
                    self.publish(vec![json!({
                        "type": "info",
                        "message": "the switch applies once the running turn ends",
                    })]);
                }
                false
            }
            name => {
                if let Some(options) = adapter.restart_for(op) {
                    self.restart = Some(options);
                    if adapter.busy() {
                        self.wait_for_turn(process, adapter, op);
                    }
                    return false;
                }
                if name == "set_approval_mode" && adapter.busy() && !adapter.mode_applies_live() {
                    self.wait_for_turn(process, adapter, op);
                }
                if name == "approve" {
                    if let Some(call) = op["call_id"].as_str() {
                        self.snapshot.answered(call);
                        if self.app_approvals.contains_key(call) {
                            let allow = op["decision"] != "deny";
                            self.decide_app_call(process, adapter, call, allow);
                            return false;
                        }
                    }
                }
                if name == "interrupt" {
                    self.stop_app_calls();
                }
                if let Some(id) = &self.id {
                    self.hub.note_sent_images(id, op);
                }
                match adapter.encode(op) {
                    Ok(output) => {
                        process.write(output.frames);
                        self.publish(output.events);
                    }
                    Err(message) => {
                        self.publish(vec![json!({ "type": "error", "message": message })])
                    }
                }
                false
            }
        }
    }

    /// Runs the app-tool calls the engine made: a turned-off one is refused,
    /// `present_files` and `propose_schedule` run here, a session tool runs
    /// on its own thread (answered through `app_answers`), after the user's
    /// approval where a LynShen session's would ask (`engine_gate`).
    fn host_calls(&mut self, process: &Process, adapter: &mut dyn Adapter, current: &Options) {
        for call in adapter.take_calls() {
            let name = call.name.as_str();
            // A resumed Codex thread keeps the tools it started with.
            if !current.app_tools.contains(&name) || !crate::app_tools::is_on(name) {
                let refused = Err(crate::app_tools::turned_off(name));
                process.write(adapter.answer(&call.token, refused));
                continue;
            }
            if !lynshen_agent_core::app_tools::SESSION_TOOLS.contains(&name) {
                let result = crate::app_tools::run(name, &call.args, &self.cwd);
                process.write(adapter.answer(&call.token, result));
                continue;
            }
            let Some(session) = self.id.clone() else {
                let early = Err("the session is not open yet".to_string());
                process.write(adapter.answer(&call.token, early));
                continue;
            };
            match crate::session_messages::engine_gate(self.hub, &session, name) {
                None => self.run_session_tool(call),
                Some(must_ask) => {
                    self.app_approval_seq += 1;
                    let id = format!("app-approval-{}", self.app_approval_seq);
                    let summary = crate::session_messages::summary(self.hub, name, &call.args);
                    let event = json!({
                        "type": "approval_request",
                        "call_id": id,
                        "name": name,
                        "summary": summary,
                        "subagent_id": null,
                        "hunks": null,
                        "kind": "tool",
                        "must_ask": must_ask,
                        "scopes": ["once"],
                    });
                    self.app_approvals.insert(id, call);
                    self.publish(vec![event]);
                }
            }
        }
    }

    /// Runs a session tool on its own thread; its answer comes back to
    /// `run`'s loop (`app_answers`).
    fn run_session_tool(&mut self, call: HostCall) {
        let (Some(hub), Some(session), Some(answers)) =
            (self.hub.handle(), self.id.clone(), self.app_answers.clone())
        else {
            return;
        };
        let stopped = Arc::new(AtomicBool::new(false));
        self.app_stops.retain(|stop| !stop.load(Ordering::SeqCst));
        self.app_stops.push(Arc::clone(&stopped));
        let seq = self.process_seq;
        thread::spawn(move || {
            let result =
                crate::session_messages::call(&hub, &session, &call.name, &call.args, &stopped);
            stopped.store(true, Ordering::SeqCst);
            let _ = answers.send((seq, call.token, result));
        });
    }

    /// The user decided a session-tool call that waited for approval.
    fn decide_app_call(
        &mut self,
        process: &mut Process,
        adapter: &mut dyn Adapter,
        call: &str,
        allow: bool,
    ) {
        let Some(host) = self.app_approvals.remove(call) else {
            return;
        };
        if allow {
            self.run_session_tool(host);
            return;
        }
        let denied = Err("The user denied this tool use.".to_string());
        process.write(adapter.answer(&host.token, denied));
    }

    /// Session-tool calls waiting for a reply give up (the turn was
    /// interrupted, or the engine stopped).
    fn stop_app_calls(&mut self) {
        for stop in self.app_stops.drain(..) {
            stop.store(true, Ordering::SeqCst);
        }
    }

    /// A new engine process: calls of the last one are answered no more.
    fn new_process(&mut self) {
        self.process_seq += 1;
        self.stop_app_calls();
        self.app_approvals.clear();
    }

    fn tagged(&self, mut event: Value) -> Value {
        event["session"] = json!(self.id);
        event
    }

    /// An ACP agent opened a conversation of its own (its session id): it is
    /// kept with the session, so reopening the session asks the agent for
    /// the same conversation.
    fn note_conversation(&mut self, adapter: &dyn Adapter) {
        let (Some(id), Some(conversation)) = (self.id.clone(), adapter.conversation()) else {
            return;
        };
        if self.conversation.as_deref() == Some(conversation.as_str()) {
            return;
        }
        let _ = self
            .hub
            .store
            .record_session_meta(&id, &json!({ "conversation": conversation }));
        self.conversation = Some(conversation);
    }

    /// The conversation is open under `id`: the session starts answering to
    /// it, and whatever it said before goes out.
    fn named(&mut self, id: String) {
        self.id = Some(id.clone());
        if let Some(key) = &self.gateway_key {
            crate::gateway::bind(key, &id);
        }
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Ok(id));
        }
        let early = std::mem::take(&mut self.early);
        for event in early {
            self.hub.broadcast(&self.tagged(event));
        }
    }

    /// `op` (a mode switch) cannot reach the running turn: clients are told
    /// it waits for the turn to end. A switch to full access answers the
    /// turn's open approvals at once (and later ones, see
    /// `allow_while_opening`), since nothing would ask under it.
    fn wait_for_turn(&mut self, process: &mut Process, adapter: &mut dyn Adapter, op: &Value) {
        let mode = op["mode"].as_str().unwrap_or_default().to_string();
        self.pending_mode = Some(mode.clone());
        self.publish(vec![
            json!({ "type": "approval_mode_pending", "mode": mode }),
        ]);
        if full_access(&mode) {
            let open: Vec<String> = self
                .snapshot
                .approvals
                .iter()
                .filter(|(_, request)| request["must_ask"] != true)
                .map(|(call, _)| call.clone())
                .collect();
            for call in open {
                self.allow(process, adapter, &call);
            }
        }
    }

    /// While a switch to full access waits for the turn, the turn's approval
    /// requests are allowed rather than shown.
    fn allow_while_opening(
        &mut self,
        process: &mut Process,
        adapter: &mut dyn Adapter,
        events: Vec<Value>,
    ) -> Vec<Value> {
        if !self.pending_mode.as_deref().is_some_and(full_access) {
            return events;
        }
        let mut kept = Vec::with_capacity(events.len());
        for event in events {
            match event["call_id"].as_str() {
                // One that asks in every mode (`sessions.messages` ask) still asks.
                Some(call) if event["type"] == "approval_request" && event["must_ask"] != true => {
                    let call = call.to_string();
                    self.allow(process, adapter, &call);
                }
                _ => kept.push(event),
            }
        }
        kept
    }

    fn allow(&mut self, process: &mut Process, adapter: &mut dyn Adapter, call: &str) {
        if self.app_approvals.contains_key(call) {
            self.snapshot.answered(call);
            self.decide_app_call(process, adapter, call, true);
            return;
        }
        let op = json!({ "op": "approve", "call_id": call, "decision": "allow" });
        match adapter.encode(&op) {
            Ok(output) => {
                self.snapshot.answered(call);
                process.write(output.frames);
                self.publish(output.events);
            }
            Err(message) => self.publish(vec![json!({ "type": "error", "message": message })]),
        }
    }

    fn publish(&mut self, events: Vec<Value>) {
        for mut event in events {
            if let Some(id) = &self.id {
                self.hub.attach_sent_images(id, &mut event);
            }
            if event["type"] == "error" {
                self.last_error = event["message"].as_str().map(str::to_string);
            }
            self.snapshot.apply(&event);
            if let Some(id) = &self.id {
                self.hub.observe(id, &event);
                self.hub.usage.tag_event(id, &mut event);
            }
            if self.id.is_some() {
                self.hub.broadcast(&self.tagged(event));
            } else {
                self.early.push(event);
            }
        }
    }
}

pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The level of a `2026-…T…Z LEVEL …` tracing line an engine logs.
pub fn log_level(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    let (stamp, level) = (words.next()?, words.next()?);
    let stamped = stamp.len() > 10 && stamp.as_bytes()[4] == b'-' && stamp.contains('T');
    (stamped && matches!(level, "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE")).then_some(level)
}

/// The engine binary: `env_override`, else `find_program`.
pub fn resolve(name: &str, env_override: &str, extra: &[PathBuf]) -> PathBuf {
    match std::env::var_os(env_override).filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => find_program(name, extra),
    }
}

/// `name` on PATH, then in the usual install directories and `extra`, else
/// the bare name. On Windows a bare name resolves through PATHEXT, as the
/// shell does: npm installs Codex and Claude Code as `codex.cmd` /
/// `claude.cmd` beside an extensionless shell script that cannot be started.
pub fn find_program(name: &str, extra: &[PathBuf]) -> PathBuf {
    let names = program_names(name);
    let path_dirs = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default();
    let home = home();
    let mut known = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".cargo").join("bin"),
        home.join(".local").join("bin"),
    ];
    // npm's global bin on Windows, often missing from an app's PATH.
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|_| cfg!(windows)) {
        known.push(PathBuf::from(appdata).join("npm"));
    }
    let extra = extra.iter().flat_map(|path| {
        let base = path.clone();
        names_for(&base)
    });
    path_dirs
        .iter()
        .chain(known.iter())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .chain(extra)
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(&names[0]))
}

/// The file names `name` may have here: itself on Unix; on Windows itself
/// when it has an extension, else `name` + each PATHEXT extension.
fn program_names(name: &str) -> Vec<String> {
    if !cfg!(windows) || Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    windows_extensions()
        .iter()
        .map(|ext| format!("{name}{ext}"))
        .collect()
}

/// `program_names` for a full path (an install location to look in).
fn names_for(path: &Path) -> Vec<PathBuf> {
    let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
        return vec![path.to_path_buf()];
    };
    program_names(file)
        .into_iter()
        .map(|name| path.with_file_name(name))
        .collect()
}

/// PATHEXT's executable extensions, lower-cased, `.exe` first.
fn windows_extensions() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_default();
    let mut exts: Vec<String> = raw
        .split(';')
        .map(|ext| ext.trim().to_ascii_lowercase())
        .filter(|ext| matches!(ext.as_str(), ".exe" | ".cmd" | ".bat" | ".com"))
        .collect();
    if exts.is_empty() {
        exts = vec![".exe".into(), ".cmd".into(), ".bat".into()];
    }
    exts.sort_by_key(|ext| ext != ".exe");
    exts.dedup();
    exts
}

/// The user's home where Claude Code and Codex keep their sessions. On
/// Windows that is USERPROFILE: a `HOME` from Git Bash or MSYS would point
/// the lookups at another directory.
pub fn home() -> PathBuf {
    let (first, second) = if cfg!(windows) {
        ("USERPROFILE", "HOME")
    } else {
        ("HOME", "USERPROFILE")
    };
    std::env::var_os(first)
        .or_else(|| std::env::var_os(second))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// A random UUID v4, for engines that take the conversation id from us.
pub fn new_uuid() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// Full access, in any of the names clients and engines use.
fn full_access(mode: &str) -> bool {
    matches!(mode, "full-access" | "full-auto" | "bypassPermissions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_closed_engine_reopens_on_its_whole_conversation() {
        let current = Options {
            model: Some("started-with".into()),
            resume: Some("conv".into()),
            resume_at: Some("rewind-point".into()),
            bin: Some("/opt/claude".into()),
            dirs: vec![PathBuf::from("/p/other")],
            ..Options::default()
        };
        let engine = adapter(Kind::Claude, Path::new("/tmp"), &current);
        let kept = asleep_options(engine.as_ref(), &current);
        assert_eq!(
            kept.resume_at, None,
            "a rewind applies to the open that asked"
        );
        assert_eq!(kept.resume, None);
        assert!(
            kept.dirs.is_empty(),
            "the project's folders are found again"
        );
        assert_eq!(kept.bin.as_deref(), Some("/opt/claude"));
        assert_eq!(kept.model.as_deref(), Some("started-with"));
        assert!(kept.approval_mode.is_some());
    }

    #[test]
    fn background_work_and_waiting_approvals_hold_the_engine() {
        let mut snapshot = Snapshot::default();
        assert!(!snapshot.holds(Kind::Codex));
        snapshot.apply(&json!({ "type": "background_tasks", "tasks": [{ "id": "t1" }] }));
        assert!(snapshot.holds(Kind::Claude));
        snapshot.apply(&json!({ "type": "background_tasks", "tasks": [] }));
        assert!(!snapshot.holds(Kind::Claude));
        // As Codex reports a subagent still working after its turn.
        snapshot.apply(&json!({ "type": "agent_runs", "workflows": [], "agents": [{ "id": "a", "status": "running" }, { "id": "b", "status": "completed" }] }));
        assert!(snapshot.holds(Kind::Codex));
        // Claude's run list comes from its transcript: not trusted to hold.
        assert!(!snapshot.holds(Kind::Claude));
        snapshot.apply(&json!({ "type": "agent_runs", "workflows": [{ "status": "completed", "agents": [{ "state": "async_launched" }] }], "agents": [] }));
        assert!(
            snapshot.holds(Kind::Codex),
            "a workflow's agent still running"
        );
        snapshot.apply(&json!({ "type": "agent_runs", "workflows": [{ "status": "completed", "agents": [{ "state": "done" }] }], "agents": [{ "status": "failed" }, { "status": "" }] }));
        assert!(!snapshot.holds(Kind::Codex));
        snapshot.apply(&json!({ "type": "approval_request", "call_id": "c1" }));
        assert!(snapshot.holds(Kind::Codex));
        snapshot.apply(&json!({ "type": "status", "message": "ready" }));
        assert!(!snapshot.holds(Kind::Codex));
    }

    #[test]
    fn the_snapshot_rebuilds_the_conversation_and_open_approvals() {
        let mut snapshot = Snapshot::default();
        for event in [
            json!({ "type": "startup", "model": "a" }),
            json!({ "type": "startup", "model": "b" }),
            json!({ "type": "user_message", "content": "hi" }),
            json!({ "type": "assistant_start" }),
            json!({ "type": "assistant_delta", "delta": "hel" }),
            json!({ "type": "assistant_delta", "delta": "lo" }),
            json!({ "type": "tool_start", "call_id": "t1", "name": "bash" }),
            json!({ "type": "tool_output", "call_id": "t1", "name": "bash", "output": "ok", "is_error": false }),
            json!({ "type": "tool_start", "call_id": "t2", "name": "read" }),
            json!({ "type": "tool_output", "call_id": "t2", "name": "read", "output": "no", "is_error": true }),
            json!({ "type": "approval_request", "call_id": "a1", "name": "bash" }),
            json!({ "type": "approval_request", "call_id": "a2", "name": "write" }),
        ] {
            snapshot.apply(&event);
        }
        snapshot.answered("a1");
        let events = snapshot.events(true);
        assert_eq!(events[0], json!({ "type": "startup", "model": "b" }));
        let transcript = events.iter().find(|e| e["type"] == "transcript").unwrap();
        // Each item says when it happened.
        let items: Vec<Value> = transcript["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                assert!(item["at"].as_u64().is_some_and(|at| at > 0), "{item}");
                let mut item = item.clone();
                item.as_object_mut().unwrap().remove("at");
                item
            })
            .collect();
        assert_eq!(
            json!(items),
            json!([
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "hello" },
                { "role": "tool", "name": "bash", "output": "ok" },
                { "role": "tool", "name": "read", "output": "no", "is_error": true },
            ])
        );
        let open: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "approval_request")
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["call_id"], "a2");
        assert!(events.iter().any(|e| e["type"] == "connecting"));

        snapshot.apply(&json!({ "type": "status", "message": "ready" }));
        assert!(!snapshot
            .events(false)
            .iter()
            .any(|e| e["type"] == "approval_request"));

        // A finished turn's figures join the transcript as a reopened one has them.
        snapshot.apply(
            &json!({ "type": "turn_stats", "session": "s1", "model": "m", "elapsed_ms": 1200 }),
        );
        let events = snapshot.events(false);
        let transcript = events.iter().find(|e| e["type"] == "transcript").unwrap();
        assert_eq!(
            transcript["items"].as_array().unwrap().last().unwrap(),
            &json!({ "role": "turn_stats", "model": "m", "elapsed_ms": 1200 })
        );
    }

    #[test]
    fn the_snapshot_names_the_engines_state_now() {
        let mut snapshot = Snapshot::default();
        // The latest model_status was sent mid-turn; the turn has ended since.
        for event in [
            json!({ "type": "model_status", "model": "gpt-5", "state": "ready" }),
            json!({ "type": "user_message", "content": "hi" }),
            json!({ "type": "connecting" }),
            json!({ "type": "model_status", "model": "gpt-5", "context_window": 272000, "state": "streaming" }),
            json!({ "type": "assistant_delta", "delta": "done" }),
            json!({ "type": "status", "message": "ready" }),
        ] {
            snapshot.apply(&event);
        }
        let idle = snapshot.events(false);
        let status = idle.iter().find(|e| e["type"] == "model_status").unwrap();
        assert_eq!(status["state"], "ready");
        assert_eq!(status["context_window"], 272000);
        assert!(!idle.iter().any(|e| e["type"] == "connecting"));
        // A turn that runs still shows: its `connecting` follows the status.
        let running = snapshot.events(true);
        let at = |kind: &str| running.iter().position(|e| e["type"] == kind).unwrap();
        assert!(at("connecting") > at("model_status"));
        // An engine whose status names no state gets none.
        let mut plain = Snapshot::default();
        plain.apply(&json!({ "type": "model_status", "model": "m" }));
        assert!(plain.events(false)[0].get("state").is_none());
    }

    #[test]
    fn a_client_that_attaches_sees_the_messages_waiting_for_the_turn() {
        let mut snapshot = Snapshot::default();
        snapshot.apply(&json!({ "type": "pending_messages", "messages": ["one"] }));
        snapshot.apply(&json!({ "type": "pending_messages", "messages": ["one", "two"] }));
        let events = snapshot.events(true);
        let queued: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "pending_messages")
            .collect();
        assert_eq!(
            queued,
            [&json!({ "type": "pending_messages", "messages": ["one", "two"] })]
        );
    }

    #[test]
    fn program_names_follow_pathext_on_windows_only() {
        let names = program_names("codex");
        if cfg!(windows) {
            assert_eq!(names[0], "codex.exe");
            assert!(names.iter().any(|name| name == "codex.cmd"));
            assert_eq!(program_names("tool.cmd"), ["tool.cmd"]);
        } else {
            assert_eq!(names, ["codex"]);
        }
    }

    #[test]
    fn uuids_are_version_4() {
        let id = new_uuid().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(new_uuid().unwrap(), id);
    }
}

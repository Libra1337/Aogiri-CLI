//! Live Activity pushes for the Aogiri app's Dynamic Island
//! (docs/daemon-protocol.md, "Live Activities"; relay-protocol.md, "Live
//! Activity push"). One activity covers this computer: what runs here, what
//! waits for the user, and how the latest turn ended. The phone registers
//! its push-to-start token and the token of each activity it runs
//! (`liveactivity_token`); the daemon asks the relay to start, update or end
//! the activity. Apple can read what goes through APNs, so the state holds
//! generic words, counts, ids and the session's title, never a command, a
//! path or a reply.
//!
//! Push-to-start tokens are kept on disk (`liveactivity.json`): they are
//! what lets an approval light up the island while the app is not running,
//! so a daemon restart must not lose them. An activity's token lives only
//! as long as its activity (at most 8 hours, then 4 on the Lock Screen) and
//! the app sends it again whenever it connects, so it stays in memory.

use crate::{
    hub::{lock, Hub},
    store::{now, write_private},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

const FILE: &str = "liveactivity.json";
/// The app's ActivityAttributes type.
const ATTRIBUTES_TYPE: &str = "ComputerActivityAttributes";
/// At most one plain update per device and activity in this window; the
/// latest state goes when it opens.
pub const WINDOW_MS: u64 = 10_000;
/// How long an activity token that comes in is taken for the one a
/// push-to-start began (the app reports it when iOS wakes it).
const CLAIM_MS: u64 = 30 * 60 * 1000;
/// iOS shows an update as stale after this long without another.
const STALE_S: u64 = 60 * 60;
/// How long the final state stays on the Lock Screen after `end`.
const DISMISS_S: u64 = 15 * 60;
/// How long APNs keeps an update or a start for a phone that is offline;
/// an end gets longer, or the activity stays.
const TTL_S: u64 = 10 * 60;
const END_TTL_S: u64 = 60 * 60;

const THINKING: &str = "思考中";
const REPLYING: &str = "写回复";
const ASK_COMMAND: &str = "电脑想运行命令";
const ASK_EDIT: &str = "电脑想修改文件";
const ASK_QUESTION: &str = "电脑在等你回答";
const ASK_OTHER: &str = "电脑在等你批准";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Running,
    Done,
    Failed,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Running => "running",
            Phase::Done => "done",
            Phase::Failed => "failed",
        }
    }

    fn ember(self) -> &'static str {
        match self {
            Phase::Waiting | Phase::Failed => "attention",
            Phase::Running => "thinking",
            Phase::Done => "done",
        }
    }

    /// Work runs or waits for the user.
    fn busy(self) -> bool {
        matches!(self, Phase::Waiting | Phase::Running)
    }
}

/// The activity's ContentState (the app's `ComputerActivityAttributes`).
#[derive(Debug, Clone, PartialEq)]
struct Content {
    phase: Phase,
    title: String,
    detail: &'static str,
    session: String,
    session_title: String,
    tasks: u32,
    waiting: u32,
    done: u32,
    total: u32,
    /// When the work started (ms).
    since: u64,
    /// The request waiting is a question (for the alert's text).
    question: bool,
    /// The newest request waiting (`<session>/<key>`), not sent: another
    /// one lights the island up again.
    request: String,
}

impl Content {
    /// Exactly the contract's fields, always all of them.
    fn state_json(&self) -> Value {
        json!({
            "phase": self.phase.name(),
            "title": self.title,
            "detail": self.detail,
            "session": self.session,
            "sessionTitle": self.session_title,
            "tasks": self.tasks,
            "waiting": self.waiting,
            "done": self.done,
            "total": self.total,
            "since": self.since as f64 / 1000.0,
            "ember": self.phase.ember(),
        })
    }

    /// The alert that lights up the island for a request.
    fn alert(&self) -> Value {
        let body = match (self.session_title.as_str(), self.question) {
            ("", _) => "打开 Aogiri 查看".to_string(),
            (name, true) => format!("「{name}」在等你回答"),
            (name, false) => format!("「{name}」等你批准"),
        };
        json!({ "title": self.title, "body": body })
    }
}

/// What one session contributes.
#[derive(Debug, Clone, Default)]
struct Line {
    /// The session's title once looked up (empty: it has none).
    title: Option<String>,
    running: bool,
    /// When the running turn started (ms).
    since: u64,
    step: &'static str,
    /// Plan steps done and in all.
    plan: (u32, u32),
    /// Requests waiting for the user: `c:<call id>` (an approval_request,
    /// which ends with its turn) or `a:<action id>` (a deferred action).
    waiting: BTreeSet<String>,
    /// What the latest request asks (an A title), and its key.
    ask: &'static str,
    latest: String,
    waiting_since: u64,
    /// The latest change (ms): the newest line is the one shown.
    touched: u64,
}

impl Line {
    fn idle(&self) -> bool {
        !self.running && self.waiting.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    token: String,
    environment: String,
}

#[derive(Debug, Clone)]
struct Activity {
    token: Token,
    /// The session the phone started it for: shown first while it runs.
    session: Option<String>,
}

/// What one device's activity was last told, for coalescing.
#[derive(Debug, Default)]
struct Lane {
    last_sent: Option<u64>,
    /// The phase the device last heard (without an activity: last saw);
    /// None: an activity the daemon has told nothing yet.
    phase: Option<Phase>,
    /// The state the activity last got.
    content: Option<Content>,
    /// A push-to-start went out in this stretch of work: no second one.
    started: bool,
    /// When a push-to-start went out whose activity's token has not come.
    unclaimed: Option<u64>,
    /// The activity came from this daemon's push-to-start: it ends when the
    /// work is over even if the daemon told it nothing yet. One the app
    /// started itself waits for the work it is about.
    ours: bool,
}

#[derive(Debug, Default)]
struct Device {
    host_name: String,
    start: Option<Token>,
    activity: Option<Activity>,
    lane: Lane,
}

#[derive(Debug, Default)]
struct State {
    devices: BTreeMap<String, Device>,
    lines: HashMap<String, Line>,
    /// The latest turn that ended: its session, its line then, and whether
    /// it failed.
    ended: Option<(String, Line, bool)>,
    /// When the deferred actions waiting were last checked against the
    /// store (an action can be closed without being decided).
    checked: u64,
}

/// One request for the relay.
#[derive(Debug, Clone)]
pub struct LivePush {
    device: String,
    token: String,
    environment: String,
    /// The push request's `liveactivity` object.
    request: Value,
    ttl: u64,
}

pub struct LiveActivities {
    path: PathBuf,
    state: Mutex<State>,
    /// Requests for the relay in the order they were decided, and whether
    /// a thread is sending them: an update never overtakes the end.
    outbox: Mutex<(Vec<LivePush>, bool)>,
}

impl LiveActivities {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let mut state = State::default();
        let saved = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        for entry in saved {
            let (Some(device), Some(token)) = (entry["device"].as_str(), entry["token"].as_str())
            else {
                continue;
            };
            let device = state.devices.entry(device.to_string()).or_default();
            device.host_name = entry["host_name"].as_str().unwrap_or_default().to_string();
            device.start = Some(Token {
                token: token.to_string(),
                environment: entry["environment"]
                    .as_str()
                    .unwrap_or("production")
                    .to_string(),
            });
        }
        Self {
            path,
            state: Mutex::new(state),
            outbox: Mutex::new((Vec::new(), false)),
        }
    }

    fn save(&self, state: &State) -> Result<(), String> {
        let list: Vec<Value> = state
            .devices
            .iter()
            .filter_map(|(device, entry)| {
                let start = entry.start.as_ref()?;
                Some(json!({
                    "device": device,
                    "token": start.token,
                    "environment": start.environment,
                    "host_name": entry.host_name,
                }))
            })
            .collect();
        write_private(&self.path, format!("{:#}\n", json!(list)).as_bytes())
            .map_err(|error| error.to_string())
    }

    /// `liveactivity_token`: `kind` `start` (the app's push-to-start token,
    /// one per device) or `activity` (a running activity's token, replacing
    /// the device's earlier one).
    pub fn register(&self, device: &str, op: &Value, at: u64) -> Result<(), String> {
        let token = op["token"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let environment = op["environment"].as_str().unwrap_or("production");
        if !hex_token(&token) || !matches!(environment, "production" | "sandbox") {
            return Err(
                "liveactivity_token requires a hex token and environment production or sandbox"
                    .to_string(),
            );
        }
        let token = Token {
            token,
            environment: environment.to_string(),
        };
        let host_name: String = op["host_name"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .chars()
            .take(64)
            .collect();
        let mut state = lock(&self.state);
        match op["kind"].as_str() {
            Some("start") => {
                // A token registered again by another device moves to it.
                for (id, other) in state.devices.iter_mut() {
                    if id != device && other.start.as_ref().is_some_and(|t| t.token == token.token)
                    {
                        other.start = None;
                    }
                }
                let entry = state.devices.entry(device.to_string()).or_default();
                if !host_name.is_empty() {
                    entry.host_name = host_name;
                }
                entry.start = Some(token);
                self.save(&state)
            }
            Some("activity") => {
                let session = op["session"].as_str().filter(|s| !s.is_empty());
                let ids = [op["activity"].as_str(), session];
                if ids.into_iter().flatten().any(|id| !id_text(id)) {
                    return Err("liveactivity_token: bad activity or session id".to_string());
                }
                let entry = state.devices.entry(device.to_string()).or_default();
                if !host_name.is_empty() {
                    entry.host_name = host_name;
                }
                // Another activity: it gets the current state at once (or,
                // one a push-to-start began, its end when the work is over).
                if entry.activity.as_ref().is_none_or(|a| a.token != token) {
                    let ours = entry
                        .lane
                        .unclaimed
                        .is_some_and(|sent| at.saturating_sub(sent) < CLAIM_MS);
                    entry.lane = Lane {
                        ours,
                        ..Lane::default()
                    };
                }
                entry.activity = Some(Activity {
                    token,
                    session: session.map(str::to_string),
                });
                Ok(())
            }
            _ => Err("liveactivity_token requires kind start or activity".to_string()),
        }
    }

    /// A revoked device's tokens go.
    pub fn forget_device(&self, device: &str) {
        let mut state = lock(&self.state);
        if state.devices.remove(device).is_some() {
            let _ = self.save(&state);
        }
    }

    /// APNs said the token is gone (410).
    fn forget_token(&self, device: &str, token: &str) {
        let mut state = lock(&self.state);
        let Some(entry) = state.devices.get_mut(device) else {
            return;
        };
        if entry
            .activity
            .as_ref()
            .is_some_and(|a| a.token.token == token)
        {
            entry.activity = None;
        }
        if entry.start.as_ref().is_some_and(|t| t.token == token) {
            entry.start = None;
            let _ = self.save(&state);
        }
    }

    fn has_tokens(&self) -> bool {
        lock(&self.state)
            .devices
            .values()
            .any(|d| d.start.is_some() || d.activity.is_some())
    }

    /// Takes a session event; true when what the island shows changed.
    /// `failed`: the turn that just ended ended on an error.
    pub fn observe(&self, session: &str, event: &Value, failed: bool, at: u64) -> bool {
        let mut state = lock(&self.state);
        let text = |name: &str| event[name].as_str().unwrap_or_default().to_string();
        match event["type"].as_str().unwrap_or_default() {
            "user_message" => {
                let line = state.lines.entry(session.to_string()).or_default();
                if !line.running {
                    line.running = true;
                    line.since = at;
                }
                line.step = THINKING;
                // Look the title up again: it follows the conversation.
                line.title = None;
                line.touched = at;
                true
            }
            kind @ ("tool_start" | "assistant_start") => {
                let Some(line) = state.lines.get_mut(session).filter(|l| l.running) else {
                    return false;
                };
                let step = match kind {
                    "tool_start" => step_word(event["name"].as_str().unwrap_or_default()),
                    _ => REPLYING,
                };
                let changed = line.step != step;
                line.step = step;
                line.touched = at;
                changed
            }
            "plan" => {
                let steps = event["plan"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let done = steps.iter().filter(|s| s["status"] == "completed").count();
                let plan = (done as u32, steps.len() as u32);
                // A plan replayed when a session opens is not work.
                let Some(line) = state.lines.get_mut(session) else {
                    return false;
                };
                let changed = line.plan != plan;
                line.plan = plan;
                changed
            }
            kind @ ("approval_request" | "action_deferred") => {
                let line = state.lines.entry(session.to_string()).or_default();
                let (call, action) = (text("call_id"), text("id"));
                // A request nobody watched became a deferred action.
                line.waiting.remove(&format!("c:{call}"));
                if line.waiting.is_empty() {
                    line.waiting_since = at;
                }
                let key = match kind {
                    "approval_request" => format!("c:{call}"),
                    _ => format!("a:{action}"),
                };
                line.waiting.insert(key.clone());
                line.latest = key;
                line.ask = ask_title(event);
                line.touched = at;
                true
            }
            kind @ ("tool_output" | "action_decided") => {
                let key = match kind {
                    "tool_output" => format!("c:{}", text("call_id")),
                    _ => format!("a:{}", text("id")),
                };
                let Some(line) = state.lines.get_mut(session) else {
                    return false;
                };
                let removed = line.waiting.remove(&key);
                if removed && line.idle() {
                    state.lines.remove(session);
                }
                removed
            }
            "status" if event["message"] == "ready" => state.turn_ended(session, failed),
            _ => false,
        }
    }

    /// The sessions the island knows about.
    fn sessions(&self) -> Vec<String> {
        lock(&self.state).lines.keys().cloned().collect()
    }

    /// Whether the deferred actions waiting are due for a check against
    /// the store (once per window, and only while one waits).
    fn check_due(&self, at: u64) -> bool {
        let mut state = lock(&self.state);
        let waiting = state
            .lines
            .values()
            .any(|line| line.waiting.iter().any(|key| key.starts_with("a:")));
        if !waiting || at.saturating_sub(state.checked) < WINDOW_MS {
            return false;
        }
        state.checked = at;
        true
    }

    /// Drops the deferred actions no longer open (closed by the user or a
    /// new run). True when something changed.
    fn keep_open_actions(&self, open: &BTreeSet<String>) -> bool {
        let mut state = lock(&self.state);
        let mut changed = false;
        state.lines.retain(|_, line| {
            let before = line.waiting.len();
            line.waiting
                .retain(|key| key.strip_prefix("a:").is_none_or(|id| open.contains(id)));
            changed |= line.waiting.len() != before;
            !line.idle()
        });
        changed
    }

    /// A session's engine went away: a turn it ran failed, and what only
    /// lived with the engine goes (a deferred action stays).
    fn closed(&self, session: &str) -> bool {
        lock(&self.state).turn_ended(session, true)
    }

    /// What is due for each device now: an `end` once the work is over,
    /// an update into waiting at once, other updates at most once per
    /// window, and a push-to-start for a request when the device has no
    /// activity. `title` looks a session's title up.
    pub fn sends(
        &self,
        at: u64,
        host: &str,
        title: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<LivePush> {
        let missing: Vec<String> = {
            let state = lock(&self.state);
            if !state
                .devices
                .values()
                .any(|d| d.start.is_some() || d.activity.is_some())
            {
                return Vec::new();
            }
            let ended = state.ended.iter().map(|(id, line, _)| (id, line));
            state
                .lines
                .iter()
                .chain(ended)
                .filter(|(_, line)| line.title.is_none())
                .map(|(id, _)| id.clone())
                .collect()
        };
        let titles: HashMap<String, String> = missing
            .into_iter()
            .map(|id| {
                let found = title(&id).map(|t| clip(&t, 40)).unwrap_or_default();
                (id, found)
            })
            .collect();
        let mut state = lock(&self.state);
        let State {
            devices,
            lines,
            ended,
        } = &mut *state;
        let lines_iter = lines.iter_mut();
        let ended_iter = ended.iter_mut().map(|(id, line, _)| (&*id, line));
        for (id, line) in lines_iter.chain(ended_iter) {
            if line.title.is_none() {
                line.title = titles.get(id).cloned();
            }
        }
        let mut out = Vec::new();
        for (device, entry) in devices.iter_mut() {
            let preferred = entry.activity.as_ref().and_then(|a| a.session.as_deref());
            let content = content(lines, ended.as_ref(), preferred);
            let phase = content.phase;
            let lane = &mut entry.lane;
            if let Some(activity) = entry.activity.as_ref() {
                let token = activity.token.clone();
                if !phase.busy() {
                    // The work is over: end the activity, which this
                    // device then forgets.
                    if lane.phase.is_some_and(Phase::busy) || (lane.phase.is_none() && lane.ours) {
                        out.push(live_push(
                            device,
                            &token,
                            request("end", &content, at),
                            END_TTL_S,
                        ));
                        entry.activity = None;
                        *lane = Lane {
                            last_sent: Some(at),
                            phase: Some(phase),
                            ..Lane::default()
                        };
                    }
                    continue;
                }
                if lane.content.as_ref() == Some(&content) {
                    continue;
                }
                // A request the activity does not show yet.
                let into_waiting = phase == Phase::Waiting
                    && lane
                        .content
                        .as_ref()
                        .is_none_or(|c| c.request != content.request);
                let open = lane
                    .last_sent
                    .is_none_or(|sent| at.saturating_sub(sent) >= WINDOW_MS);
                if !(into_waiting || open) {
                    continue;
                }
                let mut update = request("update", &content, at);
                // Lights up only on a change into waiting the activity saw
                // happen (a new one was started with its own alert).
                if into_waiting && lane.phase.is_some() {
                    update["alert"] = content.alert();
                }
                out.push(live_push(device, &token, update, TTL_S));
                lane.last_sent = Some(at);
                lane.phase = Some(phase);
                lane.content = Some(content);
                continue;
            }
            if let Some(start) = &entry.start {
                if phase == Phase::Waiting && lane.phase != Some(phase) && !lane.started {
                    let mut start_request = request("start", &content, at);
                    start_request["attributes_type"] = json!(ATTRIBUTES_TYPE);
                    start_request["attributes"] =
                        json!({ "host": host, "hostName": entry.host_name });
                    start_request["alert"] = content.alert();
                    out.push(live_push(device, start, start_request, TTL_S));
                    lane.started = true;
                    lane.unclaimed = Some(at);
                    lane.last_sent = Some(at);
                }
            }
            if !phase.busy() {
                lane.started = false;
            }
            lane.phase = Some(phase);
        }
        out
    }
}

impl State {
    /// A session's turn ended: its approvals went with it.
    fn turn_ended(&mut self, session: &str, failed: bool) -> bool {
        let Some(line) = self.lines.get_mut(session) else {
            return false;
        };
        let was_running = std::mem::take(&mut line.running);
        let before = line.waiting.len();
        line.waiting.retain(|key| key.starts_with("a:"));
        let changed = was_running || line.waiting.len() != before;
        if was_running {
            self.ended = Some((session.to_string(), line.clone(), failed));
        }
        if line.idle() {
            self.lines.remove(session);
        }
        changed
    }
}

/// What the island shows: a request waiting (the newest, or the one of the
/// session the activity is about), else the work running, else how the
/// latest turn ended.
fn content(
    lines: &HashMap<String, Line>,
    ended: Option<&(String, Line, bool)>,
    preferred: Option<&str>,
) -> Content {
    let tasks = lines.values().filter(|l| l.running).count() as u32;
    let waiting = lines.values().map(|l| l.waiting.len()).sum::<usize>() as u32;
    let pick = |want: fn(&Line) -> bool| {
        preferred
            .and_then(|p| lines.get_key_value(p))
            .filter(|(_, line)| want(line))
            .or_else(|| {
                lines
                    .iter()
                    .filter(|(_, line)| want(line))
                    .max_by_key(|(id, line)| (line.touched, *id))
            })
    };
    let shown = |id: &str, line: &Line| (session_id(id), line.title.clone().unwrap_or_default());
    if let Some((id, line)) = pick(|l| !l.waiting.is_empty()) {
        let (session, session_title) = shown(id, line);
        return Content {
            phase: Phase::Waiting,
            title: line.ask.to_string(),
            detail: "",
            session,
            session_title,
            tasks,
            waiting,
            done: line.plan.0,
            total: line.plan.1,
            // The turn's start; a request of a session not running (a
            // deferred action): when it came.
            since: if line.running {
                line.since
            } else {
                line.waiting_since
            },
            question: line.ask == ASK_QUESTION,
            request: format!("{id}/{}", line.latest),
        };
    }
    if let Some((id, line)) = pick(|l| l.running) {
        let (session, session_title) = shown(id, line);
        return Content {
            phase: Phase::Running,
            title: if session_title.is_empty() {
                "电脑任务".to_string()
            } else {
                session_title.clone()
            },
            detail: line.step,
            session,
            session_title,
            tasks,
            waiting,
            done: line.plan.0,
            total: line.plan.1,
            since: line.since,
            question: false,
            request: String::new(),
        };
    }
    let (phase, title) = match ended {
        Some((_, _, true)) => (Phase::Failed, "出错了"),
        _ => (Phase::Done, "做完了"),
    };
    let (session, session_title, line) = match ended {
        Some((id, line, _)) => {
            let (session, title) = shown(id, line);
            (session, title, line.clone())
        }
        None => (String::new(), String::new(), Line::default()),
    };
    Content {
        phase,
        title: title.to_string(),
        detail: "",
        session,
        session_title,
        tasks,
        waiting,
        done: line.plan.0,
        total: line.plan.1,
        since: line.since,
        question: false,
        request: String::new(),
    }
}

/// The push request's `liveactivity` object (relay-protocol.md).
fn request(event: &str, content: &Content, at: u64) -> Value {
    let seconds = at / 1000;
    let mut request = json!({
        "event": event,
        "content_state": content.state_json(),
        "timestamp": seconds,
        "relevance_score": if content.phase == Phase::Waiting { 100 } else { 80 },
    });
    if event == "end" {
        request["dismissal_date"] = json!(seconds + DISMISS_S);
    } else {
        request["stale_date"] = json!(seconds + STALE_S);
    }
    request
}

fn live_push(device: &str, token: &Token, request: Value, ttl: u64) -> LivePush {
    LivePush {
        device: device.to_string(),
        token: token.token.clone(),
        environment: token.environment.clone(),
        request,
        ttl,
    }
}

/// The step a tool call is, in a word: never its arguments. A tool this
/// does not know (an MCP tool, an ACP agent's titled call) is 使用工具.
fn step_word(tool: &str) -> &'static str {
    match tool {
        "bash" | "shell" | "exec_command" | "local_shell" | "write_stdin" => "运行命令",
        "write" | "str_replace" | "apply_patch" | "hashline_edit" | "cp" | "edit" => "修改文件",
        "read" | "ls" | "outline" => "读取文件",
        "ripgrep" | "grep" | "glob" | "find" | "search" => "搜索",
        "web_search" | "web_fetch" => "查资料",
        "spawn_agent" | "Task" | "Agent" => "派出子任务",
        "update_plan" | "TodoWrite" => "更新计划",
        name if name.starts_with("agent_") => "派出子任务",
        _ => "使用工具",
    }
}

/// What a request asks, as the island's title.
fn ask_title(event: &Value) -> &'static str {
    if event["questions"].as_array().is_some_and(|q| !q.is_empty()) {
        return ASK_QUESTION;
    }
    match event["kind"].as_str() {
        Some("command" | "escalation" | "elevation" | "rule") => ASK_COMMAND,
        Some("edit" | "outside_write") => ASK_EDIT,
        _ => match step_word(event["name"].as_str().unwrap_or_default()) {
            "运行命令" => ASK_COMMAND,
            "修改文件" => ASK_EDIT,
            _ => ASK_OTHER,
        },
    }
}

/// An id the relay passes on (`[A-Za-z0-9._:-]`, at most 128).
fn id_text(value: &str) -> bool {
    value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

fn session_id(id: &str) -> String {
    if id_text(id) {
        id.to_string()
    } else {
        String::new()
    }
}

/// A push-to-start or activity token: hex, longer than a device token
/// (128 bytes on current iOS).
fn hex_token(token: &str) -> bool {
    (32..=512).contains(&token.len())
        && token.len().is_multiple_of(2)
        && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

/// Takes a session event (from `push::observe`) and sends what is due.
pub fn observe(hub: &Arc<Hub>, session: &str, event: &Value, failed: bool) {
    if hub.push.live.observe(session, event, failed, now()) {
        send_due(hub);
    }
}

/// From the scheduler tick: updates whose window opened, and the end of a
/// turn whose engine went away.
pub fn tick(hub: &Arc<Hub>) {
    let live = &hub.push.live;
    if !live.has_tokens() {
        return;
    }
    for session in live.sessions() {
        if !hub.is_open(&session) {
            live.closed(&session);
        }
    }
    let at = now();
    if live.check_due(at) {
        let open = hub.store.open_actions().into_iter().map(|a| a.id).collect();
        live.keep_open_actions(&open);
    }
    send_due(hub);
}

/// `liveactivity_token` from a paired device.
pub fn register(hub: &Arc<Hub>, device: &str, op: &Value) -> Result<Value, String> {
    hub.push.live.register(device, op, now())?;
    send_due(hub);
    Ok(json!({ "type": "liveactivity_token_ok" }))
}

fn send_due(hub: &Arc<Hub>) {
    if !hub.push.live.has_tokens() {
        return;
    }
    let Some(host) = hub.relay.host_id() else {
        return;
    };
    let title = |session: &str| {
        let record = hub.store.sessions().into_iter().find(|r| r.id == session)?;
        crate::hub::shown_title(&record, None)
    };
    {
        // Decided and queued under one lock: what is decided later is
        // sent later.
        let mut outbox = lock(&hub.push.live.outbox);
        let sends = hub.push.live.sends(now(), &host, &title);
        if sends.is_empty() {
            return;
        }
        outbox.0.extend(sends);
        if std::mem::replace(&mut outbox.1, true) {
            return;
        }
    }
    let hub = Arc::clone(hub);
    // One thread at a time, in order, until the outbox is empty.
    thread::spawn(move || loop {
        let batch = {
            let mut outbox = lock(&hub.push.live.outbox);
            if outbox.0.is_empty() {
                outbox.1 = false;
                return;
            }
            std::mem::take(&mut outbox.0)
        };
        for send in batch {
            match hub
                .relay
                .live_activity(&send.token, &send.environment, &send.request, send.ttl)
            {
                Ok(410) => hub.push.live.forget_token(&send.device, &send.token),
                Ok(status) if status >= 300 => {
                    lynshen_agent_core::log_warn!(
                        "daemon",
                        "live activity push refused",
                        status = status
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    lynshen_agent_core::log_warn!(
                        "daemon",
                        "live activity push failed",
                        error = error
                    );
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(name: &str) -> (LiveActivities, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lynshen-live-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (LiveActivities::load(&dir), dir)
    }

    fn start_token() -> String {
        "ab".repeat(128)
    }

    fn activity_token() -> String {
        "cd".repeat(128)
    }

    fn register(live: &LiveActivities, device: &str, kind: &str, token: &str) {
        register_at(live, device, kind, token, T0);
    }

    fn register_at(live: &LiveActivities, device: &str, kind: &str, token: &str, at: u64) {
        live.register(
            device,
            &json!({ "op": "liveactivity_token", "kind": kind, "token": token, "environment": "sandbox", "host_name": "Chad 的 MacBook", "activity": "A1B2-C3" }),
            at,
        )
        .unwrap();
    }

    fn due(live: &LiveActivities, at: u64) -> Vec<LivePush> {
        live.sends(at, "h0st-1d", &|_| Some("修登录页".to_string()))
    }

    fn event(live: &LiveActivities, session: &str, event: Value, at: u64) {
        live.observe(session, &event, false, at);
    }

    fn state(send: &LivePush) -> &Value {
        &send.request["content_state"]
    }

    const T0: u64 = 1_800_000_000_000;

    #[test]
    fn the_content_state_has_exactly_the_contract_fields() {
        let (live, dir) = live("shape");
        register(&live, "d1", "start", &start_token());
        event(
            &live,
            "s-1",
            json!({ "type": "user_message", "content": "x" }),
            T0,
        );
        assert!(due(&live, T0).is_empty(), "no start for running work");
        event(
            &live,
            "s-1",
            json!({ "type": "approval_request", "call_id": "c1", "name": "bash", "kind": "command" }),
            T0 + 1500,
        );
        let sends = due(&live, T0 + 1500);
        assert_eq!(sends.len(), 1);
        let send = &sends[0];
        assert_eq!(
            (send.token.as_str(), send.environment.as_str()),
            (start_token().as_str(), "sandbox")
        );
        let request = &send.request;
        assert_eq!(request["event"], "start");
        assert_eq!(request["attributes_type"], "ComputerActivityAttributes");
        assert_eq!(
            request["attributes"],
            json!({ "host": "h0st-1d", "hostName": "Chad 的 MacBook" })
        );
        assert_eq!(request["relevance_score"], 100);
        assert_eq!(request["timestamp"], (T0 + 1500) / 1000);
        assert_eq!(request["stale_date"], (T0 + 1500) / 1000 + STALE_S);
        assert_eq!(
            request["alert"],
            json!({ "title": "电脑想运行命令", "body": "「修登录页」等你批准" })
        );
        let content = state(send).as_object().unwrap();
        let mut keys: Vec<&str> = content.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "detail",
                "done",
                "ember",
                "phase",
                "session",
                "sessionTitle",
                "since",
                "tasks",
                "title",
                "total",
                "waiting"
            ]
        );
        for name in [
            "phase",
            "title",
            "detail",
            "session",
            "sessionTitle",
            "ember",
        ] {
            assert!(content[name].is_string(), "{name}");
        }
        for name in ["tasks", "waiting", "done", "total"] {
            assert!(content[name].is_u64(), "{name}");
        }
        assert!(content["since"].is_f64());
        assert_eq!(
            state(send),
            &json!({
                "phase": "waiting", "title": "电脑想运行命令", "detail": "", "session": "s-1",
                "sessionTitle": "修登录页", "tasks": 1, "waiting": 1, "done": 0, "total": 0,
                "since": 1_800_000_000.0, "ember": "attention",
            })
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn step_updates_coalesce_to_one_per_window_with_the_latest_state() {
        let (live, dir) = live("coalesce");
        register(&live, "d1", "activity", &activity_token());
        event(
            &live,
            "s-1",
            json!({ "type": "user_message", "content": "x" }),
            T0,
        );
        let first = due(&live, T0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].request["event"], "update");
        assert_eq!(first[0].request["relevance_score"], 80);
        assert_eq!(state(&first[0])["phase"], "running");
        assert_eq!(state(&first[0])["title"], "修登录页");
        assert_eq!(state(&first[0])["detail"], "思考中");
        assert_eq!(state(&first[0])["ember"], "thinking");
        event(
            &live,
            "s-1",
            json!({ "type": "tool_start", "call_id": "c1", "name": "bash" }),
            T0 + 1000,
        );
        assert!(due(&live, T0 + 1000).is_empty());
        event(
            &live,
            "s-1",
            json!({ "type": "tool_start", "call_id": "c2", "name": "str_replace" }),
            T0 + 2000,
        );
        let plan = json!({ "type": "plan", "plan": [
            { "step": "a", "status": "completed" }, { "step": "b", "status": "in_progress" }, { "step": "c", "status": "pending" },
        ] });
        event(&live, "s-1", plan, T0 + 3000);
        assert!(due(&live, T0 + 3000).is_empty());
        assert!(due(&live, T0 + WINDOW_MS - 1).is_empty());
        let flushed = due(&live, T0 + WINDOW_MS);
        assert_eq!(flushed.len(), 1);
        let content = state(&flushed[0]);
        assert_eq!(
            (
                content["detail"].as_str(),
                content["done"].as_u64(),
                content["total"].as_u64()
            ),
            (Some("修改文件"), Some(1), Some(3))
        );
        assert_eq!(flushed[0].request["timestamp"], (T0 + WINDOW_MS) / 1000);
        // Nothing new: nothing sent, however long it waits.
        assert!(due(&live, T0 + 5 * WINDOW_MS).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn waiting_and_the_end_go_at_once_inside_the_window() {
        let (live, dir) = live("end");
        register(&live, "d1", "activity", &activity_token());
        event(
            &live,
            "s-1",
            json!({ "type": "user_message", "content": "x" }),
            T0,
        );
        assert_eq!(due(&live, T0).len(), 1);
        event(
            &live,
            "s-1",
            json!({ "type": "approval_request", "call_id": "c1", "name": "write", "kind": "edit" }),
            T0 + 1000,
        );
        let waiting = due(&live, T0 + 1000);
        assert_eq!(waiting.len(), 1, "into waiting goes at once");
        assert_eq!(waiting[0].request["event"], "update");
        assert_eq!(state(&waiting[0])["phase"], "waiting");
        assert_eq!(state(&waiting[0])["title"], "电脑想修改文件");
        assert_eq!(waiting[0].request["relevance_score"], 100);
        assert_eq!(waiting[0].request["alert"]["title"], "电脑想修改文件");
        // Approved: back to running, which waits for the window.
        event(
            &live,
            "s-1",
            json!({ "type": "tool_output", "call_id": "c1", "name": "write", "output": "", "is_error": false }),
            T0 + 2000,
        );
        assert!(due(&live, T0 + 2000).is_empty());
        event(
            &live,
            "s-1",
            json!({ "type": "status", "message": "ready" }),
            T0 + 3000,
        );
        let end = due(&live, T0 + 3000);
        assert_eq!(end.len(), 1, "the end goes at once");
        assert_eq!(end[0].request["event"], "end");
        assert_eq!(
            end[0].request["dismissal_date"],
            (T0 + 3000) / 1000 + DISMISS_S
        );
        assert_eq!(end[0].ttl, END_TTL_S);
        assert!(end[0].request.get("stale_date").is_none());
        assert_eq!(state(&end[0])["phase"], "done");
        assert_eq!(state(&end[0])["title"], "做完了");
        assert_eq!(state(&end[0])["ember"], "done");
        assert_eq!(state(&end[0])["since"], T0 as f64 / 1000.0);
        // The activity's token is dropped after its end.
        event(
            &live,
            "s-1",
            json!({ "type": "user_message", "content": "y" }),
            T0 + 60_000,
        );
        assert!(due(&live, T0 + 60_000).is_empty());

        // A turn that failed ends as failed.
        register(&live, "d1", "activity", &"ef".repeat(100));
        assert_eq!(due(&live, T0 + 61_000).len(), 1);
        live.observe(
            "s-1",
            &json!({ "type": "status", "message": "ready" }),
            true,
            T0 + 62_000,
        );
        let failed = due(&live, T0 + 62_000);
        assert_eq!(failed.len(), 1);
        assert_eq!(
            (
                state(&failed[0])["phase"].as_str(),
                state(&failed[0])["ember"].as_str()
            ),
            (Some("failed"), Some("attention"))
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn push_to_start_is_only_for_a_request_and_once_per_stretch_of_work() {
        let (live, dir) = live("start");
        register(&live, "d1", "start", &start_token());
        event(
            &live,
            "s-1",
            json!({ "type": "action_deferred", "id": "act-1", "call_id": "c1", "name": "bash" }),
            T0,
        );
        assert_eq!(due(&live, T0)[0].request["event"], "start");
        // Another request while the first waits: no second activity.
        event(
            &live,
            "s-2",
            json!({ "type": "approval_request", "call_id": "c9", "questions": [{ "q": "?" }] }),
            T0 + 1000,
        );
        assert!(due(&live, T0 + 1000).is_empty());
        event(
            &live,
            "s-1",
            json!({ "type": "action_decided", "id": "act-1", "decision": "allow" }),
            T0 + 2000,
        );
        event(
            &live,
            "s-2",
            json!({ "type": "status", "message": "ready" }),
            T0 + 3000,
        );
        assert!(
            due(&live, T0 + 3000).is_empty(),
            "nothing to end without the activity's token"
        );
        // A request after the work was over starts one again.
        event(
            &live,
            "s-3",
            json!({ "type": "approval_request", "call_id": "c3", "questions": [{ "q": "?" }] }),
            T0 + 4000,
        );
        let again = due(&live, T0 + 4000);
        assert_eq!(again.len(), 1);
        assert_eq!(
            again[0].request["alert"],
            json!({ "title": "电脑在等你回答", "body": "「修登录页」在等你回答" })
        );
        // The token of the activity it started arrives after the request
        // was answered and the work ended: that activity ends.
        event(
            &live,
            "s-3",
            json!({ "type": "status", "message": "ready" }),
            T0 + 5000,
        );
        assert!(due(&live, T0 + 5000).is_empty());
        register_at(&live, "d1", "activity", &activity_token(), T0 + 5500);
        let end = due(&live, T0 + 6000);
        assert_eq!(end.len(), 1);
        assert_eq!(
            (end[0].request["event"].as_str(), end[0].token.as_str()),
            (Some("end"), activity_token().as_str())
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_activity_the_app_started_waits_for_its_work() {
        let (live, dir) = live("own");
        // The phone sends a message and starts B before the daemon saw the
        // turn begin: nothing is ended.
        register(&live, "d1", "activity", &activity_token());
        assert!(due(&live, T0).is_empty());
        assert!(due(&live, T0 + 5 * WINDOW_MS).is_empty());
        event(
            &live,
            "s-1",
            json!({ "type": "user_message", "content": "x" }),
            T0 + 5 * WINDOW_MS + 1,
        );
        let run = due(&live, T0 + 5 * WINDOW_MS + 1);
        assert_eq!(run[0].request["event"], "update");
        event(
            &live,
            "s-1",
            json!({ "type": "status", "message": "ready" }),
            T0 + 5 * WINDOW_MS + 2,
        );
        assert_eq!(
            due(&live, T0 + 5 * WINDOW_MS + 2)[0].request["event"],
            "end"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_started_activity_gets_the_state_without_a_second_alert() {
        let (live, dir) = live("synced");
        register(&live, "d1", "start", &start_token());
        event(
            &live,
            "s-1",
            json!({ "type": "approval_request", "call_id": "c1", "kind": "command" }),
            T0,
        );
        assert_eq!(due(&live, T0)[0].request["event"], "start");
        // Its token arrives at once: an update right away, no alert.
        register(&live, "d1", "activity", &activity_token());
        let sync = due(&live, T0 + 500);
        assert_eq!(sync.len(), 1);
        assert_eq!(
            (
                sync[0].request["event"].as_str(),
                state(&sync[0])["phase"].as_str()
            ),
            (Some("update"), Some("waiting"))
        );
        assert!(sync[0].request.get("alert").is_none());
        // The same token again (the app reconnected): nothing new.
        register(&live, "d1", "activity", &activity_token());
        assert!(due(&live, T0 + 600).is_empty());
        // Answered and running, then a second request: alert again.
        event(
            &live,
            "s-1",
            json!({ "type": "tool_output", "call_id": "c1" }),
            T0 + 700,
        );
        event(
            &live,
            "s-1",
            json!({ "type": "user_message", "content": "x" }),
            T0 + 800,
        );
        assert!(due(&live, T0 + 800).is_empty(), "within the window");
        event(
            &live,
            "s-1",
            json!({ "type": "approval_request", "call_id": "c2", "kind": "edit" }),
            T0 + 900,
        );
        let again = due(&live, T0 + 900);
        assert_eq!(again[0].request["alert"]["title"], "电脑想修改文件");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_of_a_command_a_path_or_a_reply_goes_out() {
        let (live, dir) = live("privacy");
        register(&live, "d1", "activity", &activity_token());
        let secrets = [
            "git push --force",
            "/Users/chad/secret",
            "rm -rf",
            "the reply text",
            "Read /etc/hosts",
        ];
        event(
            &live,
            "s-1",
            json!({ "type": "user_message", "content": "rm -rf /Users/chad/secret" }),
            T0,
        );
        event(
            &live,
            "s-1",
            json!({ "type": "tool_start", "call_id": "c0", "name": "Read /etc/hosts" }),
            T0 + 1,
        );
        event(
            &live,
            "s-1",
            json!({ "type": "assistant_delta", "delta": "the reply text" }),
            T0 + 2,
        );
        let mut sent = due(&live, T0 + 3);
        assert_eq!(state(&sent[0])["detail"], "使用工具");
        event(
            &live,
            "s-1",
            json!({
                "type": "approval_request", "call_id": "c1", "name": "bash", "kind": "escalation",
                "command": "git push --force", "summary": "git push --force", "cwd": "/Users/chad/secret",
                "paths": ["/Users/chad/secret"], "justification": "rm -rf",
            }),
            T0 + 4,
        );
        sent.extend(due(&live, T0 + 4));
        assert_eq!(sent.len(), 2);
        assert_eq!(state(&sent[1])["detail"], "");
        let text =
            serde_json::to_string(&sent.iter().map(|s| &s.request).collect::<Vec<_>>()).unwrap();
        for secret in secrets {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_deferred_action_closed_without_a_decision_stops_waiting() {
        let (live, dir) = live("closed-action");
        register(&live, "d1", "activity", &activity_token());
        event(
            &live,
            "s-1",
            json!({ "type": "action_deferred", "id": "act-1", "call_id": "c1", "name": "bash" }),
            T0,
        );
        assert_eq!(state(&due(&live, T0)[0])["phase"], "waiting");
        assert!(live.check_due(T0 + WINDOW_MS));
        assert!(!live.check_due(T0 + WINDOW_MS + 1), "once per window");
        assert!(!live.keep_open_actions(&BTreeSet::from(["act-1".to_string()])));
        assert!(live.keep_open_actions(&BTreeSet::new()));
        let end = due(&live, T0 + WINDOW_MS + 2);
        assert_eq!(end[0].request["event"], "end");
        assert!(!live.check_due(T0 + 3 * WINDOW_MS), "nothing waits");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn one_activity_covers_the_computer() {
        let (live, dir) = live("merge");
        register(&live, "d1", "activity", &activity_token());
        event(
            &live,
            "s-1",
            json!({ "type": "user_message", "content": "x" }),
            T0,
        );
        event(
            &live,
            "s-2",
            json!({ "type": "user_message", "content": "y" }),
            T0 + 1,
        );
        let first = due(&live, T0 + 1);
        assert_eq!(
            (
                state(&first[0])["tasks"].as_u64(),
                state(&first[0])["session"].as_str()
            ),
            (Some(2), Some("s-2"))
        );
        event(
            &live,
            "s-1",
            json!({ "type": "approval_request", "call_id": "c1", "kind": "command" }),
            T0 + 2,
        );
        let waiting = due(&live, T0 + 2);
        assert_eq!(
            (
                state(&waiting[0])["session"].as_str(),
                state(&waiting[0])["waiting"].as_u64(),
                state(&waiting[0])["tasks"].as_u64()
            ),
            (Some("s-1"), Some(1), Some(2))
        );
        // One session ends: the other still runs, no end.
        event(
            &live,
            "s-2",
            json!({ "type": "status", "message": "ready" }),
            T0 + WINDOW_MS + 3,
        );
        let still = due(&live, T0 + WINDOW_MS + 3);
        assert_eq!(still[0].request["event"], "update");
        assert_eq!(state(&still[0])["tasks"], 1);
        // A session whose engine went away ended its turn.
        assert_eq!(live.sessions(), ["s-1"]);
        assert!(live.closed("s-1"));
        let end = due(&live, T0 + WINDOW_MS + 4);
        assert_eq!(
            (
                end[0].request["event"].as_str(),
                state(&end[0])["phase"].as_str()
            ),
            (Some("end"), Some("failed"))
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn push_to_start_tokens_are_kept_and_go_with_the_device() {
        let (live, dir) = live("keep");
        let upper = start_token().to_ascii_uppercase();
        register(&live, "d1", "start", &upper);
        register(&live, "d1", "activity", &activity_token());
        for bad in [
            json!({ "kind": "start", "token": "zz".repeat(64) }),
            json!({ "kind": "start", "token": "ab".repeat(257) }),
            json!({ "kind": "start", "token": "abc" }),
            json!({ "kind": "start", "token": start_token(), "environment": "staging" }),
            json!({ "kind": "pause", "token": start_token() }),
            json!({ "kind": "activity", "token": activity_token(), "activity": "a/b" }),
            json!({ "kind": "activity", "token": activity_token(), "session": "s 1" }),
        ] {
            assert!(live.register("d1", &bad, T0).is_err(), "{bad}");
        }
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(FILE)).unwrap()).unwrap();
        assert_eq!(
            saved,
            json!([{ "device": "d1", "token": start_token(), "environment": "sandbox", "host_name": "Chad 的 MacBook" }])
        );
        let again = LiveActivities::load(&dir);
        {
            let state = lock(&again.state);
            let device = &state.devices["d1"];
            assert_eq!(
                device.start.as_ref().map(|t| t.token.clone()),
                Some(start_token())
            );
            assert!(
                device.activity.is_none(),
                "activity tokens live in memory only"
            );
        }
        // The same phone signed in again as another device.
        register(&again, "d2", "start", &start_token());
        assert!(lock(&again.state).devices["d1"].start.is_none());
        again.forget_token("d2", &start_token());
        assert!(!again.has_tokens());
        register(&again, "d2", "start", &start_token());
        again.forget_device("d2");
        assert_eq!(fs::read_to_string(dir.join(FILE)).unwrap().trim(), "[]");
        let _ = fs::remove_dir_all(dir);
    }
}

//! Scheduled tasks: a prompt an agent receives at set local times, saved in
//! `~/.lynshen/agents/<id>/schedules.json`. A due task fires once and its
//! `next_run_at` moves past now, so times missed while the daemon (or the
//! computer) was off fire once on start, never as a burst. A task of a
//! disabled agent waits, neither firing nor moving on, and fires once when
//! the agent is enabled again. Schedule times are unix seconds.
//!
//! Each firing is a run (`last_run`, the 20 before it in `history`): it is
//! `running` from when it fires until the turn its message started ends,
//! `waiting` while an approval waits, `background` while work its turn left
//! going comes back to it (background subagents), then `done` or `failed`.
//! A time that comes while the last run is not over is skipped. A run fails
//! when its turn ends on an error, it runs past `RUN_TIMEOUT_SECS` or past
//! its `budget`, its worktree cannot be made, or its session closes midway;
//! after `MAX_FAILURES` failures in a row the task switches itself off.
//! While a run is `running` or `background` the computer is kept from idle
//! sleep (`awake`). A task with `worktree` runs each new session in a git
//! worktree of its own (`worktrees`), beside the agent's repository.

use crate::{
    agents::Agents,
    hub::{lock, Hub},
    store::{now, random_hex, Message},
};
use chrono::{
    DateTime, Datelike, Days, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone,
    Timelike,
};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

/// A run going longer than this (working, waiting or on background work)
/// is stopped and counts as failed.
pub const RUN_TIMEOUT_SECS: u64 = 2 * 60 * 60;
/// Failures in a row after which a task switches itself off.
pub const MAX_FAILURES: u32 = 3;
/// Earlier runs kept with a task (`history`).
pub const HISTORY: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub id: String,
    pub agent: String,
    pub name: String,
    pub prompt: String,
    pub enabled: bool,
    /// `once`, `hourly`, `daily`, `weekdays` or `weekly`.
    pub repeat: String,
    /// Local `HH:MM`; `hourly` uses only the minute.
    pub time: String,
    /// `weekly`: 0 = Sunday … 6 = Saturday.
    pub days: Vec<u8>,
    /// `once`: local `YYYY-MM-DD`.
    pub date: Option<String>,
    /// Each run starts a new session (carrying the last run's handoff
    /// note); false continues the last run's.
    pub new_session: bool,
    /// The agent proposed it (with its `schedule` tool); it starts switched
    /// off until the user turns it on.
    pub by_agent: bool,
    /// The `propose_schedule` card it was created from (`prop-…`), which
    /// the card looks for to show it as created.
    pub origin: Option<String>,
    pub created_at: u64,
    pub last_run_at: Option<u64>,
    /// The session the last run was delivered to.
    pub last_session: Option<String>,
    pub next_run_at: Option<u64>,
    /// A run that starts a new session starts it in a new git worktree of
    /// the agent's repository.
    pub worktree: bool,
    /// What one run may cost, with the sessions it started: gateway points
    /// for runs through the LynShen gateway, estimated US dollars for
    /// others. A run past it stops and fails.
    pub budget: Option<f64>,
    /// Runs failed in a row while the task was on.
    pub failures: u32,
    /// Times skipped in a row because the last run was not over.
    pub skipped: u32,
    pub last_run: Option<Run>,
    /// The runs before `last_run`, newest first (`HISTORY` of them).
    pub history: Vec<Run>,
}

/// One run of a task.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// When it fired (unix seconds).
    pub at: u64,
    /// When it ended (`done` or `failed`).
    pub ended_at: Option<u64>,
    /// The session its message reached; None until delivered.
    pub session: Option<String>,
    /// `preparing` (its worktree is being made), `running`, `waiting` (an
    /// approval), `background` (its turn ended; work it left going comes
    /// back to it), `done` or `failed`.
    pub status: String,
    pub error: Option<String>,
    /// The message the run sent (`m-…`), found again in its session.
    pub message: Option<String>,
    /// Its session took the message: the turn it started is the run's.
    pub started: bool,
    /// The user (or another sender) wrote in its session after it was
    /// done: what happens there since is not the run's.
    pub followed_up: bool,
    /// The worktree made for it: `{repo, path, branch, base, slug}`.
    pub worktree: Option<Value>,
    /// Where it works inside that worktree.
    pub cwd: Option<String>,
}

impl Run {
    fn new(at: u64) -> Self {
        Self {
            at,
            ended_at: None,
            session: None,
            status: "running".to_string(),
            error: None,
            message: None,
            started: false,
            followed_up: false,
            worktree: None,
            cwd: None,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "at": self.at,
            "ended_at": self.ended_at,
            "session": self.session,
            "status": self.status,
            "error": self.error,
            "message": self.message,
            "started": self.started,
            "followed_up": self.followed_up,
            "worktree": self.worktree,
            "cwd": self.cwd,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let run = value.as_object()?;
        let text = |key: &str| run.get(key).and_then(Value::as_str).map(str::to_string);
        let flag = |key: &str| run.get(key).and_then(Value::as_bool);
        Some(Self {
            at: run.get("at").and_then(Value::as_u64).unwrap_or_default(),
            ended_at: run.get("ended_at").and_then(Value::as_u64),
            session: text("session"),
            status: text("status").unwrap_or_else(|| "done".to_string()),
            error: text("error"),
            message: text("message"),
            started: flag("started").unwrap_or(true),
            followed_up: flag("followed_up").unwrap_or(false),
            worktree: run.get("worktree").filter(|tree| tree.is_object()).cloned(),
            cwd: text("cwd"),
        })
    }

    /// Not over: its worktree being made, working, waiting for an
    /// approval, or on background work.
    fn going(&self) -> bool {
        matches!(
            self.status.as_str(),
            "preparing" | "running" | "waiting" | "background"
        )
    }
}

impl Schedule {
    fn new(id: String, agent: String, created_at: u64) -> Self {
        Self {
            id,
            agent,
            name: String::new(),
            prompt: String::new(),
            enabled: true,
            repeat: String::new(),
            time: String::new(),
            days: Vec::new(),
            date: None,
            new_session: true,
            by_agent: false,
            origin: None,
            created_at,
            last_run_at: None,
            last_session: None,
            next_run_at: None,
            worktree: false,
            budget: None,
            failures: 0,
            skipped: 0,
            last_run: None,
            history: Vec::new(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "agent": self.agent,
            "name": self.name,
            "prompt": self.prompt,
            "enabled": self.enabled,
            "repeat": self.repeat,
            "time": self.time,
            "days": self.days,
            "date": self.date,
            "new_session": self.new_session,
            "by_agent": self.by_agent,
            "origin": self.origin,
            "created_at": self.created_at,
            "last_run_at": self.last_run_at,
            "last_session": self.last_session,
            "next_run_at": self.next_run_at,
            "worktree": self.worktree,
            "budget": self.budget,
            "failures": self.failures,
            "skipped": self.skipped,
            "last_run": self.last_run.as_ref().map(Run::to_json),
            "history": self.history.iter().map(Run::to_json).collect::<Vec<_>>(),
        })
    }

    /// A schedule as saved; the saved `next_run_at` stays, so one that came
    /// due while the daemon was down still fires. A task saved before runs
    /// were recorded (no `last_run`) loads with none, and no failures.
    fn from_json(value: &Value) -> Option<Self> {
        let mut schedule = Self {
            by_agent: value["by_agent"] == true,
            last_run_at: value["last_run_at"].as_u64(),
            last_session: value["last_session"].as_str().map(str::to_string),
            next_run_at: value["next_run_at"].as_u64(),
            failures: value["failures"].as_u64().unwrap_or_default() as u32,
            skipped: value["skipped"].as_u64().unwrap_or_default() as u32,
            last_run: Run::from_json(&value["last_run"]),
            history: value["history"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Run::from_json)
                .take(HISTORY)
                .collect(),
            ..Self::new(
                value["id"].as_str()?.to_string(),
                value["agent"].as_str()?.to_string(),
                value["created_at"].as_u64().unwrap_or_default(),
            )
        };
        schedule.apply(value).ok()?;
        Some(schedule)
    }

    /// Takes the editable fields present in `changes`, then checks the
    /// whole schedule.
    fn apply(&mut self, changes: &Value) -> Result<(), String> {
        let text = |key: &str| changes[key].as_str().map(|text| text.trim().to_string());
        if let Some(name) = text("name") {
            self.name = name;
        }
        if let Some(prompt) = text("prompt") {
            self.prompt = prompt;
        }
        if let Some(enabled) = changes["enabled"].as_bool() {
            self.enabled = enabled;
        }
        if let Some(new_session) = changes["new_session"].as_bool() {
            self.new_session = new_session;
        }
        if let Some(repeat) = text("repeat") {
            self.repeat = repeat;
        }
        if let Some(time) = text("time") {
            self.time = time;
        }
        if let Some(date) = text("date") {
            self.date = Some(date);
        }
        if let Some(origin) = text("origin").filter(|origin| !origin.is_empty()) {
            self.origin = Some(origin);
        }
        if let Some(worktree) = changes["worktree"].as_bool() {
            self.worktree = worktree;
        }
        // Absent: as it was; null: no budget.
        match changes.get("budget") {
            None => {}
            Some(Value::Null) => self.budget = None,
            Some(budget) => match budget.as_f64() {
                Some(budget) if budget > 0.0 && budget.is_finite() => self.budget = Some(budget),
                _ => return Err("budget must be a number above 0".to_string()),
            },
        }
        if let Some(days) = changes["days"].as_array() {
            let mut parsed = days
                .iter()
                .map(|day| day.as_u64().filter(|day| *day <= 6).map(|day| day as u8))
                .collect::<Option<Vec<u8>>>()
                .ok_or("days must be numbers 0-6 (0 is Sunday)")?;
            parsed.sort_unstable();
            parsed.dedup();
            self.days = parsed;
        }
        if self.name.is_empty() {
            return Err("schedule name must not be empty".to_string());
        }
        if self.prompt.is_empty() {
            return Err("schedule prompt must not be empty".to_string());
        }
        if !matches!(
            self.repeat.as_str(),
            "once" | "hourly" | "daily" | "weekdays" | "weekly"
        ) {
            return Err(format!(
                "unknown repeat '{}': use once, hourly, daily, weekdays or weekly",
                self.repeat
            ));
        }
        let time = NaiveTime::parse_from_str(&self.time, "%H:%M")
            .map_err(|_| format!("invalid time '{}': use HH:MM (24-hour)", self.time))?;
        self.time = time.format("%H:%M").to_string();
        if self.repeat == "weekly" && self.days.is_empty() {
            return Err("a weekly schedule needs days (0 is Sunday, 6 is Saturday)".to_string());
        }
        if self.repeat == "once" {
            let date = self
                .date
                .as_deref()
                .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
                .ok_or("a one-off schedule needs date as YYYY-MM-DD")?;
            self.date = Some(date.format("%Y-%m-%d").to_string());
        }
        Ok(())
    }

    /// Its latest run is not over.
    fn running(&self) -> bool {
        self.last_run.as_ref().is_some_and(Run::going)
    }
}

/// When `schedule` next runs strictly after `now`, in `now`'s time zone;
/// None when it is disabled or a one-off whose time has passed. A local time
/// skipped by a DST change runs an hour later; one that occurs twice runs at
/// the first.
pub fn next_run<Tz: TimeZone>(schedule: &Schedule, now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
    if !schedule.enabled {
        return None;
    }
    let time = NaiveTime::parse_from_str(&schedule.time, "%H:%M").ok()?;
    let zone = now.timezone();
    let after_now = |local: NaiveDateTime| {
        zone.from_local_datetime(&local)
            .earliest()
            .or_else(|| {
                zone.from_local_datetime(&(local + TimeDelta::hours(1)))
                    .earliest()
            })
            .filter(|at| at > now)
    };
    let today = now.naive_local().date();
    match schedule.repeat.as_str() {
        "once" => after_now(
            NaiveDate::parse_from_str(schedule.date.as_deref()?, "%Y-%m-%d")
                .ok()?
                .and_time(time),
        ),
        "hourly" => {
            let first = today.and_hms_opt(now.naive_local().hour(), time.minute(), 0)?;
            (0..=26).find_map(|hours| after_now(first + TimeDelta::hours(hours)))
        }
        repeat => (0..=8).find_map(|days| {
            let date = today.checked_add_days(Days::new(days))?;
            let weekday = date.weekday().num_days_from_sunday() as u8;
            let runs = match repeat {
                "daily" => true,
                "weekdays" => (1..=5).contains(&weekday),
                "weekly" => schedule.days.contains(&weekday),
                _ => false,
            };
            runs.then(|| after_now(date.and_time(time))).flatten()
        }),
    }
}

/// `next_run` after `now` (unix seconds) in this machine's time zone.
fn next_run_at(schedule: &Schedule, now: u64) -> Option<u64> {
    let now = Local.timestamp_opt(now as i64, 0).single()?;
    next_run(schedule, &now).map(|at| at.timestamp() as u64)
}

/// Every agent's saved schedules, read at startup.
pub fn load(agents: &Agents) -> Vec<Schedule> {
    agents
        .list()
        .iter()
        .flat_map(|agent| agents.schedules(&agent.id))
        .filter_map(|value| Schedule::from_json(&value))
        .collect()
}

fn seconds() -> u64 {
    now() / 1000
}

/// What a run has cost against `budget`, from `usage` (`schedule_usage`'s
/// shape over the run's session and the sessions it started): in points
/// when its own session went through the gateway, else in estimated US
/// dollars. Some((cost, unit)) once past the budget.
pub(crate) fn over_budget(
    usage: &Value,
    session: &str,
    budget: f64,
) -> Option<(f64, &'static str)> {
    let gateway = usage["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|row| row["session"] == session && row["gateway_requests"].as_u64() > Some(0));
    let (unit, cost) = if gateway {
        ("点", usage["totals"]["gateway_cost"].as_f64())
    } else {
        (
            "美元（估算）",
            usage["totals"]["estimated_cost_usd"].as_f64(),
        )
    };
    cost.filter(|cost| *cost > budget).map(|cost| (cost, unit))
}

/// How often running runs are checked against their time and budget, and
/// runs on background work settled (see `Hub::sweep_schedule_runs`).
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// The line `Hub::deliver` puts first names the message: `· <id>]`.
fn names_message(content: &str, message: &str) -> bool {
    content
        .lines()
        .next()
        .is_some_and(|head| head.ends_with(&format!(" · {message}]")))
}

impl Hub {
    /// All schedules, or `agent`'s.
    pub fn schedules_json(&self, agent: Option<&str>) -> Value {
        let list: Vec<Value> = lock(&self.schedules)
            .iter()
            .filter(|schedule| agent.is_none_or(|agent| schedule.agent == agent))
            .map(Schedule::to_json)
            .collect();
        json!({ "type": "schedules", "schedules": list })
    }

    /// Creates a schedule (no `id`) or changes one; its agent stays.
    pub fn save_schedule(&self, changes: &Value) -> Result<Schedule, String> {
        self.save_schedule_by(changes, None)
    }

    /// An agent's own `schedule` tool: it may create and change only its
    /// own tasks, and whatever it saves is switched off until the user
    /// turns it on.
    pub fn propose_schedule(&self, agent: &str, fields: &Value) -> Result<Schedule, String> {
        let mut changes = json!({ "agent": agent, "enabled": false });
        for key in ["id", "name", "prompt", "repeat", "time", "days", "date"] {
            if !fields[key].is_null() {
                changes[key] = fields[key].clone();
            }
        }
        self.save_schedule_by(&changes, Some(agent))
    }

    fn save_schedule_by(
        &self,
        changes: &Value,
        by_agent: Option<&str>,
    ) -> Result<Schedule, String> {
        let mut list = lock(&self.schedules);
        let (mut schedule, was_on) = match changes["id"].as_str() {
            Some(id) => list
                .iter()
                .find(|schedule| schedule.id == id)
                .filter(|schedule| by_agent.is_none_or(|agent| schedule.agent == agent))
                .map(|schedule| (schedule.clone(), schedule.enabled))
                .ok_or_else(|| format!("unknown schedule {id}"))?,
            None => {
                let agent = changes["agent"].as_str().unwrap_or_default();
                if self.agents.get(agent).is_none() {
                    return Err(format!("unknown agent '{agent}'"));
                }
                let id = random_hex(8).map_err(|error| error.to_string())?;
                let mut schedule = Schedule::new(format!("sch-{id}"), agent.to_string(), seconds());
                schedule.by_agent = by_agent.is_some();
                (schedule, false)
            }
        };
        let had_worktree = schedule.worktree;
        schedule.apply(changes)?;
        if schedule.worktree && !had_worktree {
            let agent = self
                .agents
                .get(&schedule.agent)
                .ok_or_else(|| format!("unknown agent '{}'", schedule.agent))?;
            if !crate::worktrees::in_repository(&agent.cwd) {
                return Err(format!(
                    "{} 不在 git 仓库里，不能在独立 worktree 中运行",
                    agent.cwd.display()
                ));
            }
        }
        // Switched on again: its failures so far no longer count.
        if schedule.enabled && !was_on {
            schedule.failures = 0;
        }
        schedule.next_run_at = next_run_at(&schedule, seconds());
        let mut updated = list.clone();
        match updated.iter_mut().find(|other| other.id == schedule.id) {
            Some(slot) => *slot = schedule.clone(),
            None => updated.push(schedule.clone()),
        }
        self.persist_schedules(&updated, &schedule.agent)?;
        *list = updated;
        drop(list);
        self.broadcast(&self.schedules_json(None));
        Ok(schedule)
    }

    pub fn delete_schedule(&self, id: &str) -> Result<(), String> {
        self.delete_schedule_by(id, None)
    }

    /// Deletes a schedule; `by_agent` may delete only its own. Its runs'
    /// sessions stay.
    pub fn delete_schedule_by(&self, id: &str, by_agent: Option<&str>) -> Result<(), String> {
        let mut list = lock(&self.schedules);
        let agent = list
            .iter()
            .find(|schedule| schedule.id == id)
            .filter(|schedule| by_agent.is_none_or(|agent| schedule.agent == agent))
            .map(|schedule| schedule.agent.clone())
            .ok_or_else(|| format!("unknown schedule {id}"))?;
        let mut updated = list.clone();
        updated.retain(|schedule| schedule.id != id);
        self.persist_schedules(&updated, &agent)?;
        *list = updated;
        drop(list);
        self.broadcast(&self.schedules_json(None));
        Ok(())
    }

    /// Runs a schedule now, enabled or not, without moving its next run.
    /// Refused while its last run is not over.
    pub fn run_schedule(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let at = seconds();
        let prepare = {
            let mut list = lock(&self.schedules);
            let schedule = list
                .iter_mut()
                .find(|schedule| schedule.id == id)
                .ok_or_else(|| format!("unknown schedule {id}"))?;
            match self.agents.get(&schedule.agent) {
                None => return Err(format!("unknown agent {}", schedule.agent)),
                Some(agent) if !agent.enabled => {
                    return Err(format!("agent {} is disabled", agent.id))
                }
                Some(_) => {}
            }
            if schedule.running() {
                return Err("上一次运行还没结束".to_string());
            }
            let key = format!("schedule:{id}:run:{at}:{}", self.new_id("r"));
            let prepare = self.start_run(schedule, key, at)?;
            let agent = schedule.agent.clone();
            self.persist_schedules(&list, &agent)?;
            prepare
        };
        self.broadcast(&self.schedules_json(None));
        if let Some(prepare) = prepare {
            self.prepare_worktree(prepare);
        }
        self.deliver_pending();
        Ok(())
    }

    /// Turns due schedules of enabled agents into runs. A time that comes
    /// while the last run is not over is skipped (told once in a row). The
    /// message's dedupe key names the due time, so a schedule that fired
    /// just before a crash fires only once.
    pub(crate) fn fire_due_schedules(self: &Arc<Self>) {
        let at = seconds();
        let mut list = lock(&self.schedules);
        let mut fired: Vec<String> = Vec::new();
        let mut skipped: Vec<Schedule> = Vec::new();
        let mut prepare = Vec::new();
        for schedule in list.iter_mut() {
            let Some(due) = schedule.next_run_at.filter(|due| *due <= at) else {
                continue;
            };
            if !schedule.enabled
                || !self
                    .agents
                    .get(&schedule.agent)
                    .is_some_and(|agent| agent.enabled)
            {
                continue;
            }
            if schedule.running() {
                schedule.skipped += 1;
                if schedule.skipped == 1 {
                    skipped.push(schedule.clone());
                }
            } else {
                let key = format!("schedule:{}:{due}", schedule.id);
                match self.start_run(schedule, key, at) {
                    Ok(worktree) => prepare.extend(worktree),
                    Err(error) => {
                        lynshen_agent_core::log_warn!(
                            "daemon",
                            "schedule not fired",
                            error = error
                        );
                        continue;
                    }
                }
            }
            schedule.next_run_at = next_run_at(schedule, at);
            if !fired.contains(&schedule.agent) {
                fired.push(schedule.agent.clone());
            }
        }
        if fired.is_empty() {
            return;
        }
        for agent in &fired {
            if let Err(error) = self.persist_schedules(&list, agent) {
                lynshen_agent_core::log_warn!("daemon", "schedules not saved", error = error);
            }
        }
        drop(list);
        self.broadcast(&self.schedules_json(None));
        for schedule in skipped {
            self.tell(&schedule, "skipped", "上一次运行还没结束，这次跳过。", true);
        }
        for run in prepare {
            self.prepare_worktree(run);
        }
    }

    /// Records the message for one run of `schedule` and makes that run its
    /// latest: into the last run's session when the schedule continues it
    /// and the agent still owns it, else a new one told what the last run
    /// concluded. Some(…) when the run's new session needs its worktree
    /// made first (`prepare_worktree`); the message waits for it.
    fn start_run(
        &self,
        schedule: &mut Schedule,
        dedupe_key: String,
        at: u64,
    ) -> Result<Option<Prepare>, String> {
        let session = schedule
            .last_session
            .clone()
            .filter(|_| !schedule.new_session)
            .filter(|session| {
                self.store.sessions().iter().any(|record| {
                    record.id == *session && record.agent.as_deref() == Some(&schedule.agent)
                })
            });
        let mut body = format!("定时任务「{}」：\n{}", schedule.name, schedule.prompt);
        if session.is_none() {
            let last = schedule
                .last_session
                .as_deref()
                .and_then(|last| self.agents.handoff(&schedule.agent, last));
            if let (Some(note), Some(at)) = (last, schedule.last_run_at) {
                let at = Local
                    .timestamp_opt(at as i64, 0)
                    .single()
                    .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                body.push_str(&format!("\n\n上次运行（{at}）的交接：\n{note}"));
            }
        }
        let new_session = session.is_none();
        let message = Message {
            id: self.new_id("m"),
            to: schedule.agent.clone(),
            from: format!("schedule:{}", schedule.id),
            body,
            session,
            reply_to: None,
            dedupe_key: Some(dedupe_key),
            at: now(),
        };
        let recorded = self
            .store
            .record_message(&message)
            .map_err(|error| error.to_string())?;
        // Fired for this time just before the daemon stopped: once only.
        if !recorded {
            return Ok(None);
        }
        if let Some(previous) = schedule.last_run.take() {
            schedule.history.insert(0, previous);
            schedule.history.truncate(HISTORY);
        }
        let mut run = Run::new(at);
        run.message = Some(message.id.clone());
        schedule.last_run_at = Some(at);
        schedule.skipped = 0;
        let prepare = (schedule.worktree && new_session).then(|| {
            run.status = "preparing".to_string();
            Prepare {
                schedule: schedule.id.clone(),
                agent: schedule.agent.clone(),
                message: message.id.clone(),
                slug: format!(
                    "{}-{}",
                    crate::worktrees::slug(&schedule.id),
                    Local::now().format("%m%d-%H%M")
                ),
            }
        });
        schedule.last_run = Some(run);
        Ok(prepare)
    }

    /// Makes the worktree a run's new session starts in, off this thread
    /// (git, submodules), then lets its message go. A run whose worktree
    /// cannot be made fails, and its message is not delivered.
    fn prepare_worktree(&self, prepare: Prepare) {
        let Some(hub) = self.handle() else { return };
        std::thread::spawn(move || {
            let made = hub
                .agents
                .get(&prepare.agent)
                .ok_or_else(|| format!("unknown agent {}", prepare.agent))
                .and_then(|agent| crate::worktrees::create(&agent.cwd, &prepare.slug));
            match made {
                Ok((tree, inside)) => {
                    let kept = hub.update_run(&prepare.schedule, &prepare.message, |run| {
                        run.status = "running".to_string();
                        run.worktree = Some(tree.clone());
                        run.cwd = Some(inside.display().to_string());
                    });
                    // Deleted meanwhile: nothing will run there.
                    if kept.is_none() {
                        crate::worktrees::remove_if_unused(&tree);
                    }
                    hub.deliver_pending();
                }
                Err(error) => {
                    let _ = hub.store.record_undeliverable(&prepare.message, &error);
                    hub.fail_run(&prepare.schedule, None, &error, false);
                }
            }
        });
    }

    /// A message of a scheduled run whose worktree is still being made:
    /// it waits (`deliver_pending`).
    pub(crate) fn run_preparing(&self, message: &Message) -> bool {
        let Some(id) = message.from.strip_prefix("schedule:") else {
            return false;
        };
        lock(&self.schedules).iter().any(|s| {
            s.id == id
                && s.last_run.as_ref().is_some_and(|run| {
                    run.status == "preparing" && run.message.as_deref() == Some(&message.id)
                })
        })
    }

    /// Where a scheduled run's new session works: its worktree, when it has
    /// one; None: the agent's folder.
    pub(crate) fn run_cwd(&self, message: &Message) -> Option<PathBuf> {
        let id = message.from.strip_prefix("schedule:")?;
        lock(&self.schedules)
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.last_run.as_ref())
            .filter(|run| run.message.as_deref() == Some(&message.id))
            .and_then(|run| run.cwd.as_ref().map(PathBuf::from))
    }

    /// The message of a run that could not be delivered: the run fails.
    pub(crate) fn run_undeliverable(&self, message: &Message, reason: &str) {
        let Some(id) = message.from.strip_prefix("schedule:") else {
            return;
        };
        let ours = lock(&self.schedules).iter().any(|s| {
            s.id == id
                && s.last_run
                    .as_ref()
                    .is_some_and(|run| run.message.as_deref() == Some(&message.id))
        });
        if ours {
            self.fail_run(id, None, reason, false);
        }
    }

    /// Message `message` of schedule `id` reached `session`: later runs that
    /// continue the last session go there, the run is in it, and a run in a
    /// new session retires the earlier ones (see `retire_schedule_runs`).
    pub(crate) fn schedule_delivered(&self, id: &str, session: &str, message: &str) {
        let mut list = lock(&self.schedules);
        let Some(schedule) = list.iter_mut().find(|schedule| schedule.id == id) else {
            return;
        };
        let mut tree = None;
        if let Some(run) = schedule
            .last_run
            .as_mut()
            .filter(|run| run.message.as_deref() == Some(message))
        {
            run.session = Some(session.to_string());
            tree = run.worktree.clone();
        }
        let moved = schedule.last_session.as_deref() != Some(session);
        schedule.last_session = Some(session.to_string());
        let (agent, name) = (schedule.agent.clone(), schedule.name.clone());
        if let Err(error) = self.persist_schedules(&list, &agent) {
            lynshen_agent_core::log_warn!("daemon", "schedules not saved", error = error);
        }
        drop(list);
        if let Some(tree) = tree {
            let _ = self
                .store
                .record_session_meta(session, &json!({ "worktree": tree }));
            self.broadcast(&self.sessions_json());
        }
        self.broadcast(&self.schedules_json(None));
        if moved {
            self.retire_schedule_runs(id, &name, session);
        }
    }

    /// A session event: how the scheduled run in `session`, if one is, is
    /// going. `over`: a turn ended, `failed` with this error.
    pub(crate) fn observe_run(
        &self,
        session: &str,
        event: &Value,
        over: bool,
        failed: Option<&str>,
    ) {
        let kind = event["type"].as_str().unwrap_or_default();
        if kind == "user_message" {
            return self.run_message(session, event["content"].as_str().unwrap_or_default());
        }
        let waiting = matches!(kind, "approval_request" | "action_deferred");
        let going = matches!(kind, "action_decided" | "assistant_delta" | "tool_start");
        if !(waiting || going || over) {
            // The engine is ready: a run on background work may be over.
            if kind == "status" && event["message"] == "ready" {
                self.run_activity(session);
            }
            return;
        }
        let Some((id, before)) = lock(&self.schedules).iter().find_map(|s| {
            let run = s.last_run.as_ref()?;
            (run.session.as_deref() == Some(session)
                && run.started
                && run.going()
                && !run.followed_up)
                .then(|| (s.id.clone(), run.status.clone()))
        }) else {
            return;
        };
        if let Some(error) = failed.filter(|_| over) {
            // The failed turn is an agent report with its own notice.
            return self.fail_run(&id, Some(session), error, true);
        }
        // A subagent's output (`nested`) is not the run working again (a
        // background one goes on after the turn), unless it answers the
        // approval the run waited on.
        if going && event["nested"] == true && before != "waiting" {
            return;
        }
        let deferred = || {
            self.store
                .open_actions()
                .iter()
                .any(|action| action.session_id == session)
        };
        let status = if waiting || (over && deferred()) {
            "waiting"
        } else if over && self.run_waits(session) {
            "background"
        } else if over {
            "done"
        } else {
            "running"
        };
        if status == before {
            return;
        }
        let changed = self.update_schedule(&id, |s| {
            if let Some(run) = s.last_run.as_mut() {
                run.status = status.to_string();
                if status == "done" {
                    run.ended_at = Some(seconds());
                    s.failures = 0;
                }
            }
        });
        if let (Some(schedule), "waiting") = (changed, status) {
            // The approval has its own notice.
            self.tell(&schedule, "waiting", "运行在等你审批。", false);
        }
    }

    /// A message started a turn in `session`: the run that sent it starts
    /// there; any other message in a run that is over is the user's own
    /// (the run stays done, and nothing after it is the run's).
    fn run_message(&self, session: &str, content: &str) {
        let mut list = lock(&self.schedules);
        let mut changed: Vec<String> = Vec::new();
        for schedule in list.iter_mut() {
            let Some(run) = schedule.last_run.as_mut() else {
                continue;
            };
            let ours = run
                .message
                .as_deref()
                .is_some_and(|message| names_message(content, message));
            if ours && !run.started && run.going() {
                run.started = true;
                run.session = Some(session.to_string());
                changed.push(schedule.agent.clone());
            } else if !ours
                && run.session.as_deref() == Some(session)
                && matches!(run.status.as_str(), "done" | "background")
                && !run.followed_up
            {
                run.followed_up = true;
                if run.status == "background" {
                    run.status = "done".to_string();
                    run.ended_at = Some(seconds());
                }
                changed.push(schedule.agent.clone());
            }
        }
        if changed.is_empty() {
            return;
        }
        changed.dedup();
        for agent in &changed {
            if let Err(error) = self.persist_schedules(&list, agent) {
                lynshen_agent_core::log_warn!("daemon", "schedules not saved", error = error);
            }
        }
        drop(list);
        self.broadcast(&self.schedules_json(None));
    }

    /// Background work in `session` started or ended (`set_background`): a
    /// run whose turn ended waits on it (`background`), and is done once it
    /// ends with no turn going.
    pub(crate) fn run_activity(&self, session: &str) {
        let Some((id, before)) = lock(&self.schedules).iter().find_map(|s| {
            let run = s.last_run.as_ref()?;
            (run.session.as_deref() == Some(session)
                && run.started
                && matches!(run.status.as_str(), "done" | "background")
                && !run.followed_up)
                .then(|| (s.id.clone(), run.status.clone()))
        }) else {
            return;
        };
        let status = match (before.as_str(), self.run_waits(session)) {
            ("done", true) => "background",
            ("background", false) if !self.is_busy(session) => "done",
            _ => return,
        };
        // Only from the status read above: a session thread may have moved
        // it meanwhile.
        self.update_schedule(&id, |s| {
            if let Some(run) = s.last_run.as_mut().filter(|run| run.status == before) {
                run.status = status.to_string();
                if status == "done" {
                    run.ended_at = Some(seconds());
                    s.failures = 0;
                } else {
                    run.ended_at = None;
                }
            }
        });
    }

    /// Work the run in `session` left going comes back to it: background
    /// agents, or a session it started still working.
    fn run_waits(&self, session: &str) -> bool {
        self.is_awaited(session)
            || self
                .with_children(vec![session.to_string()])
                .iter()
                .skip(1)
                .any(|child| self.is_busy(child) || self.is_awaited(child))
    }

    /// `sessions` and the sessions they started (`start_session`), and the
    /// ones those started: what their work costs.
    pub(crate) fn with_children(&self, sessions: Vec<String>) -> Vec<String> {
        let records = self.store.sessions();
        let mut all = sessions;
        let mut from = 0;
        // Messages between conversations go at most 4 hops.
        for _ in 0..4 {
            let found: Vec<String> = records
                .iter()
                .filter(|r| {
                    r.parent.as_ref().is_some_and(|p| all[from..].contains(p))
                        && !all.contains(&r.id)
                })
                .map(|r| r.id.clone())
                .collect();
            if found.is_empty() {
                break;
            }
            from = all.len();
            all.extend(found);
        }
        all
    }

    /// Stops what `session` is doing: its turn, and the background work it
    /// left going (LynShen subagents and shells, Claude Code's tasks).
    fn stop_work(&self, session: &str) {
        if self.is_busy(session) {
            let _ = self.forward(session, json!({ "op": "interrupt" }));
        }
        // LynShen and Claude Code stop their background work on request.
        let engine = self
            .store
            .sessions()
            .into_iter()
            .find(|r| r.id == session)
            .and_then(|r| r.engine);
        if self.is_background(session) && engine.as_deref().is_none_or(|e| e == "claude") {
            let _ = self.forward(session, json!({ "op": "stop_background" }));
        }
    }

    /// The engine of `session` stopped (closed, or it died): a scheduled run
    /// still going there did not finish, and an archived run's worktree,
    /// kept while the engine was open (`retire_schedule_runs`), goes if
    /// nothing is in it.
    pub(crate) fn run_session_ended(&self, session: &str) {
        let tree = self
            .store
            .sessions()
            .into_iter()
            .find(|r| r.id == session && r.archived)
            .and_then(|r| r.worktree)
            .filter(|tree| tree["gone"] != true);
        if let Some(tree) = tree {
            self.drop_worktree(session, tree);
        }
        let id = lock(&self.schedules).iter().find_map(|s| {
            let run = s.last_run.as_ref()?;
            (run.session.as_deref() == Some(session)
                && matches!(run.status.as_str(), "running" | "background"))
            .then(|| s.id.clone())
        });
        if let Some(id) = id {
            self.fail_run(
                &id,
                Some(session),
                "会话在运行中关闭了，这次运行没有完成。",
                false,
            );
        }
    }

    /// Removes `session`'s worktree `tree`, off this thread, if nothing is
    /// in it; the session then lists it with `gone: true`.
    pub(crate) fn drop_worktree(&self, session: &str, mut tree: Value) {
        let Some(hub) = self.handle() else { return };
        let session = session.to_string();
        std::thread::spawn(move || {
            if crate::worktrees::remove_if_unused(&tree) {
                tree["gone"] = json!(true);
                let _ = hub
                    .store
                    .record_session_meta(&session, &json!({ "worktree": tree }));
                hub.broadcast(&hub.sessions_json());
            }
        });
    }

    /// At start: the worktrees of archived runs whose engines the last
    /// daemon stopped before they closed go, if nothing is in them.
    pub(crate) fn clean_run_worktrees(&self) {
        for record in self.store.sessions() {
            let tree = record.worktree.filter(|tree| tree["gone"] != true);
            if let (true, Some(tree)) = (record.archived, tree) {
                self.drop_worktree(&record.id, tree);
            }
        }
    }

    /// The run of schedule `id` (in `session`, when given) failed with
    /// `error`: recorded, and told; the phones hear of the first failure in
    /// a row unless `reported` (a failed turn is a report with its own
    /// notice). The `MAX_FAILURES`th failure in a row switches the task
    /// off, which is told too. A switched-off task run by hand counts no
    /// failures.
    pub(crate) fn fail_run(&self, id: &str, session: Option<&str>, error: &str, reported: bool) {
        let mut stopped = false;
        let mut changed = false;
        let failed = self.update_schedule(id, |s| {
            let enabled = s.enabled;
            let Some(run) = s
                .last_run
                .as_mut()
                .filter(|run| session.is_none_or(|session| run.session.as_deref() == Some(session)))
                .filter(|run| run.going() || run.status == "preparing")
            else {
                return;
            };
            run.status = "failed".to_string();
            run.error = Some(error.to_string());
            run.ended_at = Some(seconds());
            changed = true;
            if enabled {
                s.failures += 1;
                if s.failures >= MAX_FAILURES {
                    s.enabled = false;
                    s.next_run_at = None;
                    stopped = true;
                }
            }
        });
        match failed {
            Some(schedule) if stopped => {
                let detail = format!(
                    "连续失败 {} 次，已自动停用。最后一次：{error}",
                    schedule.failures
                );
                self.tell(&schedule, "stopped", &detail, true);
            }
            // Told the first time in a row (a switched-off task run by hand
            // counts none, so every time).
            Some(schedule) if changed && schedule.failures <= 1 => {
                self.tell(&schedule, "failed", error, !reported)
            }
            _ => {}
        }
    }

    /// Once a `sweep_every`: runs past their time stop, runs on background
    /// work settle, and runs with a budget are checked against it.
    pub(crate) fn sweep_schedule_runs(self: &Arc<Self>) {
        static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
        {
            let mut last = lock(&LAST);
            if last.is_some_and(|at| at.elapsed() < SWEEP_EVERY) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        self.time_out_runs(seconds());
        self.settle_runs();
        self.check_budgets();
    }

    /// Runs waiting on background work settle, should a change (a child's
    /// turn ending) have come by no other way.
    fn settle_runs(&self) {
        let waiting: Vec<String> = lock(&self.schedules)
            .iter()
            .filter_map(|s| s.last_run.as_ref())
            .filter(|run| run.status == "background")
            .filter_map(|run| run.session.clone())
            .collect();
        for session in waiting {
            self.run_activity(&session);
        }
    }

    /// Runs going past the run timeout, working, waiting or on background
    /// work, fail and are stopped, so later times run again.
    pub(crate) fn time_out_runs(&self, at: u64) {
        let over: Vec<(String, Option<String>, Option<String>)> = lock(&self.schedules)
            .iter()
            .filter_map(|schedule| {
                let run = schedule.last_run.as_ref()?;
                (run.going() && at.saturating_sub(run.at) > RUN_TIMEOUT_SECS).then(|| {
                    (
                        schedule.id.clone(),
                        run.session.clone(),
                        run.message.clone(),
                    )
                })
            })
            .collect();
        for (id, session, message) in over {
            let error = format!("运行超过 {} 小时，已停止。", RUN_TIMEOUT_SECS / 3600);
            // Never delivered (no free run slot all this time): it no longer goes.
            if let (None, Some(message)) = (&session, &message) {
                let _ = self.store.record_undeliverable(message, &error);
            }
            self.fail_run(&id, session.as_deref(), &error, false);
            if let Some(session) = session {
                for session in self.with_children(vec![session]) {
                    self.stop_work(&session);
                }
            }
        }
    }

    /// Runs with a budget are checked, off this thread (gateway charges
    /// come from the account's ledger over the network); one check at a
    /// time.
    fn check_budgets(self: &Arc<Self>) {
        let any = lock(&self.schedules)
            .iter()
            .any(|s| s.budget.is_some() && s.running());
        if !any
            || self
                .budget_check
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let hub = Arc::clone(self);
        std::thread::spawn(move || {
            hub.check_budgets_now();
            hub.budget_check
                .store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// A run with a budget that has cost more, with the sessions it
    /// started, fails and is stopped. Costs are `schedule_usage`'s: settled
    /// gateway charges show a little late.
    pub(crate) fn check_budgets_now(&self) {
        let runs: Vec<(String, String, f64)> = lock(&self.schedules)
            .iter()
            .filter(|schedule| schedule.running())
            .filter_map(|schedule| {
                let session = schedule.last_run.as_ref()?.session.clone()?;
                Some((schedule.id.clone(), session, schedule.budget?))
            })
            .collect();
        for (id, session, budget) in runs {
            let sessions = self.with_children(vec![session.clone()]);
            let usage = self.usage.sessions_json(&sessions);
            if let Some((cost, unit)) = over_budget(&usage, &session, budget) {
                let error = format!("花费 {cost:.4} {unit}，超过上限 {budget}，已停止。");
                self.fail_run(&id, Some(&session), &error, false);
                for session in &sessions {
                    self.stop_work(session);
                }
            }
        }
    }

    /// At start: a run that was working when the daemon stopped did not
    /// finish; one waiting on a deferred action goes on, as does one whose
    /// message has not reached a session yet (it is delivered now).
    pub(crate) fn interrupt_schedule_runs(&self) {
        let deferred: Vec<String> = self
            .store
            .open_actions()
            .into_iter()
            .map(|action| action.session_id)
            .collect();
        let pending: Vec<String> = self
            .store
            .pending_messages()
            .into_iter()
            .map(|message| message.id)
            .collect();
        let mut list = lock(&self.schedules);
        let mut agents: Vec<String> = Vec::new();
        for schedule in list.iter_mut() {
            let Some(run) = schedule.last_run.as_mut() else {
                continue;
            };
            let waits_on_action = run.status == "waiting"
                && run.session.as_ref().is_some_and(|s| deferred.contains(s));
            let undelivered = run.status == "running"
                && run.session.is_none()
                && run.message.as_ref().is_some_and(|m| pending.contains(m));
            if run.status == "preparing" {
                if let Some(message) = &run.message {
                    let _ = self
                        .store
                        .record_undeliverable(message, "the daemon stopped first");
                }
            } else if !run.going() || waits_on_action || undelivered {
                continue;
            }
            run.status = "failed".to_string();
            run.error = Some("后台服务重启，这次运行没有完成。".to_string());
            run.ended_at = Some(seconds());
            if !agents.contains(&schedule.agent) {
                agents.push(schedule.agent.clone());
            }
        }
        for agent in agents {
            if let Err(error) = self.persist_schedules(&list, &agent) {
                lynshen_agent_core::log_warn!("daemon", "schedules not saved", error = error);
            }
        }
        // Runs still to be delivered keep the computer awake from now.
        self.sync_awake(&list);
    }

    /// Tells the clients about a run of `schedule` (`schedule_event`):
    /// `failed`, `stopped` (failed too often, switched off), `waiting` or
    /// `skipped`; with `push`, the paired phones too.
    fn tell(&self, schedule: &Schedule, kind: &str, detail: &str, push: bool) {
        let session = schedule
            .last_run
            .as_ref()
            .and_then(|run| run.session.clone())
            .unwrap_or_default();
        self.broadcast(&json!({
            "type": "schedule_event",
            "schedule": schedule.id,
            "agent": schedule.agent,
            "name": schedule.name,
            "kind": kind,
            "detail": detail,
            "session": session,
        }));
        if !push {
            return;
        }
        let head = match kind {
            "failed" => "定时任务失败",
            "stopped" => "定时任务已停用",
            _ => "定时任务跳过",
        };
        let title = format!("{head}：{}", schedule.name);
        let tag = format!("schedule:{}", schedule.id);
        self.notify_with(crate::push::Notice {
            kind: if kind == "skipped" {
                crate::push::Kind::Update
            } else {
                crate::push::Kind::Error
            },
            level: if kind == "skipped" {
                crate::push::Level::Normal
            } else {
                crate::push::Level::Urgent
            },
            tag: &tag,
            title: &title,
            body: detail,
            url: None,
            session: Some(session.as_str()).filter(|session| !session.is_empty()),
            answer: None,
        });
    }

    /// Applies `change` to the run of schedule `id` that sent `message`;
    /// saves and broadcasts. None when there is no such run (any more).
    fn update_run(&self, id: &str, message: &str, change: impl FnOnce(&mut Run)) -> Option<()> {
        let mut found = false;
        self.update_schedule(id, |s| {
            if let Some(run) = s
                .last_run
                .as_mut()
                .filter(|run| run.message.as_deref() == Some(message))
            {
                change(run);
                found = true;
            }
        });
        found.then_some(())
    }

    /// Applies `change` to schedule `id`, saves and broadcasts; returns it
    /// as changed.
    fn update_schedule(&self, id: &str, change: impl FnOnce(&mut Schedule)) -> Option<Schedule> {
        let mut list = lock(&self.schedules);
        let schedule = list.iter_mut().find(|schedule| schedule.id == id)?;
        change(schedule);
        let schedule = schedule.clone();
        if let Err(error) = self.persist_schedules(&list, &schedule.agent) {
            lynshen_agent_core::log_warn!("daemon", "schedules not saved", error = error);
        }
        drop(list);
        self.broadcast(&self.schedules_json(None));
        Some(schedule)
    }

    /// The schedule each run session belongs to (its latest and kept runs).
    pub(crate) fn run_sessions(&self) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for schedule in lock(&self.schedules).iter() {
            for run in schedule.last_run.iter().chain(&schedule.history) {
                if let Some(session) = &run.session {
                    map.entry(session.clone())
                        .or_insert_with(|| schedule.id.clone());
                }
            }
        }
        map
    }

    /// The computer stays awake while any run works (not while one only
    /// waits for an approval).
    pub(crate) fn sync_awake(&self, list: &[Schedule]) {
        self.awake.sync(
            "schedule",
            list.iter()
                .filter(|s| {
                    s.last_run.as_ref().is_some_and(|run| {
                        matches!(run.status.as_str(), "running" | "background" | "preparing")
                    })
                })
                .map(|s| s.id.clone()),
        );
    }

    /// Writes `agent`'s schedules from `list`.
    fn persist_schedules(&self, list: &[Schedule], agent: &str) -> Result<(), String> {
        self.sync_awake(list);
        let own: Vec<Value> = list
            .iter()
            .filter(|schedule| schedule.agent == agent)
            .map(Schedule::to_json)
            .collect();
        self.agents.save_schedules(agent, &own)
    }
}

/// A run whose new session waits for its worktree (`prepare_worktree`).
struct Prepare {
    schedule: String,
    agent: String,
    message: String,
    slug: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    /// Beijing time; 2026-10-01 is a Thursday.
    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_str(&format!("{text} +0800"), "%Y-%m-%d %H:%M %z").unwrap()
    }

    fn schedule(changes: Value) -> Schedule {
        let mut schedule = Schedule::new("sch-1".to_string(), "ops".to_string(), 0);
        let mut fields = json!({ "name": "n", "prompt": "p" });
        fields
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        schedule.apply(&fields).unwrap();
        schedule
    }

    fn next(changes: Value, now: &str) -> Option<DateTime<FixedOffset>> {
        next_run(&schedule(changes), &at(now))
    }

    #[test]
    fn a_schedule_keeps_the_proposal_it_came_from() {
        let made = schedule(json!({ "repeat": "daily", "time": "09:00", "origin": "prop-1a2b" }));
        assert_eq!(made.origin.as_deref(), Some("prop-1a2b"));
        let saved = Schedule::from_json(&made.to_json()).unwrap();
        assert_eq!(saved.origin.as_deref(), Some("prop-1a2b"));
        assert_eq!(
            schedule(json!({ "repeat": "daily", "time": "09:00" })).origin,
            None
        );
    }

    #[test]
    fn once_runs_at_its_date_and_never_after() {
        let once = json!({ "repeat": "once", "date": "2026-10-02", "time": "11:00" });
        assert_eq!(
            next(once.clone(), "2026-10-01 12:00"),
            Some(at("2026-10-02 11:00"))
        );
        assert_eq!(next(once.clone(), "2026-10-02 11:00"), None);
        assert_eq!(next(once, "2026-10-03 09:00"), None);
    }

    #[test]
    fn hourly_runs_at_the_minute_of_the_next_hour() {
        let hourly = json!({ "repeat": "hourly", "time": "00:15" });
        assert_eq!(
            next(hourly.clone(), "2026-10-01 10:05"),
            Some(at("2026-10-01 10:15"))
        );
        assert_eq!(
            next(hourly.clone(), "2026-10-01 10:15"),
            Some(at("2026-10-01 11:15"))
        );
        assert_eq!(
            next(hourly, "2026-10-01 23:30"),
            Some(at("2026-10-02 00:15"))
        );
    }

    #[test]
    fn daily_runs_today_or_tomorrow() {
        let daily = json!({ "repeat": "daily", "time": "11:00" });
        assert_eq!(
            next(daily.clone(), "2026-10-01 09:00"),
            Some(at("2026-10-01 11:00"))
        );
        assert_eq!(
            next(daily, "2026-10-01 11:00"),
            Some(at("2026-10-02 11:00"))
        );
    }

    #[test]
    fn weekdays_skip_the_weekend() {
        let weekdays = json!({ "repeat": "weekdays", "time": "09:30" });
        // Friday after the run: next Monday.
        assert_eq!(
            next(weekdays.clone(), "2026-10-02 10:00"),
            Some(at("2026-10-05 09:30"))
        );
        assert_eq!(
            next(weekdays, "2026-10-04 08:00"),
            Some(at("2026-10-05 09:30"))
        );
    }

    #[test]
    fn weekly_wraps_to_next_week() {
        let weekly = json!({ "repeat": "weekly", "time": "08:00", "days": [1, 3] });
        // Thursday: next Monday.
        assert_eq!(
            next(weekly.clone(), "2026-10-01 07:00"),
            Some(at("2026-10-05 08:00"))
        );
        // Wednesday after the run: the next Monday.
        assert_eq!(
            next(weekly.clone(), "2026-10-07 08:00"),
            Some(at("2026-10-12 08:00"))
        );
        let sunday = json!({ "repeat": "weekly", "time": "08:00", "days": [0] });
        assert_eq!(
            next(sunday.clone(), "2026-10-04 07:59"),
            Some(at("2026-10-04 08:00"))
        );
        assert_eq!(
            next(sunday, "2026-10-04 08:00"),
            Some(at("2026-10-11 08:00"))
        );
    }

    #[test]
    fn a_disabled_schedule_has_no_next_run() {
        assert_eq!(
            next(
                json!({ "repeat": "daily", "time": "11:00", "enabled": false }),
                "2026-10-01 09:00"
            ),
            None
        );
    }

    #[test]
    fn invalid_schedules_are_refused() {
        for bad in [
            json!({ "name": "", "prompt": "p", "repeat": "daily", "time": "11:00" }),
            json!({ "name": "n", "prompt": " ", "repeat": "daily", "time": "11:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "monthly", "time": "11:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "daily", "time": "25:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "weekly", "time": "11:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "weekly", "time": "11:00", "days": [7] }),
            json!({ "name": "n", "prompt": "p", "repeat": "once", "time": "11:00" }),
            json!({ "name": "n", "prompt": "p", "repeat": "once", "time": "11:00", "date": "2026-02-30" }),
        ] {
            let mut schedule = Schedule::new("sch-1".to_string(), "ops".to_string(), 0);
            assert!(schedule.apply(&bad).is_err(), "{bad}");
        }
    }

    // --- runs: the hub with a fake engine (a session hosted on a channel) ---

    use crate::{store::Store, Agents};
    use std::sync::mpsc;

    struct Fixture {
        hub: Arc<Hub>,
        ops: mpsc::Receiver<Value>,
        id: String,
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A hub with agent `ops`, its session `s1` hosted on a channel, and a
    /// task of it that continues `s1`, so no engine is started.
    fn fixture(label: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-schedule-{label}-{}-{}",
            std::process::id(),
            now()
        ));
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let agents = Agents::open(dir.join("agents")).unwrap();
        agents
            .create("ops", "Ops", &work, "role", &Value::Null)
            .unwrap();
        let hub = Hub::new(
            Store::open(dir.join("daemon")).unwrap(),
            agents,
            "test",
            None,
        );
        hub.store
            .record_engine_session("s1", &work, Some("ops"), None, false, None)
            .unwrap();
        let (tx, ops) = mpsc::channel();
        hub.host("s1".to_string(), tx, work, hub.next_generation());
        let saved = hub
            .save_schedule(&json!({
                "agent": "ops", "name": "巡检", "prompt": "check", "repeat": "daily",
                "time": "09:00", "new_session": false,
            }))
            .unwrap();
        lock(&hub.schedules)[0].last_session = Some("s1".to_string());
        Fixture {
            hub,
            ops,
            id: saved.id,
            dir,
        }
    }

    impl Fixture {
        fn schedule(&self) -> Schedule {
            lock(&self.hub.schedules)
                .iter()
                .find(|s| s.id == self.id)
                .cloned()
                .unwrap()
        }

        fn status(&self) -> String {
            self.schedule().last_run.unwrap().status
        }

        /// Runs the task now; the engine takes its message and starts the
        /// turn. Returns the message the engine got.
        fn start(&self) -> String {
            self.hub.run_schedule(&self.id).unwrap();
            let op = self.ops.try_iter().last().expect("the run's message");
            assert_eq!(op["op"], "user_message");
            let content = op["content"].as_str().unwrap().to_string();
            self.hub.release_claim("s1");
            self.hub
                .observe("s1", &json!({ "type": "user_message", "content": content }));
            content
        }

        /// The turn ends, on `error` when given.
        fn end(&self, error: Option<&str>) {
            if let Some(error) = error {
                self.hub
                    .observe("s1", &json!({ "type": "error", "message": error }));
            } else {
                self.hub
                    .observe("s1", &json!({ "type": "assistant_delta", "delta": "ok" }));
            }
            self.hub.set_busy("s1", false);
            self.hub
                .observe("s1", &json!({ "type": "status", "message": "ready" }));
        }
    }

    #[test]
    fn an_old_store_loads_with_no_runs_and_round_trips_the_new_fields() {
        // As saved before runs were recorded (agents' schedules.json).
        let old = json!({
            "id": "sch-1", "agent": "ops", "name": "巡检", "prompt": "check",
            "enabled": true, "repeat": "daily", "time": "09:00", "days": [], "date": null,
            "new_session": true, "by_agent": false, "created_at": 1,
            "last_run_at": 100, "last_session": "s-old", "next_run_at": 200,
        });
        let loaded = Schedule::from_json(&old).unwrap();
        assert_eq!(loaded.last_run, None);
        assert!(loaded.history.is_empty());
        assert_eq!((loaded.failures, loaded.skipped), (0, 0));
        assert!(!loaded.worktree);
        assert_eq!(loaded.budget, None);
        assert_eq!(loaded.last_run_at, Some(100));
        assert_eq!(loaded.last_session.as_deref(), Some("s-old"));
        assert_eq!(loaded.next_run_at, Some(200));

        let mut newer = loaded.clone();
        newer.worktree = true;
        newer.budget = Some(2.5);
        newer.failures = 2;
        newer.last_run = Some(Run {
            status: "failed".to_string(),
            error: Some("x".to_string()),
            session: Some("s1".to_string()),
            ..Run::new(300)
        });
        newer.history = (0..25).map(Run::new).collect();
        let mut saved = newer.to_json();
        let back = Schedule::from_json(&saved).unwrap();
        assert_eq!(back.history.len(), HISTORY);
        newer.history.truncate(HISTORY);
        assert_eq!(back, newer);
        // A run saved without `started` (none were) counts as started.
        saved["last_run"].as_object_mut().unwrap().remove("started");
        assert!(
            Schedule::from_json(&saved)
                .unwrap()
                .last_run
                .unwrap()
                .started
        );
    }

    #[test]
    fn a_budget_is_above_zero_and_null_clears_it() {
        let mut s = schedule(json!({ "repeat": "daily", "time": "09:00", "budget": 1.5 }));
        assert_eq!(s.budget, Some(1.5));
        for bad in [json!(0), json!(-1), json!("2")] {
            assert!(s.apply(&json!({ "budget": bad })).is_err());
        }
        s.apply(&json!({ "name": "m" })).unwrap();
        assert_eq!(s.budget, Some(1.5), "absent keeps it");
        s.apply(&json!({ "budget": null })).unwrap();
        assert_eq!(s.budget, None);
    }

    #[test]
    fn a_run_goes_from_running_to_done_and_keeps_the_computer_awake_meanwhile() {
        let f = fixture("done");
        let content = f.start();
        assert!(content.contains("定时任务「巡检」"), "{content}");
        let run = f.schedule().last_run.unwrap();
        assert_eq!(run.status, "running");
        assert!(run.started);
        assert_eq!(run.session.as_deref(), Some("s1"));
        assert_eq!(f.hub.awake.keys(), [format!("schedule:{}", f.id)]);
        // Another run is refused while this one goes.
        assert!(f.hub.run_schedule(&f.id).unwrap_err().contains("还没结束"));
        f.end(None);
        let run = f.schedule().last_run.unwrap();
        assert_eq!(run.status, "done");
        assert!(run.ended_at.is_some());
        assert!(f.hub.awake.keys().is_empty(), "released once done");
        // Saved with the agent's tasks.
        let saved = f.hub.agents.schedules("ops");
        assert_eq!(saved[0]["last_run"]["status"], "done");
    }

    #[test]
    fn three_failures_in_a_row_switch_a_task_off_and_switching_it_on_forgives_them() {
        let f = fixture("failures");
        for n in 1..=3 {
            f.start();
            f.end(Some("HTTP 502"));
            let s = f.schedule();
            assert_eq!(s.last_run.as_ref().unwrap().status, "failed");
            assert_eq!(
                s.last_run.as_ref().unwrap().error.as_deref(),
                Some("HTTP 502")
            );
            assert_eq!(s.failures, n);
            assert_eq!(s.enabled, n < 3, "after {n}");
        }
        let s = f.schedule();
        assert_eq!(s.next_run_at, None);
        assert_eq!(s.history.len(), 2);
        assert!(f.hub.awake.keys().is_empty());
        // Run by hand while off: a failure there counts none.
        f.start();
        f.end(Some("HTTP 502"));
        assert_eq!(f.schedule().failures, 3);
        f.hub
            .save_schedule(&json!({ "id": f.id, "enabled": true }))
            .unwrap();
        let s = f.schedule();
        assert!(s.enabled && s.next_run_at.is_some());
        assert_eq!(s.failures, 0);
        // A run that ends well forgives them too.
        f.start();
        f.end(Some("HTTP 502"));
        assert_eq!(f.schedule().failures, 1);
        f.start();
        f.end(None);
        assert_eq!(f.schedule().failures, 0);
    }

    #[test]
    fn twenty_runs_are_kept() {
        let f = fixture("history");
        for _ in 0..(HISTORY + 3) {
            f.start();
            f.end(None);
        }
        let s = f.schedule();
        assert_eq!(s.history.len(), HISTORY);
        assert!(s.history.iter().all(|run| run.status == "done"));
        assert!(s.history.windows(2).all(|pair| pair[0].at >= pair[1].at));
    }

    #[test]
    fn a_run_past_its_budget_is_stopped_and_fails() {
        let f = fixture("budget");
        f.hub
            .save_schedule(&json!({ "id": f.id, "budget": 0.4 }))
            .unwrap();
        f.start();
        // Its turn goes on, and left a background command going.
        f.hub.set_busy("s1", true);
        f.hub.set_background("s1", true, false);
        // A session it started spent too: counted with it.
        f.hub
            .store
            .record_engine_session(
                "child",
                &f.dir.join("work"),
                None,
                Some("claude"),
                false,
                None,
            )
            .unwrap();
        f.hub
            .store
            .record_session_meta("child", &json!({ "parent": "s1" }))
            .unwrap();
        let usage = [
            json!({ "turn_id": "t1", "session": "s1", "channel_kind": "local", "requests": 1, "input_tokens": 10, "estimated_cost_usd": 0.25, "started_at": 1, "ended_at": 2 }),
            json!({ "turn_id": "t2", "session": "child", "channel_kind": "local", "requests": 1, "input_tokens": 10, "estimated_cost_usd": 0.1, "started_at": 1, "ended_at": 2 }),
        ];
        let write = |rows: &[Value]| {
            let lines: String = rows.iter().map(|row| format!("{row}\n")).collect();
            std::fs::write(f.hub.store.dir().join("usage.jsonl"), lines).unwrap();
        };
        write(&usage);
        assert_eq!(f.hub.with_children(vec!["s1".to_string()]), ["s1", "child"]);
        // 0.35 of 0.4: it goes on.
        f.hub.check_budgets_now();
        assert_eq!(f.status(), "running");
        let mut more = usage.to_vec();
        more.push(json!({ "turn_id": "t3", "session": "child", "channel_kind": "local", "requests": 1, "input_tokens": 10, "estimated_cost_usd": 0.1, "started_at": 3, "ended_at": 4 }));
        write(&more);
        f.hub.check_budgets_now();
        let run = f.schedule().last_run.unwrap();
        assert_eq!(run.status, "failed");
        let error = run.error.unwrap();
        assert!(
            error.contains("美元（估算）") && error.contains("0.4"),
            "{error}"
        );
        let sent: Vec<Value> = f.ops.try_iter().collect();
        assert!(sent.iter().any(|op| op["op"] == "interrupt"), "{sent:?}");
        assert!(
            sent.iter().any(|op| op["op"] == "stop_background"),
            "{sent:?}"
        );
        assert!(f.hub.awake.keys().is_empty());
    }

    #[test]
    fn the_budget_is_counted_in_points_when_the_run_went_through_the_gateway() {
        let usage = json!({
            "totals": { "gateway_cost": 12.0, "estimated_cost_usd": 0.5 },
            "sessions": [{ "session": "s1", "gateway_requests": 3 }],
        });
        assert_eq!(over_budget(&usage, "s1", 10.0), Some((12.0, "点")));
        assert_eq!(over_budget(&usage, "s1", 20.0), None);
        let local = json!({
            "totals": { "gateway_cost": null, "estimated_cost_usd": 0.5 },
            "sessions": [{ "session": "s1", "gateway_requests": 0 }],
        });
        assert_eq!(over_budget(&local, "s1", 0.4), Some((0.5, "美元（估算）")));
        // Unknown cost (nothing priced yet) never stops a run.
        let unknown = json!({ "totals": { "gateway_cost": null, "estimated_cost_usd": null }, "sessions": [] });
        assert_eq!(over_budget(&unknown, "s1", 0.0001), None);
    }

    #[test]
    fn a_run_waits_on_background_agents_but_not_on_a_background_command() {
        let f = fixture("background");
        f.start();
        // A dev server left going: the run is done.
        f.hub.set_background("s1", true, false);
        f.end(None);
        assert_eq!(f.status(), "done");
        f.hub.set_background("s1", false, false);

        f.start();
        // A background subagent: the run waits for it, awake.
        f.hub.set_background("s1", true, true);
        f.end(None);
        assert_eq!(f.status(), "background");
        assert_eq!(f.hub.awake.keys(), [format!("schedule:{}", f.id)]);
        // Its result wakes the engine: a turn of its own, the run's again.
        f.hub.observe(
            "s1",
            &json!({ "type": "tool_start", "call_id": "t", "nested": true }),
        );
        assert_eq!(
            f.status(),
            "background",
            "a subagent's work is not the run's turn"
        );
        f.hub.observe(
            "s1",
            &json!({ "type": "assistant_delta", "delta": "result" }),
        );
        assert_eq!(f.status(), "running");
        f.hub.set_background("s1", false, false);
        f.end(None);
        assert_eq!(f.status(), "done");
        assert!(f.hub.awake.keys().is_empty());
    }

    #[test]
    fn the_users_message_after_a_run_is_theirs() {
        let f = fixture("followup");
        f.start();
        f.end(None);
        f.hub.observe(
            "s1",
            &json!({ "type": "user_message", "content": "and now this" }),
        );
        let run = f.schedule().last_run.unwrap();
        assert!(run.followed_up);
        // Their failing turn is not the run's.
        f.end(Some("HTTP 502"));
        let s = f.schedule();
        assert_eq!(s.last_run.unwrap().status, "done");
        assert_eq!(s.failures, 0);
    }

    #[test]
    fn a_run_past_its_time_is_stopped_and_fails() {
        let f = fixture("timeout");
        f.start();
        f.hub.set_busy("s1", true);
        let at = f.schedule().last_run.unwrap().at;
        f.hub.time_out_runs(at + RUN_TIMEOUT_SECS);
        assert_eq!(f.status(), "running");
        f.hub.time_out_runs(at + RUN_TIMEOUT_SECS + 1);
        let run = f.schedule().last_run.unwrap();
        assert_eq!(run.status, "failed");
        assert!(run.error.unwrap().contains("2 小时"));
        assert!(f.ops.try_iter().any(|op| op["op"] == "interrupt"));
    }

    #[test]
    fn a_time_that_comes_while_the_last_run_goes_is_skipped() {
        let f = fixture("skip");
        f.start();
        lock(&f.hub.schedules)[0].next_run_at = Some(1);
        f.hub.fire_due_schedules();
        let s = f.schedule();
        assert_eq!(s.skipped, 1);
        assert_eq!(s.history.len(), 0, "no new run");
        assert!(s.next_run_at.unwrap() > 1);
        f.end(None);
    }

    #[test]
    fn a_worktree_run_outside_git_fails_without_starting() {
        let f = fixture("no-git");
        assert!(f
            .hub
            .save_schedule(&json!({ "id": f.id, "worktree": true }))
            .unwrap_err()
            .contains("不在 git 仓库里"));
        // Its folder stopped being a repository since.
        {
            let mut list = lock(&f.hub.schedules);
            list[0].worktree = true;
            list[0].new_session = true;
        }
        f.hub.run_schedule(&f.id).unwrap();
        let start = std::time::Instant::now();
        while f.status() != "failed" {
            assert!(start.elapsed().as_secs() < 10, "{}", f.status());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let run = f.schedule().last_run.unwrap();
        assert!(run.error.unwrap().contains("不在 git 仓库里"));
        assert!(f.hub.store.pending_messages().is_empty(), "nothing waits");
        assert!(f.ops.try_iter().next().is_none(), "nothing sent");
        assert!(f.hub.awake.keys().is_empty());
    }

    #[test]
    fn a_run_cut_short_by_a_restart_fails_and_one_not_delivered_yet_goes_on() {
        let f = fixture("restart");
        f.start();
        f.hub.interrupt_schedule_runs();
        let run = f.schedule().last_run.unwrap();
        assert_eq!(run.status, "failed");
        assert!(run.error.unwrap().contains("重启"));
        // A run whose message waits for a free slot is still to come.
        let mut list = lock(&f.hub.schedules);
        let mut waiting = Run::new(1);
        waiting.message = Some("m-wait".to_string());
        list[0].last_run = Some(waiting);
        drop(list);
        f.hub
            .store
            .record_message(&Message {
                id: "m-wait".into(),
                to: "nobody".into(),
                from: format!("schedule:{}", f.id),
                body: "x".into(),
                session: None,
                reply_to: None,
                dedupe_key: None,
                at: 1,
            })
            .unwrap();
        f.hub.interrupt_schedule_runs();
        assert_eq!(f.status(), "running");
        assert_eq!(f.hub.awake.keys(), [format!("schedule:{}", f.id)]);
    }

    #[test]
    fn the_session_list_names_a_runs_task_and_its_background_work() {
        let f = fixture("list");
        f.start();
        f.hub.set_background("s1", true, false);
        let list = f.hub.sessions_json();
        let row = &list["sessions"][0];
        assert_eq!(row["schedule"], f.id.as_str());
        assert_eq!(row["background"], true);
        assert!(row["worktree"].is_null() && row["parent"].is_null());
    }
}

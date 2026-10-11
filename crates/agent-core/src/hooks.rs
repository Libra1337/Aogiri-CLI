use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

/// User-configured shell hooks that run at lifecycle points. Loaded from
/// `~/.lynshen/hooks.json` (global) and `<cwd>/.lynshen/hooks.json` (project,
/// only when the project is trusted). Cheap to clone — shared via `Arc`.
#[derive(Debug, Clone, Default)]
pub struct Hooks {
    inner: Arc<HookSet>,
}

#[derive(Debug, Default)]
struct HookSet {
    session_start: Vec<HookEntry>,
    user_prompt_submit: Vec<HookEntry>,
    pre_tool_use: Vec<HookEntry>,
    post_tool_use: Vec<HookEntry>,
    stop: Vec<HookEntry>,
    task_completed: Vec<HookEntry>,
    agent_idle: Vec<HookEntry>,
}

/// Most characters of a hook's output the main agent receives.
const MAX_REPORT_CHARS: usize = 4000;

/// How long a hook may run unless its entry sets `timeout`.
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// How long output may keep arriving after a hook exited: a background
/// process it started can hold its output open indefinitely.
const OUTPUT_GRACE: Duration = Duration::from_millis(500);

/// What a `task_completed` or `agent_idle` hook said, for the main agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    pub event: &'static str,
    /// The hook exited 0.
    pub ok: bool,
    /// Its stdout, trimmed and capped; for a failure the stderr when stdout
    /// is empty, or the exit code.
    pub text: String,
}

#[derive(Debug, Clone)]
struct HookEntry {
    command: String,
    /// Tool names this hook applies to. `None` means every tool.
    tools: Option<Vec<String>>,
    /// Past this the hook and its process group are killed and it fails.
    timeout: Duration,
}

impl HookEntry {
    fn matches(&self, tool: &str) -> bool {
        match &self.tools {
            Some(tools) => tools.iter().any(|name| name == tool),
            None => true,
        }
    }
}

struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
    /// The hook ran past its timeout and was killed; `stderr` says so.
    timed_out: bool,
}

impl Hooks {
    pub fn load(profile_dir: &Path, cwd: &Path, project_trusted: bool) -> Self {
        let mut set = HookSet::default();
        merge_file(&mut set, &profile_dir.join("hooks.json"));
        if project_trusted {
            merge_file(&mut set, &cwd.join(".lynshen").join("hooks.json"));
        }
        Self {
            inner: Arc::new(set),
        }
    }

    /// Hooks from one `hooks.json` value (tests).
    #[cfg(test)]
    pub(crate) fn from_value(value: &Value) -> Self {
        let mut set = HookSet::default();
        merge_value(&mut set, value);
        Self {
            inner: Arc::new(set),
        }
    }

    pub fn has_task_completed(&self) -> bool {
        !self.inner.task_completed.is_empty()
    }

    pub fn has_agent_idle(&self) -> bool {
        !self.inner.agent_idle.is_empty()
    }

    /// Runs `task_completed` hooks in `workdir` (the task owner's): stdin
    /// carries `{"event", "task", "workdir"}`, the environment
    /// `LYNSHEN_HOOK_WORKDIR`, `LYNSHEN_TASK_ID` and `LYNSHEN_TASK`.
    pub fn task_completed(&self, task: &Value, workdir: &Path) -> Vec<HookReport> {
        let id = task["id"].as_str().unwrap_or_default().to_string();
        let env = [("LYNSHEN_TASK_ID", id), ("LYNSHEN_TASK", task.to_string())];
        self.report(
            &self.inner.task_completed,
            "task_completed",
            ("task", task),
            workdir,
            &env,
        )
    }

    /// Runs `agent_idle` hooks in `workdir` (the subagent's) once a
    /// subagent finished: stdin carries `{"event", "agent", "workdir"}`, the
    /// environment `LYNSHEN_HOOK_WORKDIR` and `LYNSHEN_AGENT` (its path).
    pub fn agent_idle(&self, agent: &Value, workdir: &Path) -> Vec<HookReport> {
        let path = agent["path"].as_str().unwrap_or_default().to_string();
        self.report(
            &self.inner.agent_idle,
            "agent_idle",
            ("agent", agent),
            workdir,
            &[("LYNSHEN_AGENT", path)],
        )
    }

    fn report(
        &self,
        entries: &[HookEntry],
        event: &'static str,
        (key, value): (&str, &Value),
        workdir: &Path,
        env: &[(&str, String)],
    ) -> Vec<HookReport> {
        let mut reports = Vec::new();
        let workdir_text = workdir.display().to_string();
        let mut env = env.to_vec();
        env.push(("LYNSHEN_HOOK_WORKDIR", workdir_text.clone()));
        for entry in entries {
            let payload = json!({
                "event": event,
                key: value,
                "workdir": workdir_text,
                "cwd": workdir_text,
            });
            let outcome = run_command_with_env(entry, &payload, workdir, &env);
            let ok = outcome.code == 0;
            let text = if ok {
                outcome.stdout.trim().to_string()
            } else {
                block_reason(&outcome, event)
            };
            if ok && text.is_empty() {
                continue;
            }
            reports.push(HookReport {
                event,
                ok,
                text: cap(&text),
            });
        }
        reports
    }

    /// Runs `session_start` hooks and returns any non-empty stdout to surface.
    pub fn session_start(&self, cwd: &Path) -> Vec<String> {
        self.notify(&self.inner.session_start, "session_start", json!({}), cwd)
    }

    /// Runs `stop` hooks (turn finished) and returns any non-empty stdout.
    pub fn stop(&self, cwd: &Path) -> Vec<String> {
        self.notify(&self.inner.stop, "stop", json!({}), cwd)
    }

    /// Runs `user_prompt_submit` hooks. A non-zero exit blocks the prompt and the
    /// reason is returned as `Err`.
    pub fn user_prompt_submit(&self, prompt: &str, cwd: &Path) -> Result<(), String> {
        let payload = json!({
            "event": "user_prompt_submit",
            "prompt": prompt,
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.user_prompt_submit {
            let outcome = run_command(entry, &payload, cwd);
            if outcome.code != 0 {
                return Err(block_reason(&outcome, "user_prompt_submit"));
            }
        }
        Ok(())
    }

    /// Runs `pre_tool_use` hooks for `tool`. Returns `Some(reason)` to block the
    /// tool from executing.
    pub fn pre_tool(&self, tool: &str, arguments: &str, cwd: &Path) -> Option<String> {
        if self.inner.pre_tool_use.is_empty() {
            return None;
        }
        let payload = json!({
            "event": "pre_tool_use",
            "tool": tool,
            "arguments": parse_arguments(arguments),
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.pre_tool_use {
            if !entry.matches(tool) {
                continue;
            }
            let outcome = run_command(entry, &payload, cwd);
            if outcome.code != 0 {
                return Some(block_reason(&outcome, "pre_tool_use"));
            }
        }
        None
    }

    /// Runs `post_tool_use` hooks for `tool`. Best-effort; failures are ignored.
    pub fn post_tool(&self, tool: &str, output: &str, cwd: &Path) {
        if self.inner.post_tool_use.is_empty() {
            return;
        }
        let payload = json!({
            "event": "post_tool_use",
            "tool": tool,
            "output": output,
            "cwd": cwd.display().to_string(),
        });
        for entry in &self.inner.post_tool_use {
            if entry.matches(tool) {
                let _ = run_command(entry, &payload, cwd);
            }
        }
    }

    fn notify(&self, entries: &[HookEntry], event: &str, extra: Value, cwd: &Path) -> Vec<String> {
        let mut messages = Vec::new();
        for entry in entries {
            let mut payload = json!({ "event": event, "cwd": cwd.display().to_string() });
            if let (Some(map), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
                for (key, value) in extra {
                    map.insert(key.clone(), value.clone());
                }
            }
            let outcome = run_command(entry, &payload, cwd);
            let text = if outcome.timed_out {
                outcome.stderr.trim()
            } else {
                outcome.stdout.trim()
            };
            if !text.is_empty() {
                messages.push(format!("hook ({event}): {text}"));
            }
        }
        messages
    }
}

fn block_reason(outcome: &Outcome, event: &str) -> String {
    let stderr = outcome.stderr.trim();
    let stdout = outcome.stdout.trim();
    if !stderr.is_empty() {
        stderr.to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        format!("{event} hook exited with code {}", outcome.code)
    }
}

fn cap(text: &str) -> String {
    let mut capped: String = text.chars().take(MAX_REPORT_CHARS).collect();
    if capped.len() < text.len() {
        capped.push_str("\n[hook output cut]");
    }
    capped
}

fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn run_command(entry: &HookEntry, payload: &Value, cwd: &Path) -> Outcome {
    run_command_with_env(entry, payload, cwd, &[])
}

fn run_command_with_env(
    entry: &HookEntry,
    payload: &Value,
    cwd: &Path,
    env: &[(&str, String)],
) -> Outcome {
    #[cfg(windows)]
    let (shell, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("sh", "-c");

    let event = payload
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut command = Command::new(shell);
    command
        .arg(flag)
        .arg(&entry.command)
        .current_dir(cwd)
        .env("LYNSHEN_HOOK_EVENT", &event)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a timeout kills what the hook started too.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Outcome {
                code: -1,
                stdout: String::new(),
                stderr: error.to_string(),
                timed_out: false,
            }
        }
    };
    // Written and read on threads: a hook that ignores its stdin or fills a
    // pipe must not stall the turn past the timeout.
    if let Some(mut stdin) = child.stdin.take() {
        let payload = payload.to_string();
        thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
        });
    }
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        read_pipe(stdout, true, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        read_pipe(stderr, false, tx);
    }

    let deadline = Instant::now() + entry.timeout;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut open_pipes = 2;
    let mut exited: Option<(i32, Instant)> = None;
    loop {
        let now = Instant::now();
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exited = Some((status.code().unwrap_or(-1), now)),
                Ok(None) => {}
                Err(_) => exited = Some((-1, now)),
            }
        }
        if let Some((code, at)) = exited {
            if open_pipes == 0 || now >= at + OUTPUT_GRACE {
                return Outcome {
                    code,
                    stdout: String::from_utf8_lossy(&stdout).to_string(),
                    stderr: String::from_utf8_lossy(&stderr).to_string(),
                    timed_out: false,
                };
            }
        } else if now >= deadline {
            crate::tools::kill_child(&mut child);
            return Outcome {
                code: -1,
                stdout: String::from_utf8_lossy(&stdout).to_string(),
                stderr: format!(
                    "{event} hook timed out after {}s and was stopped: {}",
                    entry.timeout.as_secs_f64(),
                    entry.command
                ),
                timed_out: true,
            };
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok((true, Some(chunk))) => stdout.extend_from_slice(&chunk),
            Ok((false, Some(chunk))) => stderr.extend_from_slice(&chunk),
            Ok((_, None)) => open_pipes -= 1,
            Err(_) => {}
        }
    }
}

/// Sends what `pipe` yields, chunk by chunk, on a thread; None at its end.
fn read_pipe(
    mut pipe: impl Read + Send + 'static,
    is_stdout: bool,
    tx: mpsc::Sender<(bool, Option<Vec<u8>>)>,
) {
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if tx.send((is_stdout, Some(buffer[..read].to_vec()))).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = tx.send((is_stdout, None));
    });
}

fn merge_file(set: &mut HookSet, path: &Path) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return;
    };
    merge_value(set, &value);
}

fn merge_value(set: &mut HookSet, value: &Value) {
    push_entries(&mut set.session_start, value, "session_start");
    push_entries(&mut set.user_prompt_submit, value, "user_prompt_submit");
    push_entries(&mut set.pre_tool_use, value, "pre_tool_use");
    push_entries(&mut set.post_tool_use, value, "post_tool_use");
    push_entries(&mut set.stop, value, "stop");
    push_entries(&mut set.task_completed, value, "task_completed");
    push_entries(&mut set.agent_idle, value, "agent_idle");
}

fn push_entries(target: &mut Vec<HookEntry>, value: &Value, key: &str) {
    let Some(entries) = value.get(key).and_then(Value::as_array) else {
        return;
    };
    for entry in entries {
        let Some(command) = entry
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
        else {
            continue;
        };
        let tools = entry.get("tools").and_then(Value::as_array).map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
        // Seconds, at most a day; anything but a positive number keeps the
        // default.
        let timeout = entry
            .get("timeout")
            .and_then(Value::as_f64)
            .filter(|seconds| *seconds > 0.0)
            .and_then(|seconds| Duration::try_from_secs_f64(seconds.min(86_400.0)).ok())
            .unwrap_or(DEFAULT_HOOK_TIMEOUT);
        target.push(HookEntry {
            command: command.to_string(),
            tools,
            timeout,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks_from(value: Value) -> Hooks {
        let mut set = HookSet::default();
        push_entries(&mut set.pre_tool_use, &value, "pre_tool_use");
        Hooks {
            inner: Arc::new(set),
        }
    }

    #[test]
    fn pre_tool_blocks_on_nonzero_exit() {
        let command = if cfg!(windows) {
            "echo denied >&2 & exit /b 1"
        } else {
            "echo denied >&2; exit 1"
        };
        let hooks = hooks_from(json!({
            "pre_tool_use": [{ "command": command, "tools": ["bash"] }]
        }));
        let reason = hooks.pre_tool("bash", "{}", Path::new("."));
        assert_eq!(reason.as_deref(), Some("denied"));
        // Tool filter excludes "read".
        assert!(hooks.pre_tool("read", "{}", Path::new(".")).is_none());
    }

    #[test]
    fn task_completed_hooks_get_the_task_and_run_in_the_workdir() {
        if cfg!(windows) {
            return;
        }
        let workdir = std::env::temp_dir().join(format!(
            "lynshen-hook-task-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&workdir).unwrap();
        let hooks = Hooks::from_value(&json!({
            "task_completed": [
                { "command": "cat > payload.json; printf 'tests ok in %s for %s' \"$(basename \"$PWD\")\" \"$LYNSHEN_TASK_ID\"" },
                { "command": "echo 'quiet hook'  >/dev/null" },
                { "command": "echo broken >&2; exit 3" }
            ],
            "agent_idle": [{ "command": "printf '%s idle' \"$LYNSHEN_AGENT\"" }]
        }));
        assert!(hooks.has_task_completed() && hooks.has_agent_idle());
        let task = json!({ "id": "t2", "title": "Add parser", "status": "completed" });
        let reports = hooks.task_completed(&task, &workdir);
        let name = workdir.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            reports,
            vec![
                HookReport {
                    event: "task_completed",
                    ok: true,
                    text: format!("tests ok in {name} for t2"),
                },
                HookReport {
                    event: "task_completed",
                    ok: false,
                    text: "broken".to_string(),
                },
            ]
        );
        let payload: Value =
            serde_json::from_str(&fs::read_to_string(workdir.join("payload.json")).unwrap())
                .unwrap();
        assert_eq!(payload["event"], "task_completed");
        assert_eq!(payload["task"]["title"], "Add parser");
        assert_eq!(payload["workdir"], workdir.display().to_string());

        let reports = hooks.agent_idle(
            &json!({ "path": "/root/w", "status": "completed" }),
            &workdir,
        );
        assert_eq!(reports[0].text, "/root/w idle");
        assert!(Hooks::default().task_completed(&task, &workdir).is_empty());
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn hook_output_is_capped() {
        if cfg!(windows) {
            return;
        }
        let hooks = Hooks::from_value(&json!({
            "agent_idle": [{ "command": "head -c 9000 /dev/zero | tr '\\0' x" }]
        }));
        let reports = hooks.agent_idle(&json!({ "path": "/root/w" }), Path::new("."));
        assert!(reports[0].text.starts_with("xxxx"));
        assert!(reports[0].text.ends_with("[hook output cut]"));
        assert!(reports[0].text.len() < 4100);
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_past_its_timeout_is_killed_with_its_children_and_fails() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-hook-timeout-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let hooks = Hooks::from_value(&json!({
            "pre_tool_use": [{ "command": "sleep 30 & echo $! > child.pid; sleep 30", "timeout": 1 }],
            "stop": [{ "command": "sleep 30", "timeout": 0.5 }]
        }));

        let started = Instant::now();
        let reason = hooks
            .pre_tool("bash", "{}", &dir)
            .expect("a timed-out hook blocks");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the hook was not stopped"
        );
        assert_eq!(
            reason,
            "pre_tool_use hook timed out after 1s and was stopped: sleep 30 & echo $! > child.pid; sleep 30"
        );
        let child: i32 = fs::read_to_string(dir.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only probes the pid.
        let alive = || unsafe { libc::kill(child, 0) } == 0;
        let gone_by = Instant::now() + Duration::from_secs(5);
        while alive() && Instant::now() < gone_by {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "the hook's own child outlived the timeout");

        assert_eq!(
            hooks.stop(&dir),
            vec!["hook (stop): stop hook timed out after 0.5s and was stopped: sleep 30"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_hook_that_leaves_a_process_holding_its_output_still_returns() {
        if cfg!(windows) {
            return;
        }
        let hooks = Hooks::from_value(&json!({
            "stop": [{ "command": "sleep 5 & echo done" }]
        }));
        let started = Instant::now();
        assert_eq!(hooks.stop(Path::new(".")), vec!["hook (stop): done"]);
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn hook_timeout_is_a_minute_unless_the_entry_sets_one() {
        let hooks = Hooks::from_value(&json!({
            "stop": [
                { "command": "a" },
                { "command": "b", "timeout": 5 },
                { "command": "c", "timeout": 0 },
                { "command": "d", "timeout": "soon" },
                { "command": "e", "timeout": 1e300 }
            ]
        }));
        let timeouts = hooks
            .inner
            .stop
            .iter()
            .map(|entry| entry.timeout.as_secs())
            .collect::<Vec<_>>();
        assert_eq!(timeouts, vec![60, 5, 60, 60, 86_400]);
    }

    #[test]
    fn pre_tool_allows_on_zero_exit() {
        let hooks = hooks_from(json!({
            "pre_tool_use": [{ "command": "exit 0" }]
        }));
        assert!(hooks.pre_tool("bash", "{}", Path::new(".")).is_none());
    }
}

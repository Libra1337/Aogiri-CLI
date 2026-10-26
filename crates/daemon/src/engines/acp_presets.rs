//! The ACP agents the daemon starts itself, by id (`options.agent`): it
//! finds the program, checks its version and builds the command line, so a
//! client (a phone too) never names one. Approval modes and the LynShen
//! gateway reach an agent only through its launch flags, environment and a
//! config directory of the daemon's own; the user's own agent config is never
//! written.
//!
//! - OpenCode (1.x; 2.x is another program and is refused): `opencode acp`,
//!   its approval modes as the agents `opencode.rs` defines.
//! - oh-my-pi (`omp acp`): its approval mode is a launch flag
//!   (`--approval-mode`), so a mode change restarts it on its conversation.
//! - Pi, behind the community adapter pi-acp (from npx, pinned): it never
//!   asks before running a tool, so it runs in 全自动 only.

use super::{find_program, home, Options};
use lynshen_agent_core::ModelConfig;
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// pi-acp, the adapter Pi runs behind (pinned: the version tested).
const PI_ACP: &str = "pi-acp@0.0.34";
/// The environment variable a gateway agent reads its local key from.
const KEY_ENV: &str = "LYNSHEN_GATEWAY_TOKEN";

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// The program whose version decides whether it can run.
    program: &'static str,
    /// Other install directories it is found in (under the home folder).
    extra: &'static [&'static str],
    pub min_version: Option<&'static str>,
    /// The first version it no longer runs on (OpenCode 2.x).
    pub below_version: Option<&'static str>,
    /// It asks before running tools (else it never asks: 全自动 only).
    pub approvals: bool,
    /// It can run on the LynShen gateway.
    pub gateway: bool,
    pub install: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "opencode",
        name: "OpenCode",
        program: "opencode",
        extra: &[".opencode/bin/opencode"],
        min_version: Some("1.0.0"),
        below_version: Some("2.0.0"),
        approvals: true,
        gateway: true,
        install: "npm i -g opencode-ai@1",
    },
    Preset {
        id: "omp",
        name: "oh-my-pi",
        program: "omp",
        extra: &[".bun/bin/omp"],
        min_version: None,
        below_version: None,
        approvals: true,
        gateway: true,
        install: "bun install -g @oh-my-pi/pi-coding-agent",
    },
    Preset {
        id: "pi",
        name: "Pi",
        program: "pi",
        extra: &[],
        min_version: Some("0.81.0"),
        below_version: None,
        approvals: false,
        gateway: true,
        install: "npm install -g @earendil-works/pi-coding-agent",
    },
];

pub fn find(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// What a client's approval mode (LynShen or desktop names) launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ask,
    Edits,
    All,
}

impl Mode {
    pub fn of(name: Option<&str>) -> Self {
        match name.unwrap_or_default() {
            "auto-edit" | "acceptEdits" | "auto" => Mode::Edits,
            "full-access" | "full-auto" | "bypassPermissions" => Mode::All,
            _ => Mode::Ask,
        }
    }

    /// The name clients get in `approval_mode`.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Ask => "read-only",
            Mode::Edits => "auto-edit",
            Mode::All => "full-auto",
        }
    }
}

/// Why `preset` cannot run in `mode`: Pi never asks, so only in 全自动.
pub fn refuses_mode(preset: &Preset, mode: Option<&str>) -> Option<String> {
    (!preset.approvals && Mode::of(mode) != Mode::All).then(|| {
        format!(
            "{} 不会在运行工具前询问，只能在全自动模式下使用。",
            preset.name
        )
    })
}

/// The program to start and the one that has the version: the same,
/// except for Pi, which runs behind pi-acp from npx. None: not installed.
fn locate(preset: &Preset) -> Option<(PathBuf, PathBuf)> {
    let found = |name: &str, extra: &[&str]| {
        let extra: Vec<PathBuf> = extra.iter().map(|path| home().join(path)).collect();
        Some(find_program(name, &extra)).filter(|path| path.is_file())
    };
    let program = found(preset.program, preset.extra)?;
    let launcher = match preset.id {
        "pi" => found("npx", &[])?,
        _ => program.clone(),
    };
    Some((launcher, program))
}

/// The daemon's PATH behind the programs' own directories, so
/// `#!/usr/bin/env bun|node` finds the interpreter installed beside them.
fn search_path(programs: &[&Path]) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in programs.iter().filter_map(|program| program.parent()) {
        if !dirs.iter().any(|known| known == dir) {
            dirs.push(dir.to_path_buf());
        }
    }
    let rest = std::env::var_os("PATH").unwrap_or_default();
    let paths = dirs.into_iter().chain(std::env::split_paths(&rest));
    std::env::join_paths(paths).unwrap_or(rest)
}

/// `program --version`, read as `x.y.z`; None when it does not answer in
/// time or prints no version.
fn version(program: &Path, path: &OsString) -> Option<String> {
    let mut child = Command::new(program)
        .arg("--version")
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    parse_version(&reader.join().ok()?)
}

/// The first `x.y[.z]` in a version line (`v2.0.20`, `omp/18.1.21`).
fn parse_version(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|part| part.trim_matches('.'))
        .find(|part| part.contains('.') && part.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
}

fn older(version: &str, than: &str) -> bool {
    let parts = |text: &str| {
        let mut parts = [0u64; 3];
        for (slot, part) in parts.iter_mut().zip(text.split('.')) {
            *slot = part.parse().unwrap_or(0);
        }
        parts
    };
    parts(version) < parts(than)
}

/// Why `version` of `preset` cannot run here (too old, or a later major
/// version it does not support).
fn unsupported(preset: &Preset, version: &str) -> Option<String> {
    if let Some(min) = preset.min_version.filter(|min| older(version, min)) {
        return Some(format!(
            "{} {version} is too old (needs {min} or later): {}",
            preset.name, preset.install
        ));
    }
    if let Some(below) = preset.below_version.filter(|below| !older(version, below)) {
        return Some(format!(
            "{} {version} is not supported (needs a version before {below}): {}",
            preset.name, preset.install
        ));
    }
    None
}

/// The preset's entry for `acp_presets`: whether it can start here.
fn status(preset: &Preset) -> Value {
    let found = locate(preset);
    let version = found.as_ref().and_then(|(launcher, program)| {
        version(
            program,
            &search_path(&[launcher.as_path(), program.as_path()]),
        )
    });
    let status = match (&found, &version) {
        (None, _) => "missing",
        (Some(_), Some(version)) if unsupported(preset, version).is_some() => "unsupported",
        _ => "ready",
    };
    json!({
        "id": preset.id,
        "name": preset.name,
        "status": status,
        "path": found.map(|(_, program)| program.display().to_string()),
        "version": version,
        "min_version": preset.min_version,
        "below_version": preset.below_version,
        "approvals": preset.approvals,
        "gateway": preset.gateway,
        "install": preset.install,
        "tui": true,
    })
}

/// `acp_presets`: every preset, its programs asked in parallel.
pub fn presets_json() -> Value {
    let probes: Vec<_> = PRESETS
        .iter()
        .map(|preset| thread::spawn(move || status(preset)))
        .collect();
    let presets: Vec<Value> = probes
        .into_iter()
        .filter_map(|probe| probe.join().ok())
        .collect();
    json!({ "type": "acp_presets", "presets": presets })
}

/// The preset's launcher and program, and the `PATH` to run them with;
/// an error when it is missing or its version is not one it runs on.
fn installed(preset: &Preset) -> Result<(PathBuf, PathBuf, OsString), String> {
    let (launcher, program) = locate(preset)
        .ok_or_else(|| format!("{} is not installed: {}", preset.name, preset.install))?;
    let path = search_path(&[launcher.as_path(), program.as_path()]);
    if let Some(reason) = version(&program, &path).and_then(|v| unsupported(preset, &v)) {
        return Err(reason);
    }
    Ok((launcher, program, path))
}

/// The preset's command line in `options.approval_mode`.
pub fn command(preset: &Preset, options: &Options) -> Result<Command, String> {
    if let Some(reason) = refuses_mode(preset, options.approval_mode.as_deref()) {
        return Err(reason);
    }
    let (launcher, program, path) = installed(preset)?;
    let mut command = launch(
        preset,
        &launcher,
        &program,
        Mode::of(options.approval_mode.as_deref()),
    );
    command.env("PATH", path);
    Ok(command)
}

/// The preset's own TUI resuming `conversation` (the ACP session id, which
/// is the TUI's too) in `options.approval_mode`.
pub fn tui(preset: &Preset, options: &Options, conversation: &str) -> Result<Command, String> {
    if let Some(reason) = refuses_mode(preset, options.approval_mode.as_deref()) {
        return Err(reason);
    }
    let (_, program, path) = installed(preset)?;
    let mut command = tui_launch(
        preset,
        &program,
        options.approval_mode.as_deref(),
        conversation,
    );
    command.env("PATH", path);
    Ok(command)
}

fn launch(preset: &Preset, launcher: &Path, program: &Path, mode: Mode) -> Command {
    let mut command = Command::new(launcher);
    match preset.id {
        // Its modes are the agents `opencode::configure` adds.
        "opencode" => {
            command.arg("acp");
        }
        "omp" => {
            command.args(["acp", "--approval-mode", omp_mode(mode)]);
        }
        _ => {
            command
                .args(["-y", PI_ACP])
                .env("PI_ACP_PI_COMMAND", program);
        }
    }
    command
}

/// The TUI resuming `conversation`. OpenCode's runs on the agent of the
/// session's mode (`opencode::configure` defines it for the TUI too).
fn tui_launch(preset: &Preset, program: &Path, mode: Option<&str>, conversation: &str) -> Command {
    let mut command = Command::new(program);
    match preset.id {
        "opencode" => {
            command.args(opencode_tui_args(mode, conversation));
        }
        "omp" => {
            command.args([
                "--approval-mode",
                omp_mode(Mode::of(mode)),
                "--resume",
                conversation,
            ]);
        }
        _ => {
            command.args(["--session", conversation]);
        }
    }
    command
}

/// OpenCode's TUI on `conversation`, on the agent `mode` runs on.
pub fn opencode_tui_args(mode: Option<&str>, conversation: &str) -> Vec<String> {
    vec![
        "--session".to_string(),
        conversation.to_string(),
        "--agent".to_string(),
        super::opencode::agent_for(mode.unwrap_or_default()).to_string(),
    ]
}

/// oh-my-pi's approval mode (its own default is yolo).
fn omp_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "always-ask",
        Mode::Edits => "write",
        Mode::All => "yolo",
    }
}

/// Runs omp or Pi on the LynShen gateway: its agent directory is the
/// daemon's own (`~/.lynshen/daemon/acp/<id>`), holding the LynShen models
/// with the local `key` read from the environment; its sessions stay where
/// the user's own setup keeps them.
pub fn use_gateway(
    command: &mut Command,
    preset: &Preset,
    cwd: &Path,
    base: &str,
    key: &str,
    model: Option<&str>,
) -> Result<(), String> {
    // Sessions starting at once share the directory.
    static FILES: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _files = FILES.lock().unwrap_or_else(|e| e.into_inner());
    let models = lynshen_agent_core::lynshen_visible_models();
    if models.is_empty() {
        return Err(format!(
            "no LynShen models to give {}: choose some in the model list first",
            preset.name
        ));
    }
    configure(command, preset, &home(), cwd, base, key, &models, model)
}

#[allow(clippy::too_many_arguments)]
fn configure(
    command: &mut Command,
    preset: &Preset,
    home: &Path,
    cwd: &Path,
    base: &str,
    key: &str,
    models: &[ModelConfig],
    model: Option<&str>,
) -> Result<(), String> {
    // A model as the agent named it (`lynshen/<model>`, kept from an earlier
    // open), by its LynShen name.
    let model = model.map(|m| m.split_once('/').map_or(m, |(_, name)| name));
    let dir = home
        .join(".lynshen")
        .join("daemon")
        .join("acp")
        .join(preset.id);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let user_dir = match preset.id {
        "omp" => home.join(".omp").join("agent"),
        _ => home.join(".pi").join("agent"),
    };
    for (name, content) in gateway_files(preset, base, models, model) {
        let path = dir.join(name);
        crate::store::write_private(&path, format!("{content:#}\n").as_bytes())
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    if preset.id == "omp" {
        if let Some(model) = model {
            let provider = match model.starts_with("claude-") {
                true => "lynshen-claude",
                false => "lynshen",
            };
            command.args(["--model", &format!("{provider}/{model}")]);
        }
    }
    command.env(KEY_ENV, key).env("PI_CODING_AGENT_DIR", &dir);
    if preset.id == "omp" {
        // oh-my-pi writes to `PI_CODING_AGENT_SESSION_DIR` but looks sessions
        // up for `session/load` only under its agent directory: that
        // directory's `sessions` is the user's own.
        let sessions = user_dir.join("sessions");
        let link = dir.join("sessions");
        if std::fs::read_link(&link).ok() != Some(sessions.clone()) {
            std::fs::create_dir_all(&sessions)
                .map_err(|error| format!("cannot create {}: {error}", sessions.display()))?;
            let _ = std::fs::remove_file(&link);
            symlink_dir(&sessions, &link)
                .map_err(|error| format!("cannot link {}: {error}", link.display()))?;
        }
    } else {
        command.env(
            "PI_CODING_AGENT_SESSION_DIR",
            user_dir.join("sessions").join(session_dir_name(cwd)),
        );
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// The per-directory session folder name Pi uses:
/// `--<path without the leading "/", separators as "-">--`.
fn session_dir_name(cwd: &Path) -> String {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let cwd = cwd.to_string_lossy();
    let name: String = cwd
        .trim_start_matches('/')
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{name}--")
}

/// The files of the preset's agent directory. Claude models go over
/// Anthropic Messages, the rest over Responses. No secret: the key is an
/// environment variable's name.
fn gateway_files(
    preset: &Preset,
    base: &str,
    models: &[ModelConfig],
    model: Option<&str>,
) -> Vec<(&'static str, Value)> {
    let entry = |m: &ModelConfig| {
        let mut entry = json!({
            "id": m.name,
            "name": m.display_name.as_deref().unwrap_or(&m.name),
            "reasoning": m.reasoning.unwrap_or(!m.reasoning_efforts.is_empty()),
            "input": if m.accepts_images() { json!(["text", "image"]) } else { json!(["text"]) },
        });
        if m.context_window > 0 {
            entry["contextWindow"] = json!(m.context_window);
        }
        if m.max_output_tokens > 0 {
            entry["maxTokens"] = json!(m.max_output_tokens);
        }
        entry
    };
    let claude = |m: &&ModelConfig| m.name.starts_with("claude-");
    if preset.id == "omp" {
        let provider = |api: &str, base: String, models: Vec<Value>| json!({ "baseUrl": base, "api": api, "apiKey": KEY_ENV, "models": models });
        let responses = models.iter().filter(|m| !claude(m)).map(entry).collect();
        let messages = models.iter().filter(claude).map(entry).collect();
        let providers = json!({
            "lynshen": provider("openai-responses", format!("{base}/v1"), responses),
            "lynshen-claude": provider("anthropic-messages", base.to_string(), messages),
        });
        // JSON is YAML.
        return vec![("models.yml", json!({ "providers": providers }))];
    }
    let pi_models: Vec<Value> = models
        .iter()
        .map(|m| {
            let mut entry = entry(m);
            if claude(&m) {
                entry["api"] = json!("anthropic-messages");
                entry["baseUrl"] = json!(base);
            }
            entry
        })
        .collect();
    let default = model
        .map(str::to_string)
        .or_else(lynshen_agent_core::lynshen_default_model)
        .filter(|name| models.iter().any(|m| &m.name == name))
        .or_else(|| models.first().map(|m| m.name.clone()));
    vec![
        (
            "models.json",
            json!({ "providers": { "lynshen": {
                "name": "LynShen", "baseUrl": format!("{base}/v1"), "api": "openai-responses",
                "apiKey": format!("${KEY_ENV}"), "models": pi_models,
            } } }),
        ),
        (
            "settings.json",
            json!({ "defaultProvider": "lynshen", "defaultModel": default, "quietStartup": true }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect()
    }

    fn env(command: &Command, name: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().to_string()))
    }

    #[test]
    fn approval_modes_launch_each_agent_asking_as_the_user_chose() {
        let (bin, pi) = (Path::new("/bin/agent"), Path::new("/bin/pi"));
        assert_eq!(Mode::of(Some("manual")), Mode::Ask);
        assert_eq!(Mode::of(Some("plan")), Mode::Ask);
        assert_eq!(Mode::of(None), Mode::Ask);
        assert_eq!(Mode::of(Some("auto-edit")), Mode::Edits);
        assert_eq!(Mode::of(Some("bypassPermissions")), Mode::All);

        let omp = find("omp").unwrap();
        for (mode, flag) in [
            (Mode::Ask, "always-ask"),
            (Mode::Edits, "write"),
            (Mode::All, "yolo"),
        ] {
            assert_eq!(
                args(&launch(omp, bin, bin, mode)),
                ["acp", "--approval-mode", flag]
            );
        }
        // OpenCode's modes are agents, switched over ACP (`opencode`).
        let opencode = find("opencode").unwrap();
        assert_eq!(args(&launch(opencode, bin, bin, Mode::Ask)), ["acp"]);

        let preset = find("pi").unwrap();
        let pi_command = launch(preset, Path::new("/bin/npx"), pi, Mode::All);
        assert_eq!(pi_command.get_program(), "/bin/npx");
        assert_eq!(args(&pi_command), ["-y", PI_ACP]);
        assert_eq!(env(&pi_command, "PI_ACP_PI_COMMAND").unwrap(), "/bin/pi");
        // Pi never asks: only in 全自动.
        assert!(!preset.approvals);
        assert!(refuses_mode(preset, Some("auto-edit")).is_some());
        assert!(refuses_mode(preset, None).is_some());
        assert!(refuses_mode(preset, Some("full-access")).is_none());
        assert!(refuses_mode(omp, Some("read-only")).is_none());
        assert!(command(preset, &Options::default())
            .unwrap_err()
            .contains("全自动"));
    }

    #[test]
    fn each_agents_tui_resumes_the_conversation_asking_as_the_user_chose() {
        let bin = Path::new("/bin/agent");
        let opencode = find("opencode").unwrap();
        for (mode, agent) in [
            (Some("read-only"), "aogiri"),
            (Some("plan"), "aogiri-plan"),
            (Some("full-access"), "build"),
        ] {
            let tui = tui_launch(opencode, bin, mode, "ses_1");
            assert_eq!(tui.get_program(), "/bin/agent");
            assert_eq!(args(&tui), ["--session", "ses_1", "--agent", agent]);
        }
        let omp = find("omp").unwrap();
        for (mode, flag) in [
            (Some("read-only"), "always-ask"),
            (Some("auto-edit"), "write"),
            (Some("full-auto"), "yolo"),
        ] {
            assert_eq!(
                args(&tui_launch(omp, bin, mode, "c-1")),
                ["--approval-mode", flag, "--resume", "c-1"]
            );
        }
        // The pi program itself, not pi-acp.
        let pi = tui_launch(find("pi").unwrap(), Path::new("/bin/pi"), None, "c-2");
        assert_eq!(pi.get_program(), "/bin/pi");
        assert_eq!(args(&pi), ["--session", "c-2"]);
        assert_eq!(env(&pi, "PI_ACP_PI_COMMAND"), None);
    }

    #[test]
    fn every_preset_entry_says_whether_it_can_start() {
        let presets = presets_json();
        let entries = presets["presets"].as_array().unwrap();
        assert_eq!(entries.len(), PRESETS.len());
        for entry in entries {
            assert_eq!(entry["tui"], true);
            assert!(matches!(
                entry["status"].as_str(),
                Some("ready" | "missing" | "unsupported")
            ));
        }
    }

    #[test]
    fn versions_are_read_from_each_agents_output_and_checked() {
        assert_eq!(parse_version("v2.0.20\n").as_deref(), Some("2.0.20"));
        assert_eq!(parse_version("omp/18.1.21").as_deref(), Some("18.1.21"));
        assert_eq!(parse_version("1.2.0").as_deref(), Some("1.2.0"));
        assert_eq!(parse_version("no version"), None);
        let opencode = find("opencode").unwrap();
        assert!(unsupported(opencode, "1.14.3").is_none());
        assert!(unsupported(opencode, "2.0.20")
            .unwrap()
            .contains("opencode-ai@1"));
        assert!(unsupported(opencode, "0.9.1").is_some());
        let pi = find("pi").unwrap();
        assert!(unsupported(pi, "0.80.9").is_some());
        assert!(unsupported(pi, "0.81.0").is_none());
        assert!(unsupported(pi, "1.2.0").is_none());
        assert!(unsupported(find("omp").unwrap(), "0.0.1").is_none());
        assert_eq!(find("deepseek").map(|p| p.id), None);
    }

    #[test]
    fn session_folders_are_named_as_the_agents_name_them() {
        assert_eq!(
            session_dir_name(Path::new("/no/such/dir")),
            "--no-such-dir--"
        );
    }

    fn models() -> Vec<ModelConfig> {
        let model = |name: &str, efforts: &[&str], window: u64| ModelConfig {
            name: name.to_string(),
            context_window: window,
            max_context_window: 0,
            max_output_tokens: 0,
            reasoning_efforts: efforts.iter().map(|e| e.to_string()).collect(),
            input_cost: 0.0,
            cached_input_cost: 0.0,
            output_cost: 0.0,
            display_name: None,
            group_windows: Default::default(),
            reasoning: None,
            input_modalities: None,
            hidden: false,
        };
        vec![
            model("gpt-6", &["low", "high"], 400_000),
            model("claude-sonnet-5", &[], 0),
        ]
    }

    #[test]
    fn the_gateway_gives_pi_its_own_agent_dir_with_lynshen_models() {
        let home = std::env::temp_dir().join(format!("lynshen-acp-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let preset = find("pi").unwrap();
        let mut command = launch(preset, Path::new("/n"), Path::new("/p"), Mode::All);
        configure(
            &mut command,
            preset,
            &home,
            Path::new("/no/such/dir"),
            "http://127.0.0.1:9/gw",
            "k-1",
            &models(),
            None,
        )
        .unwrap();
        let dir = home.join(".lynshen/daemon/acp/pi");
        assert_eq!(env(&command, KEY_ENV).unwrap(), "k-1");
        assert_eq!(
            env(&command, "PI_CODING_AGENT_DIR").unwrap(),
            dir.display().to_string()
        );
        assert_eq!(
            env(&command, "PI_CODING_AGENT_SESSION_DIR").unwrap(),
            home.join(".pi/agent/sessions/--no-such-dir--")
                .display()
                .to_string()
        );
        let read = |name: &str| -> Value {
            serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
        };
        let provider = &read("models.json")["providers"]["lynshen"];
        assert_eq!(provider["baseUrl"], "http://127.0.0.1:9/gw/v1");
        assert_eq!(provider["api"], "openai-responses");
        assert_eq!(provider["apiKey"], format!("${KEY_ENV}"));
        assert_eq!(provider["models"][0]["id"], "gpt-6");
        assert_eq!(provider["models"][0]["reasoning"], true);
        assert_eq!(provider["models"][0]["contextWindow"], 400_000);
        assert_eq!(provider["models"][1]["api"], "anthropic-messages");
        assert_eq!(provider["models"][1]["baseUrl"], "http://127.0.0.1:9/gw");
        assert!(provider["models"][1].get("contextWindow").is_none());
        assert_eq!(read("settings.json")["defaultProvider"], "lynshen");
        // The key never reaches the files.
        for name in ["models.json", "settings.json"] {
            assert!(!std::fs::read_to_string(dir.join(name))
                .unwrap()
                .contains("k-1"));
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_gateway_gives_omp_two_providers_and_the_chosen_model() {
        let home = std::env::temp_dir().join(format!("lynshen-acp-omp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let preset = find("omp").unwrap();
        let mut command = launch(preset, Path::new("/o"), Path::new("/o"), Mode::Ask);
        configure(
            &mut command,
            preset,
            &home,
            Path::new("/no/such/dir"),
            "http://gw",
            "k-2",
            &models(),
            Some("lynshen-claude/claude-sonnet-5"),
        )
        .unwrap();
        assert_eq!(
            args(&command),
            [
                "acp",
                "--approval-mode",
                "always-ask",
                "--model",
                "lynshen-claude/claude-sonnet-5"
            ]
        );
        assert_eq!(env(&command, "PI_CODING_AGENT_SESSION_DIR"), None);
        let sessions = home.join(".lynshen/daemon/acp/omp/sessions");
        assert_eq!(
            std::fs::read_link(&sessions).unwrap(),
            home.join(".omp/agent/sessions")
        );
        let text =
            std::fs::read_to_string(home.join(".lynshen/daemon/acp/omp/models.yml")).unwrap();
        let providers = &serde_json::from_str::<Value>(&text).unwrap()["providers"];
        assert_eq!(providers["lynshen"]["baseUrl"], "http://gw/v1");
        assert_eq!(providers["lynshen"]["apiKey"], KEY_ENV);
        assert_eq!(providers["lynshen"]["models"][0]["id"], "gpt-6");
        assert_eq!(providers["lynshen-claude"]["api"], "anthropic-messages");
        assert_eq!(providers["lynshen-claude"]["baseUrl"], "http://gw");
        assert_eq!(
            providers["lynshen-claude"]["models"][0]["id"],
            "claude-sonnet-5"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}

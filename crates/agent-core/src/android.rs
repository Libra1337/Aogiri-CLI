//! Android phone control through android-use
//! (github.com/austinintelligence/android-use, MIT): its `au` binary drives
//! one Android phone or emulator over ADB with an accessibility helper app.
//!
//! Off by default. The user turns it on (`android_use: true` in config.json,
//! Desktop's settings row or `/android on`) and installs the pinned release
//! (`install`), which is checked against the SHA-256 pinned here before it
//! is unpacked into `~/.lynshen/tools/android-use/<version>`. The model then
//! gets two tools, `android_read` and `android_act`, which take au's command
//! strings. Both run through one `au serve --mcp` per process (`call`):
//! au holds an exclusive lock per state folder, so one server per session
//! would fail in every session but the first. It stops after `IDLE_STOP`.
//!
//! Every `android_act` asks like a shell command and never further: no
//! session-wide "always allow", no safety classifier; camera, microphone
//! and recording ask in every mode. The approval names the action in
//! Chinese with the app it happens in (`approval_summary`).
//!
//! What the phone shows goes to the session's own model request only (the
//! tool result), never into logs.

use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

pub const READ_TOOL: &str = "android_read";
pub const ACT_TOOL: &str = "android_act";
/// The android-use release LynShen installs.
pub const VERSION: &str = "1.2.0";
/// config.json key: whether the model is offered the phone tools.
pub const CONFIG_KEY: &str = "android_use";
const RELEASES: &str = "https://github.com/austinintelligence/android-use/releases/download";
/// The release archives of `VERSION` and their SHA-256 (the release's
/// SHA256SUMS, equal to GitHub's asset digests): (os, arch, asset, sha256).
const ASSETS: [(&str, &str, &str, &str); 6] = [
    (
        "macos",
        "aarch64",
        "android-use-v1.2.0-macos-aarch64.tar.gz",
        "040e51f57dbf460b508b91e02583c386b4f8b8622a262f60ffbe124a95f806e1",
    ),
    (
        "macos",
        "x86_64",
        "android-use-v1.2.0-macos-x86_64.tar.gz",
        "b8012aa98e229631b44c8e84f953354fee0eec2e74d2696120d9c65fad6d82c6",
    ),
    (
        "linux",
        "aarch64",
        "android-use-v1.2.0-linux-aarch64.tar.gz",
        "4c2c8055b61a07661321fe27243145c8c2dbc1d307ae696ad140f1f51e81663c",
    ),
    (
        "linux",
        "x86_64",
        "android-use-v1.2.0-linux-x86_64.tar.gz",
        "800039f52a3da033d91680abc89d96d0ef94c50affe6f9e6be7623035673996f",
    ),
    (
        "windows",
        "aarch64",
        "android-use-v1.2.0-windows-aarch64.zip",
        "8c55c7739ed2156d4b5c8e9a886506e863314f692e6b887a93be9b087acc2d3b",
    ),
    (
        "windows",
        "x86_64",
        "android-use-v1.2.0-windows-x86_64.zip",
        "da1c69c06f47dde003264f07085cb56c7f9904b7f1f63916a378d81ef6d20a4a",
    ),
];
const MAX_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;
/// One tool call: a chain may wait up to 30 s per step.
const CALL_TIMEOUT: Duration = Duration::from_secs(180);
/// An unused server stops, freeing the phone for other programs.
const IDLE_STOP: Duration = Duration::from_secs(300);
const QUICK: Duration = Duration::from_secs(5);
const SETUP_TIMEOUT: Duration = Duration::from_secs(240);

// ---- tools ------------------------------------------------------------

/// The model's tools, as au 1.2.0 lists them, with when to use them.
pub fn definitions() -> Vec<Value> {
    let input = json!({
        "type": "object",
        "properties": { "command": { "type": "string" } },
        "required": ["command"]
    });
    vec![
        json!({
            "type": "function",
            "name": READ_TOOL,
            "description": "Look at the user's Android phone without changing it. Use only when the user asks for something on their phone. status | screen [full|changes|matching \"TEXT\"] | find \"TEXT\" | page [text [matching \"TEXT\"]] | notifications | browser tabs. `help` lists every command. Read `screen` once if the state is unknown; rows are names to quote, with a tag for non-buttons: `sw Wi-Fi=on`, `edit Email=\"x\"`; `Save#2` means tap \"Save\" number 2; `e7` has no label: tap e7.",
            "parameters": input.clone(),
        }),
        json!({
            "type": "function",
            "name": ACT_TOOL,
            "description": "Change the user's Android phone; the user approves each call. Join up to 16 steps with `then`. Quote names exactly as `screen` shows them.\ntap \"LABEL\" [number N] | tap eN | toggle \"LABEL\" | type \"TEXT\" in \"FIELD\" | scroll up|down|left|right [in \"AREA\"] [until \"TEXT\"] | press back|home|recents|notifications|enter | open app \"NAME\" | open link \"URL\" | wait for text \"TEXT\" up to N seconds | then find \"TEXT\"|screen (a look ends the chain) | verify text \"TEXT\" exists | page open \"URL\" | page click \"LABEL\" | page type \"TEXT\" in \"FIELD\"\nEvery action returns the new screen: check it shows what you meant before the next step. stale: nothing ran, retry. partial or unknown: read before acting again, never repeat. Ask the user first: purchases, messages, account or security changes, deletes, camera, microphone, location, recording. Other commands: android_read help",
            "parameters": input,
        }),
    ]
}

pub fn is_tool(name: &str) -> bool {
    name == READ_TOOL || name == ACT_TOOL
}

/// What a tool call returns: the phone's text, the screen image when au
/// attached one (`kind: image`, attached to the model input as `read` does
/// an image), or an error with guidance for the user.
pub fn run(name: &str, args: &Value, au: &Path) -> Value {
    let Some(command) = args
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
    else {
        return json!({ "error": "missing command" });
    };
    match call(au, name, command) {
        Ok(result) => tool_value(&result),
        Err(error) => {
            let mut value = json!({ "error": error.message });
            if let Some(hint) = error.hint {
                value["hint_for_user"] = json!(hint);
            }
            value
        }
    }
}

struct CallError {
    message: String,
    hint: Option<String>,
}

/// One tools/call result of au as LynShen's tool value.
fn tool_value(result: &Value) -> Value {
    let items = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let text = items
        .iter()
        .filter(|item| item["type"] == "text")
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut value = if result["isError"] == true {
        let status = result["structuredContent"]["status"]
            .as_str()
            .unwrap_or_default();
        let mut value = json!({ "error": text, "status": status });
        if let Some(hint) = error_hint(status) {
            value["hint_for_user"] = json!(hint);
        }
        value
    } else {
        json!({ "text": text })
    };
    if let Some(image) = items.iter().find(|item| {
        item["type"] == "image"
            && item["data"].as_str().is_some()
            && item["mimeType"]
                .as_str()
                .is_some_and(|mime| mime.starts_with("image/"))
    }) {
        value["kind"] = json!("image");
        value["mime"] = image["mimeType"].clone();
        value["base64"] = image["data"].clone();
    }
    value
}

/// Chinese guidance for an error au reports about the connection.
fn error_hint(status: &str) -> Option<String> {
    match status {
        // Not enrolled, or the enrolled phone is not connected: what the
        // phone's connection says decides the advice.
        "identity" | "device" => Some(hint(&device_phase(&adb_devices()))),
        "permission" => Some(hint("enable_accessibility")),
        "locked" | "screen_off" => Some("请解锁手机并保持亮屏，然后再试。".to_string()),
        "helper_killed" | "stopped" => Some(hint("install_helper")),
        _ => None,
    }
}

// ---- the server -------------------------------------------------------

struct Server {
    client: crate::mcp::McpClient,
    au: PathBuf,
    used: Instant,
}

fn server_slot() -> &'static Arc<Mutex<Option<Server>>> {
    static SLOT: OnceLock<Arc<Mutex<Option<Server>>>> = OnceLock::new();
    SLOT.get_or_init(|| {
        let slot = Arc::new(Mutex::new(None::<Server>));
        let reaper = Arc::clone(&slot);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(30));
            if let Ok(mut server) = reaper.lock() {
                if server
                    .as_ref()
                    .is_some_and(|server| server.used.elapsed() >= IDLE_STOP)
                {
                    *server = None;
                }
            }
        });
        slot
    })
}

/// Sends one command to the process's au server, starting it first. A
/// transport failure drops the server (the next call starts a new one); the
/// call is never sent again here, since an action may have run.
fn call(au: &Path, tool: &str, command: &str) -> Result<Value, CallError> {
    let mut slot = server_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.as_ref().is_some_and(|server| server.au != au) {
        *slot = None;
    }
    if slot.is_none() {
        *slot = Some(start(au)?);
    }
    let server = slot.as_mut().expect("started above");
    server.used = Instant::now();
    let result = server
        .client
        .call_tool_value(tool, json!({ "command": command }));
    server.used = Instant::now();
    result.map_err(|error| {
        *slot = None;
        CallError {
            message: format!("android-use stopped answering: {error}. Read the screen before acting again."),
            hint: None,
        }
    })
}

fn start(au: &Path) -> Result<Server, CallError> {
    let state = state_dir().map_err(|error| CallError {
        message: error.to_string(),
        hint: None,
    })?;
    fs::create_dir_all(&state).map_err(|error| CallError {
        message: format!("cannot create {}: {error}", state.display()),
        hint: None,
    })?;
    let client = crate::mcp::spawn_stdio_client(
        "android-use",
        &au.display().to_string(),
        &["serve".to_string(), "--mcp".to_string()],
        &au_env(),
        &state,
        CALL_TIMEOUT,
    )
    .map_err(|error| {
        // A second process (another LynShen, or the user's own au serve)
        // already drives the phone.
        let busy = error.contains("connection closed") || error.contains("another au serve");
        CallError {
            message: format!("android-use did not start: {error}"),
            hint: busy.then(|| "另一个程序（另一个 LynShen 或 au serve）正在控制这台手机。关掉它后再试。".to_string()),
        }
    })?;
    Ok(Server {
        client,
        au: au.to_path_buf(),
        used: Instant::now(),
    })
}

/// Stops the process's au server now (turned off, reinstalled).
pub fn stop_server() {
    if let Ok(mut slot) = server_slot().lock() {
        *slot = None;
    }
}

/// au's environment: LynShen's own state folder, so it never fights over
/// the lock with an au the user runs for another agent, and the adb found
/// here (au itself looks only at AU_ADB, the SDK variables and PATH).
fn au_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Ok(state) = state_dir() {
        env.insert("AU_HOME".to_string(), state.display().to_string());
    }
    if let Some(adb) = find_adb() {
        env.insert("AU_ADB".to_string(), adb.display().to_string());
    }
    env
}

// ---- install ----------------------------------------------------------

/// `~/.lynshen/tools/android-use`.
pub fn tools_dir() -> io::Result<PathBuf> {
    Ok(crate::config::profile_dir()?
        .join("tools")
        .join("android-use"))
}

fn state_dir() -> io::Result<PathBuf> {
    Ok(tools_dir()?.join("state"))
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "au.exe"
    } else {
        "au"
    }
}

/// The installed au of `VERSION`, if any.
pub fn installed_binary() -> Option<PathBuf> {
    let au = tools_dir().ok()?.join(VERSION).join(binary_name());
    au.is_file().then_some(au)
}

/// This platform's release asset and its pinned SHA-256.
pub fn platform_asset() -> Option<(&'static str, &'static str)> {
    asset_for(env::consts::OS, env::consts::ARCH)
}

fn asset_for(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    ASSETS
        .iter()
        .find(|(o, a, _, _)| *o == os && *a == arch)
        .map(|(_, _, asset, sha)| (*asset, *sha))
}

pub fn asset_url(asset: &str) -> String {
    format!("{RELEASES}/v{VERSION}/{asset}")
}

/// Downloads `VERSION` for this platform, checks it and installs it.
/// Returns the installed binary.
pub fn install() -> Result<PathBuf, String> {
    let (asset, sha256) = platform_asset().ok_or_else(|| {
        format!(
            "android-use {VERSION} has no build for {} {}",
            env::consts::OS,
            env::consts::ARCH
        )
    })?;
    let bytes = crate::skills::fetch(
        &asset_url(asset),
        MAX_ARCHIVE_BYTES,
        crate::skills::INSTALL_TIMEOUTS,
    )
    .map_err(|error| format!("download failed: {error}"))?;
    let dir = tools_dir()
        .map_err(|error| error.to_string())?
        .join(VERSION);
    stop_server();
    install_archive(&bytes, sha256, archive_kind(asset), &dir)
}

fn archive_kind(asset: &str) -> &'static str {
    if asset.ends_with(".zip") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Checks `bytes` against `sha256` before anything is written, unpacks
/// them next to `dir` and moves them into place once au is there.
pub(crate) fn install_archive(
    bytes: &[u8],
    sha256: &str,
    kind: &str,
    dir: &Path,
) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(sha256) {
        return Err(format!(
            "checksum mismatch: expected {sha256}, got {actual}; nothing was installed"
        ));
    }
    let parent = dir
        .parent()
        .ok_or_else(|| "install directory has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let staging = parent.join(format!(".{VERSION}-staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let unpacked = match kind {
        "zip" => crate::skills::extract_zip(bytes, &staging),
        _ => crate::skills::extract_tar_gz(bytes, &staging),
    };
    let au = staging.join(binary_name());
    if let Err(error) = unpacked.and_then(|()| {
        if au.is_file() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "the archive has no {}",
                binary_name()
            )))
        }
    }) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("unpacking failed: {error}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&au, fs::Permissions::from_mode(0o755));
    }
    let _ = fs::remove_dir_all(dir);
    fs::rename(&staging, dir).map_err(|error| {
        let _ = fs::remove_dir_all(&staging);
        format!("cannot move into {}: {error}", dir.display())
    })?;
    Ok(dir.join(binary_name()))
}

// ---- settings ---------------------------------------------------------

/// Turns the phone tools on or off in config.json (read again each turn).
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let path = crate::config::profile_dir()
        .map_err(|error| error.to_string())?
        .join("config.json");
    crate::config::edit_config_json(&path, |config| {
        config.insert(CONFIG_KEY.to_string(), json!(enabled));
        Ok(())
    })?;
    if !enabled {
        stop_server();
    }
    Ok(())
}

/// The au a session may use: on and installed; else why the tools are off.
pub fn availability(enabled: bool) -> Result<PathBuf, String> {
    if !enabled {
        return Err(
            "Android phone control is off. The user turns it on in Settings (安卓手机) or with /android on."
                .to_string(),
        );
    }
    installed_binary().ok_or_else(|| {
        format!("android-use {VERSION} is not installed. The user installs it in Settings (安卓手机) or with /android install.")
    })
}

// ---- status and setup -------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String,
    pub model: String,
}

/// `adb devices -l`, parsed.
pub(crate) fn parse_adb_devices(text: &str) -> Vec<AdbDevice> {
    text.lines()
        .skip_while(|line| !line.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let serial = words.next()?.to_string();
            let state = words.next()?.to_string();
            let model = words
                .find_map(|word| word.strip_prefix("model:"))
                .unwrap_or_default()
                .replace('_', " ");
            Some(AdbDevice {
                serial,
                state,
                model,
            })
        })
        .collect()
}

fn adb_devices() -> Option<Vec<AdbDevice>> {
    let adb = find_adb()?;
    let output = run_quiet(&adb, &["devices", "-l"], QUICK)?;
    Some(parse_adb_devices(&output))
}

/// au's phase names, for the phone's connection alone (no adb: None).
fn device_phase(devices: &Option<Vec<AdbDevice>>) -> String {
    match devices {
        None => "install_platform_tools",
        Some(devices) if devices.iter().any(|d| d.state == "device") => {
            if devices.iter().filter(|d| d.state == "device").count() > 1 {
                "choose_device"
            } else {
                "setup_host"
            }
        }
        Some(devices) if devices.iter().any(|d| d.state == "unauthorized") => "approve_usb",
        Some(devices) if devices.iter().any(|d| d.state == "offline") => "offline",
        Some(_) => "connect_device",
    }
    .to_string()
}

/// What the user does next, in Chinese, for an au phase.
pub fn hint(phase: &str) -> String {
    match phase {
        "install_platform_tools" => "电脑上没有找到 adb（Android 平台工具）。请安装 Android SDK Platform-Tools（https://developer.android.com/tools/releases/platform-tools ，macOS 也可以运行 brew install android-platform-tools），或在环境变量 AU_ADB 里写 adb 的完整路径。",
        "connect_device" => "没有连接安卓手机。① 用能传数据的 USB 线连接手机并解锁；② 打开 设置 → 关于手机，连点「版本号」7 次打开开发者选项；③ 在 设置 → 开发者选项 里打开「USB 调试」；④ 手机弹出「允许 USB 调试吗？」时勾选「始终允许使用这台计算机进行调试」，再点「允许」。",
        "approve_usb" => "手机在等你确认 USB 调试：解锁手机，在「允许 USB 调试吗？」弹窗（显示这台电脑的 RSA 密钥指纹）里点「允许」。没看到弹窗就拔插一次数据线；还不行就在开发者选项里点「撤销 USB 调试授权」后重新连接。",
        "offline" => "手机显示离线：解锁手机，拔插一次数据线后再试。",
        "choose_device" => "连接了不止一台安卓设备。请只保留要控制的那一台，再点「连接手机」。",
        "reconnect_device" => "之前连接的手机现在没有连上。请用数据线连接并解锁它；换了手机需要重新点「连接手机」。",
        "setup_host" | "install_helper" => "需要在手机上安装 Android-Use 助手：在设置里点「连接手机」（命令行里运行 /android setup）。手机上会出现安装提示，按提示允许。",
        "enable_accessibility" => "在手机上打开 设置 → 无障碍 → Android-Use（有的手机在「已下载的应用」或「已安装的服务」里），开启并同意系统提示，保持手机解锁，然后再试。",
        "ready" => "已就绪：可以让 AI 操作这台手机了。每一步操作都会先请你确认。",
        _ => "运行「连接手机」检查哪里还没准备好。",
    }
    .to_string()
}

/// How to connect without a cable, for the settings page.
pub const WIRELESS_HINT: &str = "无线连接（Android 11 及以上）：手机和电脑连同一个 Wi-Fi，在 开发者选项 → 无线调试 里打开它并点「使用配对码配对设备」，在电脑终端运行 adb pair 手机显示的IP:端口 并输入配对码，再运行 adb connect IP:端口。无线调试让配对过的电脑都能控制手机，用完请关掉。";

/// The settings page's view: installed, on, adb and the phones, the next
/// step. `doctor` runs au's own check (slower; it asks the helper).
pub fn status(enabled: bool, doctor: bool) -> Value {
    let au = installed_binary();
    let adb = find_adb();
    let devices = adb_devices();
    let mut phase = device_phase(&devices);
    if let (Some(au), true) = (&au, doctor) {
        if let Some(report) = run_au_json(au, &["doctor", "--json"], Duration::from_secs(30)) {
            if let Some(found) = report["phase"].as_str() {
                phase = found.to_string();
            }
        }
    }
    let ready = phase == "ready";
    json!({
        "type": "android_use",
        "enabled": enabled,
        "version": VERSION,
        "installed": au.is_some(),
        "asset": platform_asset().map(|(asset, _)| asset),
        "adb": adb.map(|adb| adb.display().to_string()),
        "devices": devices.unwrap_or_default().iter().map(|d| json!({
            "serial": d.serial,
            "state": d.state,
            "model": d.model,
        })).collect::<Vec<_>>(),
        "phase": phase,
        "ready": ready,
        "hint": hint(&phase),
        "wireless_hint": WIRELESS_HINT,
        "license": "MIT",
        "source": "https://github.com/austinintelligence/android-use",
    })
}

/// `au setup`: enrolls the one connected phone and installs the helper app
/// on it, which then opens Accessibility for the user to switch it on.
pub fn setup() -> Result<Value, String> {
    let au = installed_binary()
        .ok_or_else(|| format!("android-use {VERSION} is not installed"))?;
    // The server holds the state lock; setup needs it.
    stop_server();
    let report = run_au_json(&au, &["setup", "--json", "--no-agents"], SETUP_TIMEOUT)
        .ok_or_else(|| "au setup did not answer".to_string())?;
    let phase = match report["phase"].as_str() {
        Some(phase) => phase.to_string(),
        // An error report ({"ok":0,"e":...}): the connection decides.
        None => device_phase(&adb_devices()),
    };
    Ok(json!({
        "type": "android_use_setup",
        "phase": phase,
        "ready": report["ready"] == 1,
        "hint": hint(&phase),
    }))
}

fn run_au_json(au: &Path, args: &[&str], timeout: Duration) -> Option<Value> {
    let mut command = Command::new(au);
    command.args(args).envs(au_env());
    if let Ok(state) = state_dir() {
        let _ = fs::create_dir_all(&state);
    }
    let output = run_with_timeout(command, timeout)?;
    // au prints one JSON line on stdout, also on failure.
    output
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
}

fn run_quiet(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    run_with_timeout(command, timeout)
}

/// stdout of `command`, or None when it fails to start or outlasts
/// `timeout` (then it is killed).
fn run_with_timeout(mut command: Command, timeout: Duration) -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = io::Read::read_to_string(&mut stdout, &mut text);
        text
    });
    let deadline = Instant::now() + timeout;
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
    reader.join().ok()
}

/// adb as au would find it, and in the SDK's default folders too: a
/// service (the daemon) often runs without the user's PATH.
pub fn find_adb() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    let mut candidates = Vec::new();
    if let Some(adb) = env::var_os("AU_ADB") {
        candidates.push(PathBuf::from(adb));
    }
    for key in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(sdk) = env::var_os(key) {
            candidates.push(PathBuf::from(sdk).join("platform-tools").join(exe));
        }
    }
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Android/Sdk/platform-tools")
                .join(exe),
        );
    }
    if let Some(home) = crate::secrets::home_dir() {
        candidates.push(home.join("Library/Android/sdk/platform-tools").join(exe));
        candidates.push(home.join("Android/Sdk/platform-tools").join(exe));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(exe));
    candidates.push(PathBuf::from("/usr/local/bin").join(exe));
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return Some(path);
    }
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(exe))
            .find(|path| path.is_file())
    })
}

// ---- approval text ----------------------------------------------------

/// The approval's text for a phone call, in Chinese: 「在 微信 里点「发送」」.
/// The app is the one a step opens, else the phone's foreground app.
pub fn approval_summary(name: &str, arguments: &str) -> String {
    let command = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|args| args["command"].as_str().map(str::to_string))
        .unwrap_or_default();
    let steps = split_steps(&command);
    let needs_app = steps.first().is_some_and(|step| {
        !matches!(
            step.first().map(String::as_str),
            Some("open" | "press" | "page" | "new" | "select" | "close")
        )
    });
    let app = if needs_app {
        foreground_package().map(|package| app_label(&package))
    } else {
        None
    };
    describe(name, &command, app.as_deref())
}

/// `describe` without the phone: `app` is the foreground app's name.
pub(crate) fn describe(name: &str, command: &str, app: Option<&str>) -> String {
    let steps = split_steps(command);
    if steps.is_empty() {
        return command.to_string();
    }
    if name == READ_TOOL {
        return describe_read(&steps[0], app);
    }
    let mut app = app.map(str::to_string);
    let mut parts = Vec::new();
    for step in &steps {
        let (text, next_app) = describe_step(step, app.as_deref());
        parts.push(text);
        if let Some(next) = next_app {
            app = Some(next);
        }
    }
    parts.join("，然后")
}

fn describe_read(step: &[String], app: Option<&str>) -> String {
    let word = |index: usize| step.get(index).map(String::as_str).unwrap_or("");
    let place = app.map(|app| format!(" {app} ")).unwrap_or_default();
    match word(0) {
        "screen" => format!("读取手机上{place}的屏幕内容"),
        "find" => format!("在手机屏幕上找「{}」", word(1)),
        "page" => "读取 Chrome 里当前网页的内容".to_string(),
        "notifications" => "读取手机通知".to_string(),
        "browser" => "查看 Chrome 打开的标签页".to_string(),
        "location" => "读取手机定位".to_string(),
        "status" | "help" | "capabilities" => "查看 Android-Use 的状态".to_string(),
        _ => format!("查看手机：{}", step.join(" ")),
    }
}

/// One step in Chinese, and the app the phone shows after it when the
/// step opens one.
fn describe_step(step: &[String], app: Option<&str>) -> (String, Option<String>) {
    let word = |index: usize| step.get(index).map(String::as_str).unwrap_or("");
    let lower = |index: usize| word(index).to_ascii_lowercase();
    let in_app = |action: String| match app {
        Some(app) => format!("在 {app} 里{action}"),
        None => format!("在手机上{action}"),
    };
    let numbered = |index: usize| {
        if lower(index) == "number" {
            format!("（第 {} 个）", word(index + 1))
        } else {
            String::new()
        }
    };
    match (lower(0).as_str(), lower(1).as_str()) {
        ("tap", "point") => (in_app(format!("点坐标 ({}, {})", word(2), word(3))), None),
        ("tap", target) if target.starts_with('e') && target[1..].parse::<u32>().is_ok() => {
            (in_app(format!("点屏幕上标号 {} 的控件", word(1))), None)
        }
        ("tap" | "click", _) => (in_app(format!("点「{}」{}", word(1), numbered(2))), None),
        ("hold", _) => (in_app(format!("长按「{}」", word(1))), None),
        ("toggle", _) => (in_app(format!("切换「{}」开关", word(1))), None),
        ("set" | "turn", _) => (in_app(format!("设置「{}」", step[1..].join(" "))), None),
        ("type", _) => (
            in_app(format!("在「{}」里输入「{}」", word(3), word(1))),
            None,
        ),
        ("scroll", direction) => {
            let way = match direction {
                "up" => "向上",
                "down" => "向下",
                "left" => "向左",
                _ => "向右",
            };
            let until = step
                .iter()
                .position(|word| word.eq_ignore_ascii_case("until"))
                .map(|at| format!("，直到出现「{}」", word(at + 1)))
                .unwrap_or_default();
            (in_app(format!("{way}滚动{until}")), None)
        }
        ("swipe", _) => (in_app("滑动屏幕".to_string()), None),
        ("press", key) => {
            let (text, next) = match key {
                "back" => ("按返回键", None),
                "home" => ("回到主屏幕", Some("主屏幕")),
                "recents" => ("打开最近任务", None),
                "notifications" => ("下拉通知栏", None),
                "enter" => ("按回车", None),
                _ => ("按键", None),
            };
            (text.to_string(), next.map(str::to_string))
        }
        ("open", "app") => (format!("打开「{}」", word(2)), Some(word(2).to_string())),
        ("open", "link") => (format!("打开链接 {}", word(2)), None),
        ("open", "setting") => (
            format!("打开系统设置里的「{}」", word(2)),
            Some("设置".to_string()),
        ),
        ("open", "notification") => (format!("打开通知「{}」", word(2)), None),
        ("dismiss", _) => (format!("清除通知「{}」", word(2)), None),
        ("run", _) => (format!("执行通知操作「{}」", word(3)), None),
        ("wait", _) => {
            let text = step
                .iter()
                .skip(1)
                .find(|word| !matches!(word.to_ascii_lowercase().as_str(), "for" | "text"))
                .cloned()
                .unwrap_or_default();
            (format!("等待「{text}」出现"), None)
        }
        ("verify", _) => ("确认屏幕上的内容".to_string(), None),
        ("find" | "screen", _) => ("看一下屏幕".to_string(), None),
        ("page", action) => {
            let text = match action {
                "open" => format!("在 Chrome 里打开 {}", word(2)),
                "click" => format!("在网页上点「{}」", word(2)),
                "type" => format!("在网页的「{}」里输入「{}」", word(4), word(2)),
                "press" => format!("在网页上按 {}", word(2)),
                "back" => "网页后退".to_string(),
                "forward" => "网页前进".to_string(),
                "reload" => "刷新网页".to_string(),
                "screenshot" => "给网页截图".to_string(),
                _ => format!("操作网页：{}", step.join(" ")),
            };
            (text, Some("Chrome".to_string()))
        }
        ("new", _) => (
            format!("在 Chrome 新标签页打开 {}", word(2)),
            Some("Chrome".to_string()),
        ),
        ("select", _) => (format!("切换到 Chrome 标签页「{}」", word(2)), None),
        ("close", _) => (format!("关闭 Chrome 标签页「{}」", word(2)), None),
        ("capture", _) => ("截取手机屏幕".to_string(), None),
        ("take", facing) => (
            format!(
                "用{}摄像头拍照",
                if facing == "front" { "前置" } else { "后置" }
            ),
            None,
        ),
        ("record", what) => (
            format!(
                "{} {} 秒",
                if what == "microphone" {
                    "用麦克风录音"
                } else {
                    "录屏"
                },
                word(3)
            ),
            None,
        ),
        _ => (format!("操作手机：{}", step.join(" ")), None),
    }
}

/// An action a person decides in every mode: the camera, the microphone,
/// recording the screen.
pub fn always_asks(name: &str, arguments: &str) -> bool {
    if name != ACT_TOOL {
        return false;
    }
    let command = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|args| args["command"].as_str().map(str::to_string))
        .unwrap_or_default();
    split_steps(&command).iter().any(|step| {
        let first = step.first().map(|w| w.to_ascii_lowercase()).unwrap_or_default();
        matches!(first.as_str(), "take" | "record")
    })
}

/// au's command split into `then` steps of words and quoted strings
/// (unquoted, escapes resolved).
fn split_steps(command: &str) -> Vec<Vec<String>> {
    let mut steps = vec![Vec::new()];
    let mut chars = command.chars().peekable();
    while let Some(first) = chars.next() {
        if first.is_whitespace() {
            continue;
        }
        let quoted = matches!(first, '"' | '“' | '”');
        let mut token = String::new();
        if quoted {
            while let Some(current) = chars.next() {
                match current {
                    '"' | '“' | '”' => break,
                    '\\' => match chars.next() {
                        Some('n') => token.push('\n'),
                        Some('t') => token.push('\t'),
                        Some(other) => token.push(other),
                        None => {}
                    },
                    other => token.push(other),
                }
            }
        } else {
            token.push(first);
            while let Some(&next) = chars.peek() {
                if next.is_whitespace() || matches!(next, '"' | '“' | '”') {
                    break;
                }
                token.push(next);
                chars.next();
            }
        }
        if !quoted && token.eq_ignore_ascii_case("then") {
            steps.push(Vec::new());
            continue;
        }
        steps.last_mut().expect("one step at least").push(token);
    }
    steps.retain(|step| !step.is_empty());
    steps
}

/// The phone's foreground app package, from adb (au does not report it).
fn foreground_package() -> Option<String> {
    let adb = find_adb()?;
    let endpoint = fs::read_to_string(state_dir().ok()?.join("device.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|device| device["endpoint"].as_str().map(str::to_string));
    let mut args = Vec::new();
    if let Some(endpoint) = &endpoint {
        args.extend(["-s", endpoint.as_str()]);
    }
    args.extend(["shell", "dumpsys", "activity", "activities"]);
    let output = run_quiet(&adb, &args, Duration::from_secs(3))?;
    parse_resumed_package(&output)
}

/// The package of the resumed activity in `dumpsys activity activities`.
pub(crate) fn parse_resumed_package(dumpsys: &str) -> Option<String> {
    dumpsys
        .lines()
        .filter(|line| line.contains("ResumedActivity"))
        .find_map(|line| {
            line.split_whitespace().find_map(|word| {
                let (package, _) = word.split_once('/')?;
                (package.contains('.')
                    && package
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                .then(|| package.to_string())
            })
        })
}

/// An app's name for the approval text: common apps by name, the rest by
/// package.
pub(crate) fn app_label(package: &str) -> String {
    const NAMES: [(&str, &str); 40] = [
        ("com.tencent.mm", "微信"),
        ("com.tencent.mobileqq", "QQ"),
        ("com.tencent.wework", "企业微信"),
        ("com.eg.android.AlipayGphone", "支付宝"),
        ("com.taobao.taobao", "淘宝"),
        ("com.tmall.wireless", "天猫"),
        ("com.jingdong.app.mall", "京东"),
        ("com.xunmeng.pinduoduo", "拼多多"),
        ("com.sankuai.meituan", "美团"),
        ("me.ele", "饿了么"),
        ("com.ss.android.ugc.aweme", "抖音"),
        ("com.smile.gifmaker", "快手"),
        ("com.xingin.xhs", "小红书"),
        ("com.sina.weibo", "微博"),
        ("tv.danmaku.bili", "哔哩哔哩"),
        ("com.zhihu.android", "知乎"),
        ("com.netease.cloudmusic", "网易云音乐"),
        ("com.tencent.qqmusic", "QQ 音乐"),
        ("com.alibaba.android.rimet", "钉钉"),
        ("com.ss.android.lark", "飞书"),
        ("com.autonavi.minimap", "高德地图"),
        ("com.baidu.BaiduMap", "百度地图"),
        ("com.sdu.didi.psnger", "滴滴出行"),
        ("com.baidu.searchbox", "百度"),
        ("com.android.settings", "设置"),
        ("com.android.chrome", "Chrome"),
        ("com.android.vending", "Play 商店"),
        ("com.google.android.youtube", "YouTube"),
        ("com.google.android.gm", "Gmail"),
        ("com.google.android.apps.maps", "Google 地图"),
        ("com.google.android.apps.messaging", "信息"),
        ("com.google.android.dialer", "电话"),
        ("com.google.android.contacts", "通讯录"),
        ("com.google.android.calendar", "日历"),
        ("com.google.android.deskclock", "时钟"),
        ("com.google.android.apps.photos", "相册"),
        ("com.google.android.GoogleCamera", "相机"),
        ("com.android.camera", "相机"),
        ("com.google.android.apps.nexuslauncher", "主屏幕"),
        ("dev.codex.aubridge", "Android-Use"),
    ];
    NAMES
        .iter()
        .find(|(id, _)| *id == package)
        .map(|(_, name)| name.to_string())
        .or_else(|| package.contains("launcher").then(|| "主屏幕".to_string()))
        .unwrap_or_else(|| package.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_platform_has_a_pinned_archive_and_checksum() {
        for os in ["macos", "linux", "windows"] {
            for arch in ["aarch64", "x86_64"] {
                let (asset, sha) = asset_for(os, arch).expect("pinned");
                assert!(asset.contains(VERSION) && asset.contains(os) && asset.contains(arch));
                assert_eq!(sha.len(), 64);
                assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
                assert_eq!(archive_kind(asset), if os == "windows" { "zip" } else { "tar.gz" });
            }
        }
        assert!(asset_for("freebsd", "x86_64").is_none());
        assert_eq!(
            asset_url("android-use-v1.2.0-macos-aarch64.tar.gz"),
            "https://github.com/austinintelligence/android-use/releases/download/v1.2.0/android-use-v1.2.0-macos-aarch64.tar.gz"
        );
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-android-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A release-shaped tar.gz: au and LICENSE at the top.
    fn archive(with_binary: bool) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut add = |name: &str, body: &[u8], mode: u32| {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder.append_data(&mut header, name, body).unwrap();
        };
        if with_binary {
            add(binary_name(), b"#!/bin/sh\necho '{\"version\":\"1.2.0\"}'\n", 0o644);
        }
        add("LICENSE", b"MIT License", 0o644);
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn a_checked_archive_is_installed_with_an_executable_au() {
        let root = temp_dir("install");
        let dir = root.join(VERSION);
        let bytes = archive(true);
        let au = install_archive(&bytes, &sha(&bytes), "tar.gz", &dir).unwrap();
        assert_eq!(au, dir.join(binary_name()));
        assert!(au.is_file() && dir.join("LICENSE").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&au).unwrap().permissions().mode() & 0o777, 0o755);
        }
        // No staging folder is left beside it.
        let left = fs::read_dir(&root).unwrap().count();
        assert_eq!(left, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_wrong_checksum_installs_nothing_and_keeps_the_old_install() {
        let root = temp_dir("mismatch");
        let dir = root.join(VERSION);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(binary_name()), b"old").unwrap();
        let bytes = archive(true);
        let error = install_archive(&bytes, &"0".repeat(64), "tar.gz", &dir).unwrap_err();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert_eq!(fs::read(dir.join(binary_name())).unwrap(), b"old");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_archive_without_au_is_refused() {
        let root = temp_dir("nobinary");
        let dir = root.join(VERSION);
        let bytes = archive(false);
        let error = install_archive(&bytes, &sha(&bytes), "tar.gz", &dir).unwrap_err();
        assert!(error.contains("has no"), "{error}");
        assert!(!dir.exists());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn approvals_name_the_action_and_the_app_in_chinese() {
        let act = |command: &str, app: Option<&str>| describe(ACT_TOOL, command, app);
        assert_eq!(act(r#"tap "发送""#, Some("微信")), "在 微信 里点「发送」");
        assert_eq!(
            act(r#"open app "微信" then type "晚上好" in "输入框" then tap "发送""#, None),
            "打开「微信」，然后在 微信 里在「输入框」里输入「晚上好」，然后在 微信 里点「发送」"
        );
        assert_eq!(
            act(r#"tap "Save" number 2"#, Some("设置")),
            "在 设置 里点「Save」（第 2 个）"
        );
        assert_eq!(act("tap e7", None), "在手机上点屏幕上标号 e7 的控件");
        assert_eq!(
            act(r#"press home then open app "Chrome" then page open "https://example.com""#, None),
            "回到主屏幕，然后打开「Chrome」，然后在 Chrome 里打开 https://example.com"
        );
        assert_eq!(
            act(r#"scroll down until "Wi-Fi""#, Some("设置")),
            "在 设置 里向下滚动，直到出现「Wi-Fi」"
        );
        assert_eq!(act(r#"toggle "Airplane mode""#, Some("设置")), "在 设置 里切换「Airplane mode」开关");
        // A `then` inside quotes is text, not a step.
        assert_eq!(act(r#"tap "then what""#, Some("QQ")), "在 QQ 里点「then what」");
        assert_eq!(
            describe(READ_TOOL, "screen", Some("设置")),
            "读取手机上 设置 的屏幕内容"
        );
        assert_eq!(describe(READ_TOOL, "page", None), "读取 Chrome 里当前网页的内容");
    }

    #[test]
    fn camera_microphone_and_recording_always_ask() {
        let args = |command: &str| json!({ "command": command }).to_string();
        assert!(always_asks(ACT_TOOL, &args("take rear camera photo")));
        assert!(always_asks(ACT_TOOL, &args(r#"open app "Camera" then record screen for 5 seconds"#)));
        assert!(!always_asks(ACT_TOOL, &args(r#"tap "Record""#)));
        assert!(!always_asks(READ_TOOL, &args("take rear camera photo")));
    }

    #[test]
    fn the_foreground_app_comes_from_the_resumed_activity() {
        let dumpsys = "  Stack #1:\n    mResumedActivity: ActivityRecord{1a2b u0 com.tencent.mm/.ui.LauncherUI t12}\n";
        assert_eq!(parse_resumed_package(dumpsys).as_deref(), Some("com.tencent.mm"));
        let newer = "  topResumedActivity=ActivityRecord{9f u0 com.android.settings/.Settings t7}\n";
        assert_eq!(parse_resumed_package(newer).as_deref(), Some("com.android.settings"));
        assert_eq!(parse_resumed_package("nothing here"), None);
        assert_eq!(app_label("com.tencent.mm"), "微信");
        assert_eq!(app_label("com.example.notes"), "com.example.notes");
        assert_eq!(app_label("com.miui.home.launcher"), "主屏幕");
    }

    #[test]
    fn devices_and_their_state_give_chinese_guidance() {
        let listed = "List of devices attached\nR58N123 unauthorized usb:1-1 transport_id:3\nemulator-5554 device product:sdk_gphone64 model:sdk_gphone64_arm64 device:emu64a transport_id:1\n";
        let devices = parse_adb_devices(listed);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].state, "unauthorized");
        assert_eq!(devices[1].model, "sdk gphone64 arm64");
        assert_eq!(device_phase(&None), "install_platform_tools");
        assert_eq!(device_phase(&Some(Vec::new())), "connect_device");
        assert_eq!(device_phase(&Some(devices[..1].to_vec())), "approve_usb");
        assert_eq!(device_phase(&Some(devices.clone())), "setup_host");
        assert!(hint("approve_usb").contains("允许 USB 调试"));
        assert!(hint("connect_device").contains("USB 调试"));
        assert!(hint("install_platform_tools").contains("adb"));
        assert!(WIRELESS_HINT.contains("adb pair"));
    }

    #[test]
    fn au_results_become_text_images_or_errors_with_guidance() {
        let ok = tool_value(&json!({
            "content": [{ "type": "text", "text": "Done.\nNow on screen:\nWi-Fi" }],
            "isError": false
        }));
        assert_eq!(ok, json!({ "text": "Done.\nNow on screen:\nWi-Fi" }));
        let image = tool_value(&json!({
            "content": [
                { "type": "text", "text": "The screen image is attached" },
                { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" }
            ]
        }));
        assert_eq!(image["kind"], "image");
        assert_eq!(image["mime"], "image/png");
        assert_eq!(image["base64"], "iVBORw0KGgo=");
        let locked = tool_value(&json!({
            "content": [{ "type": "text", "text": "The phone is locked." }],
            "isError": true,
            "structuredContent": { "status": "locked" }
        }));
        assert_eq!(locked["error"], "The phone is locked.");
        assert!(locked["hint_for_user"].as_str().unwrap().contains("解锁"));
        let permission = tool_value(&json!({
            "content": [{ "type": "text", "text": "off" }],
            "isError": true,
            "structuredContent": { "status": "permission" }
        }));
        assert!(permission["hint_for_user"].as_str().unwrap().contains("无障碍"));
    }

    #[test]
    fn the_tools_are_offered_with_one_command_string() {
        let definitions = definitions();
        let names: Vec<_> = definitions.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, [READ_TOOL, ACT_TOOL]);
        for definition in &definitions {
            assert_eq!(definition["parameters"]["required"], json!(["command"]));
        }
        assert!(is_tool(ACT_TOOL) && is_tool(READ_TOOL) && !is_tool("bash"));
        assert!(run(ACT_TOOL, &json!({}), Path::new("/nonexistent/au"))["error"]
            .as_str()
            .unwrap()
            .contains("missing command"));
        assert!(availability(false).unwrap_err().contains("off"));
    }
}

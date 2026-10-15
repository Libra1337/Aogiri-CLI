//! The shell the model's commands run in, and the one a terminal opens:
//! `shell` and `terminal_shell` in config.json (see `docs/sandbox.md`,
//! "Shell").
//!
//! Detection looks for the shells a platform offers: PowerShell 7, Windows
//! PowerShell 5.1 and Git Bash on Windows; zsh, bash and sh elsewhere. "auto"
//! takes the first installed of `WINDOWS_AUTO_ORDER` on Windows and the
//! user's login shell elsewhere.
//!
//! A zsh or bash takes its environment from one snapshot of an interactive
//! login shell (`snapshot`): what ~/.zprofile and ~/.zshrc set up (PATH for
//! Homebrew, nvm, pyenv or conda, proxies) reaches the commands, which then
//! run with `-c`. The snapshot runs once per process and setting, with a
//! timeout; without one, commands run as a login shell (`-lc`), and when
//! even that does not start, in `/bin/sh -c`.

use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// What "auto" takes on Windows: the first of these that is installed. Most
/// models write bash, so Git Bash comes first; put `Kind::Pwsh` first to
/// prefer PowerShell 7.
pub const WINDOWS_AUTO_ORDER: [Kind; 3] = [Kind::GitBash, Kind::Pwsh, Kind::WindowsPowerShell];

/// How long a shell may take to print its version.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the login shell may take to read its rc files for the snapshot,
/// and to start as a login shell when the snapshot failed.
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
/// Names the file the snapshot writes to, in the snapshot shell's
/// environment.
const SNAPSHOT_FILE_VAR: &str = "LYNSHEN_ENV_SNAPSHOT";
/// Ends a complete snapshot.
const SNAPSHOT_END: &str = "__LYNSHEN_ENV_END__";
/// Variables the snapshot leaves out: the snapshot shell's own state.
const SNAPSHOT_SKIP: [&str; 5] = ["PWD", "OLDPWD", "SHLVL", "_", SNAPSHOT_FILE_VAR];

/// Runs before each PowerShell command: UTF-8 both ways (else Chinese text
/// is garbled under a GBK console code page), and no progress bars. Setting
/// the console's encoding fails without a console, which is fine.
pub const POWERSHELL_PRELUDE: &str = "$OutputEncoding = New-Object System.Text.UTF8Encoding $false; try { [Console]::OutputEncoding = $OutputEncoding } catch {}; try { [Console]::InputEncoding = $OutputEncoding } catch {}; $ProgressPreference = 'SilentlyContinue'";

/// Set for the model's commands: no colours, no pager, no prompt for git
/// credentials (`GIT_TERMINAL_PROMPT` only when it is not set).
pub const MODEL_ENV: [(&str, &str); 4] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    /// Linux and other Unix systems.
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::MacOs => "macos",
            Self::Linux => "linux",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// PowerShell 7 (`pwsh.exe`).
    Pwsh,
    /// Windows PowerShell 5.1 (`powershell.exe`).
    WindowsPowerShell,
    /// Git for Windows' bash.
    GitBash,
    Zsh,
    Bash,
    /// `sh`, or another POSIX shell under its own name (dash, ksh).
    Sh,
}

impl Kind {
    const ALL: [Kind; 6] = [
        Kind::Pwsh,
        Kind::WindowsPowerShell,
        Kind::GitBash,
        Kind::Zsh,
        Kind::Bash,
        Kind::Sh,
    ];

    /// The value in config.json.
    pub fn id(self) -> &'static str {
        match self {
            Self::Pwsh => "pwsh",
            Self::WindowsPowerShell => "powershell",
            Self::GitBash => "gitbash",
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Sh => "sh",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Pwsh => "PowerShell 7",
            Self::WindowsPowerShell => "Windows PowerShell 5.1",
            Self::GitBash => "Git Bash",
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Sh => "sh",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }

    pub fn powershell(self) -> bool {
        matches!(self, Self::Pwsh | Self::WindowsPowerShell)
    }

    /// The shells settings offer on `platform`, in their order.
    pub fn offered(platform: Platform) -> &'static [Kind] {
        match platform {
            Platform::Windows => &[Kind::Pwsh, Kind::WindowsPowerShell, Kind::GitBash],
            _ => &[Kind::Zsh, Kind::Bash, Kind::Sh],
        }
    }

    /// A shell program's kind, from its file name; None for a program the
    /// model's commands cannot run in (fish, cmd).
    pub fn of_program(path: &Path, platform: Platform) -> Option<Self> {
        match program_name(path).as_str() {
            "pwsh" => Some(Self::Pwsh),
            "powershell" => Some(Self::WindowsPowerShell),
            "bash" if platform == Platform::Windows => Some(Self::GitBash),
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "sh" | "dash" | "ash" | "ksh" | "mksh" => Some(Self::Sh),
            _ => None,
        }
    }

    /// The name with the version, as the model and the user read it:
    /// "PowerShell 7.5.2", "Git Bash (bash 5.2.37)", "zsh 5.9".
    pub fn label(self, version: Option<&str>) -> String {
        match (self, version) {
            (Self::Pwsh, Some(version)) => format!("PowerShell {version}"),
            (Self::WindowsPowerShell, Some(version)) => format!("Windows PowerShell {version}"),
            (Self::GitBash, Some(version)) => format!("Git Bash (bash {version})"),
            (Self::Sh, Some(version)) => format!("sh ({version})"),
            (_, Some(version)) => format!("{} {version}", self.id()),
            (_, None) => self.name().to_string(),
        }
    }
}

/// `pwsh` for `C:\Program Files\PowerShell\7\pwsh.exe`.
fn program_name(path: &Path) -> String {
    let text = path.to_string_lossy();
    let name = text
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&text)
        .to_lowercase();
    name.strip_suffix(".exe")
        .map(str::to_string)
        .unwrap_or(name)
}

/// The `shell` (or `terminal_shell`) setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Auto,
    Kind(Kind),
    /// An absolute path to a shell program.
    Path(PathBuf),
}

impl Choice {
    pub fn parse(setting: &str) -> Result<Self, String> {
        let setting = setting.trim();
        if setting.is_empty() || setting == "auto" {
            return Ok(Self::Auto);
        }
        if let Some(kind) = Kind::parse(setting) {
            return Ok(Self::Kind(kind));
        }
        if is_absolute(setting) {
            return Ok(Self::Path(PathBuf::from(setting)));
        }
        Err(format!(
            "unknown shell '{setting}': use auto, pwsh, powershell, gitbash, zsh, bash, sh or an absolute path"
        ))
    }
}

/// An absolute path on Unix or Windows (`C:\…`, `\\server\…`).
fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with("\\\\")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// `shell` and `terminal_shell` in config.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub shell: String,
    /// None: the terminal follows `shell`.
    pub terminal_shell: Option<String>,
}

impl Settings {
    pub fn from_config(value: &Value) -> Self {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Self {
            shell: text("shell").unwrap_or_else(|| "auto".to_string()),
            terminal_shell: text("terminal_shell"),
        }
    }

    /// The settings in ~/.lynshen/config.json; "auto" when it cannot be read.
    pub fn read() -> Self {
        let value = crate::config::lynshen_dir()
            .ok()
            .and_then(|dir| fs::read_to_string(dir.join("config.json")).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        Self::from_config(&value)
    }

    /// What a terminal opens: `terminal_shell`, else `shell`.
    pub fn terminal(&self) -> &str {
        self.terminal_shell.as_deref().unwrap_or(&self.shell)
    }
}

/// A shell program and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub kind: Kind,
    pub path: PathBuf,
}

/// What detection looks at: the platform, the environment and the login
/// shell the user database names. Tests make one up.
#[derive(Debug, Clone)]
pub struct Host {
    pub platform: Platform,
    vars: HashMap<String, OsString>,
    /// The user's login shell in the user database (Unix).
    account_shell: Option<PathBuf>,
}

impl Host {
    pub fn current() -> Self {
        Self::new(Platform::current(), env::vars_os(), account_shell())
    }

    fn new(
        platform: Platform,
        vars: impl IntoIterator<Item = (OsString, OsString)>,
        account_shell: Option<PathBuf>,
    ) -> Self {
        // Windows' variable names are case-insensitive.
        let vars = vars
            .into_iter()
            .filter_map(|(name, value)| {
                let name = name.into_string().ok()?;
                let name = match platform {
                    Platform::Windows => name.to_uppercase(),
                    _ => name,
                };
                Some((name, value))
            })
            .collect();
        Self {
            platform,
            vars,
            account_shell,
        }
    }

    fn var(&self, name: &str) -> Option<PathBuf> {
        self.vars
            .get(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    fn path_dirs(&self) -> Vec<PathBuf> {
        let separator = if self.platform == Platform::Windows {
            ';'
        } else {
            ':'
        };
        self.vars
            .get("PATH")
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
            .split(separator)
            .map(|dir| dir.trim().trim_matches('"'))
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .collect()
    }

    /// `name` (`name.exe` on Windows) in a PATH directory.
    fn which(&self, name: &str) -> Option<PathBuf> {
        let file = match self.platform {
            Platform::Windows => format!("{name}.exe"),
            _ => name.to_string(),
        };
        self.path_dirs()
            .into_iter()
            .map(|dir| dir.join(&file))
            .find(|path| self.runnable(path))
    }

    fn runnable(&self, path: &Path) -> bool {
        if path.is_file() {
            return self.platform == Platform::Windows || executable(path);
        }
        // An app execution alias (`WindowsApps\pwsh.exe`, how the Store and
        // MSIX packages install PowerShell) is a reparse point that is not
        // followed.
        self.platform == Platform::Windows
            && fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_dir())
    }

    /// Where `kind` may be installed, most likely first.
    fn candidates(&self, kind: Kind) -> Vec<PathBuf> {
        let under = |var: &str, rest: &[&str]| {
            self.var(var).map(|mut path| {
                path.extend(rest);
                path
            })
        };
        let mut paths: Vec<Option<PathBuf>> = Vec::new();
        match (self.platform, kind) {
            (Platform::Windows, Kind::Pwsh) => {
                paths.push(self.which("pwsh"));
                paths.push(under("PROGRAMFILES", &["PowerShell", "7", "pwsh.exe"]));
                paths.push(under(
                    "LOCALAPPDATA",
                    &["Microsoft", "WindowsApps", "pwsh.exe"],
                ));
                paths.push(under(
                    "PROGRAMFILES",
                    &["PowerShell", "7-preview", "pwsh.exe"],
                ));
                paths.push(under("USERPROFILE", &[".dotnet", "tools", "pwsh.exe"]));
            }
            (Platform::Windows, Kind::WindowsPowerShell) => {
                paths.push(under(
                    "SYSTEMROOT",
                    &["System32", "WindowsPowerShell", "v1.0", "powershell.exe"],
                ));
                paths.push(self.which("powershell"));
            }
            // Never `bash.exe` from PATH: `System32\bash.exe` is WSL's.
            (Platform::Windows, Kind::GitBash) => {
                paths.push(under("PROGRAMFILES", &["Git", "bin", "bash.exe"]));
                paths.push(under("PROGRAMFILES(X86)", &["Git", "bin", "bash.exe"]));
                paths.push(under(
                    "LOCALAPPDATA",
                    &["Programs", "Git", "bin", "bash.exe"],
                ));
                paths.push(
                    self.which("git")
                        .and_then(|git| git_root(&git))
                        .map(|root| root.join("bin").join("bash.exe")),
                );
                paths.push(under(
                    "USERPROFILE",
                    &["scoop", "apps", "git", "current", "bin", "bash.exe"],
                ));
            }
            (Platform::Windows, _) => {}
            (_, Kind::Zsh | Kind::Bash | Kind::Sh) => {
                let name = kind.id();
                paths.push(self.which(name));
                paths.push(Some(PathBuf::from("/bin").join(name)));
                paths.push(Some(PathBuf::from("/usr/bin").join(name)));
            }
            (_, Kind::Pwsh | Kind::WindowsPowerShell | Kind::GitBash) => {}
        }
        paths.into_iter().flatten().collect()
    }

    /// Where `kind` is installed.
    pub fn find(&self, kind: Kind) -> Option<PathBuf> {
        self.candidates(kind)
            .into_iter()
            .find(|path| self.runnable(path))
    }

    fn find_shell(&self, kind: Kind) -> Option<Shell> {
        self.find(kind).map(|path| Shell { kind, path })
    }

    /// The user's login shell (Unix): `$SHELL`, else the user database's.
    fn login_shell_path(&self) -> Option<PathBuf> {
        [self.var("SHELL"), self.account_shell.clone()]
            .into_iter()
            .flatten()
            .find(|path| path.is_absolute() && self.runnable(path))
    }

    /// The login shell when commands can run in it (zsh, bash, sh and the
    /// like, not fish).
    fn login_shell(&self) -> Option<Shell> {
        [self.var("SHELL"), self.account_shell.clone()]
            .into_iter()
            .flatten()
            .filter(|path| path.is_absolute() && self.runnable(path))
            .find_map(|path| {
                Kind::of_program(&path, self.platform)
                    .filter(|kind| !kind.powershell())
                    .map(|kind| Shell { kind, path })
            })
    }

    /// What "auto" runs the model's commands in: on Windows the first
    /// installed of `WINDOWS_AUTO_ORDER`; elsewhere the login shell, else
    /// zsh, bash or sh.
    pub fn auto(&self) -> Shell {
        match self.platform {
            Platform::Windows => WINDOWS_AUTO_ORDER
                .into_iter()
                .find_map(|kind| self.find_shell(kind))
                .unwrap_or_else(|| Shell {
                    kind: Kind::WindowsPowerShell,
                    path: PathBuf::from("powershell.exe"),
                }),
            platform => {
                let order = if platform == Platform::MacOs {
                    [Kind::Zsh, Kind::Bash, Kind::Sh]
                } else {
                    [Kind::Bash, Kind::Zsh, Kind::Sh]
                };
                self.login_shell()
                    .or_else(|| order.into_iter().find_map(|kind| self.find_shell(kind)))
                    .unwrap_or_else(|| Shell {
                        kind: Kind::Sh,
                        path: PathBuf::from("/bin/sh"),
                    })
            }
        }
    }

    /// The shell `choice` names, or "auto" with the reason when it cannot
    /// be used here.
    pub fn resolve(&self, choice: &Choice) -> (Shell, Option<String>) {
        let instead = |reason: String| {
            let shell = self.auto();
            let note = format!("{reason}; commands run in {}", shell.kind.name());
            (shell, Some(note))
        };
        match choice {
            Choice::Auto => (self.auto(), None),
            Choice::Kind(kind) if Kind::offered(self.platform).contains(kind) => {
                match self.find_shell(*kind) {
                    Some(shell) => (shell, None),
                    None => instead(format!("{} is not installed", kind.name())),
                }
            }
            Choice::Kind(kind) => instead(format!(
                "{} is not offered on {}",
                kind.name(),
                self.platform.as_str()
            )),
            Choice::Path(path) => match Kind::of_program(path, self.platform) {
                Some(kind) if self.runnable(path) => (
                    Shell {
                        kind,
                        path: path.clone(),
                    },
                    None,
                ),
                Some(_) => instead(format!("{} does not exist", path.display())),
                None => instead(format!(
                    "{} is not a shell commands run in (PowerShell, bash, zsh or sh)",
                    path.display()
                )),
            },
        }
    }

    /// The program a terminal opens for `setting`, with its arguments and
    /// variables. Any program may be named by path (cmd.exe, fish); "auto"
    /// is what the model's commands use on Windows, and the login shell
    /// (whichever it is) elsewhere.
    pub fn terminal(&self, setting: &str) -> TerminalShell {
        let shell = match Choice::parse(setting) {
            Ok(Choice::Path(path)) if self.runnable(&path) => {
                return TerminalShell::for_program(path, self.platform)
            }
            Ok(Choice::Kind(kind)) => self.find(kind),
            _ => None,
        };
        let path = shell.unwrap_or_else(|| match self.platform {
            Platform::Windows => WINDOWS_AUTO_ORDER
                .into_iter()
                .find_map(|kind| self.find(kind))
                .or_else(|| self.var("COMSPEC"))
                .unwrap_or_else(|| PathBuf::from("powershell.exe")),
            platform => self.login_shell_path().unwrap_or_else(|| {
                let fallback = if platform == Platform::MacOs {
                    "/bin/zsh"
                } else {
                    "/bin/bash"
                };
                [fallback, "/bin/sh"]
                    .into_iter()
                    .map(PathBuf::from)
                    .find(|path| self.runnable(path))
                    .unwrap_or_else(|| PathBuf::from("/bin/sh"))
            }),
        });
        TerminalShell::for_program(path, self.platform)
    }
}

/// Git for Windows' folder from its `git.exe` (`<root>\cmd\git.exe`,
/// `<root>\bin\git.exe` or `<root>\mingw64\bin\git.exe`).
fn git_root(git: &Path) -> Option<PathBuf> {
    let dir = git.parent()?;
    let name = dir.file_name()?.to_string_lossy().to_lowercase();
    let root = match name.as_str() {
        "cmd" => dir.parent()?,
        "bin" => {
            let parent = dir.parent()?;
            let mingw = parent
                .file_name()
                .is_some_and(|name| name.to_string_lossy().to_lowercase().starts_with("mingw"));
            if mingw {
                parent.parent()?
            } else {
                parent
            }
        }
        _ => return None,
    };
    Some(root.to_path_buf())
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(_: &Path) -> bool {
    true
}

#[cfg(unix)]
fn account_shell() -> Option<PathBuf> {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: getpwuid_r writes only into `entry` and `buffer`, whose sizes
    // it is given; `pw_shell` points into `buffer`, read before it is freed.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut found = std::ptr::null_mut();
        let code = libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        );
        if code != 0 || found.is_null() || entry.pw_shell.is_null() {
            return None;
        }
        let shell = std::ffi::CStr::from_ptr(entry.pw_shell).to_bytes();
        (!shell.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(shell)))
    }
}

#[cfg(not(unix))]
fn account_shell() -> Option<PathBuf> {
    None
}

/// What a terminal runs: the program, its arguments and the variables it
/// adds to the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalShell {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl TerminalShell {
    fn for_program(program: PathBuf, platform: Platform) -> Self {
        let mut env = Vec::new();
        let args: &[&str] = match Kind::of_program(&program, platform) {
            Some(Kind::Pwsh | Kind::WindowsPowerShell) => &["-NoLogo"],
            Some(Kind::GitBash) => {
                // Else Git Bash's /etc/profile starts in the home directory.
                env.push(("CHERE_INVOKING".to_string(), "1".to_string()));
                &["--login", "-i"]
            }
            Some(Kind::Zsh | Kind::Bash | Kind::Sh) => &["-l"],
            None if program_name(&program) == "fish" => &["-l"],
            None => &[],
        };
        Self {
            program,
            args: args.iter().map(|arg| arg.to_string()).collect(),
            env,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "program": self.program.display().to_string(),
            "args": self.args,
            "env": self.env.iter().cloned().collect::<HashMap<_, _>>(),
        })
    }
}

/// The first `1.2.3`-like run in `text`.
fn first_version(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|part| part.trim_matches('.'))
        .find(|part| part.contains('.') && part.split('.').all(|digits| !digits.is_empty()))
        .map(str::to_string)
}

/// The version in what `kind` printed for its version; for `sh`, the shell
/// it is when it tells ("bash 3.2.57" on macOS), else None (dash).
pub fn parse_version(kind: Kind, output: &str) -> Option<String> {
    let version = first_version(output)?;
    if kind != Kind::Sh {
        return Some(version);
    }
    let lower = output.to_lowercase();
    ["bash", "zsh", "ksh"]
        .into_iter()
        .find(|name| lower.contains(name))
        .map(|name| format!("{name} {version}"))
}

/// Asks `shell` for its version.
pub fn probe_version(shell: &Shell) -> Option<String> {
    let args: &[&str] = match shell.kind {
        Kind::WindowsPowerShell => &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$PSVersionTable.PSVersion.ToString()",
        ],
        _ => &["--version"],
    };
    let output = run_quiet(&shell.path, args, VERSION_TIMEOUT)?;
    parse_version(shell.kind, &output)
}

/// `program`'s standard output, if it ends within `timeout`.
fn run_quiet(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    hide_window(&mut command);
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    if wait_for(&mut child, timeout).is_none() {
        kill_tree(&mut child);
        return None;
    }
    reader
        .join()
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// The child's status once it exits within `timeout`.
fn wait_for(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => return None,
        }
    }
}

/// Kills the child and, on Unix, its process group (what its rc files
/// started).
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: the child was started as the leader of its own group and is
    // not reaped yet, so the group id is still ours.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn own_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

fn temp_file(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!(
        "lynshen-{label}-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A shell's environment variables, as `env -0` gave them.
pub type Vars = Vec<(OsString, OsString)>;

/// The environment of `shell` started as an interactive login shell
/// (`-ilc`): what the user's rc files set up. `env` replaces the inherited
/// environment when given (tests). The shell's input and output are closed
/// and it writes the variables to a file, so an rc file that reads input
/// gets end of file, and a program it starts in the background (an
/// ssh-agent) cannot hold a pipe open. A shell that does not finish within
/// `timeout` is killed with what it started.
pub fn snapshot(shell: &Path, env: Option<&Vars>, timeout: Duration) -> Result<Vars, String> {
    let file = temp_file("env");
    let script = format!(
        "command env -0 > \"${SNAPSHOT_FILE_VAR}\" && printf '%s' {SNAPSHOT_END} >> \"${SNAPSHOT_FILE_VAR}\""
    );
    let mut command = Command::new(shell);
    command
        .arg("-ilc")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(vars) = env {
        command.env_clear().envs(vars.iter().cloned());
    }
    command.env(SNAPSHOT_FILE_VAR, &file);
    let home = match env {
        Some(vars) => vars
            .iter()
            .find(|(name, _)| name == "HOME")
            .map(|(_, home)| PathBuf::from(home)),
        None => env::var_os("HOME").map(PathBuf::from),
    };
    if let Some(home) = home.filter(|home| home.is_dir()) {
        command.current_dir(home);
    }
    own_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("{} did not start: {error}", shell.display()))?;
    let result = match wait_for(&mut child, timeout) {
        Some(_) => fs::read(&file)
            .map_err(|_| format!("{} -ilc wrote no environment", shell.display()))
            .and_then(|bytes| parse_snapshot(&bytes)),
        None => {
            kill_tree(&mut child);
            Err(format!(
                "{} -ilc did not finish within {} s",
                shell.display(),
                timeout.as_secs()
            ))
        }
    };
    let _ = fs::remove_file(&file);
    result
}

/// The variables of a complete snapshot (`NAME=value`, NUL after each).
fn parse_snapshot(bytes: &[u8]) -> Result<Vars, String> {
    let body = bytes
        .strip_suffix(SNAPSHOT_END.as_bytes())
        .ok_or("the login shell's environment was cut short")?;
    let vars: Vars = body
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let equals = entry.iter().position(|byte| *byte == b'=')?;
            let (name, value) = (&entry[..equals], &entry[equals + 1..]);
            let skip = name.is_empty()
                || SNAPSHOT_SKIP
                    .iter()
                    .any(|skipped| skipped.as_bytes() == name);
            (!skip).then(|| (os_string(name), os_string(value)))
        })
        .collect();
    if !vars.iter().any(|(name, _)| name == "PATH") {
        return Err("the login shell's environment has no PATH".to_string());
    }
    Ok(vars)
}

#[cfg(unix)]
fn os_string(bytes: &[u8]) -> OsString {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os_string(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Whether `shell -lc` starts and ends within `timeout`: a login profile
/// that hangs must not hang every command.
fn login_starts(shell: &Path, env: Option<&Vars>, timeout: Duration) -> bool {
    let mut command = Command::new(shell);
    command
        .args(["-lc", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(vars) = env {
        command.env_clear().envs(vars.iter().cloned());
    }
    own_group(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    match wait_for(&mut child, timeout) {
        Some(status) => status.success(),
        None => {
            kill_tree(&mut child);
            false
        }
    }
}

/// What the model's commands run in.
#[derive(Debug, Clone)]
pub struct CommandShell {
    pub platform: Platform,
    pub shell: Shell,
    pub version: Option<String>,
    /// The login shell's environment (`snapshot`): commands run in it with
    /// `-c`.
    pub env: Option<Arc<Vars>>,
    /// Commands start a login shell (`-lc`).
    pub login: bool,
    /// Why commands do not run as the setting asks: a shell that is not
    /// installed, a snapshot that failed.
    pub notes: Vec<String>,
    /// With Git Bash: the PowerShell for Windows-native work, and its
    /// version.
    pub helper: Option<(Kind, Option<String>)>,
    /// The shell the setting picked that cannot run here, when this one runs
    /// instead: Git Bash under the Windows sandbox (`in_windows_sandbox`).
    pub instead_of: Option<Kind>,
}

/// One line of a command: the program, its arguments, and the variables
/// it sets on top of the engine's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vars,
}

impl CommandShell {
    /// How commands ran before the setting: `sh -lc`, or Windows PowerShell.
    /// For a `ToolState` no engine set a shell on (unit tests).
    pub fn legacy() -> Self {
        let platform = Platform::current();
        let shell = match platform {
            Platform::Windows => Shell {
                kind: Kind::WindowsPowerShell,
                path: PathBuf::from("powershell"),
            },
            _ => Shell {
                kind: Kind::Sh,
                path: PathBuf::from("sh"),
            },
        };
        Self {
            platform,
            shell,
            version: None,
            env: None,
            login: true,
            notes: Vec::new(),
            helper: None,
            instead_of: None,
        }
    }

    /// The shell for `setting` on `host`, with its version, and for zsh and
    /// bash on Unix their login environment.
    pub fn detect(host: &Host, setting: &str) -> Self {
        let mut notes = Vec::new();
        let choice = Choice::parse(setting).unwrap_or_else(|error| {
            notes.push(error);
            Choice::Auto
        });
        let (mut shell, note) = host.resolve(&choice);
        notes.extend(note);
        let mut env = None;
        let mut login = true;
        let posix_login =
            host.platform != Platform::Windows && matches!(shell.kind, Kind::Zsh | Kind::Bash);
        if posix_login {
            match snapshot(&shell.path, None, SNAPSHOT_TIMEOUT) {
                Ok(vars) => {
                    env = Some(Arc::new(vars));
                    login = false;
                }
                Err(error) if login_starts(&shell.path, None, SNAPSHOT_TIMEOUT) => notes.push(
                    format!("{error}; commands run in {} -lc", shell.path.display()),
                ),
                Err(error) => {
                    notes.push(format!(
                        "{error}, and {} -lc did not start either; commands run in /bin/sh -c",
                        shell.path.display()
                    ));
                    shell = Shell {
                        kind: Kind::Sh,
                        path: PathBuf::from("/bin/sh"),
                    };
                    login = false;
                }
            }
        }
        let version = probe_version(&shell);
        let helper = (shell.kind == Kind::GitBash)
            .then(|| {
                [Kind::Pwsh, Kind::WindowsPowerShell]
                    .into_iter()
                    .find_map(|kind| host.find_shell(kind))
            })
            .flatten()
            .map(|helper| (helper.kind, probe_version(&helper)));
        Self {
            platform: host.platform,
            shell,
            version,
            env,
            login,
            notes,
            helper,
            instead_of: None,
        }
    }

    /// The shell for commands under the Windows sandbox: Git Bash cannot
    /// start there (its MSYS2 runtime needs to write pipes only the user's
    /// own token may), so PowerShell 7, else Windows PowerShell, runs them
    /// instead. Any other shell is kept, and so is Git Bash on a machine with
    /// no PowerShell (the sandbox then refuses it with a reason).
    pub fn in_windows_sandbox(self, host: &Host) -> Self {
        if self.shell.kind != Kind::GitBash {
            return self;
        }
        let Some(kind) = [Kind::Pwsh, Kind::WindowsPowerShell]
            .into_iter()
            .find(|kind| host.find(*kind).is_some())
        else {
            return self;
        };
        let mut shell = Self::detect(host, kind.id());
        shell.notes.insert(
            0,
            format!(
                "Git Bash cannot run in the Windows sandbox (preview): commands run in {} instead",
                kind.name()
            ),
        );
        shell.instead_of = Some(Kind::GitBash);
        shell
    }

    /// The command line that runs `command`, with the model's variables
    /// (`MODEL_ENV`).
    pub fn invocation(&self, command: &str) -> Invocation {
        let mut env: Vars = self.env.as_deref().cloned().unwrap_or_default();
        let args = if self.shell.kind.powershell() {
            vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                format!("{POWERSHELL_PRELUDE}\n{command}"),
            ]
        } else {
            let flag = if self.login { "-lc" } else { "-c" };
            vec![flag.to_string(), command.to_string()]
        };
        let set = |env: &Vars, name: &str| {
            env.iter().any(|(key, _)| key == name) || std::env::var_os(name).is_some()
        };
        if self.shell.kind == Kind::GitBash {
            // Else Git Bash's /etc/profile starts in the home directory.
            env.push(("CHERE_INVOKING".into(), "1".into()));
        }
        if !self.shell.kind.powershell()
            && !["LC_ALL", "LC_CTYPE", "LANG"]
                .iter()
                .any(|name| set(&env, name))
        {
            let lang = if self.platform == Platform::MacOs {
                "en_US.UTF-8"
            } else {
                "C.UTF-8"
            };
            env.push(("LANG".into(), lang.into()));
        }
        if !set(&env, "GIT_TERMINAL_PROMPT") {
            env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
        }
        for (name, value) in MODEL_ENV {
            env.retain(|(key, _)| key != name);
            env.push((name.into(), value.into()));
        }
        Invocation {
            program: self.shell.path.display().to_string(),
            args,
            env,
        }
    }

    /// "PowerShell 7.5.2", "zsh 5.9".
    pub fn label(&self) -> String {
        self.shell.kind.label(self.version.as_deref())
    }

    /// What the system prompt says about the shell.
    pub fn prompt(&self) -> String {
        let label = self.label();
        match self.shell.kind {
            Kind::Pwsh => format!(
                "Commands run in {label} (pwsh): use PowerShell syntax, such as `$env:NAME` for environment variables."
            ),
            Kind::WindowsPowerShell => format!(
                "Commands run in {label}. It has no `&&` or `||`: separate commands with `;`, and run a command only when the one before it succeeded with `if ($?) {{ … }}`. Use `$env:NAME` for environment variables."
            ),
            Kind::GitBash => {
                let mut line = format!(
                    "Commands run in {label} on Windows: use bash syntax, and write paths with forward slashes (`C:/Users/me`)."
                );
                if let Some((kind, version)) = &self.helper {
                    let program = if *kind == Kind::Pwsh {
                        "pwsh"
                    } else {
                        "powershell"
                    };
                    line.push_str(&format!(
                        " For Windows-native work, run `{program} -NoProfile -Command \"…\"` ({}).",
                        kind.label(version.as_deref())
                    ));
                }
                line
            }
            Kind::Zsh | Kind::Bash | Kind::Sh => format!("Commands run in {label}."),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.shell.kind.id(),
            "name": self.shell.kind.name(),
            "label": self.label(),
            "path": self.shell.path.display().to_string(),
            "version": self.version,
            "login": self.login,
            "snapshot": self.env.is_some(),
            "notes": self.notes,
            "helper": self.helper.as_ref().map(|(kind, version)| json!({
                "id": kind.id(),
                "name": kind.name(),
                "version": version,
            })),
            // Set when the Windows sandbox replaced the chosen shell.
            "instead_of": self.instead_of.map(Kind::id),
        })
    }
}

/// The commands' shell for each setting this process used: the snapshot
/// runs once per setting (and again after `forget`).
fn commands_cache() -> &'static Mutex<HashMap<String, Arc<CommandShell>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<CommandShell>>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// The shell the model's commands run in for `setting` (`shell` in
/// config.json). The first call for a setting detects it, which can take a
/// few seconds (see `prewarm`); later calls return the same.
pub fn for_commands(setting: &str) -> Arc<CommandShell> {
    // The Windows sandbox may run another shell than the setting picks
    // (`CommandShell::in_windows_sandbox`), and Desktop turns it on and off
    // while engines run: one entry with it and one without.
    let sandboxed = crate::sandbox::windows_sandbox_on();
    let key = if sandboxed {
        format!("{setting}\u{0}windows-sandbox")
    } else {
        setting.to_string()
    };
    let mut cache = commands_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(shell) = cache.get(&key) {
        return Arc::clone(shell);
    }
    let host = Host::current();
    let mut shell = CommandShell::detect(&host, setting);
    if sandboxed {
        shell = shell.in_windows_sandbox(&host);
    }
    let shell = Arc::new(shell);
    crate::log_info!(
        "shell",
        "commands' shell",
        setting = setting,
        shell = shell.label(),
        path = shell.shell.path.display().to_string(),
        snapshot = shell.env.is_some()
    );
    for note in &shell.notes {
        crate::log_warn!("shell", "shell setting", note = note.as_str());
    }
    cache.insert(key, Arc::clone(&shell));
    shell
}

/// Detects the shell for `setting` in the background, so the first command
/// does not wait for it.
pub fn prewarm(setting: &str) {
    let setting = setting.to_string();
    thread::spawn(move || {
        for_commands(&setting);
    });
}

/// Detects everything again on next use: the settings page's "detect
/// again", after the user installed a shell or changed an rc file.
pub fn forget() {
    commands_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    *installed_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// A shell settings offer: where it is installed and its version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub kind: Kind,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
}

fn installed_cache() -> &'static Mutex<Option<Arc<Vec<Installed>>>> {
    static CACHE: OnceLock<Mutex<Option<Arc<Vec<Installed>>>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// The shells `host` offers, each asked for its version at the same time.
pub fn installed(host: &Host) -> Vec<Installed> {
    let probes: Vec<_> = Kind::offered(host.platform)
        .iter()
        .map(|&kind| {
            let path = host.find(kind);
            let shell = path.clone().map(|path| Shell { kind, path });
            (
                kind,
                path,
                thread::spawn(move || shell.as_ref().and_then(probe_version)),
            )
        })
        .collect();
    probes
        .into_iter()
        .map(|(kind, path, probe)| Installed {
            kind,
            path,
            version: probe.join().ok().flatten(),
        })
        .collect()
}

/// What auto tries, in order (`login` is the user's login shell).
pub fn auto_order(platform: Platform) -> Vec<&'static str> {
    match platform {
        Platform::Windows => WINDOWS_AUTO_ORDER.iter().map(|kind| kind.id()).collect(),
        Platform::MacOs => vec!["login", "zsh", "bash", "sh"],
        Platform::Linux => vec!["login", "bash", "zsh", "sh"],
    }
}

/// The `shells` reply: the settings, the shells this computer has (with
/// versions), what "auto" takes, and what commands and terminals run in now.
/// `refresh` detects everything again first (see `forget`).
pub fn shells_json(refresh: bool) -> Value {
    if refresh {
        forget();
    }
    let host = Host::current();
    let settings = Settings::read();
    let installed = {
        let mut cache = installed_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(cache.get_or_insert_with(|| Arc::new(installed(&host))))
    };
    let commands = for_commands(&settings.shell);
    shells_value(&host, &settings, &installed, &commands)
}

fn shells_value(
    host: &Host,
    settings: &Settings,
    installed: &[Installed],
    commands: &CommandShell,
) -> Value {
    let auto = host.auto();
    json!({
        "type": "shells",
        "platform": host.platform.as_str(),
        "shell": settings.shell,
        "terminal_shell": settings.terminal_shell,
        "auto": { "id": auto.kind.id(), "path": auto.path.display().to_string() },
        "auto_order": auto_order(host.platform),
        "shells": installed.iter().map(|shell| json!({
            "id": shell.kind.id(),
            "name": shell.kind.name(),
            "installed": shell.path.is_some(),
            "path": shell.path.as_ref().map(|path| path.display().to_string()),
            "version": shell.version,
        })).collect::<Vec<_>>(),
        "commands": commands.to_json(),
        "terminal": host.terminal(settings.terminal()).to_json(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// A temporary directory, removed when dropped.
    struct Dir(PathBuf);

    impl Dir {
        fn new(label: &str) -> Self {
            let dir = temp_file(&format!("shell-test-{label}"));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// An executable file at `rel` that prints `output`.
        fn program(&self, rel: &str, output: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn host(platform: Platform, vars: &[(&str, &Path)], account: Option<&Path>) -> Host {
        Host::new(
            platform,
            vars.iter()
                .map(|(name, value)| (OsString::from(name), value.as_os_str().to_os_string())),
            account.map(Path::to_path_buf),
        )
    }

    fn join_paths(platform: Platform, dirs: &[PathBuf]) -> PathBuf {
        let separator = if platform == Platform::Windows {
            ";"
        } else {
            ":"
        };
        PathBuf::from(
            dirs.iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(separator),
        )
    }

    #[test]
    fn settings_parse_names_paths_and_reject_the_rest() {
        assert_eq!(Choice::parse(""), Ok(Choice::Auto));
        assert_eq!(Choice::parse(" auto "), Ok(Choice::Auto));
        assert_eq!(Choice::parse("pwsh"), Ok(Choice::Kind(Kind::Pwsh)));
        assert_eq!(
            Choice::parse("powershell"),
            Ok(Choice::Kind(Kind::WindowsPowerShell))
        );
        assert_eq!(Choice::parse("gitbash"), Ok(Choice::Kind(Kind::GitBash)));
        assert_eq!(
            Choice::parse("/opt/homebrew/bin/bash"),
            Ok(Choice::Path(PathBuf::from("/opt/homebrew/bin/bash")))
        );
        assert_eq!(
            Choice::parse(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Ok(Choice::Path(PathBuf::from(
                r"C:\Program Files\PowerShell\7\pwsh.exe"
            )))
        );
        assert!(Choice::parse("fish").is_err());
        assert!(Choice::parse("bin/zsh").is_err());

        let settings = Settings::from_config(&json!({ "shell": "zsh", "terminal_shell": "" }));
        assert_eq!(settings.shell, "zsh");
        assert_eq!(settings.terminal_shell, None);
        assert_eq!(settings.terminal(), "zsh");
        let settings = Settings::from_config(&json!({ "terminal_shell": "/bin/bash" }));
        assert_eq!(settings.shell, "auto");
        assert_eq!(settings.terminal(), "/bin/bash");
    }

    #[test]
    fn windows_detection_finds_each_shell_where_it_installs() {
        let dir = Dir::new("windows");
        let program_files = dir.path("Program Files");
        let system_root = dir.path("Windows");
        let local = dir.path("Local");
        // Nothing installed: auto still names Windows PowerShell.
        let empty = host(
            Platform::Windows,
            &[
                ("ProgramFiles", &program_files),
                ("SystemRoot", &system_root),
            ],
            None,
        );
        assert_eq!(empty.find(Kind::Pwsh), None);
        assert_eq!(empty.auto().kind, Kind::WindowsPowerShell);

        let powershell = dir.program(
            "Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
            "5.1.22621.4391",
        );
        // An MSIX install: only the app execution alias in WindowsApps.
        let alias = dir.program("Local/Microsoft/WindowsApps/pwsh.exe", "PowerShell 7.4.6");
        let vars = [
            ("ProgramFiles", program_files.as_path()),
            // Variable names are case-insensitive on Windows.
            ("SYSTEMROOT", system_root.as_path()),
            ("LocalAppData", local.as_path()),
        ];
        let windows = host(Platform::Windows, &vars, None);
        assert_eq!(windows.find(Kind::WindowsPowerShell), Some(powershell));
        assert_eq!(windows.find(Kind::Pwsh), Some(alias.clone()));
        assert_eq!(windows.auto().kind, Kind::Pwsh);

        // The installer's location wins over the alias; PATH over both.
        let installed = dir.program("Program Files/PowerShell/7/pwsh.exe", "PowerShell 7.5.2");
        assert_eq!(
            host(Platform::Windows, &vars, None).find(Kind::Pwsh),
            Some(installed)
        );
        let scoop = dir.program("scoop/shims/pwsh.exe", "PowerShell 7.5.3");
        let path = join_paths(
            Platform::Windows,
            &[dir.path("empty"), dir.path("scoop/shims")],
        );
        let with_path: Vec<_> = vars
            .iter()
            .copied()
            .chain([("Path", path.as_path())])
            .collect();
        assert_eq!(
            host(Platform::Windows, &with_path, None).find(Kind::Pwsh),
            Some(scoop)
        );

        // Git Bash: from Program Files, or next to git.exe on PATH, never
        // bash.exe on PATH (WSL's).
        let wsl = dir.program("Windows/System32/bash.exe", "wsl");
        let path = join_paths(Platform::Windows, &[wsl.parent().unwrap().to_path_buf()]);
        let wsl_only: Vec<_> = vars
            .iter()
            .copied()
            .chain([("PATH", path.as_path())])
            .collect();
        assert_eq!(
            host(Platform::Windows, &wsl_only, None).find(Kind::GitBash),
            None
        );
        dir.program("Tools/Git/cmd/git.exe", "git version 2.47.1");
        let bash = dir.program(
            "Tools/Git/bin/bash.exe",
            "GNU bash, version 5.2.37(1)-release",
        );
        let path = join_paths(Platform::Windows, &[dir.path("Tools/Git/cmd")]);
        let with_git: Vec<_> = vars
            .iter()
            .copied()
            .chain([("PATH", path.as_path())])
            .collect();
        let windows = host(Platform::Windows, &with_git, None);
        assert_eq!(windows.find(Kind::GitBash), Some(bash.clone()));
        // Auto takes Git Bash first (WINDOWS_AUTO_ORDER).
        assert_eq!(
            windows.auto(),
            Shell {
                kind: WINDOWS_AUTO_ORDER[0],
                path: windows.find(WINDOWS_AUTO_ORDER[0]).unwrap()
            }
        );
        let program_files_bash = dir.program("Program Files/Git/bin/bash.exe", "bash");
        assert_eq!(
            host(Platform::Windows, &vars, None).find(Kind::GitBash),
            Some(program_files_bash)
        );
    }

    #[test]
    fn a_chosen_shell_that_is_missing_falls_back_to_auto_with_a_note() {
        let dir = Dir::new("missing");
        let system_root = dir.path("Windows");
        let powershell = dir.program(
            "Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
            "5.1",
        );
        let windows = host(Platform::Windows, &[("SystemRoot", &system_root)], None);
        let (shell, note) = windows.resolve(&Choice::Kind(Kind::Pwsh));
        assert_eq!(shell.path, powershell);
        assert_eq!(
            note.as_deref(),
            Some("PowerShell 7 is not installed; commands run in Windows PowerShell 5.1")
        );
        let (shell, note) = windows.resolve(&Choice::Kind(Kind::Zsh));
        assert_eq!(shell.kind, Kind::WindowsPowerShell);
        assert!(note.unwrap().contains("not offered on windows"));
        let (_, note) = windows.resolve(&Choice::Path(dir.path("cmd.exe")));
        assert!(note.unwrap().contains("is not a shell"));
        let (shell, note) = windows.resolve(&Choice::Path(powershell.clone()));
        assert_eq!((shell.kind, note), (Kind::WindowsPowerShell, None));
    }

    #[cfg(unix)]
    #[test]
    fn unix_auto_takes_the_login_shell_when_commands_can_run_in_it() {
        let dir = Dir::new("unix");
        let zsh = dir.program("bin/zsh", "zsh 5.9 (arm64-apple-darwin26.0)");
        let bash = dir.program("brew/bash", "GNU bash, version 5.2.37(1)-release");
        let fish = dir.program("bin/fish", "fish, version 3.7.1");
        let path = join_paths(Platform::MacOs, &[dir.path("bin"), dir.path("brew")]);

        let mac = host(
            Platform::MacOs,
            &[("PATH", &path), ("SHELL", &bash)],
            Some(&zsh),
        );
        assert_eq!(
            mac.auto(),
            Shell {
                kind: Kind::Bash,
                path: bash.clone()
            }
        );
        // $SHELL is fish: the user database's zsh.
        let mac = host(
            Platform::MacOs,
            &[("PATH", &path), ("SHELL", &fish)],
            Some(&zsh),
        );
        assert_eq!(
            mac.auto(),
            Shell {
                kind: Kind::Zsh,
                path: zsh.clone()
            }
        );
        // Only fish: zsh from PATH on macOS, bash first on Linux.
        let mac = host(
            Platform::MacOs,
            &[("PATH", &path), ("SHELL", &fish)],
            Some(&fish),
        );
        assert_eq!(mac.auto().path, zsh);
        let linux = host(Platform::Linux, &[("PATH", &path), ("SHELL", &fish)], None);
        assert_eq!(linux.auto().path, bash);
        // The terminal opens the login shell whichever it is.
        let terminal = linux.terminal("auto");
        assert_eq!(
            (terminal.program, terminal.args),
            (fish.clone(), vec!["-l".to_string()])
        );
        // A shell named in the settings, for commands and the terminal.
        assert_eq!(mac.find(Kind::Bash), Some(bash.clone()));
        let terminal = mac.terminal("bash");
        assert_eq!(
            (terminal.program, terminal.args),
            (bash, vec!["-l".to_string()])
        );
        // A missing path or name: the terminal falls back to auto.
        assert_eq!(mac.terminal("/nowhere/zsh").program, fish);
    }

    #[test]
    fn the_windows_sandbox_runs_powershell_instead_of_git_bash() {
        let dir = Dir::new("windows-sandbox");
        let program_files = dir.path("Program Files");
        let system_root = dir.path("Windows");
        dir.program("Program Files/Git/bin/bash.exe", "");
        let vars = [
            ("ProgramFiles", program_files.as_path()),
            ("SystemRoot", system_root.as_path()),
        ];
        let windows = host(Platform::Windows, &vars, None);
        let bash = CommandShell::detect(&windows, "gitbash");
        assert_eq!(bash.shell.kind, Kind::GitBash);
        // No PowerShell at all: Git Bash stays (the sandbox refuses it).
        let kept = bash.clone().in_windows_sandbox(&windows);
        assert_eq!((kept.shell.kind, kept.instead_of), (Kind::GitBash, None));
        // Windows PowerShell, then PowerShell 7 once it is installed.
        dir.program(
            "Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
            "5.1.26100.1",
        );
        let windows = host(Platform::Windows, &vars, None);
        let shell = CommandShell::detect(&windows, "gitbash").in_windows_sandbox(&windows);
        assert_eq!(shell.shell.kind, Kind::WindowsPowerShell);
        assert_eq!(shell.instead_of, Some(Kind::GitBash));
        assert!(shell.notes[0].contains("Git Bash cannot run in the Windows sandbox"));
        assert_eq!(shell.to_json()["instead_of"], "gitbash");
        dir.program("Program Files/PowerShell/7/pwsh.exe", "PowerShell 7.5.2");
        let windows = host(Platform::Windows, &vars, None);
        let shell = CommandShell::detect(&windows, "auto").in_windows_sandbox(&windows);
        assert_eq!(
            (shell.shell.kind, shell.instead_of),
            (Kind::Pwsh, Some(Kind::GitBash))
        );
        // Any other shell is kept as it is.
        let pwsh = CommandShell::detect(&windows, "pwsh");
        assert_eq!(pwsh.clone().in_windows_sandbox(&windows).instead_of, None);
        assert_eq!(pwsh.to_json()["instead_of"], Value::Null);
    }

    #[test]
    fn terminals_on_windows_open_auto_or_any_program_named() {
        let dir = Dir::new("terminal");
        let program_files = dir.path("Program Files");
        let pwsh = dir.program("Program Files/PowerShell/7/pwsh.exe", "PowerShell 7.5.2");
        let cmd = dir.program("Windows/System32/cmd.exe", "");
        let vars = [
            ("ProgramFiles", program_files.as_path()),
            ("ComSpec", cmd.as_path()),
        ];
        let windows = host(Platform::Windows, &vars, None);
        let terminal = windows.terminal("auto");
        assert_eq!(
            (terminal.program, terminal.args),
            (pwsh, vec!["-NoLogo".to_string()])
        );
        let terminal = windows.terminal(&cmd.display().to_string());
        assert_eq!(
            (terminal.program.clone(), terminal.args.len()),
            (cmd.clone(), 0)
        );
        let bash = dir.program("Program Files/Git/bin/bash.exe", "");
        let terminal = host(Platform::Windows, &vars, None).terminal("gitbash");
        assert_eq!(terminal.program, bash);
        assert_eq!(terminal.args, ["--login", "-i"]);
        assert_eq!(
            terminal.env,
            [("CHERE_INVOKING".to_string(), "1".to_string())]
        );
        // Nothing found: %COMSPEC%.
        let bare = host(Platform::Windows, &[("ComSpec", cmd.as_path())], None);
        assert_eq!(bare.terminal("auto").program, cmd);
    }

    #[test]
    fn the_shells_reply_lists_what_settings_offer() {
        let dir = Dir::new("reply");
        let program_files = dir.path("Program Files");
        let system_root = dir.path("Windows");
        let powershell = dir.program(
            "Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
            "5.1",
        );
        let bash = dir.program("Program Files/Git/bin/bash.exe", "bash");
        let vars = [
            ("ProgramFiles", program_files.as_path()),
            ("SystemRoot", system_root.as_path()),
        ];
        let windows = host(Platform::Windows, &vars, None);
        let settings = Settings::from_config(&json!({ "shell": "pwsh" }));
        let installed: Vec<Installed> = Kind::offered(Platform::Windows)
            .iter()
            .map(|&kind| Installed {
                kind,
                path: windows.find(kind),
                version: windows.find(kind).map(|_| "5.1.22621.4391".to_string()),
            })
            .collect();
        let (shell, note) = windows.resolve(&Choice::Kind(Kind::Pwsh));
        let commands = CommandShell {
            notes: note.into_iter().collect(),
            ..command_shell(
                shell.kind,
                &shell.path.display().to_string(),
                Platform::Windows,
            )
        };
        let reply = shells_value(&windows, &settings, &installed, &commands);
        assert_eq!(reply["type"], "shells");
        assert_eq!(reply["platform"], "windows");
        assert_eq!(reply["shell"], "pwsh");
        assert_eq!(reply["terminal_shell"], Value::Null);
        assert_eq!(reply["auto_order"], json!(WINDOWS_AUTO_ORDER.map(Kind::id)));
        let ids: Vec<_> = reply["shells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|shell| (shell["id"].as_str().unwrap(), shell["installed"] == true))
            .collect();
        assert_eq!(
            ids,
            [("pwsh", false), ("powershell", true), ("gitbash", true)]
        );
        // As paths: Windows' separators may differ in the text.
        let path = PathBuf::from(reply["shells"][1]["path"].as_str().unwrap());
        assert_eq!(path, powershell);
        assert_eq!(reply["shells"][1]["version"], "5.1.22621.4391");
        // PowerShell 7 is missing: auto's first installed shell runs (Git
        // Bash, or Windows PowerShell with PowerShell 7 first), and says why.
        let auto = windows.auto();
        assert!(matches!(auto.kind, Kind::GitBash | Kind::WindowsPowerShell));
        assert_eq!(reply["auto"]["id"], auto.kind.id());
        assert_eq!(reply["commands"]["id"], auto.kind.id());
        assert!(reply["commands"]["notes"][0]
            .as_str()
            .unwrap()
            .starts_with("PowerShell 7 is not installed"));
        assert_eq!(
            reply["terminal"]["program"],
            auto.path.display().to_string()
        );
        if auto.path == bash {
            assert_eq!(reply["terminal"]["env"]["CHERE_INVOKING"], "1");
        }
    }

    #[test]
    fn versions_are_read_from_what_each_shell_prints() {
        assert_eq!(
            parse_version(Kind::Pwsh, "PowerShell 7.5.2\n").as_deref(),
            Some("7.5.2")
        );
        assert_eq!(
            parse_version(Kind::Pwsh, "PowerShell 7.6.0-preview.4").as_deref(),
            Some("7.6.0")
        );
        assert_eq!(
            parse_version(Kind::WindowsPowerShell, "5.1.22621.4391\r\n").as_deref(),
            Some("5.1.22621.4391")
        );
        assert_eq!(
            parse_version(Kind::Zsh, "zsh 5.9 (arm64-apple-darwin26.0)").as_deref(),
            Some("5.9")
        );
        assert_eq!(
            parse_version(
                Kind::GitBash,
                "GNU bash, version 5.2.37(1)-release (x86_64-pc-msys)"
            )
            .as_deref(),
            Some("5.2.37")
        );
        assert_eq!(
            parse_version(Kind::Sh, "GNU bash, version 3.2.57(1)-release").as_deref(),
            Some("bash 3.2.57")
        );
        assert_eq!(parse_version(Kind::Sh, "sh: 0: Illegal option --"), None);
        assert_eq!(parse_version(Kind::Bash, "no version here"), None);
        assert_eq!(Kind::Pwsh.label(Some("7.5.2")), "PowerShell 7.5.2");
        assert_eq!(Kind::Pwsh.label(None), "PowerShell 7");
        assert_eq!(
            Kind::GitBash.label(Some("5.2.37")),
            "Git Bash (bash 5.2.37)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn installed_shells_report_their_versions() {
        let dir = Dir::new("versions");
        dir.program("bin/zsh", "zsh 5.9 (x86_64-apple-darwin23.0)");
        dir.program("bin/bash", "GNU bash, version 5.2.37(1)-release");
        let path = join_paths(Platform::MacOs, &[dir.path("bin")]);
        let mac = host(Platform::MacOs, &[("PATH", &path)], None);
        let shells = installed(&mac);
        let found = |kind: Kind| shells.iter().find(|shell| shell.kind == kind).unwrap();
        assert_eq!(found(Kind::Zsh).version.as_deref(), Some("5.9"));
        assert_eq!(found(Kind::Bash).version.as_deref(), Some("5.2.37"));
        assert_eq!(found(Kind::Zsh).path, Some(dir.path("bin/zsh")));
        assert_eq!(shells.len(), 3);
    }

    fn command_shell(kind: Kind, path: &str, platform: Platform) -> CommandShell {
        CommandShell {
            platform,
            shell: Shell {
                kind,
                path: PathBuf::from(path),
            },
            version: None,
            env: None,
            login: true,
            notes: Vec::new(),
            helper: None,
            instead_of: None,
        }
    }

    fn value<'a>(invocation: &'a Invocation, name: &str) -> Option<&'a OsStr> {
        invocation
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_os_str())
    }

    #[test]
    fn each_shell_gets_its_own_command_line() {
        let pwsh = command_shell(Kind::Pwsh, r"C:\PowerShell\7\pwsh.exe", Platform::Windows);
        let line = pwsh.invocation("git status && echo ok");
        assert_eq!(line.program, r"C:\PowerShell\7\pwsh.exe");
        assert_eq!(
            line.args[..4],
            ["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]
        );
        // UTF-8 output and input before the command.
        let script = &line.args[4];
        assert!(script.starts_with(POWERSHELL_PRELUDE));
        assert!(script.ends_with("\ngit status && echo ok"));
        for part in [
            "$OutputEncoding = New-Object System.Text.UTF8Encoding $false",
            "[Console]::OutputEncoding = $OutputEncoding",
            "[Console]::InputEncoding = $OutputEncoding",
        ] {
            assert!(POWERSHELL_PRELUDE.contains(part), "{part}");
        }
        assert_eq!(value(&line, "LANG"), None);

        let windows = command_shell(Kind::WindowsPowerShell, "powershell.exe", Platform::Windows);
        assert_eq!(
            windows.invocation("dir").args[4],
            format!("{POWERSHELL_PRELUDE}\ndir")
        );

        let git_bash = command_shell(Kind::GitBash, r"C:\Git\bin\bash.exe", Platform::Windows);
        let line = git_bash.invocation("ls");
        assert_eq!(line.args, ["-lc", "ls"]);
        assert_eq!(value(&line, "CHERE_INVOKING"), Some(OsStr::new("1")));

        let sh = command_shell(Kind::Sh, "sh", Platform::Linux);
        assert_eq!(sh.invocation("ls").args, ["-lc", "ls"]);

        // With the login environment: `-c`, in that environment.
        let mut zsh = command_shell(Kind::Zsh, "/bin/zsh", Platform::MacOs);
        zsh.login = false;
        zsh.env = Some(Arc::new(vec![
            ("PATH".into(), "/Users/me/.nvm/bin:/usr/bin".into()),
            ("PAGER".into(), "less".into()),
            ("LANG".into(), "zh_CN.UTF-8".into()),
        ]));
        let line = zsh.invocation("node -v");
        assert_eq!(line.args, ["-c", "node -v"]);
        assert_eq!(
            value(&line, "PATH"),
            Some(OsStr::new("/Users/me/.nvm/bin:/usr/bin"))
        );
        // The user's locale stays; the model's variables win over the rest.
        assert_eq!(value(&line, "LANG"), Some(OsStr::new("zh_CN.UTF-8")));
        assert_eq!(value(&line, "PAGER"), Some(OsStr::new("cat")));
        assert_eq!(line.env.iter().filter(|(key, _)| key == "PAGER").count(), 1);
    }

    #[test]
    fn model_commands_get_no_colours_pagers_or_prompts() {
        for kind in [Kind::Pwsh, Kind::GitBash, Kind::Zsh] {
            let line = command_shell(kind, "shell", Platform::Windows).invocation("x");
            for (name, expected) in MODEL_ENV {
                assert_eq!(value(&line, name), Some(OsStr::new(expected)), "{name}");
            }
            if env::var_os("GIT_TERMINAL_PROMPT").is_none() {
                assert_eq!(value(&line, "GIT_TERMINAL_PROMPT"), Some(OsStr::new("0")));
            }
        }
        // A GIT_TERMINAL_PROMPT the user set stays.
        let mut zsh = command_shell(Kind::Zsh, "/bin/zsh", Platform::MacOs);
        zsh.env = Some(Arc::new(vec![("GIT_TERMINAL_PROMPT".into(), "1".into())]));
        let line = zsh.invocation("git fetch");
        assert_eq!(value(&line, "GIT_TERMINAL_PROMPT"), Some(OsStr::new("1")));
        assert_eq!(
            line.env
                .iter()
                .filter(|(key, _)| key == "GIT_TERMINAL_PROMPT")
                .count(),
            1
        );
        // A terminal is the user's: none of these.
        let dir = Dir::new("terminal-env");
        let zsh = dir.program("bin/zsh", "");
        let terminal = host(Platform::MacOs, &[("SHELL", &zsh)], None).terminal("auto");
        assert!(terminal.env.is_empty());
    }

    #[test]
    fn the_prompt_names_the_shell_and_how_to_write_for_it() {
        let mut shell = command_shell(Kind::Pwsh, "pwsh.exe", Platform::Windows);
        shell.version = Some("7.5.2".to_string());
        assert_eq!(
            shell.prompt(),
            "Commands run in PowerShell 7.5.2 (pwsh): use PowerShell syntax, such as `$env:NAME` for environment variables."
        );
        let mut shell = command_shell(Kind::WindowsPowerShell, "powershell.exe", Platform::Windows);
        shell.version = Some("5.1.22621.4391".to_string());
        let prompt = shell.prompt();
        assert!(prompt.starts_with("Commands run in Windows PowerShell 5.1.22621.4391."));
        assert!(prompt.contains("no `&&` or `||`"));
        assert!(prompt.contains("`;`"));
        assert!(prompt.contains("`if ($?) { … }`"));

        let mut shell = command_shell(Kind::GitBash, "bash.exe", Platform::Windows);
        shell.version = Some("5.2.37".to_string());
        shell.helper = Some((Kind::Pwsh, Some("7.5.2".to_string())));
        let prompt = shell.prompt();
        assert!(prompt
            .starts_with("Commands run in Git Bash (bash 5.2.37) on Windows: use bash syntax"));
        assert!(prompt.ends_with(
            "For Windows-native work, run `pwsh -NoProfile -Command \"…\"` (PowerShell 7.5.2)."
        ));
        shell.helper = Some((Kind::WindowsPowerShell, None));
        assert!(shell
            .prompt()
            .ends_with("run `powershell -NoProfile -Command \"…\"` (Windows PowerShell 5.1)."));
        shell.helper = None;
        assert!(!shell.prompt().contains("Windows-native"));

        let mut shell = command_shell(Kind::Zsh, "/bin/zsh", Platform::MacOs);
        shell.version = Some("5.9".to_string());
        assert_eq!(shell.prompt(), "Commands run in zsh 5.9.");
    }

    #[test]
    fn a_snapshot_must_be_complete_and_have_a_path() {
        let complete =
            b"PATH=/a:/b\0PWD=/home/me\0MULTI=one\ntwo\0_=/usr/bin/env\0__LYNSHEN_ENV_END__";
        let vars = parse_snapshot(complete).unwrap();
        assert_eq!(
            vars,
            vec![
                (OsString::from("PATH"), OsString::from("/a:/b")),
                (OsString::from("MULTI"), OsString::from("one\ntwo")),
            ]
        );
        assert!(parse_snapshot(b"PATH=/a\0").is_err());
        assert!(parse_snapshot(b"HOME=/h\0__LYNSHEN_ENV_END__").is_err());
    }

    /// A made-up home whose bash profile adds to PATH, as ~/.zshrc does for
    /// nvm or pyenv; `profile` is the rest of it.
    #[cfg(unix)]
    fn bash_home(label: &str, profile: &str) -> (Dir, Vars) {
        let dir = Dir::new(label);
        fs::write(
            dir.path(".bash_profile"),
            format!("export PATH=\"$HOME/tools/bin:$PATH\"\nexport FROM_RC=yes\n{profile}\n"),
        )
        .unwrap();
        let env = vec![
            ("HOME".into(), dir.0.clone().into_os_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ];
        (dir, env)
    }

    #[cfg(unix)]
    fn find_bash() -> Option<PathBuf> {
        ["/bin/bash", "/usr/bin/bash"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
    }

    #[cfg(unix)]
    #[test]
    fn the_snapshot_has_what_the_login_shell_set_up() {
        let Some(bash) = find_bash() else { return };
        let (dir, env) = bash_home("snapshot", "echo noise from the profile");
        let vars = snapshot(&bash, Some(&env), SNAPSHOT_TIMEOUT).unwrap();
        let var = |name: &str| {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        // After what /etc/profile set (path_helper on macOS).
        let path = var("PATH").unwrap();
        assert!(
            path.starts_with(&format!("{}/tools/bin:", dir.0.display())),
            "{path}"
        );
        assert_eq!(var("FROM_RC").as_deref(), Some("yes"));
        assert_eq!(var("PWD"), None);
        assert_eq!(var(SNAPSHOT_FILE_VAR), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_zsh_snapshot_has_path_from_zprofile_and_zshrc() {
        let zsh = PathBuf::from("/bin/zsh");
        if !zsh.is_file() {
            return;
        }
        let dir = Dir::new("zsh-snapshot");
        fs::write(
            dir.path(".zprofile"),
            "export PATH=\"$HOME/brew/bin:$PATH\"\n",
        )
        .unwrap();
        // What `nvm` or `conda init` add, and an rc file that talks and
        // reads input.
        fs::write(
            dir.path(".zshrc"),
            "export PATH=\"$HOME/nvm/bin:$PATH\"\necho welcome\nread -r answer\n",
        )
        .unwrap();
        let env = vec![
            ("HOME".into(), dir.0.clone().into_os_string()),
            ("ZDOTDIR".into(), dir.0.clone().into_os_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ];
        let vars = snapshot(&zsh, Some(&env), SNAPSHOT_TIMEOUT).unwrap();
        let path = vars
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, path)| path.to_string_lossy().into_owned())
            .unwrap();
        let home = dir.0.display();
        assert!(
            path.starts_with(&format!("{home}/nvm/bin:{home}/brew/bin:")),
            "{path}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_hanging_or_exiting_rc_file_ends_the_snapshot_without_it() {
        let Some(bash) = find_bash() else { return };
        let (_dir, env) = bash_home("snapshot-hang", "sleep 30");
        let started = Instant::now();
        let error = snapshot(&bash, Some(&env), Duration::from_millis(500)).unwrap_err();
        assert!(error.contains("did not finish"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        // The profile hangs a login shell too: commands run in /bin/sh.
        assert!(!login_starts(&bash, Some(&env), Duration::from_millis(500)));

        let (_dir, env) = bash_home("snapshot-exit", "exit 3");
        let error = snapshot(&bash, Some(&env), SNAPSHOT_TIMEOUT).unwrap_err();
        assert!(error.contains("wrote no environment"), "{error}");
        let (_dir, env) = bash_home("login-ok", "");
        assert!(login_starts(&bash, Some(&env), SNAPSHOT_TIMEOUT));
    }

    #[cfg(unix)]
    #[test]
    fn a_command_runs_in_the_snapshot_environment() {
        let Some(bash) = find_bash() else { return };
        let (dir, env) = bash_home("snapshot-run", "");
        fs::create_dir_all(dir.path("tools/bin")).unwrap();
        let tool = dir.path("tools/bin/mytool");
        fs::write(&tool, "#!/bin/sh\necho tool-ran\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let vars = snapshot(&bash, Some(&env), SNAPSHOT_TIMEOUT).unwrap();
        let shell = CommandShell {
            platform: Platform::current(),
            shell: Shell {
                kind: Kind::Bash,
                path: bash,
            },
            version: None,
            env: Some(Arc::new(vars)),
            login: false,
            notes: Vec::new(),
            helper: None,
            instead_of: None,
        };
        let line = shell.invocation("mytool; echo \"$FROM_RC $TERM $PAGER\"");
        let output = Command::new(&line.program)
            .args(&line.args)
            .env_clear()
            .envs(line.env.iter().cloned())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "tool-ran\nyes dumb cat\n"
        );
    }

    /// Runs `command` as the model's commands run on Windows.
    #[cfg(windows)]
    fn run_windows(shell: &CommandShell, command: &str) -> String {
        let line = shell.invocation(command);
        let output = Command::new(&line.program)
            .args(&line.args)
            .envs(line.env.iter().cloned())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).expect("UTF-8 output")
    }

    #[cfg(windows)]
    fn windows_shell(kind: Kind) -> Option<CommandShell> {
        let mut shell = command_shell(kind, "", Platform::Windows);
        shell.shell.path = Host::current().find(kind)?;
        shell.version = probe_version(&shell.shell);
        Some(shell)
    }

    #[cfg(windows)]
    #[test]
    fn powershell_7_runs_commands_with_utf8_output() {
        let Some(pwsh) = windows_shell(Kind::Pwsh) else {
            assert!(
                env::var_os("GITHUB_ACTIONS").is_none(),
                "windows-latest has PowerShell 7"
            );
            return;
        };
        assert!(
            pwsh.version.as_deref().is_some_and(|v| v.starts_with('7')),
            "{pwsh:?}"
        );
        let output = run_windows(&pwsh, "Write-Output '中文 ok' && Write-Output $env:TERM");
        assert_eq!(output.trim_end(), "中文 ok\r\ndumb");
    }

    #[cfg(windows)]
    #[test]
    fn windows_powershell_runs_commands_with_utf8_output() {
        let shell = windows_shell(Kind::WindowsPowerShell).expect("Windows PowerShell");
        assert!(
            shell
                .version
                .as_deref()
                .is_some_and(|v| v.starts_with("5.1")),
            "{shell:?}"
        );
        // The characters are built in PowerShell, so only the output's
        // encoding is tested.
        let output = run_windows(
            &shell,
            "Write-Output ([string][char]0x4E2D + [char]0x6587); if ($?) { Write-Output next }",
        );
        assert_eq!(output.trim_end(), "中文\r\nnext");
    }

    #[cfg(windows)]
    #[test]
    fn git_bash_runs_commands_in_the_working_directory() {
        let Some(bash) = windows_shell(Kind::GitBash) else {
            assert!(
                env::var_os("GITHUB_ACTIONS").is_none(),
                "windows-latest has Git Bash"
            );
            return;
        };
        let dir = Dir::new("git-bash");
        let line = bash.invocation("echo $((40+2)) && pwd -W");
        let output = Command::new(&line.program)
            .args(&line.args)
            .envs(line.env.iter().cloned())
            .current_dir(&dir.0)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        let name = dir.0.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(text.starts_with("42"), "{text}");
        assert!(text.contains(&name), "{text}");
    }
}

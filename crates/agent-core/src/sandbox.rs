//! OS sandbox for shell commands, modelled on Codex: the sandbox sets what a
//! command can touch, the approval mode decides what happens when a command
//! asks to run outside it (`escalate`).
//!
//! - `read-only`: commands can read, not write.
//! - `workspace-write`: the working directory, the agent's read-write
//!   directories, temp and package-cache directories are writable. Inside
//!   them `.git`, `.lynshen` and `.agents` stay read-only.
//! - `full-access`: no sandbox.
//!
//! Credentials (`~/.ssh`, `~/.gnupg`, `~/.aws`, LynShen's own auth and daemon
//! state) are unreadable in every sandboxed mode. macOS uses Seatbelt
//! (`sandbox-exec`), Linux `bwrap`. Windows has no OS sandbox by default: a
//! mode other than `full-access` is a boundary the model is told about and
//! declares crossing (`escalate`), not one that is enforced (`enforced`).
//! With `sandbox_windows = "restricted-token"` (a prototype, see
//! `WindowsSandbox`) LynShen sandboxes commands there itself. File tools run
//! in-process and check writes against the same rules.

use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

/// How shell commands are sandboxed on Windows (`sandbox_windows` in
/// config.json). Elsewhere the setting has no effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsSandbox {
    /// No OS sandbox (the default): the mode is not enforced.
    Off,
    /// A restricted, low-integrity token in a job object, set up by LynShen
    /// itself without administrator rights (`lynshen_sandbox::windows`).
    RestrictedToken,
}

impl WindowsSandbox {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "off" => Ok(Self::Off),
            "restricted-token" => Ok(Self::RestrictedToken),
            other => Err(format!(
                "unknown sandbox_windows '{other}': use off or restricted-token"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::RestrictedToken => "restricted-token",
        }
    }
}

/// This process's Windows sandbox: a machine setting, so one value for every
/// session and agent the process runs.
static WINDOWS_RESTRICTED_TOKEN: AtomicBool = AtomicBool::new(false);

/// Sets this process's Windows sandbox (config.json `sandbox_windows`, read
/// with the config).
pub fn set_windows_sandbox(choice: WindowsSandbox) {
    WINDOWS_RESTRICTED_TOKEN.store(choice == WindowsSandbox::RestrictedToken, Ordering::Relaxed);
}

pub fn windows_sandbox() -> WindowsSandbox {
    if WINDOWS_RESTRICTED_TOKEN.load(Ordering::Relaxed) {
        WindowsSandbox::RestrictedToken
    } else {
        WindowsSandbox::Off
    }
}

/// Whether the Windows sandbox runs commands here: on Windows, with
/// `sandbox_windows = "restricted-token"`.
pub fn windows_sandbox_on() -> bool {
    cfg!(windows) && windows_sandbox() == WindowsSandbox::RestrictedToken
}

static RUNNER: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The program that starts a command in the Windows sandbox
/// (`lynshen __sandbox-exec …`, see `lynshen_sandbox::RUNNER_ARG`): this
/// executable unless set here (tests set the `lynshen` binary).
pub fn set_sandbox_runner(path: PathBuf) {
    if let Ok(mut runner) = RUNNER.lock() {
        *runner = Some(path);
    }
}

#[cfg(windows)]
fn sandbox_runner() -> Result<PathBuf, String> {
    if let Some(runner) = RUNNER.lock().ok().and_then(|runner| runner.clone()) {
        return Ok(runner);
    }
    env::current_exe().map_err(|error| format!("cannot find the lynshen executable: {error}"))
}

/// Takes off what the Windows sandbox changed on disk for this process
/// (ACL entries, integrity labels) and removes its temp directory. Call
/// when the engine exits; what a crashed engine left is taken off by the
/// next one that sandboxes a command. Nothing to do elsewhere.
pub fn release_os_sandbox() {
    #[cfg(windows)]
    if let Err(error) = lynshen_sandbox::windows::cleanup() {
        crate::log_warn!(
            "sandbox",
            "the Windows sandbox could not undo every change; the next engine retries",
            error = error
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "full-access" => Ok(Self::FullAccess),
            other => Err(format!(
                "unknown sandbox '{other}': use read-only, workspace-write or full-access"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAccess => "full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    /// May run outside the sandbox without asking.
    Allow,
    /// Always asks, even where the approval mode would not.
    Ask,
    /// Never runs.
    Forbid,
}

impl RuleAction {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "forbid" => Ok(Self::Forbid),
            other => Err(format!(
                "unknown rule action '{other}': use allow, ask or forbid"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRule {
    /// Matches a command whose words start with these words.
    pub prefix: String,
    pub action: RuleAction,
}

/// What the command rules and the built-in lists say about a command (see
/// `SandboxPolicy::check_command`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandCheck {
    /// `Forbid`: a part never runs. `Ask`: a person must decide, in every
    /// approval mode. `Allow`: a rule allows every part. None: the approval
    /// mode decides.
    pub action: Option<RuleAction>,
    /// Why it is forbidden or must be asked.
    pub reason: Option<String>,
    /// A part runs with elevated privileges (sudo, su, doas, runas, ...).
    pub elevation: bool,
    /// No approval may reach beyond this call: an interpreter, a delete, an
    /// elevation, a forced push.
    pub once_only: bool,
    /// The prefix a session or project approval would remember; None when
    /// its parts start differently.
    pub prefix: Option<String>,
    /// The paths its text shows it writes.
    pub writes: Vec<PathBuf>,
    /// A write its text shows lands outside the boundary (the workspace,
    /// the read-write directories, temp and the package caches).
    pub writes_outside: bool,
    /// It deletes something its text does not name (a variable, a command
    /// substitution, `xargs rm`): outside the sandbox that may be anywhere.
    pub unknown_delete: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Directories outside the working directory that commands and file
    /// tools may write.
    pub writable_dirs: Vec<PathBuf>,
    /// Directories outside the working directory that file tools may read.
    pub readable_dirs: Vec<PathBuf>,
    pub network: bool,
    pub rules: Vec<CommandRule>,
    /// Command prefixes allowed for this session inside the sandbox only:
    /// manual mode stops asking for them, an escalation still asks (see
    /// `allowed_in_sandbox`).
    pub sandboxed_allow: Vec<String>,
}

impl SandboxPolicy {
    /// Whether commands run inside an OS sandbox.
    pub fn is_sandboxed(&self) -> bool {
        self.mode != SandboxMode::FullAccess
    }

    /// Whether an OS sandbox enforces the mode: macOS and Linux, and
    /// Windows with `sandbox_windows = "restricted-token"`, in a mode other
    /// than `full-access`. Elsewhere commands run unconfined.
    pub fn enforced(&self) -> bool {
        self.is_sandboxed() && os_sandbox()
    }

    /// Why the OS sandbox cannot run here (bubblewrap missing, or blocked by
    /// AppArmor), with how to fix it; None when it runs or is not used.
    pub fn unavailable(&self) -> Option<String> {
        self.check_available().err()
    }

    /// What the command rules and the built-in lists say about `command`
    /// (see `check_command`), without the checks that need its directory.
    pub fn rule_for(&self, command: &str) -> Option<RuleAction> {
        self.check_command(command, None).action
    }

    /// What the command rules and the built-in lists say about `command`,
    /// run from `dirs` = (its directory, the agent's workspace).
    ///
    /// The command is split into its parts (`&&`, `||`, `;`, `|`, ...; see
    /// `command_text`), wrappers such as `sudo` or `env` come off, and each
    /// part is matched on its own: `forbid` (a rule or the built-in list)
    /// wins, then the built-in list that always asks, then the longest
    /// matching rule. The command is forbidden when a part is, must be asked
    /// when a part must, and allowed when every part but the neutral ones
    /// (`cd`, `echo`, `grep`, ...) is.
    pub fn check_command(&self, command: &str, dirs: Option<(&Path, &Path)>) -> CommandCheck {
        use crate::command_text as text;
        let home = home().map(|home| real(&home));
        let credentials = denied_reads();
        let settings = agent_settings();
        let roots = dirs.map(|(_, workspace)| self.boundary_roots(workspace));
        let workspace = dirs.map(|(_, workspace)| real(workspace));
        let mut check = CommandCheck::default();
        let mut forbid: Option<String> = None;
        let mut ask: Option<String> = None;
        let mut all_allowed = true;
        let mut counted = 0;
        let mut prefixes: Vec<String> = Vec::new();
        let mut no_prefix = false;
        let parts = text::segments(command);
        for part in &parts {
            let user = self.user_rule(part);
            let mut part_forbid = (user == Some(RuleAction::Forbid))
                .then(|| "forbidden by the agent's command rules".to_string());
            let mut part_ask = (user == Some(RuleAction::Ask))
                .then(|| "it matches an ask command rule".to_string());
            let elevated = part.elevated
                || matches!(
                    part.program().as_str(),
                    "sudo" | "su" | "doas" | "pkexec" | "gsudo" | "runas"
                );
            if elevated {
                check.elevation = true;
                part_ask.get_or_insert_with(|| "it runs with elevated privileges".to_string());
            }
            if let Some(force) = text::git_push(part) {
                check.once_only |= force;
                part_ask.get_or_insert_with(|| "it pushes to a git remote".to_string());
            }
            if let Some(setting) = text::system_setting(part) {
                part_ask.get_or_insert_with(|| format!("it {setting}"));
            }
            if let Some((cwd, _)) = dirs {
                let resolve = |word: &str| text::resolve_word(word, cwd, home.as_deref());
                if text::is_delete(part) {
                    let targets = text::delete_targets(part);
                    if targets.is_empty() {
                        check.unknown_delete = true;
                    }
                    for word in &targets {
                        let Some(target) = resolve(word) else {
                            check.unknown_delete = true;
                            continue;
                        };
                        let real_target = real_or_parent(&target);
                        // The root, the home directory or a directory around
                        // it, or around the workspace.
                        let around = |dir: &PathBuf| {
                            dir.starts_with(&real_target) || dir.starts_with(&target)
                        };
                        let protected = target.parent().is_none()
                            || real_target.parent().is_none()
                            || home.as_ref().is_some_and(around)
                            || workspace.as_ref().is_some_and(|workspace| {
                                workspace != &real_target && workspace.starts_with(&real_target)
                            });
                        if protected {
                            part_forbid.get_or_insert_with(|| {
                                format!(
                                    "never run: it deletes {}, the root, the home directory or a parent of the workspace",
                                    target.display()
                                )
                            });
                        } else if !roots
                            .iter()
                            .flatten()
                            .any(|root| real_target.starts_with(root))
                        {
                            part_ask.get_or_insert_with(|| {
                                format!("it deletes {}, outside the workspace", target.display())
                            });
                        }
                    }
                }
                let mentioned = part
                    .raw
                    .iter()
                    .chain(&part.redirects)
                    .filter(|word| text::looks_like_path(word))
                    .filter_map(|word| resolve(word));
                for path in mentioned {
                    let real_path = real_or_parent(&path);
                    let inside = |guarded: &PathBuf| {
                        real_path.starts_with(guarded) || path.starts_with(guarded)
                    };
                    if credentials.iter().any(inside) {
                        part_ask.get_or_insert_with(|| {
                            format!("it touches credentials ({})", path.display())
                        });
                    } else if !text::is_neutral(part) && settings.iter().any(inside) {
                        // Its own settings: their rules decide what asks.
                        part_ask.get_or_insert_with(|| {
                            format!("it changes LynShen's settings ({})", path.display())
                        });
                    }
                }
                for target in text::write_targets(part).iter().filter_map(|w| resolve(w)) {
                    let real_target = real_or_parent(&target);
                    if !roots
                        .iter()
                        .flatten()
                        .any(|root| real_target.starts_with(root))
                    {
                        check.writes_outside = true;
                    }
                    if !check.writes.contains(&target) {
                        check.writes.push(target);
                    }
                }
            }
            check.once_only |= text::is_interpreter(part) || text::is_delete(part) || elevated;
            if let Some(reason) = part_forbid {
                forbid.get_or_insert(reason);
            } else if let Some(reason) = part_ask {
                ask.get_or_insert(reason);
            }
            if !text::is_neutral(part) {
                counted += 1;
                all_allowed &= user == Some(RuleAction::Allow);
                match text::prefix(part) {
                    Some(prefix) if !prefixes.contains(&prefix) => prefixes.push(prefix),
                    Some(_) => {}
                    None => no_prefix = true,
                }
            }
        }
        if counted == 0 {
            prefixes.extend(parts.first().and_then(text::prefix));
        }
        // A write outside goes where the user saw it go, once: no rule
        // covers it, and no prefix is remembered for it (a client makes
        // the directory writable instead).
        if check.writes_outside {
            all_allowed = false;
            check.once_only = true;
        }
        check.prefix = (!no_prefix && prefixes.len() == 1).then(|| prefixes.remove(0));
        (check.action, check.reason) = match (forbid, ask) {
            (Some(reason), _) => (Some(RuleAction::Forbid), Some(reason)),
            (None, Some(reason)) => (Some(RuleAction::Ask), Some(reason)),
            _ if counted > 0 && all_allowed => (Some(RuleAction::Allow), None),
            _ => (None, None),
        };
        check
    }

    /// Whether every part of `command` but the neutral ones starts with a
    /// prefix allowed inside the sandbox (`sandboxed_allow`).
    pub fn allowed_in_sandbox(&self, command: &str) -> bool {
        use crate::command_text as text;
        if self.sandboxed_allow.is_empty() {
            return false;
        }
        let mut counted = 0;
        for part in text::segments(command) {
            if text::is_neutral(&part) {
                continue;
            }
            counted += 1;
            let words = text::allow_words(&part);
            if !self
                .sandboxed_allow
                .iter()
                .any(|prefix| words_start_with(words, prefix, false))
            {
                return false;
            }
        }
        counted > 0
    }

    /// The command rule for one part of a command line: `forbid` wins over
    /// any other match, otherwise the longest matching prefix, `ask` before
    /// `allow` at the same length. `ask` and `forbid` match the part in any
    /// case, as written, without its wrappers or with the program named by
    /// its file name (`/usr/bin/git push`, `GIT.EXE PUSH`); `allow` matches
    /// only the words as written (`command_text::allow_words`), so
    /// `./git commit` or `PATH=. git commit` is not `git commit`.
    fn user_rule(&self, part: &crate::command_text::Segment) -> Option<RuleAction> {
        let mut named = part.words.clone();
        if let Some(first) = named.first_mut() {
            *first = crate::command_text::program_name(first);
        }
        let loose = [&part.raw, &part.words, &named];
        let exact = crate::command_text::allow_words(part);
        let mut best: Option<(usize, RuleAction)> = None;
        for rule in &self.rules {
            let matched = match rule.action {
                RuleAction::Allow => words_start_with(exact, &rule.prefix, false),
                RuleAction::Ask | RuleAction::Forbid => loose
                    .iter()
                    .any(|words| words_start_with(words, &rule.prefix, true)),
            };
            if !matched {
                continue;
            }
            if rule.action == RuleAction::Forbid {
                return Some(RuleAction::Forbid);
            }
            let length = rule.prefix.split_whitespace().count();
            let better = match best {
                None => true,
                Some((known, action)) => {
                    length > known
                        || (length == known
                            && action == RuleAction::Allow
                            && rule.action == RuleAction::Ask)
                }
            };
            if better {
                best = Some((length, rule.action));
            }
        }
        best.map(|(_, action)| action)
    }

    /// Where the agent may write without crossing the boundary, whatever
    /// the mode: the workspace, the read-write directories, temp and the
    /// package caches.
    fn boundary_roots(&self, workspace: &Path) -> Vec<PathBuf> {
        let mut roots = vec![workspace.to_path_buf()];
        roots.extend(self.writable_dirs.iter().cloned());
        roots.extend(temp_dirs());
        roots.extend(cache_dirs());
        roots.iter().map(|root| real(root)).collect()
    }

    /// Whether a write to `path` stays inside the boundary (see
    /// `boundary_roots`).
    pub fn within_boundary(&self, workspace: &Path, path: &Path) -> bool {
        let target = real_or_parent(path);
        self.boundary_roots(workspace)
            .iter()
            .any(|root| target.starts_with(root))
    }

    /// Directories a command started in `cwd` may write.
    pub fn writable_roots(&self, cwd: &Path) -> Vec<PathBuf> {
        if self.mode == SandboxMode::ReadOnly {
            return Vec::new();
        }
        let mut roots = vec![cwd.to_path_buf()];
        roots.extend(self.writable_dirs.iter().cloned());
        roots.extend(temp_dirs());
        roots.extend(cache_dirs());
        let mut roots: Vec<PathBuf> = roots.iter().map(|root| real(root)).collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// `.git` (and a worktree's real git directory), `.lynshen` and `.agents`
    /// under the working directory and the read-write directories, and the
    /// read-only directories. A path
    /// that contains `cwd` itself is left writable, so a subagent working
    /// inside `.lynshen/agents/…` can still write its own worktree.
    pub fn protected_paths(&self, cwd: &Path) -> Vec<PathBuf> {
        let cwd = real(cwd);
        let mut roots = vec![cwd.clone()];
        roots.extend(self.writable_dirs.iter().map(|dir| real(dir)));
        let mut protected = Vec::new();
        for root in roots {
            for name in [".git", ".lynshen", ".agents"] {
                let path = root.join(name);
                if path.is_dir() {
                    protected.push(path);
                } else if name == ".git" && path.is_file() {
                    // A worktree's `.git` file points at its git directory.
                    protected.push(path.clone());
                    if let Some(target) = gitdir_target(&path) {
                        protected.push(real(&target));
                    }
                }
            }
        }
        // Read-only directories stay read-only even when they sit under a
        // writable root such as the temp directory.
        protected.extend(self.readable_dirs.iter().map(|dir| real(dir)));
        protected.retain(|path| !cwd.starts_with(path));
        protected.sort();
        protected.dedup();
        protected
    }

    /// Checks an in-process file write against the sandbox. `path` is the
    /// resolved target.
    pub fn check_write(&self, cwd: &Path, path: &Path) -> Result<(), String> {
        if !self.is_sandboxed() {
            return Ok(());
        }
        if self.mode == SandboxMode::ReadOnly {
            return Err("the sandbox is read-only: files cannot be written".to_string());
        }
        let target = real_or_parent(path);
        if let Some(protected) = self
            .protected_paths(cwd)
            .into_iter()
            .find(|protected| target.starts_with(protected))
        {
            return Err(format!(
                "{} is read-only in the sandbox (inside {}); change it with a shell command using escalate",
                path.display(),
                protected.display()
            ));
        }
        if self
            .writable_roots(cwd)
            .iter()
            .any(|root| target.starts_with(root))
        {
            Ok(())
        } else {
            Err(format!(
                "{} is outside the sandbox's writable directories",
                path.display()
            ))
        }
    }

    /// The command line to run `program args` in the sandbox from `cwd`.
    /// Unchanged under `full-access`.
    pub fn wrap(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<(String, Vec<String>), String> {
        self.wrap_with(program, args, cwd, &[], cwd)
    }

    /// `wrap` for an agent confined to `root`, a subagent's worktree inside
    /// `project`, started from `cwd` (inside `root`): `root`, temp and
    /// package-cache directories are writable; `project` (around `root`)
    /// and the read-write directories are read-only, even where they lie in
    /// temp.
    pub fn wrap_confined(
        &self,
        program: &str,
        args: &[&str],
        root: &Path,
        project: &Path,
        cwd: &Path,
    ) -> Result<(String, Vec<String>), String> {
        let mut denied = vec![project.to_path_buf()];
        denied.extend(self.writable_dirs.iter().cloned());
        let confined = SandboxPolicy {
            writable_dirs: Vec::new(),
            ..self.clone()
        };
        confined.wrap_with(program, args, root, &denied, cwd)
    }

    fn wrap_with(
        &self,
        program: &str,
        args: &[&str],
        root: &Path,
        denied: &[PathBuf],
        cwd: &Path,
    ) -> Result<(String, Vec<String>), String> {
        let plain = || {
            (
                program.to_string(),
                args.iter().map(|arg| arg.to_string()).collect(),
            )
        };
        if !self.is_sandboxed() {
            return Ok(plain());
        }
        if cfg!(target_os = "macos") {
            let mut wrapped = vec![
                "-p".to_string(),
                self.seatbelt_profile_for(root, denied),
                "--".to_string(),
                program.to_string(),
            ];
            wrapped.extend(args.iter().map(|arg| arg.to_string()));
            Ok(("/usr/bin/sandbox-exec".to_string(), wrapped))
        } else if cfg!(target_os = "linux") {
            let bwrap = find_bwrap().ok_or_else(bwrap_missing)?;
            let mut wrapped = self.bwrap_args_for(root, denied, cwd);
            wrapped.push("--".to_string());
            wrapped.push(program.to_string());
            wrapped.extend(args.iter().map(|arg| arg.to_string()));
            Ok((bwrap.display().to_string(), wrapped))
        } else if windows_sandbox_on() {
            self.windows_wrap(program, args, root, denied)
        } else {
            // No OS sandbox (Windows by default): the command runs unconfined.
            Ok(plain())
        }
    }

    /// What a command started in `root` may touch in the Windows sandbox:
    /// `root` and the read-write directories (none in `read-only`) with
    /// their protected paths read-only, and a private temp directory. The
    /// user's temp directory and the package caches are not granted:
    /// granting a directory walks everything in it and opens it to every
    /// low-integrity process for the session. `denied` stay read-only (a
    /// confined agent's project).
    pub fn windows_spec(&self, root: &Path, denied: &[PathBuf]) -> lynshen_sandbox::Spec {
        let writable = self.mode != SandboxMode::ReadOnly;
        let mut roots = Vec::new();
        if writable {
            roots.push(real(root));
            roots.extend(self.writable_dirs.iter().map(|dir| real(dir)));
            roots.sort();
            roots.dedup();
        }
        lynshen_sandbox::Spec {
            writable_roots: roots,
            read_only: self.protected_paths(root),
            deny_write: denied.iter().map(|dir| real(dir)).collect(),
            deny_read: denied_reads(),
            network: self.network,
            private_temp: writable,
        }
    }

    #[cfg(windows)]
    fn windows_wrap(
        &self,
        program: &str,
        args: &[&str],
        root: &Path,
        denied: &[PathBuf],
    ) -> Result<(String, Vec<String>), String> {
        use lynshen_sandbox::Backend;
        windows_shell_supported(program)?;
        windows_backend()?.wrap(
            &self.windows_spec(root, denied),
            &sandbox_runner()?,
            program,
            args,
        )
    }

    #[cfg(not(windows))]
    fn windows_wrap(
        &self,
        _program: &str,
        _args: &[&str],
        _root: &Path,
        _denied: &[PathBuf],
    ) -> Result<(String, Vec<String>), String> {
        Err("the Windows sandbox runs on Windows only".to_string())
    }

    /// Errors when this platform cannot run the OS sandbox, so a host can
    /// refuse to start instead of failing on the first command. A platform
    /// without one (Windows) has nothing to fail: see `enforced`.
    pub fn check_available(&self) -> Result<(), String> {
        if !self.is_sandboxed() {
            return Ok(());
        }
        if cfg!(target_os = "macos") {
            Path::new("/usr/bin/sandbox-exec")
                .exists()
                .then_some(())
                .ok_or_else(|| "sandbox-exec is missing".to_string())
        } else if cfg!(target_os = "linux") {
            let bwrap = find_bwrap().ok_or_else(bwrap_missing)?;
            bwrap_runs(&bwrap)
        } else if windows_sandbox_on() {
            windows_check()
        } else {
            Ok(())
        }
    }

    /// Seatbelt profile: everything is allowed except writing outside the
    /// writable roots (or into protected paths), reading credentials and,
    /// without network, outbound connections.
    pub fn seatbelt_profile(&self, cwd: &Path) -> String {
        self.seatbelt_profile_for(cwd, &[])
    }

    /// With `denied` directories (a confined agent), writes to them are
    /// denied, then `cwd` (which may lie in one) is opened again with its
    /// protected paths closed: the last matching rule wins.
    fn seatbelt_profile_for(&self, cwd: &Path, denied: &[PathBuf]) -> String {
        let mut profile =
            String::from("(version 1)\n(allow default)\n(deny file-write*\n  (require-all\n");
        for root in self.writable_roots(cwd) {
            profile.push_str(&format!(
                "    (require-not (subpath {}))\n",
                sbpl_string(&root)
            ));
        }
        profile.push_str("    (require-not (subpath \"/dev\"))))\n");
        for path in self.protected_paths(cwd) {
            profile.push_str(&format!(
                "(deny file-write* (subpath {}))\n",
                sbpl_string(&path)
            ));
        }
        for path in denied_reads() {
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath {}))\n",
                sbpl_string(&path)
            ));
        }
        if !denied.is_empty() {
            for dir in denied {
                profile.push_str(&format!(
                    "(deny file-write* (subpath {}))\n",
                    sbpl_string(&real(dir))
                ));
            }
            profile.push_str(&format!(
                "(allow file-write* (subpath {}))\n",
                sbpl_string(&real(cwd))
            ));
            for path in self.protected_paths(cwd) {
                profile.push_str(&format!(
                    "(deny file-write* (subpath {}))\n",
                    sbpl_string(&path)
                ));
            }
        }
        if !self.network {
            profile.push_str("(deny network-outbound (remote ip))\n");
        }
        profile
    }

    /// bwrap arguments: the whole file system read-only, writable roots
    /// bound read-write, protected paths re-bound read-only on top,
    /// credentials hidden and, without network, a new network namespace.
    /// A protected path may not exist (a read-only directory not created
    /// yet, a worktree's pruned git directory): bwrap fails every command on
    /// a `--ro-bind` it cannot find, so those binds are `--ro-bind-try`.
    pub fn bwrap_args(&self, cwd: &Path) -> Vec<String> {
        self.bwrap_args_for(cwd, &[], cwd)
    }

    /// With `denied` directories (a confined agent), they are bound
    /// read-only, then `root` (which may lie in one) read-write again with
    /// its protected paths read-only on top; the shell starts in `cwd`.
    fn bwrap_args_for(&self, root: &Path, denied: &[PathBuf], cwd: &Path) -> Vec<String> {
        let mut args: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        let path = |path: &Path| path.display().to_string();
        for writable in self.writable_roots(root).iter().filter(|dir| dir.exists()) {
            args.extend(["--bind".to_string(), path(writable), path(writable)]);
        }
        for protected in self.protected_paths(root) {
            args.extend([
                "--ro-bind-try".to_string(),
                path(&protected),
                path(&protected),
            ]);
        }
        if !denied.is_empty() {
            for dir in denied
                .iter()
                .map(|dir| real(dir))
                .filter(|dir| dir.exists())
            {
                args.extend(["--ro-bind".to_string(), path(&dir), path(&dir)]);
            }
            let root = real(root);
            args.extend(["--bind".to_string(), path(&root), path(&root)]);
            for protected in self.protected_paths(&root) {
                args.extend([
                    "--ro-bind-try".to_string(),
                    path(&protected),
                    path(&protected),
                ]);
            }
        }
        for secret in denied_reads().into_iter().filter(|secret| secret.exists()) {
            if secret.is_dir() {
                args.extend(["--tmpfs".to_string(), path(&secret)]);
            } else {
                args.extend([
                    "--ro-bind".to_string(),
                    "/dev/null".to_string(),
                    path(&secret),
                ]);
            }
        }
        if !self.network {
            args.push("--unshare-net".to_string());
        }
        args.extend([
            "--die-with-parent".to_string(),
            "--chdir".to_string(),
            path(cwd),
        ]);
        args
    }

    /// The system prompt's note on the sandbox: what commands and file
    /// tools may do, how to get more, and what always asks. Never empty,
    /// so the model is told in every mode, `full-access` and Windows too.
    pub fn prompt(&self) -> String {
        let list = |dirs: &[PathBuf]| {
            dirs.iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let extra = if self.writable_dirs.is_empty() {
            String::new()
        } else {
            format!(", {}", list(&self.writable_dirs))
        };
        let read_only = if self.readable_dirs.is_empty() {
            String::new()
        } else {
            format!(" Read-only: {}.", list(&self.readable_dirs))
        };
        let escalate =
            "with `escalate: true` and a one-line `justification`; it may need approval.";
        let (tag, shell) = if !self.is_sandboxed() {
            (
                format!("<sandbox mode=\"{}\">", self.mode.as_str()),
                "No sandbox: commands run with the user's permissions.".to_string(),
            )
        } else if !self.enforced() {
            (
                format!("<sandbox mode=\"{}\" enforced=\"false\">", self.mode.as_str()),
                format!(
                    "No OS sandbox on this platform: commands run with the user's permissions. Your boundary is the working directory{extra}, temp and package caches.{read_only} Run a command that writes outside it, installs software or changes settings {escalate}"
                ),
            )
        } else {
            // The Windows sandbox grants a private temp directory, not the
            // package caches, and cannot turn the network off.
            let windows = windows_sandbox_on();
            let writable = match self.mode {
                SandboxMode::ReadOnly => "Nothing is writable.".to_string(),
                _ if windows => format!(
                    "Writable: the working directory{extra} and a private temp directory (TEMP); .git, .lynshen and .agents in them are read-only. Package caches are read-only too, so installs (npm install, cargo fetch, pip install) need escalate."
                ),
                _ => format!(
                    "Writable: the working directory{extra}, temp and package caches; .git, .lynshen and .agents in them are read-only."
                ),
            };
            let network = if windows {
                // `sandbox_network: false` is not enforced there.
                "Network is not restricted (the Windows sandbox cannot turn it off)".to_string()
            } else {
                format!("Network is {}", if self.network { "on" } else { "off" })
            };
            (
                format!("<sandbox mode=\"{}\">", self.mode.as_str()),
                format!(
                    "Commands run in an OS sandbox. {writable}{read_only} {network}; credentials such as ~/.ssh are unreadable. For more (git commit, installing software, writing elsewhere{}), run the command {escalate}",
                    if self.network || windows { "" } else { ", network" },
                ),
            )
        };
        format!(
            "{tag}\n{shell}\nFile tools read anywhere but credentials; writing outside the writable directories may need approval. Always asked, in every mode: sudo, git push, deleting outside the workspace, system settings, credentials.\n</sandbox>"
        )
    }
}

impl SandboxPolicy {
    /// The default: the working directory writable, network on, commits
    /// allowed to leave the sandbox and pushes always asked. On Windows too:
    /// nothing enforces it there (`enforced`), but the model is told the
    /// boundary and declares crossing it, and file tools keep to it.
    pub fn default_for_platform() -> Self {
        Self {
            mode: SandboxMode::WorkspaceWrite,
            writable_dirs: Vec::new(),
            readable_dirs: Vec::new(),
            network: true,
            rules: rules_from_json(&default_rules_json()).expect("default rules parse"),
            sandboxed_allow: Vec::new(),
        }
    }

    /// This policy with `layer`'s directories and rules added.
    pub fn with_layer(mut self, layer: &SandboxLayer) -> Self {
        for dir in &layer.writable {
            if !self.writable_dirs.contains(dir) {
                self.writable_dirs.push(dir.clone());
            }
        }
        for dir in &layer.readable {
            if !self.readable_dirs.contains(dir) {
                self.readable_dirs.push(dir.clone());
            }
        }
        self.rules.extend(layer.rules.iter().cloned());
        for prefix in &layer.sandboxed {
            if !self.sandboxed_allow.contains(prefix) {
                self.sandboxed_allow.push(prefix.clone());
            }
        }
        self
    }
}

/// Directories and command rules a sandbox gets from one source: a
/// project's settings (`projects` in config.json) or the session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxLayer {
    pub writable: Vec<PathBuf>,
    pub readable: Vec<PathBuf>,
    pub rules: Vec<CommandRule>,
    /// Command prefixes allowed inside the sandbox only (the session's; see
    /// `SandboxPolicy::sandboxed_allow`).
    pub sandboxed: Vec<String>,
}

impl SandboxLayer {
    /// `{"sandbox_directories": [...], "command_rules": [...]}`, as a
    /// `projects` entry of config.json holds them. A directory that no
    /// longer exists is dropped, and an invalid rule skipped.
    pub fn from_json(value: &Value) -> Self {
        let (writable, readable) =
            directories_from_json(&value["sandbox_directories"], false).unwrap_or_default();
        let rules = value["command_rules"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|rule| rules_from_json(&json!([rule])).ok())
            .flatten()
            .collect();
        Self {
            writable,
            readable,
            rules,
            sandboxed: Vec::new(),
        }
    }

    /// Adds `dir`, read-write or read-only (moving it from the other list
    /// when it is there).
    pub fn add_dir(&mut self, dir: PathBuf, writable: bool) {
        self.remove_dir(&dir);
        if writable {
            self.writable.push(dir);
        } else {
            self.readable.push(dir);
        }
    }

    /// Removes `dir`; false when it was not there.
    pub fn remove_dir(&mut self, dir: &Path) -> bool {
        remove_dir_from(&mut self.writable, dir) | remove_dir_from(&mut self.readable, dir)
    }

    /// Adds an `allow` rule for `prefix`. A rule that asks or forbids the
    /// same prefix stays as it is (false): an approval never loosens one.
    pub fn allow(&mut self, prefix: &str) -> bool {
        match self
            .rules
            .iter()
            .find(|rule| rule.prefix.eq_ignore_ascii_case(prefix))
        {
            Some(rule) => rule.action == RuleAction::Allow,
            None => {
                self.rules.push(CommandRule {
                    prefix: prefix.to_string(),
                    action: RuleAction::Allow,
                });
                true
            }
        }
    }

    /// Allows `prefix` inside the sandbox only.
    pub fn allow_in_sandbox(&mut self, prefix: &str) {
        if !self.sandboxed.iter().any(|known| known == prefix) {
            self.sandboxed.push(prefix.to_string());
        }
    }
}

/// Removes `dir` from `dirs` (the same directory named another way too);
/// false when it was not there.
pub fn remove_dir_from(dirs: &mut Vec<PathBuf>, dir: &Path) -> bool {
    let before = dirs.len();
    let real_dir = real(dir);
    dirs.retain(|known| known != dir && real(known) != real_dir);
    dirs.len() != before
}

/// Sets `dir` to `mode` (`rw` or `ro`) in a `sandbox_directories` array,
/// adding it when missing.
pub fn directory_json_set(dirs: &mut Value, dir: &Path, mode: &str) {
    directory_json_remove(dirs, dir);
    dirs.as_array_mut()
        .expect("an array")
        .push(json!({ "path": dir.display().to_string(), "mode": mode }));
}

/// Removes `dir` from a `sandbox_directories` array; false when it was not
/// there.
pub fn directory_json_remove(dirs: &mut Value, dir: &Path) -> bool {
    if !dirs.is_array() {
        *dirs = json!([]);
    }
    let list = dirs.as_array_mut().expect("an array");
    let before = list.len();
    let real_dir = real(dir);
    list.retain(|entry| {
        let path = PathBuf::from(entry["path"].as_str().unwrap_or_default());
        path != dir && real(&path) != real_dir
    });
    list.len() != before
}

/// Adds `{"prefix", "action": "allow"}` to a `command_rules` array. A rule
/// for the same prefix stays as it is (false when it asks or forbids).
pub fn allow_rule_json(rules: &mut Value, prefix: &str) -> bool {
    if !rules.is_array() {
        *rules = json!([]);
    }
    let list = rules.as_array_mut().expect("an array");
    match list.iter().find(|rule| {
        rule["prefix"]
            .as_str()
            .is_some_and(|known| known.trim().eq_ignore_ascii_case(prefix))
    }) {
        Some(rule) => rule["action"] == "allow",
        None => {
            list.push(json!({ "prefix": prefix, "action": "allow" }));
            true
        }
    }
}

pub fn default_rules_json() -> Value {
    json!([
        { "prefix": "git add", "action": "allow" },
        { "prefix": "git commit", "action": "allow" },
        { "prefix": "git push", "action": "ask" },
    ])
}

/// `[{"prefix": "git push", "action": "allow" | "ask" | "forbid"}]`.
pub fn rules_from_json(value: &Value) -> Result<Vec<CommandRule>, String> {
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| {
            let prefix = item["prefix"].as_str().unwrap_or_default().trim();
            if prefix.is_empty() {
                return Err("a command rule needs a prefix".to_string());
            }
            Ok(CommandRule {
                prefix: prefix.to_string(),
                action: RuleAction::parse(item["action"].as_str().unwrap_or_default())?,
            })
        })
        .collect()
}

pub fn rules_to_json(rules: &[CommandRule]) -> Value {
    json!(rules
        .iter()
        .map(|rule| json!({
            "prefix": rule.prefix,
            "action": match rule.action {
                RuleAction::Allow => "allow",
                RuleAction::Ask => "ask",
                RuleAction::Forbid => "forbid",
            },
        }))
        .collect::<Vec<_>>())
}

/// `[{"path": "/abs/dir", "mode": "ro" | "rw"}]` → (read-write, read-only).
/// A directory that no longer exists is dropped when `strict` is false (a
/// saved setting), and is an error when true (a change being made).
pub fn directories_from_json(
    value: &Value,
    strict: bool,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut writable = Vec::new();
    let mut readable = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        let mode = item["mode"].as_str().unwrap_or("ro");
        if !matches!(mode, "ro" | "rw") {
            return Err(format!("directory mode must be ro or rw, not '{mode}'"));
        }
        let path = PathBuf::from(item["path"].as_str().unwrap_or_default());
        if !path.is_absolute() || !path.is_dir() {
            if strict {
                return Err(format!(
                    "not an existing absolute directory: {}",
                    path.display()
                ));
            }
            continue;
        }
        if mode == "rw" {
            writable.push(path);
        } else {
            readable.push(path);
        }
    }
    Ok((writable, readable))
}

pub fn directories_to_json(writable: &[PathBuf], readable: &[PathBuf]) -> Value {
    let entry =
        |path: &PathBuf, mode: &str| json!({ "path": path.display().to_string(), "mode": mode });
    json!(writable
        .iter()
        .map(|path| entry(path, "rw"))
        .chain(readable.iter().map(|path| entry(path, "ro")))
        .collect::<Vec<_>>())
}

fn gitdir_target(git_file: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(git_file).ok()?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let target = PathBuf::from(target);
    Some(if target.is_absolute() {
        target
    } else {
        git_file.parent()?.join(target)
    })
}

pub fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The home directory with symlinks resolved, as paths are compared.
pub fn real_home() -> Option<PathBuf> {
    home().map(|home| real(&home))
}

/// Credentials no sandboxed command, no file tool and no remote client may
/// read: `~/.ssh`, `~/.gnupg`, `~/.aws`, LynShen's own auth and daemon
/// state, the key that decrypts Desktop's stored secrets, and on Windows
/// GnuPG's `%APPDATA%\gnupg`.
pub fn denied_reads() -> Vec<PathBuf> {
    let Some(home) = home() else {
        return Vec::new();
    };
    let mut denied: Vec<PathBuf> = [
        ".ssh",
        ".gnupg",
        ".aws",
        ".lynshen/auth.json",
        ".lynshen/daemon",
    ]
    .iter()
    .map(|path| real(&home.join(path)))
    .collect();
    if cfg!(windows) {
        if let Some(appdata) = env::var_os("APPDATA") {
            denied.push(real(&PathBuf::from(appdata).join("gnupg")));
        }
    }
    for key in crate::secrets::key_files() {
        let key = real(&key);
        if !denied.contains(&key) {
            denied.push(key);
        }
    }
    denied
}

/// LynShen's own settings (config.json): a write there changes which
/// commands ask, so it always asks.
pub fn agent_settings() -> Vec<PathBuf> {
    home()
        .map(|home| vec![real(&home.join(".lynshen/config.json"))])
        .unwrap_or_default()
}

/// Paths a write always asks for, whatever the mode or the writable
/// directories: credentials and LynShen's settings.
pub fn guarded_writes() -> Vec<PathBuf> {
    let mut guarded = denied_reads();
    guarded.extend(agent_settings());
    guarded
}

/// Whether `words` start with the words of `prefix` (in any case with
/// `any_case`).
fn words_start_with(words: &[String], prefix: &str, any_case: bool) -> bool {
    let prefix: Vec<&str> = prefix.split_whitespace().collect();
    !prefix.is_empty()
        && words.len() >= prefix.len()
        && words.iter().zip(&prefix).all(|(word, want)| {
            if any_case {
                word.to_lowercase() == want.to_lowercase()
            } else {
                word == want
            }
        })
}

fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![env::temp_dir()];
    if cfg!(unix) {
        dirs.push(PathBuf::from("/tmp"));
    }
    dirs.into_iter().filter(|dir| dir.exists()).collect()
}

/// Package managers' caches, so installs and builds work in the sandbox.
fn cache_dirs() -> Vec<PathBuf> {
    let Some(home) = home() else {
        return Vec::new();
    };
    [
        ".cache",
        ".npm",
        ".pnpm-store",
        ".yarn",
        ".bun",
        ".cargo/registry",
        ".cargo/git",
        ".rustup",
        "go/pkg",
        ".m2",
        ".gradle",
        "Library/Caches",
    ]
    .iter()
    .map(|dir| home.join(dir))
    .filter(|dir| dir.exists())
    .collect()
}

/// The real path, or the path as given when it does not exist.
fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The real path of `path`, resolving its nearest existing ancestor when
/// `path` itself does not exist yet (a file about to be created).
fn real_or_parent(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Ok(real) = current.canonicalize() {
            return missing.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn find_bwrap() -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join("bwrap"))
            .find(|candidate| candidate.is_file())
    })
}

/// Whether this platform has an OS sandbox LynShen drives by default.
const OS_SANDBOX: bool = cfg!(any(target_os = "macos", target_os = "linux"));

/// Whether an OS sandbox runs commands here: `OS_SANDBOX`, or the Windows
/// sandbox when `sandbox_windows` turns it on.
fn os_sandbox() -> bool {
    OS_SANDBOX || windows_sandbox_on()
}

/// Git Bash (MSYS2) cannot start under the Windows sandbox's token: its
/// runtime opens pipes whose ACL names only the user, which a
/// write-restricted token cannot write. Commands then run in PowerShell
/// (`shell::for_commands`); this refusal is left for a machine with no
/// PowerShell, said plainly instead of in MSYS2's own words.
pub fn windows_shell_supported(program: &str) -> Result<(), String> {
    use crate::shell::{Kind, Platform};
    if Kind::of_program(Path::new(program), Platform::Windows) == Some(Kind::GitBash) {
        return Err("Git Bash cannot run in the Windows sandbox (preview), and no PowerShell was found to run commands in instead. Run this command with escalate: true, or ask the user to install PowerShell 7 or turn the Windows sandbox off".to_string());
    }
    Ok(())
}

/// The Windows backend, keeping its state (the ledgers of what it changed)
/// in `~/.lynshen/sandbox`.
#[cfg(windows)]
fn windows_backend() -> Result<lynshen_sandbox::windows::RestrictedToken, String> {
    let home = home().ok_or("the Windows sandbox needs a home directory (USERPROFILE)")?;
    Ok(lynshen_sandbox::windows::RestrictedToken::new(
        home.join(".lynshen").join("sandbox"),
    ))
}

/// Whether the Windows sandbox can run here: a restricted token can be
/// made (an interactive logon), and the runner exists.
#[cfg(windows)]
fn windows_check() -> Result<(), String> {
    use lynshen_sandbox::Backend;
    windows_backend()?.check()?;
    let runner = sandbox_runner()?;
    if !runner.is_file() {
        return Err(format!(
            "the Windows sandbox runner {} is missing",
            runner.display()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn windows_check() -> Result<(), String> {
    Ok(())
}

/// Starts `bwrap` once per process with the binds every command gets: on
/// Ubuntu 24.04 and later AppArmor stops it from creating the user
/// namespace it needs, and every command would fail.
fn bwrap_runs(bwrap: &Path) -> Result<(), String> {
    static PROBE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    PROBE
        .get_or_init(|| {
            let output = std::process::Command::new(bwrap)
                .args([
                    "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--", "sh", "-c",
                    ":",
                ])
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|error| format!("bwrap could not start: {error}"))?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "bwrap cannot create a sandbox here ({}). AppArmor on Ubuntu 24.04 and later blocks the user namespaces it needs: allow them with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` (or give bwrap an AppArmor profile), or set the sandbox to full-access",
                stderr.trim()
            ))
        })
        .clone()
}

fn bwrap_missing() -> String {
    "the sandbox needs bubblewrap (bwrap) on PATH: install it (apt install bubblewrap / dnf install bubblewrap), or set the sandbox to full-access".to_string()
}

fn sbpl_string(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: SandboxMode) -> SandboxPolicy {
        SandboxPolicy {
            mode,
            writable_dirs: Vec::new(),
            readable_dirs: Vec::new(),
            network: true,
            rules: vec![
                CommandRule {
                    prefix: "git commit".into(),
                    action: RuleAction::Allow,
                },
                CommandRule {
                    prefix: "git".into(),
                    action: RuleAction::Ask,
                },
                CommandRule {
                    prefix: "git push --force".into(),
                    action: RuleAction::Forbid,
                },
            ],
            sandboxed_allow: Vec::new(),
        }
    }

    fn work(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("lynshen-sandbox-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join(".lynshen/agents/sub")).unwrap();
        real(&dir)
    }

    #[test]
    fn the_windows_sandbox_is_off_unless_chosen() {
        assert_eq!(WindowsSandbox::parse("off"), Ok(WindowsSandbox::Off));
        assert_eq!(
            WindowsSandbox::parse("restricted-token"),
            Ok(WindowsSandbox::RestrictedToken)
        );
        assert!(WindowsSandbox::parse("on").is_err());
        // Nothing in this test process turns it on.
        assert_eq!(windows_sandbox(), WindowsSandbox::Off);
        assert!(!windows_sandbox_on());
    }

    #[test]
    fn git_bash_is_refused_by_the_windows_sandbox_in_plain_words() {
        let error = windows_shell_supported(r"C:\Program Files\Git\bin\bash.exe").unwrap_err();
        assert!(error.contains("Git Bash cannot run"), "{error}");
        assert!(windows_shell_supported(r"C:\Program Files\PowerShell\7\pwsh.exe").is_ok());
        assert!(windows_shell_supported("cmd.exe").is_ok());
    }

    #[test]
    fn the_windows_sandbox_grants_the_workspace_and_a_private_temp_only() {
        let dir = work("windows-spec");
        let extra = dir.join("extra");
        fs::create_dir_all(&extra).unwrap();
        let mut policy = policy(SandboxMode::WorkspaceWrite);
        policy.writable_dirs = vec![extra.clone()];
        let spec = policy.windows_spec(&dir, &[]);
        assert_eq!(spec.writable_roots, vec![dir.clone(), extra.clone()]);
        assert!(spec.private_temp);
        // Neither the user's temp directory nor the package caches.
        for root in temp_dirs().iter().chain(cache_dirs().iter()) {
            assert!(!spec.writable_roots.contains(&real(root)), "{root:?}");
        }
        assert!(spec.read_only.contains(&dir.join(".git")));
        assert!(spec.read_only.contains(&dir.join(".lynshen")));
        assert_eq!(spec.deny_read, denied_reads());
        assert!(spec.network);
        // A confined agent: its worktree only, the project around it denied.
        let worktree = dir.join(".lynshen/agents/sub");
        let confined = SandboxPolicy {
            writable_dirs: Vec::new(),
            ..policy.clone()
        }
        .windows_spec(&worktree, std::slice::from_ref(&dir));
        assert_eq!(confined.writable_roots, vec![worktree.clone()]);
        assert_eq!(confined.deny_write, vec![dir.clone()]);
        // Read-only: nothing writable, not even temp.
        let read_only = SandboxPolicy {
            mode: SandboxMode::ReadOnly,
            ..policy
        }
        .windows_spec(&dir, &[]);
        assert!(read_only.writable_roots.is_empty());
        assert!(!read_only.private_temp);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_key_to_desktop_s_secrets_is_a_credential() {
        let denied = denied_reads();
        let home = real(&home().unwrap());
        assert!(denied.contains(&real(&home.join(".lynshen/secret.key"))));
        assert!(denied.contains(&real(&home.join(".lynshen/auth.json"))));
    }

    #[test]
    fn every_platform_starts_with_the_workspace_boundary() {
        let default = SandboxPolicy::default_for_platform();
        assert_eq!(default.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(default.enforced(), os_sandbox());
        assert!(default.prompt().contains("escalate: true"));
    }

    #[test]
    fn the_prompt_tells_the_model_what_it_can_do_in_every_mode() {
        let mut policy = policy(SandboxMode::WorkspaceWrite);
        policy.writable_dirs = vec![PathBuf::from("/srv/shared")];
        let prompt = policy.prompt();
        if os_sandbox() {
            assert!(
                prompt.starts_with("<sandbox mode=\"workspace-write\">"),
                "{prompt}"
            );
            assert!(prompt.contains("/srv/shared"), "{prompt}");
            assert!(prompt.contains("escalate: true"), "{prompt}");
        } else {
            assert!(prompt.contains("enforced=\"false\""), "{prompt}");
            assert!(prompt.contains("No OS sandbox"), "{prompt}");
        }
        let full = SandboxPolicy {
            mode: SandboxMode::FullAccess,
            ..policy.clone()
        }
        .prompt();
        assert!(full.starts_with("<sandbox mode=\"full-access\">"), "{full}");
        assert!(full.contains("No sandbox"), "{full}");
        // Every mode says what file tools may do and what always asks.
        for prompt in [&prompt, &full] {
            assert!(prompt.contains("File tools read anywhere but credentials"));
            assert!(prompt.contains("Always asked, in every mode: sudo, git push"));
            assert!(prompt.ends_with("</sandbox>"));
        }
    }

    #[test]
    fn every_part_of_a_command_line_is_matched_on_its_own() {
        use RuleAction::{Allow, Ask, Forbid};
        let policy = policy(SandboxMode::WorkspaceWrite);
        // Allowed only when every part is; moving around does not count.
        assert_eq!(policy.rule_for("cd app && git commit -m x"), Some(Allow));
        assert_eq!(policy.rule_for("git commit -m x && make"), None);
        // One part that asks or is forbidden decides.
        assert_eq!(policy.rule_for("make && git status"), Some(Ask));
        assert_eq!(
            policy.rule_for("make; git push --force origin | tail"),
            Some(Forbid)
        );
        // An allow rule matches the words as written: only wrappers that
        // do not change the program come off. A variable, `env`, `export`
        // or another path can make `git` anything.
        assert_eq!(policy.rule_for("nohup time git commit -m x"), Some(Allow));
        let only_allow = SandboxPolicy {
            rules: vec![CommandRule {
                prefix: "git commit".into(),
                action: Allow,
            }],
            ..policy.clone()
        };
        for command in [
            "FOO=1 command git commit -m x",
            "PATH=. git commit -m x",
            "GIT_CONFIG_PARAMETERS=\"'core.hooksPath'='/tmp/h'\" git commit -m x",
            "env LD_PRELOAD=/tmp/x.so git commit -m x",
            "/usr/bin/git commit -m x",
            "./git commit -m x",
            "export PATH=.:$PATH && git commit -m x",
            "GIT COMMIT -m x",
        ] {
            assert_eq!(only_allow.rule_for(command), None, "{command}");
            assert_ne!(policy.rule_for(command), Some(Allow), "{command}");
        }
        // Ask and forbid match however it is written.
        assert_eq!(policy.rule_for("/usr/bin/git status"), Some(Ask));
        assert_eq!(policy.rule_for("GIT.EXE STATUS"), Some(Ask));
        assert_eq!(policy.rule_for("FOO=1 git Push --FORCE"), Some(Forbid));
        assert_eq!(policy.rule_for("echo $(git status)"), Some(Ask));
    }

    #[test]
    fn an_ask_rule_beats_an_allow_rule_of_the_same_length() {
        let mut policy = policy(SandboxMode::WorkspaceWrite);
        let rule = |prefix: &str, action| CommandRule {
            prefix: prefix.to_string(),
            action,
        };
        for rules in [
            vec![
                rule("npm publish", RuleAction::Ask),
                rule("npm publish", RuleAction::Allow),
            ],
            vec![
                rule("npm publish", RuleAction::Allow),
                rule("npm publish", RuleAction::Ask),
            ],
        ] {
            policy.rules = rules;
            assert_eq!(policy.rule_for("npm publish"), Some(RuleAction::Ask));
        }
        // An approval never turns an ask or forbid rule into an allow.
        let mut layer = SandboxLayer {
            rules: vec![rule("npm publish", RuleAction::Ask)],
            ..SandboxLayer::default()
        };
        assert!(!layer.allow("npm publish"));
        assert_eq!(layer.rules, vec![rule("npm publish", RuleAction::Ask)]);
        assert!(layer.allow("npm test"));
        let mut saved = json!([{ "prefix": "curl", "action": "forbid" }]);
        assert!(!allow_rule_json(&mut saved, "curl"));
        assert_eq!(saved, json!([{ "prefix": "curl", "action": "forbid" }]));
    }

    #[test]
    fn an_allow_rule_does_not_cover_a_write_outside_or_an_unknown_delete() {
        let dir = work("allow-outside");
        let mut policy = policy(SandboxMode::WorkspaceWrite);
        policy.rules.push(CommandRule {
            prefix: "npm install".into(),
            action: RuleAction::Allow,
        });
        let check = |command: &str| policy.check_command(command, Some((&dir, &dir)));
        assert_eq!(check("npm install").action, Some(RuleAction::Allow));
        assert_eq!(
            check("npm install > log.txt").action,
            Some(RuleAction::Allow)
        );
        let outside = check("npm install > /srv/www/index.html");
        assert_eq!(outside.action, None, "{outside:?}");
        assert!(outside.writes_outside && outside.once_only);
        let copy = check("cp shot.png /srv/www/");
        assert!(copy.writes_outside && copy.once_only, "{copy:?}");
        for command in ["rm -rf $OUT", "rm -rf $(pwd)/..", "ls | xargs rm", "rm"] {
            assert!(check(command).unknown_delete, "{command}");
        }
        assert!(!check("rm -rf build $TMPDIR/x").unknown_delete);
    }

    #[test]
    fn a_session_may_allow_a_prefix_inside_the_sandbox_only() {
        let mut policy = policy(SandboxMode::WorkspaceWrite);
        assert!(!policy.allowed_in_sandbox("cargo test"));
        policy.sandboxed_allow = vec!["cargo test".to_string()];
        assert!(policy.allowed_in_sandbox("cd x && cargo test -q | tail"));
        assert!(!policy.allowed_in_sandbox("cargo test && make"));
        assert!(!policy.allowed_in_sandbox("PATH=. cargo test"));
        assert!(!policy.allowed_in_sandbox("cd x"));
        // It is not an allow rule: leaving the sandbox still asks.
        assert_eq!(policy.rule_for("cargo test"), None);
    }

    #[test]
    fn the_built_in_lists_ask_or_refuse_whatever_the_rules_say() {
        use RuleAction::{Allow, Ask, Forbid};
        let dir = work("built-in");
        let mut policy = policy(SandboxMode::FullAccess);
        policy.rules = ["sudo", "git push", "rm", "launchctl", "cat", "reg"]
            .into_iter()
            .map(|prefix| CommandRule {
                prefix: prefix.to_string(),
                action: Allow,
            })
            .collect();
        let check = |command: &str| policy.check_command(command, Some((&dir, &dir)));
        for command in [
            "sudo make install",
            "doas reboot",
            "Start-Process powershell -Verb RunAs",
            "git push origin main",
            "launchctl load ~/Library/LaunchAgents/x.plist",
            r"reg add HKLM\Software\X /v Y /d 1",
            "Set-ExecutionPolicy Bypass",
            "rm -f /etc/hosts.bak",
            "cat ~/.ssh/id_ed25519",
            "echo key >> ~/.ssh/authorized_keys",
        ] {
            let check = check(command);
            assert_eq!(check.action, Some(Ask), "{command}: {check:?}");
            assert!(check.reason.is_some(), "{command}");
        }
        assert!(check("sudo make install").elevation);
        let parent = dir.parent().unwrap().display().to_string();
        let home = home().unwrap();
        let own = home.file_name().unwrap().to_string_lossy().to_string();
        let above_home = home.parent().unwrap().display().to_string();
        for command in [
            "rm -rf /".to_string(),
            "rm -rf ~".to_string(),
            format!("rm -rf ~{own}"),
            format!("rm -rf {above_home}"),
            "sudo rm -rf $HOME/*".to_string(),
            "rmdir ..".to_string(),
            format!("rm -r {parent}"),
            "bash -c 'rm -rf /'".to_string(),
            "find ~ -name '*.tmp' -delete".to_string(),
            "find / -exec rm -rf {} +".to_string(),
            "ls | ForEach-Object { Remove-Item ~ -Recurse }".to_string(),
        ] {
            let check = check(&command);
            assert_eq!(check.action, Some(Forbid), "{command}: {check:?}");
            assert!(check.reason.unwrap().starts_with("never run"), "{command}");
        }
        // Inside the workspace, temp and the read-write directories a delete
        // is the rules' to decide.
        assert_eq!(check("rm -rf build target").action, Some(Allow));
        assert_eq!(check("rm -f /tmp/x.log").action, Some(Allow));
        // `cat` is neutral: nothing asks, nothing allows.
        assert_eq!(check("cat ~/.lynshen/config.json").action, None);
    }

    #[test]
    fn a_check_names_what_an_approval_may_remember() {
        let dir = work("remember");
        let policy = policy(SandboxMode::WorkspaceWrite);
        let check = |command: &str| policy.check_command(command, Some((&dir, &dir)));
        assert_eq!(
            check("cd web && npm install --save x").prefix.as_deref(),
            Some("npm install")
        );
        assert_eq!(check("make && npm test").prefix, None);
        assert!(!check("npm install").once_only);
        for command in [
            "python3 x.py",
            "node -e 1",
            "rm a.txt",
            "sudo ls",
            "git push -f",
            "pwsh -c ls",
        ] {
            assert!(check(command).once_only, "{command}");
        }
        assert_eq!(
            check("cp out.png /srv/www/").writes,
            vec![PathBuf::from("/srv/www")]
        );
    }

    #[test]
    fn rules_pick_forbid_first_then_the_longest_prefix() {
        let policy = policy(SandboxMode::WorkspaceWrite);
        assert_eq!(policy.rule_for("git commit -m x"), Some(RuleAction::Allow));
        assert_eq!(policy.rule_for("git status"), Some(RuleAction::Ask));
        assert_eq!(
            policy.rule_for("git push --force origin"),
            Some(RuleAction::Forbid)
        );
        assert_eq!(policy.rule_for("gitk"), None);
        assert_eq!(policy.rule_for("ls"), None);
    }

    #[test]
    fn writes_stay_in_writable_roots_and_out_of_protected_paths() {
        let dir = work("writes");
        let policy = policy(SandboxMode::WorkspaceWrite);
        assert!(policy.check_write(&dir, &dir.join("src/new.rs")).is_ok());
        assert!(policy.check_write(&dir, &dir.join(".git/config")).is_err());
        assert!(policy.check_write(&dir, &dir.join(".lynshen/x")).is_err());
        assert!(policy.check_write(&dir, Path::new("/etc/hosts")).is_err());
        // A subagent working inside .lynshen can still write its own worktree.
        let sub = dir.join(".lynshen/agents/sub");
        fs::write(sub.join(".git"), "gitdir: ../../../.git/worktrees/sub\n").unwrap();
        assert!(policy.check_write(&sub, &sub.join("file.txt")).is_ok());
        assert!(policy.check_write(&sub, &sub.join(".git")).is_err());
        let read_only = SandboxPolicy {
            mode: SandboxMode::ReadOnly,
            ..policy.clone()
        };
        assert!(read_only
            .check_write(&dir, &dir.join("src/new.rs"))
            .is_err());
        let full = SandboxPolicy {
            mode: SandboxMode::FullAccess,
            ..policy
        };
        assert!(full.check_write(&dir, Path::new("/etc/hosts")).is_ok());
    }

    #[test]
    fn a_worktree_protects_its_real_git_directory() {
        let dir = work("worktree");
        let real_git = dir.join("main-repo-git");
        fs::create_dir_all(&real_git).unwrap();
        let wt = dir.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", real_git.display())).unwrap();
        let protected = policy(SandboxMode::WorkspaceWrite).protected_paths(&wt);
        assert!(protected.contains(&wt.join(".git")));
        assert!(protected.contains(&real(&real_git)));
    }

    #[test]
    fn bwrap_binds_roots_then_protects_and_hides_secrets() {
        let dir = work("bwrap");
        let mut offline = policy(SandboxMode::WorkspaceWrite);
        offline.network = false;
        let args = offline.bwrap_args(&dir).join(" ");
        let bind = args
            .find(&format!("--bind {0} {0}", dir.display()))
            .unwrap();
        let protect = args
            .find(&format!(
                "--ro-bind-try {0} {0}",
                dir.join(".git").display()
            ))
            .unwrap();
        assert!(bind < protect, "{args}");
        assert!(args.starts_with("--ro-bind / /"));
        assert!(args.contains("--unshare-net"));
        // A protected path that does not exist is skipped, not an error that
        // would fail every command.
        let mut missing = offline.clone();
        missing.readable_dirs = vec![dir.join("not-created-yet")];
        let args = missing.bwrap_args(&dir).join(" ");
        assert!(
            args.contains(&format!(
                "--ro-bind-try {0} {0}",
                dir.join("not-created-yet").display()
            )),
            "{args}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_os_sandbox_enforces_the_policy() {
        use std::process::Command;
        let dir = work("seatbelt");
        let outside = real(&env::temp_dir())
            .parent()
            .unwrap()
            .join(format!("lynshen-sbx-outside-{}", std::process::id()));
        let _ = fs::remove_file(&outside);
        let policy = policy(SandboxMode::WorkspaceWrite);
        let run = |script: &str| {
            let (program, args) = policy.wrap("/bin/sh", &["-c", script], &dir).unwrap();
            Command::new(program)
                .args(args)
                .current_dir(&dir)
                .status()
                .unwrap()
                .success()
        };
        assert!(run("echo ok > inside.txt"));
        assert!(!run("echo no > .git/config"));
        assert!(!run(&format!("echo no > {}", outside.display())));
        assert!(!outside.exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_confined_agent_writes_only_its_worktree() {
        use std::process::Command;
        let project = work("confined");
        let worktree = project.join(".lynshen/agents/sub");
        fs::write(
            worktree.join(".git"),
            "gitdir: ../../../.git/worktrees/sub\n",
        )
        .unwrap();
        let extra =
            real(&env::temp_dir()).join(format!("lynshen-sbx-extra-{}", std::process::id()));
        fs::create_dir_all(&extra).unwrap();
        let mut policy = policy(SandboxMode::WorkspaceWrite);
        policy.writable_dirs = vec![extra.clone()];
        let run = |script: &str| {
            let (program, args) = policy
                .wrap_confined("/bin/sh", &["-c", script], &worktree, &project, &worktree)
                .unwrap();
            Command::new(program)
                .args(args)
                .current_dir(&worktree)
                .status()
                .unwrap()
                .success()
        };
        assert!(run("echo ok > inside.txt"));
        assert!(worktree.join("inside.txt").exists());
        // The project around the worktree, its git file and the parent's
        // read-write directories are out of reach, though all sit in temp.
        assert!(!run(&format!(
            "echo no > {}",
            project.join("escape.txt").display()
        )));
        assert!(!project.join("escape.txt").exists());
        assert!(!run("echo no > .git"));
        assert!(!run(&format!(
            "echo no > {}",
            extra.join("x.txt").display()
        )));
        assert!(!extra.join("x.txt").exists());
        // Unconfined, the same policy writes both.
        let (program, args) = policy
            .wrap(
                "/bin/sh",
                &[
                    "-c",
                    &format!("echo yes > {}", extra.join("y.txt").display()),
                ],
                &project,
            )
            .unwrap();
        assert!(Command::new(program).args(args).status().unwrap().success());
        let _ = fs::remove_dir_all(&extra);
        let _ = fs::remove_dir_all(&project);
    }
}

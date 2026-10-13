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
//! (`sandbox-exec`), Linux `bwrap`; Windows supports `full-access` only.
//! File tools run in-process and check writes against the same rules.

use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

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
}

impl SandboxPolicy {
    /// Whether commands run inside an OS sandbox.
    pub fn is_sandboxed(&self) -> bool {
        self.mode != SandboxMode::FullAccess
    }

    /// The rule that applies to `command`: `forbid` wins over any other
    /// match, otherwise the longest matching prefix.
    pub fn rule_for(&self, command: &str) -> Option<RuleAction> {
        let words: Vec<&str> = command.split_whitespace().collect();
        let matches: Vec<&CommandRule> = self
            .rules
            .iter()
            .filter(|rule| {
                let prefix: Vec<&str> = rule.prefix.split_whitespace().collect();
                !prefix.is_empty() && words.starts_with(&prefix)
            })
            .collect();
        if matches.iter().any(|rule| rule.action == RuleAction::Forbid) {
            return Some(RuleAction::Forbid);
        }
        matches
            .into_iter()
            .max_by_key(|rule| rule.prefix.split_whitespace().count())
            .map(|rule| rule.action)
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
        let roots = dirs.map(|(_, workspace)| self.boundary_roots(workspace));
        let workspace = dirs.map(|(_, workspace)| real(workspace));
        let mut check = CommandCheck::default();
        let mut forbid: Option<String> = None;
        let mut ask: Option<String> = None;
        let mut all_allowed = true;
        let mut counted = 0;
        let mut prefixes: Vec<String> = Vec::new();
        let parts = text::segments(command);
        for part in &parts {
            let user = self.user_rule(part);
            let mut part_forbid = (user == Some(RuleAction::Forbid))
                .then(|| "it matches a forbid command rule".to_string());
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
                for target in text::delete_targets(part).iter().filter_map(|w| resolve(w)) {
                    let real_target = real_or_parent(&target);
                    let protected = target.parent().is_none()
                        || home
                            .as_ref()
                            .is_some_and(|home| real_target == *home || target == *home)
                        || workspace.as_ref().is_some_and(|workspace| {
                            workspace != &real_target && workspace.starts_with(&real_target)
                        });
                    if protected {
                        part_forbid.get_or_insert_with(|| {
                            format!(
                                "it deletes {}: the root, the home directory or a parent of the workspace",
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
                let mentioned = part
                    .raw
                    .iter()
                    .chain(&part.redirects)
                    .filter(|word| text::looks_like_path(word))
                    .filter_map(|word| resolve(word));
                for path in mentioned {
                    let real_path = real_or_parent(&path);
                    if credentials
                        .iter()
                        .any(|secret| real_path.starts_with(secret) || path.starts_with(secret))
                    {
                        part_ask.get_or_insert_with(|| {
                            format!("it touches credentials ({})", path.display())
                        });
                    }
                }
                for target in text::write_targets(part).iter().filter_map(|w| resolve(w)) {
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
                if let Some(prefix) = text::prefix(part) {
                    if !prefixes.contains(&prefix) {
                        prefixes.push(prefix);
                    }
                }
            }
        }
        if counted == 0 {
            prefixes.extend(parts.first().and_then(text::prefix));
        }
        check.prefix = (prefixes.len() == 1).then(|| prefixes.remove(0));
        (check.action, check.reason) = match (forbid, ask) {
            (Some(reason), _) => (Some(RuleAction::Forbid), Some(reason)),
            (None, Some(reason)) => (Some(RuleAction::Ask), Some(reason)),
            _ if counted > 0 && all_allowed => (Some(RuleAction::Allow), None),
            _ => (None, None),
        };
        check
    }

    /// The command rule for one part of a command line: `forbid` wins over
    /// any other match, otherwise the longest matching prefix. A rule matches
    /// the part as written, without its wrappers, or with the program named
    /// by its file name (`/usr/bin/git push` matches `git push`).
    fn user_rule(&self, part: &crate::command_text::Segment) -> Option<RuleAction> {
        let mut named = part.words.clone();
        if let Some(first) = named.first_mut() {
            *first = crate::command_text::program_name(first);
        }
        let variants = [&part.raw, &part.words, &named];
        let matches: Vec<&CommandRule> = self
            .rules
            .iter()
            .filter(|rule| {
                let prefix: Vec<&str> = rule.prefix.split_whitespace().collect();
                !prefix.is_empty()
                    && variants.iter().any(|words| {
                        words.len() >= prefix.len()
                            && words.iter().zip(&prefix).all(|(word, want)| word == want)
                    })
            })
            .collect();
        if matches.iter().any(|rule| rule.action == RuleAction::Forbid) {
            return Some(RuleAction::Forbid);
        }
        matches
            .into_iter()
            .max_by_key(|rule| rule.prefix.split_whitespace().count())
            .map(|rule| rule.action)
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

    /// Directories outside the working directory that the file *read* tools
    /// may read while this sandbox is active: everywhere this agent's own
    /// shell commands may write (the read-write directories, temp and the
    /// package caches), plus the read-only directories. A screenshot or log
    /// a command just wrote to /tmp is readable; nothing becomes writable.
    /// Empty of writable roots under `read-only`, where no command writes.
    pub fn tool_read_roots(&self, cwd: &Path) -> Vec<PathBuf> {
        let mut roots = self.writable_roots(cwd);
        roots.extend(self.readable_dirs.iter().map(|dir| real(dir)));
        roots.extend(self.writable_dirs.iter().map(|dir| real(dir)));
        roots.sort();
        roots.dedup();
        roots
    }

    /// Whether `path` lies in one of the extra read-write directories.
    pub fn in_writable_dir(&self, path: &Path) -> bool {
        let target = real_or_parent(path);
        self.writable_dirs
            .iter()
            .any(|dir| target.starts_with(real(dir)))
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
        } else {
            Err("the sandbox is not supported on this platform; set the sandbox to full-access or run under WSL2".to_string())
        }
    }

    /// Errors when this platform cannot run the sandbox, so a host can refuse
    /// to start instead of failing on the first command.
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
            find_bwrap().map(|_| ()).ok_or_else(bwrap_missing)
        } else {
            Err("the sandbox is not supported on this platform; set the sandbox to full-access or run under WSL2".to_string())
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

    /// A paragraph for the system prompt describing the sandbox.
    pub fn prompt(&self) -> String {
        if !self.is_sandboxed() {
            return String::new();
        }
        let writable = match self.mode {
            SandboxMode::ReadOnly => "Nothing is writable.".to_string(),
            _ => format!(
                "Writable: the working directory{}, temp and package-cache directories; .git, .lynshen and .agents inside them are read-only.",
                if self.writable_dirs.is_empty() {
                    String::new()
                } else {
                    format!(
                        ", {}",
                        self.writable_dirs
                            .iter()
                            .map(|dir| dir.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            ),
        };
        let readable = if self.readable_dirs.is_empty() {
            String::new()
        } else {
            format!(
                " Also readable with file tools: {}.",
                self.readable_dirs
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            "<sandbox mode=\"{}\">\nShell commands run in a sandbox. {writable}{readable} Network is {}. Credentials such as ~/.ssh are unreadable. A command that needs more (committing to git, writing elsewhere{}) runs outside the sandbox with `escalate: true` and a one-line `justification`, once approved.\n</sandbox>",
            self.mode.as_str(),
            if self.network { "on" } else { "off" },
            if self.network { "" } else { ", network access" },
        )
    }
}

impl SandboxPolicy {
    /// The default: the working directory writable, network on, commits
    /// allowed to leave the sandbox and pushes always asked. Windows has no
    /// sandbox yet and starts at `full-access`.
    pub fn default_for_platform() -> Self {
        Self {
            mode: if cfg!(windows) {
                SandboxMode::FullAccess
            } else {
                SandboxMode::WorkspaceWrite
            },
            writable_dirs: Vec::new(),
            readable_dirs: Vec::new(),
            network: true,
            rules: rules_from_json(&default_rules_json()).expect("default rules parse"),
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

/// Credentials no sandboxed command, and no remote client, may read.
pub fn denied_reads() -> Vec<PathBuf> {
    let Some(home) = home() else {
        return Vec::new();
    };
    [
        ".ssh",
        ".gnupg",
        ".aws",
        ".lynshen/auth.json",
        ".lynshen/daemon",
    ]
    .iter()
    .map(|path| real(&home.join(path)))
    .collect()
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

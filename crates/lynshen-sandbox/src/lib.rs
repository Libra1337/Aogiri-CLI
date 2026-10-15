//! OS sandbox backends that LynShen drives itself, without a helper such as
//! `sandbox-exec` or `bwrap`:
//!
//! - Windows (`windows`): a restricted, low-integrity token inside a job
//!   object. Writable roots are granted to per-session capability SIDs by
//!   ACL; credential paths get a no-read-up label. No administrator rights.
//! - Linux (`linux`): Landlock (file system, TCP) and seccomp (sockets),
//!   applied by the runner to itself before it executes the command. Needs
//!   no user namespaces, so it works where AppArmor blocks bubblewrap.
//!
//! A backend turns a command into a command line that starts the runner
//! (the LynShen binary with `RUNNER_ARG`), so callers spawn it like any
//! other process. The engine's policy (`agent-core/src/sandbox.rs`) stays
//! the contract: it resolves a `Spec` per command.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

// The seccomp filter knows these two architectures' system call numbers.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub mod linux;
#[cfg(windows)]
pub mod windows;

/// The first argument that makes the LynShen binary run as the sandbox
/// runner: `lynshen __sandbox-exec <backend> <json> -- <program> <args>…`.
pub const RUNNER_ARG: &str = "__sandbox-exec";

/// What one sandboxed command may touch. Paths are absolute, with symlinks
/// resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    /// Directories the command may write, recursively.
    pub writable_roots: Vec<PathBuf>,
    /// Paths inside the writable roots that stay read-only (`.git`,
    /// `.lynshen`, `.agents`, read-only directories).
    pub read_only: Vec<PathBuf>,
    /// Directories that are read-only for this command although a writable
    /// root contains them (a confined agent's project), except where a
    /// writable root lies inside them (its worktree).
    pub deny_write: Vec<PathBuf>,
    /// Paths the command may not read (credentials).
    pub deny_read: Vec<PathBuf>,
    /// Whether the command may use the network.
    pub network: bool,
    /// Gives the command a writable temp directory of its own (`TEMP` and
    /// `TMP`), removed with the session, instead of the user's. Windows
    /// only: granting the user's temp directory would walk all of it and
    /// open it to every low-integrity process.
    pub private_temp: bool,
}

/// One platform's sandbox.
pub trait Backend {
    /// The name `RUNNER_ARG` takes.
    fn name(&self) -> &'static str;
    /// Errors when the backend cannot run here, saying how to fix it.
    fn check(&self) -> Result<(), String>;
    /// Whether `Spec::network == false` keeps a command off the network.
    fn enforces_network(&self) -> bool;
    /// Whether `Spec::deny_read` is enforced.
    fn enforces_deny_read(&self) -> bool;
    /// The command line that runs `program args` under `spec`; `runner` is
    /// the executable that handles `RUNNER_ARG`.
    fn wrap(
        &self,
        spec: &Spec,
        runner: &Path,
        program: &str,
        args: &[&str],
    ) -> Result<(String, Vec<String>), String>;
}

/// The runner's command line: `runner RUNNER_ARG backend json -- program args`.
#[cfg_attr(
    not(any(
        windows,
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )),
    allow(dead_code)
)]
pub(crate) fn runner_command(
    runner: &Path,
    backend: &str,
    json: String,
    program: &str,
    args: &[&str],
) -> (String, Vec<String>) {
    let mut wrapped = vec![
        RUNNER_ARG.to_string(),
        backend.to_string(),
        json,
        "--".to_string(),
        program.to_string(),
    ];
    wrapped.extend(args.iter().map(|arg| arg.to_string()));
    (runner.display().to_string(), wrapped)
}

/// The runner: `args` are the arguments after `RUNNER_ARG`. Sets up the
/// sandbox, runs the command in it and returns its exit code. Errors are
/// printed to stderr and exit with 126 (cannot run), like a shell.
pub fn runner_main(args: &[OsString]) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("lynshen sandbox: {error}");
            126
        }
    }
}

fn run(args: &[OsString]) -> Result<i32, String> {
    let text = |index: usize| {
        args.get(index).and_then(|arg| arg.to_str()).ok_or_else(|| {
            "usage: __sandbox-exec <backend> <json> -- <program> [args…]".to_string()
        })
    };
    let backend = text(0)?;
    let config: serde_json::Value =
        serde_json::from_str(text(1)?).map_err(|error| format!("bad sandbox settings: {error}"))?;
    if text(2)? != "--" || args.len() < 4 {
        return Err("missing `-- <program>`".to_string());
    }
    let command = &args[3..];
    match backend {
        #[cfg(windows)]
        "windows" => windows::run_command(&config, command),
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        "landlock" => linux::exec_command(&config, command),
        other => {
            let _ = (config, command);
            Err(format!(
                "sandbox backend {other} is not available on this platform"
            ))
        }
    }
}

/// Whether `path` is `root` or lies under it.
#[cfg_attr(
    not(any(
        windows,
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )),
    allow(dead_code)
)]
pub(crate) fn within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// Whether one of `a` and `b` contains the other.
#[cfg_attr(
    not(any(
        windows,
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )),
    allow(dead_code)
)]
pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    within(a, b) || within(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_command_carries_the_backend_settings_and_the_command() {
        let (program, args) = runner_command(
            Path::new("/bin/lynshen"),
            "landlock",
            "{}".to_string(),
            "/bin/sh",
            &["-c", "echo hi"],
        );
        assert_eq!(program, "/bin/lynshen");
        assert_eq!(
            args,
            vec![RUNNER_ARG, "landlock", "{}", "--", "/bin/sh", "-c", "echo hi"]
        );
    }

    #[test]
    fn a_bad_runner_command_exits_126() {
        assert_eq!(runner_main(&[]), 126);
        let args: Vec<OsString> = ["nope", "{}", "--", "x"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(runner_main(&args), 126);
    }
}

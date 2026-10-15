//! The Windows sandbox, `restricted-token` mode: no administrator rights,
//! Windows 10 1607 and later.
//!
//! - Writes: the token is write-restricted (`token`). Each writable root
//!   is granted to its own random capability SID by an inheritable ACE,
//!   and the token lists the SIDs of the command's roots. Protected paths
//!   (`.git`, …) get a deny-write ACE for those SIDs.
//! - The integrity level is Low, so writes also need a Low label: the
//!   writable roots get one, in the same pass as their ACE.
//! - Reads: credential paths get a Medium no-read-up label, so a Low
//!   process cannot read them; the user's own programs (Medium) still can.
//! - Processes: the command runs in a job that is killed when the runner
//!   ends, and at Low integrity it cannot open the user's other processes
//!   or send input to their windows.
//! - Temp: with `Spec::private_temp` the command gets a temp directory of
//!   its own (`TEMP`, `TMP`), removed with the session.
//! - Network: not restricted (that needs a firewall rule, so administrator
//!   rights, or an AppContainer).
//!
//! Every ACE and label is taken off again when the engine exits
//! (`cleanup`), or by the next engine if it crashed (`session`).

mod acl;
mod process;
mod session;
mod sid;
mod token;

#[cfg(test)]
mod tests;

pub use acl::{explicit_ace, explicit_label, Label};
pub use process::{spawn, Sandboxed};
pub use session::{cleanup, last_prepare_stats, root_sid, Grant, PrepareStats};
pub use sid::Sid;
pub use token::sandbox_token;
#[cfg(test)]
use token::Variant;

use std::{
    ffi::OsString,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use crate::{runner_command, Backend, Spec};

/// The `restricted-token` backend. `state_dir` holds the ledgers of ACL
/// changes (`session`); `low` runs commands at Low integrity, which is
/// what makes credential paths unreadable (off only to compare tools'
/// behaviour).
#[derive(Debug, Clone)]
pub struct RestrictedToken {
    pub state_dir: PathBuf,
    pub low: bool,
}

impl RestrictedToken {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            low: true,
        }
    }

    /// Grants what `spec` needs and returns the SIDs the command's token
    /// lists (the runner gets them on its command line) and its temp
    /// directory.
    pub fn prepare(&self, spec: &Spec) -> Result<Grant, String> {
        session::prepare(&self.state_dir, spec, self.low)
    }
}

impl Backend for RestrictedToken {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn check(&self) -> Result<(), String> {
        static PROBE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        PROBE
            .get_or_init(|| sandbox_token(&[Sid::random()], true).map(drop))
            .clone()
    }

    fn enforces_network(&self) -> bool {
        false
    }

    fn enforces_deny_read(&self) -> bool {
        self.low
    }

    fn wrap(
        &self,
        spec: &Spec,
        runner: &Path,
        program: &str,
        args: &[&str],
    ) -> Result<(String, Vec<String>), String> {
        let grant = self.prepare(spec)?;
        let settings = serde_json::json!({
            "caps": grant.caps.iter().map(Sid::to_string_sid).collect::<Vec<_>>(),
            "low": self.low,
            "temp": grant.temp.map(|temp| temp.display().to_string()),
        });
        Ok(runner_command(
            runner,
            self.name(),
            settings.to_string(),
            program,
            args,
        ))
    }
}

/// The runner: starts `command` with the sandbox token in a job, with this
/// process's input, output, directory and environment (`TEMP` and `TMP`
/// pointing at its private temp directory, when it has one), and returns
/// its exit code. Whatever the command leaves running ends with the job
/// when the runner returns.
pub(crate) fn run_command(
    settings: &serde_json::Value,
    command: &[OsString],
) -> Result<i32, String> {
    if let Some(temp) = settings["temp"].as_str() {
        // The runner is single-threaded here; the child inherits it.
        std::env::set_var("TEMP", temp);
        std::env::set_var("TMP", temp);
    }
    let caps = settings["caps"]
        .as_array()
        .ok_or("missing caps")?
        .iter()
        .map(|sid| Sid::parse(sid.as_str().unwrap_or_default()))
        .collect::<Result<Vec<_>, _>>()?;
    let low = settings["low"].as_bool().unwrap_or(true);
    let token = sandbox_token(&caps, low)?;
    let child = spawn(&token, &command[0], &command[1..], None, None)?;
    let code = child.wait(None)?.unwrap_or(1);
    Ok(code as i32)
}

/// `value` as a NUL-terminated UTF-16 string.
pub(crate) fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

//! What a host's child process starts goes with it. A daemon runs each
//! engine (Claude Code, Codex, an ACP agent) as a child, and the engine
//! starts its own children (MCP servers, shells); when the engine stops, or
//! the daemon exits, none of them may be left running.
//!
//! Unix: the child leads its own process group, which is signalled as a
//! whole, and the daemon ends the groups it tracks when it exits
//! (`terminate_tool_processes`). Windows has no process groups that can be
//! signalled: the child goes into a job object that ends every process in it
//! when told to, or when its last handle closes (the daemon exiting, however
//! it exits). A process the child starts before it joins the job (the few
//! microseconds after it starts) is not in it.

use std::process::{Child, Command};

/// Sets `command` up so its process can lead a group (Unix: a new process
/// group). Call before spawning, then [`ProcessGroup::adopt`] the child.
pub fn prepare(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// A child and everything it starts.
pub struct ProcessGroup {
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ProcessGroup {
    /// The group `child` (spawned after [`prepare`]) leads. None when the
    /// system refuses (Windows: the job could not be made or joined); the
    /// child then runs as before, ending alone.
    pub fn adopt(child: &Child) -> Option<Self> {
        #[cfg(unix)]
        {
            crate::tools::track_tool_group(child.id());
            Some(Self { pid: child.id() })
        }
        #[cfg(windows)]
        {
            windows::adopt(child).map(|job| Self { job })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            None
        }
    }

    /// Ends the processes the child started and left running. `now`: at once
    /// (Unix SIGKILL), else asking them to stop (SIGTERM). Windows ends them
    /// at once either way.
    pub fn end(&self, now: bool) {
        #[cfg(unix)]
        {
            let signal = if now { libc::SIGKILL } else { libc::SIGTERM };
            // SAFETY: kill(2) on the group this process created for the
            // child. The group id cannot name another group while any
            // process of this one lives.
            unsafe { libc::killpg(self.pid as libc::pid_t, signal) };
        }
        #[cfg(windows)]
        {
            let _ = now;
            windows::terminate(&self.job);
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::Child;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A job holding `child`, whose processes end when the job's last
    /// handle closes.
    pub(super) fn adopt(child: &Child) -> Option<OwnedHandle> {
        // SAFETY: a new unnamed job with default security; the handle is
        // owned from here on.
        let job = unsafe {
            let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if raw.is_null() {
                return None;
            }
            OwnedHandle::from_raw_handle(raw)
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the job handle is open and the struct is the size given.
        let set = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        // SAFETY: both handles are open; the child's is owned by `child`.
        let joined = set != 0
            && unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } != 0;
        joined.then_some(job)
    }

    pub(super) fn terminate(job: &OwnedHandle) {
        // SAFETY: the job handle is open.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Until `done` or 10 seconds.
    fn eventually(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn ending_the_group_ends_what_the_child_started() {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-process-group-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("grandchild.pid");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("sleep 60 & echo $! > '{}'; wait", pid_file.display()),
        ]);
        prepare(&mut command);
        let mut child = command.spawn().unwrap();
        let group = ProcessGroup::adopt(&child).unwrap();
        assert!(eventually(
            || std::fs::read_to_string(&pid_file).is_ok_and(|text| text.ends_with('\n'))
        ));
        let grandchild: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The child itself is the group's leader, not this test's group.
        // SAFETY: getpgid(2) reads a process's group.
        assert_eq!(
            unsafe { libc::getpgid(child.id() as libc::pid_t) },
            child.id() as libc::pid_t
        );

        group.end(false);
        let _ = child.wait();
        // SAFETY: kill(2) with signal 0 only asks whether the process exists.
        assert!(
            eventually(|| unsafe { libc::kill(grandchild, 0) } != 0),
            "the grandchild outlived its group"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn ending_the_job_ends_what_the_child_started() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };
        // Processes in the job now.
        let active = |group: &ProcessGroup| {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: the job handle is open and the struct is the size given.
            let ok = unsafe {
                QueryInformationJobObject(
                    group.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "QueryInformationJobObject failed");
            info.ActiveProcesses
        };
        // cmd starts ping, which keeps running for a minute.
        let mut command = Command::new("cmd");
        command
            .args(["/d", "/c", "ping", "-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null());
        prepare(&mut command);
        let mut child = command.spawn().unwrap();
        let group = ProcessGroup::adopt(&child).expect("a job for the child");
        assert!(
            eventually(|| active(&group) >= 2),
            "the grandchild did not join the job"
        );

        group.end(true);
        let _ = child.wait();
        assert!(
            eventually(|| active(&group) == 0),
            "a process of the job outlived it"
        );
    }
}

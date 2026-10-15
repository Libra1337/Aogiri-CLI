// Portions adapted from openai/codex, codex-rs/windows-sandbox-rs/src/process.rs
// (Apache-2.0, Copyright 2025 OpenAI). See NOTICE.

//! Starting a command with the sandbox token, inside a job object.
//!
//! The job is created with `KILL_ON_JOB_CLOSE`: when its last handle closes
//! (the runner exits or is killed, for example on a timeout), Windows ends
//! every process in it, including ones whose parent has already exited,
//! which a process-tree kill (`taskkill /T`) cannot find. A child cannot
//! leave the job (no breakaway). UI limits keep it off the clipboard, the
//! desktop switch and system settings.

use std::{
    ffi::{OsStr, OsString},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    System::{
        Console::{
            GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
            JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
            JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS,
            JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_READCLIPBOARD,
            JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        },
        StationsAndDesktops::{
            GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
        },
        Threading::{
            CreateProcessAsUserW, GetCurrentThreadId, GetExitCodeProcess, ResumeThread,
            TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, INFINITE,
            PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
        },
    },
};

use super::sid::last_error;

/// A command running in the sandbox, with its job.
pub struct Sandboxed {
    process: OwnedHandle,
    job: OwnedHandle,
    pub pid: u32,
}

impl Sandboxed {
    /// Waits up to `timeout` (forever with None); the exit code once it
    /// has exited.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<Option<u32>, String> {
        let millis = timeout.map_or(INFINITE, |timeout| {
            timeout.as_millis().min(u128::from(INFINITE - 1)) as u32
        });
        // SAFETY: the process handle is owned and open.
        unsafe {
            match WaitForSingleObject(self.process.as_raw_handle(), millis) {
                WAIT_OBJECT_0 => {
                    let mut code = 0u32;
                    if GetExitCodeProcess(self.process.as_raw_handle(), &mut code) == 0 {
                        return Err(format!("GetExitCodeProcess failed: {}", last_error()));
                    }
                    Ok(Some(code))
                }
                WAIT_TIMEOUT => Ok(None),
                _ => Err(format!("waiting for the command failed: {}", last_error())),
            }
        }
    }

    /// Ends every process in the job.
    pub fn kill(&self) {
        // SAFETY: the job handle is owned and open.
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), 1);
        }
    }
}

/// Starts `program args` with `token` in a new job, in `cwd` (None: this
/// process's working directory) and this process's environment. `stdio`
/// are the handles for its input, output and errors; None passes this
/// process's own.
pub fn spawn(
    token: &OwnedHandle,
    program: &OsStr,
    args: &[OsString],
    cwd: Option<&Path>,
    stdio: Option<[HANDLE; 3]>,
) -> Result<Sandboxed, String> {
    let application = resolve_program(program)?;
    let mut command_line = quote_arg(program);
    for arg in args {
        command_line.push(' ' as u16);
        command_line.extend(quote_arg(arg));
    }
    command_line.push(0);
    let application_wide = wide_os(application.as_os_str());
    let cwd_wide = cwd.map(|cwd| wide_os(cwd.as_os_str()));
    // SAFETY: every pointer passed to Windows points at a buffer that
    // outlives the call; handles are owned as soon as they are returned.
    unsafe {
        let job = new_job()?;
        let console = std_handle_is_console();
        let handles = match stdio {
            Some(handles) => handles,
            None => [
                GetStdHandle(STD_INPUT_HANDLE),
                GetStdHandle(STD_OUTPUT_HANDLE),
                GetStdHandle(STD_ERROR_HANDLE),
            ],
        };
        for handle in handles {
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
        let mut desktop = current_desktop();
        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = handles[0];
        startup.hStdOutput = handles[1];
        startup.hStdError = handles[2];
        // PowerShell can fail to start (STATUS_DLL_INIT_FAILED) under a
        // restricted token unless the desktop is named explicitly (Codex).
        if let Some(desktop) = desktop.as_mut() {
            startup.lpDesktop = desktop.as_mut_ptr();
        }
        // Output redirected (the engine's case): no console window.
        let flags = CREATE_SUSPENDED | if console { 0 } else { CREATE_NO_WINDOW };
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessAsUserW(
            token.as_raw_handle(),
            application_wide.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            flags,
            ptr::null(),
            cwd_wide.as_ref().map_or(ptr::null(), |cwd| cwd.as_ptr()),
            &startup,
            &mut info,
        ) == 0
        {
            return Err(format!(
                "starting {} in the sandbox failed: {}",
                application.display(),
                last_error()
            ));
        }
        let process = OwnedHandle::from_raw_handle(info.hProcess);
        let thread = OwnedHandle::from_raw_handle(info.hThread);
        if AssignProcessToJobObject(job.as_raw_handle(), info.hProcess) == 0 {
            let error = last_error();
            TerminateProcess(info.hProcess, 1);
            return Err(format!("putting the command in a job failed: {error}"));
        }
        ResumeThread(thread.as_raw_handle());
        Ok(Sandboxed {
            process,
            job,
            pid: info.dwProcessId,
        })
    }
}

unsafe fn new_job() -> Result<OwnedHandle, String> {
    let raw = CreateJobObjectW(ptr::null(), ptr::null());
    if raw.is_null() {
        return Err(format!("CreateJobObjectW failed: {}", last_error()));
    }
    let job = OwnedHandle::from_raw_handle(raw);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if SetInformationJobObject(
        raw,
        JobObjectExtendedLimitInformation,
        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    ) == 0
    {
        return Err(format!("setting the job's limits failed: {}", last_error()));
    }
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
    };
    if SetInformationJobObject(
        raw,
        JobObjectBasicUIRestrictions,
        (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
        std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
    ) == 0
    {
        return Err(format!(
            "setting the job's UI limits failed: {}",
            last_error()
        ));
    }
    Ok(job)
}

/// `window station\desktop` of this thread, NUL-terminated.
unsafe fn current_desktop() -> Option<Vec<u16>> {
    let name = |object: HANDLE| -> Option<Vec<u16>> {
        let mut buffer = vec![0u16; 256];
        let mut needed = 0u32;
        if object.is_null()
            || GetUserObjectInformationW(
                object,
                UOI_NAME,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 2) as u32,
                &mut needed,
            ) == 0
        {
            return None;
        }
        let end = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        buffer.truncate(end);
        Some(buffer)
    };
    let mut full = name(GetProcessWindowStation())?;
    full.push('\\' as u16);
    full.extend(name(GetThreadDesktop(GetCurrentThreadId()))?);
    full.push(0);
    Some(full)
}

unsafe fn std_handle_is_console() -> bool {
    let mut mode = 0;
    GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut mode) != 0
}

/// The program to start: a path as given, a bare name searched on `PATH`
/// (with `.exe` added when it has no extension), never in the working
/// directory, as Rust's `Command` does.
fn resolve_program(program: &OsStr) -> Result<PathBuf, String> {
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let names: Vec<PathBuf> = if path.extension().is_some() {
        vec![path.to_path_buf()]
    } else {
        vec![path.with_extension("exe"), path.to_path_buf()]
    };
    let dirs = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&dirs)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| format!("{} not found on PATH", path.display()))
}

/// One argument as the C runtime's command-line parser reads it back.
pub(crate) fn quote_arg(arg: &OsStr) -> Vec<u16> {
    let units: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|&unit| matches!(unit, 0x20 | 0x09 | 0x0a | 0x0b | 0x22));
    if !needs_quotes {
        return units;
    }
    let mut quoted = vec!['"' as u16];
    let mut backslashes = 0usize;
    for &unit in &units {
        if unit == '\\' as u16 {
            backslashes += 1;
            continue;
        }
        if unit == '"' as u16 {
            // Backslashes before a quote are doubled, and the quote escaped.
            quoted.extend(std::iter::repeat_n('\\' as u16, backslashes * 2 + 1));
        } else {
            quoted.extend(std::iter::repeat_n('\\' as u16, backslashes));
        }
        backslashes = 0;
        quoted.push(unit);
    }
    // Backslashes before the closing quote are doubled.
    quoted.extend(std::iter::repeat_n('\\' as u16, backslashes * 2));
    quoted.push('"' as u16);
    quoted
}

fn wide_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(arg: &str) -> String {
        String::from_utf16(&quote_arg(OsStr::new(arg))).unwrap()
    }

    #[test]
    fn arguments_round_trip_through_the_c_runtime_rules() {
        assert_eq!(quoted("plain"), "plain");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("two words"), "\"two words\"");
        assert_eq!(quoted(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quoted(r"C:\dir\"), r"C:\dir\");
        assert_eq!(quoted(r"C:\my dir\"), r#""C:\my dir\\""#);
        assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
    }
}

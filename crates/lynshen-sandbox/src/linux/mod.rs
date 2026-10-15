//! The Linux sandbox without user namespaces: Landlock and seccomp.
//!
//! Where bubblewrap cannot run (not installed, or AppArmor on Ubuntu 24.04
//! and later blocking the user namespaces it needs), the runner restricts
//! itself and then executes the command, which inherits the restrictions:
//!
//! - Landlock (kernel 5.13+) on the file system: the writable roots get
//!   every write right; everything else is readable but not writable.
//!   Landlock only grants, it cannot deny below a grant, so the readable
//!   set is "/" with the credential paths cut out: a directory on the way
//!   to one is listed child by child (`read_rules`).
//! - With the network off: Landlock (kernel 6.7+) refuses TCP bind and
//!   connect, and a seccomp filter refuses IPv4 and IPv6 sockets of any
//!   kind (UDP too) and io_uring, which could open sockets around it.
//!
//! What it cannot do, unlike bubblewrap: keep `.git` (or any path) inside
//! a writable root read-only, hide a credential inside a writable root,
//! hide the names of the files in a credential directory (only their
//! contents: the directories on the way stay listable, and listing is
//! granted to everything below them), or give the command a private
//! loopback interface (with the network off, localhost is unreachable too).

use std::{
    ffi::{CString, OsString},
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    path::{Path, PathBuf},
};

use crate::{runner_command, within, Backend, Spec};

// Landlock ABI (include/uapi/linux/landlock.h).
const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: u32 = 1;
const ACCESS_FS_EXECUTE: u64 = 1 << 0;
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_READ_FILE: u64 = 1 << 2;
const ACCESS_FS_READ_DIR: u64 = 1 << 3;
const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
/// ABI 2 (5.19): moving or linking a file to another directory.
const ACCESS_FS_REFER: u64 = 1 << 13;
/// ABI 3 (6.2): truncating a file.
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
/// ABI 4 (6.7).
const ACCESS_NET_BIND_TCP: u64 = 1 << 0;
const ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;

const READ: u64 = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
/// Rights that make sense on a file, not only a directory.
const FILE_RIGHTS: u64 =
    ACCESS_FS_EXECUTE | ACCESS_FS_WRITE_FILE | ACCESS_FS_READ_FILE | ACCESS_FS_TRUNCATE;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI version this kernel offers; 0 without Landlock (too
/// old, or not among the enabled LSMs).
pub fn abi() -> i32 {
    // SAFETY: a query with no attribute, as the ABI documents.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    version.max(0) as i32
}

/// Every file-system right this ABI knows.
fn handled_fs(abi: i32) -> u64 {
    let mut rights = (1u64 << 13) - 1;
    if abi >= 2 {
        rights |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        rights |= ACCESS_FS_TRUNCATE;
    }
    rights
}

fn write_rights(abi: i32) -> u64 {
    handled_fs(abi) & !READ
        | ACCESS_FS_WRITE_FILE
        | ACCESS_FS_REMOVE_DIR
        | ACCESS_FS_REMOVE_FILE
        | ACCESS_FS_MAKE_CHAR
        | ACCESS_FS_MAKE_DIR
        | ACCESS_FS_MAKE_REG
        | ACCESS_FS_MAKE_SOCK
        | ACCESS_FS_MAKE_FIFO
        | ACCESS_FS_MAKE_BLOCK
        | ACCESS_FS_MAKE_SYM
}

/// The `landlock` backend.
#[derive(Debug, Clone, Default)]
pub struct Landlock;

impl Backend for Landlock {
    fn name(&self) -> &'static str {
        "landlock"
    }

    fn check(&self) -> Result<(), String> {
        match abi() {
            0 => Err("Landlock is not available: it needs Linux 5.13 or later with landlock among the enabled LSMs (see /sys/kernel/security/lsm)".to_string()),
            _ => Ok(()),
        }
    }

    fn enforces_network(&self) -> bool {
        true
    }

    fn enforces_deny_read(&self) -> bool {
        true
    }

    fn wrap(
        &self,
        spec: &Spec,
        runner: &Path,
        program: &str,
        args: &[&str],
    ) -> Result<(String, Vec<String>), String> {
        self.check()?;
        Ok(runner_command(
            runner,
            self.name(),
            spec_to_json(spec).to_string(),
            program,
            args,
        ))
    }
}

pub(crate) fn spec_to_json(spec: &Spec) -> serde_json::Value {
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "writable_roots": list(&spec.writable_roots),
        "deny_read": list(&spec.deny_read),
        "network": spec.network,
    })
}

fn spec_from_json(value: &serde_json::Value) -> Spec {
    let list = |key: &str| {
        value[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|path| path.as_str().map(PathBuf::from))
            .collect::<Vec<_>>()
    };
    Spec {
        writable_roots: list("writable_roots"),
        deny_read: list("deny_read"),
        network: value["network"].as_bool().unwrap_or(true),
        ..Spec::default()
    }
}

/// The runner: restricts this process, then replaces it with `command`.
pub(crate) fn exec_command(
    settings: &serde_json::Value,
    command: &[OsString],
) -> Result<i32, String> {
    let prepared = Prepared::new(&spec_from_json(settings))?;
    prepared
        .apply()
        .map_err(|error| format!("entering the sandbox failed: {error}"))?;
    let error = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    eprintln!(
        "lynshen sandbox: cannot run {}: {error}",
        Path::new(&command[0]).display()
    );
    Ok(if error.kind() == io::ErrorKind::NotFound {
        127
    } else {
        126
    })
}

/// A ruleset and filter built in advance, so that applying them needs no
/// allocation (it may run between fork and exec).
pub struct Prepared {
    ruleset: OwnedFd,
    seccomp: Option<Vec<libc::sock_filter>>,
}

impl Prepared {
    pub fn new(spec: &Spec) -> Result<Prepared, String> {
        let abi = abi();
        if abi == 0 {
            return Err("Landlock is not available on this kernel".to_string());
        }
        let handled_net = if !spec.network && abi >= 4 {
            ACCESS_NET_BIND_TCP | ACCESS_NET_CONNECT_TCP
        } else {
            0
        };
        let attr = RulesetAttr {
            handled_access_fs: handled_fs(abi),
            handled_access_net: handled_net,
        };
        // Without network rights the smaller, ABI 1 struct is what older
        // kernels accept.
        let size = if handled_net == 0 { 8 } else { 16 };
        // SAFETY: the attribute is `size` bytes of a valid struct.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                size as usize,
                0u32,
            )
        };
        if fd < 0 {
            return Err(format!(
                "landlock_create_ruleset failed: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: the kernel returned a new descriptor.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let handled = handled_fs(abi);
        let mut rules = read_rules(Path::new("/"), &spec.deny_read);
        for root in &spec.writable_roots {
            rules.push((root.clone(), READ | write_rights(abi)));
        }
        // Terminals and /dev/null.
        rules.push((PathBuf::from("/dev"), READ | ACCESS_FS_WRITE_FILE));
        for (path, access) in rules {
            add_rule(&ruleset, &path, access & handled)?;
        }
        Ok(Prepared {
            ruleset,
            seccomp: (!spec.network).then(network_filter),
        })
    }

    /// Restricts the calling thread, and what it executes, for good.
    /// Allocates nothing.
    pub fn apply(&self) -> io::Result<()> {
        // SAFETY: plain system calls on a descriptor and a filter this
        // value owns.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0u32,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if let Some(filter) = &self.seccomp {
                let program = libc::sock_fprog {
                    len: filter.len() as u16,
                    filter: filter.as_ptr() as *mut libc::sock_filter,
                };
                if libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                    &program as *const libc::sock_fprog,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }
}

/// Read rights for everything under `dir` but the `denied` paths: `dir`
/// whole when no denied path lies inside it, else `dir` listable and each
/// child covered the same way. A grant reaches everything below it, so
/// this is the only way to leave a hole.
fn read_rules(dir: &Path, denied: &[PathBuf]) -> Vec<(PathBuf, u64)> {
    if denied.iter().any(|path| path == dir) {
        return Vec::new();
    }
    if !denied.iter().any(|path| within(path, dir)) {
        return vec![(dir.to_path_buf(), READ)];
    }
    let mut rules = vec![(dir.to_path_buf(), ACCESS_FS_READ_DIR)];
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        rules.extend(read_rules(&entry.path(), denied));
    }
    rules
}

fn add_rule(ruleset: &OwnedFd, path: &Path, access: u64) -> Result<(), String> {
    let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
        return Ok(());
    };
    // SAFETY: a NUL-terminated path; the descriptor is closed below.
    let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        // Gone, or not ours to open: nothing to grant.
        return Ok(());
    }
    // SAFETY: the kernel returned a new descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let is_dir = fs::metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    let access = if is_dir { access } else { access & FILE_RIGHTS };
    if access == 0 {
        return Ok(());
    }
    let rule = PathBeneathAttr {
        allowed_access: access,
        parent_fd: fd.as_raw_fd(),
    };
    // SAFETY: the rule is a valid, packed struct.
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            RULE_PATH_BENEATH,
            &rule as *const PathBeneathAttr,
            0u32,
        )
    };
    if result != 0 {
        return Err(format!(
            "landlock_add_rule({}) failed: {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// A seccomp filter that refuses IPv4 and IPv6 sockets (`EACCES`) and
/// io_uring (`EPERM`), and system calls of another architecture.
fn network_filter() -> Vec<libc::sock_filter> {
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64
    let statement = |code: u32, k: u32| libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |k: u32, jt: u8, jf: u8| libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k,
    };
    let load = |offset: u32| statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset);
    let ret = |value: u32| statement(libc::BPF_RET | libc::BPF_K, value);
    let allow = ret(libc::SECCOMP_RET_ALLOW);
    let refuse = |errno: i32| ret(libc::SECCOMP_RET_ERRNO | errno as u32);
    // seccomp_data: nr at 0, arch at 4, args[0] at 16 (low half).
    let mut filter = vec![load(4), jump(ARCH, 1, 0), refuse(libc::EPERM), load(0)];
    #[cfg(target_arch = "x86_64")]
    filter.extend([
        // x32 system calls carry bit 30.
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: 0x4000_0000,
        },
        refuse(libc::EPERM),
    ]);
    filter.extend([
        jump(libc::SYS_io_uring_setup as u32, 0, 1),
        refuse(libc::EPERM),
        jump(libc::SYS_socket as u32, 1, 0),
        jump(libc::SYS_socketpair as u32, 0, 4),
        load(16),
        jump(libc::AF_INET as u32, 1, 0),
        jump(libc::AF_INET6 as u32, 0, 1),
        refuse(libc::EACCES),
        allow,
    ]);
    filter
}

#[cfg(test)]
mod tests;

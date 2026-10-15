//! The engine side of the Windows sandbox: which capability SID each
//! writable root is granted to, the ACEs and labels set for this process,
//! and taking them off again.
//!
//! Every SID is random and lives as long as this process: a root is
//! granted the first time a command needs it, and every grant is revoked
//! by `cleanup` (the engine calls it on exit). Each change is written to a
//! ledger in the state directory before it is made, so changes left by a
//! process that died are undone by the next one. The ledger file stays
//! open, unshared for writing, while its process lives; that is how a
//! process tells a live ledger from a stale one.
//!
//! Labels are shared state: two engines may label the same workspace. A
//! label is put back only by the last process that set it, to what it was
//! before the first one did.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
    Security::{
        Authorization::{DENY_ACCESS, GRANT_ACCESS},
        WinLowLabelSid, WinMediumLabelSid,
    },
    Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE},
    System::{
        SystemServices::{SYSTEM_MANDATORY_LABEL_NO_READ_UP, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP},
        Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject, INFINITE},
    },
};

use super::{
    acl::{self, Label},
    sid::Sid,
    wide,
};
use crate::{overlaps, within, Spec};

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// How long the last `prepare` spent changing ACLs, and on what: for
/// measurements (tests, logs).
#[derive(Debug, Clone, Default)]
pub struct PrepareStats {
    pub granted: Vec<(PathBuf, std::time::Duration)>,
    pub total: std::time::Duration,
}

static LAST_STATS: Mutex<Option<PrepareStats>> = Mutex::new(None);

pub fn last_prepare_stats() -> Option<PrepareStats> {
    LAST_STATS.lock().ok().and_then(|stats| stats.clone())
}

struct Session {
    ledger: File,
    ledger_path: PathBuf,
    state_dir: PathBuf,
    low: bool,
    /// Writable root → its capability SID.
    roots: HashMap<PathBuf, Sid>,
    /// A confined command's read-only directories → the SID that marks
    /// its token, which they deny writes to.
    markers: HashMap<Vec<PathBuf>, Sid>,
    /// (path, SID) pairs that already carry a deny-write ACE.
    denied: HashSet<(PathBuf, Sid)>,
    /// Credential paths already labelled no-read-up.
    unreadable: HashSet<PathBuf>,
}

/// What `prepare` set up for one command.
#[derive(Debug, Clone)]
pub struct Grant {
    /// The SIDs the command's token lists as restricting SIDs.
    pub caps: Vec<Sid>,
    /// Its private temp directory (`Spec::private_temp`).
    pub temp: Option<PathBuf>,
}

/// Grants what `spec` needs (see the module) and returns the SIDs the
/// command's token must list: its writable roots' and, for a confined
/// command, its marker.
pub fn prepare(state_dir: &Path, spec: &Spec, low: bool) -> Result<Grant, String> {
    let started = Instant::now();
    let _lock = AclLock::acquire()?;
    let mut guard = SESSION
        .lock()
        .map_err(|_| "the sandbox session lock is poisoned".to_string())?;
    if guard.is_none() {
        *guard = Some(Session::start(state_dir, low)?);
    }
    let session = guard.as_mut().expect("just started");
    let mut stats = PrepareStats::default();
    let mut caps = Vec::new();
    let mut labelled_root = false;
    let low_label = acl::inheritable_label(
        &Sid::well_known(WinLowLabelSid)?,
        SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
    );
    let mut roots = spec.writable_roots.clone();
    let temp = if spec.private_temp {
        let temp = session.ledger_path.with_extension("tmp");
        fs::create_dir_all(&temp)
            .map_err(|error| format!("creating {} failed: {error}", temp.display()))?;
        roots.push(temp.clone());
        Some(temp)
    } else {
        None
    };
    for root in roots.iter().filter(|root| root.is_dir()) {
        if let Some(sid) = session.roots.get(root) {
            caps.push(sid.clone());
            continue;
        }
        check_root(root)?;
        let sid = Sid::random();
        let began = Instant::now();
        session.record_ace(root, &sid)?;
        let label = if session.low {
            session.record_label(root)?;
            labelled_root = true;
            Some(&low_label)
        } else {
            None
        };
        acl::add_ace(root, &sid, GRANT_ACCESS, acl::WRITE_ALLOW, label)?;
        stats.granted.push((root.clone(), began.elapsed()));
        session.roots.insert(root.clone(), sid.clone());
        caps.push(sid);
    }
    for path in spec.read_only.iter().filter(|path| path.exists()) {
        for root in roots.iter().filter(|root| overlaps(root, path)) {
            let Some(sid) = session.roots.get(root).cloned() else {
                continue;
            };
            session.deny_write(path, &sid)?;
        }
    }
    if !spec.deny_write.is_empty() {
        let mut key = spec.deny_write.clone();
        key.sort();
        let marker = session
            .markers
            .entry(key)
            .or_insert_with(Sid::random)
            .clone();
        for dir in spec.deny_write.iter().filter(|dir| dir.exists()) {
            // A writable root inside it (the confined agent's worktree)
            // stays writable: its own allow ACE comes first below it.
            session.deny_write(dir, &marker)?;
        }
        caps.push(marker);
    }
    if session.low {
        let unreadable = acl::inheritable_label(
            &Sid::well_known(WinMediumLabelSid)?,
            SYSTEM_MANDATORY_LABEL_NO_READ_UP | SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
        );
        for path in spec.deny_read.iter().filter(|path| path.exists()) {
            let known = session.unreadable.contains(path);
            // A root labelled just now may have pushed its label below
            // the path: set the path's own again.
            if known && !labelled_root {
                continue;
            }
            if !known {
                session.record_label(path)?;
            }
            let label = if path.is_dir() {
                unreadable.clone()
            } else {
                Label {
                    flags: 0,
                    ..unreadable.clone()
                }
            };
            acl::set_label(path, Some(&label))?;
            session.unreadable.insert(path.clone());
        }
    }
    stats.total = started.elapsed();
    if let Ok(mut last) = LAST_STATS.lock() {
        *last = Some(stats);
    }
    Ok(Grant { caps, temp })
}

/// Takes off everything this process's session changed. Safe to call more
/// than once, and without a session.
pub fn cleanup() -> Result<(), String> {
    let session = SESSION
        .lock()
        .map_err(|_| "the sandbox session lock is poisoned".to_string())?
        .take();
    let Some(session) = session else {
        return Ok(());
    };
    // Same order as `prepare`: the cross-process lock, then the session.
    let _lock = AclLock::acquire()?;
    let Session {
        ledger,
        ledger_path,
        state_dir,
        ..
    } = session;
    drop(ledger);
    undo_ledger(&state_dir, &ledger_path)
}

impl Session {
    fn start(state_dir: &Path, low: bool) -> Result<Session, String> {
        fs::create_dir_all(state_dir)
            .map_err(|error| format!("creating {} failed: {error}", state_dir.display()))?;
        undo_stale_ledgers(state_dir);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or_default();
        let ledger_path = state_dir.join(format!("{}-{nanos}.jsonl", std::process::id()));
        let ledger = OpenOptions::new()
            .create_new(true)
            .write(true)
            .share_mode(FILE_SHARE_READ)
            .open(&ledger_path)
            .map_err(|error| format!("creating {} failed: {error}", ledger_path.display()))?;
        Ok(Session {
            ledger,
            ledger_path,
            state_dir: state_dir.to_path_buf(),
            low,
            roots: HashMap::new(),
            markers: HashMap::new(),
            denied: HashSet::new(),
            unreadable: HashSet::new(),
        })
    }

    fn deny_write(&mut self, path: &Path, sid: &Sid) -> Result<(), String> {
        let key = (path.to_path_buf(), sid.clone());
        if self.denied.contains(&key) {
            return Ok(());
        }
        self.record_ace(path, sid)?;
        acl::add_ace(path, sid, DENY_ACCESS, acl::WRITE_DENY, None)?;
        self.denied.insert(key);
        Ok(())
    }

    fn record_ace(&mut self, path: &Path, sid: &Sid) -> Result<(), String> {
        self.record(serde_json::json!({
            "ace": path.display().to_string(),
            "sid": sid.to_string_sid(),
        }))
    }

    /// Records the label `path` has now, or, when another live process
    /// changed it first, the one it had before that process did.
    fn record_label(&mut self, path: &Path) -> Result<(), String> {
        let previous = match original_label(&self.state_dir, &self.ledger_path, path) {
            Some(previous) => previous,
            None => acl::explicit_label(path)?
                .map(|label| label.to_json())
                .unwrap_or(serde_json::Value::Null),
        };
        self.record(serde_json::json!({
            "label": path.display().to_string(),
            "previous": previous,
        }))
    }

    fn record(&mut self, entry: serde_json::Value) -> Result<(), String> {
        writeln!(self.ledger, "{entry}")
            .and_then(|_| self.ledger.flush())
            .map_err(|error| format!("writing the sandbox ledger failed: {error}"))
    }
}

/// The label a live ledger other than `own` recorded for `path` before it
/// changed it: `Some(Value::Null)` for none.
fn original_label(state_dir: &Path, own: &Path, path: &Path) -> Option<serde_json::Value> {
    let wanted = path.display().to_string();
    for ledger in ledgers(state_dir)
        .into_iter()
        .filter(|ledger| ledger != own)
    {
        if !is_live(&ledger) {
            continue;
        }
        for entry in read_ledger(&ledger) {
            if entry["label"].as_str() == Some(wanted.as_str()) {
                return Some(entry["previous"].clone());
            }
        }
    }
    None
}

/// Undoes the ledgers of processes that ended without cleaning up.
fn undo_stale_ledgers(state_dir: &Path) {
    for ledger in ledgers(state_dir) {
        if !is_live(&ledger) {
            let _ = undo_ledger(state_dir, &ledger);
        }
    }
}

/// Undoes a ledger's entries, last first, then removes it with the
/// session's private temp directory. A label that a live process also set
/// is left to that process. When an entry cannot be undone the ledger
/// stays, so the next engine tries again.
fn undo_ledger(state_dir: &Path, ledger: &Path) -> Result<(), String> {
    // What the commands left in temp goes first: its grant then needs no
    // walk (a path that is gone has nothing to revoke).
    let _ = fs::remove_dir_all(ledger.with_extension("tmp"));
    let mut errors = Vec::new();
    for entry in read_ledger(ledger).into_iter().rev() {
        if let (Some(path), Some(sid)) = (entry["ace"].as_str(), entry["sid"].as_str()) {
            if let Err(error) = Sid::parse(sid).and_then(|sid| acl::revoke(Path::new(path), &sid)) {
                errors.push(error);
            }
        } else if let Some(path) = entry["label"].as_str() {
            let path = Path::new(path);
            if !path.exists() || original_label(state_dir, ledger, path).is_some() {
                continue;
            }
            let previous = Label::from_json(&entry["previous"]);
            if let Err(error) = acl::set_label(path, previous.as_ref()) {
                errors.push(error);
            }
        }
    }
    if errors.is_empty() {
        let _ = fs::remove_file(ledger);
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn ledgers(state_dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(state_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect()
}

fn read_ledger(ledger: &Path) -> Vec<serde_json::Value> {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(ledger)
    else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// Whether the process that writes `ledger` still runs: it keeps the file
/// open and refuses to share it for writing.
fn is_live(ledger: &Path) -> bool {
    OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(ledger)
        .is_err()
}

/// Serialises ACL changes between LynShen processes: a named mutex in the
/// session's namespace.
struct AclLock(HANDLE);

impl AclLock {
    fn acquire() -> Result<AclLock, String> {
        // SAFETY: the name is NUL-terminated; the handle is closed on drop.
        unsafe {
            let handle = CreateMutexW(
                std::ptr::null(),
                0,
                wide("Local\\LynShenSandboxAcl").as_ptr(),
            );
            if handle.is_null() {
                return Err(format!(
                    "creating the sandbox lock failed: {}",
                    super::sid::last_error()
                ));
            }
            match WaitForSingleObject(handle, INFINITE) {
                WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(AclLock(handle)),
                _ => {
                    CloseHandle(handle);
                    Err("waiting for the sandbox lock failed".to_string())
                }
            }
        }
    }
}

impl Drop for AclLock {
    fn drop(&mut self) {
        // SAFETY: the mutex is owned by this thread and the handle is open.
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

/// The capability SID this process granted `root`, if it did.
pub fn root_sid(root: &Path) -> Option<Sid> {
    SESSION.lock().ok()?.as_ref()?.roots.get(root).cloned()
}

/// Refuses a writable root whose grant would walk the whole profile or a
/// drive: a drive root, the user's profile or a directory around it.
fn check_root(root: &Path) -> Result<(), String> {
    // Both sides canonical: `\\?\C:\...`, long names (temp paths often
    // come with 8.3 names such as `RUNNER~1`).
    let canonical = |path: PathBuf| path.canonicalize().unwrap_or(path);
    let profile = std::env::var_os("USERPROFILE").map(|profile| canonical(PathBuf::from(profile)));
    let root = &canonical(root.to_path_buf());
    if root.parent().is_none()
        || root
            .parent()
            .is_some_and(|parent| parent.parent().is_none() && root.ends_with("Users"))
        || profile
            .as_ref()
            .is_some_and(|profile| within(profile, root))
    {
        return Err(format!(
            "the Windows sandbox does not make {} writable (a drive root or the user profile): open a project folder instead",
            root.display()
        ));
    }
    Ok(())
}

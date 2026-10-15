// Portions adapted from openai/codex, codex-rs/windows-sandbox-rs/src/acl.rs
// (Apache-2.0, Copyright 2025 OpenAI). See NOTICE.

//! ACEs for the sandbox's SIDs on files and directories, and integrity
//! labels. Every change is inheritable (`OI|CI`) and set with
//! `SetNamedSecurityInfoW`, which pushes it down to what already exists
//! below the path; that walk is the cost of granting a directory.

use std::{path::Path, ptr};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL},
    Security::{
        AddMandatoryAce,
        Authorization::{
            GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, ACCESS_MODE,
            DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
            SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
        },
        GetAce, InitializeAcl, ACE_HEADER, ACL, ACL_REVISION, DACL_SECURITY_INFORMATION,
        INHERITED_ACE, LABEL_SECURITY_INFORMATION, OBJECT_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SUB_CONTAINERS_AND_OBJECTS_INHERIT, SYSTEM_MANDATORY_LABEL_ACE,
    },
    Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC,
        WRITE_OWNER,
    },
    System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, SYSTEM_MANDATORY_LABEL_ACE_TYPE,
    },
};

use super::{sid::Sid, wide};

/// What a writable root grants its capability SID: read, write, run and
/// delete, but not `FILE_DELETE_CHILD`. Deleting or renaming an entry then
/// takes `DELETE` on the entry itself, which a protected path (`.git`)
/// denies; with `FILE_DELETE_CHILD` on its parent it could be renamed away
/// and replaced.
pub const WRITE_ALLOW: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// What a protected path denies: every way to change it, its entries, its
/// attributes or its ACL, and to delete or rename it.
pub const WRITE_DENY: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | FILE_DELETE_CHILD
    | WRITE_DAC
    | WRITE_OWNER;

/// A mandatory integrity label: the level (`S-1-16-4096` low,
/// `S-1-16-8192` medium), its policy (`NO_WRITE_UP` 1, `NO_READ_UP` 2) and
/// ACE flags (inheritance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub level: Sid,
    pub policy: u32,
    pub flags: u32,
}

impl Label {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "level": self.level.to_string_sid(),
            "policy": self.policy,
            "flags": self.flags,
        })
    }

    pub fn from_json(value: &serde_json::Value) -> Option<Label> {
        Some(Label {
            level: Sid::parse(value["level"].as_str()?).ok()?,
            policy: value["policy"].as_u64()? as u32,
            flags: value["flags"].as_u64()? as u32,
        })
    }
}

/// Adds an inheritable `GRANT_ACCESS` or `DENY_ACCESS` ACE for `sid` to
/// `path` and, with `label`, sets its label in the same pass.
pub fn add_ace(
    path: &Path,
    sid: &Sid,
    mode: ACCESS_MODE,
    mask: u32,
    label: Option<&Label>,
) -> Result<(), String> {
    debug_assert!(mode == GRANT_ACCESS || mode == DENY_ACCESS);
    let entry = explicit_access(sid, mode, mask);
    // SAFETY: `entry` points at a SID that outlives the calls; the ACLs and
    // the descriptor Windows allocates are freed by their guards.
    unsafe {
        let current = Dacl::read(path)?;
        let mut updated: *mut ACL = ptr::null_mut();
        let code = SetEntriesInAclW(1, &entry, current.dacl, &mut updated);
        if code != ERROR_SUCCESS {
            return Err(format!(
                "SetEntriesInAclW failed for {}: {code}",
                path.display()
            ));
        }
        let updated = Local(updated.cast());
        let mut info: OBJECT_SECURITY_INFORMATION = DACL_SECURITY_INFORMATION;
        let sacl = label.map(label_acl);
        if sacl.is_some() {
            info |= LABEL_SECURITY_INFORMATION;
        }
        set(
            path,
            info,
            updated.0.cast(),
            sacl.as_ref().map_or(ptr::null(), |acl| acl.as_ptr().cast()),
        )
    }
}

/// Removes every explicit ACE for `sid` from `path`, and from what below it
/// inherited them. Nothing to remove is not an error, and a path that is
/// gone neither.
pub fn revoke(path: &Path, sid: &Sid) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let entry = explicit_access(sid, REVOKE_ACCESS, 0);
    // SAFETY: as in `add_ace`.
    unsafe {
        let current = Dacl::read(path)?;
        if current.dacl.is_null() {
            return Ok(());
        }
        let mut updated: *mut ACL = ptr::null_mut();
        let code = SetEntriesInAclW(1, &entry, current.dacl, &mut updated);
        if code != ERROR_SUCCESS {
            return Err(format!(
                "SetEntriesInAclW failed for {}: {code}",
                path.display()
            ));
        }
        let updated = Local(updated.cast());
        // An unchanged ACL needs no walk below the path.
        if (*updated.0.cast::<ACL>()).AceCount == (*current.dacl).AceCount {
            return Ok(());
        }
        set(
            path,
            DACL_SECURITY_INFORMATION,
            updated.0.cast(),
            ptr::null(),
        )
    }
}

/// Whether `path` carries an ACE for `sid` that it did not inherit:
/// `Some(true)` an allow, `Some(false)` a deny.
pub fn explicit_ace(path: &Path, sid: &Sid) -> Result<Option<bool>, String> {
    // SAFETY: the ACEs are read inside the descriptor the guard keeps alive;
    // an allowed or denied ACE's SID starts after its header and mask.
    unsafe {
        let current = Dacl::read(path)?;
        if current.dacl.is_null() {
            return Ok(None);
        }
        for index in 0..u32::from((*current.dacl).AceCount) {
            let mut ace: *mut core::ffi::c_void = ptr::null_mut();
            if GetAce(current.dacl, index, &mut ace) == 0 {
                continue;
            }
            let header = &*(ace as *const ACE_HEADER);
            let kind = u32::from(header.AceType);
            if u32::from(header.AceFlags) & INHERITED_ACE != 0
                || !(kind == ACCESS_ALLOWED_ACE_TYPE || kind == ACCESS_DENIED_ACE_TYPE)
            {
                continue;
            }
            let ace_sid = (ace as *mut u8).add(8) as PSID;
            if sid.equals(ace_sid) {
                return Ok(Some(kind == ACCESS_ALLOWED_ACE_TYPE));
            }
        }
        Ok(None)
    }
}

/// The label `path` carries itself (not one it inherited).
pub fn explicit_label(path: &Path) -> Result<Option<Label>, String> {
    // SAFETY: as in `explicit_ace`; a label ACE's SID starts at `SidStart`.
    unsafe {
        let mut sacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let code = GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sacl,
            &mut descriptor,
        );
        if code != ERROR_SUCCESS {
            return Err(format!(
                "reading the label of {} failed: {code}",
                path.display()
            ));
        }
        let _descriptor = Local(descriptor);
        if sacl.is_null() {
            return Ok(None);
        }
        for index in 0..u32::from((*sacl).AceCount) {
            let mut ace: *mut core::ffi::c_void = ptr::null_mut();
            if GetAce(sacl, index, &mut ace) == 0 {
                continue;
            }
            let label = &*(ace as *const SYSTEM_MANDATORY_LABEL_ACE);
            let flags = u32::from(label.Header.AceFlags);
            if u32::from(label.Header.AceType) == SYSTEM_MANDATORY_LABEL_ACE_TYPE
                && flags & INHERITED_ACE == 0
            {
                return Ok(Some(Label {
                    level: Sid::copy(&label.SidStart as *const u32 as PSID)?,
                    policy: label.Mask,
                    flags,
                }));
            }
        }
        Ok(None)
    }
}

/// Sets `path`'s own label to `label`, or removes it (`None`), and pushes
/// the change down to what inherited it.
pub fn set_label(path: &Path, label: Option<&Label>) -> Result<(), String> {
    let acl = match label {
        Some(label) => label_acl(label),
        None => empty_acl(),
    };
    // SAFETY: the ACL buffer outlives the call.
    unsafe {
        set(
            path,
            LABEL_SECURITY_INFORMATION,
            ptr::null(),
            acl.as_ptr().cast(),
        )
    }
}

/// An inheritable label at `level` with `policy`.
pub fn inheritable_label(level: &Sid, policy: u32) -> Label {
    Label {
        level: level.clone(),
        policy,
        flags: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    }
}

unsafe fn set(
    path: &Path,
    info: OBJECT_SECURITY_INFORMATION,
    dacl: *const ACL,
    sacl: *const ACL,
) -> Result<(), String> {
    let code = SetNamedSecurityInfoW(
        wide(path).as_ptr(),
        SE_FILE_OBJECT,
        info,
        ptr::null_mut(),
        ptr::null_mut(),
        dacl,
        sacl,
    );
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "changing the security of {} failed: {}",
            path.display(),
            std::io::Error::from_raw_os_error(code as i32)
        ))
    }
}

fn explicit_access(sid: &Sid, mode: ACCESS_MODE, mask: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.as_psid().cast(),
        },
    }
}

/// A SACL holding one label ACE, in a `u32` buffer so it is aligned.
fn label_acl(label: &Label) -> Vec<u32> {
    let size = std::mem::size_of::<ACL>() + std::mem::size_of::<SYSTEM_MANDATORY_LABEL_ACE>() + 68;
    let mut buffer = vec![0u32; size.div_ceil(4)];
    // SAFETY: the buffer is large enough for the header, one ACE and the
    // largest SID.
    unsafe {
        let acl = buffer.as_mut_ptr().cast::<ACL>();
        InitializeAcl(acl, (buffer.len() * 4) as u32, ACL_REVISION);
        AddMandatoryAce(
            acl,
            ACL_REVISION,
            label.flags,
            label.policy,
            label.level.as_psid(),
        );
    }
    buffer
}

fn empty_acl() -> Vec<u32> {
    let mut buffer = vec![0u32; std::mem::size_of::<ACL>().div_ceil(4)];
    // SAFETY: the buffer holds an ACL header.
    unsafe {
        InitializeAcl(
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 4) as u32,
            ACL_REVISION,
        );
    }
    buffer
}

/// Memory Windows allocated with `LocalAlloc`.
struct Local(*mut core::ffi::c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by Windows with LocalAlloc, freed once.
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

/// A path's DACL, inside the descriptor that holds it.
struct Dacl {
    dacl: *mut ACL,
    _descriptor: Local,
}

impl Dacl {
    unsafe fn read(path: &Path) -> Result<Dacl, String> {
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let code = GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        );
        if code != ERROR_SUCCESS {
            return Err(format!(
                "reading the ACL of {} failed: {}",
                path.display(),
                std::io::Error::from_raw_os_error(code as i32)
            ));
        }
        Ok(Dacl {
            dacl,
            _descriptor: Local(descriptor),
        })
    }
}

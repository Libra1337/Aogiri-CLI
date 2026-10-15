// Portions adapted from openai/codex, codex-rs/windows-sandbox-rs/src/token.rs
// (Apache-2.0, Copyright 2025 OpenAI). See NOTICE.

//! The token a sandboxed command runs with.
//!
//! `CreateRestrictedToken` with `WRITE_RESTRICTED`: every write is checked
//! twice, once against the user's SIDs and once against the restricting
//! SIDs (the command's capability SIDs, the logon SID and Everyone), and is
//! allowed only when both pass. Reads are checked against the user's SIDs
//! only. `DISABLE_MAX_PRIVILEGE` drops every privilege but
//! SeChangeNotifyPrivilege. Then, with `low`, the integrity level drops to
//! Low: writes need a Low label on the target (no write up), and a path
//! labelled Medium with no-read-up cannot be read at all.

use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_SUCCESS, GENERIC_ALL, HANDLE, HLOCAL},
    Security::{
        Authorization::{
            SetEntriesInAclW, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, TRUSTEE_IS_SID,
            TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
        },
        CreateRestrictedToken, GetLengthSid, GetTokenInformation, SetTokenInformation,
        TokenDefaultDacl, TokenGroups, TokenIntegrityLevel, TokenUser, WinLocalSystemSid,
        WinLowLabelSid, WinWorldSid, ACL, DISABLE_MAX_PRIVILEGE, LUA_TOKEN, SID_AND_ATTRIBUTES,
        TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL,
        TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
        TOKEN_USER, WRITE_RESTRICTED,
    },
    System::{
        SystemServices::{SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

use super::sid::{last_error, Sid};

/// A primary token for `CreateProcessAsUserW`, restricted as the module
/// says, with `capabilities` among its restricting SIDs.
pub fn sandbox_token(capabilities: &[Sid], low: bool) -> Result<OwnedHandle, String> {
    token_with(capabilities, low, Variant::WriteRestricted)
}

/// How restricted a token is: what the sandbox uses, and two variants the
/// tests compare tools against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    /// The sandbox's token.
    WriteRestricted,
    /// Write-restricted, with the user's own SID among the restricting
    /// SIDs: writes are then checked as the user's (no protection by ACL).
    #[cfg_attr(not(test), allow(dead_code))]
    WithUser,
    /// Not write-restricted: only the integrity level limits writes.
    #[cfg_attr(not(test), allow(dead_code))]
    Unrestricted,
}

pub(crate) fn token_with(
    capabilities: &[Sid],
    low: bool,
    variant: Variant,
) -> Result<OwnedHandle, String> {
    // SAFETY: handles are owned as soon as Windows returns them; buffers
    // outlive the calls that read them.
    unsafe {
        let mut raw: HANDLE = ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            &mut raw,
        ) == 0
        {
            return Err(format!("OpenProcessToken failed: {}", last_error()));
        }
        let base = OwnedHandle::from_raw_handle(raw);
        let logon = logon_sid(raw)?.ok_or_else(|| {
            "this process's token has no logon SID (a service or a batch logon?); the Windows sandbox needs an interactive logon".to_string()
        })?;
        let user = token_user(raw)?;
        let everyone = Sid::well_known(WinWorldSid)?;
        let extra: Vec<&Sid> = match variant {
            Variant::WriteRestricted => vec![&logon, &everyone],
            Variant::WithUser => vec![&logon, &everyone, &user],
            Variant::Unrestricted => Vec::new(),
        };
        let chosen: Vec<&Sid> = match variant {
            Variant::Unrestricted => Vec::new(),
            _ => capabilities.iter().chain(extra).collect(),
        };
        let restricting: Vec<SID_AND_ATTRIBUTES> = chosen
            .into_iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.as_psid(),
                Attributes: 0,
            })
            .collect();
        let flags = match variant {
            Variant::Unrestricted => DISABLE_MAX_PRIVILEGE | LUA_TOKEN,
            _ => DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
        };
        let mut restricted: HANDLE = ptr::null_mut();
        if CreateRestrictedToken(
            base.as_raw_handle(),
            flags,
            0,
            ptr::null(),
            0,
            ptr::null(),
            restricting.len() as u32,
            if restricting.is_empty() {
                ptr::null()
            } else {
                restricting.as_ptr()
            },
            &mut restricted,
        ) == 0
        {
            return Err(format!("CreateRestrictedToken failed: {}", last_error()));
        }
        let token = OwnedHandle::from_raw_handle(restricted);
        set_default_dacl(
            restricted,
            &[&logon, &user, &Sid::well_known(WinLocalSystemSid)?],
        )?;
        if low {
            let level = Sid::well_known(WinLowLabelSid)?;
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: level.as_psid(),
                    Attributes: SE_GROUP_INTEGRITY as u32,
                },
            };
            if SetTokenInformation(
                restricted,
                TokenIntegrityLevel,
                (&label as *const TOKEN_MANDATORY_LABEL).cast(),
                (std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32)
                    + GetLengthSid(level.as_psid()),
            ) == 0
            {
                return Err(format!(
                    "lowering the integrity level failed: {}",
                    last_error()
                ));
            }
        }
        Ok(token)
    }
}

/// What the command creates without an ACL of its own (pipes, processes,
/// threads) is open to its logon session: the restricted write check passes
/// on the logon SID, so the command can use its own pipes and children,
/// and the user's other processes can still manage them.
unsafe fn set_default_dacl(token: HANDLE, sids: &[&Sid]) -> Result<(), String> {
    let entries: Vec<EXPLICIT_ACCESS_W> = sids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: sid.as_psid().cast(),
            },
        })
        .collect();
    let mut dacl: *mut ACL = ptr::null_mut();
    let code = SetEntriesInAclW(
        entries.len() as u32,
        entries.as_ptr(),
        ptr::null(),
        &mut dacl,
    );
    if code != ERROR_SUCCESS {
        return Err(format!("SetEntriesInAclW failed: {code}"));
    }
    let info = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
    let ok = SetTokenInformation(
        token,
        TokenDefaultDacl,
        (&info as *const TOKEN_DEFAULT_DACL).cast(),
        std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
    );
    let error = last_error();
    LocalFree(dacl as HLOCAL);
    if ok == 0 {
        return Err(format!("setting the default DACL failed: {error}"));
    }
    Ok(())
}

/// The token's information of `class`, in an 8-byte aligned buffer.
unsafe fn information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u64>, String> {
    let mut needed = 0u32;
    GetTokenInformation(token, class, ptr::null_mut(), 0, &mut needed);
    if needed == 0 {
        return Err(format!(
            "GetTokenInformation({class}) failed: {}",
            last_error()
        ));
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    if GetTokenInformation(
        token,
        class,
        buffer.as_mut_ptr().cast(),
        (buffer.len() * 8) as u32,
        &mut needed,
    ) == 0
    {
        return Err(format!(
            "GetTokenInformation({class}) failed: {}",
            last_error()
        ));
    }
    Ok(buffer)
}

unsafe fn logon_sid(token: HANDLE) -> Result<Option<Sid>, String> {
    let buffer = information(token, TokenGroups)?;
    let groups = &*(buffer.as_ptr() as *const TOKEN_GROUPS);
    let entries = std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize);
    let logon_id = SE_GROUP_LOGON_ID as u32;
    for group in entries {
        if group.Attributes & logon_id == logon_id {
            return Sid::copy(group.Sid).map(Some);
        }
    }
    Ok(None)
}

unsafe fn token_user(token: HANDLE) -> Result<Sid, String> {
    let buffer = information(token, TokenUser)?;
    let user = &*(buffer.as_ptr() as *const TOKEN_USER);
    Sid::copy(user.User.Sid)
}

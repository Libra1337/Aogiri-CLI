// Portions adapted from openai/codex, codex-rs/windows-sandbox-rs/src/token.rs
// and src/cap.rs (Apache-2.0, Copyright 2025 OpenAI). See NOTICE.

//! Security identifiers: parsing, printing, well-known SIDs and the random
//! capability SIDs that writable roots are granted to.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    ptr,
    time::{SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{LocalFree, HLOCAL},
    Security::{
        Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW},
        CopySid, CreateWellKnownSid, EqualSid, GetLengthSid, IsValidSid, PSID, WELL_KNOWN_SID_TYPE,
    },
};

use super::wide;

/// A SID in a buffer this process owns.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Sid(Vec<u8>);

impl std::fmt::Debug for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string_sid())
    }
}

impl Sid {
    /// `S-1-5-21-…` to a SID.
    pub fn parse(text: &str) -> Result<Sid, String> {
        let mut psid: PSID = ptr::null_mut();
        // SAFETY: the string is NUL-terminated; on success Windows allocates
        // the SID with LocalAlloc, copied below and then freed.
        unsafe {
            if ConvertStringSidToSidW(wide(text).as_ptr(), &mut psid) == 0 {
                return Err(format!("invalid SID {text}: {}", last_error()));
            }
            let sid = Sid::copy(psid);
            LocalFree(psid as HLOCAL);
            sid
        }
    }

    /// Copies the SID `psid` points at.
    ///
    /// # Safety
    /// `psid` must point at a valid SID.
    pub unsafe fn copy(psid: PSID) -> Result<Sid, String> {
        if IsValidSid(psid) == 0 {
            return Err("invalid SID".to_string());
        }
        let length = GetLengthSid(psid);
        let mut buffer = vec![0u8; length as usize];
        if CopySid(length, buffer.as_mut_ptr().cast(), psid) == 0 {
            return Err(format!("CopySid failed: {}", last_error()));
        }
        Ok(Sid(buffer))
    }

    pub fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Sid, String> {
        let mut buffer = vec![0u8; 68];
        let mut size = buffer.len() as u32;
        // SAFETY: the buffer holds `size` bytes, the largest SID there is.
        let ok = unsafe {
            CreateWellKnownSid(kind, ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size)
        };
        if ok == 0 {
            return Err(format!(
                "CreateWellKnownSid({kind}) failed: {}",
                last_error()
            ));
        }
        buffer.truncate(size as usize);
        Ok(Sid(buffer))
    }

    /// A SID no account has: `S-1-5-21-` and four random numbers, as Codex
    /// makes its capability SIDs. A token that lists it as a restricting SID
    /// may write where an ACL grants it.
    pub fn random() -> Sid {
        let state = RandomState::new();
        let mut parts = [0u32; 4];
        for (index, part) in parts.iter_mut().enumerate() {
            let mut hasher = state.build_hasher();
            hasher.write_usize(index);
            hasher.write_u32(std::process::id());
            hasher.write_u128(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|time| time.as_nanos())
                    .unwrap_or_default(),
            );
            *part = hasher.finish() as u32;
        }
        let [a, b, c, d] = parts;
        Sid::parse(&format!("S-1-5-21-{a}-{b}-{c}-{d}")).expect("a well-formed SID")
    }

    pub fn as_psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }

    pub fn to_string_sid(&self) -> String {
        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: the SID is valid; Windows allocates the string, freed below.
        unsafe {
            if ConvertSidToStringSidW(self.as_psid(), &mut text) == 0 {
                return "S-?".to_string();
            }
            let mut length = 0;
            while *text.add(length) != 0 {
                length += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            LocalFree(text as HLOCAL);
            value
        }
    }

    /// Whether `psid` is this SID.
    ///
    /// # Safety
    /// `psid` must point at a valid SID.
    pub unsafe fn equals(&self, psid: PSID) -> bool {
        EqualSid(self.as_psid(), psid) != 0
    }
}

pub(crate) fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}

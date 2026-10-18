//! The computer's hardware fingerprint for the Monoize gateway (gateway spec
//! `authorized-devices.spec.md` AD-H1): a device key works only on the
//! computer that authorized it, and the account's seats count computers.
//!
//! Three components, each read from the operating system and sent only as a
//! keyed hash (`HMAC-SHA256("lynshen-device-v1/<name>", value)`, base64url):
//!
//! | name | macOS | Windows | Linux |
//! |---|---|---|---|
//! | `hw` | `IOPlatformUUID` | SMBIOS system UUID | `/sys/class/dmi/id/product_uuid` (root) |
//! | `inst` | — | `MachineGuid` | `/etc/machine-id` |
//! | `nic` | built-in primary MAC | permanent MAC of a physical adapter | MAC of a physical device |
//!
//! The gateway re-keys the values per account with a server secret before it
//! stores them. Placeholder ids (all zeros, the common OEM dummy UUIDs) and
//! locally administered or multicast MACs (random Wi-Fi addresses, VPN, TAP
//! and virtual adapters) are left out. `hw` and `inst` decide; `nic` matters
//! only where neither can be read, so a dock or a USB adapter does not make a
//! new device.

use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// The request header (AD-H1).
pub const HEADER: &str = "X-LynShen-Device";
const KEY_PREFIX: &str = "lynshen-device-v1/";
/// The DT-D2 prefix of the desktop's `device_hash` (Aogiri `device_id.rs`).
const DESKTOP_HASH_PREFIX: &str = "lynshen-desktop-device-v1:";
const MAX_HOST_BYTES: usize = 64;

/// Raw identifiers as read from the OS; never sent or stored as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    /// Firmware / platform UUID.
    pub hw: Option<String>,
    /// OS installation id (MachineGuid, machine-id).
    pub inst: Option<String>,
    /// Primary physical MAC, 12 lowercase hex digits.
    pub nic: Option<String>,
    /// The id the desktop's DT-D2 `device_hash` uses on this OS.
    pub desktop_id: Option<String>,
    pub host: Option<String>,
    pub os: &'static str,
}

/// HMAC-SHA256 (RFC 2104) with SHA-256, so no `hmac` dependency is needed.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn component(name: &str, value: &str) -> String {
    let key = format!("{KEY_PREFIX}{name}");
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(hmac_sha256(key.as_bytes(), value.as_bytes()))
}

/// A UUID or machine id as 32 lowercase hex digits, or None for a value that
/// names no machine.
pub(crate) fn normalize_id(raw: &str) -> Option<String> {
    let hex: String = raw
        .trim()
        .trim_matches(|c| c == '{' || c == '}')
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let first = hex.as_bytes()[0];
    if hex.bytes().all(|b| b == first) {
        return None;
    }
    const PLACEHOLDERS: [&str; 2] = [
        "03000200040005000006000700080009",
        "00020003000400050006000700080009",
    ];
    (!PLACEHOLDERS.contains(&hex.as_str())).then_some(hex)
}

/// A MAC as 12 lowercase hex digits, or None for one that is not a burned-in
/// global address (all zeros, broadcast, multicast, locally administered).
pub(crate) fn normalize_mac(raw: &[u8]) -> Option<String> {
    if raw.len() != 6 || raw.iter().all(|b| *b == 0) || raw.iter().all(|b| *b == 0xff) {
        return None;
    }
    if raw[0] & 0x01 != 0 || raw[0] & 0x02 != 0 {
        return None;
    }
    Some(raw.iter().map(|b| format!("{b:02x}")).collect())
}

/// Parses `aa:bb:cc:dd:ee:ff` (or with `-`) into bytes.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_mac_text(text: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = text
        .trim()
        .split([':', '-'])
        .map(|part| u8::from_str_radix(part, 16).ok())
        .collect::<Option<_>>()?;
    (bytes.len() == 6).then_some(bytes)
}

impl Sources {
    /// The `X-LynShen-Device` value, or None when no component is readable.
    pub fn header_value(&self, cli_version: &str) -> Option<String> {
        let hw = self.hw.as_deref().and_then(normalize_id);
        let inst = self.inst.as_deref().and_then(normalize_id);
        let nic = self.nic.clone();
        if hw.is_none() && inst.is_none() && nic.is_none() {
            return None;
        }
        let mut value = String::from("v1");
        for (name, raw) in [("hw", &hw), ("inst", &inst), ("nic", &nic)] {
            if let Some(raw) = raw {
                value.push_str(&format!(";{name}={}", component(name, raw)));
            }
        }
        if let Some(hash) = self.desktop_id.as_deref().and_then(desktop_hash_of) {
            value.push_str(&format!(";dt={hash}"));
        }
        value.push_str(&format!(";os={}", self.os));
        if let Some(host) = self
            .host
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
        {
            let mut host = host.to_string();
            while host.len() > MAX_HOST_BYTES {
                host.pop();
            }
            let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(host.as_bytes());
            value.push_str(&format!(";host={encoded}"));
        }
        let cli: String = cli_version
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || ".+-".contains(*c))
            .take(32)
            .collect();
        if !cli.is_empty() {
            value.push_str(&format!(";cli={cli}"));
        }
        Some(value)
    }
}

/// DT-D2 (Aogiri `device_id.rs`): `hex(SHA-256(prefix + id.trim().lowercase()))`.
fn desktop_hash_of(machine_id: &str) -> Option<String> {
    let id = machine_id.trim().to_lowercase();
    if !id.chars().any(|c| c.is_alphanumeric() && c != '0') {
        return None;
    }
    let digest = Sha256::digest(format!("{DESKTOP_HASH_PREFIX}{id}").as_bytes());
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// This computer's header value, read once per process; None where nothing is
/// readable. `LYNSHEN_DEVICE_ID=off` sends none (tests, containers by choice).
pub fn header_value() -> Option<&'static str> {
    static VALUE: OnceLock<Option<String>> = OnceLock::new();
    VALUE
        .get_or_init(|| {
            if std::env::var("LYNSHEN_DEVICE_ID").is_ok_and(|v| v.trim() == "off") {
                return None;
            }
            read_sources().header_value(env!("CARGO_PKG_VERSION"))
        })
        .as_deref()
}

/// The headers every request to the Monoize gateway carries, for provider
/// `provider`; empty for every other provider.
pub fn gateway_headers(provider: &str) -> Vec<(String, String)> {
    if provider != "monoize" {
        return Vec::new();
    }
    header_value()
        .map(|value| vec![(HEADER.to_string(), value.to_string())])
        .unwrap_or_default()
}

// ---------------------------------------------------------------- macOS

/// The `IOPlatformUUID` in `ioreg -rd1 -c IOPlatformExpertDevice` output.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_platform_uuid(ioreg: &str) -> Option<String> {
    ioreg.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim().trim_start_matches(['|', ' ']) == "\"IOPlatformUUID\"")
            .then(|| value.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    })
}

/// The built-in primary MAC from `ioreg -r -c IOEthernetController -l -d 2`:
/// each controller block carries `IOMACAddress`, its interface child
/// `IOBuiltin` and `IOPrimaryInterface`. The primary built-in one wins, else
/// the built-in one with the lowest BSD name.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_primary_mac(ioreg: &str) -> Option<String> {
    #[derive(Default)]
    struct Block {
        mac: Option<String>,
        bsd: Option<String>,
        builtin: bool,
        primary: bool,
    }
    let mut blocks: Vec<Block> = Vec::new();
    for line in ioreg.lines() {
        let trimmed = line.trim_start_matches([' ', '|']);
        if line.starts_with("+-o") {
            blocks.push(Block::default());
            continue;
        }
        let Some(block) = blocks.last_mut() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "\"IOMACAddress\"" => {
                let hex = value.trim_matches(['<', '>']);
                let bytes: Option<Vec<u8>> = (0..hex.len())
                    .step_by(2)
                    .map(|i| {
                        hex.get(i..i + 2)
                            .and_then(|b| u8::from_str_radix(b, 16).ok())
                    })
                    .collect();
                block.mac = bytes.as_deref().and_then(normalize_mac);
            }
            "\"BSD Name\"" => block.bsd = Some(value.trim_matches('"').to_string()),
            "\"IOBuiltin\"" => block.builtin = value == "Yes",
            "\"IOPrimaryInterface\"" => block.primary = value == "Yes",
            _ => {}
        }
    }
    let mut builtin: Vec<&Block> = blocks
        .iter()
        .filter(|b| b.builtin && b.mac.is_some())
        .collect();
    builtin.sort_by(|a, b| b.primary.cmp(&a.primary).then(a.bsd.cmp(&b.bsd)));
    builtin.first().and_then(|b| b.mac.clone())
}

// ---------------------------------------------------------------- Windows

/// The system UUID of SMBIOS structure type 1 in a raw `GetSystemFirmwareTable`
/// ('RSMB') buffer: an 8-byte header, then the structures. The first three
/// UUID fields are little-endian from SMBIOS 2.6 on.
#[cfg(any(windows, test))]
pub(crate) fn smbios_system_uuid(raw: &[u8]) -> Option<String> {
    let major = *raw.get(1)?;
    let minor = *raw.get(2)?;
    let length = u32::from_le_bytes(raw.get(4..8)?.try_into().ok()?) as usize;
    let table = raw.get(8..8 + length.min(raw.len().saturating_sub(8)))?;
    let mut at = 0;
    while at + 4 <= table.len() {
        let kind = table[at];
        let formatted = table[at + 1] as usize;
        if formatted < 4 || at + formatted > table.len() {
            return None;
        }
        if kind == 1 && formatted >= 0x19 {
            let u = &table[at + 8..at + 24];
            let le = (major, minor) >= (2, 6);
            let order: [usize; 16] = if le {
                [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15]
            } else {
                [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
            };
            let hex: String = order.iter().map(|i| format!("{:02x}", u[*i])).collect();
            return normalize_id(&hex);
        }
        if kind == 127 {
            return None;
        }
        // Skip the formatted area and the string set (ends with two NULs).
        let mut next = at + formatted;
        while next + 1 < table.len() && !(table[next] == 0 && table[next + 1] == 0) {
            next += 1;
        }
        at = next + 2;
    }
    None
}

/// One network adapter as Windows reports it (`GetIfTable2`).
#[cfg(any(windows, test))]
#[derive(Debug, Clone)]
pub(crate) struct WindowsAdapter {
    pub permanent_mac: Vec<u8>,
    /// `IF_TYPE_ETHERNET_CSMACD` (6) or `IF_TYPE_IEEE80211` (71).
    pub if_type: u32,
    /// `InterfaceAndOperStatusFlags.HardwareInterface`.
    pub hardware: bool,
    pub description: String,
}

/// The primary physical adapter's permanent MAC: hardware Ethernet or Wi-Fi
/// adapters whose description names no virtual adapter; Wi-Fi first (built
/// into laptops), then the lowest MAC.
#[cfg(any(windows, test))]
pub(crate) fn windows_primary_mac(adapters: &[WindowsAdapter]) -> Option<String> {
    const VIRTUAL: [&str; 8] = [
        "virtual",
        "vpn",
        "tap",
        "hyper-v",
        "vmware",
        "virtualbox",
        "loopback",
        "bluetooth",
    ];
    let mut candidates: Vec<(bool, String)> = adapters
        .iter()
        .filter(|a| a.hardware && (a.if_type == 6 || a.if_type == 71))
        .filter(|a| {
            let description = a.description.to_lowercase();
            !VIRTUAL.iter().any(|word| description.contains(word))
        })
        .filter_map(|a| normalize_mac(&a.permanent_mac).map(|mac| (a.if_type != 71, mac)))
        .collect();
    candidates.sort();
    candidates.into_iter().next().map(|(_, mac)| mac)
}

// ---------------------------------------------------------------- Linux

/// One entry of `/sys/class/net`.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone)]
pub(crate) struct LinuxInterface {
    /// The `device` link exists (a bus device, not a virtual interface).
    pub has_device: bool,
    /// `type` (1 = Ethernet/Wi-Fi ARPHRD_ETHER).
    pub kind: u32,
    /// `addr_assign_type` (0 = permanent).
    pub addr_assign_type: u32,
    pub address: String,
    pub wireless: bool,
}

/// The primary physical interface's MAC: a bus device with a permanent
/// Ethernet address; wireless first, then the lowest MAC.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn linux_primary_mac(interfaces: &[LinuxInterface]) -> Option<String> {
    let mut candidates: Vec<(bool, String)> = interfaces
        .iter()
        .filter(|i| i.has_device && i.kind == 1 && i.addr_assign_type == 0)
        .filter_map(|i| {
            parse_mac_text(&i.address)
                .as_deref()
                .and_then(normalize_mac)
                .map(|mac| (!i.wireless, mac))
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next().map(|(_, mac)| mac)
}

/// A machine-id file's value; `uninitialized` names no machine.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn machine_id_file(text: &str) -> Option<String> {
    let id = text.trim();
    (!id.is_empty() && id != "uninitialized").then(|| id.to_string())
}

// ---------------------------------------------------------------- readers

#[cfg(any(target_os = "macos", windows))]
fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn read_sources() -> Sources {
    let platform = command_output("/usr/sbin/ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"]);
    let hw = platform.as_deref().and_then(macos_platform_uuid);
    let nic = command_output(
        "/usr/sbin/ioreg",
        &["-r", "-c", "IOEthernetController", "-l", "-d", "2"],
    )
    .as_deref()
    .and_then(macos_primary_mac);
    let host = command_output("/usr/sbin/scutil", &["--get", "ComputerName"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| command_output("hostname", &[]).map(|s| s.trim().to_string()));
    Sources {
        desktop_id: hw.clone(),
        hw,
        inst: None,
        nic,
        host,
        os: "macos",
    }
}

#[cfg(target_os = "linux")]
fn read_sources() -> Sources {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let inst = read("/etc/machine-id")
        .as_deref()
        .and_then(machine_id_file)
        .or_else(|| {
            read("/var/lib/dbus/machine-id")
                .as_deref()
                .and_then(machine_id_file)
        });
    let hw = read("/sys/class/dmi/id/product_uuid");
    let mut interfaces = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let path = entry.path();
            let value = |name: &str| std::fs::read_to_string(path.join(name)).ok();
            interfaces.push(LinuxInterface {
                has_device: path.join("device").exists(),
                kind: value("type")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0),
                addr_assign_type: value("addr_assign_type")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(1),
                address: value("address").unwrap_or_default(),
                wireless: path.join("wireless").exists() || path.join("phy80211").exists(),
            });
        }
    }
    let host = read("/proc/sys/kernel/hostname").map(|s| s.trim().to_string());
    Sources {
        desktop_id: inst.clone(),
        hw,
        inst,
        nic: linux_primary_mac(&interfaces),
        host,
        os: "linux",
    }
}

#[cfg(windows)]
fn read_sources() -> Sources {
    let inst = command_output(
        "reg",
        &[
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
            "/reg:64",
        ],
    )
    .and_then(|out| {
        out.lines()
            .find(|line| line.contains("MachineGuid"))
            .and_then(|line| line.split_whitespace().last())
            .map(str::to_string)
    });
    Sources {
        desktop_id: inst.clone(),
        hw: windows::smbios_uuid(),
        inst,
        nic: windows::adapters().as_deref().and_then(windows_primary_mac),
        host: std::env::var("COMPUTERNAME").ok(),
        os: "windows",
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn read_sources() -> Sources {
    Sources::default()
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_TABLE2,
    };
    use windows_sys::Win32::System::SystemInformation::GetSystemFirmwareTable;

    /// 'RSMB', the raw SMBIOS provider.
    const RSMB: u32 = u32::from_be_bytes(*b"RSMB");

    pub(super) fn smbios_uuid() -> Option<String> {
        // SAFETY: a null buffer of size 0 asks for the size; the second call
        // writes at most `size` bytes into a buffer of that size.
        unsafe {
            let size = GetSystemFirmwareTable(RSMB, 0, std::ptr::null_mut(), 0);
            if size == 0 {
                return None;
            }
            let mut buffer = vec![0u8; size as usize];
            let written = GetSystemFirmwareTable(RSMB, 0, buffer.as_mut_ptr(), size);
            if written == 0 || written > size {
                return None;
            }
            buffer.truncate(written as usize);
            super::smbios_system_uuid(&buffer)
        }
    }

    pub(super) fn adapters() -> Option<Vec<super::WindowsAdapter>> {
        // SAFETY: GetIfTable2 allocates the table; it is read within
        // NumEntries and freed with FreeMibTable.
        unsafe {
            let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if GetIfTable2(&mut table) != 0 || table.is_null() {
                return None;
            }
            let count = (*table).NumEntries as usize;
            let first = std::ptr::addr_of!((*table).Table)
                .cast::<windows_sys::Win32::NetworkManagement::IpHelper::MIB_IF_ROW2>();
            let rows = std::slice::from_raw_parts(first, count);
            let adapters = rows
                .iter()
                .map(|row| {
                    let length = (row.PhysicalAddressLength as usize)
                        .min(row.PermanentPhysicalAddress.len());
                    let description_end = row
                        .Description
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(row.Description.len());
                    super::WindowsAdapter {
                        permanent_mac: row.PermanentPhysicalAddress[..length].to_vec(),
                        if_type: row.Type,
                        // Bit 0 of the flags byte: HardwareInterface.
                        hardware: row.InterfaceAndOperStatusFlags._bitfield & 0x01 != 0,
                        description: String::from_utf16_lossy(&row.Description[..description_end]),
                    }
                })
                .collect();
            FreeMibTable(table as *const core::ffi::c_void);
            Some(adapters)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "4C4C4544-0042-3510-8051-B4C04F4E3132";

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn ids_are_normalized_and_placeholders_dropped() {
        assert_eq!(
            normalize_id(UUID).as_deref(),
            Some("4c4c4544004235108051b4c04f4e3132")
        );
        assert_eq!(
            normalize_id("{4c4c4544-0042-3510-8051-b4c04f4e3132}\r\n"),
            normalize_id(UUID)
        );
        assert_eq!(normalize_id("00000000-0000-0000-0000-000000000000"), None);
        assert_eq!(normalize_id("FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"), None);
        assert_eq!(normalize_id("03000200-0400-0500-0006-000700080009"), None);
        assert_eq!(normalize_id("not a uuid"), None);
        assert_eq!(
            normalize_id("0123456789abcdef0123456789abcdef").as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn only_burned_in_global_macs_count() {
        assert_eq!(
            normalize_mac(&[0x3c, 0x22, 0xfb, 0x01, 0x02, 0x03]).as_deref(),
            Some("3c22fb010203")
        );
        assert_eq!(
            normalize_mac(&[0x3e, 0x22, 0xfb, 0x01, 0x02, 0x03]),
            None,
            "locally administered (random Wi-Fi, VPN)"
        );
        assert_eq!(
            normalize_mac(&[0x01, 0x00, 0x5e, 0, 0, 1]),
            None,
            "multicast"
        );
        assert_eq!(normalize_mac(&[0; 6]), None);
        assert_eq!(normalize_mac(&[0xff; 6]), None);
        assert_eq!(
            parse_mac_text("3C-22-FB-01-02-03\n").as_deref(),
            Some(&[0x3c, 0x22, 0xfb, 1, 2, 3][..])
        );
    }

    #[test]
    fn the_header_carries_keyed_components_only() {
        let sources = Sources {
            hw: Some(UUID.to_string()),
            inst: Some("0123456789abcdef0123456789abcdef".to_string()),
            nic: Some("3c22fb010203".to_string()),
            desktop_id: Some(UUID.to_string()),
            host: Some("Chad 的 MacBook Pro".to_string()),
            os: "macos",
        };
        let value = sources.header_value("0.8.0").unwrap();
        assert!(value.starts_with("v1;hw="), "{value}");
        assert!(
            !value.to_lowercase().contains("4c4c4544"),
            "raw ids never leave the computer"
        );
        assert!(!value.contains("3c22fb010203"));
        let fields: std::collections::HashMap<&str, &str> = value
            .split(';')
            .skip(1)
            .filter_map(|f| f.split_once('='))
            .collect();
        for name in ["hw", "inst", "nic"] {
            assert_eq!(fields[name].len(), 43, "{name}");
        }
        // The component name is part of the key: equal raw values differ.
        assert_ne!(component("hw", "x"), component("inst", "x"));
        // dt is the desktop's DT-D2 hash of the same id (the Aogiri test vector).
        assert_eq!(fields["dt"], desktop_hash_of(UUID).unwrap());
        assert_eq!(fields["os"], "macos");
        assert_eq!(fields["cli"], "0.8.0");
        let host = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(fields["host"])
            .unwrap();
        assert_eq!(String::from_utf8(host).unwrap(), "Chad 的 MacBook Pro");
        // Stable: the same sources give the same value.
        assert_eq!(sources.header_value("0.8.0").unwrap(), value);
        // Nothing readable: no header.
        let empty = Sources {
            os: "linux",
            host: Some("h".into()),
            ..Sources::default()
        };
        assert_eq!(empty.header_value("0.8.0"), None);
        let placeholder = Sources {
            hw: Some("00000000-0000-0000-0000-000000000000".into()),
            os: "linux",
            ..Sources::default()
        };
        assert_eq!(placeholder.header_value("0.8.0"), None);
    }

    #[test]
    fn only_the_monoize_provider_gets_the_header() {
        assert!(gateway_headers("openai").is_empty());
        assert!(gateway_headers("lynshen").is_empty());
        if let Some(value) = header_value() {
            assert_eq!(
                gateway_headers("monoize"),
                vec![(HEADER.to_string(), value.to_string())]
            );
        }
    }

    #[test]
    fn macos_sources_parse() {
        let platform = "+-o IOPlatformExpertDevice  <class IOPlatformExpertDevice>\n    {\n      \"IOPlatformSerialNumber\" = \"C02X\"\n      \"IOPlatformUUID\" = \"4C4C4544-0042-3510-8051-B4C04F4E3132\"\n    }\n";
        assert_eq!(macos_platform_uuid(platform).as_deref(), Some(UUID));
        let nics = "+-o AppleThunderboltIPPort  <class AppleThunderboltIPPort>\n  |   \"IOMACAddress\" = <3c22fb0000aa>\n  +-o en1  <class IOEthernetInterface>\n        \"BSD Name\" = \"en1\"\n        \"IOPrimaryInterface\" = No\n        \"IOBuiltin\" = Yes\n+-o IOSkywalkLegacyEthernet  <class IOSkywalkLegacyEthernet>\n  |   \"IOMACAddress\" = <3c22fb0000bb>\n  +-o en0  <class IOSkywalkLegacyEthernetInterface>\n        \"BSD Name\" = \"en0\"\n        \"IOPrimaryInterface\" = Yes\n        \"IOBuiltin\" = Yes\n+-o AppleUSBDeviceNCM11Data  <class AppleUSBDeviceNCM11Data>\n  |   \"IOMACAddress\" = <000000000001>\n  +-o en5  <class IOEthernetInterface>\n        \"BSD Name\" = \"en5\"\n        \"IOPrimaryInterface\" = No\n        \"IOBuiltin\" = No\n";
        assert_eq!(macos_primary_mac(nics).as_deref(), Some("3c22fb0000bb"));
        // Without a primary flag the built-in one with the lowest BSD name wins; a
        // USB adapter (not built in) never does.
        let docked = nics.replace(
            "\"IOPrimaryInterface\" = Yes",
            "\"IOPrimaryInterface\" = No",
        );
        assert_eq!(macos_primary_mac(&docked).as_deref(), Some("3c22fb0000bb"));
        let only_usb = "+-o X  <class X>\n  |   \"IOMACAddress\" = <3c22fb0000cc>\n  +-o en7  <class IOEthernetInterface>\n        \"BSD Name\" = \"en7\"\n        \"IOBuiltin\" = No\n";
        assert_eq!(macos_primary_mac(only_usb), None);
    }

    /// An SMBIOS 3.x table: a type-0 structure with one string, then type 1.
    fn smbios(major: u8, uuid: [u8; 16]) -> Vec<u8> {
        let mut table = vec![0u8, 4, 0, 0];
        table.extend_from_slice(b"BIOS\0\0");
        let mut system = vec![1u8, 0x1b, 1, 0];
        system.extend_from_slice(&[1, 2, 3, 4]);
        system.extend_from_slice(&uuid);
        system.extend_from_slice(&[6, 0, 0]);
        table.extend_from_slice(&system);
        table.extend_from_slice(b"\0\0");
        table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        let mut raw = vec![0, major, 0, 0];
        raw.extend_from_slice(&(table.len() as u32).to_le_bytes());
        raw.extend_from_slice(&table);
        raw
    }

    #[test]
    fn windows_smbios_uuid_follows_the_byte_order_of_the_version() {
        let bytes = [
            0x44, 0x45, 0x4c, 0x4c, 0x42, 0x00, 0x10, 0x35, 0x80, 0x51, 0xb4, 0xc0, 0x4f, 0x4e,
            0x31, 0x32,
        ];
        assert_eq!(smbios_system_uuid(&smbios(3, bytes)), normalize_id(UUID));
        assert_eq!(
            smbios_system_uuid(&smbios(2, bytes)).as_deref(),
            Some("44454c4c420010358051b4c04f4e3132")
        );
        assert_eq!(smbios_system_uuid(&smbios(3, [0; 16])), None);
        assert_eq!(smbios_system_uuid(&[0, 3, 0]), None);
        assert_eq!(smbios_system_uuid(&smbios(3, bytes)[..20]), None);
    }

    #[test]
    fn windows_primary_mac_skips_virtual_adapters() {
        let adapter =
            |mac: [u8; 6], if_type: u32, hardware: bool, description: &str| WindowsAdapter {
                permanent_mac: mac.to_vec(),
                if_type,
                hardware,
                description: description.to_string(),
            };
        let adapters = vec![
            adapter(
                [0x00, 0x15, 0x5d, 1, 1, 1],
                6,
                true,
                "Hyper-V Virtual Ethernet Adapter",
            ),
            adapter(
                [0x00, 0xff, 0x01, 2, 2, 2],
                6,
                true,
                "TAP-Windows Adapter V9",
            ),
            adapter([0x10, 0x00, 0x00, 3, 3, 3], 6, false, "WAN Miniport"),
            adapter(
                [0x50, 0x00, 0x00, 4, 4, 4],
                6,
                true,
                "Intel(R) Ethernet Connection",
            ),
            adapter(
                [0x40, 0x00, 0x00, 5, 5, 5],
                71,
                true,
                "Intel(R) Wi-Fi 6 AX201",
            ),
        ];
        assert_eq!(
            windows_primary_mac(&adapters).as_deref(),
            Some("400000050505")
        );
        assert_eq!(
            windows_primary_mac(&adapters[..4]).as_deref(),
            Some("500000040404")
        );
        assert_eq!(windows_primary_mac(&adapters[..3]), None);
    }

    #[test]
    fn linux_sources_parse() {
        let iface = |_name: &str,
                     has_device: bool,
                     kind: u32,
                     assign: u32,
                     address: &str,
                     wireless: bool| LinuxInterface {
            has_device,
            kind,
            addr_assign_type: assign,
            address: address.to_string(),
            wireless,
        };
        let interfaces = vec![
            iface("lo", false, 772, 0, "00:00:00:00:00:00", false),
            iface("docker0", false, 1, 3, "02:42:ac:11:00:01", false),
            iface("enp3s0", true, 1, 0, "50:00:00:04:04:04\n", false),
            iface("wlp2s0", true, 1, 0, "40:00:00:05:05:05\n", true),
            iface("wlx-random", true, 1, 1, "30:00:00:06:06:06", true),
        ];
        assert_eq!(
            linux_primary_mac(&interfaces).as_deref(),
            Some("400000050505")
        );
        assert_eq!(
            linux_primary_mac(&interfaces[..3]).as_deref(),
            Some("500000040404")
        );
        assert_eq!(linux_primary_mac(&interfaces[..2]), None);
        assert_eq!(
            machine_id_file("0123456789abcdef0123456789abcdef\n").as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(machine_id_file("uninitialized\n"), None);
        assert_eq!(machine_id_file(""), None);
    }

    #[test]
    fn this_computer_has_a_stable_value() {
        let first = read_sources().header_value("0");
        assert_eq!(first, read_sources().header_value("0"));
    }
}

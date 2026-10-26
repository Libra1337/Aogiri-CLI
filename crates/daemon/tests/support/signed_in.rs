//! The test HOME signed in to LynShen as a computer (relay-protocol.md §7):
//! a device key in auth.json and a gateway URL nothing answers on, so no
//! test ever reaches a real gateway. LAN pairing needs the sign-in.

use serde_json::{json, Value};
use std::{env, fs, path::PathBuf};

/// The device key the test HOME holds while signed in.
pub const TEST_DEVICE_KEY: &str = "sk-test-computer-device-key";

fn profile() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap()).join(".lynshen")
}

fn edit(file: &str, change: impl FnOnce(&mut Value)) {
    let path = profile().join(file);
    let mut value: Value = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    change(&mut value);
    fs::write(&path, value.to_string()).unwrap();
}

/// Signs the test HOME in (idempotent). Call after `setup()`.
pub fn sign_in() {
    edit("config.json", |config| {
        config["lynshen_web_url"] = json!("http://127.0.0.1:9");
    });
    edit("auth.json", |auth| {
        auth["providers"]["lynshen-cli"] = json!(TEST_DEVICE_KEY);
    });
}

/// Signs the test HOME out.
#[allow(dead_code)]
pub fn sign_out() {
    edit("auth.json", |auth| {
        if let Some(providers) = auth["providers"].as_object_mut() {
            providers.remove("lynshen-cli");
        }
    });
}

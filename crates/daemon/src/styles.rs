//! Reply styles for the desktop: list the styles a LynShen engine finds,
//! write and delete the personal ones (see `lynshen_agent_core::styles`).
//! Sessions read the files at each turn, so nothing is sent to them. A
//! failure is an error reply with a stable `code` (`StyleError`), which the
//! desktop words in its own language; the English detail goes to the log
//! and the reply's `message`.

use lynshen_agent_core::styles::{self, StyleError};
use serde_json::{json, Value};
use std::path::Path;

pub fn handle(name: &str, op: &Value) -> Result<Value, String> {
    let result = match name {
        // The project whose own styles to include: an absolute folder.
        "styles_list" => styles::list_json(
            op["cwd"]
                .as_str()
                .map(Path::new)
                .filter(|cwd| cwd.is_absolute()),
        ),
        // The style's id travels as `style`: `id` is a client's request id.
        "style_save" => save(op),
        "style_delete" => delete(op),
        _ => return Err(format!("unknown op {name}")),
    };
    Ok(result.unwrap_or_else(|error| {
        lynshen_agent_core::log_warn!(
            "daemon",
            "reply style op failed",
            op = name,
            code = error.code,
            detail = error.detail.as_str()
        );
        error.reply()
    }))
}

fn save(op: &Value) -> Result<Value, StyleError> {
    let text = |key: &str| op[key].as_str().unwrap_or_default();
    let id = op["style"].as_str().filter(|id| !id.trim().is_empty());
    styles::save(
        &profile()?,
        id,
        text("name"),
        text("description"),
        text("instructions"),
    )
    .map(|(id, path)| json!({ "type": "style_saved", "style": id, "path": path }))
}

fn delete(op: &Value) -> Result<Value, StyleError> {
    let id = op["style"].as_str().unwrap_or_default().trim();
    styles::delete(&profile()?, id).map(|()| json!({ "type": "style_deleted", "style": id }))
}

fn profile() -> Result<std::path::PathBuf, StyleError> {
    styles::profile_dir().map_err(|error| StyleError {
        code: "read_failed",
        detail: error.to_string(),
    })
}

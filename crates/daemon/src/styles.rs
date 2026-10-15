//! Reply styles for the desktop: list the styles a LynShen engine finds,
//! write and delete the personal ones (see `lynshen_agent_core::styles`).
//! Sessions read the files at each turn, so nothing is sent to them.

use lynshen_agent_core::styles;
use serde_json::{json, Value};
use std::path::Path;

pub fn handle(name: &str, op: &Value) -> Result<Value, String> {
    let text = |key: &str| op[key].as_str().unwrap_or_default();
    match name {
        // The project whose own styles to include: an absolute folder.
        "styles_list" => styles::list_json(
            op["cwd"]
                .as_str()
                .map(Path::new)
                .filter(|cwd| cwd.is_absolute()),
        ),
        // The style's id travels as `style`: `id` is a client's request id.
        "style_save" => {
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
        "style_delete" => {
            let id = op["style"]
                .as_str()
                .filter(|id| !id.trim().is_empty())
                .ok_or("style_delete requires style")?;
            styles::delete(&profile()?, id)
                .map(|()| json!({ "type": "style_deleted", "style": id.trim() }))
        }
        _ => Err(format!("unknown op {name}")),
    }
}

fn profile() -> Result<std::path::PathBuf, String> {
    lynshen_agent_core::styles::profile_dir().map_err(|error| error.to_string())
}

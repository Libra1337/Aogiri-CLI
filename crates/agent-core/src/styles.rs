//! Reply styles: how the model writes to the user (tone, length, structure,
//! how much it explains), chosen by the user and never a change to what the
//! engine does. Built-in presets, plus Markdown files with a frontmatter
//! `name` and `description` in `~/.lynshen/styles/` and, in a trusted
//! project, `.lynshen/styles/`. The file name without `.md` is the style's
//! id; for one id a project file wins over a user file, which wins over a
//! built-in. `default` is no style at all.
//!
//! The style is the system prompt's last section (`prompt_section`), after
//! the stable prefix the provider caches across sessions. A change during a
//! conversation reaches the model as a note (`prompt::Note::Style`), so the
//! kept prompt stays byte-identical. See `docs/serve-protocol.md` → "Reply
//! styles".

use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// The id that means no style: the engine's own prompt as it is.
pub const DEFAULT_ID: &str = "default";

/// The longest style body a prompt carries; a longer one is cut there.
pub const MAX_BYTES: usize = 8 * 1024;

const MAX_ID_CHARS: usize = 64;
const MAX_NAME_CHARS: usize = 40;
const MAX_DESCRIPTION_CHARS: usize = 200;

/// What the section tells the model before the style's own text: what a
/// style may change, and what it never changes.
const FRAMING: &str = "Reply style: the user chose how your messages to them should read. Follow it for tone, length, structure and how much you explain; where it differs from the reply guidance above (for example \"Be concise\"), the style wins. It changes nothing else: the rules on safety, tools, approvals, the sandbox, how you do and verify the work, and the project's instructions still apply. Code, comments, commit messages and files follow the project's conventions, not this style. It applies to what you write to the user, not to tool input, summaries or messages to other agents. If the user's message asks for something else (more detail, another format), do what the message asks. Write in the language the user writes in; the style's own language does not set it.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleSource {
    Builtin,
    User,
    Project,
}

impl StyleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Style {
    pub id: String,
    pub name: String,
    pub description: String,
    /// The text the prompt carries: the file's body, at most `MAX_BYTES`.
    pub instructions: String,
    pub source: StyleSource,
    /// The file; None for a built-in.
    pub path: Option<PathBuf>,
    /// The body was longer than `MAX_BYTES` and was cut.
    pub truncated: bool,
}

/// Why a style file does not load, or why saving, deleting or listing
/// styles failed: a stable `code` that clients word in their own language,
/// and the English `detail` for logs.
///
/// - Loading a file: `reserved_id` (`default.md`), `invalid_id` (the file
///   name), `no_instructions`, `read_failed` (the file or its folder).
/// - Saving: `name_missing`, `name_too_long` (also more than one line),
///   `description_too_long` (also more than one line),
///   `instructions_missing`, `instructions_too_long`, `reserved_id`,
///   `invalid_id`, `write_failed`.
/// - Deleting: `reserved_id`, `invalid_id`, `not_found`, `delete_failed`,
///   `config_failed` (the file is gone, but config.json's `reply_style`
///   still names it).
/// - Listing: `read_failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleError {
    pub code: &'static str,
    pub detail: String,
}

impl StyleError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// The daemon's error reply: the code, and the detail as `message`.
    pub fn reply(&self) -> Value {
        json!({ "type": "error", "code": self.code, "message": self.detail })
    }
}

impl std::fmt::Display for StyleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// A file in a styles folder that does not load, and why (`StyleError`'s
/// loading codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleProblem {
    pub path: PathBuf,
    pub error: StyleError,
}

#[derive(Debug, Clone, Default)]
pub struct StyleInventory {
    /// The styles in effect, one per id: built-ins first in their order,
    /// then the others by name.
    pub styles: Vec<Style>,
    pub problems: Vec<StyleProblem>,
}

impl StyleInventory {
    pub fn get(&self, id: &str) -> Option<&Style> {
        self.styles.iter().find(|style| style.id == id)
    }
}

struct Builtin {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    instructions: &'static str,
}

const BUILTINS: [Builtin; 5] = [
    Builtin {
        id: DEFAULT_ID,
        name: "默认",
        description: "不加风格，按 LynShen 默认的方式回复",
        instructions: "",
    },
    Builtin {
        id: "concise",
        name: "简洁",
        description: "先给结论，省掉铺垫和复述",
        instructions: "- 第一句给出结论或结果：做了什么，答案是什么。
- 不写开场白，不逐步复述过程，不写结尾总结。
- 简单的问题用 1 到 3 句回答。
- 只在信息确实成组时用列表，不加装饰性的标题。
- 以下内容保持完整：错误和失败的检查结果、风险和安全提醒、破坏性操作前的确认、用户要求解释的内容。",
    },
    Builtin {
        id: "explanatory",
        name: "详细讲解",
        description: "说明做法、关键决定和取舍",
        instructions: "- 说明做了什么，也说明为什么这样做：采用的方法、关键的决定、放弃的方案和原因。
- 改完代码后，在回复里加一小段“要点”，写两三条和这次改动或这段代码有关的说明，例如它依赖的约定、容易出错的地方。
- 新手可能不懂的术语第一次出现时，用一句话解释。
- 讲解写在回复里，不写进代码注释。
- 按“结论、过程、原因”的顺序组织；较长的回复用小标题分段。",
    },
    Builtin {
        id: "learning",
        name: "教学引导",
        description: "边做边讲概念，邀请你写一小段",
        instructions: "- 把用户当作正在学习的人：边做边讲背后的概念和原理，说明为什么选这种做法。
- 遇到适合练手的一小段代码（几行，有真实的设计取舍，例如错误处理或数据结构的选择）时，先问用户想不想自己写。用户同意后，在文件里留一个 TODO(human) 标记，并说明已经完成了什么、要写什么、要考虑什么。用户没有同意时，自己写完。
- 用户写完后，先对这段代码给一条具体的反馈，再继续任务。
- 有帮助时，在回复末尾提一个简短的问题，帮用户检查自己是否理解。
- 不要为了教学停下必要的工作，任务仍然要完成。",
    },
    Builtin {
        id: "formal",
        name: "正式汇报",
        description: "结论、过程、验证、风险的正式报告",
        instructions: "- 使用正式的书面语气，不用表情符号、口语和网络用语。
- 较完整的回复按这个顺序组织：结论、背景、所做的工作、验证、风险与后续。没有内容的部分省略。
- 用事实和数字说明，少用形容词；不确定的地方明确写出。
- 简单的问题直接给出简短、正式的答复，不必套用整个结构。
- 写成可以直接转发给上级或同事的样子。",
    },
];

/// The built-in presets, `default` first.
pub fn builtins() -> Vec<Style> {
    BUILTINS
        .iter()
        .map(|builtin| Style {
            id: builtin.id.to_string(),
            name: builtin.name.to_string(),
            description: builtin.description.to_string(),
            instructions: builtin.instructions.to_string(),
            source: StyleSource::Builtin,
            path: None,
            truncated: false,
        })
        .collect()
}

/// Whether `id` can name a style file: not empty, at most 64 characters,
/// no white space, control characters or characters a file name cannot
/// hold on Windows, and not starting with `.` or `-` (hidden files, and
/// what `/style` reads as a flag).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && !id.starts_with(['.', '-'])
        && !id.chars().any(|ch| {
            ch.is_whitespace()
                || ch.is_control()
                || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
}

/// The LynShen profile folder (`~/.lynshen`).
pub fn profile_dir() -> io::Result<PathBuf> {
    crate::config::profile_dir()
}

/// The personal styles folder of a profile.
pub fn user_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("styles")
}

/// A project's styles folder.
pub fn project_dir(cwd: &Path) -> PathBuf {
    cwd.join(".lynshen").join("styles")
}

/// The folders styles load from, in the order they are read: the profile's,
/// then the project's when `project` (a trusted project's directory) is
/// given.
pub fn style_dirs(profile_dir: &Path, project: Option<&Path>) -> Vec<(StyleSource, PathBuf)> {
    let mut dirs = vec![(StyleSource::User, user_dir(profile_dir))];
    if let Some(cwd) = project {
        dirs.push((StyleSource::Project, project_dir(cwd)));
    }
    dirs
}

/// Every style in effect: the built-ins, then the files of `style_dirs`,
/// each replacing an earlier style of the same id. `project` is the
/// directory of a trusted project, None for none (or not trusted).
pub fn inventory(profile_dir: &Path, project: Option<&Path>) -> StyleInventory {
    let mut inventory = StyleInventory {
        styles: builtins(),
        problems: Vec::new(),
    };
    for (source, dir) in style_dirs(profile_dir, project) {
        for style in read_dir(&dir, source, &mut inventory.problems) {
            match inventory
                .styles
                .iter_mut()
                .find(|known| known.id == style.id)
            {
                Some(known) => *known = style,
                None => inventory.styles.push(style),
            }
        }
    }
    let builtin_rank = |id: &str| BUILTINS.iter().position(|builtin| builtin.id == id);
    inventory.styles.sort_by(|left, right| {
        match (
            builtin_rank(left.id.as_str()),
            builtin_rank(right.id.as_str()),
        ) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left
                .name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id)),
        }
    });
    inventory
}

/// The `.md` files of one folder that load, sorted by id; the others become
/// problems. A missing folder has none.
fn read_dir(dir: &Path, source: StyleSource, problems: &mut Vec<StyleProblem>) -> Vec<Style> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            problems.push(StyleProblem {
                path: dir.to_path_buf(),
                error: StyleError::new("read_failed", format!("cannot read the folder: {error}")),
            });
            return Vec::new();
        }
    };
    let mut files = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("md")
                && !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with('.'))
                && path.is_file()
        })
        .collect::<Vec<_>>();
    files.sort();
    let mut styles = Vec::new();
    for path in files {
        match read_file(&path, source) {
            Ok(style) => styles.push(style),
            Err(error) => problems.push(StyleProblem { path, error }),
        }
    }
    styles
}

/// One style file, or why it does not load.
fn read_file(path: &Path, source: StyleSource) -> Result<Style, StyleError> {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    if id == DEFAULT_ID {
        return Err(StyleError::new(
            "reserved_id",
            "\"default\" is reserved for no style; rename the file",
        ));
    }
    if !valid_id(&id) {
        return Err(StyleError::new("invalid_id", "the file name cannot be a style id: use no spaces or / \\ : * ? \" < > |, and do not start it with . or -"));
    }
    let content = fs::read_to_string(path)
        .map_err(|error| StyleError::new("read_failed", format!("cannot read it: {error}")))?;
    let content = content.replace("\r\n", "\n");
    let field = |key| crate::prompt::read_frontmatter_field(&content, key).unwrap_or_default();
    let body = frontmatter_body(&content).trim();
    if body.is_empty() {
        return Err(StyleError::new(
            "no_instructions",
            "no instructions after the frontmatter",
        ));
    }
    let (instructions, truncated) = cut(body, MAX_BYTES);
    let name = field("name");
    Ok(Style {
        name: if name.trim().is_empty() {
            id.clone()
        } else {
            name.trim().to_string()
        },
        id,
        description: field("description").trim().to_string(),
        instructions: instructions.to_string(),
        source,
        path: Some(path.to_path_buf()),
        truncated,
    })
}

/// The text after a leading `---` frontmatter block, or all of it.
fn frontmatter_body(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            return &rest[offset..];
        }
    }
    // Never closed: no frontmatter after all.
    content
}

/// `text` cut to at most `max` bytes on a character boundary.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// The system prompt's reply-style section for `style`, starting with the
/// blank line that separates it from the text before; empty for `default`.
pub fn prompt_section(style: &Style) -> String {
    if style.id == DEFAULT_ID || style.instructions.trim().is_empty() {
        return String::new();
    }
    format!(
        "\n\n<reply_style id=\"{}\" name=\"{}\">\n{FRAMING}\n\n{}\n</reply_style>",
        crate::prompt::escape_xml(&style.id),
        crate::prompt::escape_xml(&style.name),
        style.instructions.trim()
    )
}

/// The style a session uses now: its own choice (`chosen`), else the
/// default for new conversations (`reply_style` in config.json).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The id asked for: the session's, else the default's.
    pub requested: String,
    /// The session chose it (`set_style`), rather than following the default.
    pub chosen: bool,
    /// The style, None when `requested` names none (`missing`) or is `default`.
    pub style: Option<Style>,
}

impl Resolved {
    /// `requested` names no style now: the session runs with none.
    pub fn missing(&self) -> bool {
        self.style.is_none() && self.requested != DEFAULT_ID
    }

    /// The prompt section (see `prompt_section`); empty without a style.
    pub fn section(&self) -> String {
        self.style.as_ref().map(prompt_section).unwrap_or_default()
    }

    pub fn view(&self) -> crate::event::ReplyStyleView {
        let (name, source) = match &self.style {
            Some(style) => (style.name.clone(), style.source.as_str()),
            None if self.missing() => (self.requested.clone(), "missing"),
            None => (BUILTINS[0].name.to_string(), StyleSource::Builtin.as_str()),
        };
        crate::event::ReplyStyleView {
            style: self.requested.clone(),
            name,
            source,
            chosen: self.chosen,
            missing: self.missing(),
        }
    }
}

/// An id as config.json or an op gives it: empty means `default`.
pub fn normalize_id(id: &str) -> &str {
    match id.trim() {
        "" => DEFAULT_ID,
        id => id,
    }
}

/// The style for a session that chose `chosen` (None: it follows
/// `default_id`, config.json's `reply_style`).
pub fn resolve(inventory: &StyleInventory, chosen: Option<&str>, default_id: &str) -> Resolved {
    let requested = normalize_id(chosen.unwrap_or(default_id)).to_string();
    let style = if requested == DEFAULT_ID {
        None
    } else {
        inventory.get(&requested).cloned()
    };
    Resolved {
        requested,
        chosen: chosen.is_some(),
        style,
    }
}

/// An id for a new style named `name`: its ASCII letters and digits in
/// lower case joined by `-`, else `style`, with `-2`, `-3`… added while a
/// style of that id exists in `inventory`.
pub fn id_for_name(name: &str, inventory: &StyleInventory) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let base: String = if slug.is_empty() {
        "style".to_string()
    } else {
        slug.chars().take(MAX_ID_CHARS - 4).collect()
    };
    let taken = |id: &str| id == DEFAULT_ID || inventory.get(id).is_some();
    if !taken(base.as_str()) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken(id.as_str()))
        .expect("an unused id")
}

/// The file text of a style: frontmatter, then the instructions. A value
/// is written as it is (the frontmatter reader takes the rest of the line),
/// in double quotes when it would otherwise read as a quoted value or a
/// block scalar.
fn file_text(name: &str, description: &str, instructions: &str) -> String {
    let value = |text: &str| {
        if text.starts_with(['"', '\'', '>', '|']) || text.ends_with(['"', '\'']) {
            format!("\"{text}\"")
        } else {
            text.to_string()
        }
    };
    let mut text = format!("---\nname: {}\n", value(name));
    if !description.is_empty() {
        text.push_str(&format!("description: {}\n", value(description)));
    }
    text.push_str("---\n\n");
    text.push_str(instructions.trim());
    text.push('\n');
    text
}

/// Writes a personal style: `id` replaces that file, None makes a new id
/// from `name`. Returns the id and the file.
pub fn save(
    profile_dir: &Path,
    id: Option<&str>,
    name: &str,
    description: &str,
    instructions: &str,
) -> Result<(String, PathBuf), StyleError> {
    let name = name.trim();
    let description = description.trim();
    let instructions = instructions.trim();
    if name.is_empty() {
        return Err(StyleError::new(
            "name_missing",
            "a reply style needs a name",
        ));
    }
    if name.chars().count() > MAX_NAME_CHARS || name.contains('\n') {
        return Err(StyleError::new(
            "name_too_long",
            format!("a style name is one line of at most {MAX_NAME_CHARS} characters"),
        ));
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS || description.contains('\n') {
        return Err(StyleError::new(
            "description_too_long",
            format!(
                "a style description is one line of at most {MAX_DESCRIPTION_CHARS} characters"
            ),
        ));
    }
    if instructions.is_empty() {
        return Err(StyleError::new(
            "instructions_missing",
            "a reply style needs instructions",
        ));
    }
    if instructions.len() > MAX_BYTES {
        return Err(StyleError::new(
            "instructions_too_long",
            format!("the instructions are longer than {} KB", MAX_BYTES / 1024),
        ));
    }
    let id = match id.map(str::trim) {
        Some(DEFAULT_ID) => {
            return Err(StyleError::new(
                "reserved_id",
                "\"default\" is reserved for no style",
            ))
        }
        Some(id) if !valid_id(id) => {
            return Err(StyleError::new(
                "invalid_id",
                format!("not a valid style id: {id}"),
            ))
        }
        Some(id) => id.to_string(),
        None => id_for_name(name, &inventory(profile_dir, None)),
    };
    let dir = user_dir(profile_dir);
    let written = |error: io::Error, path: &Path| {
        StyleError::new(
            "write_failed",
            format!("cannot write {}: {error}", path.display()),
        )
    };
    fs::create_dir_all(&dir).map_err(|error| written(error, &dir))?;
    let path = dir.join(format!("{id}.md"));
    crate::config::write_atomically(&path, &file_text(name, description, instructions), None)
        .map_err(|error| written(error, &path))?;
    Ok((id, path))
}

/// Deletes a personal style file. When config.json's `reply_style` names it,
/// new conversations go back to `default`.
pub fn delete(profile_dir: &Path, id: &str) -> Result<(), StyleError> {
    let id = id.trim();
    if id == DEFAULT_ID {
        return Err(StyleError::new(
            "reserved_id",
            "\"default\" is no style file",
        ));
    }
    if !valid_id(id) {
        return Err(StyleError::new(
            "invalid_id",
            format!("not a valid style id: {id}"),
        ));
    }
    let path = user_dir(profile_dir).join(format!("{id}.md"));
    fs::remove_file(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => StyleError::new(
            "not_found",
            format!(
                "no personal style {id} in {}",
                user_dir(profile_dir).display()
            ),
        ),
        _ => StyleError::new(
            "delete_failed",
            format!("cannot delete {}: {error}", path.display()),
        ),
    })?;
    let config = profile_dir.join("config.json");
    if !config.exists() {
        return Ok(());
    }
    crate::config::edit_config_json(&config, |config| {
        if config.get("reply_style").and_then(Value::as_str) == Some(id) {
            config.insert("reply_style".to_string(), json!(DEFAULT_ID));
        }
        Ok(())
    })
    .map_err(|error| StyleError::new("config_failed", error))
}

/// config.json's `reply_style` in `profile_dir`, `default` when unset or
/// unreadable.
pub fn default_id(profile_dir: &Path) -> String {
    fs::read_to_string(profile_dir.join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|config| config.get("reply_style")?.as_str().map(str::to_string))
        .map(|id| normalize_id(&id).to_string())
        .unwrap_or_else(|| DEFAULT_ID.to_string())
}

pub fn style_json(style: &Style) -> Value {
    json!({
        "id": style.id,
        "name": style.name,
        "description": style.description,
        "instructions": style.instructions,
        "source": style.source.as_str(),
        "path": style.path,
        "truncated": style.truncated,
    })
}

/// The daemon's `styles_list` reply for this profile: every style in effect
/// (with `cwd`'s project styles once it is trusted), the default for new
/// conversations, the folders and what does not load (each also logged,
/// with its English detail).
pub fn list_json(cwd: Option<&Path>) -> Result<Value, StyleError> {
    let unreadable = |error: io::Error| StyleError::new("read_failed", error.to_string());
    let profile = crate::config::profile_dir().map_err(unreadable)?;
    let trusted = match cwd {
        Some(cwd) if crate::trust::project_has_local_resources(cwd) => Some(
            crate::trust::TrustStore::load_or_create()
                .map_err(unreadable)?
                .decision_for(cwd)
                .unwrap_or(false),
        ),
        Some(_) => Some(true),
        None => None,
    };
    let project = cwd.filter(|_| trusted == Some(true));
    Ok(list_value(&profile, cwd, project, trusted))
}

fn list_value(
    profile: &Path,
    cwd: Option<&Path>,
    project: Option<&Path>,
    trusted: Option<bool>,
) -> Value {
    let inventory = inventory(profile, project);
    for problem in &inventory.problems {
        crate::log_warn!(
            "styles",
            "a reply style does not load",
            path = problem.path.display().to_string(),
            code = problem.error.code,
            detail = problem.error.detail.as_str()
        );
    }
    let mut dirs = vec![json!({ "scope": "user", "path": user_dir(profile) })];
    if let Some(cwd) = cwd {
        dirs.push(json!({ "scope": "project", "path": project_dir(cwd) }));
    }
    json!({
        "type": "styles_list",
        "styles": inventory.styles.iter().map(style_json).collect::<Vec<_>>(),
        "default": default_id(profile),
        "dirs": dirs,
        "projectTrusted": trusted,
        "problems": inventory.problems.iter().map(|problem| json!({
            "path": problem.path,
            "code": problem.error.code,
            "message": problem.error.detail,
        })).collect::<Vec<_>>(),
    })
}

/// The `/style` listing: each style with its id, the current one marked.
pub fn listing(inventory: &StyleInventory, current: &Resolved, default_id: &str) -> String {
    let mut text = String::new();
    let current_name = current.view().name;
    text.push_str(&format!(
        "reply style: {current_name} ({}{})",
        current.requested,
        if current.missing() {
            ", missing: no style applies"
        } else if current.chosen {
            ", this conversation"
        } else {
            ", the default"
        }
    ));
    let default_name = inventory
        .get(normalize_id(default_id))
        .map_or(default_id.to_string(), |style| style.name.clone());
    text.push_str(&format!(
        " · default for new conversations: {default_name} ({})\n",
        normalize_id(default_id)
    ));
    for style in &inventory.styles {
        let mark = if style.id == current.requested {
            "*"
        } else {
            " "
        };
        let source = match &style.path {
            Some(path) => format!(" [{}: {}]", style.source.as_str(), path.display()),
            None => String::new(),
        };
        let description = if style.description.is_empty() {
            String::new()
        } else {
            format!(" - {}", style.description)
        };
        text.push_str(&format!(
            "{mark} {:<12} {}{description}{source}\n",
            style.id, style.name
        ));
    }
    text.push_str("/style <id> sets this conversation's style; /style <id> --global also makes it the default for new conversations. Styles are Markdown files (frontmatter name and description, then the instructions) in ~/.lynshen/styles, and in .lynshen/styles of a trusted project.");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-styles-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, file: &str, text: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(file), text).unwrap();
    }

    fn ids(inventory: &StyleInventory) -> Vec<&str> {
        inventory
            .styles
            .iter()
            .map(|style| style.id.as_str())
            .collect()
    }

    #[test]
    fn the_builtins_come_first_with_default_empty() {
        let profile = temp("builtins");
        let inventory = inventory(&profile, None);
        assert_eq!(
            ids(&inventory),
            ["default", "concise", "explanatory", "learning", "formal"]
        );
        assert_eq!(inventory.styles[1].name, "简洁");
        assert!(inventory.styles[1..]
            .iter()
            .all(|style| !style.instructions.is_empty() && style.path.is_none()));
        assert_eq!(prompt_section(&inventory.styles[0]), "");
        assert!(inventory.problems.is_empty());
        let _ = fs::remove_dir_all(profile);
    }

    #[test]
    fn a_style_file_is_its_frontmatter_and_body() {
        let profile = temp("file");
        write(
            &user_dir(&profile),
            "terse.md",
            "---\r\nname: \"Terse 短\"\r\ndescription: >-\r\n  Short\r\n  answers\r\n---\r\n\r\nAnswer in one line.\r\n",
        );
        write(&user_dir(&profile), "plain.md", "No frontmatter here.\n");
        let inventory = inventory(&profile, None);
        let terse = inventory.get("terse").unwrap();
        assert_eq!(terse.name, "Terse 短");
        assert_eq!(terse.description, "Short answers");
        assert_eq!(terse.instructions, "Answer in one line.");
        assert_eq!(terse.source, StyleSource::User);
        let plain = inventory.get("plain").unwrap();
        assert_eq!(
            (plain.name.as_str(), plain.description.as_str()),
            ("plain", "")
        );
        assert_eq!(plain.instructions, "No frontmatter here.");
        // Built-ins first, then the files by name.
        assert_eq!(&ids(&inventory)[5..], ["plain", "terse"]);
        let _ = fs::remove_dir_all(profile);
    }

    #[test]
    fn a_project_file_replaces_a_user_file_which_replaces_a_builtin() {
        let profile = temp("precedence");
        let project = temp("precedence-project");
        write(
            &user_dir(&profile),
            "concise.md",
            "---\nname: Mine\n---\nuser\n",
        );
        write(&user_dir(&profile), "team.md", "user team\n");
        write(
            &project_dir(&project),
            "team.md",
            "---\nname: Team\n---\nproject team\n",
        );
        let untrusted = inventory(&profile, None);
        assert_eq!(untrusted.get("concise").unwrap().instructions, "user");
        assert_eq!(untrusted.get("team").unwrap().source, StyleSource::User);
        let trusted = inventory(&profile, Some(&project));
        let team = trusted.get("team").unwrap();
        assert_eq!(
            (team.source, team.instructions.as_str()),
            (StyleSource::Project, "project team")
        );
        // The replaced built-in keeps its place in the order.
        assert_eq!(ids(&trusted)[1], "concise");
        assert_eq!(trusted.styles.iter().filter(|s| s.id == "team").count(), 1);
        let _ = fs::remove_dir_all(profile);
        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn files_that_cannot_be_styles_are_problems() {
        let profile = temp("problems");
        let dir = user_dir(&profile);
        write(&dir, "default.md", "never\n");
        write(&dir, "two words.md", "spaced\n");
        write(&dir, "empty.md", "---\nname: Empty\n---\n\n");
        write(&dir, ".hidden.md", "hidden\n");
        write(&dir, "notes.txt", "not markdown\n");
        write(&dir, "中文风格.md", "可以用中文文件名\n");
        let inventory = inventory(&profile, None);
        let mut problems = inventory
            .problems
            .iter()
            .map(|problem| {
                problem
                    .path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        problems.sort();
        assert_eq!(problems, ["default.md", "empty.md", "two words.md"]);
        let mut codes = inventory
            .problems
            .iter()
            .map(|problem| problem.error.code)
            .collect::<Vec<_>>();
        codes.sort();
        assert_eq!(codes, ["invalid_id", "no_instructions", "reserved_id"]);
        assert_eq!(&ids(&inventory)[5..], ["中文风格"]);
        assert_eq!(
            inventory.get("default").unwrap().source,
            StyleSource::Builtin
        );
        let _ = fs::remove_dir_all(profile);
    }

    #[test]
    fn a_long_body_is_cut_on_a_character_boundary() {
        let profile = temp("long");
        let body = "长".repeat(MAX_BYTES);
        write(&user_dir(&profile), "long.md", &body);
        let inventory = inventory(&profile, None);
        let long = inventory.get("long").unwrap();
        assert!(long.truncated);
        assert!(long.instructions.len() <= MAX_BYTES && long.instructions.len() > MAX_BYTES - 3);
        assert!(long.instructions.chars().all(|ch| ch == '长'));
        let _ = fs::remove_dir_all(profile);
    }

    #[test]
    fn ids_keep_to_file_names_a_command_can_type() {
        for id in ["concise", "my_style-2", "中文", "A1"] {
            assert!(valid_id(id), "{id}");
        }
        for id in [
            "",
            "two words",
            "a/b",
            "a\\b",
            ".x",
            "-x",
            "x?",
            "x".repeat(65).as_str(),
        ] {
            assert!(!valid_id(id), "{id}");
        }
    }

    #[test]
    fn the_section_frames_the_style_and_keeps_every_rule() {
        let style = &builtins()[1];
        let section = prompt_section(style);
        assert!(section.starts_with("\n\n<reply_style id=\"concise\" name=\"简洁\">\nReply style:"));
        assert!(section.ends_with(&format!("{}\n</reply_style>", style.instructions)));
        assert!(section.contains("It changes nothing else"));
        assert!(section.contains("Write in the language the user writes in"));
        let quoted = Style {
            name: "a \"b\" <c>".to_string(),
            ..style.clone()
        };
        assert!(prompt_section(&quoted).contains("name=\"a &quot;b&quot; &lt;c&gt;\""));
    }

    #[test]
    fn a_session_uses_its_choice_else_the_default_and_survives_a_missing_style() {
        let profile = temp("resolve");
        let inventory = inventory(&profile, None);
        let follows = resolve(&inventory, None, "concise");
        assert_eq!(
            (follows.requested.as_str(), follows.chosen),
            ("concise", false)
        );
        assert!(follows.section().contains("简洁"));
        let chosen = resolve(&inventory, Some("formal"), "concise");
        assert!(chosen.chosen && chosen.section().contains("正式汇报"));
        let none = resolve(&inventory, Some("default"), "concise");
        assert_eq!((none.section(), none.missing()), (String::new(), false));
        assert_eq!(none.view().name, "默认");
        let gone = resolve(&inventory, None, "deleted");
        assert!(gone.missing() && gone.section().is_empty());
        let view = gone.view();
        assert_eq!(
            (view.style.as_str(), view.source, view.missing),
            ("deleted", "missing", true)
        );
        assert_eq!(resolve(&inventory, None, " ").requested, "default");
        let _ = fs::remove_dir_all(profile);
    }

    #[test]
    fn save_writes_a_file_that_reads_back_and_delete_resets_the_default() {
        let profile = temp("save");
        let (id, path) = save(
            &profile,
            None,
            "Code Review 评审",
            "Review tone: plain",
            "Point at the line.",
        )
        .unwrap();
        assert_eq!(id, "code-review");
        assert_eq!(path, user_dir(&profile).join("code-review.md"));
        let (again, _) = save(&profile, None, "Code Review", "", "Other.").unwrap();
        assert_eq!(again, "code-review-2");
        let (chinese, _) = save(&profile, None, "我的风格", "", "中文。").unwrap();
        assert_eq!(chinese, "style");
        let read = inventory(&profile, None);
        let review = read.get("code-review").unwrap();
        assert_eq!(
            (
                review.name.as_str(),
                review.description.as_str(),
                review.instructions.as_str()
            ),
            (
                "Code Review 评审",
                "Review tone: plain",
                "Point at the line."
            )
        );
        // A value that would read as quoted or as a block scalar keeps its text.
        save(&profile, Some("odd"), "> arrow", "| pipe", "x").unwrap();
        let odd = inventory(&profile, None).get("odd").cloned().unwrap();
        assert_eq!(
            (odd.name.as_str(), odd.description.as_str()),
            ("> arrow", "| pipe")
        );
        // Saving by id replaces the file.
        save(&profile, Some("code-review"), "Review", "", "New text.").unwrap();
        assert_eq!(
            inventory(&profile, None)
                .get("code-review")
                .unwrap()
                .instructions,
            "New text."
        );

        fs::write(
            profile.join("config.json"),
            r#"{"reply_style":"code-review","model":"m"}"#,
        )
        .unwrap();
        assert_eq!(default_id(&profile), "code-review");
        delete(&profile, "code-review").unwrap();
        assert!(!path.exists());
        assert_eq!(default_id(&profile), "default");
        let config: Value =
            serde_json::from_str(&fs::read_to_string(profile.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(config["model"], "m");
        assert!(delete(&profile, "code-review").is_err());
        let _ = fs::remove_dir_all(profile);
    }

    #[test]
    fn save_and_delete_refuse_with_a_code_for_each_reason() {
        let profile = temp("refuse");
        let code = |result: Result<(String, PathBuf), StyleError>| result.unwrap_err().code;
        let long = "x".repeat(MAX_BYTES + 1);
        assert_eq!(code(save(&profile, None, "", "", "x")), "name_missing");
        assert_eq!(
            code(save(&profile, None, &"名".repeat(41), "", "x")),
            "name_too_long"
        );
        assert_eq!(
            code(save(&profile, None, "Name", "two\nlines", "x")),
            "description_too_long"
        );
        assert_eq!(
            code(save(&profile, None, "Name", "", "  ")),
            "instructions_missing"
        );
        assert_eq!(
            code(save(&profile, None, "Name", "", &long)),
            "instructions_too_long"
        );
        assert_eq!(
            code(save(&profile, Some("default"), "Name", "", "x")),
            "reserved_id"
        );
        assert_eq!(
            code(save(&profile, Some("a/b"), "Name", "", "x")),
            "invalid_id"
        );
        // The styles folder cannot be made: a file stands in its place.
        fs::write(user_dir(&profile), "not a folder").unwrap();
        let error = save(&profile, None, "Name", "", "x").unwrap_err();
        assert_eq!(error.code, "write_failed");
        assert!(error.detail.contains("cannot write"), "{error}");
        assert_eq!(delete(&profile, "default").unwrap_err().code, "reserved_id");
        assert_eq!(delete(&profile, "a/b").unwrap_err().code, "invalid_id");
        fs::remove_file(user_dir(&profile)).unwrap();
        assert_eq!(delete(&profile, "concise").unwrap_err().code, "not_found");
        assert_eq!(
            StyleError::new("not_found", "gone").reply(),
            json!({ "type": "error", "code": "not_found", "message": "gone" })
        );
        let _ = fs::remove_dir_all(profile);
    }

    #[test]
    fn the_list_reply_names_the_default_folders_and_problems() {
        let profile = temp("list");
        let project = temp("list-project");
        write(&user_dir(&profile), "bad name.md", "x\n");
        fs::write(profile.join("config.json"), r#"{"reply_style":"formal"}"#).unwrap();
        let value = list_value(&profile, Some(&project), None, Some(false));
        assert_eq!(value["type"], "styles_list");
        assert_eq!(value["default"], "formal");
        assert_eq!(value["styles"][0]["id"], "default");
        assert_eq!(value["styles"][0]["path"], Value::Null);
        assert_eq!(value["dirs"][1]["scope"], "project");
        assert_eq!(value["projectTrusted"], false);
        assert_eq!(value["problems"].as_array().unwrap().len(), 1);
        assert_eq!(value["problems"][0]["code"], "invalid_id");
        let _ = fs::remove_dir_all(profile);
        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn the_listing_marks_the_current_style() {
        let inventory = StyleInventory {
            styles: builtins(),
            problems: Vec::new(),
        };
        let text = listing(
            &inventory,
            &resolve(&inventory, Some("concise"), "formal"),
            "formal",
        );
        assert!(text.starts_with("reply style: 简洁 (concise, this conversation) · default for new conversations: 正式汇报 (formal)\n"));
        assert!(text.contains("* concise"));
        assert!(text.contains("  formal"));
    }
}

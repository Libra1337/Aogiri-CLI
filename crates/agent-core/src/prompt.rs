use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 64 * 1024;

/// How to use the tools in a coding session, naming only the edit tools that
/// are enabled. Behavior (finish the task, verify, ...) is the base prompt's.
fn tool_guidance(edit_tools: &[String]) -> String {
    let enabled = |name: &str| edit_tools.iter().any(|tool| tool == name);
    let edit_names: Vec<&str> = crate::config::EDIT_TOOL_NAMES
        .into_iter()
        .filter(|name| enabled(name))
        .collect();
    let mut guidance = "Tool use: explore with read, ls and ripgrep; run commands with bash. Make independent calls (reads, searches, checks) together in one response.".to_string();
    if edit_names.is_empty() {
        guidance.push_str(" No file-edit tools are enabled: describe the changes instead.");
        return guidance;
    }
    guidance.push_str(&format!(
        " Read a file before you edit it. Edit files with {}.",
        edit_names.join(" or ")
    ));
    guidance.push_str(if enabled("write") {
        " Create new files with write."
    } else if enabled("apply_patch") {
        " Create new files with apply_patch."
    } else {
        " Create new files with bash (for example a heredoc)."
    });
    guidance
}

#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub date: String,
    pub cwd: PathBuf,
    pub edit_tools: Vec<String>,
    pub project_instructions: Vec<ProjectInstruction>,
    pub skills: Vec<SkillPromptItem>,
    /// A chat session: its base prompt carries its own tool guidance, and
    /// plan mode does not apply.
    pub chat: bool,
    /// The shell sandbox's note; empty without a sandbox.
    pub sandbox: String,
    /// The host's text (a daemon agent's brief and peers); empty without a
    /// host. When it changes the kept prompt is built again; the host's
    /// state (the dispatcher's tasks) is a note instead.
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstruction {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPromptItem {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommand {
    pub command: String,
    pub skill: SkillPromptItem,
}

/// Where the per-session part of the system prompt begins: everything before
/// it is the stable prefix providers cache across sessions and subagents.
pub(crate) const ENV_START: &str = "\n\n<env>\n";

/// The length of the system prompt's stable prefix (see `ENV_START`).
pub(crate) fn stable_prefix_len(system_prompt: &str) -> Option<usize> {
    system_prompt.find(ENV_START)
}

/// The whole prompt, as a turn joins its parts (core::TurnPrompt).
///
/// The system prompt is ordered for prompt caching: what is the same across
/// sessions comes first, so providers can reuse the cached prefix. Base
/// prompt and tool guidance (fixed per config), project instructions and
/// skills (fixed per project), the sandbox note, the working directory and
/// the date, then the host's text. A session keeps the prompt its first turn
/// built (`SessionPrompt`); what changes later (plan mode, the date, the
/// sandbox, the host's state) reaches the model as a note (`Note`).
#[cfg(test)]
pub fn build_system_prompt(base: &str, context: &PromptContext) -> String {
    let mut prompt = runtime_prompt(base, context);
    prompt.push_str(&project_section(&context.project_instructions));
    prompt.push_str(&skills_section(&context.skills));
    prompt.push_str(&tail_section(context));
    prompt
}

/// The base prompt and its tool guidance, fixed per config: the head of the
/// system prompt. The runtime context (directory, date) comes after the
/// skills, in `tail_section`, so the head stays cacheable.
pub fn runtime_prompt(base: &str, context: &PromptContext) -> String {
    let mut prompt = base.trim_end().to_string();
    if !context.chat {
        prompt.push_str("\n\n");
        prompt.push_str(&tool_guidance(&context.edit_tools));
    }
    prompt
}

/// The project's instruction files (AGENTS.md, CLAUDE.md …) as the system
/// prompt carries them; empty without any.
pub fn project_section(instructions: &[ProjectInstruction]) -> String {
    if instructions.is_empty() {
        return String::new();
    }
    let mut section = String::from("\n\n<project_context>\n");
    section.push_str("Project-specific instructions and guidelines:\n\n");
    for instruction in instructions {
        section.push_str(&format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
            escape_xml(&instruction.path.display().to_string()),
            instruction.content.trim_end()
        ));
    }
    section.push_str("</project_context>");
    section
}

/// The skills list as the system prompt carries it; empty without any.
pub fn skills_section(skills: &[SkillPromptItem]) -> String {
    let mut section = String::new();
    if skills.is_empty() {
        return section;
    }
    section.push_str("\n\nSkills: when a task matches a skill's description, read its file first. Resolve relative paths in a skill against its directory.\n<available_skills>\n");
    for skill in skills {
        section.push_str(&format!(
            "<skill name=\"{}\" location=\"{}\">{}</skill>\n",
            escape_xml(&skill.name),
            escape_xml(&skill.path.display().to_string()),
            escape_xml(&skill.description)
        ));
    }
    section.push_str("</available_skills>");
    section
}

/// What follows the skills: the sandbox note, the working directory and the
/// date, and the host's text.
pub fn tail_section(context: &PromptContext) -> String {
    let mut tail = String::new();
    if !context.sandbox.trim().is_empty() {
        tail.push_str("\n\n");
        tail.push_str(context.sandbox.trim_end());
    }
    tail.push_str(&format!(
        "{ENV_START}Working directory: {}\nDate: {}\n</env>",
        context.cwd.display(),
        context.date
    ));
    if !context.host.trim().is_empty() {
        tail.push_str("\n\n");
        tail.push_str(context.host.trim_end());
    }
    tail
}

/// A session's system prompt in its parts, built when its first turn starts
/// and kept while what it is built from stays the same, across reopening: a
/// provider reuses its prompt cache only for a byte-identical prefix. The
/// date and the sandbox change it by a note instead (see `notes_due`); a
/// change to the base prompt, the edit tools, the project's instructions,
/// the skills or the host's text makes the session take the prompt built now
/// (`outdated_by`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPrompt {
    /// The base prompt with its tool guidance (`runtime_prompt`).
    pub head: String,
    pub project: String,
    pub skills: String,
    pub tail: String,
    /// The date `tail` gives.
    pub date: String,
    /// The sandbox note `tail` gives; empty without a sandbox.
    pub sandbox: String,
    /// The host's text `tail` ends with; empty without a host.
    pub host: String,
}

impl SessionPrompt {
    pub fn text(&self) -> String {
        format!("{}{}{}{}", self.head, self.project, self.skills, self.tail)
    }

    /// Whether `now`, the prompt a turn would build now, was built from
    /// other inputs than this one: another base prompt (prompt.txt, a new
    /// version), edit tools, project instructions, skills (a trust
    /// decision) or host text (a daemon agent's brief, handoffs and peers:
    /// the model saw them current every turn before the prompt was kept).
    /// The rest of the tail is left out: its date and sandbox note reach the
    /// model as notes, and the working directory is the session's.
    pub fn outdated_by(&self, now: &SessionPrompt) -> bool {
        self.head != now.head
            || self.project != now.project
            || self.skills != now.skills
            || self.host != now.host
    }

    pub fn to_json(&self) -> Value {
        json!({
            "head": self.head,
            "project": self.project,
            "skills": self.skills,
            "tail": self.tail,
            "date": self.date,
            "sandbox": self.sandbox,
            "host": self.host,
        })
    }

    pub fn from_json(value: &Value) -> Option<Self> {
        let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            head: field("head")?,
            project: field("project")?,
            skills: field("skills")?,
            tail: field("tail")?,
            date: field("date")?,
            sandbox: field("sandbox")?,
            // Saved before it was kept: a host's session builds its prompt
            // again once.
            host: field("host").unwrap_or_default(),
        })
    }
}

/// What a note tells the model about: something that changed since the
/// session's system prompt was built. The latest note of each kind is what
/// the model knows (see `notes_due`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    PlanMode,
    /// The short reminder each user turn in plan mode starts with, after the
    /// one that turned it on (see `plan_mode::REMINDER`).
    PlanReminder,
    Date,
    /// The shell sandbox (`SandboxPolicy::prompt`).
    Sandbox,
    /// The host's current state (`HostExtensions::context`).
    Host,
}

impl Note {
    const ALL: [Note; 5] = [
        Note::PlanMode,
        Note::PlanReminder,
        Note::Date,
        Note::Sandbox,
        Note::Host,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Note::PlanMode => "plan_mode",
            Note::PlanReminder => "plan_reminder",
            Note::Date => "date",
            Note::Sandbox => "sandbox",
            Note::Host => "host",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Note::ALL.into_iter().find(|note| note.as_str() == name)
    }

    /// Whether the note states what holds until a later note of its kind,
    /// so a compaction keeps the latest one in view. A plan reminder only
    /// repeats the plan-mode note.
    pub fn lasting(self) -> bool {
        self != Note::PlanReminder
    }
}

/// How a note opens: it is a user message the engine adds, not the user's.
pub const NOTE_OPEN: &str = "<system-reminder>\n";
const NOTE_CLOSE: &str = "\n</system-reminder>";

fn note(body: &str) -> String {
    format!("{NOTE_OPEN}{}{NOTE_CLOSE}", body.trim())
}

/// The note when the host's state, told earlier, is empty now (a dispatch
/// that ended, say).
const HOST_EMPTY_NOTE: &str =
    "No active tasks or host state now; what earlier notes listed no longer applies.";

/// The note when the shell runs without a sandbox now.
const SANDBOX_OFF_NOTE: &str =
    "The shell sandbox is off now: commands run without one (full access), as the approval mode allows.";

/// What the notes describe now: the approval mode's plan mode, today's date,
/// the shell sandbox and the host's state.
pub struct NoteState {
    pub plan_mode: bool,
    /// The turn starts with a user's message: in plan mode it gets a short
    /// reminder, unless plan mode is told in full just then.
    pub user_turn: bool,
    pub date: String,
    /// The sandbox note (`SandboxPolicy::prompt`); empty without a sandbox.
    pub sandbox: String,
    pub host: String,
}

/// The notes to add before the next request: each kind whose latest note
/// (`last`) says something else than the state now. Without one, the model
/// knows what the system prompt (`prompt`) says: plan mode off, its date and
/// sandbox, no host state. Before the first turn (no `prompt`) the prompt
/// still to be built will say what holds now.
pub fn notes_due<'a>(
    now: &NoteState,
    prompt: Option<&SessionPrompt>,
    last: impl Fn(Note) -> Option<&'a str>,
) -> Vec<(Note, String)> {
    let mut due = Vec::new();
    let prompt_date = prompt.map_or(now.date.as_str(), |prompt| prompt.date.as_str());
    let date = note(&format!("The date is now {}.", now.date));
    if last(Note::Date).map_or(now.date != prompt_date, |told| told != date) {
        due.push((Note::Date, date));
    }
    let plan = note(if now.plan_mode {
        crate::plan_mode::PROMPT_ADDENDUM
    } else {
        crate::plan_mode::ENDED_NOTE
    });
    let plan_told = last(Note::PlanMode).map_or(!now.plan_mode, |told| told == plan);
    if !plan_told {
        due.push((Note::PlanMode, plan));
    }
    let prompt_sandbox = prompt.map_or(now.sandbox.as_str(), |prompt| prompt.sandbox.as_str());
    let sandbox = if now.sandbox.trim().is_empty() {
        note(SANDBOX_OFF_NOTE)
    } else {
        note(&format!(
            "The shell sandbox changed:\n{}",
            now.sandbox.trim()
        ))
    };
    if last(Note::Sandbox).map_or(now.sandbox.trim() != prompt_sandbox.trim(), |told| {
        told != sandbox
    }) {
        due.push((Note::Sandbox, sandbox));
    }
    let host = note(if now.host.trim().is_empty() {
        HOST_EMPTY_NOTE
    } else {
        &now.host
    });
    if last(Note::Host).map_or(!now.host.trim().is_empty(), |told| told != host) {
        due.push((Note::Host, host));
    }
    // Last, so it is next to the user's message.
    if now.plan_mode && now.user_turn && plan_told {
        due.push((Note::PlanReminder, note(crate::plan_mode::REMINDER)));
    }
    due
}

pub fn discover_skills(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> io::Result<Vec<SkillPromptItem>> {
    discover_skills_from(
        profile_dir,
        cwd,
        project_trusted,
        crate::secrets::home_dir().map(|home| home.join(".agents").join("skills")),
    )
}

fn discover_skills_from(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    user_agents_dir: Option<PathBuf>,
) -> io::Result<Vec<SkillPromptItem>> {
    let mut skills = Vec::new();
    let mut global_skills = Vec::new();
    read_skills_dir(&profile_dir.join("skills"), &mut global_skills)?;
    for skill in global_skills {
        if crate::skills::is_skill_path_enabled(profile_dir, &skill.path)? {
            skills.push(skill);
        }
    }
    // `~/.agents/skills/` is the cross-tool convention for user-level skills.
    if let Some(dir) = user_agents_dir {
        read_skills_dir(&dir, &mut skills)?;
    }
    if project_trusted {
        read_skills_dir(&cwd.join(".lynshen").join("skills"), &mut skills)?;
        // `.agents/skills/` is the cross-tool convention for project skills.
        read_skills_dir(&cwd.join(".agents").join("skills"), &mut skills)?;
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills.dedup_by(|left, right| left.name == right.name && left.path == right.path);
    Ok(skills)
}

pub fn skill_commands(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> io::Result<Vec<SkillCommand>> {
    skill_commands_from(
        profile_dir,
        cwd,
        project_trusted,
        crate::secrets::home_dir().map(|home| home.join(".agents").join("skills")),
    )
}

fn skill_commands_from(
    profile_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    user_agents_dir: Option<PathBuf>,
) -> io::Result<Vec<SkillCommand>> {
    let mut commands = discover_skills_from(profile_dir, cwd, project_trusted, user_agents_dir)?
        .into_iter()
        .map(|skill| SkillCommand {
            command: format!("/{}", skill_command_name(&skill.name)),
            skill,
        })
        .collect::<Vec<_>>();
    commands.retain(|entry| entry.command != "/");
    commands.sort_by(|left, right| left.command.cmp(&right.command));
    Ok(commands)
}

/// Directories read-only file tools may read outside the workspace: the
/// directory each discovered skill lives in, so the model can read SKILL.md
/// and follow its relative references. Mutating tools stay confined.
pub fn skill_read_roots(skills: &[SkillPromptItem]) -> Vec<PathBuf> {
    let mut roots = skills
        .iter()
        .filter_map(|skill| skill.path.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots
}

pub fn skill_message(skill: &SkillPromptItem, request: &str) -> io::Result<String> {
    let content = fs::read_to_string(&skill.path)?;
    let mut message = format!(
        "Use the following skill instructions for this request.\n\n<skill name=\"{}\" path=\"{}\">\n{}\n</skill>",
        escape_xml(&skill.name),
        escape_xml(&skill.path.display().to_string()),
        content.trim_end()
    );
    if !request.trim().is_empty() {
        message.push_str("\n\nUser request:\n");
        message.push_str(request.trim());
    }
    Ok(message)
}

pub fn skill_pin_message(skill: &SkillPromptItem) -> io::Result<String> {
    let content = fs::read_to_string(&skill.path)?;
    Ok(format!(
        "<skill name=\"{}\" path=\"{}\">\n{}\n</skill>",
        escape_xml(&skill.name),
        escape_xml(&skill.path.display().to_string()),
        content.trim_end()
    ))
}

pub fn discover_project_instructions(cwd: &Path) -> io::Result<Vec<ProjectInstruction>> {
    let dirs = project_instruction_dirs(cwd);

    let mut instructions = Vec::new();
    let mut remaining = PROJECT_INSTRUCTIONS_MAX_BYTES;
    for dir in dirs {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if path.exists() {
                if remaining == 0 {
                    return Ok(instructions);
                }
                let (content, bytes_read) = read_limited_utf8(&path, remaining)?;
                remaining = remaining.saturating_sub(bytes_read);
                instructions.push(ProjectInstruction {
                    path: path.clone(),
                    content,
                });
            }
        }
    }
    Ok(instructions)
}

fn project_instruction_dirs(cwd: &Path) -> Vec<&Path> {
    let mut dirs = cwd.ancestors().collect::<Vec<_>>();
    dirs.reverse();
    if let Some(index) = dirs.iter().position(|dir| dir.join(".git").exists()) {
        return dirs[index..].to_vec();
    }
    if let Some(index) = dirs
        .iter()
        .position(|dir| dir.join("AGENTS.md").exists() || dir.join("CLAUDE.md").exists())
    {
        return dirs[index..].to_vec();
    }
    vec![cwd]
}

fn read_limited_utf8(path: &Path, max_bytes: usize) -> io::Result<(String, usize)> {
    let bytes = fs::read(path)?;
    let truncated = bytes.len() > max_bytes;
    let mut end = bytes.len().min(max_bytes);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    let mut content = String::from_utf8_lossy(&bytes[..end]).to_string();
    if truncated {
        content.push_str("\n\n[project instructions truncated by LynShen budget]\n");
    }
    Ok((content, end))
}

fn skill_command_name(name: &str) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            output.push('-');
            previous_dash = true;
        }
    }
    output.trim_matches('-').to_string()
}

fn read_skills_dir(dir: &Path, skills: &mut Vec<SkillPromptItem>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let skill_path = path.join("SKILL.md");
            if skill_path.exists() {
                if let Some(skill) = read_skill_file(&skill_path)? {
                    skills.push(skill);
                }
            } else {
                read_skills_dir(&path, skills)?;
            }
        } else if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
            if let Some(skill) = read_skill_file(&path)? {
                skills.push(skill);
            }
        }
    }
    Ok(())
}

fn read_skill_file(path: &Path) -> io::Result<Option<SkillPromptItem>> {
    let content = fs::read_to_string(path)?;
    let name = read_frontmatter_field(&content, "name")
        .or_else(|| {
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "skill".to_string());
    let description = read_frontmatter_field(&content, "description")
        .or_else(|| first_non_empty_body_line(&content))
        .unwrap_or_default();
    if description.is_empty() {
        return Ok(None);
    }
    Ok(Some(SkillPromptItem {
        name,
        description,
        path: path.to_path_buf(),
    }))
}

pub(crate) fn read_frontmatter_field(content: &str, key: &str) -> Option<String> {
    let mut lines = content.lines().peekable();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    while let Some(line) = lines.next() {
        if line.trim_end() == "---" {
            return None;
        }
        // An indented line, or one without a colon, continues another field.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        if field.trim() != key {
            continue;
        }
        let value = value.trim();
        // A block scalar (`>`, `|`, `>-`, `|+`…): the indented lines below.
        if value.starts_with(['>', '|']) && value.len() <= 2 {
            let mut parts = Vec::new();
            while let Some(next) =
                lines.next_if(|next| next.trim().is_empty() || next.starts_with([' ', '\t']))
            {
                parts.push(next.trim());
            }
            let separator = if value.starts_with('>') { " " } else { "\n" };
            return Some(parts.join(separator).trim().to_string());
        }
        return Some(value.trim_matches('"').trim_matches('\'').to_string());
    }
    None
}

fn first_non_empty_body_line(content: &str) -> Option<String> {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "---")
        .map(|line| line.trim_start_matches('#').trim().to_string())
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn coding_context(edit_tools: Vec<String>) -> PromptContext {
        PromptContext {
            date: "2026-05-27".to_string(),
            cwd: PathBuf::from("/repo"),
            edit_tools,
            ..PromptContext::default()
        }
    }

    #[test]
    fn the_stable_prefix_is_the_same_across_days_and_directories() {
        let context = |cwd: &str, date: &str| PromptContext {
            date: date.to_string(),
            cwd: PathBuf::from(cwd),
            ..coding_context(vec!["hashline_edit".to_string()])
        };
        let a = build_system_prompt("base", &context("/a", "2026-10-09"));
        let b = build_system_prompt("base", &context("/b/worktree", "2026-10-10"));
        let (sa, sb) = (
            stable_prefix_len(&a).unwrap(),
            stable_prefix_len(&b).unwrap(),
        );
        assert_eq!(a[..sa], b[..sb]);
        assert!(a[sa..].starts_with(ENV_START) && a[sa..].contains("2026-10-09"));
    }

    #[test]
    fn chat_prompt_has_no_coding_tool_guidance() {
        let prompt = build_system_prompt(
            crate::chat::CHAT_SYSTEM_PROMPT,
            &PromptContext {
                chat: true,
                ..coding_context(crate::config::default_edit_tools())
            },
        );
        assert!(!prompt.contains("Tool use:"));
        assert!(prompt.contains("web_fetch"));
        assert!(prompt.ends_with("Date: 2026-05-27\n</env>"));
    }

    #[test]
    fn prompt_includes_env_project_context_and_skills() {
        let prompt = build_system_prompt(
            "Base prompt",
            &PromptContext {
                cwd: PathBuf::from("C:/repo"),
                project_instructions: vec![ProjectInstruction {
                    path: PathBuf::from("C:/repo/AGENTS.md"),
                    content: "Follow project rules.".to_string(),
                }],
                skills: vec![SkillPromptItem {
                    name: "review".to_string(),
                    description: "Review <code> & tests".to_string(),
                    path: PathBuf::from("C:/skills/review/SKILL.md"),
                }],
                ..coding_context(crate::config::default_edit_tools())
            },
        );

        assert!(prompt.contains("<env>\nWorking directory: C:/repo\nDate: 2026-05-27\n</env>"));
        assert!(prompt.contains("<project_context>"));
        assert!(prompt.contains("Follow project rules."));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains(
            "<skill name=\"review\" location=\"C:/skills/review/SKILL.md\">Review &lt;code&gt; &amp; tests</skill>"
        ));
    }

    #[test]
    fn stable_parts_come_before_the_date_and_host_text() {
        let prompt = build_system_prompt(
            "Base prompt",
            &PromptContext {
                project_instructions: vec![ProjectInstruction {
                    path: PathBuf::from("/repo/AGENTS.md"),
                    content: "Follow project rules.".to_string(),
                }],
                skills: vec![SkillPromptItem {
                    name: "review".to_string(),
                    description: "Review code".to_string(),
                    path: PathBuf::from("/skills/review/SKILL.md"),
                }],
                sandbox: "<sandbox mode=\"workspace-write\">\nnote\n</sandbox>".to_string(),
                host: "Peers: ops (busy)".to_string(),
                ..coding_context(crate::config::default_edit_tools())
            },
        );
        let position = |needle: &str| {
            prompt
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        let order = [
            position("Base prompt"),
            position("Tool use:"),
            position("<project_context>"),
            position("<available_skills>"),
            position("<sandbox"),
            position("<env>"),
            position("Date: 2026-05-27"),
            position("Peers: ops (busy)"),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
        // Only the date line differs between two days' prompts, near the end.
        let tomorrow = build_system_prompt(
            "Base prompt",
            &PromptContext {
                date: "2026-05-28".to_string(),
                ..coding_context(crate::config::default_edit_tools())
            },
        );
        let today = build_system_prompt(
            "Base prompt",
            &coding_context(crate::config::default_edit_tools()),
        );
        let shared = today
            .bytes()
            .zip(tomorrow.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        assert_eq!(&today[shared..], "7\n</env>");
    }

    #[test]
    fn default_guidance_names_only_hashline_edit() {
        let prompt =
            build_system_prompt("Base", &coding_context(crate::config::default_edit_tools()));
        assert!(prompt.contains("Edit files with hashline_edit."));
        assert!(prompt.contains("Create new files with bash"));
        for disabled in ["str_replace", "apply_patch", "with write"] {
            assert!(!prompt.contains(disabled), "guidance names {disabled}");
        }
    }

    #[test]
    fn guidance_lists_enabled_edit_tools_and_new_file_rule() {
        let all = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
        let prompt = build_system_prompt(
            "Base",
            &coding_context(all(&["hashline_edit", "write", "apply_patch"])),
        );
        assert!(prompt.contains("Edit files with hashline_edit or write or apply_patch."));
        assert!(prompt.contains("Create new files with write."));
        let patch_only = build_system_prompt("Base", &coding_context(all(&["apply_patch"])));
        assert!(patch_only.contains("Create new files with apply_patch."));
        let none = build_system_prompt("Base", &coding_context(Vec::new()));
        assert!(none.contains("No file-edit tools are enabled"));
        assert!(!none.contains("Edit files with"));
    }

    #[test]
    fn discovers_project_instructions_from_root_to_cwd() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-instruction-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("repo");
        let nested = project.join("crates").join("cli");
        fs::create_dir_all(&nested).unwrap();
        fs::write(project.join("AGENTS.md"), "root agents").unwrap();
        fs::write(nested.join("CLAUDE.md"), "nested claude").unwrap();

        let instructions = discover_project_instructions(&nested).unwrap();

        assert_eq!(instructions.len(), 2);
        assert_eq!(
            instructions[0]
                .path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("AGENTS.md")
        );
        assert_eq!(
            instructions[1]
                .path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("CLAUDE.md")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn frontmatter_reads_block_scalars_and_skips_continuations() {
        let text = "---\nname: finetone\ndescription: >-\n  Tune the voice\n  of prose.\nlicense: MIT\n---\nbody";
        assert_eq!(
            read_frontmatter_field(text, "description").unwrap(),
            "Tune the voice of prose."
        );
        assert_eq!(read_frontmatter_field(text, "license").unwrap(), "MIT");
        let quoted = "---\nother: |\n  a: b\nname: 'Code Review'\n---\n";
        assert_eq!(
            read_frontmatter_field(quoted, "name").unwrap(),
            "Code Review"
        );
    }

    #[test]
    fn discovers_frontmatter_skill_files() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-skill-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = root.join("profile").join("skills").join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: review\ndescription: Review code carefully\n---\nbody\n",
        )
        .unwrap();

        let skills =
            discover_skills_from(&root.join("profile"), &root.join("cwd"), true, None).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "review");
        assert_eq!(skills[0].description, "Review code carefully");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovers_nested_skill_files() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-nested-skill-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = root
            .join("profile")
            .join("skills")
            .join("bundle")
            .join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: nested-review\ndescription: Review nested code\n---\nbody\n",
        )
        .unwrap();

        let skills =
            discover_skills_from(&root.join("profile"), &root.join("cwd"), true, None).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "nested-review");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skill_commands_slugify_names() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-skill-command-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skill_dir = root.join("profile").join("skills").join("code-review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Code Review\ndescription: Review code\n---\nbody\n",
        )
        .unwrap();

        let commands =
            skill_commands_from(&root.join("profile"), &root.join("cwd"), true, None).unwrap();

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "/code-review");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn disabled_global_skills_are_hidden_but_trusted_project_skills_load() {
        let root = std::env::temp_dir().join(format!(
            "lynshen-skill-sources-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = root.join("profile");
        let cwd = root.join("repo");
        let global = profile.join("skills/global");
        let user_agents = root.join("home/.agents/skills");
        let user_agents_skill = user_agents.join("home-skill");
        let project = cwd.join(".lynshen/skills/project");
        let agents = cwd.join(".agents/skills/agents-skill");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&user_agents_skill).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&agents).unwrap();
        fs::write(
            global.join("SKILL.md"),
            "---\nname: global\ndescription: Global\n---\n",
        )
        .unwrap();
        fs::write(
            user_agents_skill.join("SKILL.md"),
            "---\nname: home-agents\ndescription: Home agents dir\n---\n",
        )
        .unwrap();
        fs::write(
            project.join("SKILL.md"),
            "---\nname: project\ndescription: Project\n---\n",
        )
        .unwrap();
        fs::write(
            agents.join("SKILL.md"),
            "---\nname: agents\ndescription: Agents dir\n---\n",
        )
        .unwrap();
        crate::skills::set_skill_enabled(&profile, "global", false).unwrap();

        let trusted =
            discover_skills_from(&profile, &cwd, true, Some(user_agents.clone())).unwrap();
        let untrusted = discover_skills_from(&profile, &cwd, false, Some(user_agents)).unwrap();

        assert_eq!(
            trusted
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["agents", "home-agents", "project"]
        );
        // Project trust only gates project dirs; user-level sources always load.
        assert_eq!(
            untrusted
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["home-agents"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_prompt_is_its_sections_joined() {
        let context = PromptContext {
            date: "2026-10-09".to_string(),
            cwd: PathBuf::from("/p"),
            edit_tools: Vec::new(),
            project_instructions: vec![ProjectInstruction {
                path: PathBuf::from("/p/AGENTS.md"),
                content: "Be brief.".to_string(),
            }],
            skills: vec![SkillPromptItem {
                name: "pdf".to_string(),
                description: "PDFs".to_string(),
                path: PathBuf::from("/s/pdf/SKILL.md"),
            }],
            sandbox: "Sandboxed.".to_string(),
            host: "Host.".to_string(),
            ..PromptContext::default()
        };
        let project = project_section(&context.project_instructions);
        assert!(project.contains("Be brief."));
        assert!(skills_section(&context.skills).contains("<skill name=\"pdf\""));
        let tail = tail_section(&context);
        assert!(tail.contains("Sandboxed.") && tail.contains(ENV_START) && tail.ends_with("Host."));
        assert_eq!(
            build_system_prompt("Base.", &context),
            format!(
                "{}{}{}{}",
                runtime_prompt("Base.", &context),
                project,
                skills_section(&context.skills),
                tail
            )
        );
        assert_eq!(project_section(&[]), "");
        assert_eq!(skills_section(&[]), "");
    }

    /// The prompt as `build_system_prompt` wrote it in one piece before it was
    /// split into sections (LynShen 0.4.29), kept to prove the split sends the
    /// same bytes. It still added the plan-mode addendum when the turn
    /// started in plan mode (`plan_mode`).
    fn prompt_before_the_split(base: &str, context: &PromptContext, plan_mode: bool) -> String {
        let mut prompt = base.trim_end().to_string();
        if !context.chat {
            prompt.push_str("\n\n");
            prompt.push_str(&tool_guidance(&context.edit_tools));
        }
        if !context.project_instructions.is_empty() {
            prompt.push_str("\n\n<project_context>\n");
            prompt.push_str("Project-specific instructions and guidelines:\n\n");
            for instruction in &context.project_instructions {
                prompt.push_str(&format!(
                    "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                    escape_xml(&instruction.path.display().to_string()),
                    instruction.content.trim_end()
                ));
            }
            prompt.push_str("</project_context>");
        }
        if !context.skills.is_empty() {
            prompt.push_str("\n\nSkills: when a task matches a skill's description, read its file first. Resolve relative paths in a skill against its directory.\n<available_skills>\n");
            for skill in &context.skills {
                prompt.push_str(&format!(
                    "<skill name=\"{}\" location=\"{}\">{}</skill>\n",
                    escape_xml(&skill.name),
                    escape_xml(&skill.path.display().to_string()),
                    escape_xml(&skill.description)
                ));
            }
            prompt.push_str("</available_skills>");
        }
        if !context.sandbox.trim().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(context.sandbox.trim_end());
        }
        prompt.push_str(&format!(
            "{ENV_START}Working directory: {}\nDate: {}\n</env>",
            context.cwd.display(),
            context.date
        ));
        if plan_mode && !context.chat {
            prompt.push_str("\n\n");
            prompt.push_str(crate::plan_mode::PROMPT_ADDENDUM);
        }
        if !context.host.trim().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(context.host.trim_end());
        }
        prompt
    }

    /// The bytes changed once, on purpose: the plan-mode addendum left the
    /// system prompt, which now stays the same for the whole session so the
    /// provider can serve it from its cache. Plan mode reaches the model as a
    /// note instead (`notes_due`). Outside plan mode the bytes are those of
    /// before; in plan mode they are those of before without the addendum.
    #[test]
    fn the_split_prompt_sends_the_same_bytes_as_before() {
        let full = PromptContext {
            date: "2026-10-11".to_string(),
            cwd: PathBuf::from("/work/p"),
            edit_tools: vec!["hashline_edit".to_string()],
            project_instructions: vec![
                ProjectInstruction {
                    path: PathBuf::from("/work/AGENTS.md"),
                    content: "Use tabs.\n".to_string(),
                },
                ProjectInstruction {
                    path: PathBuf::from("/work/p/CLAUDE.md"),
                    content: "<b>&</b>".to_string(),
                },
            ],
            skills: vec![SkillPromptItem {
                name: "pdf & co".to_string(),
                description: "Read <PDFs>".to_string(),
                path: PathBuf::from("/s/pdf/SKILL.md"),
            }],
            sandbox: "Sandboxed.\n".to_string(),
            host: "Host.\n".to_string(),
            ..PromptContext::default()
        };
        let bare = PromptContext {
            date: "2026-10-11".to_string(),
            cwd: PathBuf::from("/w"),
            ..PromptContext::default()
        };
        let chat = PromptContext {
            chat: true,
            ..full.clone()
        };
        for context in [&full, &bare, &chat] {
            let prompt = build_system_prompt("Base.\n", context);
            assert_eq!(prompt, prompt_before_the_split("Base.\n", context, false));
            // A chat session never had plan mode.
            let in_plan_mode = prompt_before_the_split("Base.\n", context, true);
            assert_eq!(
                prompt == in_plan_mode,
                context.chat,
                "plan mode changed the old prompt"
            );
            assert!(!prompt.contains("<plan_mode>"));
        }
        let in_plan_mode = prompt_before_the_split("Base.\n", &full, true);
        assert_eq!(
            in_plan_mode.replacen(&format!("\n\n{}", crate::plan_mode::PROMPT_ADDENDUM), "", 1),
            build_system_prompt("Base.\n", &full)
        );
    }

    #[test]
    fn a_session_prompt_survives_its_json() {
        let prompt = SessionPrompt {
            head: "Base.".to_string(),
            project: "\n\n<project_context>…".to_string(),
            skills: String::new(),
            tail: "\n\n<env>\nDate: 2026-10-12\n</env>".to_string(),
            date: "2026-10-12".to_string(),
            sandbox: String::new(),
            host: "<agent id=\"ops\">".to_string(),
        };
        assert_eq!(
            SessionPrompt::from_json(&prompt.to_json()),
            Some(prompt.clone())
        );
        assert_eq!(
            prompt.text(),
            "Base.\n\n<project_context>…\n\n<env>\nDate: 2026-10-12\n</env>"
        );
        assert_eq!(SessionPrompt::from_json(&json!({ "head": "x" })), None);
    }

    /// The prompt a session keeps, as `core` builds it from `context`.
    fn session_prompt(base: &str, context: &PromptContext) -> SessionPrompt {
        SessionPrompt {
            head: runtime_prompt(base, context),
            project: project_section(&context.project_instructions),
            skills: skills_section(&context.skills),
            tail: tail_section(context),
            date: context.date.clone(),
            sandbox: context.sandbox.clone(),
            host: context.host.clone(),
        }
    }

    #[test]
    fn a_kept_prompt_is_outdated_by_new_instructions_tools_skills_or_host_text_only() {
        let context = PromptContext {
            date: "2026-10-12".to_string(),
            cwd: PathBuf::from("/work"),
            edit_tools: crate::config::default_edit_tools(),
            project_instructions: vec![ProjectInstruction {
                path: PathBuf::from("/work/AGENTS.md"),
                content: "Run the tests.".to_string(),
            }],
            sandbox: "<sandbox mode=\"workspace-write\">\nnote\n</sandbox>".to_string(),
            host: "<state>waiting on CI</state>".to_string(),
            ..PromptContext::default()
        };
        let kept = session_prompt("Base.", &context);
        // What reaches the model as a note, or is the session's own, keeps it.
        let later = PromptContext {
            date: "2026-10-13".to_string(),
            sandbox: "<sandbox mode=\"read-only\">\nnote\n</sandbox>".to_string(),
            ..context.clone()
        };
        assert!(!kept.outdated_by(&session_prompt("Base.", &later)));
        // prompt.txt or a new version's prompt, AGENTS.md, the edit tools,
        // the skills (a trust decision), the host's text (an agent's brief)
        // each take the prompt built now.
        let edited = PromptContext {
            project_instructions: vec![ProjectInstruction {
                path: PathBuf::from("/work/AGENTS.md"),
                content: "Run the tests and the linter.".to_string(),
            }],
            ..later.clone()
        };
        let no_write = PromptContext {
            edit_tools: vec!["edit".to_string()],
            ..later.clone()
        };
        let trusted = PromptContext {
            skills: vec![SkillPromptItem {
                name: "review".to_string(),
                description: "Review a change".to_string(),
                path: PathBuf::from("/work/.lynshen/skills/review/SKILL.md"),
            }],
            ..later.clone()
        };
        let briefed = PromptContext {
            host: "<state>CI green</state>".to_string(),
            ..later.clone()
        };
        for (base, context) in [
            ("New base.", &later),
            ("Base.", &edited),
            ("Base.", &no_write),
            ("Base.", &trusted),
            ("Base.", &briefed),
        ] {
            assert!(kept.outdated_by(&session_prompt(base, context)), "{base}");
        }
    }

    fn state(plan_mode: bool, date: &str, host: &str) -> NoteState {
        NoteState {
            plan_mode,
            user_turn: false,
            date: date.to_string(),
            sandbox: String::new(),
            host: host.to_string(),
        }
    }

    fn kinds(due: &[(Note, String)]) -> Vec<Note> {
        due.iter().map(|(note, _)| *note).collect()
    }

    /// `last` for notes told as `told` says (the latest of each kind last).
    fn told<'a>(told: &'a [(Note, String)]) -> impl Fn(Note) -> Option<&'a str> {
        move |note| {
            told.iter()
                .rev()
                .find(|(kind, _)| *kind == note)
                .map(|(_, text)| text.as_str())
        }
    }

    fn prompt_of(date: &str, sandbox: &str) -> SessionPrompt {
        SessionPrompt {
            date: date.to_string(),
            sandbox: sandbox.to_string(),
            ..SessionPrompt::default()
        }
    }

    #[test]
    fn notes_tell_what_changed_since_the_prompt_or_the_last_note() {
        let prompt = prompt_of("2026-10-12", "");
        // As the prompt says: nothing to tell.
        assert!(notes_due(&state(false, "2026-10-12", ""), Some(&prompt), |_| None).is_empty());
        // Before the first turn the prompt to come says what holds now.
        assert!(notes_due(&state(false, "2026-10-13", ""), None, |_| None).is_empty());

        let due = notes_due(
            &state(true, "2026-10-13", "Status: running"),
            Some(&prompt),
            |_| None,
        );
        assert_eq!(kinds(&due), [Note::Date, Note::PlanMode, Note::Host]);
        assert_eq!(
            due[0].1,
            "<system-reminder>\nThe date is now 2026-10-13.\n</system-reminder>"
        );
        assert!(due[1].1.starts_with("<system-reminder>\n<plan_mode>"));
        assert!(due.iter().all(|(_, text)| text.starts_with(NOTE_OPEN)));

        // Once told, the same state needs no note; a change does.
        assert!(notes_due(
            &state(true, "2026-10-13", "Status: running"),
            Some(&prompt),
            told(&due)
        )
        .is_empty());
        let off = notes_due(
            &state(false, "2026-10-13", "Status: done"),
            Some(&prompt),
            told(&due),
        );
        assert_eq!(kinds(&off), [Note::PlanMode, Note::Host]);
        assert!(off[0].1.contains(crate::plan_mode::ENDED_NOTE));
        assert_eq!(
            off[1].1,
            "<system-reminder>\nStatus: done\n</system-reminder>"
        );
        assert_eq!(Note::parse(Note::Host.as_str()), Some(Note::Host));
        assert_eq!(Note::parse(Note::Sandbox.as_str()), Some(Note::Sandbox));
    }

    #[test]
    fn a_sandbox_change_is_a_note() {
        let workspace =
            "<sandbox mode=\"workspace-write\">\nWritable: the working directory.\n</sandbox>";
        let read_only = "<sandbox mode=\"read-only\">\nNothing is writable.\n</sandbox>";
        let prompt = prompt_of("2026-10-12", workspace);
        let now = |sandbox: &str| NoteState {
            sandbox: sandbox.to_string(),
            ..state(false, "2026-10-12", "")
        };
        assert!(notes_due(&now(workspace), Some(&prompt), |_| None).is_empty());

        let mut notes = notes_due(&now(read_only), Some(&prompt), |_| None);
        assert_eq!(kinds(&notes), [Note::Sandbox]);
        assert_eq!(
            notes[0].1,
            format!(
                "<system-reminder>\nThe shell sandbox changed:\n{read_only}\n</system-reminder>"
            )
        );
        assert!(notes_due(&now(read_only), Some(&prompt), told(&notes)).is_empty());
        // Back to what the prompt says: the model was told otherwise since.
        notes.extend(notes_due(&now(workspace), Some(&prompt), told(&notes)));
        assert_eq!(kinds(&notes), [Note::Sandbox, Note::Sandbox]);
        assert!(notes[1].1.contains(workspace));
        // No sandbox at all.
        let off = notes_due(&now(""), Some(&prompt), told(&notes));
        assert_eq!(kinds(&off), [Note::Sandbox]);
        assert!(off[0].1.contains(SANDBOX_OFF_NOTE));
    }

    #[test]
    fn plan_mode_reminds_each_user_turn_after_the_one_that_told_it() {
        let prompt = prompt_of("2026-10-12", "");
        let user_turn = |plan_mode: bool| NoteState {
            user_turn: true,
            ..state(plan_mode, "2026-10-12", "")
        };
        let mut notes = notes_due(&user_turn(true), Some(&prompt), |_| None);
        // The rules in full, no reminder on top.
        assert_eq!(kinds(&notes), [Note::PlanMode]);
        notes.extend(notes_due(&user_turn(true), Some(&prompt), told(&notes)));
        assert_eq!(kinds(&notes), [Note::PlanMode, Note::PlanReminder]);
        assert!(notes[1].1.contains(crate::plan_mode::REMINDER));
        // Only a user's message gets one, and only in plan mode.
        let in_plan_mode = state(true, "2026-10-12", "");
        assert!(notes_due(&in_plan_mode, Some(&prompt), told(&notes)).is_empty());
        let off = notes_due(&user_turn(false), Some(&prompt), told(&notes));
        assert_eq!(kinds(&off), [Note::PlanMode]);
        assert!(!Note::PlanReminder.lasting());
    }

    #[test]
    fn a_host_state_that_empties_is_told_in_a_short_note() {
        let prompt = prompt_of("2026-10-12", "");
        let tasks = notes_due(
            &state(false, "2026-10-12", "<dispatch>…</dispatch>"),
            Some(&prompt),
            |_| None,
        );
        assert_eq!(kinds(&tasks), [Note::Host]);
        let empty = notes_due(&state(false, "2026-10-12", ""), Some(&prompt), told(&tasks));
        assert_eq!(kinds(&empty), [Note::Host]);
        assert_eq!(
            empty[0].1,
            format!("<system-reminder>\n{HOST_EMPTY_NOTE}\n</system-reminder>")
        );
        assert!(notes_due(&state(false, "2026-10-12", ""), Some(&prompt), told(&empty)).is_empty());
    }
}

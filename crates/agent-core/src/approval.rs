//! What an approval request tells the client beyond the call itself: what
//! kind of decision it is, why the model wants it, and how far an approval
//! may reach (see `docs/serve-protocol.md` → `approval_request`).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalKind {
    /// A file edit inside the workspace (asked in manual mode).
    Edit,
    /// A command inside the sandbox, or with no sandbox to leave.
    Command,
    /// A command that leaves the sandbox (`escalate: true`).
    Escalation,
    /// A write outside the working directory and the writable directories:
    /// a file tool's, or a command's whose target the text shows.
    OutsideWrite,
    /// sudo, su, doas, runas, `Start-Process -Verb RunAs`, ...
    Elevation,
    /// An `ask` command rule or the built-in list that always asks.
    Rule,
    /// Any other tool (MCP, merge_agent, checkpoint restore, ...).
    #[default]
    Tool,
}

impl ApprovalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Command => "command",
            Self::Escalation => "escalation",
            Self::OutsideWrite => "outside_write",
            Self::Elevation => "elevation",
            Self::Rule => "rule",
            Self::Tool => "tool",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "edit" => Self::Edit,
            "command" => Self::Command,
            "escalation" => Self::Escalation,
            "outside_write" => Self::OutsideWrite,
            "elevation" => Self::Elevation,
            "rule" => Self::Rule,
            "tool" => Self::Tool,
            _ => return None,
        })
    }

    /// The call crosses the sandbox or the workspace boundary: when it runs
    /// without asking, clients are told (`escalated`).
    pub fn crosses_boundary(self) -> bool {
        matches!(self, Self::Escalation | Self::OutsideWrite)
    }

    /// The shell's kinds: a session or project approval remembers a command
    /// prefix, never the whole tool.
    pub fn is_command(self) -> bool {
        matches!(
            self,
            Self::Command | Self::Escalation | Self::OutsideWrite | Self::Elevation | Self::Rule
        )
    }
}

/// How far an approval reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ApprovalScope {
    /// This call only.
    Once,
    /// This session: commands with the prefix, or the edit tool.
    Session,
    /// An `allow` rule for the prefix, saved for the project (or globally,
    /// per `always_allow_scope`).
    Project,
}

impl ApprovalScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session => "session",
            Self::Project => "project",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "once" => Ok(Self::Once),
            "session" => Ok(Self::Session),
            "project" => Ok(Self::Project),
            other => Err(format!(
                "unknown scope '{other}': use once, session or project"
            )),
        }
    }
}

/// What an `approval_request` carries besides the call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApprovalInfo {
    pub kind: ApprovalKind,
    /// A person must decide: the mode, "always allow" and a switch to a
    /// looser mode never approve it.
    pub must_ask: bool,
    /// The model's reason (a shell call's `justification`).
    pub justification: Option<String>,
    /// Where it runs (a command's `workdir`, else the agent's directory).
    pub cwd: PathBuf,
    /// The shell command as the model wrote it.
    pub command: Option<String>,
    /// Paths outside the workspace and the writable directories it writes.
    pub paths: Vec<PathBuf>,
    /// The directory a client may offer to make writable.
    pub dir_suggestion: Option<PathBuf>,
    /// The command prefix a session or project approval remembers.
    pub prefix: Option<String>,
    /// How far the engine lets an approval reach.
    pub scopes: Vec<ApprovalScope>,
    /// An elevation the user completes: `terminal` (they run it in their own
    /// terminal) or `uac` (Windows asks once approved).
    pub elevation: Option<&'static str>,
    /// A command that runs inside an enforced OS sandbox: a session
    /// approval allows its prefix there only, not out of the sandbox (not
    /// on the wire).
    pub inside_sandbox: bool,
}

impl ApprovalInfo {
    /// The fields `approval_request` adds to the call.
    pub fn wire_fields(&self, value: &mut Value) {
        value["kind"] = json!(self.kind.as_str());
        value["must_ask"] = json!(self.must_ask);
        value["justification"] = json!(self.justification);
        value["cwd"] = json!(self.cwd.display().to_string());
        value["command"] = json!(self.command);
        value["paths"] = json!(self
            .paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>());
        value["dir_suggestion"] = json!(self
            .dir_suggestion
            .as_ref()
            .map(|dir| dir.display().to_string()));
        value["prefix"] = json!(self.prefix);
        value["scopes"] = json!(self
            .scopes
            .iter()
            .map(|scope| scope.as_str())
            .collect::<Vec<_>>());
        value["elevation"] = json!(self.elevation);
    }

    pub fn allows(&self, scope: ApprovalScope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// The elevation a client offers for this platform.
pub fn elevation_prompt() -> &'static str {
    if cfg!(windows) {
        "uac"
    } else {
        "terminal"
    }
}

/// The directory to offer as writable for a write to `path`: under the home
/// directory its first directory (`~/Desktop` for `~/Desktop/a/b.txt`; two
/// levels for a OneDrive folder), elsewhere the directory the file is in.
/// None for the home directory itself, a root, or credentials.
pub fn dir_suggestion(
    path: &Path,
    home: Option<&Path>,
    credentials: &[PathBuf],
) -> Option<PathBuf> {
    let parent = |path: &Path| path.parent().map(Path::to_path_buf);
    let suggestion =
        match home.and_then(|home| path.strip_prefix(home).ok().map(|rest| (home, rest))) {
            Some((home, rest)) => {
                let mut parts = rest.components();
                let first = parts.next()?;
                let mut dir = home.join(first);
                let is_dir_target = rest.components().count() == 1;
                let name = first.as_os_str().to_string_lossy().to_lowercase();
                // OneDrive's folders, and folders that hold many apps'
                // settings (`~/Library/LaunchAgents`, not all of
                // `~/Library`): one level deeper.
                if name.starts_with("onedrive")
                    || matches!(name.as_str(), "library" | "appdata" | ".config" | ".local")
                {
                    if let Some(second) = parts.next() {
                        if rest.components().count() > 2 {
                            dir = dir.join(second);
                        }
                    }
                }
                // `~/notes.txt`: a file right in the home directory has no
                // directory to offer but home itself.
                if is_dir_target && !path.is_dir() {
                    return None;
                }
                dir
            }
            None => {
                if path.is_dir() {
                    path.to_path_buf()
                } else {
                    parent(path)?
                }
            }
        };
    offerable(&suggestion, home, credentials).then_some(suggestion)
}

/// A directory a client may offer to make writable: not a root, the home
/// directory or one around it, nor one with or in credentials.
fn offerable(dir: &Path, home: Option<&Path>, credentials: &[PathBuf]) -> bool {
    dir.parent().is_some()
        && !home.is_some_and(|home| home.starts_with(dir))
        && !credentials
            .iter()
            .any(|secret| dir.starts_with(secret) || secret.starts_with(dir))
}

/// `dir_suggestion` as a directory that exists (`add_dir` takes only
/// those): the suggestion, or the nearest directory above it that exists
/// and may be offered.
pub fn existing_dir_suggestion(
    path: &Path,
    home: Option<&Path>,
    credentials: &[PathBuf],
) -> Option<PathBuf> {
    let suggestion = dir_suggestion(path, home, credentials)?;
    let existing = suggestion.ancestors().find(|dir| dir.is_dir())?;
    offerable(existing, home, credentials).then(|| existing.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_name_the_folder_under_home_or_the_file_s_directory() {
        let home = Path::new("/home/me");
        let secrets = vec![home.join(".ssh")];
        let suggest = |path: &str| dir_suggestion(Path::new(path), Some(home), &secrets);
        assert_eq!(
            suggest("/home/me/Desktop/notes/a.txt"),
            Some(home.join("Desktop"))
        );
        assert_eq!(
            suggest("/home/me/OneDrive/Desktop/a.txt"),
            Some(home.join("OneDrive/Desktop"))
        );
        assert_eq!(
            suggest("/srv/www/index.html"),
            Some(PathBuf::from("/srv/www"))
        );
        assert_eq!(suggest("/home/me/notes.txt"), None);
        assert_eq!(suggest("/home/me/.ssh/config"), None);
        assert_eq!(suggest("/a.txt"), None);
        assert_eq!(
            suggest("/home/me/Library/LaunchAgents/x.plist"),
            Some(home.join("Library/LaunchAgents"))
        );
        assert_eq!(
            suggest("/home/me/.config/tool/x.toml"),
            Some(home.join(".config/tool"))
        );
        // LynShen's own directory holds its credentials: never offered.
        let secrets = vec![home.join(".lynshen/auth.json")];
        assert_eq!(
            dir_suggestion(
                Path::new("/home/me/.lynshen/config.json"),
                Some(home),
                &secrets
            ),
            None
        );
    }

    #[test]
    fn a_suggestion_is_a_directory_that_exists() {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("lynshen-suggest-{}", std::process::id()));
        std::fs::create_dir_all(base.join("www")).unwrap();
        let suggest = |path: &Path| existing_dir_suggestion(path, None, &[]);
        assert_eq!(
            suggest(&base.join("www/index.html")),
            Some(base.join("www"))
        );
        // Not there yet: the nearest directory that is.
        assert_eq!(
            suggest(&base.join("www/new/deep/x.txt")),
            Some(base.join("www"))
        );
        // Never the home directory or a directory around it.
        let home = base.join("www/me");
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(
            existing_dir_suggestion(&base.join("www/x.txt"), Some(&home), &[]),
            None
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn info_serializes_every_field() {
        let info = ApprovalInfo {
            kind: ApprovalKind::Escalation,
            must_ask: false,
            justification: Some("install deps".into()),
            cwd: PathBuf::from("/work"),
            command: Some("npm install".into()),
            paths: Vec::new(),
            dir_suggestion: None,
            prefix: Some("npm install".into()),
            scopes: vec![
                ApprovalScope::Once,
                ApprovalScope::Session,
                ApprovalScope::Project,
            ],
            elevation: None,
            inside_sandbox: false,
        };
        let mut value = json!({});
        info.wire_fields(&mut value);
        assert_eq!(
            value,
            json!({
                "kind": "escalation",
                "must_ask": false,
                "justification": "install deps",
                "cwd": "/work",
                "command": "npm install",
                "paths": [],
                "dir_suggestion": null,
                "prefix": "npm install",
                "scopes": ["once", "session", "project"],
                "elevation": null,
            })
        );
    }
}

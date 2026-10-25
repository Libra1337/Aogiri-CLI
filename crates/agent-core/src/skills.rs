use flate2::read::GzDecoder;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Cursor, Read},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tar::Archive;
use zip::ZipArchive;

pub mod mirror;

/// Limit for one download: a package, a single skill file.
const MAX_PACKAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_TREE_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 4096;
const SKILL_STATE_FILE: &str = "skills-state.json";
/// In a skill installed from a GitHub repository (or the mirror's copy of
/// one): the repository it came from.
const SOURCE_FILE: &str = ".lynshen-source";
pub const ANTHROPIC_SKILLS_URL: &str = "https://github.com/anthropics/skills";
const ANTHROPIC_SKILLS_INDEX: &str = include_str!("anthropic-skills.json");

/// Public GitHub repositories of skills the desktop marketplace lists beside
/// Anthropic's: (shown name, repository). Their skills sit in `skills/<id>/`
/// or at the top level (`<id>/SKILL.md`); see [`fetch_github_source`]. The
/// skills mirror builds the same list (deploy/skills-mirror/sources.json).
pub const COMMUNITY_SKILL_SOURCES: [(&str, &str); 3] = [
    ("Ikaleio", "https://github.com/Ikaleio/skills"),
    ("Superpowers", "https://github.com/obra/superpowers"),
    (
        "Composio",
        "https://github.com/ComposioHQ/awesome-claude-skills",
    ),
];

/// How long one marketplace request may wait for its connection, and for
/// the whole response. From China GitHub is often slow or blocked, so a
/// request gives up rather than hold the catalog for minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub total: Duration,
}

/// Listing a source: a repository's tree and its SKILL.md files.
pub const CATALOG_TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(8),
    total: Duration::from_secs(20),
};
/// One file or package of a skill being installed.
pub const INSTALL_TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(8),
    total: Duration::from_secs(60),
};

/// A skill in a GitHub repository's `<dir>/<id>/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub license: String,
    /// False for source-available skills whose terms forbid redistribution.
    pub redistributable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSource {
    pub name: String,
    pub repository: String,
    /// A commit SHA; empty follows the default branch.
    pub revision: String,
    /// The directory holding the skill folders: `skills`, or empty for the
    /// repository's top level.
    pub dir: String,
    pub skills: Vec<SourceSkill>,
}

impl SkillSource {
    pub fn homepage(&self, id: &str) -> String {
        format!(
            "{}/tree/{}/{}",
            self.repository,
            self.git_ref(),
            self.skill_path(id)
        )
    }

    /// `<dir>/<id>` in the repository.
    fn skill_path(&self, id: &str) -> String {
        if self.dir.is_empty() {
            id.to_string()
        } else {
            format!("{}/{id}", self.dir)
        }
    }

    fn git_ref(&self) -> &str {
        if self.revision.is_empty() {
            "HEAD"
        } else {
            &self.revision
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub package_url: Option<String>,
    pub package_sha256: Option<String>,
    pub package_type: Option<String>,
    pub tags: Vec<String>,
    pub enabled: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marketplace {
    pub skills: Vec<MarketplaceSkill>,
    pub default_skill_ids: Vec<String>,
}

pub fn fetch_marketplace(api_url: &str, api_key: Option<&str>) -> Result<Marketplace, String> {
    let url = format!("{}/v1/skills/marketplace", api_url.trim_end_matches('/'));
    let mut request = ureq::get(&url).timeout(Duration::from_secs(30));
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = request.set("Authorization", &format!("Bearer {key}"));
    }
    let response = request.call().map_err(|error| error.to_string())?;
    let value = response
        .into_json::<Value>()
        .map_err(|error| error.to_string())?;
    parse_marketplace(&value)
}

/// The marketplace for the configured LynShen API. The endpoint is public, so
/// without a LynShen login it is asked without a token.
pub fn fetch_lynshen_marketplace() -> Result<Marketplace, String> {
    let config = crate::config::Config::load_or_create().map_err(|error| error.to_string())?;
    let auth = crate::oauth::ensure_session(&config.lynshen_api_url, config.encrypt_secrets).ok();
    fetch_marketplace(
        &config.lynshen_api_url,
        auth.as_ref().and_then(|auth| auth.lynshen_access_token()),
    )
}

/// Where LynShen loads installed user skills from: `~/.lynshen/skills`.
pub fn profile_skills_dir() -> io::Result<PathBuf> {
    Ok(crate::config::profile_dir()?.join("skills"))
}

/// The bundled index of github.com/anthropics/skills, pinned to a reviewed
/// commit so listing needs no GitHub request.
pub fn anthropic_source() -> Result<SkillSource, String> {
    let value = serde_json::from_str(ANTHROPIC_SKILLS_INDEX)
        .map_err(|error| format!("invalid bundled Anthropic skills index: {error}"))?;
    parse_source_index(&value)
}

pub fn fetch_extra_skill_source(spec: &str) -> Result<Option<SkillSource>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }
    if spec == "anthropic" || normalize_repository_url(spec) == ANTHROPIC_SKILLS_URL {
        return anthropic_source().map(Some);
    }

    let (_, repository) = github_repository_parts(spec)?;
    fetch_github_source(&repository, spec).map(Some)
}

/// A GitHub repository's skills, wherever its `SKILL.md` files sit: every
/// `<id>/SKILL.md` at the top level or `<dir>/<id>/SKILL.md` one level down,
/// from the directory holding the most. Read at the default branch's current
/// commit, which installs then use, so what is installed is what was listed
/// (as the mirror pins it). Names, descriptions and licenses come from each
/// SKILL.md's frontmatter, read in parallel; a skill without a license has
/// the repository's.
pub fn fetch_github_source(name: &str, repository: &str) -> Result<SkillSource, String> {
    read_github_source(name, repository, &|url, limit| {
        fetch(url, limit, CATALOG_TIMEOUTS)
    })
}

fn read_github_source(
    name: &str,
    repository: &str,
    fetch: &(dyn Fn(&str, usize) -> io::Result<Vec<u8>> + Sync),
) -> Result<SkillSource, String> {
    let (owner, repo) = github_repository_parts(repository)?;
    let json = |url: &str| -> Result<Value, String> {
        let bytes = fetch(url, MAX_TREE_BYTES).map_err(|error| error.to_string())?;
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())
    };
    let api = format!("https://api.github.com/repos/{owner}/{repo}");
    let meta = json(&api)?;
    let branch = meta["default_branch"]
        .as_str()
        .ok_or_else(|| format!("{repository} has no default branch"))?;
    let revision = json(&format!("{api}/branches/{}", encode_url_path(branch)))?["commit"]["sha"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{repository}: no commit for {branch}"))?;
    validate_revision(&revision).map_err(|error| error.to_string())?;
    let repository_license = meta["license"]["spdx_id"]
        .as_str()
        .filter(|id| *id != "NOASSERTION")
        .unwrap_or_default()
        .to_string();
    let tree = json(&format!("{api}/git/trees/{revision}?recursive=1"))?;
    let mut by_dir: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for path in tree["tree"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["type"] == "blob")
        .filter_map(|entry| entry["path"].as_str())
    {
        let parts: Vec<&str> = path.split('/').collect();
        let (dir, id) = match parts.as_slice() {
            [id, "SKILL.md"] => ("", *id),
            [dir, id, "SKILL.md"] => (*dir, *id),
            _ => continue,
        };
        if !id.starts_with('.') && validate_skill_id(id).is_ok() {
            by_dir
                .entry(dir.to_string())
                .or_default()
                .push(id.to_string());
        }
    }
    let (dir, ids) = by_dir
        .into_iter()
        // `skills/` or the top level hold a repository's own skills; another
        // folder (a vendored collection) only when there are none there.
        .max_by_key(|(dir, ids)| (dir == "skills" || dir.is_empty(), ids.len()))
        .ok_or_else(|| format!("no SKILL.md found in {repository}"))?;
    let read = |id: &str| -> Result<[String; 3], String> {
        let path = if dir.is_empty() {
            format!("{id}/SKILL.md")
        } else {
            format!("{dir}/{id}/SKILL.md")
        };
        let bytes = fetch(
            &format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{revision}/{}",
                encode_url_path(&path)
            ),
            256 * 1024,
        )
        .map_err(|error| error.to_string())?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(["name", "description", "license"]
            .map(|key| crate::prompt::read_frontmatter_field(&text, key).unwrap_or_default()))
    };
    let reads: Vec<Result<[String; 3], String>> = std::thread::scope(|scope| {
        let read = &read;
        let handles: Vec<_> = ids.iter().map(|id| scope.spawn(move || read(id))).collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("SKILL.md read failed".to_string()))
            })
            .collect()
    });
    // raw.githubusercontent.com blocked while the API answers: a list of bare
    // ids is no catalog, and must not replace a cached one.
    if let Some(Err(error)) = reads.first().filter(|_| reads.iter().all(Result::is_err)) {
        return Err(error.clone());
    }
    let metas = reads.into_iter().map(Result::unwrap_or_default);
    let mut skills: Vec<SourceSkill> = ids
        .iter()
        .zip(metas)
        .map(|(id, [skill_name, description, license])| {
            let license = match license.trim() {
                "" => repository_license.clone(),
                own => own.to_string(),
            };
            SourceSkill {
                id: id.clone(),
                name: if skill_name.is_empty() {
                    id.clone()
                } else {
                    skill_name
                },
                description,
                tags: Vec::new(),
                redistributable: offered_license(&license),
                license,
            }
        })
        .collect();
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(SkillSource {
        name: name.to_string(),
        repository: normalize_repository_url(repository),
        revision,
        dir,
        skills,
    })
}

/// Whether LynShen offers a GitHub skill under `license` (its frontmatter
/// `license`, else its repository's as GitHub detects it): not when its
/// terms say proprietary or source-available, as Anthropic's document
/// skills (and copies of them in other repositories) do; those are listed
/// with their license and repository link, not installed. A skill that
/// states no terms (the author's own public repository) installs from
/// GitHub as published; the mirror packs only licensed ones.
fn offered_license(license: &str) -> bool {
    let terms = license.to_ascii_lowercase();
    !["proprietary", "source-available", "source available"]
        .iter()
        .any(|word| terms.contains(word))
}

/// A source as the marketplace cache and the mirror's catalog store it: the
/// bundled index's fields, plus `dir`.
pub fn source_json(source: &SkillSource) -> Value {
    serde_json::json!({
        "name": source.name,
        "repository": source.repository,
        "revision": source.revision,
        "dir": source.dir,
        "skills": source.skills.iter().map(|skill| serde_json::json!({
            "id": skill.id,
            "name": skill.name,
            "description": skill.description,
            "tags": skill.tags,
            "license": skill.license,
            "redistributable": skill.redistributable,
        })).collect::<Vec<_>>(),
    })
}

/// Reads [`source_json`] back. Everything that later names a path or a URL
/// (the repository, revision, folder and ids) is checked; a missing name,
/// description or license is empty, a missing `redistributable` false.
pub fn parse_source(value: &Value) -> Result<SkillSource, String> {
    let name = read_string(value, "name").ok_or("source missing name")?;
    let repository = read_string(value, "repository").ok_or("source missing repository")?;
    github_repository_parts(&repository)?;
    let revision = read_string(value, "revision").unwrap_or_default();
    if !revision.is_empty() {
        validate_revision(&revision).map_err(|error| error.to_string())?;
    }
    let dir = read_string(value, "dir").unwrap_or_default();
    if !dir.is_empty()
        && !dir
            .split('/')
            .all(|part| validate_folder_name(part).is_ok())
    {
        return Err(format!("unsafe source folder: {dir}"));
    }
    let mut skills: Vec<SourceSkill> = Vec::new();
    for item in value["skills"].as_array().ok_or("source missing skills")? {
        let id = read_string(item, "id").ok_or("source skill missing id")?;
        validate_skill_id(&id).map_err(|error| error.to_string())?;
        if skills.iter().any(|skill| skill.id == id) {
            return Err(format!("duplicate source skill id: {id}"));
        }
        skills.push(SourceSkill {
            name: read_string(item, "name").unwrap_or_else(|| id.clone()),
            description: read_string(item, "description").unwrap_or_default(),
            tags: read_strings(item, "tags"),
            license: read_string(item, "license").unwrap_or_default(),
            redistributable: item["redistributable"].as_bool().unwrap_or(false),
            id,
        });
    }
    Ok(SkillSource {
        name,
        repository: normalize_repository_url(&repository),
        revision,
        dir,
        skills,
    })
}

/// One folder name of a repository path: no `.`/`..`, no separators.
fn validate_folder_name(name: &str) -> io::Result<()> {
    let safe = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if safe {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe folder name: {name}"),
        ))
    }
}

fn parse_source_index(value: &Value) -> Result<SkillSource, String> {
    let name = read_string(value, "name").ok_or_else(|| "source index missing name".to_string())?;
    let repository = read_string(value, "repository")
        .ok_or_else(|| "source index missing repository".to_string())?;
    github_repository_parts(&repository)?;
    let revision = read_string(value, "revision").unwrap_or_default();
    if !revision.is_empty() {
        validate_revision(&revision).map_err(|error| error.to_string())?;
    }
    let skill_values = value
        .get("skills")
        .and_then(Value::as_array)
        .ok_or_else(|| "source index missing skills".to_string())?;
    let mut skills: Vec<SourceSkill> = Vec::with_capacity(skill_values.len());
    for item in skill_values {
        let id = read_string(item, "id").ok_or_else(|| "source skill missing id".to_string())?;
        validate_skill_id(&id).map_err(|error| error.to_string())?;
        let field = |key: &str| {
            read_string(item, key).ok_or_else(|| format!("source skill {id} missing {key}"))
        };
        let skill = SourceSkill {
            name: field("name")?,
            description: field("description")?,
            tags: read_strings(item, "tags"),
            license: field("license")?,
            redistributable: item
                .get("redistributable")
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("source skill {id} missing redistributable"))?,
            id,
        };
        if skills.iter().any(|existing| existing.id == skill.id) {
            return Err(format!("duplicate source skill id: {}", skill.id));
        }
        skills.push(skill);
    }
    Ok(SkillSource {
        name,
        repository: normalize_repository_url(&repository),
        revision,
        dir: "skills".to_string(),
        skills,
    })
}

/// Installs the whole `skills/<id>/` directory of `source` into
/// `skills_dir/<id>`, replacing an earlier install only once every file has
/// downloaded.
pub fn install_source_skill(
    skills_dir: &Path,
    source: &SkillSource,
    skill: &SourceSkill,
) -> io::Result<PathBuf> {
    install_github_skill(skills_dir, source, skill, &download)
}

fn install_github_skill(
    skills_dir: &Path,
    source: &SkillSource,
    skill: &SourceSkill,
    fetch: &dyn Fn(&str, usize) -> io::Result<Vec<u8>>,
) -> io::Result<PathBuf> {
    validate_skill_id(&skill.id)?;
    check_source_free(skills_dir, &skill.id, &source.repository)?;
    let (owner, repository) = github_repository_parts(&source.repository)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if !source.revision.is_empty() {
        validate_revision(&source.revision)?;
    }
    let git_ref = source.git_ref();
    let tree = fetch(
        &format!(
            "https://api.github.com/repos/{owner}/{repository}/git/trees/{git_ref}?recursive=1"
        ),
        MAX_TREE_BYTES,
    )?;
    let tree = serde_json::from_slice::<Value>(&tree)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if tree.get("truncated").and_then(Value::as_bool) == Some(true) {
        return Err(invalid_data(format!(
            "GitHub returned a truncated tree for {}",
            source.repository
        )));
    }

    let prefix = format!("{}/", source.skill_path(&skill.id));
    let mut files = Vec::new();
    let mut declared_bytes = 0_u64;
    for entry in tree
        .get("tree")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_data("GitHub tree response missing tree".to_string()))?
    {
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(relative) = path.strip_prefix(&prefix) else {
            continue;
        };
        let relative = safe_path_components(Path::new(relative))
            .ok_or_else(|| invalid_data(format!("unsafe path in GitHub tree: {path}")))?;
        let mode = entry
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match entry.get("type").and_then(Value::as_str) {
            Some("tree") => continue,
            Some("blob") if mode == "100644" || mode == "100755" => {}
            _ => {
                return Err(invalid_data(format!(
                    "unsupported GitHub tree entry: {path}"
                )))
            }
        }
        let size = entry
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_data(format!("GitHub tree entry has no size: {path}")))?;
        if size > MAX_PACKAGE_BYTES as u64 {
            return Err(invalid_data(format!(
                "skill file exceeds {MAX_PACKAGE_BYTES} byte limit: {path}"
            )));
        }
        declared_bytes = declared_bytes.saturating_add(size);
        if declared_bytes > MAX_EXTRACTED_BYTES {
            return Err(invalid_data(format!(
                "skill exceeds {MAX_EXTRACTED_BYTES} byte limit"
            )));
        }
        files.push((path.to_string(), relative, mode == "100755"));
        if files.len() > MAX_PACKAGE_FILES {
            return Err(invalid_data(format!(
                "skill exceeds {MAX_PACKAGE_FILES} file limit"
            )));
        }
    }
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "skill files not found in {}: {}",
                source.repository, skill.id
            ),
        ));
    }

    let dir = skills_dir.join(&skill.id);
    let staging = staging_dir(&dir, "download");
    recreate_dir(&staging)?;
    let downloaded = (|| {
        let mut actual_bytes = 0_u64;
        for (path, relative, executable) in files {
            let url = format!(
                "https://raw.githubusercontent.com/{owner}/{repository}/{git_ref}/{}",
                encode_url_path(&path)
            );
            let bytes = fetch(&url, MAX_PACKAGE_BYTES)?;
            actual_bytes = actual_bytes.saturating_add(bytes.len() as u64);
            if actual_bytes > MAX_EXTRACTED_BYTES {
                return Err(invalid_data(format!(
                    "skill exceeds {MAX_EXTRACTED_BYTES} byte limit"
                )));
            }
            let out = staging.join(relative);
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&out, bytes)?;
            set_mode(&out, Some(if executable { 0o755 } else { 0o644 }))?;
        }
        if !staging.join("SKILL.md").is_file() {
            return Err(invalid_data(format!(
                "skill {} does not contain SKILL.md",
                skill.id
            )));
        }
        record_source(&staging, &source.repository)
    })();
    if let Err(error) = downloaded {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    atomic_replace_dir(&staging, &dir)?;
    Ok(dir)
}

/// Installs a marketplace skill into `skills_dir`; its enabled state (a
/// LynShen profile's `skills-state.json`) is the caller's.
pub fn install_marketplace_skill(
    skills_dir: &Path,
    skill: &MarketplaceSkill,
) -> io::Result<PathBuf> {
    let dir = skills_dir.join(safe_skill_dir(&skill.id));
    if let Some(url) = skill
        .package_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        install_skill_package(&dir, skill, url)?;
    } else {
        install_inline_skill(&dir, skill)?;
    }
    Ok(dir)
}

pub fn install_default_skills(profile_dir: &Path, marketplace: &Marketplace) -> io::Result<usize> {
    let mut installed = 0;
    for id in &marketplace.default_skill_ids {
        if let Some(skill) = marketplace.skills.iter().find(|skill| &skill.id == id) {
            install_marketplace_skill(&profile_dir.join("skills"), skill)?;
            set_skill_enabled(profile_dir, &skill.id, true)?;
            installed += 1;
        }
    }
    Ok(installed)
}

pub fn uninstall_skill(profile_dir: &Path, id: &str) -> io::Result<bool> {
    let skills_dir = profile_dir.join("skills");
    let target = skills_dir.join(safe_skill_dir(id));
    if !target.exists() {
        return Ok(false);
    }
    ensure_direct_child(&skills_dir, &target)?;
    fs::remove_dir_all(&target)?;
    set_skill_enabled(profile_dir, id, true)?;
    Ok(true)
}

pub fn set_skill_enabled(profile_dir: &Path, id: &str, enabled: bool) -> io::Result<()> {
    let id = safe_skill_dir(id);
    let mut state = SkillState::read(profile_dir)?;
    if enabled {
        state
            .disabled_paths
            .remove(&path_key(&profile_dir.join("skills").join(&id)));
        state.disabled.remove(&id);
    } else {
        state.disabled.insert(id);
    }
    state.write(profile_dir)
}

/// Turns the skill in folder `dir` on or off for every LynShen session. A
/// folder of `~/.lynshen/skills` named as `/skills disable` names it is
/// switched by that name; any other (a skill pack's, `~/.agents/skills`, a
/// project's) by its path.
pub fn set_skill_dir_enabled(profile_dir: &Path, dir: &Path, enabled: bool) -> io::Result<()> {
    let root = profile_dir.join("skills");
    let id = dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| dir.parent() == Some(root.as_path()) && safe_skill_dir(name) == *name);
    let mut state = SkillState::read(profile_dir)?;
    let key = path_key(dir);
    match (enabled, id) {
        (true, id) => {
            state.disabled_paths.remove(&key);
            if let Some(id) = id {
                state.disabled.remove(id);
            }
        }
        (false, Some(id)) => {
            state.disabled.insert(id.to_string());
        }
        (false, None) => {
            state.disabled_paths.insert(key);
        }
    }
    state.write(profile_dir)
}

pub fn is_skill_path_enabled(profile_dir: &Path, path: &Path) -> io::Result<bool> {
    Ok(SkillState::read(profile_dir)?.enabled(profile_dir, path))
}

/// Where a skill LynShen loads comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// `~/.lynshen/skills`: installed, imported or created for LynShen.
    User,
    /// `~/.agents/skills`, which other coding agents read too.
    Shared,
    /// The project's `.lynshen/skills` or `.agents/skills`.
    Project,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Shared => "shared",
            Self::Project => "project",
        }
    }
}

/// One skill found on disk, loaded or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkill {
    pub name: String,
    pub description: String,
    /// The folder holding its SKILL.md.
    pub dir: PathBuf,
    pub scope: SkillScope,
    pub enabled: bool,
}

/// What keeps a skill from loading, or makes it load other than expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillProblem {
    /// An error stops the skill (or every LynShen turn) from loading; a
    /// warning does not.
    pub error: bool,
    /// `unreadable`, `no_description`, `duplicate`, `untrusted` or `state`.
    pub kind: &'static str,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillInventory {
    pub skills: Vec<InstalledSkill>,
    pub problems: Vec<SkillProblem>,
    /// Whether the project's skills load; `None` without a project.
    pub project_trusted: Option<bool>,
}

/// Every skill LynShen finds for a session in `cwd` (none of a project's
/// without one), on or off, and what keeps any from loading.
pub fn installed_skills(cwd: Option<&Path>) -> io::Result<SkillInventory> {
    let profile = crate::config::profile_dir()?;
    let trusted = match cwd {
        Some(cwd) if crate::trust::project_has_local_resources(cwd) => Some(
            crate::trust::TrustStore::load_or_create()?
                .decision_for(cwd)
                .unwrap_or(false),
        ),
        Some(_) => Some(true),
        None => None,
    };
    Ok(crate::prompt::skill_inventory(
        &profile,
        cwd,
        trusted,
        crate::secrets::home_dir().map(|home| home.join(".agents").join("skills")),
    ))
}

/// The skills in another agent's folders (Claude Code's), all listed as on:
/// LynShen does not switch them.
pub fn skills_in(roots: &[(SkillScope, PathBuf)]) -> SkillInventory {
    crate::prompt::scan_skill_roots(roots, &|_| true)
}

/// `set_skill_dir_enabled` for this profile.
pub fn switch_skill(dir: &Path, enabled: bool) -> io::Result<()> {
    set_skill_dir_enabled(&crate::config::profile_dir()?, dir, enabled)
}

/// `remove_skill_dir` for this profile.
pub fn delete_skill(root: &Path, dir: &Path) -> io::Result<()> {
    remove_skill_dir(&crate::config::profile_dir()?, root, dir)
}

/// Deletes the skill folder `dir`, a direct child of `root` holding a
/// SKILL.md, and forgets its switch when `root` is LynShen's own.
pub fn remove_skill_dir(profile_dir: &Path, root: &Path, dir: &Path) -> io::Result<()> {
    ensure_direct_child(root, dir)?;
    if !dir.join("SKILL.md").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("not a skill folder: {}", dir.display()),
        ));
    }
    if root == profile_dir.join("skills") {
        set_skill_dir_enabled(profile_dir, dir, true)?;
    }
    fs::remove_dir_all(dir)
}

/// A new skill `skills_dir/<name>/SKILL.md` for the user to fill in. Names
/// follow the Agent Skills format: lowercase letters, digits and hyphens.
pub fn create_skill(skills_dir: &Path, name: &str, description: &str) -> io::Result<PathBuf> {
    let name = name.trim();
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.len() > 64 || validate_skill_id(name).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("skill name must be 1-64 lowercase letters, digits or hyphens: {name}"),
        ));
    }
    if description.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a skill needs a description: the model reads it to decide when to use the skill",
        ));
    }
    let dir = skills_dir.join(name);
    if dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("a skill named {name} already exists"),
        ));
    }
    fs::create_dir_all(&dir)?;
    let file = dir.join("SKILL.md");
    fs::write(
        &file,
        format!(
            "---\nname: {name}\ndescription: >-\n  {description}\n---\n\n# {name}\n\nWhen to use this skill, and the steps to follow.\n"
        ),
    )?;
    Ok(file)
}

/// The skills-state.json switches: folders of `~/.lynshen/skills` by name
/// (as `/skills disable` sets them) and any other skill folder by path.
#[derive(Debug, Default)]
pub(crate) struct SkillState {
    disabled: BTreeSet<String>,
    disabled_paths: BTreeSet<String>,
}

impl SkillState {
    pub(crate) fn read(profile_dir: &Path) -> io::Result<Self> {
        let path = profile_dir.join(SKILL_STATE_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        let value = serde_json::from_str::<Value>(&content).unwrap_or(Value::Null);
        let list = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        };
        Ok(Self {
            disabled: list("disabled"),
            disabled_paths: list("disabled_paths"),
        })
    }

    fn write(&self, profile_dir: &Path) -> io::Result<()> {
        let mut value = serde_json::json!({ "disabled": self.disabled });
        if !self.disabled_paths.is_empty() {
            value["disabled_paths"] = serde_json::json!(self.disabled_paths);
        }
        write_skill_state(profile_dir, &value)
    }

    /// Whether the skill whose SKILL.md is `file` loads.
    pub(crate) fn enabled(&self, profile_dir: &Path, file: &Path) -> bool {
        if let Ok(relative) = file.strip_prefix(profile_dir.join("skills")) {
            if let Some(Component::Normal(id)) = relative.components().next() {
                match id.to_str() {
                    Some(id) if self.disabled.contains(id) => return false,
                    None => return false,
                    _ => {}
                }
            }
        }
        match file.parent() {
            Some(dir) if !self.disabled_paths.is_empty() => {
                !self.disabled_paths.contains(&path_key(dir))
            }
            _ => true,
        }
    }
}

/// A skill folder as skills-state.json names it: resolved, so the daemon and
/// an engine reaching it by different paths agree.
fn path_key(dir: &Path) -> String {
    fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

pub fn installed_skill_ids(profile_dir: &Path) -> io::Result<Vec<String>> {
    let root = profile_dir.join("skills");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let disabled = SkillState::read(profile_dir)?.disabled;
    let mut installed = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.path().join("SKILL.md").exists() {
            let id = entry.file_name().to_string_lossy().to_string();
            installed.push(if disabled.contains(&id) {
                format!("{id} (disabled)")
            } else {
                id
            });
        }
    }
    installed.sort();
    Ok(installed)
}

/// The repository an installed skill came from; None for one from the
/// LynShen marketplace, made by hand, or installed before sources were
/// recorded.
pub fn installed_source(skills_dir: &Path, id: &str) -> Option<String> {
    fs::read_to_string(skills_dir.join(safe_skill_dir(id)).join(SOURCE_FILE))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Whether `id` is installed from `repository`: a skill another repository
/// installed under the same name is not this one. One whose source was not
/// recorded counts for every source.
pub fn installed_from(skills_dir: &Path, id: &str, repository: &str) -> bool {
    skill_installed(skills_dir, id)
        && installed_source(skills_dir, id)
            .is_none_or(|source| source == normalize_repository_url(repository))
}

/// Two repositories can name a skill alike: one must not replace the
/// other's. An error while `id` is installed from another repository.
pub fn check_source_free(skills_dir: &Path, id: &str, repository: &str) -> io::Result<()> {
    let repository = normalize_repository_url(repository);
    match installed_source(skills_dir, id).filter(|other| *other != repository) {
        Some(other) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("skill {id} from {other} is already installed; remove it first"),
        )),
        None => Ok(()),
    }
}

/// Notes in an installed skill the repository it came from.
pub(crate) fn record_source(dir: &Path, repository: &str) -> io::Result<()> {
    fs::write(dir.join(SOURCE_FILE), normalize_repository_url(repository))
}

pub fn skill_installed(skills_dir: &Path, id: &str) -> bool {
    skills_dir
        .join(safe_skill_dir(id))
        .join("SKILL.md")
        .is_file()
}

pub fn parse_marketplace(value: &Value) -> Result<Marketplace, String> {
    let skills_value = value
        .get("skills")
        .and_then(Value::as_array)
        .ok_or_else(|| "marketplace response missing skills".to_string())?;
    let skills = skills_value
        .iter()
        .filter_map(parse_skill)
        .filter(|skill| skill.enabled)
        .collect::<Vec<_>>();
    Ok(Marketplace {
        skills,
        default_skill_ids: read_strings(value, "default_skill_ids"),
    })
}

fn parse_skill(value: &Value) -> Option<MarketplaceSkill> {
    let id = read_string(value, "id")?;
    let name = read_string(value, "name")?;
    let description = read_string(value, "description")?;
    let content = read_string(value, "content").unwrap_or_default();
    let package_url = read_string(value, "package_url");
    if content.is_empty() && package_url.is_none() {
        return None;
    }
    let enabled = value
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let updated_at = value
        .get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(MarketplaceSkill {
        id,
        name,
        description,
        content,
        package_url,
        package_sha256: read_string(value, "package_sha256"),
        package_type: read_string(value, "package_type"),
        tags: read_strings(value, "tags"),
        enabled,
        updated_at,
    })
}

fn read_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn read_strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn install_inline_skill(dir: &Path, skill: &MarketplaceSkill) -> io::Result<()> {
    let staging = staging_dir(dir, "inline");
    recreate_dir(&staging)?;
    if let Err(error) = fs::write(staging.join("SKILL.md"), normalized_content(skill)) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    atomic_replace_dir(&staging, dir)
}

fn install_skill_package(dir: &Path, skill: &MarketplaceSkill, url: &str) -> io::Result<()> {
    let expected = skill.package_sha256.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "marketplace package is missing required package_sha256",
        )
    })?;
    let bytes = download(url, MAX_PACKAGE_BYTES)?;
    verify_sha256(&bytes, expected)?;
    let package_type = skill
        .package_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| infer_package_type(url));
    install_package_bytes(dir, &bytes, package_type)
}

/// Unpacks a checked `zip` or `tar.gz` package and installs the folder in it
/// that holds SKILL.md as `dir`, replacing an earlier install only once the
/// package is unpacked.
fn install_package_bytes(dir: &Path, bytes: &[u8], package_type: &str) -> io::Result<()> {
    let temp_dir = staging_dir(dir, "extract");
    recreate_dir(&temp_dir)?;
    let extract_result = match package_type {
        "zip" => extract_zip(bytes, &temp_dir),
        "tar.gz" | "tgz" => extract_tar_gz(bytes, &temp_dir),
        other => {
            let _ = fs::remove_dir_all(&temp_dir);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported skill package type: {other}"),
            ));
        }
    };
    if let Err(error) = extract_result {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(error);
    }
    let Some(root) = find_skill_root(&temp_dir) else {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "skill package does not contain SKILL.md",
        ));
    };
    let staging = if root == temp_dir {
        temp_dir
    } else {
        let ready = staging_dir(dir, "ready");
        recreate_dir(&ready)?;
        if let Err(error) = copy_dir_contents(&root, &ready) {
            let _ = fs::remove_dir_all(&temp_dir);
            let _ = fs::remove_dir_all(&ready);
            return Err(error);
        }
        let _ = fs::remove_dir_all(&temp_dir);
        ready
    };
    atomic_replace_dir(&staging, dir)
}

fn download(url: &str, limit: usize) -> io::Result<Vec<u8>> {
    fetch(url, limit, INSTALL_TIMEOUTS)
}

/// GETs `url` (or reads a `file://` URL or a path), at most `limit` bytes,
/// through the proxy the environment names (HTTPS_PROXY and the like, as
/// web_fetch does; a SOCKS proxy is not supported, so it connects directly).
pub(crate) fn fetch(url: &str, limit: usize, timeouts: Timeouts) -> io::Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return read_bounded(fs::File::open(path)?, limit);
    }
    if !url.contains("://") {
        return read_bounded(fs::File::open(url)?, limit);
    }
    fetch_through(url, limit, timeouts, crate::web_fetch::env_proxy(url))
}

fn fetch_through(
    url: &str,
    limit: usize,
    timeouts: Timeouts,
    proxy: Option<String>,
) -> io::Result<Vec<u8>> {
    let builder = ureq::AgentBuilder::new()
        .timeout_connect(timeouts.connect)
        .timeout(timeouts.total);
    let proxy = proxy
        .filter(|proxy| !proxy.to_ascii_lowercase().starts_with("socks"))
        .and_then(|proxy| ureq::Proxy::new(proxy).ok());
    let agent = match proxy {
        Some(proxy) => builder.proxy(proxy).build(),
        None => builder.build(),
    };
    let response = agent
        .get(url)
        .set("User-Agent", "lynshen")
        .call()
        .map_err(|error| io::Error::other(request_error(url, error)))?;
    read_bounded(response.into_reader(), limit)
}

/// `raw.githubusercontent.com: connection failed (…)`: the host, not the
/// whole URL, so a source's warning stays one short line.
fn request_error(url: &str, error: ureq::Error) -> String {
    let host = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['/', '?'])
        .next()
        .unwrap_or_default();
    match error {
        ureq::Error::Status(code, _) => format!("{host} answered HTTP {code}"),
        ureq::Error::Transport(transport) => {
            let what = match transport.kind() {
                ureq::ErrorKind::Dns => "DNS lookup failed",
                ureq::ErrorKind::ConnectionFailed => "connection failed",
                ureq::ErrorKind::Io => "timed out or the connection broke",
                ureq::ErrorKind::ProxyConnect | ureq::ErrorKind::ProxyUnauthorized => {
                    "the proxy refused the connection"
                }
                _ => "request failed",
            };
            match transport.message() {
                Some(message) => format!("{host}: {what} ({message})"),
                None => format!("{host}: {what}"),
            }
        }
    }
}

fn read_bounded(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut reader = reader.take((limit + 1) as u64);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid_data(format!("download exceeds {limit} byte limit")));
    }
    Ok(bytes)
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn verify_sha256(bytes: &[u8], expected: &str) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "skill package sha256 mismatch: expected {}, got {}",
                expected.trim(),
                actual
            ),
        ))
    }
}

fn infer_package_type(url: &str) -> &str {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        "tar.gz"
    } else {
        "zip"
    }
}

pub(crate) fn extract_zip(bytes: &[u8], dest: &Path) -> io::Result<()> {
    extract_zip_within(bytes, dest, MAX_EXTRACTED_BYTES)
}

fn extract_zip_within(bytes: &[u8], dest: &Path, max_bytes: u64) -> io::Result<()> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(zip_err)?;
    if archive.len() > MAX_PACKAGE_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("skill package exceeds {MAX_PACKAGE_FILES} file limit"),
        ));
    }
    let mut extracted_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(zip_err)?;
        if let Some(mode) = file.unix_mode() {
            let file_type = mode & 0o170000;
            if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "skill package links and special files are not allowed",
                ));
            }
        }
        extracted_bytes = extracted_bytes.saturating_add(file.size());
        if extracted_bytes > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("extracted skill exceeds {max_bytes} byte limit"),
            ));
        }
        let path = safe_path_components(Path::new(file.name())).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe path in skill package: {}", file.name()),
            )
        })?;
        let out = dest.join(path);
        if file.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        // The size counted above is the one the archive declares; a deflated
        // entry can hold far more. Write no more than declared, then the
        // entry must end (reading to its end also checks its CRC).
        let declared = file.size();
        let mut output = fs::File::create(&out)?;
        let written = io::copy(&mut (&mut file).take(declared), &mut output)?;
        let mut rest = [0_u8; 1];
        if written != declared || file.read(&mut rest)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "skill package entry is not its declared size: {}",
                    file.name()
                ),
            ));
        }
        set_mode(&out, file.unix_mode())?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Permission bits only: no setuid, setgid or sticky bit from a package.
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
    Ok(())
}

pub(crate) fn extract_tar_gz(bytes: &[u8], dest: &Path) -> io::Result<()> {
    extract_tar_gz_within(bytes, dest, MAX_EXTRACTED_BYTES)
}

fn extract_tar_gz_within(bytes: &[u8], dest: &Path, max_bytes: u64) -> io::Result<()> {
    let gz = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(gz);
    let mut extracted_bytes = 0_u64;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= MAX_PACKAGE_FILES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("skill package exceeds {MAX_PACKAGE_FILES} file limit"),
            ));
        }
        let mut entry = entry?;
        let path = entry.path()?;
        let safe = safe_path_components(&path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe path in skill package: {}", path.display()),
            )
        })?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "skill package links and special files are not allowed",
            ));
        }
        // The size the entry's data is read with: a pax `size` record
        // overrides the header's, which may say 0.
        extracted_bytes = extracted_bytes.saturating_add(entry.size());
        if extracted_bytes > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("extracted skill exceeds {max_bytes} byte limit"),
            ));
        }
        let out = dest.join(safe);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(out)?;
    }
    Ok(())
}

fn safe_path_components(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn find_skill_root(dir: &Path) -> Option<PathBuf> {
    let skill = dir.join("SKILL.md");
    if skill.exists() {
        return Some(dir.to_path_buf());
    }
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_skill_root(&path) {
                return Some(found);
            }
        }
    }
    None
}

fn copy_dir_contents(src: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_contents(&src_path, &dest_path)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
            preserve_file_permissions(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

fn preserve_file_permissions(src: &Path, dest: &Path) -> io::Result<()> {
    fs::set_permissions(dest, fs::metadata(src)?.permissions())
}

fn recreate_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir_all(dir)
}

fn staging_dir(dir: &Path, label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("skill");
    dir.with_file_name(format!(".{name}-{label}-{nonce}"))
}

fn atomic_replace_dir(staging: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "skill has no parent"))?;
    fs::create_dir_all(parent)?;
    let backup = staging_dir(destination, "backup");
    let had_destination = destination.exists();
    if had_destination {
        fs::rename(destination, &backup)?;
    }
    if let Err(error) = fs::rename(staging, destination) {
        if had_destination {
            let _ = fs::rename(&backup, destination);
        }
        let _ = fs::remove_dir_all(staging);
        return Err(error);
    }
    if had_destination {
        let _ = fs::remove_dir_all(backup);
    }
    Ok(())
}

fn ensure_direct_child(parent: &Path, child: &Path) -> io::Result<()> {
    // `Path::parent` drops a trailing `..` or `.` as if it were a folder name,
    // so `<root>/..` would pass as a child of `<root>`: the last component
    // must be a plain name.
    let plain_name = matches!(
        child.components().next_back(),
        Some(std::path::Component::Normal(_))
    );
    if plain_name && child.parent() == Some(parent) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill path escapes skills directory",
        ))
    }
}

fn write_skill_state(profile_dir: &Path, value: &Value) -> io::Result<()> {
    fs::create_dir_all(profile_dir)?;
    let path = profile_dir.join(SKILL_STATE_FILE);
    let temp = profile_dir.join(format!(".{SKILL_STATE_FILE}.tmp"));
    fs::write(
        &temp,
        format!("{}\n", serde_json::to_string_pretty(&value)?),
    )?;
    match fs::rename(&temp, &path) {
        Ok(()) => Ok(()),
        Err(error) if path.exists() => {
            let backup = profile_dir.join(format!(".{SKILL_STATE_FILE}.backup"));
            let _ = fs::remove_file(&backup);
            fs::rename(&path, &backup)?;
            if let Err(move_error) = fs::rename(&temp, &path) {
                let _ = fs::rename(&backup, &path);
                let _ = fs::remove_file(&temp);
                return Err(io::Error::new(
                    move_error.kind(),
                    format!("{error}; replacement failed: {move_error}"),
                ));
            }
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(temp);
            Err(error)
        }
    }
}

fn zip_err(error: zip::result::ZipError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn normalized_content(skill: &MarketplaceSkill) -> String {
    let content = skill.content.trim_end();
    if content.starts_with("---") {
        format!("{content}\n")
    } else {
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{content}\n",
            skill.name, skill.description
        )
    }
}

fn validate_skill_id(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id.starts_with('-')
        || id.ends_with('-')
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe source skill id: {id}"),
        ));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> io::Result<&str> {
    if revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(revision)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source revision must be a 40-character Git commit SHA",
        ))
    }
}

fn encode_url_path(path: &str) -> String {
    let mut output = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn normalize_repository_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

fn github_repository_parts(url: &str) -> Result<(String, String), String> {
    let normalized = normalize_repository_url(url);
    let path = normalized
        .strip_prefix("https://github.com/")
        .ok_or_else(|| "extra_skills_source must be an HTTPS GitHub repository URL".to_string())?;
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err("extra_skills_source must name one GitHub owner/repository".to_string());
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

fn safe_skill_dir(id: &str) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            output.push('-');
            previous_dash = true;
        }
    }
    let output = output.trim_matches('-').to_string();
    if output.is_empty() {
        "skill".to_string()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::Digest;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn parses_enabled_marketplace_skills() {
        let marketplace = parse_marketplace(&json!({
            "skills": [
                { "id": "review", "name": "Review", "description": "Review code", "content": "body", "enabled": true },
                { "id": "off", "name": "Off", "description": "Hidden", "content": "body", "enabled": false },
                { "id": "pkg", "name": "Package", "description": "Package skill", "package_url": "https://example.com/skill.zip", "package_type": "zip", "enabled": true }
            ],
            "default_skill_ids": ["review", "off"]
        }))
        .unwrap();

        assert_eq!(marketplace.skills.len(), 2);
        assert_eq!(marketplace.skills[0].id, "review");
        assert_eq!(
            marketplace.skills[1].package_url.as_deref(),
            Some("https://example.com/skill.zip")
        );
        assert_eq!(marketplace.default_skill_ids, vec!["review", "off"]);
    }

    #[test]
    fn bundled_anthropic_index_is_pinned_and_marks_document_skills() {
        let source = anthropic_source().unwrap();

        assert_eq!(source.name, "anthropic");
        assert_eq!(source.repository, ANTHROPIC_SKILLS_URL);
        assert_eq!(source.revision.len(), 40);
        let skill = |id: &str| source.skills.iter().find(|skill| skill.id == id).unwrap();
        assert!(skill("mcp-builder").redistributable);
        assert!(!skill("mcp-builder").description.is_empty());
        let restricted = source
            .skills
            .iter()
            .filter(|skill| !skill.redistributable)
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(restricted, ["docx", "pdf", "pptx", "xlsx"]);
        assert_eq!(
            source.homepage("pdf"),
            format!("{ANTHROPIC_SKILLS_URL}/tree/{}/skills/pdf", source.revision)
        );
    }

    #[test]
    fn source_index_rejects_unsafe_ids_and_incomplete_entries() {
        let base = json!({
            "name": "test",
            "repository": "https://github.com/example/skills",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "skills": [{
                "id": "safe-skill", "name": "Safe", "description": "Safe skill",
                "license": "MIT", "redistributable": true
            }]
        });
        assert!(parse_source_index(&base).is_ok());

        for id in ["../escape", "nested/escape", "UPPER", "-dash"] {
            let mut unsafe_index = base.clone();
            unsafe_index["skills"][0]["id"] = json!(id);
            assert!(parse_source_index(&unsafe_index).is_err(), "{id}");
        }
        let mut no_license_flag = base.clone();
        no_license_flag["skills"][0]
            .as_object_mut()
            .unwrap()
            .remove("redistributable");
        assert!(parse_source_index(&no_license_flag).is_err());
        let mut branch = base;
        branch["revision"] = json!("main");
        assert!(parse_source_index(&branch).is_err());
    }

    #[test]
    fn installs_the_whole_github_skill_directory_at_the_pinned_revision() {
        let root = test_dir("lynshen-github-skill-test");
        let source = github_source();
        let tree = json!({ "truncated": false, "tree": [
            { "path": "skills/demo", "type": "tree", "mode": "040000" },
            { "path": "skills/demo/SKILL.md", "type": "blob", "mode": "100644", "size": 30 },
            { "path": "skills/demo/scripts", "type": "tree", "mode": "040000" },
            { "path": "skills/demo/scripts/run me.sh", "type": "blob", "mode": "100755", "size": 9 },
            { "path": "skills/other/SKILL.md", "type": "blob", "mode": "100644", "size": 5 }
        ]});
        let revision = source.revision.clone();
        let fetched = std::cell::RefCell::new(Vec::new());
        let fetch = |url: &str, _limit: usize| -> io::Result<Vec<u8>> {
            fetched.borrow_mut().push(url.to_string());
            let raw = format!("https://raw.githubusercontent.com/example/skills/{revision}/");
            match url.strip_prefix(&raw) {
                None => Ok(tree.to_string().into_bytes()),
                Some("skills/demo/SKILL.md") => Ok(b"---\nname: demo\n---\n".to_vec()),
                Some("skills/demo/scripts/run%20me.sh") => Ok(b"echo ok\n".to_vec()),
                Some(other) => panic!("unexpected download {other}"),
            }
        };

        let dir = install_github_skill(&root, &source, &source.skills[0], &fetch).unwrap();

        assert_eq!(dir, root.join("demo"));
        assert_eq!(
            fetched.borrow()[0],
            format!("https://api.github.com/repos/example/skills/git/trees/{revision}?recursive=1")
        );
        assert_eq!(fetched.borrow().len(), 3);
        assert!(skill_installed(&root, "demo"));
        assert_eq!(
            fs::read_to_string(dir.join("scripts/run me.sh")).unwrap(),
            "echo ok\n"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(dir.join("scripts/run me.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            installed_source(&root, "demo").as_deref(),
            Some("https://github.com/example/skills")
        );
        assert!(installed_from(
            &root,
            "demo",
            "https://github.com/example/skills/"
        ));

        // Another repository's skill of the same name does not replace it
        // (nothing is downloaded); the same repository updates it.
        let mut other = github_source();
        other.repository = "https://github.com/someone/else".to_string();
        let before = fetched.borrow().len();
        let error = install_github_skill(&root, &other, &other.skills[0], &fetch).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fetched.borrow().len(), before);
        assert!(!installed_from(&root, "demo", &other.repository));
        install_github_skill(&root, &source, &source.skills[0], &fetch).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn github_install_rejects_escaping_paths_and_keeps_the_existing_skill() {
        let root = test_dir("lynshen-github-escape-test");
        let installed = root.join("demo");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("SKILL.md"), "old content").unwrap();
        let source = github_source();
        for (path, kind) in [
            ("skills/demo/../../escape", "blob"),
            ("skills/demo/link", "commit"),
        ] {
            let tree = json!({ "tree": [
                { "path": "skills/demo/SKILL.md", "type": "blob", "mode": "100644", "size": 3 },
                { "path": path, "type": kind, "mode": "100644", "size": 3 }
            ]});
            let fetch = |_: &str, _: usize| Ok(tree.to_string().into_bytes());

            let error = install_github_skill(&root, &source, &source.skills[0], &fetch)
                .unwrap_err()
                .to_string();

            assert!(error.contains(path), "{error}");
        }
        let no_skill_file = json!({ "tree": [
            { "path": "skills/demo/README.md", "type": "blob", "mode": "100644", "size": 3 }
        ]});
        let fetch = |url: &str, _: usize| {
            Ok(if url.contains("api.github.com") {
                no_skill_file.to_string().into_bytes()
            } else {
                b"doc".to_vec()
            })
        };
        let error = install_github_skill(&root, &source, &source.skills[0], &fetch).unwrap_err();
        assert!(error.to_string().contains("SKILL.md"), "{error}");
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            "old content"
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installs_skill_file() {
        let root = test_dir("lynshen-marketplace-skill-test");
        let skill = MarketplaceSkill {
            id: "Code Review".to_string(),
            name: "Code Review".to_string(),
            description: "Review code".to_string(),
            content: "Be strict.".to_string(),
            package_url: None,
            package_sha256: None,
            package_type: None,
            tags: vec![],
            enabled: true,
            updated_at: String::new(),
        };

        let dir = install_marketplace_skill(&root.join("skills"), &skill).unwrap();

        assert_eq!(dir, root.join("skills").join("code-review"));
        let content = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(content.contains("name: Code Review"));
        assert!(content.contains("Be strict."));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extracts_zip_skill_package_contents() {
        let root = test_dir("lynshen-zip-skill-test");
        let package = root.join("skill.zip");
        fs::create_dir_all(&root).unwrap();
        create_zip(
            &package,
            &[
                (
                    "bundle/SKILL.md",
                    "---\nname: packaged\ndescription: Packaged skill\n---\n\nUse script.",
                ),
                ("bundle/scripts/run.sh", "#!/bin/sh\necho ok\n"),
            ],
        );
        let package_hash = format!("{:x}", sha2::Sha256::digest(fs::read(&package).unwrap()));
        let skill = MarketplaceSkill {
            id: "packaged".to_string(),
            name: "Packaged".to_string(),
            description: "Packaged skill".to_string(),
            content: String::new(),
            package_url: Some(format!("file://{}", package.display())),
            package_sha256: Some(package_hash),
            package_type: Some("zip".to_string()),
            tags: vec![],
            enabled: true,
            updated_at: String::new(),
        };

        install_marketplace_skill(&root.join("skills"), &skill).unwrap();

        assert!(root.join("skills/packaged/SKILL.md").exists());
        assert_eq!(
            fs::read_to_string(root.join("skills/packaged/scripts/run.sh")).unwrap(),
            "#!/bin/sh\necho ok\n"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(root.join("skills/packaged/scripts/run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_install_requires_sha256_and_preserves_existing_skill() {
        let root = test_dir("lynshen-package-sha-test");
        let package = root.join("skill.zip");
        let installed = root.join("skills/packaged");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("SKILL.md"), "old content").unwrap();
        create_zip(
            &package,
            &[(
                "SKILL.md",
                "---\nname: packaged\ndescription: New\n---\nnew",
            )],
        );
        let skill = MarketplaceSkill {
            id: "packaged".to_string(),
            name: "Packaged".to_string(),
            description: "Packaged skill".to_string(),
            content: String::new(),
            package_url: Some(format!("file://{}", package.display())),
            package_sha256: None,
            package_type: Some("zip".to_string()),
            tags: vec![],
            enabled: true,
            updated_at: String::new(),
        };

        let error = install_marketplace_skill(&root.join("skills"), &skill).unwrap_err();

        assert!(error.to_string().contains("package_sha256"));
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            "old content"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsafe_archive_path_is_rejected_without_replacing_existing_skill() {
        let root = test_dir("lynshen-package-escape-test");
        let package = root.join("skill.zip");
        let installed = root.join("skills/packaged");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("SKILL.md"), "old content").unwrap();
        create_zip(
            &package,
            &[
                (
                    "SKILL.md",
                    "---\nname: packaged\ndescription: New\n---\nnew",
                ),
                ("../escape.sh", "bad"),
            ],
        );
        let hash = format!("{:x}", sha2::Sha256::digest(fs::read(&package).unwrap()));
        let skill = MarketplaceSkill {
            id: "packaged".to_string(),
            name: "Packaged".to_string(),
            description: "Packaged skill".to_string(),
            content: String::new(),
            package_url: Some(format!("file://{}", package.display())),
            package_sha256: Some(hash),
            package_type: Some("zip".to_string()),
            tags: vec![],
            enabled: true,
            updated_at: String::new(),
        };

        let error = install_marketplace_skill(&root.join("skills"), &skill).unwrap_err();

        assert!(error.to_string().contains("unsafe path"));
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            "old content"
        );
        assert!(!root.join("escape.sh").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lifecycle_enable_disable_and_uninstall_updates_state() {
        let root = test_dir("lynshen-skill-lifecycle-test");
        let installed = root.join("skills/review");
        fs::create_dir_all(&installed).unwrap();
        fs::write(
            installed.join("SKILL.md"),
            "---\nname: review\ndescription: Review\n---\n",
        )
        .unwrap();

        assert!(skill_installed(&root.join("skills"), "review"));
        assert!(is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        set_skill_enabled(&root, "review", false).unwrap();
        assert!(!is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        assert_eq!(installed_skill_ids(&root).unwrap(), ["review (disabled)"]);
        set_skill_enabled(&root, "review", true).unwrap();
        assert!(is_skill_path_enabled(&root, &installed.join("SKILL.md")).unwrap());
        assert!(uninstall_skill(&root, "review").unwrap());
        assert!(!skill_installed(&root.join("skills"), "review"));
        assert!(!uninstall_skill(&root, "review").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn created_and_removed_skills_keep_to_their_folder() {
        let root = test_dir("lynshen-skill-create-test");
        let skills_dir = root.join("skills");
        let file =
            create_skill(&skills_dir, "release-notes", "Write release\n notes: short").unwrap();
        assert_eq!(file, skills_dir.join("release-notes/SKILL.md"));
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(
            crate::prompt::read_frontmatter_field(&text, "description").as_deref(),
            Some("Write release notes: short")
        );
        assert!(create_skill(&skills_dir, "release-notes", "again").is_err());
        assert!(create_skill(&skills_dir, "Release Notes", "x").is_err());
        assert!(create_skill(&skills_dir, "../escape", "x").is_err());
        assert!(create_skill(&skills_dir, "empty", " ").is_err());

        let dir = skills_dir.join("release-notes");
        set_skill_dir_enabled(&root, &dir, false).unwrap();
        // Only a skill folder directly in the root goes.
        assert!(remove_skill_dir(&root, &skills_dir, &root.join("elsewhere")).is_err());
        assert!(remove_skill_dir(&root, &skills_dir, &skills_dir).is_err());
        // `<root>/..` and `<root>/.` are not children of the root, even when
        // the folder they name holds a SKILL.md.
        fs::write(root.join("SKILL.md"), "---\nname: x\ndescription: y\n---\n").unwrap();
        assert!(remove_skill_dir(&root, &skills_dir, &skills_dir.join("..")).is_err());
        assert!(remove_skill_dir(&root, &skills_dir, &skills_dir.join(".")).is_err());
        assert!(root.join("SKILL.md").is_file());
        fs::create_dir_all(skills_dir.join("not-a-skill")).unwrap();
        assert!(remove_skill_dir(&root, &skills_dir, &skills_dir.join("not-a-skill")).is_err());
        remove_skill_dir(&root, &skills_dir, &dir).unwrap();
        assert!(!dir.exists());
        assert!(SkillState::read(&root).unwrap().disabled.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_download_has_a_hard_size_limit() {
        let error = read_bounded(
            Cursor::new(vec![0_u8; MAX_PACKAGE_BYTES + 1]),
            MAX_PACKAGE_BYTES,
        )
        .unwrap_err();
        assert!(error.to_string().contains("byte limit"));
    }

    /// A tar.gz of `(header, data)` pairs, written as given: the data need
    /// not match the header's size, and paths are not checked.
    fn raw_tar_gz(entries: Vec<(tar::Header, Vec<u8>)>) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (header, data) in entries {
            tar.append(&header, data.as_slice()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn tar_header(path: &str, kind: tar::EntryType, size: u64) -> tar::Header {
        let mut header = tar::Header::new_ustar();
        let name = &mut header.as_ustar_mut().unwrap().name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_entry_type(kind);
        header.set_size(size);
        header.set_mode(0o644);
        header.set_cksum();
        header
    }

    #[test]
    fn a_tar_package_is_held_to_its_real_size_and_carries_no_links() {
        let root = test_dir("lynshen-tar-limits-test");
        let skill = || {
            let text = b"---\nname: demo\ndescription: Demo\n---\n".to_vec();
            (
                tar_header("demo/SKILL.md", tar::EntryType::Regular, text.len() as u64),
                text,
            )
        };
        let unpack = |entries, limit| {
            let dest = root.join(format!("out-{}", entries_id()));
            fs::create_dir_all(&dest).unwrap();
            extract_tar_gz_within(&raw_tar_gz(entries), &dest, limit).map(|()| dest)
        };
        assert!(unpack(vec![skill()], 1024).is_ok());

        // A pax `size` record says 4096 while the header says 0: the limit
        // counts the 4096 bytes that would be written.
        let record = b"13 size=4096\n".to_vec();
        let pax = tar_header("pax", tar::EntryType::XHeader, record.len() as u64);
        let big = tar_header("demo/big.bin", tar::EntryType::Regular, 0);
        let error = unpack(vec![skill(), (pax, record), (big, vec![0; 4096])], 1024).unwrap_err();
        assert!(error.to_string().contains("byte limit"), "{error}");

        for (path, kind) in [
            ("demo/link", tar::EntryType::Symlink),
            ("demo/hard", tar::EntryType::Link),
            ("demo/fifo", tar::EntryType::Fifo),
        ] {
            let mut link = tar_header(path, kind, 0);
            link.set_link_name("../../outside").unwrap();
            link.set_cksum();
            let error = unpack(vec![skill(), (link, Vec::new())], 1024).unwrap_err();
            assert!(error.to_string().contains("links"), "{path}: {error}");
        }
        for path in ["../escape.sh", "/abs/escape.sh", "demo/../../escape.sh"] {
            let entry = (
                tar_header(path, tar::EntryType::Regular, 3),
                b"bad".to_vec(),
            );
            let error = unpack(vec![skill(), entry], 1024).unwrap_err();
            assert!(error.to_string().contains("unsafe path"), "{path}: {error}");
        }
        assert!(!root.join("escape.sh").exists());
        let _ = fs::remove_dir_all(root);
    }

    fn entries_id() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn a_zip_package_is_held_to_its_real_size() {
        let root = test_dir("lynshen-zip-limits-test");
        fs::create_dir_all(&root).unwrap();
        let package = root.join("skill.zip");
        {
            let mut zip = zip::ZipWriter::new(fs::File::create(&package).unwrap());
            let stored = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(0o755);
            zip.start_file("demo/SKILL.md", stored).unwrap();
            zip.write_all(b"---\nname: demo\ndescription: Demo\n---\n")
                .unwrap();
            zip.start_file("demo/big.bin", stored).unwrap();
            zip.write_all(&[0; 4096]).unwrap();
            zip.finish().unwrap();
        }
        let mut honest = fs::read(&package).unwrap();
        // big.bin's central directory record.
        let entry = honest
            .windows(4)
            .enumerate()
            .filter(|(_, window)| *window == b"PK\x01\x02")
            .map(|(at, _)| at)
            .find(|&at| honest[at + 46..].starts_with(b"demo/big.bin"))
            .unwrap();
        // A setuid file: the bit is dropped.
        honest[entry + 38..entry + 42].copy_from_slice(&(0o104755_u32 << 16).to_le_bytes());
        let dest = root.join("honest");
        fs::create_dir_all(&dest).unwrap();
        extract_zip_within(&honest, &dest, 1 << 20).unwrap();
        assert_eq!(fs::read(dest.join("demo/big.bin")).unwrap().len(), 4096);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(dest.join("demo/big.bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );

        // The central directory says big.bin holds 1 byte: it holds 4096.
        let mut lying = honest.clone();
        lying[entry + 24..entry + 28].copy_from_slice(&1_u32.to_le_bytes());
        let dest = root.join("lying");
        fs::create_dir_all(&dest).unwrap();
        let error = extract_zip_within(&lying, &dest, 1 << 20).unwrap_err();
        assert!(error.to_string().contains("declared size"), "{error}");
        assert!(fs::metadata(dest.join("demo/big.bin")).unwrap().len() <= 1);
        let _ = fs::remove_dir_all(root);
    }

    fn github_source() -> SkillSource {
        SkillSource {
            name: "example".to_string(),
            repository: "https://github.com/example/skills".to_string(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
            dir: "skills".to_string(),
            skills: vec![SourceSkill {
                id: "demo".to_string(),
                name: "Demo".to_string(),
                description: "Demo skill".to_string(),
                tags: Vec::new(),
                license: "MIT".to_string(),
                redistributable: true,
            }],
        }
    }

    fn test_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{}-{}",
            prefix,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn create_zip(path: &Path, files: &[(&str, &str)]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, content) in files {
            let opts = if name.ends_with(".sh") {
                zip::write::FileOptions::default().unix_permissions(0o755)
            } else {
                zip::write::FileOptions::default().unix_permissions(0o644)
            };
            zip.start_file(*name, opts).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn a_repository_whose_skill_files_cannot_be_read_is_an_error_not_a_bare_list() {
        let tree = json!({ "tree": [
            { "path": "skills/one/SKILL.md", "type": "blob" },
            { "path": "skills/two/SKILL.md", "type": "blob" },
            { "path": "README.md", "type": "blob" }
        ]})
        .to_string();
        let blocked = |url: &str, _: usize| -> io::Result<Vec<u8>> {
            if let Some(answer) = github_api(url, &tree) {
                Ok(answer.into_bytes())
            } else {
                Err(io::Error::other(
                    "raw.githubusercontent.com: connection failed",
                ))
            }
        };
        let error = read_github_source("Example", "https://github.com/example/skills", &blocked)
            .unwrap_err();
        assert!(error.contains("raw.githubusercontent.com"), "{error}");

        // One file read: the source lists both, the other without a description.
        let partly = |url: &str, _: usize| -> io::Result<Vec<u8>> {
            if let Some(answer) = github_api(url, &tree) {
                Ok(answer.into_bytes())
            } else if url.ends_with("/skills/one/SKILL.md") {
                Ok(b"---\nname: One\ndescription: The first\n---\n".to_vec())
            } else {
                Err(io::Error::other("timed out"))
            }
        };
        let source =
            read_github_source("Example", "https://github.com/example/skills", &partly).unwrap();
        assert_eq!(source.dir, "skills");
        assert_eq!(source.skills.len(), 2);
        assert_eq!(source.skills[0].name, "One");
        assert_eq!(source.skills[0].description, "The first");
        assert_eq!(source.skills[1].name, "two");
        assert!(source.skills[1].redistributable);
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// GitHub's API for example/skills, MIT-licensed, default branch `main`
    /// at SHA, its tree `tree`; None for any other URL.
    fn github_api(url: &str, tree: &str) -> Option<String> {
        let api = "https://api.github.com/repos/example/skills";
        if url == api {
            Some(json!({ "default_branch": "main", "license": { "spdx_id": "MIT" } }).to_string())
        } else if url == format!("{api}/branches/main") {
            Some(json!({ "commit": { "sha": SHA } }).to_string())
        } else if url == format!("{api}/git/trees/{SHA}?recursive=1") {
            Some(tree.to_string())
        } else {
            None
        }
    }

    #[test]
    fn a_github_source_is_read_at_one_commit_with_each_skill_license() {
        let tree = json!({ "tree": [
            { "path": "skills/open/SKILL.md", "type": "blob" },
            { "path": "skills/closed/SKILL.md", "type": "blob" },
            { "path": "skills/apache/SKILL.md", "type": "blob" }
        ]})
        .to_string();
        let raw = format!("https://raw.githubusercontent.com/example/skills/{SHA}/skills/");
        let fetched = std::sync::Mutex::new(Vec::new());
        let fetch = |url: &str, _limit: usize| -> io::Result<Vec<u8>> {
            fetched.lock().unwrap().push(url.to_string());
            let body = github_api(url, &tree).unwrap_or_else(|| match url.strip_prefix(&raw) {
                Some("open/SKILL.md") => "---\nname: Open\n---\n".to_string(),
                Some("closed/SKILL.md") => {
                    "---\nname: Closed\nlicense: Proprietary. LICENSE.txt has complete terms\n---\n"
                        .to_string()
                }
                Some("apache/SKILL.md") => {
                    "---\nname: Apache\nlicense: Apache License 2.0\n---\n".to_string()
                }
                _ => panic!("unexpected download {url}"),
            });
            Ok(body.into_bytes())
        };

        let source =
            read_github_source("Example", "https://github.com/example/skills", &fetch).unwrap();

        assert_eq!(source.revision, SHA);
        assert_eq!(source.dir, "skills");
        let skill = |id: &str| source.skills.iter().find(|skill| skill.id == id).unwrap();
        // No license of its own: the repository's.
        assert_eq!(skill("open").license, "MIT");
        assert!(skill("open").redistributable);
        assert!(!skill("closed").redistributable);
        assert_eq!(skill("apache").license, "Apache License 2.0");
        assert!(skill("apache").redistributable);
        // Nothing read at a moving branch.
        assert!(!fetched
            .lock()
            .unwrap()
            .iter()
            .any(|url| url.contains("/HEAD")));
        assert_eq!(
            source.homepage("open"),
            format!("https://github.com/example/skills/tree/{SHA}/skills/open")
        );
    }

    #[test]
    fn proprietary_and_source_available_terms_are_not_offered() {
        for license in [
            "MIT",
            "MIT License",
            "Apache-2.0",
            "apache license, version 2.0",
            "BSD-3-Clause",
            // States no terms: installed from the author's repository.
            "",
        ] {
            assert!(offered_license(license), "{license}");
        }
        for license in [
            "Proprietary",
            "Proprietary. LICENSE.txt has complete terms",
            "Anthropic source-available",
        ] {
            assert!(!offered_license(license), "{license}");
        }
    }

    #[test]
    fn a_source_survives_the_cache_format() {
        let mut source = github_source();
        source.dir = String::new();
        let parsed = parse_source(&source_json(&source)).unwrap();
        assert_eq!(parsed, source);
        let mut unsafe_dir = source_json(&source);
        unsafe_dir["dir"] = json!("skills/../..");
        assert!(parse_source(&unsafe_dir).is_err());
        let mut unsafe_id = source_json(&source);
        unsafe_id["skills"][0]["id"] = json!("../x");
        assert!(parse_source(&unsafe_id).is_err());
        let mut branch = source_json(&source);
        branch["revision"] = json!("main");
        assert!(parse_source(&branch).is_err());
    }

    /// Accepts connections on a loopback port; `answer` writes the reply to
    /// each request line it reads (or nothing, to stall).
    fn listen(answer: fn(&str) -> Option<String>) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (seen, requests) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                let _ = seen.send(request.trim_end().to_string());
                match answer(&request) {
                    Some(reply) => {
                        let _ = stream.write_all(reply.as_bytes());
                    }
                    None => std::thread::sleep(Duration::from_secs(30)),
                }
            }
        });
        (address, requests)
    }

    #[test]
    fn a_silent_server_times_out_instead_of_holding_the_catalog() {
        let (address, _) = listen(|_| None);
        let short = Timeouts {
            connect: Duration::from_secs(1),
            total: Duration::from_secs(1),
        };
        let started = std::time::Instant::now();
        let error = fetch(&format!("http://{address}/tree"), 1024, short).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "{error}");
        assert!(error.to_string().starts_with("127.0.0.1:"), "{error}");
    }

    #[test]
    fn requests_go_through_the_proxy_the_environment_names() {
        let (proxy, requests) = listen(|_| {
            Some("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string())
        });
        let bytes = fetch_through(
            "http://skills.example.invalid/catalog.json",
            1024,
            CATALOG_TIMEOUTS,
            Some(format!("http://{proxy}")),
        )
        .unwrap();
        assert_eq!(bytes, b"ok");
        assert_eq!(
            requests.recv().unwrap(),
            "GET http://skills.example.invalid/catalog.json HTTP/1.1"
        );
        // ureq cannot speak SOCKS: such a proxy is passed over, not a failure
        // of every request.
        let error = fetch_through(
            "http://127.0.0.1:9/x",
            1024,
            CATALOG_TIMEOUTS,
            Some("socks5://127.0.0.1:1080".to_string()),
        )
        .unwrap_err();
        assert!(!error.to_string().contains("proxy"), "{error}");
    }

    #[test]
    fn the_mirror_builds_the_community_sources_the_engine_lists() {
        let built: Value =
            serde_json::from_str(include_str!("../../../deploy/skills-mirror/sources.json"))
                .unwrap();
        let built: Vec<(&str, &str)> = built
            .as_array()
            .unwrap()
            .iter()
            .map(|source| {
                (
                    source["name"].as_str().unwrap(),
                    source["repository"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(built, COMMUNITY_SKILL_SOURCES);
    }

    /// Network: the community repositories list their skills with metadata.
    #[test]
    #[ignore]
    fn community_sources_list_skills() {
        for (name, repository) in COMMUNITY_SKILL_SOURCES {
            let source = fetch_github_source(name, repository).unwrap();
            eprintln!(
                "{name}: dir={:?} {} skills",
                source.dir,
                source.skills.len()
            );
            for skill in source.skills.iter().take(3) {
                eprintln!(
                    "  {} | {} | {:.60}",
                    skill.id, skill.name, skill.description
                );
            }
            assert!(!source.skills.is_empty());
            assert!(source
                .skills
                .iter()
                .any(|skill| !skill.description.is_empty()));
        }
    }
}

//! Skills for the desktop: the marketplace (github.com/anthropics/skills and
//! community skill repositories in one catalog, installed into the personal
//! skills directory of the session's engine) and the skills on this computer
//! (listed with their switches and load problems, switched, created and
//! deleted).

use crate::engines;
use lynshen_agent_core::skills::{self, SkillInventory, SkillScope};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn handle(name: &str, op: &Value) -> Result<Value, String> {
    let backend = op["backend"].as_str().unwrap_or("lynshen");
    let text = |key: &str| {
        op[key]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{name} requires {key}"))
    };
    // The project whose own skills to include: an absolute folder.
    let cwd = op["cwd"]
        .as_str()
        .map(Path::new)
        .filter(|cwd| cwd.is_absolute());
    match name {
        "skills_catalog" => catalog(install_dir(backend)?),
        "skill_install" => install(install_dir(backend)?, text("source")?, text("skill")?)
            .map(|path| json!({ "type": "skill_installed", "path": path })),
        "skills_list" => list(backend, cwd),
        "skill_set_enabled" => {
            let enabled = op["enabled"]
                .as_bool()
                .ok_or("skill_set_enabled requires enabled")?;
            set_enabled(backend, cwd, Path::new(text("path")?), enabled)
        }
        "skill_uninstall" => {
            // A listed skill by its folder, or a catalog skill by its id.
            let dir = match op["path"].as_str() {
                Some(path) => PathBuf::from(path),
                None => install_dir(backend)?.join(catalog_id(text("skill")?)?),
            };
            uninstall(backend, cwd, &dir)
                .map(|()| json!({ "type": "skill_uninstalled", "path": dir }))
        }
        "skill_create" => skills::create_skill(
            &skills::profile_skills_dir().map_err(|error| error.to_string())?,
            text("name")?,
            op["description"].as_str().unwrap_or_default(),
        )
        .map(|path| json!({ "type": "skill_created", "path": path }))
        .map_err(|error| error.to_string()),
        _ => Err(format!("unknown op {name}")),
    }
}

/// Claude Code reads `~/.claude/skills`; every other engine runs with the
/// LynShen profile's skills.
fn install_dir(backend: &str) -> Result<PathBuf, String> {
    if backend != "claude" {
        return skills::profile_skills_dir().map_err(|error| error.to_string());
    }
    Ok(claude_dir()?.join("skills"))
}

fn claude_dir() -> Result<PathBuf, String> {
    let home = engines::home();
    if home.as_os_str().is_empty() {
        return Err("home directory not found".to_string());
    }
    Ok(home.join(".claude"))
}

/// A catalog id names one folder in the install directory.
fn catalog_id(id: &str) -> Result<&str, String> {
    let safe = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !id.starts_with('.');
    safe.then_some(id)
        .ok_or_else(|| format!("unsafe skill id: {id}"))
}

/// The folders an engine loads skills from, the personal one first.
fn roots(backend: &str, cwd: Option<&Path>) -> Result<Vec<(SkillScope, PathBuf)>, String> {
    let mut roots = vec![(SkillScope::User, install_dir(backend)?)];
    if backend == "claude" {
        roots.extend(cwd.map(|cwd| (SkillScope::Project, cwd.join(".claude").join("skills"))));
        return Ok(roots);
    }
    let home = engines::home();
    if !home.as_os_str().is_empty() {
        roots.push((SkillScope::Shared, home.join(".agents").join("skills")));
    }
    if let Some(cwd) = cwd {
        roots.push((SkillScope::Project, cwd.join(".lynshen").join("skills")));
        roots.push((SkillScope::Project, cwd.join(".agents").join("skills")));
    }
    Ok(roots)
}

fn inventory(backend: &str, cwd: Option<&Path>) -> Result<SkillInventory, String> {
    if backend == "claude" {
        return Ok(skills::skills_in(&roots(backend, cwd)?));
    }
    skills::installed_skills(cwd).map_err(|error| error.to_string())
}

/// The skills an engine finds, each with its switch (LynShen's only) and
/// whether it can be deleted here: a folder of the personal directory. A
/// skill in `~/.agents/skills` is other tools' too, a project's belongs to
/// the project.
fn list(backend: &str, cwd: Option<&Path>) -> Result<Value, String> {
    let inventory = inventory(backend, cwd)?;
    let roots = roots(backend, cwd)?;
    let personal = &roots[0].1;
    let skills: Vec<Value> = inventory
        .skills
        .iter()
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "path": skill.dir,
                "scope": skill.scope.as_str(),
                "enabled": skill.enabled,
                "toggleable": backend != "claude",
                "removable": skill.scope == SkillScope::User && skill.dir.parent() == Some(personal.as_path()),
            })
        })
        .collect();
    let problems: Vec<Value> = inventory
        .problems
        .iter()
        .map(|problem| {
            json!({
                "level": if problem.error { "error" } else { "warning" },
                "kind": problem.kind,
                "path": problem.path,
                "message": problem.message,
            })
        })
        .collect();
    let dirs: Vec<Value> = roots
        .iter()
        .map(|(scope, path)| json!({ "scope": scope.as_str(), "path": path }))
        .collect();
    Ok(json!({
        "type": "skills_list",
        "backend": backend,
        "skills": skills,
        "problems": problems,
        "dirs": dirs,
        "projectTrusted": inventory.project_trusted,
    }))
}

/// Switches a skill the list has (nothing else is written to the switches),
/// then lists again. Claude Code's skills are Claude Code's to switch.
fn set_enabled(
    backend: &str,
    cwd: Option<&Path>,
    dir: &Path,
    enabled: bool,
) -> Result<Value, String> {
    if backend == "claude" {
        return Err("LynShen does not switch Claude Code's skills".to_string());
    }
    let found = inventory(backend, cwd)?
        .skills
        .into_iter()
        .find(|skill| skill.dir == dir)
        .ok_or_else(|| format!("no such skill: {}", dir.display()))?;
    skills::switch_skill(&found.dir, enabled).map_err(|error| error.to_string())?;
    list(backend, cwd)
}

fn uninstall(backend: &str, cwd: Option<&Path>, dir: &Path) -> Result<(), String> {
    let roots = roots(backend, cwd)?;
    let personal = &roots[0].1;
    if dir.parent() != Some(personal.as_path()) {
        return Err(format!(
            "only a skill in {} can be deleted here: {}",
            personal.display(),
            dir.display()
        ));
    }
    skills::delete_skill(personal, dir).map_err(|error| error.to_string())
}

/// The community repositories, fetched at most once an hour: listing one
/// reads every SKILL.md it has.
fn community_sources() -> Vec<(String, Result<skills::SkillSource, String>)> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    type Cached = (Instant, Result<skills::SkillSource, String>);
    static CACHE: Mutex<Vec<(String, Cached)>> = Mutex::new(Vec::new());
    const FRESH: Duration = Duration::from_secs(3600);
    // `LYNSHEN_SKILL_SOURCES="Name=https://github.com/o/r;…"` replaces the
    // list (empty: none, as the tests run offline).
    let configured: Vec<(String, String)> = match std::env::var("LYNSHEN_SKILL_SOURCES") {
        Ok(list) => list
            .split(';')
            .filter_map(|item| item.split_once('='))
            .map(|(name, repository)| (name.trim().to_string(), repository.trim().to_string()))
            .collect(),
        Err(_) => skills::COMMUNITY_SKILL_SOURCES
            .iter()
            .map(|(name, repository)| (name.to_string(), repository.to_string()))
            .collect(),
    };
    configured
        .iter()
        .map(|(name, repository)| {
            let cached = CACHE.lock().ok().and_then(|cache| {
                cache
                    .iter()
                    .find(|(repo, (at, result))| {
                        repo == repository && (result.is_ok() && at.elapsed() < FRESH)
                    })
                    .map(|(_, (_, result))| result.clone())
            });
            let result = cached.unwrap_or_else(|| {
                let result = skills::fetch_github_source(name, repository);
                if let Ok(mut cache) = CACHE.lock() {
                    cache.retain(|(repo, _)| repo != repository);
                    cache.push((repository.clone(), (Instant::now(), result.clone())));
                }
                result
            });
            (name.clone(), result)
        })
        .collect()
}

/// A catalog entry's `source`: `anthropic`, or a community repository's
/// `owner/repo`.
fn source_key(repository: &str) -> String {
    repository
        .trim_start_matches("https://github.com/")
        .to_string()
}

/// Anthropic's catalog is bundled; a community repository that cannot be read
/// is only a warning.
fn catalog(dir: PathBuf) -> Result<Value, String> {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let mut add = |source: &skills::SkillSource, key: &str, shown: &str| {
        entries.extend(source.skills.iter().map(|skill| {
            json!({
                "id": skill.id,
                "name": skill.name,
                "description": skill.description,
                "tags": skill.tags,
                "source": key,
                "sourceName": shown,
                "isDefault": false,
                "installed": skills::skill_installed(&dir, &skill.id),
                "license": skill.license,
                "redistributable": skill.redistributable,
                "homepage": source.homepage(&skill.id),
            })
        }))
    };
    add(&skills::anthropic_source()?, "anthropic", "Anthropic");
    for (name, result) in community_sources() {
        match result {
            Ok(source) => add(&source, &source_key(&source.repository), &source.name),
            Err(error) => warnings.push(format!("{name}: {error}")),
        }
    }
    let key = |entry: &Value| {
        (
            entry["source"] != "anthropic",
            entry["sourceName"].as_str().unwrap_or_default().to_string(),
            entry["name"].as_str().unwrap_or_default().to_lowercase(),
        )
    };
    entries.sort_by_key(key);
    Ok(json!({
        "type": "skills_catalog",
        "skills": entries,
        "warnings": warnings,
        "installDir": dir,
    }))
}

/// The skill is looked up again here rather than trusting anything the
/// client sent beyond its source and id. Anthropic's source-available skills
/// are listed but not installed, as in the TUI's `/skills`.
fn install(dir: PathBuf, source: &str, id: &str) -> Result<PathBuf, String> {
    let found = if source == "anthropic" {
        skills::anthropic_source()?
    } else {
        community_sources()
            .into_iter()
            .find_map(|(_, result)| result.ok().filter(|s| source_key(&s.repository) == source))
            .ok_or_else(|| format!("unknown skill source: {source}"))?
    };
    let skill = found
        .skills
        .iter()
        .find(|skill| skill.id == id)
        .ok_or_else(|| format!("skill not found in {}: {id}", found.name))?;
    if !skill.redistributable {
        return Err(format!(
            "skill {id} is not offered: {}; not redistributed by LynShen",
            skill.license
        ));
    }
    skills::install_source_skill(&dir, &found, skill).map_err(|error| error.to_string())
}

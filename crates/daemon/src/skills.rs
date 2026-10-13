//! Skills for the desktop: the marketplace (github.com/anthropics/skills and
//! community skill repositories in one catalog, installed into the personal
//! skills directory of the session's engine; see `skill_market` for how the
//! community sources are cached and read) and the skills on this computer
//! (listed with their switches and load problems, switched, created and
//! deleted).

use crate::{engines, skill_market};
use lynshen_agent_core::skills::{self, mirror, SkillInventory, SkillScope};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

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
        "skills_catalog" => catalog(install_dir(backend)?, op),
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

/// A catalog entry's `source`: `anthropic`, or a community repository's
/// `owner/repo`.
fn source_key(repository: &str) -> String {
    repository
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_start_matches("https://github.com/")
        .to_string()
}

/// How long `wait: true` waits for a refresh: the mirror's short timeouts
/// plus GitHub's tree and SKILL.md reads.
const WAIT_LIMIT: Duration = Duration::from_secs(60);

/// Anthropic's bundled index and the community sources as cached, at once.
/// A source that is missing or old is read again in the background
/// (`refreshing`); `refresh: true` reads every source again now, and
/// `wait: true` answers once that refresh has ended. A source that cannot be
/// read is a warning of its own, beside the skills it had.
fn catalog(dir: PathBuf, op: &Value) -> Result<Value, String> {
    let configured = skill_market::configured();
    let mut cached = skill_market::read();
    let all = op["refresh"] == true;
    if !configured.is_empty()
        && (all || skill_market::due(&cached, &configured, skill_market::now_ms()))
    {
        skill_market::start_refresh(configured.clone(), all);
    }
    if op["wait"] == true {
        skill_market::wait_refresh(WAIT_LIMIT);
        cached = skill_market::read();
    }
    let mut entries = Vec::new();
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
    let mut sources = vec![json!({
        "key": "anthropic",
        "name": "Anthropic",
        "origin": "bundled",
        "fetchedAt": null,
        "error": null,
    })];
    let mut warnings = Vec::new();
    for (name, repository) in &configured {
        let key = source_key(repository);
        let entry = skill_market::find(&cached, repository);
        if let Some(entry) = entry {
            add(&entry.source, &key, name);
            if let Some(error) = &entry.error {
                warnings.push(format!("{name}: {error}"));
            }
        }
        sources.push(json!({
            "key": key,
            "name": name,
            "origin": entry.and_then(|entry| entry.origin.clone()),
            "fetchedAt": entry.and_then(|entry| entry.fetched_at),
            "error": entry.and_then(|entry| entry.error.clone()),
        }));
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
        "sources": sources,
        "refreshing": skill_market::refreshing(),
        "installDir": dir,
    }))
}

/// The skill is looked up again here rather than trusting anything the
/// client sent beyond its source and id: in the bundled index, or in a
/// configured community source as cached (read first when the cache has
/// none). Anthropic's source-available skills are listed but not installed,
/// as in the TUI's `/skills`. The LynShen mirror's checked archive comes
/// first; GitHub is the fallback.
fn install(dir: PathBuf, source: &str, id: &str) -> Result<PathBuf, String> {
    let found = if source == "anthropic" {
        skills::anthropic_source()?
    } else {
        let configured = skill_market::configured();
        let repository = configured
            .iter()
            .map(|(_, repository)| repository.clone())
            .find(|repository| source_key(repository) == source)
            .ok_or_else(|| format!("unknown skill source: {source}"))?;
        let cached_source = || {
            let cached = skill_market::read();
            skill_market::find(&cached, &repository)
                .filter(|entry| !entry.source.skills.is_empty())
                .map(|entry| entry.source.clone())
        };
        match cached_source() {
            Some(found) => found,
            None => {
                skill_market::start_refresh(configured, false);
                skill_market::wait_refresh(WAIT_LIMIT);
                cached_source().ok_or_else(|| format!("cannot read skill source {source}"))?
            }
        }
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
    let mut mirror_error = None;
    if let Some(url) = mirror::mirror_url() {
        let installed = mirror::fetch_mirror(&url).and_then(|published| {
            mirror::install_from_mirror(&published, &dir, &found.repository, id)
                .map_err(|error| error.to_string())
        });
        match installed {
            Ok(path) => return Ok(path),
            Err(error) => mirror_error = Some(error),
        }
    }
    skills::install_source_skill(&dir, &found, skill).map_err(|error| match mirror_error {
        Some(mirror_failed) => format!("{error} (LynShen mirror: {mirror_failed})"),
        None => error.to_string(),
    })
}

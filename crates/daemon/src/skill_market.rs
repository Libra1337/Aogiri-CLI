//! The marketplace's community sources, kept on disk so the desktop's
//! catalog opens at once: `~/.lynshen/cache/skills-catalog.json` holds each
//! source's skills with when they were read, and the last error reading it.
//! One background refresh at a time reads them again, from the LynShen
//! mirror first and from GitHub for what the mirror does not have, with
//! short timeouts. A source that fails keeps its cached skills.

use lynshen_agent_core::skills::{self, mirror, SkillSource};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::{Condvar, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// A source read this long ago is read again on the next catalog.
const FRESH: Duration = Duration::from_secs(3600);
/// A source that failed is tried again on its own only after this long; the
/// desktop's refresh tries at once.
const RETRY_FAILED: Duration = Duration::from_secs(600);

/// One community source as the cache keeps it.
#[derive(Debug, Clone)]
pub struct Cached {
    /// The configured name and repository; no skills until one read worked.
    pub source: SkillSource,
    /// `mirror` or `github`: where the skills were read.
    pub origin: Option<String>,
    /// When the skills were read (ms since the epoch).
    pub fetched_at: Option<u64>,
    /// When reading was last tried (ms since the epoch).
    pub checked_at: u64,
    /// Why the last try failed.
    pub error: Option<String>,
}

/// The community repositories: (shown name, repository).
/// `LYNSHEN_SKILL_SOURCES="Name=https://github.com/o/r;…"` replaces the list
/// (empty: none, as the tests run offline).
pub fn configured() -> Vec<(String, String)> {
    match std::env::var("LYNSHEN_SKILL_SOURCES") {
        Ok(list) => list
            .split(';')
            .filter_map(|item| item.split_once('='))
            .map(|(name, repository)| (name.trim().to_string(), repository.trim().to_string()))
            .collect(),
        Err(_) => skills::COMMUNITY_SKILL_SOURCES
            .iter()
            .map(|(name, repository)| (name.to_string(), repository.to_string()))
            .collect(),
    }
}

fn cache_file() -> Option<PathBuf> {
    let skills_dir = skills::profile_skills_dir().ok()?;
    Some(
        skills_dir
            .parent()?
            .join("cache")
            .join("skills-catalog.json"),
    )
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn same_repository(left: &str, right: &str) -> bool {
    let normal = |url: &str| {
        url.trim()
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .to_ascii_lowercase()
    };
    normal(left) == normal(right)
}

/// The cached sources; an unreadable cache is an empty one.
pub fn read() -> Vec<Cached> {
    let Some(text) = cache_file().and_then(|file| fs::read_to_string(file).ok()) else {
        return Vec::new();
    };
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    value["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let source = skills::parse_source(item)
                .ok()
                .or_else(|| never_read(item))?;
            Some(Cached {
                source,
                origin: item["origin"].as_str().map(str::to_string),
                fetched_at: item["fetchedAt"].as_u64(),
                checked_at: item["checkedAt"].as_u64().unwrap_or(0),
                error: item["error"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// A source no read has worked for (one configured with a repository that is
/// not on GitHub, say) keeps only its name and error: with no skills,
/// nothing is built from its repository.
fn never_read(item: &Value) -> Option<SkillSource> {
    let empty = item["skills"].as_array().is_some_and(Vec::is_empty);
    empty.then(|| SkillSource {
        name: item["name"].as_str().unwrap_or_default().to_string(),
        repository: item["repository"].as_str().unwrap_or_default().to_string(),
        revision: String::new(),
        dir: String::new(),
        skills: Vec::new(),
    })
}

fn write(cached: &[Cached]) -> Result<(), String> {
    let file = cache_file().ok_or("home directory not found")?;
    let sources: Vec<Value> = cached
        .iter()
        .map(|entry| {
            let mut value = skills::source_json(&entry.source);
            value["origin"] = json!(entry.origin);
            value["fetchedAt"] = json!(entry.fetched_at);
            value["checkedAt"] = json!(entry.checked_at);
            value["error"] = json!(entry.error);
            value
        })
        .collect();
    let text = serde_json::to_string(&json!({ "version": 1, "sources": sources }))
        .map_err(|error| error.to_string())?;
    let dir = file.parent().ok_or("cache file has no folder")?;
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let temp = dir.join(format!(".skills-catalog-{}.tmp", std::process::id()));
    fs::write(&temp, text).map_err(|error| error.to_string())?;
    fs::rename(&temp, &file).map_err(|error| {
        let _ = fs::remove_file(&temp);
        error.to_string()
    })
}

/// The cache entry for a configured source.
pub fn find<'a>(cached: &'a [Cached], repository: &str) -> Option<&'a Cached> {
    cached
        .iter()
        .find(|entry| same_repository(&entry.source.repository, repository))
}

/// Whether a source is missing, old, or failed long enough ago.
fn stale(entry: Option<&Cached>, now: u64) -> bool {
    let Some(entry) = entry else {
        return true;
    };
    let wait = if entry.error.is_some() {
        RETRY_FAILED
    } else {
        FRESH
    };
    now.saturating_sub(entry.checked_at) >= wait.as_millis() as u64
}

/// Whether any configured source is [`stale`].
pub fn due(cached: &[Cached], configured: &[(String, String)], now: u64) -> bool {
    configured
        .iter()
        .any(|(_, repository)| stale(find(cached, repository), now))
}

/// Reads the stale sources again (every one when `all`) and stores the
/// result: the mirror's copy where it has one, GitHub (those sources at
/// once) for the rest. A source that fails keeps the skills it had.
fn refresh(configured: &[(String, String)], all: bool) {
    let old = read();
    let started = now_ms();
    let mut rest: Vec<usize> = (0..configured.len())
        .filter(|&at| all || stale(find(&old, &configured[at].1), started))
        .collect();
    if rest.is_empty() {
        return;
    }
    let mut fresh: Vec<(usize, Result<(SkillSource, &str), String>)> = Vec::new();
    let mut mirror_error = None;
    if let Some(url) = mirror::mirror_url() {
        match mirror::fetch_mirror(&url) {
            Ok(found) => rest.retain(|&at| match found.source(&configured[at].1) {
                Some(source) => {
                    fresh.push((at, Ok((source.clone(), "mirror"))));
                    false
                }
                None => true,
            }),
            Err(error) => mirror_error = Some(error),
        }
    }
    let from_github: Vec<(usize, Result<SkillSource, String>)> = thread::scope(|scope| {
        let handles: Vec<_> = rest
            .iter()
            .map(|&at| {
                let (name, repository) = &configured[at];
                (
                    at,
                    scope.spawn(move || skills::fetch_github_source(name, repository)),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(at, handle)| {
                let result = handle
                    .join()
                    .unwrap_or_else(|_| Err("reading the repository failed".to_string()));
                (at, result)
            })
            .collect()
    });
    fresh.extend(from_github.into_iter().map(|(at, result)| {
        let result = result
            .map(|source| (source, "github"))
            .map_err(|error| match &mirror_error {
                Some(mirror_failed) => format!("{error}; LynShen mirror: {mirror_failed}"),
                None => error,
            });
        (at, result)
    }));

    let checked_at = now_ms().max(started);
    let mut cached = Vec::new();
    for (at, (name, repository)) in configured.iter().enumerate() {
        let previous = find(&old, repository);
        let result = fresh.iter().find(|(index, _)| *index == at).map(|(_, r)| r);
        let entry = match result {
            Some(Ok((source, origin))) => Cached {
                source: SkillSource {
                    name: name.clone(),
                    ..source.clone()
                },
                origin: Some(origin.to_string()),
                fetched_at: Some(checked_at),
                checked_at,
                error: None,
            },
            Some(Err(error)) => match previous {
                Some(previous) => Cached {
                    checked_at,
                    error: Some(error.clone()),
                    ..previous.clone()
                },
                None => Cached {
                    source: SkillSource {
                        name: name.clone(),
                        repository: repository.clone(),
                        revision: String::new(),
                        dir: String::new(),
                        skills: Vec::new(),
                    },
                    origin: None,
                    fetched_at: None,
                    checked_at,
                    error: Some(error.clone()),
                },
            },
            // Not read this time: as it was.
            None => match previous {
                Some(previous) => previous.clone(),
                None => continue,
            },
        };
        cached.push(entry);
    }
    if let Err(error) = write(&cached) {
        eprintln!("lynshen daemon: cannot write the skills catalog cache: {error}");
    }
}

/// The background refresh: whether one runs, and whether every source is to
/// be read again once it ends (a refresh asked for while one ran).
struct Refresh {
    running: bool,
    again: bool,
}

static STATE: Mutex<Refresh> = Mutex::new(Refresh {
    running: false,
    again: false,
});
static ENDED: Condvar = Condvar::new();

fn state() -> std::sync::MutexGuard<'static, Refresh> {
    STATE.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Clears the running flag and wakes the waiters when a refresh panics.
struct Ending;

impl Drop for Ending {
    fn drop(&mut self) {
        if thread::panicking() {
            let mut state = state();
            state.running = false;
            state.again = false;
            ENDED.notify_all();
        }
    }
}

/// Starts a refresh in the background: of the stale sources, or of every
/// source when `all`. While one runs, `all` queues another after it.
pub fn start_refresh(configured: Vec<(String, String)>, all: bool) {
    {
        let mut state = state();
        if state.running {
            state.again |= all;
            return;
        }
        state.running = true;
    }
    thread::spawn(move || {
        let _ending = Ending;
        let mut all = all;
        loop {
            refresh(&configured, all);
            let mut state = state();
            if !state.again {
                state.running = false;
                ENDED.notify_all();
                return;
            }
            state.again = false;
            all = true;
        }
    });
}

pub fn refreshing() -> bool {
    state().running
}

/// Waits up to `limit` for a running refresh to end.
pub fn wait_refresh(limit: Duration) {
    let deadline = Instant::now() + limit;
    let mut state = state();
    while state.running {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        state = ENDED
            .wait_timeout(state, left)
            .unwrap_or_else(|poison| poison.into_inner())
            .0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(repository: &str, checked_at: u64, error: Option<&str>) -> Cached {
        Cached {
            source: SkillSource {
                name: "x".to_string(),
                repository: repository.to_string(),
                revision: String::new(),
                dir: "skills".to_string(),
                skills: Vec::new(),
            },
            origin: Some("github".to_string()),
            fetched_at: Some(checked_at),
            checked_at,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn a_source_is_read_again_when_missing_old_or_failed_a_while_ago() {
        let repo = "https://github.com/o/r";
        let configured = vec![("R".to_string(), repo.to_string())];
        let hour = FRESH.as_millis() as u64;
        let retry = RETRY_FAILED.as_millis() as u64;
        let now = 10 * hour;
        assert!(due(&[], &configured, now));
        assert!(!due(&[entry(repo, now - hour + 1, None)], &configured, now));
        assert!(due(&[entry(repo, now - hour, None)], &configured, now));
        // A failure is not tried again on every open.
        assert!(!due(
            &[entry(repo, now - retry + 1, Some("x"))],
            &configured,
            now
        ));
        assert!(due(
            &[entry(repo, now - retry, Some("x"))],
            &configured,
            now
        ));
        // The same repository written another way is the same source.
        assert!(!due(
            &[entry("https://github.com/O/R/", now, None)],
            &configured,
            now
        ));
        assert!(!due(&[], &[], now));
    }
}

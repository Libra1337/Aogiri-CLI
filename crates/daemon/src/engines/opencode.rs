//! OpenCode (`opencode acp`, 1.x) as an ACP agent: what the daemon adds to
//! its start, and how the client's approval modes map onto it.
//!
//! - Approval modes are OpenCode agents, switched live with
//!   `session/set_config_option` (`mode`):
//!   - 谨慎 (`read-only`) and 自动编辑 (`auto-edit`): `aogiri`, an agent the
//!     daemon defines for the process (`OPENCODE_CONFIG_CONTENT`). Every tool
//!     asks first except the reading ones, and the user's own rules still
//!     apply with `allow` turned into `ask`: a rule the user set to `deny`
//!     still denies. In 自动编辑 the daemon answers OpenCode's edit requests
//!     itself.
//!   - 全自动 (`full-auto`): `build`, OpenCode as the user configured it.
//!   - 计划 (`plan`): OpenCode's `plan` agent. It cannot edit files but may
//!     run commands.
//!
//!   The user's rules come from `opencode debug config` in the session's
//!   folder, run before the agent starts.
//! - On the LynShen gateway: a `lynshen` provider on the daemon's local
//!   gateway (the session's local key in `LYNSHEN_GATEWAY_TOKEN`), with the
//!   LynShen models the user shows, and only that provider. Claude models
//!   use the Anthropic Messages API, OpenAI's the Responses API, the others
//!   Chat Completions. Prices are left at 0: the gateway bills each turn.
//! - OpenCode 2.x (`@opencode/cli`) is refused.

use super::Options;
use lynshen_agent_core::ModelConfig;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// The agent 谨慎 and 自动编辑 run on.
pub const ASK_AGENT: &str = "aogiri";
/// The environment variable the gateway key is read from.
const KEY_ENV: &str = "LYNSHEN_GATEWAY_TOKEN";
const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";
/// How long `opencode debug config` may take.
const RULES_TIMEOUT: Duration = Duration::from_secs(20);
/// Sessions started together (a restore) share one reading of the rules.
const RULES_TTL: Duration = Duration::from_secs(30);

/// Tools that only read, or keep the session's own notes: the ask agent
/// leaves them as the user's config has them.
const READING: &[&str] = &[
    "read",
    "glob",
    "grep",
    "list",
    "lsp",
    "todowrite",
    "skill",
    "question",
    "codesearch",
];

/// Permission keys whose value must be one action, never a pattern map.
const ACTION_ONLY: &[&str] = &[
    "todowrite",
    "question",
    "webfetch",
    "websearch",
    "doom_loop",
];

/// A JSON value that keeps object keys in their order. OpenCode reads
/// permission rules in order (the last one that matches decides), and
/// `serde_json::Value` sorts keys here.
#[derive(Debug, Clone, PartialEq)]
pub enum Ordered {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}

impl Ordered {
    fn get(&self, key: &str) -> Option<&Ordered> {
        match self {
            Ordered::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Ordered> {
        match self {
            Ordered::Object(entries) => entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Ordered::String(text) => Some(text),
            _ => None,
        }
    }

    /// Sets `key`, in its place when present, else at the end.
    fn set(&mut self, key: &str, value: Ordered) {
        if let Ordered::Object(entries) = self {
            match entries.iter_mut().find(|(k, _)| k == key) {
                Some((_, existing)) => *existing = value,
                None => entries.push((key.to_string(), value)),
            }
        }
    }

    fn text(text: &str) -> Ordered {
        Ordered::String(text.to_string())
    }
}

impl From<Value> for Ordered {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Ordered::Null,
            Value::Bool(b) => Ordered::Bool(b),
            Value::Number(n) => Ordered::Number(n),
            Value::String(s) => Ordered::String(s),
            Value::Array(items) => Ordered::Array(items.into_iter().map(Ordered::from).collect()),
            Value::Object(map) => Ordered::Object(
                map.into_iter()
                    .map(|(k, v)| (k, Ordered::from(v)))
                    .collect(),
            ),
        }
    }
}

impl serde::Serialize for Ordered {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Ordered::Null => serializer.serialize_unit(),
            Ordered::Bool(b) => serializer.serialize_bool(*b),
            Ordered::Number(n) => serde::Serialize::serialize(n, serializer),
            Ordered::String(s) => serializer.serialize_str(s),
            Ordered::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Ordered::Object(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }
            fn visit_none<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Ordered, E> {
                Ok(Ordered::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Ordered, E> {
                Ok(Ordered::Number(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Ordered, E> {
                Ok(Ordered::Number(n.into()))
            }
            fn visit_f64<E>(self, n: f64) -> Result<Ordered, E> {
                Ok(serde_json::Number::from_f64(n).map_or(Ordered::Null, Ordered::Number))
            }
            fn visit_str<E>(self, s: &str) -> Result<Ordered, E> {
                Ok(Ordered::String(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> Result<Ordered, E> {
                Ok(Ordered::String(s))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Ordered, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Ordered::Array(items))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Ordered, A::Error> {
                let mut entries: Vec<(String, Ordered)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Ordered>()? {
                    match entries.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, existing)) => *existing = value,
                        None => entries.push((key, value)),
                    }
                }
                Ok(Ordered::Object(entries))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A command's program name without folder or Windows extension.
fn program_stem(command: &str) -> &str {
    let file = command.rsplit(['/', '\\']).next().unwrap_or(command);
    match file.rsplit_once('.') {
        Some((stem, ext))
            if ["exe", "cmd", "bat", "ps1"]
                .iter()
                .any(|e| ext.eq_ignore_ascii_case(e)) =>
        {
            stem
        }
        _ => file,
    }
}

/// Whether an ACP session runs OpenCode's ACP server (`opencode acp`).
pub fn is_opencode(options: &Options) -> bool {
    options
        .command
        .as_deref()
        .is_some_and(|command| program_stem(command).eq_ignore_ascii_case("opencode"))
        && options.args.first().map(String::as_str) == Some("acp")
}

/// The OpenCode agent a client approval mode runs on.
pub fn agent_for(mode: &str) -> &'static str {
    match mode {
        "full-auto" | "full-access" | "bypassPermissions" => "build",
        "plan" => "plan",
        _ => ASK_AGENT,
    }
}

/// Whether the daemon answers OpenCode's edit requests itself in `mode`.
pub fn allows_edits(mode: &str) -> bool {
    matches!(mode, "auto-edit" | "auto")
}

/// Why the agent `initialize` described cannot run here: OpenCode 2.x.
pub fn unsupported(agent_info: &Value) -> Option<String> {
    let name = agent_info["name"].as_str().unwrap_or_default();
    if !name.eq_ignore_ascii_case("opencode") {
        return None;
    }
    let version = agent_info["version"].as_str().unwrap_or_default();
    let major: u64 = version
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()?;
    (major >= 2).then(|| {
        format!(
            "OpenCode {version} is not supported: install OpenCode 1.x (npm i -g opencode-ai@1)"
        )
    })
}

/// `allow` becomes `ask`, except for reading tools.
fn asked(action: &Ordered, reading: bool) -> Ordered {
    match action.as_str() {
        Some("allow") if !reading => Ordered::text("ask"),
        _ => action.clone(),
    }
}

/// A rule value with `allow` turned into `ask` (see `asked`).
fn transformed(value: &Ordered, reading: bool) -> Ordered {
    match value {
        Ordered::Object(patterns) => Ordered::Object(
            patterns
                .iter()
                .map(|(pattern, action)| (pattern.clone(), asked(action, reading)))
                .collect(),
        ),
        action => asked(action, reading),
    }
}

/// `"x"` as `{"*": "x"}`, a pattern map as it is.
fn patterns(value: &Ordered) -> Vec<(String, Ordered)> {
    match value {
        Ordered::Object(entries) => entries.clone(),
        Ordered::String(_) => vec![("*".to_string(), value.clone())],
        _ => Vec::new(),
    }
}

/// The ask agent's permission: every tool asks except the reading ones,
/// then the user's rules (`permission`, then `agent.build.permission`) with
/// `allow` turned into `ask`, so their `deny` rules still deny.
///
/// One known difference from OpenCode's own evaluation: a user rule
/// `"*": "deny"` (deny everything) is kept, but reading tools stay allowed
/// and per-tool rules written before it apply again.
pub fn ask_permission(user: &Ordered, build: &Ordered) -> Ordered {
    let text = Ordered::text;
    let mut out = Ordered::Object(vec![
        ("*".into(), text("ask")),
        (
            "read".into(),
            Ordered::Object(vec![
                ("*".into(), text("allow")),
                ("*.env".into(), text("ask")),
                ("*.env.*".into(), text("ask")),
                ("*.env.example".into(), text("allow")),
            ]),
        ),
    ]);
    for tool in READING.iter().filter(|tool| **tool != "read") {
        out.set(tool, text("allow"));
    }
    out.set("plan_enter", text("allow"));
    out.set("plan_exit", text("deny"));
    out.set("doom_loop", text("ask"));
    out.set("external_directory", text("ask"));
    for rules in [user, build] {
        for (key, value) in patterns(rules) {
            if key == "*" {
                // allow or ask: what the agent does already.
                if patterns(&value)
                    .iter()
                    .any(|(_, action)| action.as_str() == Some("deny"))
                {
                    out.set("*", text("deny"));
                }
                continue;
            }
            let reading = READING.contains(&key.as_str());
            let value = transformed(&value, reading);
            if ACTION_ONLY.contains(&key.as_str()) {
                let action = match &value {
                    Ordered::Object(_) => value.get("*").cloned(),
                    action => Some(action.clone()),
                };
                if let Some(action) = action {
                    out.set(&key, action);
                }
                continue;
            }
            match out.get_mut(&key) {
                Some(existing) => {
                    let mut merged = Ordered::Object(patterns(existing));
                    for (pattern, action) in patterns(&value) {
                        merged.set(&pattern, action);
                    }
                    *existing = merged;
                }
                None => out.set(&key, value),
            }
        }
    }
    out
}

/// The agent 谨慎 and 自动编辑 run on.
fn ask_agent(user: &Ordered, build: &Ordered) -> Ordered {
    Ordered::Object(vec![
        ("mode".into(), Ordered::text("primary")),
        (
            "description".into(),
            Ordered::text("Aogiri: tools ask before they run, as your OpenCode rules allow"),
        ),
        ("permission".into(), ask_permission(user, build)),
    ])
}

/// The AI SDK package OpenCode talks to a LynShen model with: Anthropic
/// Messages for Claude (prompt caching, thinking), OpenAI Responses for
/// OpenAI's models, Chat Completions for the others.
pub fn wire_package(model: &str) -> &'static str {
    let name = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    let openai = name.starts_with("gpt-")
        || name.starts_with("codex")
        || ["o1", "o3", "o4"]
            .iter()
            .any(|series| name == *series || name.starts_with(&format!("{series}-")));
    if name.starts_with("claude") {
        "@ai-sdk/anthropic"
    } else if openai {
        "@ai-sdk/openai"
    } else {
        "@ai-sdk/openai-compatible"
    }
}

/// The config that puts OpenCode on the LynShen gateway at `base` (the
/// local gateway, `…/gw`): the `lynshen` provider with `models`, on
/// `preferred` when it is one of them, else the first.
pub fn gateway_config(
    models: &[ModelConfig],
    base: &str,
    preferred: Option<&str>,
) -> Result<Value, String> {
    if models.is_empty() {
        return Err(
            "no LynShen models to give OpenCode: choose some in the model list first".to_string(),
        );
    }
    let mut table = Map::new();
    for model in models {
        let context = match model.context_window {
            0 => 128_000,
            window => window,
        };
        let output = match model.max_output_tokens {
            0 => 32_000,
            tokens => tokens,
        };
        let images = model.accepts_images();
        let input = if images {
            json!(["text", "image"])
        } else {
            json!(["text"])
        };
        let entry = json!({
            "name": model.display_name.clone().unwrap_or_else(|| model.name.clone()),
            "limit": { "context": context, "output": output },
            // The gateway bills each turn; OpenCode's own estimate would mislead.
            "cost": { "input": 0, "output": 0 },
            "reasoning": model.reasoning.unwrap_or(!model.reasoning_efforts.is_empty()),
            "attachment": images,
            "modalities": { "input": input, "output": ["text"] },
            "provider": { "npm": wire_package(&model.name) },
        });
        table.insert(model.name.clone(), entry);
    }
    let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
    let model = preferred.filter(|p| names.contains(p)).unwrap_or(names[0]);
    // Titles and summaries: a light model when the list has one.
    let small = names
        .iter()
        .find(|name| {
            let name = name.to_ascii_lowercase();
            ["flash", "mini", "haiku", "lite"]
                .iter()
                .any(|light| name.contains(light))
        })
        .copied()
        .unwrap_or(model);
    Ok(json!({
        "provider": {
            "lynshen": {
                "name": "LynShen",
                "npm": "@ai-sdk/openai-compatible",
                "options": {
                    "baseURL": format!("{}/v1", base.trim_end_matches('/')),
                    "apiKey": format!("{{env:{KEY_ENV}}}"),
                },
                "models": table,
            }
        },
        "enabled_providers": ["lynshen"],
        "model": format!("lynshen/{model}"),
        "small_model": format!("lynshen/{small}"),
    }))
}

/// `add` merged into `base`: objects key by key, anything else replaced.
fn merge(base: &mut Ordered, add: Ordered) {
    match (base, add) {
        (Ordered::Object(base), Ordered::Object(add)) => {
            for (key, value) in add {
                match base.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, existing)) => merge(existing, value),
                    None => base.push((key, value)),
                }
            }
        }
        (base, add) => *base = add,
    }
}

/// The first JSON object in `text` (OpenCode may print something before it).
fn first_object(text: &str) -> Option<Ordered> {
    let start = text.find('{')?;
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Ordered>()
        .next()?
        .ok()
        .filter(|value| matches!(value, Ordered::Object(_)))
}

type Rules = (Ordered, Ordered);
/// Rules read, by program and folder, with when they were read.
type RulesCache = Mutex<HashMap<(OsString, PathBuf), (Instant, Rules)>>;

fn rules_cache() -> &'static RulesCache {
    static CACHE: OnceLock<RulesCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The user's permission rules as OpenCode resolves them in `cwd`: top-level
/// `permission` and `agent.build.permission`, from `opencode debug config`.
fn user_rules(program: &OsString, options: &Options, cwd: &Path) -> Result<Rules, String> {
    let key = (program.clone(), cwd.to_path_buf());
    if let Some((at, rules)) = rules_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        if at.elapsed() < RULES_TTL {
            return Ok(rules.clone());
        }
    }
    let mut child = Command::new(program)
        .args(["debug", "config"])
        .envs(options.env.iter().map(|(name, value)| (name, value)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run opencode debug config: {error}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + RULES_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("opencode debug config did not answer in time".to_string());
            }
        }
    }
    let text = reader.join().unwrap_or_default();
    let config = first_object(&text).ok_or("opencode debug config printed no configuration")?;
    let rules = (
        config.get("permission").cloned().unwrap_or(Ordered::Null),
        config
            .get("agent")
            .and_then(|agent| agent.get("build"))
            .and_then(|build| build.get("permission"))
            .cloned()
            .unwrap_or(Ordered::Null),
    );
    rules_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, (Instant::now(), rules.clone()));
    Ok(rules)
}

/// Prepares `command` (`opencode acp`) for a session in `cwd`: the ask
/// agent, and with `gateway` (the local gateway's base URL and the session's
/// key) the LynShen provider. Returns notices for the session's clients.
pub fn configure(
    command: &mut Command,
    options: &Options,
    cwd: &Path,
    gateway: Option<(&str, &str)>,
) -> Result<Vec<String>, String> {
    let mut notices = Vec::new();
    let program = command.get_program().to_os_string();
    let (user, build) = user_rules(&program, options, cwd).unwrap_or_else(|error| {
        notices.push(format!(
            "[opencode] {error}; in 谨慎 and 自动编辑 your own OpenCode deny rules ask instead"
        ));
        (Ordered::Null, Ordered::Null)
    });
    // A configuration the user gave the agent comes first.
    let mut config = options
        .env
        .iter()
        .find(|(name, _)| name == CONFIG_ENV)
        .and_then(|(_, value)| serde_json::from_str::<Ordered>(value).ok())
        .filter(|value| matches!(value, Ordered::Object(_)))
        .unwrap_or(Ordered::Object(Vec::new()));
    merge(
        &mut config,
        Ordered::Object(vec![(
            "agent".into(),
            Ordered::Object(vec![(ASK_AGENT.into(), ask_agent(&user, &build))]),
        )]),
    );
    if let Some((base, key)) = gateway {
        let models = lynshen_agent_core::lynshen_visible_models();
        let preferred = options
            .model
            .clone()
            .or_else(lynshen_agent_core::lynshen_default_model);
        merge(
            &mut config,
            Ordered::from(gateway_config(&models, base, preferred.as_deref())?),
        );
        command.env(KEY_ENV, key);
    }
    let content = serde_json::to_string(&config).map_err(|error| error.to_string())?;
    command.env(CONFIG_ENV, content);
    Ok(notices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(command: &str, args: &[&str]) -> Options {
        Options {
            command: Some(command.to_string()),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..Options::default()
        }
    }

    #[test]
    fn opencode_is_known_by_its_program_and_acp_argument() {
        assert!(is_opencode(&options("opencode", &["acp"])));
        assert!(is_opencode(&options(
            "/opt/homebrew/bin/opencode",
            &["acp"]
        )));
        assert!(is_opencode(&options(
            "C:\\npm\\node_modules\\opencode-ai\\bin\\opencode.exe",
            &["acp"]
        )));
        assert!(!is_opencode(&options("opencode", &["serve"])));
        assert!(!is_opencode(&options("gemini", &["--experimental-acp"])));
    }

    #[test]
    fn modes_are_opencode_agents() {
        assert_eq!(agent_for("read-only"), ASK_AGENT);
        assert_eq!(agent_for("auto-edit"), ASK_AGENT);
        assert_eq!(agent_for("full-auto"), "build");
        assert_eq!(agent_for("full-access"), "build");
        assert_eq!(agent_for("plan"), "plan");
        assert!(allows_edits("auto-edit") && !allows_edits("read-only"));
    }

    #[test]
    fn opencode_2_is_refused() {
        assert!(unsupported(&json!({ "name": "OpenCode", "version": "1.19.0" })).is_none());
        assert!(
            unsupported(&json!({ "name": "OpenCode", "version": "2.0.29" }))
                .unwrap()
                .contains("1.x")
        );
        assert!(unsupported(&json!({ "name": "Gemini CLI", "version": "3.0.0" })).is_none());
        assert!(unsupported(&json!({ "name": "OpenCode" })).is_none());
    }

    /// OpenCode's evaluation: the last rule that matches decides; no match asks.
    fn decide(permission: &Ordered, tool: &str, pattern: &str) -> String {
        fn matches(glob: &str, text: &str) -> bool {
            match glob.split_once('*') {
                None => glob == text,
                Some((head, tail)) => text.strip_prefix(head).is_some_and(|rest| {
                    (0..=rest.len()).any(|i| rest.is_char_boundary(i) && matches(tail, &rest[i..]))
                }),
            }
        }
        let mut action = "ask".to_string();
        for (key, value) in patterns(permission) {
            for (glob, act) in patterns(&value) {
                if matches(&key, tool) && matches(&glob, pattern) {
                    action = act.as_str().unwrap().to_string();
                }
            }
        }
        action
    }

    fn ordered(text: &str) -> Ordered {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn the_ask_agent_turns_allow_into_ask_and_keeps_deny() {
        let user = ordered(
            r#"{
                "bash": { "*": "allow", "rm *": "deny", "git status": "allow" },
                "edit": "allow",
                "webfetch": "deny",
                "read": { "secrets/*": "deny" },
                "mcp_*": "allow"
            }"#,
        );
        let build = ordered(r#"{ "bash": { "npm publish*": "deny" } }"#);
        let p = ask_permission(&user, &build);
        assert_eq!(decide(&p, "bash", "ls"), "ask");
        assert_eq!(decide(&p, "bash", "git status"), "ask");
        assert_eq!(decide(&p, "bash", "rm -rf x"), "deny");
        assert_eq!(decide(&p, "bash", "npm publish --tag x"), "deny");
        assert_eq!(decide(&p, "edit", "a.rs"), "ask");
        assert_eq!(decide(&p, "webfetch", "https://x"), "deny");
        assert_eq!(decide(&p, "mcp_db", "q"), "ask");
        assert_eq!(decide(&p, "task", "general"), "ask");
        // Reading stays free, except .env files and what the user denies.
        assert_eq!(decide(&p, "read", "src/a.rs"), "allow");
        assert_eq!(decide(&p, "read", ".env"), "ask");
        assert_eq!(decide(&p, "read", "secrets/key"), "deny");
        assert_eq!(decide(&p, "grep", "x"), "allow");
        assert_eq!(
            p.get("webfetch"),
            Some(&Ordered::text("deny")),
            "webfetch takes one action"
        );
        // The catch-all comes first in what OpenCode reads.
        let text = serde_json::to_string(&p).unwrap();
        assert!(text.starts_with(r#"{"*":"ask","read":"#), "{text}");
    }

    #[test]
    fn rules_keep_the_users_order() {
        // An exception written after a wider deny still applies, as ask.
        let p = ask_permission(
            &ordered(r#"{ "bash": { "*": "deny", "git *": "allow" } }"#),
            &Ordered::Null,
        );
        assert_eq!(decide(&p, "bash", "git log"), "ask");
        assert_eq!(decide(&p, "bash", "curl x"), "deny");
        let p = ask_permission(
            &ordered(r#"{ "bash": { "git *": "allow", "*": "deny" } }"#),
            &Ordered::Null,
        );
        assert_eq!(decide(&p, "bash", "git log"), "deny");
    }

    #[test]
    fn a_global_allow_asks_and_a_global_deny_denies() {
        let p = ask_permission(&Ordered::text("allow"), &Ordered::Null);
        assert_eq!(decide(&p, "bash", "ls"), "ask");
        assert_eq!(decide(&p, "read", "a"), "allow");
        let p = ask_permission(&ordered(r#"{ "*": "deny" }"#), &Ordered::Null);
        assert_eq!(decide(&p, "bash", "ls"), "deny");
        assert_eq!(decide(&p, "edit", "a"), "deny");
        // No rules read: everything that acts asks.
        let p = ask_permission(&Ordered::Null, &Ordered::Null);
        assert_eq!(decide(&p, "bash", "ls"), "ask");
        assert_eq!(decide(&p, "plan_exit", "x"), "deny");
    }

    fn model(name: &str) -> ModelConfig {
        ModelConfig {
            name: name.to_string(),
            context_window: 200_000,
            max_context_window: 0,
            max_output_tokens: 0,
            reasoning_efforts: Vec::new(),
            input_cost: 0.0,
            cached_input_cost: 0.0,
            output_cost: 0.0,
            display_name: None,
            group_windows: Default::default(),
            reasoning: None,
            input_modalities: None,
        }
    }

    #[test]
    fn the_gateway_provider_offers_the_users_models_on_the_local_gateway() {
        let models = vec![
            model("claude-opus-4-8"),
            model("gpt-5.5"),
            model("deepseek-v4.1-flash"),
        ];
        let config = gateway_config(&models, "http://127.0.0.1:7788/gw", Some("gpt-5.5")).unwrap();
        let provider = &config["provider"]["lynshen"];
        assert_eq!(
            provider["options"]["baseURL"],
            "http://127.0.0.1:7788/gw/v1"
        );
        assert_eq!(provider["options"]["apiKey"], "{env:LYNSHEN_GATEWAY_TOKEN}");
        let table = &provider["models"];
        assert_eq!(
            table["claude-opus-4-8"]["provider"]["npm"],
            "@ai-sdk/anthropic"
        );
        assert_eq!(table["gpt-5.5"]["provider"]["npm"], "@ai-sdk/openai");
        assert_eq!(
            table["deepseek-v4.1-flash"]["provider"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(table["gpt-5.5"]["cost"]["input"], 0);
        assert_eq!(table["gpt-5.5"]["limit"]["context"], 200_000);
        assert_eq!(config["model"], "lynshen/gpt-5.5");
        assert_eq!(config["small_model"], "lynshen/deepseek-v4.1-flash");
        assert_eq!(config["enabled_providers"], json!(["lynshen"]));
        // A model the list lacks: the first.
        let config = gateway_config(&models, "http://h/gw", Some("other")).unwrap();
        assert_eq!(config["model"], "lynshen/claude-opus-4-8");
        assert!(gateway_config(&[], "http://h/gw", None).is_err());
    }

    #[test]
    fn wire_packages_follow_the_model_family() {
        assert_eq!(wire_package("claude-sonnet-5"), "@ai-sdk/anthropic");
        assert_eq!(
            wire_package("anthropic/claude-haiku-5"),
            "@ai-sdk/anthropic"
        );
        assert_eq!(wire_package("gpt-5.5-codex"), "@ai-sdk/openai");
        assert_eq!(wire_package("o3"), "@ai-sdk/openai");
        assert_eq!(wire_package("o4-mini"), "@ai-sdk/openai");
        assert_eq!(wire_package("glm-5.3"), "@ai-sdk/openai-compatible");
        assert_eq!(wire_package("omni-x"), "@ai-sdk/openai-compatible");
    }

    #[test]
    fn the_users_own_config_content_is_kept_under_ours() {
        let mut base =
            ordered(r#"{ "agent": { "build": { "model": "x/y" } }, "share": "disabled" }"#);
        merge(
            &mut base,
            ordered(r#"{ "agent": { "aogiri": { "mode": "primary" } } }"#),
        );
        assert_eq!(
            serde_json::to_string(&base).unwrap(),
            r#"{"agent":{"build":{"model":"x/y"},"aogiri":{"mode":"primary"}},"share":"disabled"}"#
        );
        let found =
            first_object("warn: x\n{\"permission\": {\"bash\": \"deny\", \"a\": \"allow\"}}\n")
                .unwrap();
        assert_eq!(
            serde_json::to_string(found.get("permission").unwrap()).unwrap(),
            r#"{"bash":"deny","a":"allow"}"#
        );
    }
}

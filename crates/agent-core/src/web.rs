//! The `web_search` and `web_fetch` tools' engines.
//!
//! `web_search` (engine `auto`, the default) tries, in this order, and stops
//! at the first that finds pages:
//! 1. native search by the conversation's own model, when its provider can
//!    run a search tool on its side: a Claude model with Anthropic's
//!    `web_search` server tool over Messages, a GPT model with the Responses
//!    `web_search` tool. `web_search_model` names another model for this step
//!    instead (empty: the conversation's model).
//! 2. the gateway's search (POST /tools/v1/search on a LynShen or Monoize
//!    gateway, with the key the conversation already uses). The gateway
//!    searches with a key it holds itself; no search key is on this machine.
//! 3. this machine: a search engine's own HTML result page, no key, no login.
//!
//! The result says which one answered (`engine`, and `model` when a model
//! searched) and, when an earlier step failed, why (`note`).
//!
//! `web_fetch` fetches from this machine unless `web_fetch_engine` names a
//! gateway engine (POST /tools/v1/fetch); the local engine lives in
//! `web_fetch`.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `auto` (the default) runs the chain above; `gateway` goes to the LynShen
/// gateway's search only. The other names are ones earlier builds wrote,
/// read as `auto`.
pub const SEARCH_ENGINES: &[&str] = &[
    "auto", "gateway", "local", "model", "native", "parallel", "brave",
];
pub const FETCH_ENGINES: &[&str] = &["local", "jina", "firecrawl", "parallel"];
/// Every released version accepts `auto` (0.4.23 and earlier reject any name
/// but `auto`, `parallel` and `brave`), so a config.json this version saves
/// still loads in an older engine left running across an update.
pub const DEFAULT_SEARCH_ENGINE: &str = "auto";
pub const DEFAULT_FETCH_ENGINE: &str = "local";
/// `web_search_model` empty: the conversation's own model searches.
pub const DEFAULT_SEARCH_MODEL: &str = "";
/// What 0.7.7 and earlier wrote as the default `web_search_model`. A config
/// that still holds it never chose it, so it reads as the new default.
pub const RETIRED_SEARCH_MODEL: &str = "claude-haiku-5-5";
/// Searches the model may run for one `web_search` call: the query, and a
/// refinement or two when the first results miss.
const MODEL_SEARCH_MAX_USES: u64 = 3;
const MODEL_SEARCH_MAX_TOKENS: u64 = 1500;
/// A search and its report take 5–15 s; the rest is a slow upstream.
const MODEL_SEARCH_READ_TIMEOUT: Duration = Duration::from_secs(120);
const MODEL_SEARCH_SYSTEM: &str = "You search the web for another AI agent and report what the results say. Always search first; never answer from memory. Report the facts that answer the query, each followed by its source URL in parentheses. Copy numbers, versions, dates and names exactly as the pages give them. If the results do not answer the query, say so and say what they did cover. Plain text, no preamble, at most 300 words.";
/// A model whose route answered without searching (no server tool on that
/// route) is not asked again for this long: each try costs a model call.
const NO_TOOL_PAUSE: Duration = Duration::from_secs(30 * 60);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The gateway gives a fetch vendor 30 s per page; this leaves room for it.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// The gateway's search answers in about a second; 30 s is a stuck upstream.
const GATEWAY_SEARCH_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SEARCH_RESULTS: u64 = 10;
const QUERY_MAX_CHARS: usize = 400;
/// A result page is a few hundred KB of markup at most.
const MAX_SEARCH_PAGE_BYTES: u64 = 1024 * 1024;
/// The HTML endpoints serve a stripped page to an unknown client.
const SEARCH_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

/// What the web tools need from the engine's config and login state, set on
/// the tool state at the start of every turn.
#[derive(Debug, Clone)]
pub struct WebTools {
    pub api_url: String,
    pub encrypt_secrets: bool,
    pub search_engine: String,
    pub fetch_engine: String,
    /// A LynShen session exists; only the `gateway` engine needs one.
    pub signed_in: bool,
    /// Step 1: the model that searches natively, or why none does (not a
    /// model whose provider runs a search tool, no gateway key, ...).
    pub search_model: Result<SearchModel, String>,
    /// Step 2: the gateway's search, or why there is none.
    pub gateway: Result<GatewaySearch, String>,
}

impl WebTools {
    /// Whether `web_search` goes only through the LynShen gateway's search
    /// (`web_search_engine: gateway`, with the LynShen login).
    pub fn gateway_search(&self) -> bool {
        self.search_engine == "gateway"
    }

    /// Whether `web_search` can run at all: the default chain always can
    /// (this machine is its last step), the `gateway` engine needs a login.
    pub fn search_available(&self) -> bool {
        !self.gateway_search() || self.signed_in
    }
}

/// The wire a model searches over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSearch {
    /// Anthropic Messages with the `web_search_20250305` server tool.
    Messages,
    /// OpenAI Responses with the `web_search` tool, streamed: the gateway's
    /// GPT routes answer a non-streaming request with a stream.
    Responses,
}

/// The model behind step 1 and how to reach it: the provider the
/// conversation already uses.
#[derive(Clone)]
pub struct SearchModel {
    pub model: String,
    pub wire: NativeSearch,
    /// Whether this is the conversation's own model (not `web_search_model`).
    pub conversation: bool,
    url: String,
    api_key: String,
    /// The gateway's device header (`device_id`), then the route chosen for
    /// this model (`monoize_providers` / `lynshen_groups`).
    headers: Vec<(String, String)>,
    connect_timeout: Duration,
}

impl std::fmt::Debug for SearchModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SearchModel")
            .field("model", &self.model)
            .field("wire", &self.wire)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// The wire on which `model` can search natively on `provider`, or why it
/// cannot. Only these have a provider-side search tool that works through
/// the gateways (probed 2026-10-19): Claude over Messages, GPT over
/// Responses. Gemini's grounding and Grok's live search do not come
/// through; DeepSeek, GLM, Kimi, Qwen and MiniMax have none.
fn native_search_wire(provider: &str, model: &str) -> Result<NativeSearch, String> {
    let name = model.to_ascii_lowercase();
    let wire = if llm_provider_kit::is_anthropic_model(&name) {
        NativeSearch::Messages
    } else if name.starts_with("gpt-") && !name.starts_with("gpt-image") {
        NativeSearch::Responses
    } else {
        return Err(format!("{model} has no native web search"));
    };
    // OpenAI's own API serves the Responses tool with the same Bearer key;
    // every other provider's endpoint (and Anthropic's own, whose keys are
    // not Bearer) is unknown ground, and a wrong guess costs a model call.
    let known = match wire {
        NativeSearch::Messages => matches!(provider, "lynshen" | "monoize"),
        NativeSearch::Responses => matches!(provider, "lynshen" | "monoize" | "openai"),
    };
    if known {
        Ok(wire)
    } else {
        Err(format!("{provider} does not run a search tool for {model}"))
    }
}

impl SearchModel {
    /// Step 1 for this config and conversation model, or why there is none.
    pub(crate) fn from_config(
        config: &crate::config::Config,
        api_key: Option<String>,
        headers: &HashMap<String, Vec<(String, String)>>,
    ) -> Result<Self, String> {
        let pinned = config.web_search_model.trim();
        let (model, conversation) = if pinned.is_empty() {
            (config.model.trim(), true)
        } else {
            (pinned, false)
        };
        if model.is_empty() {
            return Err("no model".to_string());
        }
        let wire = native_search_wire(&config.provider, model)?;
        // A gateway that has listed its models and left this one out cannot
        // serve it. LynShen's list is its own; Monoize's is `models`.
        if !conversation {
            let served = if config.provider == "lynshen" {
                &config.lynshen_models
            } else {
                &config.models
            };
            if !served.is_empty() && !served.iter().any(|entry| entry.name == model) {
                return Err(format!("the gateway does not serve {model}"));
            }
        }
        let api_key = api_key
            .filter(|key| !key.trim().is_empty())
            .or_else(|| std::env::var(&config.api_key_env).ok())
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .ok_or_else(|| format!("no API key for {}", config.provider))?;
        let base = config.base_url.trim();
        let url = match wire {
            NativeSearch::Messages => llm_provider_kit::anthropic::messages_url(base),
            NativeSearch::Responses => llm_provider_kit::responses::responses_url(base),
        };
        Ok(Self {
            model: model.to_string(),
            wire,
            conversation,
            url,
            api_key,
            headers: crate::device_id::gateway_headers(&config.provider)
                .into_iter()
                .chain(headers.get(model).cloned().unwrap_or_default())
                .collect(),
            connect_timeout: Duration::from_secs(config.connect_timeout_seconds),
        })
    }
}

/// Step 2: the gateway's own search, reached with the key the conversation
/// uses. Only LynShen and Monoize gateways have one.
#[derive(Clone)]
pub struct GatewaySearch {
    url: String,
    api_key: String,
    headers: Vec<(String, String)>,
    connect_timeout: Duration,
}

impl std::fmt::Debug for GatewaySearch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewaySearch")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl GatewaySearch {
    pub(crate) fn from_config(
        config: &crate::config::Config,
        api_key: Option<String>,
    ) -> Result<Self, String> {
        if !matches!(config.provider.as_str(), "lynshen" | "monoize") {
            return Err(format!(
                "{} is not a LynShen or Monoize gateway",
                config.provider
            ));
        }
        let api_key = api_key
            .filter(|key| !key.trim().is_empty())
            .or_else(|| std::env::var(&config.api_key_env).ok())
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .ok_or_else(|| format!("no API key for {}", config.provider))?;
        Ok(Self {
            url: format!("{}/tools/v1/search", gateway_origin(&config.base_url)),
            api_key,
            headers: crate::device_id::gateway_headers(&config.provider),
            connect_timeout: Duration::from_secs(config.connect_timeout_seconds),
        })
    }
}

/// The gateway's origin from its API base URL (`https://host/v1` →
/// `https://host`): the tools live beside `/v1`, not under it.
fn gateway_origin(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base).to_string()
}

pub fn search_definition() -> Value {
    json!({
        "type": "function",
        "name": "web_search",
        "description": "Search the web: what the results say, with sources (title, url, snippet). Read pages with web_fetch.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Up to 400 characters." },
                "max_results": { "type": "number", "description": "1-10, default 10." },
                "freshness": { "type": "string", "enum": ["day", "week", "month", "year"], "description": "Only pages published within this period." }
            },
            "required": ["query"]
        }
    })
}

pub fn run_search(args: &Value, web: Option<&WebTools>) -> Value {
    let Some(web) = web else {
        return json!({ "error": "web_search is not available in this session" });
    };
    if !web.gateway_search() {
        return search(
            args,
            web.search_model.as_ref().ok(),
            web.gateway.as_ref().ok(),
            &Live,
        );
    }
    let body = match search_body(args, &web.search_engine) {
        Ok(body) => body,
        Err(error) => return json!({ "error": error }),
    };
    match gateway_post(web, "/tools/v1/search", body) {
        Ok(value) => {
            let results = value
                .get("results")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            crate::log_info!(
                "web_search",
                "searched",
                engine = web.search_engine.clone(),
                results = results
            );
            value
        }
        Err(error) => {
            crate::log_warn!(
                "web_search",
                "search failed",
                engine = web.search_engine.clone(),
                error = error.clone()
            );
            json!({ "error": error })
        }
    }
}

/// Runs `web_fetch` on the configured engine: `local` fetches from this
/// machine, anything else goes through the gateway.
pub fn run_fetch(args: &Value, web: Option<&WebTools>) -> Value {
    let Some(web) = web.filter(|web| web.fetch_engine != "local") else {
        return crate::web_fetch::run(args);
    };
    let Some(url) = args.get("url").and_then(Value::as_str) else {
        return json!({ "error": "missing url" });
    };
    if let Err(error) = crate::web_fetch::validate_url(url) {
        return json!({ "url": url, "error": error });
    }
    let max_bytes = args.get("max_bytes").and_then(Value::as_u64);
    let body = json!({ "url": url, "engine": web.fetch_engine });
    match gateway_post(web, "/tools/v1/fetch", body) {
        Ok(value) => {
            crate::log_info!(
                "web_fetch",
                "fetched via gateway",
                engine = web.fetch_engine.clone()
            );
            fetch_result(&value, url, max_bytes)
        }
        Err(error) => {
            crate::log_warn!(
                "web_fetch",
                "gateway fetch failed",
                engine = web.fetch_engine.clone(),
                error = error.clone()
            );
            json!({ "url": url, "error": error })
        }
    }
}

fn search_body(args: &Value, engine: &str) -> Result<Value, String> {
    let query = search_query(args)?;
    // `gateway` only says where to search; the gateway picks the vendor.
    let engine = if engine == "gateway" { "auto" } else { engine };
    let mut body = json!({ "query": query, "engine": engine });
    if let Some(value) = args.get("max_results") {
        let count = value
            .as_u64()
            .filter(|count| (1..=MAX_SEARCH_RESULTS).contains(count))
            .ok_or("max_results must be an integer from 1 to 10")?;
        body["max_results"] = json!(count);
    }
    if let Some(freshness) = args.get("freshness").and_then(Value::as_str) {
        body["freshness"] = json!(freshness);
    }
    Ok(body)
}

fn search_query(args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .ok_or("missing query")?;
    Ok(query.chars().take(QUERY_MAX_CHARS).collect())
}

fn search_max_results(args: &Value) -> Result<usize, String> {
    match args.get("max_results") {
        None | Some(Value::Null) => Ok(MAX_SEARCH_RESULTS as usize),
        Some(value) => value
            .as_u64()
            .filter(|count| (1..=MAX_SEARCH_RESULTS).contains(count))
            .map(|count| count as usize)
            .ok_or_else(|| "max_results must be an integer from 1 to 10".to_string()),
    }
}

/// Shapes a gateway fetch like a local one (`url`, `status`, `text`), capped
/// at `max_bytes` when the caller asked for less.
fn fetch_result(value: &Value, requested_url: &str, max_bytes: Option<u64>) -> Value {
    let mut text = value
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut result = json!({
        "url": value.get("url").and_then(Value::as_str).unwrap_or(requested_url),
        "engine": value.get("engine").cloned().unwrap_or(Value::Null),
    });
    if let Some(status) = value.get("status").and_then(Value::as_u64) {
        result["status"] = json!(status);
    }
    if let Some(title) = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        result["title"] = json!(title);
    }
    if let Some(published) = value
        .get("published_at")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    {
        result["published_at"] = json!(published);
    }
    if let Some(cap) = max_bytes
        .map(|cap| cap as usize)
        .filter(|cap| text.len() > *cap)
    {
        let mut end = cap;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        result["truncated"] = json!(true);
    }
    result["text"] = json!(text);
    result
}

// ---------------------------------------------------------------------------
// The chain: native search by a model, the gateway's search, this machine.
// ---------------------------------------------------------------------------

/// Everything `web_search` reaches over the network. A test injects its own.
pub(crate) trait SearchIo {
    /// One native search request; the reply as JSON (Messages), or the
    /// output items of a completed stream (Responses).
    fn ask_model(&self, model: &SearchModel, body: &Value) -> Result<Value, String>;
    /// POST to the gateway's search; its JSON reply.
    fn ask_gateway(&self, gateway: &GatewaySearch, body: &Value) -> Result<Value, String>;
    /// A search engine's result page.
    fn get_page(&self, url: &str) -> Result<String, String>;
}

struct Live;

impl SearchIo for Live {
    fn ask_model(&self, model: &SearchModel, body: &Value) -> Result<Value, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(model.connect_timeout)
            .timeout_read(MODEL_SEARCH_READ_TIMEOUT)
            .build();
        let mut request = agent
            .post(&model.url)
            .set("Authorization", &format!("Bearer {}", model.api_key));
        if model.wire == NativeSearch::Messages {
            request = request.set(
                "anthropic-version",
                llm_provider_kit::anthropic::ANTHROPIC_VERSION,
            );
        } else {
            request = request.set("Accept", "text/event-stream");
        }
        for (name, value) in &model.headers {
            request = request.set(name, value);
        }
        let response = request.send_json(body.clone()).map_err(status_error)?;
        match model.wire {
            NativeSearch::Messages => response
                .into_json::<Value>()
                .map_err(|error| format!("unreadable reply: {error}")),
            NativeSearch::Responses => {
                let items =
                    llm_provider_kit::responses::read_sse_stream(response.into_reader(), |_| {
                        Ok(())
                    })?;
                Ok(Value::Array(items))
            }
        }
    }

    fn ask_gateway(&self, gateway: &GatewaySearch, body: &Value) -> Result<Value, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(gateway.connect_timeout)
            .timeout_read(GATEWAY_SEARCH_READ_TIMEOUT)
            .build();
        let mut request = agent
            .post(&gateway.url)
            .set("Authorization", &format!("Bearer {}", gateway.api_key));
        for (name, value) in &gateway.headers {
            request = request.set(name, value);
        }
        request
            .send_json(body.clone())
            .map_err(status_error)?
            .into_json::<Value>()
            .map_err(|error| format!("unreadable reply: {error}"))
    }

    fn get_page(&self, url: &str) -> Result<String, String> {
        crate::web_fetch::get_text(url, SEARCH_USER_AGENT, MAX_SEARCH_PAGE_BYTES)
    }
}

/// An HTTP failure as one line: the status and the server's own message.
fn status_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_json::<Value>().unwrap_or(Value::Null);
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            format!("HTTP {code}: {message}")
        }
        ureq::Error::Transport(transport) => format!("unreachable: {transport}"),
    }
}

/// Models whose route answered without searching, and when: they are not
/// asked again before `NO_TOOL_PAUSE` has passed.
fn no_tool_models() -> &'static Mutex<HashMap<String, Instant>> {
    static MODELS: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> =
        std::sync::OnceLock::new();
    MODELS.get_or_init(Default::default)
}

fn paused(model: &str) -> bool {
    no_tool_models()
        .lock()
        .ok()
        .and_then(|models| models.get(model).copied())
        .is_some_and(|since| since.elapsed() < NO_TOOL_PAUSE)
}

fn pause(model: &str) {
    if let Ok(mut models) = no_tool_models().lock() {
        models.insert(model.to_string(), Instant::now());
    }
}

/// Why a model's reply carried no search: its route has no search tool. Such
/// a model is paused rather than asked every time.
const ANSWERED_WITHOUT_SEARCHING: &str = "it answered without searching";

/// `web_search` on the default engine: the chain in the module docs.
fn search(
    args: &Value,
    model: Option<&SearchModel>,
    gateway: Option<&GatewaySearch>,
    io: &dyn SearchIo,
) -> Value {
    let query = match search_query(args) {
        Ok(query) => query,
        Err(error) => return json!({ "error": error }),
    };
    let max_results = match search_max_results(args) {
        Ok(max) => max,
        Err(error) => return json!({ "error": error }),
    };
    let freshness = args.get("freshness").and_then(Value::as_str);
    let mut skipped: Vec<String> = Vec::new();

    if let Some(model) = model.filter(|model| !paused(&model.model)) {
        let reply = io
            .ask_model(model, &model_search_body(model, &query, freshness))
            .and_then(|reply| match model.wire {
                NativeSearch::Messages => read_model_search(&reply, max_results),
                NativeSearch::Responses => read_responses_search(&reply, max_results),
            });
        match reply {
            Ok(mut value) => {
                crate::log_info!(
                    "web_search",
                    "searched",
                    engine = "model",
                    model = model.model.clone(),
                    results = value["results"].as_array().map_or(0, Vec::len)
                );
                value["query"] = json!(query);
                value["engine"] = json!("model");
                value["model"] = json!(model.model);
                if !model.conversation {
                    value["note"] = json!(format!(
                        "searched by {} (web_search_model), not the conversation's model",
                        model.model
                    ));
                }
                return value;
            }
            Err(error) => {
                crate::log_warn!(
                    "web_search",
                    "model search failed",
                    model = model.model.clone(),
                    error = error.clone()
                );
                if error == ANSWERED_WITHOUT_SEARCHING {
                    pause(&model.model);
                }
                skipped.push(format!("{} could not search ({error})", model.model));
            }
        }
    }

    if let Some(gateway) = gateway {
        match gateway_search(gateway, io, &query, max_results, freshness) {
            Ok(mut value) => {
                crate::log_info!(
                    "web_search",
                    "searched",
                    engine = "gateway",
                    results = value["results"].as_array().map_or(0, Vec::len)
                );
                value["query"] = json!(query);
                if !skipped.is_empty() {
                    value["note"] = json!(format!(
                        "{}, so the gateway's search answered",
                        skipped.join("; ")
                    ));
                }
                return value;
            }
            Err(error) => {
                crate::log_warn!("web_search", "gateway search failed", error = error.clone());
                skipped.push(format!("the gateway's search failed ({error})"));
            }
        }
    }

    let mut local = local_search(args, io);
    if skipped.is_empty() {
        return local;
    }
    let why = skipped.join("; ");
    if let Some(error) = local.get("error").and_then(Value::as_str) {
        local["error"] = json!(format!("{why}; {error}"));
    } else {
        let note = match local.get("note").and_then(Value::as_str) {
            Some(note) => {
                format!("{why}, so these results come from this machine's search; {note}")
            }
            None => format!("{why}, so these results come from this machine's search"),
        };
        local["note"] = json!(note);
    }
    local
}

/// Step 2: the gateway's search. An error when it fails or finds nothing.
fn gateway_search(
    gateway: &GatewaySearch,
    io: &dyn SearchIo,
    query: &str,
    max_results: usize,
    freshness: Option<&str>,
) -> Result<Value, String> {
    let mut body = json!({ "query": query, "max_results": max_results });
    if let Some(period) = freshness {
        body["freshness"] = json!(period);
    }
    let reply = io.ask_gateway(gateway, &body)?;
    let results: Vec<Value> = reply
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            let text = |key: &str| {
                item.get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
            };
            let mut result = json!({ "title": text("title").unwrap_or(url), "url": url });
            // The summary is written for a model to read; the snippet is the
            // short form when there is none.
            if let Some(snippet) = text("summary").or_else(|| text("snippet")) {
                result["snippet"] = json!(snippet);
            }
            if let Some(date) = text("published_at") {
                result["published"] = json!(date);
            }
            Some(result)
        })
        .take(max_results)
        .collect();
    if results.is_empty() {
        return Err("it found no pages".to_string());
    }
    let engine = reply
        .get("engine")
        .and_then(Value::as_str)
        .unwrap_or("gateway");
    Ok(json!({ "engine": format!("gateway ({engine})"), "results": results }))
}

fn model_search_body(model: &SearchModel, query: &str, freshness: Option<&str>) -> Value {
    let mut ask = format!("Query: {query}");
    if let Some(period) = freshness {
        ask.push_str(&format!(
            "\nOnly use pages published within the past {period}."
        ));
    }
    match model.wire {
        NativeSearch::Messages => json!({
            "model": model.model,
            "max_tokens": MODEL_SEARCH_MAX_TOKENS,
            "system": MODEL_SEARCH_SYSTEM,
            "messages": [{ "role": "user", "content": ask }],
            "tools": [{
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": MODEL_SEARCH_MAX_USES,
            }],
        }),
        NativeSearch::Responses => json!({
            "model": model.model,
            "instructions": MODEL_SEARCH_SYSTEM,
            "input": ask,
            "tools": [{ "type": "web_search" }],
            "reasoning": { "effort": "low" },
            "max_output_tokens": MODEL_SEARCH_MAX_TOKENS * 2,
            "store": false,
            "stream": true,
        }),
    }
}

/// A Responses search's output items as `web_search` returns them: the
/// report, the searches it ran, and the pages its report cites.
fn read_responses_search(items: &Value, max_results: usize) -> Result<Value, String> {
    let items = items.as_array().ok_or("the reply carried no output")?;
    let mut searches = Vec::new();
    let mut report = String::new();
    let mut pages: Vec<Value> = Vec::new();
    let mut searched = false;
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("web_search_call") => {
                searched = true;
                if let Some(query) = item.pointer("/action/query").and_then(Value::as_str) {
                    searches.push(query.to_string());
                }
                for source in item
                    .pointer("/action/sources")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(url) = source.get("url").and_then(Value::as_str) {
                        push_page(&mut pages, url, None);
                    }
                }
            }
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                    for note in part
                        .get("annotations")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|note| {
                            note.get("type").and_then(Value::as_str) == Some("url_citation")
                        })
                    {
                        if let Some(url) = note.get("url").and_then(Value::as_str) {
                            push_page(&mut pages, url, note.get("title").and_then(Value::as_str));
                        }
                    }
                    report.push_str(text);
                }
            }
            _ => {}
        }
    }
    if !searched {
        return Err(ANSWERED_WITHOUT_SEARCHING.to_string());
    }
    if pages.is_empty() {
        return Err("its searches found no pages".to_string());
    }
    // Cited pages were pushed after the sources of the calls; cited first.
    pages.sort_by_key(|page| page.get("cited").is_none());
    for page in &mut pages {
        if let Some(map) = page.as_object_mut() {
            map.remove("cited");
        }
    }
    pages.truncate(max_results);
    Ok(json!({ "searches": searches, "summary": cap_report(&report), "results": pages }))
}

/// Adds a page once; a page with a title came from a citation.
fn push_page(pages: &mut Vec<Value>, url: &str, title: Option<&str>) {
    let title = title.filter(|title| !title.is_empty());
    if let Some(page) = pages.iter_mut().find(|page| page["url"] == url) {
        if let Some(title) = title {
            page["title"] = json!(title);
            page["cited"] = json!(true);
        }
        return;
    }
    let mut page = json!({ "title": title.unwrap_or(url), "url": url });
    if title.is_some() {
        page["cited"] = json!(true);
    }
    pages.push(page);
}

fn cap_report(report: &str) -> String {
    let report = report.trim();
    match report.char_indices().nth(MAX_REPORT_CHARS) {
        Some((end, _)) => format!("{}…", &report[..end]),
        None => report.to_string(),
    }
}

/// The longest report the conversation gets; the model is asked for 300
/// words, about 2 KB.
const MAX_REPORT_CHARS: usize = 6000;

/// The search model's reply as `web_search` returns it: the report, the
/// searches it ran and the pages they found (the ones the report cites
/// first, each with the passage cited). An error when no search found a page.
fn read_model_search(reply: &Value, max_results: usize) -> Result<Value, String> {
    let blocks = reply
        .get("content")
        .and_then(Value::as_array)
        .ok_or("the reply carried no content")?;
    let mut searches = Vec::new();
    let mut pages: Vec<Value> = Vec::new();
    let mut errors = Vec::new();
    let mut report = String::new();
    let mut cited: Vec<(String, String)> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("server_tool_use") => {
                if let Some(query) = block.pointer("/input/query").and_then(Value::as_str) {
                    searches.push(query.to_string());
                }
            }
            Some("web_search_tool_result") => match block.get("content") {
                Some(Value::Array(items)) => {
                    for item in items {
                        let Some(url) = item.get("url").and_then(Value::as_str) else {
                            continue;
                        };
                        if pages.iter().any(|page| page["url"] == url) {
                            continue;
                        }
                        let mut page = json!({
                            "title": item.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(url),
                            "url": url,
                        });
                        if let Some(age) = item.get("page_age").and_then(Value::as_str) {
                            page["published"] = json!(age);
                        }
                        pages.push(page);
                    }
                }
                other => errors.push(
                    other
                        .and_then(|error| error.get("error_code"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                ),
            },
            Some("text") => {
                report.push_str(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                for citation in block
                    .get("citations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let (Some(url), Some(text)) = (
                        citation.get("url").and_then(Value::as_str),
                        citation.get("cited_text").and_then(Value::as_str),
                    ) {
                        if !cited.iter().any(|(seen, _)| seen == url) {
                            cited.push((url.to_string(), text.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if pages.is_empty() {
        return Err(match (errors.first(), searches.is_empty()) {
            (Some(error), _) => format!("the search failed: {error}"),
            (None, true) => ANSWERED_WITHOUT_SEARCHING.to_string(),
            (None, false) => "its searches found no pages".to_string(),
        });
    }
    // The pages the report rests on first, with what it took from each.
    for page in &mut pages {
        if let Some((_, text)) = cited.iter().find(|(url, _)| page["url"] == url.as_str()) {
            page["snippet"] = json!(text);
        }
    }
    pages.sort_by_key(|page| {
        cited
            .iter()
            .position(|(url, _)| page["url"] == url.as_str())
            .unwrap_or(usize::MAX)
    });
    pages.truncate(max_results);
    Ok(json!({ "searches": searches, "summary": cap_report(&report), "results": pages }))
}

// ---------------------------------------------------------------------------
// Local search: a search engine's own HTML result page, parsed here. No key,
// no login, and the same proxy as web_fetch.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// A result page to try: Bing first (its markup is stable and its snippets
/// are the longest), its China host when the global one is unreachable,
/// DuckDuckGo's HTML endpoint, then Sogou, which answers from networks in
/// China where the others are blocked or redirected.
struct SearchSource {
    engine: &'static str,
    url: fn(&str) -> String,
    parse: fn(&str, usize) -> Vec<SearchResult>,
}

const SEARCH_SOURCES: &[SearchSource] = &[
    SearchSource {
        engine: "bing",
        url: |query| format!("https://www.bing.com/search?q={query}&count=20&setlang=en"),
        parse: parse_bing,
    },
    SearchSource {
        engine: "bing",
        url: |query| format!("https://cn.bing.com/search?q={query}&count=20&ensearch=1"),
        parse: parse_bing,
    },
    SearchSource {
        engine: "duckduckgo",
        url: |query| format!("https://html.duckduckgo.com/html/?q={query}"),
        parse: parse_duckduckgo,
    },
    SearchSource {
        engine: "sogou",
        url: |query| format!("https://www.sogou.com/web?query={query}"),
        parse: parse_sogou,
    },
];

/// Step 3: this machine's search. The error is in Chinese: it is the last
/// step, and the user reads it.
fn local_search(args: &Value, io: &dyn SearchIo) -> Value {
    let query = match search_query(args) {
        Ok(query) => query,
        Err(error) => return json!({ "error": error }),
    };
    let max_results = match search_max_results(args) {
        Ok(max) => max,
        Err(error) => return json!({ "error": error }),
    };
    let encoded = url_encode(&query);
    let mut failures = Vec::new();
    for source in SEARCH_SOURCES {
        let url = (source.url)(&encoded);
        match io.get_page(&url) {
            Ok(page) => {
                let results = (source.parse)(&page, max_results);
                if results.is_empty() {
                    failures.push(format!("{}：{}", source.engine, empty_page_reason(&page)));
                    continue;
                }
                // An engine the network redirects elsewhere answers a page of
                // its own: results shaped right and about something else. Such
                // a page names one of the query's words at most, so the next
                // engine gets its turn rather than the model getting nonsense.
                if !results_match_query(&query, &results) {
                    failures.push(format!("{}：结果与查询无关", source.engine));
                    continue;
                }
                crate::log_info!(
                    "web_search",
                    "searched",
                    engine = source.engine,
                    results = results.len()
                );
                let mut value = json!({
                    "query": query,
                    "engine": source.engine,
                    "results": results
                        .iter()
                        .map(|result| json!({
                            "title": result.title,
                            "url": result.url,
                            "snippet": result.snippet,
                        }))
                        .collect::<Vec<_>>(),
                });
                if args.get("freshness").and_then(Value::as_str).is_some() {
                    value["note"] = json!(
                        "freshness is not supported by the local search engine and was ignored"
                    );
                }
                return value;
            }
            Err(error) => failures.push(format!("{}：{error}", source.engine)),
        }
    }
    crate::log_warn!("web_search", "search failed", error = failures.join("; "));
    json!({
        "query": query,
        "error": format!("本机的搜索引擎都没有给出结果（{}）", failures.join("；")),
    })
}

/// Latin words that say nothing about what a page is about.
const FILLER_WORDS: &[&str] = &[
    "the", "and", "for", "with", "how", "what", "when", "where", "which", "who", "why", "does",
    "from", "into", "about", "are", "was", "can", "you", "your", "this", "that", "there", "have",
    "has", "not", "but", "all", "any", "get", "its", "new", "latest", "newest", "current",
    "version", "best", "use", "using", "vs",
];

/// The query's telling parts: Latin words longer than two characters that
/// are not filler, and each pair of adjacent CJK characters (Chinese has no
/// spaces; a pair is the shortest unit that names something).
fn query_terms(query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    let mut terms: Vec<String> = Vec::new();
    for word in lower.split(|c: char| !c.is_alphanumeric() || is_cjk(c)) {
        if word.chars().count() > 2 && word.is_ascii() && !FILLER_WORDS.contains(&word) {
            terms.push(word.to_string());
        }
    }
    let chars: Vec<char> = lower.chars().collect();
    for pair in chars.windows(2) {
        if is_cjk(pair[0]) && is_cjk(pair[1]) {
            terms.push(pair.iter().collect());
        }
    }
    terms.sort_unstable();
    terms.dedup();
    terms
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}')
}

/// Whether results are about the query. A page the network answers in place
/// of the engine is about its own subject and names one term of the query at
/// most, often the broadest one ("rust" for "rust ureq crate"). Results
/// count as matching when they name more than half of the query's terms, or
/// at least two and a third of them (a long question whose answer uses other
/// words). A query without telling terms cannot be checked and matches.
fn results_match_query(query: &str, results: &[SearchResult]) -> bool {
    let terms = query_terms(query);
    if terms.is_empty() {
        return true;
    }
    let text = results
        .iter()
        .map(|result| format!("{} {} {}", result.title, result.url, result.snippet))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let found = terms
        .iter()
        .filter(|term| text.contains(term.as_str()))
        .count();
    found * 2 > terms.len() || (found >= 2 && found * 3 >= terms.len())
}

/// Why a page carried no results, as far as it says itself.
fn empty_page_reason(page: &str) -> &'static str {
    let lower = page.to_ascii_lowercase();
    if [
        "anomaly-modal",
        "captcha",
        "unusual traffic",
        "are you a human",
        "antispider",
        "安全验证",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "要求人机验证"
    } else {
        "结果页里没有结果"
    }
}

/// Bing wraps each organic result in `li.b_algo`: a heading anchor with the
/// title and target, and a `<p>` with the snippet.
fn parse_bing(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results: Vec<SearchResult> = Vec::new();
    for block in split_blocks(html, "b_algo") {
        // The result's own link is the heading's; a block also holds a
        // breadcrumb link to the same site above it.
        let heading = element_html(block, "h2").unwrap_or("");
        let Some((href, title)) = first_link(heading).or_else(|| first_link(block)) else {
            continue;
        };
        let Some(url) = result_url(&href) else {
            continue;
        };
        if results.iter().any(|result| result.url == url) {
            continue;
        }
        results.push(SearchResult {
            title: if title.is_empty() { url.clone() } else { title },
            url,
            snippet: element_text(block, "p").unwrap_or_default(),
        });
        if results.len() >= max_results {
            break;
        }
    }
    results
}

/// DuckDuckGo's HTML endpoint: `a.result__a` carries the title behind a
/// `/l/?uddg=…` redirect, `a.result__snippet` the snippet below it.
fn parse_duckduckgo(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results: Vec<SearchResult> = Vec::new();
    for link in links(html) {
        if link.class.contains("result__a") {
            if results.len() >= max_results {
                break;
            }
            let Some(url) = result_url(&link.href) else {
                continue;
            };
            if results.iter().any(|result| result.url == url) {
                continue;
            }
            results.push(SearchResult {
                title: if link.text.is_empty() {
                    url.clone()
                } else {
                    link.text
                },
                url,
                snippet: String::new(),
            });
        } else if link.class.contains("result__snippet") {
            if let Some(last) = results.last_mut() {
                if last.snippet.is_empty() {
                    last.snippet = link.text;
                }
            }
        }
    }
    results
}

/// Sogou wraps each organic result in `div.vrwrap`: a heading with the
/// title behind Sogou's own `/link?url=` redirect, the target itself in a
/// `data-url` attribute, and the snippet in a `space-txt` or
/// `cacheresult_summary` block.
fn parse_sogou(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results: Vec<SearchResult> = Vec::new();
    for block in split_blocks(html, "class=\"vrwrap\"") {
        let title = element_html(block, "h3")
            .map(strip_tags)
            .unwrap_or_default();
        // The heading's own link when it points straight at the page (Sogou's
        // redirect is relative, so `result_url` passes it over), else the
        // first `data-url` of the block.
        let target = first_link(element_html(block, "h3").unwrap_or(""))
            .map(|(href, _)| href)
            .into_iter()
            .chain(
                block
                    .match_indices(" data-url=")
                    .filter_map(|(at, _)| crate::web_fetch::attr_value(&block[at..], "data-url")),
            )
            .find_map(|href| result_url(&href));
        let Some(url) = target else {
            continue;
        };
        if title.is_empty() || results.iter().any(|result| result.url == url) {
            continue;
        }
        let snippet = ["cacheresult_summary", "space-txt", "str-info", "str_info"]
            .iter()
            .find_map(|marker| {
                let at = block.find(marker)?;
                let rest = &block[at..];
                let start = rest.find('>')? + 1;
                let end = rest[start..]
                    .find("</div")
                    .map_or(rest.len(), |end| start + end);
                Some(strip_tags(&rest[start..end])).filter(|text| !text.is_empty())
            })
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
        if results.len() >= max_results {
            break;
        }
    }
    results
}

/// The slices of `html` that start at each occurrence of `marker` (a class
/// name) and run to the next one: one result block each.
fn split_blocks<'a>(html: &'a str, marker: &str) -> Vec<&'a str> {
    let starts: Vec<usize> = html.match_indices(marker).map(|(at, _)| at).collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(html.len());
            &html[start..end]
        })
        .collect()
}

struct Link {
    class: String,
    href: String,
    text: String,
}

/// Every `<a>` in `html` with its class, href and visible text.
fn links(html: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("<a") {
        let after = &rest[at + 2..];
        // `<abbr>` and friends: an anchor's name ends at a space or `>`.
        if !after.starts_with([' ', '\t', '\n', '\r', '>']) {
            rest = after;
            continue;
        }
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let tag_body = &after[..tag_end];
        let body = &after[tag_end + 1..];
        let text_end = body.find("</a").unwrap_or(body.len());
        links.push(Link {
            class: crate::web_fetch::attr_value(tag_body, "class").unwrap_or_default(),
            href: crate::web_fetch::attr_value(tag_body, "href").unwrap_or_default(),
            text: strip_tags(&body[..text_end]),
        });
        rest = &body[text_end..];
    }
    links
}

/// The first link in `block` that points at a page: (href, text).
fn first_link(block: &str) -> Option<(String, String)> {
    links(block)
        .into_iter()
        .find(|link| result_url(&link.href).is_some())
        .map(|link| (link.href, link.text))
}

/// The markup inside the first `<name>` element in `block`.
fn element_html<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let at = block.find(&open)?;
    let after = &block[at + open.len()..];
    if !after.starts_with([' ', '\t', '\n', '\r', '>']) {
        return None;
    }
    let tag_end = after.find('>')?;
    let body = &after[tag_end + 1..];
    let end = body.find(&format!("</{name}")).unwrap_or(body.len());
    Some(&body[..end])
}

/// The text of the first `<name>` element in `block`.
fn element_text(block: &str, name: &str) -> Option<String> {
    element_html(block, name)
        .map(strip_tags)
        .filter(|text| !text.is_empty())
}

/// An http(s) result URL, or None when the href is a fragment, a script or
/// an engine's own page. A redirect through the engine (DuckDuckGo's
/// `uddg=`, Bing's `ck/a?…u=a1<base64url>`) is decoded to its target.
fn result_url(href: &str) -> Option<String> {
    let href = href.trim();
    let absolute = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    };
    if let Some(target) = query_param(&absolute, "uddg").map(|value| percent_decode(&value)) {
        return result_url(&target);
    }
    if absolute.contains("/ck/a") {
        if let Some(target) = query_param(&absolute, "u")
            .and_then(|value| value.strip_prefix("a1").map(str::to_string))
            .and_then(|value| decode_base64_url(&value))
        {
            return result_url(&target);
        }
    }
    if !absolute.starts_with("http://") && !absolute.starts_with("https://") {
        return None;
    }
    let host = absolute
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let own = [
        "bing.com",
        "duckduckgo.com",
        "microsofttranslator.com",
        "sogou.com",
    ];
    if own
        .iter()
        .any(|engine| host == *engine || host.ends_with(&format!(".{engine}")))
    {
        return None;
    }
    Some(absolute)
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split(['?', '#']).nth(1)?;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| crate::web_fetch::decode_entities(key).trim() == name)
        .map(|(_, value)| value.to_string())
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // One line: a title or snippet spread over several source lines reads as
    // one sentence.
    crate::web_fetch::decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Percent-encodes a query for a URL. Unreserved characters stay as they
/// are, everything else becomes %XX of its UTF-8 bytes.
fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Bing's `u=a1…` parameter is base64url without padding.
fn decode_base64_url(text: &str) -> Option<String> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .ok()?;
    String::from_utf8(decoded).ok()
}

fn gateway_post(web: &WebTools, path: &str, body: Value) -> Result<Value, String> {
    let auth = crate::oauth::ensure_session(&web.api_url, web.encrypt_secrets)?;
    let token = auth
        .lynshen_access_token()
        .ok_or("not logged in to LynShen. Run /login.")?;
    let url = format!("{}{}", web.api_url.trim_end_matches('/'), path);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build();
    match agent
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body)
    {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|error| format!("invalid gateway response: {error}")),
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_json::<Value>().unwrap_or(Value::Null);
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            Err(format!("LynShen gateway HTTP {code}: {message}"))
        }
        Err(ureq::Error::Transport(transport)) => {
            Err(format!("could not reach the LynShen gateway: {transport}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_body_carries_engine_and_validates() {
        let body = search_body(
            &json!({ "query": " rust ", "max_results": 3, "freshness": "week" }),
            "brave",
        )
        .unwrap();
        assert_eq!(
            body,
            json!({ "query": "rust", "engine": "brave", "max_results": 3, "freshness": "week" })
        );
        assert_eq!(
            search_body(&json!({ "query": "  " }), "auto").unwrap_err(),
            "missing query"
        );
        assert!(search_body(&json!({ "query": "x", "max_results": 11 }), "auto").is_err());
        assert!(search_body(&json!({ "query": "x", "max_results": 2.5 }), "auto").is_err());
    }

    #[test]
    fn fetch_result_matches_local_shape_and_caps_text() {
        let gateway = json!({
            "object": "web.fetch", "engine": "jina", "url": "https://example.com/",
            "title": "Example", "content": "héllo world", "status": 200
        });
        let result = fetch_result(&gateway, "https://example.com", Some(2));
        assert_eq!(result["url"], "https://example.com/");
        assert_eq!(result["status"], 200);
        assert_eq!(result["title"], "Example");
        // The cap backs off to a char boundary instead of splitting "é".
        assert_eq!(result["text"], "h");
        assert_eq!(result["truncated"], true);

        let full = fetch_result(&gateway, "https://example.com", None);
        assert_eq!(full["text"], "héllo world");
        assert!(full.get("truncated").is_none());
    }

    #[test]
    fn local_engine_and_missing_config_fetch_locally() {
        let local = WebTools {
            api_url: "https://api.lynshen.org".to_string(),
            encrypt_secrets: false,
            search_engine: "auto".to_string(),
            fetch_engine: "local".to_string(),
            signed_in: true,
            search_model: Err("none in this test".to_string()),
            gateway: Err("none in this test".to_string()),
        };
        for web in [None, Some(&local)] {
            let result = run_fetch(&json!({ "url": "ftp://example.com" }), web);
            assert!(
                result["error"].as_str().unwrap().contains("http"),
                "{result}"
            );
        }
    }

    #[test]
    fn search_without_config_reports_unavailable() {
        let result = run_search(&json!({ "query": "x" }), None);
        assert!(result["error"].as_str().unwrap().contains("not available"));
    }

    #[test]
    fn local_search_needs_no_login_and_the_gateway_does() {
        let mut web = WebTools {
            api_url: "https://api.lynshen.org".to_string(),
            encrypt_secrets: false,
            search_engine: "local".to_string(),
            fetch_engine: "local".to_string(),
            signed_in: false,
            search_model: Err("none in this test".to_string()),
            gateway: Err("none in this test".to_string()),
        };
        assert!(!web.gateway_search());
        assert!(web.search_available());
        web.search_engine = "gateway".to_string();
        assert!(web.gateway_search());
        assert!(!web.search_available());
        web.signed_in = true;
        assert!(web.search_available());
    }

    /// A fetcher over fixed pages that records what was asked for.
    struct FakeFetcher {
        pages: Vec<(&'static str, Result<String, String>)>,
        asked: std::cell::RefCell<Vec<String>>,
    }

    impl SearchIo for FakeFetcher {
        fn ask_model(&self, _: &SearchModel, _: &Value) -> Result<Value, String> {
            Err("no model in this test".to_string())
        }

        fn ask_gateway(&self, _: &GatewaySearch, _: &Value) -> Result<Value, String> {
            Err("no gateway in this test".to_string())
        }

        fn get_page(&self, url: &str) -> Result<String, String> {
            self.asked.borrow_mut().push(url.to_string());
            self.pages
                .iter()
                .find(|(host, _)| url.contains(*host))
                .map(|(_, page)| page.clone())
                .unwrap_or_else(|| Err("not in the fixture".to_string()))
        }
    }

    /// Bing's organic block, trimmed to the parts the parser reads.
    const BING_PAGE: &str = r#"<html><body><ol id="b_results">
        <li class="b_ad"><a href="https://ads.example/pay">Ad</a><p>buy now</p></li>
        <li class="b_algo"><h2><a href="https://doc.rust-lang.org/book/" h="ID=1">The Rust
            Programming Language</a></h2><div class="b_caption"><p>Learn Rust &amp; its
            ownership model.</p></div></li>
        <li class="b_algo"><h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=1&amp;u=a1aHR0cHM6Ly9jcmF0ZXMuaW8vY3JhdGVzL3VyZXE">crates.io:
            ureq</a></h2><p>A simple HTTP client.</p></li>
        </ol></body></html>"#;

    const DDG_PAGE: &str = r#"<html><body>
        <div class="result results_links"><h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fa&amp;rut=9f">Example
        A</a></h2><a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fa">First
        snippet.</a></div>
        <div class="result results_links"><h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fb">Example B</a>
        </h2><a class="result__snippet" href="/l/?x=1">Second snippet.</a></div>
        </body></html>"#;

    #[test]
    fn bing_results_carry_title_url_and_snippet() {
        let results = parse_bing(BING_PAGE, 10);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "The Rust Programming Language");
        assert_eq!(results[0].url, "https://doc.rust-lang.org/book/");
        assert_eq!(results[0].snippet, "Learn Rust & its ownership model.");
        // The engine's own redirect is decoded to the target page.
        assert_eq!(results[1].url, "https://crates.io/crates/ureq");
        assert_eq!(results[1].snippet, "A simple HTTP client.");
        // max_results caps the list.
        assert_eq!(parse_bing(BING_PAGE, 1).len(), 1);
        assert!(parse_bing("<html><body>nothing</body></html>", 10).is_empty());
    }

    #[test]
    fn duckduckgo_results_decode_the_redirect_url() {
        let results = parse_duckduckgo(DDG_PAGE, 10);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "Example A");
        assert_eq!(results[0].url, "https://example.org/a");
        assert_eq!(results[0].snippet, "First snippet.");
        assert_eq!(results[1].url, "https://example.org/b");
        assert_eq!(results[1].snippet, "Second snippet.");
        assert_eq!(parse_duckduckgo(DDG_PAGE, 1).len(), 1);
    }

    #[test]
    fn result_urls_decode_redirects_and_drop_the_engines_own_pages() {
        assert_eq!(
            result_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.test%2Fp%3Fq%3D1&rut=x").unwrap(),
            "https://a.test/p?q=1"
        );
        assert_eq!(
            result_url("https://www.bing.com/ck/a?u=a1aHR0cHM6Ly9iLnRlc3Qv").unwrap(),
            "https://b.test/"
        );
        assert_eq!(result_url("https://c.test/x").unwrap(), "https://c.test/x");
        assert_eq!(result_url("#top"), None);
        assert_eq!(result_url("javascript:void(0)"), None);
        assert_eq!(result_url("/search?q=next+page"), None);
        assert_eq!(result_url("https://www.bing.com/images/search?q=x"), None);
        assert_eq!(
            url_encode("rust ureq proxy?&=/"),
            "rust+ureq+proxy%3F%26%3D%2F"
        );
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
    }

    #[test]
    fn local_search_tries_bing_then_duckduckgo() {
        let fetcher = FakeFetcher {
            pages: vec![
                ("www.bing.com", Err("connection failed".to_string())),
                // Reachable but empty: a consent or captcha page.
                (
                    "cn.bing.com",
                    Ok("<html><body>no results</body></html>".to_string()),
                ),
                ("html.duckduckgo.com", Ok(DDG_PAGE.to_string())),
            ],
            asked: Default::default(),
        };
        let result = local_search(
            &json!({ "query": " example ", "max_results": 1, "freshness": "week" }),
            &fetcher,
        );
        assert!(result.get("error").is_none(), "{result}");
        assert_eq!(result["engine"], "duckduckgo");
        assert_eq!(result["query"], "example");
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["url"], "https://example.org/a");
        // freshness has no equivalent here, so it is reported as ignored.
        assert!(result["note"].as_str().unwrap().contains("freshness"));
        let asked = fetcher.asked.borrow().clone();
        assert_eq!(asked.len(), 3, "{asked:?}");
        assert!(asked[0].starts_with("https://www.bing.com/search?q=example"));
        assert!(asked[1].starts_with("https://cn.bing.com/search?q=example"));
        assert!(asked[2].starts_with("https://html.duckduckgo.com/html/?q=example"));

        // The first engine that answers wins and the rest are not asked.
        let fetcher = FakeFetcher {
            pages: vec![("www.bing.com", Ok(BING_PAGE.to_string()))],
            asked: Default::default(),
        };
        let result = local_search(&json!({ "query": "rust" }), &fetcher);
        assert_eq!(result["engine"], "bing");
        assert_eq!(fetcher.asked.borrow().len(), 1);
        assert!(result.get("note").is_none(), "{result}");
    }

    #[test]
    fn an_engine_answering_about_something_else_does_not_count() {
        // What this machine's exit gives for any query: a page of results
        // shaped right and about nothing asked for.
        const UNRELATED: &str = r#"<html><body><ol id="b_results">
        <li class="b_algo"><h2><a href="https://huatu.com/">华图在线</a></h2><p>公务员考试培训</p></li>
        </ol></body></html>"#;
        let fetcher = FakeFetcher {
            pages: vec![
                ("www.bing.com", Ok(UNRELATED.to_string())),
                ("cn.bing.com", Ok(UNRELATED.to_string())),
                ("html.duckduckgo.com", Ok(DDG_PAGE.to_string())),
            ],
            asked: Default::default(),
        };
        let result = local_search(&json!({ "query": "example" }), &fetcher);
        assert_eq!(result["engine"], "duckduckgo", "{result}");
        assert_eq!(fetcher.asked.borrow().len(), 3);

        // Every engine answering about something else is a failure, not a
        // page of results the model would take for an answer.
        let fetcher = FakeFetcher {
            pages: vec![
                ("www.bing.com", Ok(UNRELATED.to_string())),
                ("cn.bing.com", Ok(UNRELATED.to_string())),
                ("html.duckduckgo.com", Ok(UNRELATED.to_string())),
            ],
            asked: Default::default(),
        };
        let result = local_search(&json!({ "query": "ureq rust crate" }), &fetcher);
        let error = result["error"].as_str().unwrap_or_default();
        assert!(error.contains("结果与查询无关"), "{result}");
        assert!(result.get("results").is_none(), "{result}");

        // What this machine really answers for "rust ureq crate": the Rust
        // home page and tutorials. One word of three is not the query.
        const BROAD: &str = r#"<html><body><ol id="b_results">
        <li class="b_algo"><h2><a href="https://rust-lang.org/">Rust Programming Language</a></h2><p>Rust is blazingly fast.</p></li>
        <li class="b_algo"><h2><a href="https://www.runoob.com/rust/">Rust 教程</a></h2><p>Rust 语言由 Mozilla 开发</p></li>
        </ol></body></html>"#;
        let fetcher = FakeFetcher {
            pages: vec![
                ("www.bing.com", Ok(BROAD.to_string())),
                ("cn.bing.com", Ok(BROAD.to_string())),
                ("html.duckduckgo.com", Ok(BROAD.to_string())),
            ],
            asked: Default::default(),
        };
        let result = local_search(&json!({ "query": "rust ureq crate" }), &fetcher);
        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("结果与查询无关"),
            "{result}"
        );
        // The same page for a query it does answer is a result.
        let fetcher = FakeFetcher {
            pages: vec![("www.bing.com", Ok(BROAD.to_string()))],
            asked: Default::default(),
        };
        assert_eq!(
            local_search(&json!({ "query": "rust language" }), &fetcher)["engine"],
            "bing"
        );
    }

    #[test]
    fn a_query_the_results_cannot_be_checked_against_is_taken_as_it_is() {
        // Only short words and single CJK characters: nothing to compare, so
        // the engine's answer stands.
        let fetcher = FakeFetcher {
            pages: vec![("www.bing.com", Ok(BING_PAGE.to_string()))],
            asked: Default::default(),
        };
        assert_eq!(
            local_search(&json!({ "query": "go 棋 AI" }), &fetcher)["engine"],
            "bing"
        );
    }

    #[test]
    fn local_search_reports_bad_arguments_and_a_dead_network() {
        assert!(local_search(
            &json!({ "query": "  " }),
            &FakeFetcher {
                pages: vec![],
                asked: Default::default()
            }
        )["error"]
            .as_str()
            .unwrap()
            .contains("missing query"));
        assert!(local_search(
            &json!({ "query": "x", "max_results": 50 }),
            &FakeFetcher {
                pages: vec![],
                asked: Default::default()
            }
        )["error"]
            .as_str()
            .unwrap()
            .contains("max_results"));
        let dead = FakeFetcher {
            pages: vec![],
            asked: Default::default(),
        };
        let result = local_search(&json!({ "query": "x" }), &dead);
        let error = result["error"].as_str().unwrap();
        assert!(error.contains("本机的搜索引擎都没有给出结果"), "{error}");
        assert_eq!(dead.asked.borrow().len(), SEARCH_SOURCES.len());
    }

    /// Fixed answers for each step, recording what each was asked.
    struct FakeIo {
        model: Result<Value, String>,
        gateway: Result<Value, String>,
        pages: FakeFetcher,
        asked_model: std::cell::RefCell<Vec<Value>>,
        asked_gateway: std::cell::RefCell<Vec<Value>>,
    }

    impl FakeIo {
        fn new(model: Result<Value, String>, gateway: Result<Value, String>) -> Self {
            Self {
                model,
                gateway,
                pages: FakeFetcher {
                    pages: vec![("www.bing.com", Ok(BING_PAGE.to_string()))],
                    asked: Default::default(),
                },
                asked_model: Default::default(),
                asked_gateway: Default::default(),
            }
        }
    }

    impl SearchIo for FakeIo {
        fn ask_model(&self, _: &SearchModel, body: &Value) -> Result<Value, String> {
            self.asked_model.borrow_mut().push(body.clone());
            self.model.clone()
        }

        fn ask_gateway(&self, _: &GatewaySearch, body: &Value) -> Result<Value, String> {
            self.asked_gateway.borrow_mut().push(body.clone());
            self.gateway.clone()
        }

        fn get_page(&self, url: &str) -> Result<String, String> {
            self.pages.get_page(url)
        }
    }

    fn search_model(model: &str, wire: NativeSearch, conversation: bool) -> SearchModel {
        SearchModel {
            model: model.to_string(),
            wire,
            conversation,
            url: "https://gateway.test/v1/messages".to_string(),
            api_key: "key".to_string(),
            headers: Vec::new(),
            connect_timeout: Duration::from_secs(1),
        }
    }

    fn gateway() -> GatewaySearch {
        GatewaySearch {
            url: "https://gateway.test/tools/v1/search".to_string(),
            api_key: "key".to_string(),
            headers: Vec::new(),
            connect_timeout: Duration::from_secs(1),
        }
    }

    /// The gateway's search reply (DeepSeek behind it).
    fn gateway_reply() -> Value {
        json!({ "object": "web.search", "engine": "deepseek", "query": "q", "results": [
            { "title": "ureq 3.4.2", "url": "https://crates.io/crates/ureq",
              "snippet": "Simple HTTP", "summary": "ureq 3.4.2 was released on 2026-09-13.",
              "published_at": "2026-09-13T00:00:00Z", "site_name": "crates.io" },
            { "title": "ureq docs", "url": "https://docs.rs/ureq", "snippet": "API docs" }
        ] })
    }

    /// A Messages reply with one search, as the gateway returns it (the
    /// pages' encrypted content left out).
    fn searched_reply() -> Value {
        json!({ "content": [
            { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": { "query": "ureq crate latest version" } },
            { "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                { "type": "web_search_result", "title": "ureq - Rust", "url": "https://docs.rs/ureq", "encrypted_content": "x" },
                { "type": "web_search_result", "title": "ureq - crates.io", "url": "https://crates.io/crates/ureq", "page_age": "September 13, 2026", "encrypted_content": "x" },
                { "type": "web_search_result", "title": "", "url": "https://github.com/algesten/ureq", "encrypted_content": "x" }
            ] },
            { "type": "text", "text": "The newest ureq is " },
            { "type": "text", "text": "3.4.2, released September 13, 2026", "citations": [
                { "type": "web_search_result_location", "url": "https://crates.io/crates/ureq", "title": "ureq - crates.io", "cited_text": "ureq 3.4.2 · Simple, safe HTTP client" }
            ] },
            { "type": "text", "text": " (https://crates.io/crates/ureq).\n" }
        ] })
    }

    #[test]
    fn a_model_search_reports_its_summary_and_the_pages_it_cites_first() {
        let result = read_model_search(&searched_reply(), 10).unwrap();
        assert_eq!(result["searches"], json!(["ureq crate latest version"]));
        assert_eq!(
            result["summary"],
            "The newest ureq is 3.4.2, released September 13, 2026 (https://crates.io/crates/ureq)."
        );
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        // The cited page leads, with the passage the summary took from it.
        assert_eq!(results[0]["url"], "https://crates.io/crates/ureq");
        assert_eq!(
            results[0]["snippet"],
            "ureq 3.4.2 · Simple, safe HTTP client"
        );
        assert_eq!(results[0]["published"], "September 13, 2026");
        assert_eq!(results[1]["url"], "https://docs.rs/ureq");
        assert!(results[1].get("snippet").is_none());
        // A page without a title goes by its URL.
        assert_eq!(results[2]["title"], "https://github.com/algesten/ureq");
        assert_eq!(
            read_model_search(&searched_reply(), 1).unwrap()["results"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_model_search_that_found_nothing_is_an_error() {
        let failed = json!({ "content": [
            { "type": "server_tool_use", "id": "s", "name": "web_search", "input": { "query": "q" } },
            { "type": "web_search_tool_result", "tool_use_id": "s", "content": { "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" } }
        ] });
        assert_eq!(
            read_model_search(&failed, 10).unwrap_err(),
            "the search failed: max_uses_exceeded"
        );
        let unsearched = json!({ "content": [{ "type": "text", "text": "From memory: 2.9" }] });
        assert_eq!(
            read_model_search(&unsearched, 10).unwrap_err(),
            "it answered without searching"
        );
        assert!(read_model_search(&json!({ "error": "x" }), 10).is_err());
    }

    /// A Responses search as the stream's output items: one search call
    /// with its sources, then the cited report.
    fn responses_reply() -> Value {
        json!([
            { "type": "reasoning", "id": "rs_1", "summary": [] },
            { "type": "web_search_call", "id": "ws_1", "status": "completed",
              "action": { "type": "search", "query": "ureq latest version",
                          "sources": [{ "type": "url", "url": "https://docs.rs/ureq" },
                                      { "type": "url", "url": "https://crates.io/crates/ureq" }] } },
            { "type": "message", "role": "assistant", "content": [{
                "type": "output_text",
                "text": "The newest ureq is 3.4.2 (crates.io).",
                "annotations": [{ "type": "url_citation", "url": "https://crates.io/crates/ureq",
                                  "title": "ureq - crates.io", "start_index": 0, "end_index": 10 }]
            }] }
        ])
    }

    #[test]
    fn a_responses_search_reports_its_cited_pages_first() {
        let result = read_responses_search(&responses_reply(), 10).unwrap();
        assert_eq!(result["searches"], json!(["ureq latest version"]));
        assert_eq!(result["summary"], "The newest ureq is 3.4.2 (crates.io).");
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0],
            json!({ "title": "ureq - crates.io", "url": "https://crates.io/crates/ureq" })
        );
        assert_eq!(
            results[1],
            json!({ "title": "https://docs.rs/ureq", "url": "https://docs.rs/ureq" })
        );
        // A reply without a search call is the model answering from memory.
        let unsearched =
            json!([{ "type": "message", "content": [{ "type": "output_text", "text": "3.0" }] }]);
        assert_eq!(
            read_responses_search(&unsearched, 10).unwrap_err(),
            ANSWERED_WITHOUT_SEARCHING
        );
    }

    #[test]
    fn the_conversation_model_searches_first() {
        // Claude: the Messages server tool.
        let claude = search_model("claude-test-a", NativeSearch::Messages, true);
        let io = FakeIo::new(Ok(searched_reply()), Ok(gateway_reply()));
        let result = search(
            &json!({ "query": " ureq latest ", "freshness": "week" }),
            Some(&claude),
            Some(&gateway()),
            &io,
        );
        assert_eq!(result["engine"], "model", "{result}");
        assert_eq!(result["model"], "claude-test-a");
        assert_eq!(result["query"], "ureq latest");
        // The conversation's own model: nothing to point out.
        assert!(result.get("note").is_none(), "{result}");
        assert!(io.asked_gateway.borrow().is_empty());
        assert!(io.pages.asked.borrow().is_empty());
        let asked = io.asked_model.borrow();
        assert_eq!(asked[0]["model"], "claude-test-a");
        assert_eq!(asked[0]["tools"][0]["type"], "web_search_20250305");
        let ask = asked[0]["messages"][0]["content"].as_str().unwrap();
        assert!(
            ask.contains("ureq latest") && ask.contains("past week"),
            "{ask}"
        );
        drop(asked);

        // GPT: the Responses tool, streamed.
        let gpt = search_model("gpt-test-a", NativeSearch::Responses, true);
        let io = FakeIo::new(Ok(responses_reply()), Ok(gateway_reply()));
        let result = search(
            &json!({ "query": "ureq" }),
            Some(&gpt),
            Some(&gateway()),
            &io,
        );
        assert_eq!(result["engine"], "model", "{result}");
        assert_eq!(result["model"], "gpt-test-a");
        let asked = io.asked_model.borrow();
        assert_eq!(asked[0]["tools"][0]["type"], "web_search");
        assert_eq!(asked[0]["stream"], true);
        drop(asked);

        // A pinned web_search_model says it was not the conversation's model.
        let pinned = search_model("claude-test-b", NativeSearch::Messages, false);
        let io = FakeIo::new(Ok(searched_reply()), Ok(gateway_reply()));
        let result = search(&json!({ "query": "ureq" }), Some(&pinned), None, &io);
        assert!(
            result["note"]
                .as_str()
                .unwrap()
                .contains("not the conversation's model"),
            "{result}"
        );
    }

    #[test]
    fn a_model_that_cannot_search_hands_over_to_the_gateway() {
        // The model's route fails: the gateway's search answers, and says why.
        let model = search_model("claude-test-c", NativeSearch::Messages, true);
        let io = FakeIo::new(
            Err("HTTP 503: No available upstream provider".to_string()),
            Ok(gateway_reply()),
        );
        let result = search(
            &json!({ "query": "ureq", "max_results": 1, "freshness": "day" }),
            Some(&model),
            Some(&gateway()),
            &io,
        );
        assert_eq!(result["engine"], "gateway (deepseek)", "{result}");
        assert_eq!(
            result["results"],
            json!([{ "title": "ureq 3.4.2", "url": "https://crates.io/crates/ureq",
                     "snippet": "ureq 3.4.2 was released on 2026-09-13.",
                     "published": "2026-09-13T00:00:00Z" }])
        );
        let note = result["note"].as_str().unwrap();
        assert!(
            note.contains("claude-test-c") && note.contains("HTTP 503"),
            "{note}"
        );
        assert_eq!(
            io.asked_gateway.borrow()[0],
            json!({ "query": "ureq", "max_results": 1, "freshness": "day" })
        );
        assert!(io.pages.asked.borrow().is_empty());

        // A model without native search: straight to the gateway, no note.
        let io = FakeIo::new(Ok(searched_reply()), Ok(gateway_reply()));
        let result = search(&json!({ "query": "ureq" }), None, Some(&gateway()), &io);
        assert_eq!(result["engine"], "gateway (deepseek)");
        assert!(result.get("note").is_none(), "{result}");
        assert!(io.asked_model.borrow().is_empty());
    }

    #[test]
    fn a_route_without_the_search_tool_is_paused() {
        // The route answered from memory: the gateway answers, and the model
        // is not asked again for a while.
        let model = search_model("claude-test-no-tool", NativeSearch::Messages, true);
        let unsearched = json!({ "content": [{ "type": "text", "text": "From memory" }] });
        let io = FakeIo::new(Ok(unsearched), Ok(gateway_reply()));
        for _ in 0..2 {
            let result = search(
                &json!({ "query": "ureq" }),
                Some(&model),
                Some(&gateway()),
                &io,
            );
            assert_eq!(result["engine"], "gateway (deepseek)", "{result}");
        }
        assert_eq!(io.asked_model.borrow().len(), 1);
        // An error that is not about the tool does not pause the model.
        let flaky = search_model("claude-test-flaky", NativeSearch::Messages, true);
        let io = FakeIo::new(Err("HTTP 502: upstream".to_string()), Ok(gateway_reply()));
        for _ in 0..2 {
            search(
                &json!({ "query": "ureq" }),
                Some(&flaky),
                Some(&gateway()),
                &io,
            );
        }
        assert_eq!(io.asked_model.borrow().len(), 2);
    }

    #[test]
    fn this_machine_searches_last_and_says_why() {
        let model = search_model("claude-test-d", NativeSearch::Messages, true);
        let io = FakeIo::new(
            Err("HTTP 502: upstream".to_string()),
            Err("HTTP 503: web search is not available right now".to_string()),
        );
        let result = search(
            &json!({ "query": "rust" }),
            Some(&model),
            Some(&gateway()),
            &io,
        );
        assert_eq!(result["engine"], "bing", "{result}");
        let note = result["note"].as_str().unwrap();
        assert!(
            note.contains("HTTP 502") && note.contains("gateway's search failed"),
            "{note}"
        );
        // An empty gateway answer is a failure too.
        let io = FakeIo::new(Err("x".to_string()), Ok(json!({ "results": [] })));
        let result = search(
            &json!({ "query": "rust" }),
            Some(&model),
            Some(&gateway()),
            &io,
        );
        assert_eq!(result["engine"], "bing", "{result}");
        assert!(result["note"].as_str().unwrap().contains("found no pages"));

        // Everything fails: one error naming every step, ending in Chinese.
        let mut io = FakeIo::new(
            Err("HTTP 502: upstream".to_string()),
            Err("HTTP 503: down".to_string()),
        );
        io.pages.pages.clear();
        let result = search(
            &json!({ "query": "rust" }),
            Some(&model),
            Some(&gateway()),
            &io,
        );
        let error = result["error"].as_str().unwrap();
        assert!(
            error.contains("HTTP 502")
                && error.contains("HTTP 503")
                && error.contains("本机的搜索引擎都没有给出结果"),
            "{error}"
        );

        // Bad arguments reach nothing.
        let io = FakeIo::new(Ok(searched_reply()), Ok(gateway_reply()));
        let result = search(
            &json!({ "query": " " }),
            Some(&model),
            Some(&gateway()),
            &io,
        );
        assert_eq!(result["error"], "missing query");
        assert!(io.asked_model.borrow().is_empty() && io.asked_gateway.borrow().is_empty());
    }

    fn config(value: Value) -> crate::config::Config {
        crate::config::Config::from_value(
            &value.to_string(),
            std::path::PathBuf::from("config.json"),
        )
        .unwrap()
    }

    #[test]
    fn the_conversation_model_searches_where_its_provider_can() {
        let mut headers = HashMap::new();
        headers.insert(
            "claude-sonnet-5".to_string(),
            vec![("X-Monoize-Provider".to_string(), "p-1".to_string())],
        );
        let key = || Some("key".to_string());
        let claude = config(json!({
            "provider": "monoize", "protocol": "chat", "model": "claude-sonnet-5",
            "base_url": "https://gateway.test/v1"
        }));
        let model = SearchModel::from_config(&claude, key(), &headers).unwrap();
        assert_eq!(model.model, "claude-sonnet-5");
        assert_eq!(model.wire, NativeSearch::Messages);
        assert!(model.conversation);
        assert_eq!(model.url, "https://gateway.test/v1/messages");
        // The gateway's device header (when this computer has one), then the route.
        let expected: Vec<(String, String)> = crate::device_id::gateway_headers("monoize")
            .into_iter()
            .chain(headers["claude-sonnet-5"].clone())
            .collect();
        assert_eq!(model.headers, expected);
        assert!(SearchModel::from_config(&claude, None, &headers)
            .unwrap_err()
            .contains("API key"));

        let gpt = config(json!({
            "provider": "lynshen", "model": "gpt-6-sol", "base_url": "https://api.lynshen.org/v1"
        }));
        let model = SearchModel::from_config(&gpt, key(), &headers).unwrap();
        assert_eq!(model.wire, NativeSearch::Responses);
        assert_eq!(model.url, "https://api.lynshen.org/v1/responses");

        // No native search: the gateway's search is the first step.
        for model in [
            "deepseek-v4.1-flash",
            "gemini-3.8-flash",
            "grok-4.5",
            "gpt-image-2",
        ] {
            let other = config(json!({ "provider": "monoize", "model": model }));
            assert!(
                SearchModel::from_config(&other, key(), &headers).is_err(),
                "{model}"
            );
        }
        // A provider that may not run the tool is not guessed at.
        let zenmux = config(json!({ "provider": "zenmux", "model": "claude-sonnet-5" }));
        assert!(SearchModel::from_config(&zenmux, key(), &headers).is_err());

        // web_search_model pins another model, which the gateway must serve.
        let pinned = config(json!({
            "provider": "monoize", "model": "deepseek-v4.1-flash", "web_search_model": "claude-sonnet-5",
            "models": [{ "name": "deepseek-v4.1-flash" }, { "name": "claude-sonnet-5" }]
        }));
        let model = SearchModel::from_config(&pinned, key(), &headers).unwrap();
        assert_eq!(model.model, "claude-sonnet-5");
        assert!(!model.conversation);
        let unserved = config(json!({
            "provider": "monoize", "model": "deepseek-v4.1-flash", "web_search_model": "claude-sonnet-5",
            "models": [{ "name": "deepseek-v4.1-flash" }]
        }));
        assert!(SearchModel::from_config(&unserved, key(), &headers)
            .unwrap_err()
            .contains("does not serve"));
        // The retired default reads as unset: the conversation's model searches.
        let old = config(json!({
            "provider": "monoize", "model": "gpt-6-sol", "web_search_model": "claude-haiku-5-5"
        }));
        assert_eq!(
            SearchModel::from_config(&old, key(), &headers)
                .unwrap()
                .model,
            "gpt-6-sol"
        );
    }

    #[test]
    fn the_gateway_search_lives_beside_v1_on_a_gateway() {
        let key = || Some("key".to_string());
        let monoize = config(json!({
            "provider": "monoize", "model": "glm-5.3", "base_url": "https://www.lynshen.org/v1/"
        }));
        let gateway = GatewaySearch::from_config(&monoize, key()).unwrap();
        assert_eq!(gateway.url, "https://www.lynshen.org/tools/v1/search");
        assert_eq!(
            gateway.headers,
            crate::device_id::gateway_headers("monoize")
        );
        assert!(GatewaySearch::from_config(&monoize, None).is_err());
        let direct = config(json!({ "provider": "deepseek", "model": "deepseek-chat" }));
        assert!(GatewaySearch::from_config(&direct, key()).is_err());
    }

    /// Sogou's organic blocks, trimmed to the parts the parser reads.
    const SOGOU_PAGE: &str = r#"<html><body>
        <div class="vrwrap"><h3 class="vr-title"><a href="/link?url=abc">Rust 一个简单方便的 http 库 <em>ureq</em></a></h3>
        <div class="fz-mid space-txt">ureq 是一个 Rust 的 HTTP 客户端</div>
        <a class="citeLinkClass" href="/link?url=abc"></a><div data-url="https://mp.weixin.qq.com/s/x"></div></div>
        <div class="vrwrap"><h3 class="vr-title"><a href="https://github.com/algesten/ureq">algesten/ureq</a></h3>
        <div class="fz-mid" id="cacheresult_summary_1">A simple, safe HTTP client</div></div>
        <div class="vrwrap"><h3 class="vr-title"><a href="/link?url=nourl">No target</a></h3></div>
        </body></html>"#;

    #[test]
    fn sogou_results_take_the_target_page() {
        let results = parse_sogou(SOGOU_PAGE, 10);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "Rust 一个简单方便的 http 库 ureq");
        assert_eq!(results[0].url, "https://mp.weixin.qq.com/s/x");
        assert_eq!(results[0].snippet, "ureq 是一个 Rust 的 HTTP 客户端");
        assert_eq!(results[1].url, "https://github.com/algesten/ureq");
        assert_eq!(results[1].snippet, "A simple, safe HTTP client");
        assert_eq!(parse_sogou(SOGOU_PAGE, 1).len(), 1);
    }

    #[test]
    fn sogou_answers_where_the_others_are_blocked() {
        let fetcher = FakeFetcher {
            pages: vec![
                ("www.bing.com", Err("connection reset".to_string())),
                ("cn.bing.com", Err("connection reset".to_string())),
                (
                    "html.duckduckgo.com",
                    Ok("<html>anomaly-modal captcha</html>".to_string()),
                ),
                ("www.sogou.com", Ok(SOGOU_PAGE.to_string())),
            ],
            asked: Default::default(),
        };
        let result = local_search(&json!({ "query": "rust ureq" }), &fetcher);
        assert_eq!(result["engine"], "sogou", "{result}");
        assert_eq!(fetcher.asked.borrow().len(), 4);
    }

    #[test]
    fn chinese_queries_are_checked_too() {
        let page = |title: &str, snippet: &str| {
            vec![SearchResult {
                title: title.to_string(),
                url: "https://news.test/a".to_string(),
                snippet: snippet.to_string(),
            }]
        };
        let terms = query_terms("2026年诺贝尔物理学奖 the latest");
        assert!(
            terms.contains(&"物理".to_string()) && !terms.contains(&"latest".to_string()),
            "{terms:?}"
        );
        assert!(results_match_query(
            "2026年诺贝尔物理学奖",
            &page("2026年诺贝尔物理学奖揭晓", "瑞典皇家科学院宣布")
        ));
        // What a hijacked page answers: about its own subject.
        assert!(!results_match_query(
            "2026年诺贝尔物理学奖",
            &page("公务员考试培训", "华图在线")
        ));
        // Filler words prove nothing: "latest version" alone is not the query.
        assert!(!results_match_query(
            "ureq latest version",
            &page("Latest version of Windows", "the newest version")
        ));
    }

    #[test]
    #[ignore = "hits the real search engines"]
    fn live_local_search_returns_results() {
        let result = local_search(
            &json!({ "query": "rust ureq crate", "max_results": 3 }),
            &Live,
        );
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
        assert!(result.get("error").is_none(), "{result}");
        let results = result["results"].as_array().unwrap();
        assert!(!results.is_empty(), "{result}");
        for item in results {
            assert!(item["url"].as_str().unwrap().starts_with("http"), "{item}");
            assert!(!item["title"].as_str().unwrap().is_empty(), "{item}");
        }
    }
}

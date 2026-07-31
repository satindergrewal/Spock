//! Anthropic server-tool emulation (advisor + web_search) for Spock.
//! Spock-only — no Claude Code / VSCode changes.

use crate::backends::{get_backend, BackendHandle, UpstreamBody};
use crate::config::EnvOverrides;
use crate::config::UA;
use crate::error::{Error, Result};
use crate::route;
use crate::state::AppState;
use crate::translate::openai_to_anthropic;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct AdvisorConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub max_tokens: u32,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            max_tokens: 4096,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebSearchConfig {
    pub enabled: bool,
    pub provider: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub max_results: u32,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "duckduckgo".into(),
            base_url: None,
            api_key: None,
            api_key_env: None,
            max_results: 5,
        }
    }
}

impl AdvisorConfig {
    pub fn from_toml_table(t: &toml::Value) -> Self {
        Self {
            enabled: t.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            model: t
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            max_tokens: t
                .get("max_tokens")
                .and_then(|v| v.as_integer())
                .unwrap_or(4096) as u32,
        }
    }
}

impl WebSearchConfig {
    pub fn from_toml_table(t: &toml::Value) -> Self {
        Self {
            enabled: t.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            provider: t
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("duckduckgo")
                .to_string(),
            base_url: t
                .get("base_url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            api_key: t
                .get("api_key")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            api_key_env: t
                .get("api_key_env")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            max_results: t
                .get("max_results")
                .and_then(|v| v.as_integer())
                .unwrap_or(5) as u32,
        }
    }

    pub fn resolve_key(&self) -> Option<String> {
        if let Some(k) = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(k.to_string());
        }
        if let Some(env) = self
            .api_key_env
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Ok(v) = std::env::var(env) {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        for name in ["BRAVE_API_KEY", "SERPER_API_KEY", "TAVILY_API_KEY"] {
            if let Ok(v) = std::env::var(name) {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        None
    }
}

#[allow(dead_code)]
pub fn load_advisor(cfg_text: &str) -> AdvisorConfig {
    toml::from_str::<toml::Value>(cfg_text)
        .ok()
        .and_then(|v| v.get("advisor").cloned())
        .map(|t| AdvisorConfig::from_toml_table(&t))
        .unwrap_or_default()
}

#[allow(dead_code)]
pub fn load_web_search(cfg_text: &str) -> WebSearchConfig {
    toml::from_str::<toml::Value>(cfg_text)
        .ok()
        .and_then(|v| v.get("web_search").cloned())
        .map(|t| WebSearchConfig::from_toml_table(&t))
        .unwrap_or_default()
}

pub fn request_has_advisor(a: &Value) -> bool {
    a.get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter().any(|t| {
                t.get("type").and_then(|x| x.as_str()) == Some("advisor_20260301")
                    || t.get("name").and_then(|x| x.as_str()) == Some("advisor")
                        && t.get("input_schema").is_none()
            })
        })
        .unwrap_or(false)
}

pub fn request_has_web_search(a: &Value) -> bool {
    let in_tools = a
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter().any(|t| {
                let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
                let name = t.get("name").and_then(|x| x.as_str()).unwrap_or("");
                ty.starts_with("web_search") || name == "web_search"
            })
        })
        .unwrap_or(false);
    in_tools
}

pub fn advisor_model_from_request(a: &Value) -> Option<String> {
    a.get("tools").and_then(|t| t.as_array()).and_then(|arr| {
        arr.iter().find_map(|t| {
            if t.get("type").and_then(|x| x.as_str()) == Some("advisor_20260301")
                || t.get("name").and_then(|x| x.as_str()) == Some("advisor")
            {
                t.get("model")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
    })
}

/// Inject normal function tools so OpenAI-compat models can "call" server tools.
pub fn inject_emulated_function_tools(oai: &mut Value, want_advisor: bool, want_web_search: bool) {
    let Some(obj) = oai.as_object_mut() else {
        return;
    };
    let mut tools = obj
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    if want_advisor
        && !tools
            .iter()
            .any(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some("advisor"))
    {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "advisor",
                "description": "Consult a stronger-model advisor. No arguments required; full conversation is forwarded automatically.",
                "parameters": {"type": "object", "properties": {}}
            }
        }));
    }
    if want_web_search
        && !tools
            .iter()
            .any(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some("web_search"))
    {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the public web. Provide a query string.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"}
                    },
                    "required": ["query"]
                }
            }
        }));
    }
    if !tools.is_empty() {
        obj.insert("tools".into(), Value::Array(tools));
        // Ensure model may call tools
        if obj.get("tool_choice").is_none() {
            obj.insert("tool_choice".into(), json!("auto"));
        }
    }
}

/// Run advisor review via a nested chat completion.
pub fn run_advisor_review(
    state: &AppState,
    history: &Value,
    advisor_model_hint: &str,
    max_tokens: u32,
) -> Result<String> {
    let client_model = if advisor_model_hint.is_empty() {
        "fable".to_string()
    } else {
        advisor_model_hint.to_string()
    };
    let resolved = state.with_config(|c| route::resolve(c, &client_model))??;
    let be = {
        let backends = state
            .backends
            .read()
            .map_err(|_| Error::Msg("backends lock".into()))?;
        get_backend(&backends, &resolved.backend)?.clone()
    };

    let mut messages = Vec::new();
    messages.push(json!({
        "role": "system",
        "content": "You are a senior technical advisor. Review the conversation and return a concise plan or course-correction. Do not implement code. Be direct: verdict (approve/refine/pivot), plan steps, risks, and what to avoid."
    }));
    // Flatten anthropic-ish or openai history into a single user brief if needed
    if let Some(arr) = history.as_array() {
        for m in arr {
            messages.push(m.clone());
        }
    } else if let Some(arr) = history.get("messages").and_then(|m| m.as_array()) {
        for m in arr {
            // Convert anthropic content blocks to text if needed
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = m.get("content");
            let text = match content {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => content.map(|c| c.to_string()).unwrap_or_default(),
            };
            if !text.is_empty() {
                messages.push(json!({"role": role, "content": text}));
            }
        }
    }

    let body = json!({
        "model": resolved.upstream_model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0.2
    });

    eprintln!(
        "  advisor → {}:{} ({})",
        resolved.backend,
        resolved.upstream_model,
        be.family_name()
    );

    match be.chat(&body, false, &state.oauth)? {
        UpstreamBody::Json(o) => {
            let choice = o
                .pointer("/choices/0/message/content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if choice.is_empty() {
                // reasoning-only fallback
                let r = o
                    .pointer("/choices/0/message/reasoning_content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("(empty advisor response)");
                Ok(r.to_string())
            } else {
                Ok(choice)
            }
        }
        UpstreamBody::Stream(_) => Err(Error::Msg("advisor: unexpected stream".into())),
    }
}

/// Minimal web search — DuckDuckGo HTML (no key) or Brave if key present.
pub fn run_web_search(cfg: &WebSearchConfig, query: &str) -> Result<Value> {
    let q = query.trim();
    if q.is_empty() {
        return Err(Error::Msg("web_search: empty query".into()));
    }
    let max = cfg.max_results.clamp(1, 10) as usize;
    let provider = cfg.provider.to_ascii_lowercase();
    let key = cfg.resolve_key();

    if provider == "searxng" || provider == "searx" {
        let base = cfg
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("http://127.0.0.1:8888");
        return searxng_search(base, q, max);
    }
    if (provider == "brave" || key.as_ref().map(|k| k.starts_with("BSA")).unwrap_or(false))
        && key.is_some()
    {
        return brave_search(key.as_deref().unwrap(), q, max);
    }
    if provider == "serper" && key.is_some() {
        return serper_search(key.as_deref().unwrap(), q, max);
    }
    duckduckgo_search(q, max)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .user_agent(UA)
        .build()
}

fn brave_search(key: &str, q: &str, max: usize) -> Result<Value> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding_lite(q),
        max
    );
    let resp = agent()
        .get(&url)
        .set("Accept", "application/json")
        .set("X-Subscription-Token", key)
        .call()
        .map_err(|e| Error::Msg(format!("brave search: {e}")))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| Error::Msg(format!("brave json: {e}")))?;
    let mut results = Vec::new();
    if let Some(arr) = v.pointer("/web/results").and_then(|r| r.as_array()) {
        for r in arr.iter().take(max) {
            results.push(json!({
                "title": r.get("title").and_then(|t| t.as_str()).unwrap_or(""),
                "url": r.get("url").and_then(|t| t.as_str()).unwrap_or(""),
                "snippet": r.get("description").and_then(|t| t.as_str()).unwrap_or(""),
            }));
        }
    }
    Ok(json!(results))
}

fn serper_search(key: &str, q: &str, max: usize) -> Result<Value> {
    let body = json!({"q": q, "num": max});
    let resp = agent()
        .post("https://google.serper.dev/search")
        .set("Content-Type", "application/json")
        .set("X-API-KEY", key)
        .send_json(body)
        .map_err(|e| Error::Msg(format!("serper search: {e}")))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| Error::Msg(format!("serper json: {e}")))?;
    let mut results = Vec::new();
    if let Some(arr) = v.get("organic").and_then(|r| r.as_array()) {
        for r in arr.iter().take(max) {
            results.push(json!({
                "title": r.get("title").and_then(|t| t.as_str()).unwrap_or(""),
                "url": r.get("link").and_then(|t| t.as_str()).unwrap_or(""),
                "snippet": r.get("snippet").and_then(|t| t.as_str()).unwrap_or(""),
            }));
        }
    }
    Ok(json!(results))
}

fn searxng_search(base: &str, q: &str, max: usize) -> Result<Value> {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/search?q={}&format=json", urlencoding_lite(q));
    let resp = agent()
        .get(&url)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| Error::Msg(format!("searxng: {e}")))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| Error::Msg(format!("searxng json: {e}")))?;
    let mut results = Vec::new();
    if let Some(arr) = v.get("results").and_then(|r| r.as_array()) {
        for r in arr.iter().take(max) {
            results.push(json!({
                "title": r.get("title").and_then(|t| t.as_str()).unwrap_or(""),
                "url": r.get("url").and_then(|t| t.as_str()).unwrap_or(""),
                "snippet": r.get("content").and_then(|t| t.as_str()).unwrap_or(""),
            }));
        }
    }
    if results.is_empty() {
        results.push(json!({
            "title": "No results",
            "url": format!("{base}/search?q={}", urlencoding_lite(q)),
            "snippet": "SearXNG returned no results for this query."
        }));
    }
    Ok(json!(results))
}

/// Content blocks Claude Code's WebSearch client tool expects on the nested
/// Messages response: `server_tool_use` + `web_search_tool_result` (content =
/// array of `{title,url}` hits — optional `encrypted_content`/`page_age` ignored).
/// A trailing plain-text block keeps webviews / transcript history readable.
fn web_search_content_blocks(
    tool_use_id: &str,
    query: &str,
    results: &Value,
    display_text: &str,
) -> Vec<Value> {
    let hits: Vec<Value> = results
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let title = r
                        .get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let url = r
                        .get("url")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    if url.is_empty() && title.is_empty() {
                        return None;
                    }
                    let mut hit = json!({
                        "type": "web_search_result",
                        "title": title,
                        "url": url,
                    });
                    if let Some(s) = r.get("snippet").and_then(|t| t.as_str()) {
                        if !s.is_empty() {
                            hit.as_object_mut()
                                .unwrap()
                                .insert("encrypted_content".into(), json!(s));
                        }
                    }
                    Some(hit)
                })
                .collect()
        })
        .unwrap_or_default();

    let result_content = if hits.is_empty() {
        // Claude Code path: non-array content is treated as error.
        json!({"error_code": "no_results"})
    } else {
        Value::Array(hits)
    };

    vec![
        json!({
            "type": "server_tool_use",
            "id": tool_use_id,
            "name": "web_search",
            "input": {"query": query}
        }),
        json!({
            "type": "web_search_tool_result",
            "tool_use_id": tool_use_id,
            "content": result_content
        }),
        json!({"type": "text", "text": display_text}),
    ]
}

fn duckduckgo_search(q: &str, max: usize) -> Result<Value> {
    // Instant Answer API — limited but keyless; good enough for v1 emulation.
    let url = format!(
        "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
        urlencoding_lite(q)
    );
    let resp = agent()
        .get(&url)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| Error::Msg(format!("duckduckgo: {e}")))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| Error::Msg(format!("duckduckgo json: {e}")))?;
    let mut results = Vec::new();
    if let Some(abs) = v.get("AbstractText").and_then(|t| t.as_str()) {
        if !abs.is_empty() {
            results.push(json!({
                "title": v.get("Heading").and_then(|t| t.as_str()).unwrap_or("DuckDuckGo"),
                "url": v.get("AbstractURL").and_then(|t| t.as_str()).unwrap_or(""),
                "snippet": abs,
            }));
        }
    }
    if let Some(arr) = v.get("RelatedTopics").and_then(|r| r.as_array()) {
        for item in arr {
            if results.len() >= max {
                break;
            }
            if let Some(text) = item.get("Text").and_then(|t| t.as_str()) {
                results.push(json!({
                    "title": text.chars().take(80).collect::<String>(),
                    "url": item.get("FirstURL").and_then(|t| t.as_str()).unwrap_or(""),
                    "snippet": text,
                }));
            } else if let Some(topics) = item.get("Topics").and_then(|t| t.as_array()) {
                for t in topics {
                    if results.len() >= max {
                        break;
                    }
                    if let Some(text) = t.get("Text").and_then(|x| x.as_str()) {
                        results.push(json!({
                            "title": text.chars().take(80).collect::<String>(),
                            "url": t.get("FirstURL").and_then(|x| x.as_str()).unwrap_or(""),
                            "snippet": text,
                        }));
                    }
                }
            }
        }
    }
    if results.is_empty() {
        results.push(json!({
            "title": "No structured results",
            "url": format!("https://duckduckgo.com/?q={}", urlencoding_lite(q)),
            "snippet": "DuckDuckGo Instant Answer returned no abstract; open the URL for full results. Configure [web_search] provider=brave with BRAVE_API_KEY for richer results."
        }));
    }
    Ok(json!(results))
}

fn urlencoding_lite(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Multi-step non-stream loop: model may call advisor / web_search; Spock executes.
/// Returns final Anthropic-shaped message value.
///
/// Critical: client tools (Read, Bash, …) are NOT executed by Spock. If the model
/// calls any client tool, we return that turn to Claude Code so its tool runner
/// can execute it. We only loop when the model calls advisor / web_search.
#[allow(clippy::too_many_arguments)]
pub fn run_with_server_tools(
    state: &AppState,
    anthropic_req: &Value,
    mut oai: Value,
    client_model: &str,
    include_thinking: bool,
    advisor_cfg: &AdvisorConfig,
    web_cfg: &WebSearchConfig,
    be: &BackendHandle,
    _env: &EnvOverrides,
) -> Result<Value> {
    let want_advisor = advisor_cfg.enabled && request_has_advisor(anthropic_req);
    let want_web = web_cfg.enabled && request_has_web_search(anthropic_req);
    inject_emulated_function_tools(&mut oai, want_advisor, want_web);

    let advisor_hint = advisor_cfg
        .model
        .clone()
        .or_else(|| advisor_model_from_request(anthropic_req))
        .unwrap_or_else(|| "fable".into());

    let mut messages = oai
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    let mut collected_server_blocks: Vec<Value> = Vec::new();
    const MAX_ROUNDS: usize = 6;

    for round in 0..MAX_ROUNDS {
        let mut body = oai.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("messages".into(), Value::Array(messages.clone()));
            obj.insert("stream".into(), json!(false));
        }

        let resp = match be.chat(&body, false, &state.oauth)? {
            UpstreamBody::Json(j) => j,
            UpstreamBody::Stream(_) => {
                return Err(Error::Msg("server_tools: unexpected stream".into()))
            }
        };

        let choice = resp.pointer("/choices/0").cloned().unwrap_or(json!({}));
        let msg = choice.get("message").cloned().unwrap_or(json!({}));
        let tool_calls = msg
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        if tool_calls.is_empty() {
            // Final assistant message — merge any server blocks then convert
            let mut anth = openai_to_anthropic(&resp, client_model, include_thinking);
            if !collected_server_blocks.is_empty() {
                if let Some(content) = anth.get_mut("content").and_then(|c| c.as_array_mut()) {
                    let mut merged = collected_server_blocks.clone();
                    merged.append(content);
                    *content = merged;
                }
            }
            let _ = round;
            return Ok(anth);
        }

        // Partition tool calls: server tools (advisor/web_search) we run now;
        // client tools (Read, Bash, …) must be returned to Claude Code to execute.
        let mut server_calls: Vec<&Value> = Vec::new();
        let mut client_calls: Vec<&Value> = Vec::new();
        for tc in &tool_calls {
            let name = tc
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let is_server =
                (name == "advisor" && want_advisor) || (name == "web_search" && want_web);
            if is_server {
                server_calls.push(tc);
            } else {
                client_calls.push(tc);
            }
        }

        // If the model calls client tools, return this turn to Claude Code.
        // Prepend any server_tool_use blocks we already collected earlier so the
        // Advisor UI still lights up; Claude Code executes the client tool_use.
        if !client_calls.is_empty() {
            let mut anth = openai_to_anthropic(&resp, client_model, include_thinking);
            if !collected_server_blocks.is_empty() {
                if let Some(content) = anth.get_mut("content").and_then(|c| c.as_array_mut()) {
                    let mut merged = collected_server_blocks.clone();
                    merged.append(content);
                    *content = merged;
                }
            }
            return Ok(anth);
        }

        // Only server tools this turn — run them and loop.
        // Append assistant tool_calls message
        messages.push(msg.clone());

        for tc in server_calls {
            let id = tc
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("call_0")
                .to_string();
            let name = tc
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args_raw = tc
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let args: Value = serde_json::from_str(args_raw).unwrap_or(json!({}));

            // Emit blocks Claude Code already knows how to parse.
            // - advisor: plain text (webview has no advisor_tool_result renderer).
            // - web_search: real server_tool_use + web_search_tool_result so the
            //   WebSearch client tool (nested Messages with web_search_20250305)
            //   can extract {title,url} hits. Plain text alone leaves WebSearch empty.
            let (tool_result_content, server_blocks) = match name {
                "advisor" if want_advisor => {
                    let review = run_advisor_review(
                        state,
                        &json!({"messages": messages}),
                        &advisor_hint,
                        advisor_cfg.max_tokens,
                    )?;
                    let display = if review.trim().is_empty() {
                        "Advisor reviewed the conversation (empty response).".to_string()
                    } else {
                        format!("Advisor review:\n\n{review}")
                    };
                    let blocks = vec![json!({"type": "text", "text": display})];
                    (review, blocks)
                }
                "web_search" if want_web => {
                    let query = args
                        .get("query")
                        .and_then(|q| q.as_str())
                        .unwrap_or("")
                        .to_string();
                    let results = run_web_search(web_cfg, &query)?;
                    let text = results.to_string();
                    // Also keep a human-readable text block for webviews that only
                    // render text (and for models reading history as plain content).
                    let display = if query.is_empty() {
                        format!("Web search results:\n{text}")
                    } else {
                        format!("Web search for “{query}”:\n{text}")
                    };
                    let blocks = web_search_content_blocks(&id, &query, &results, &display);
                    (text, blocks)
                }
                // Unreachable (filtered above) — defensive.
                other => {
                    let err = format!("unknown or disabled server tool: {other}");
                    (err, vec![])
                }
            };

            collected_server_blocks.extend(server_blocks);
            messages.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": tool_result_content
            }));
        }
    }

    Err(Error::Msg(
        "server_tools: exceeded max advisor/web_search rounds".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_advisor_type() {
        let a = json!({"tools":[{"type":"advisor_20260301","name":"advisor","model":"fable"}]});
        assert!(request_has_advisor(&a));
        assert_eq!(advisor_model_from_request(&a).as_deref(), Some("fable"));
    }

    #[test]
    fn detect_web_search() {
        let a = json!({"tools":[{"type":"web_search_20250305","name":"web_search"}]});
        assert!(request_has_web_search(&a));
    }

    #[test]
    fn web_search_blocks_have_server_tool_shape() {
        let results = json!([
            {"title":"Rust","url":"https://rust-lang.org/","snippet":"safe systems"}
        ]);
        let blocks = web_search_content_blocks("call_1", "rust", &results, "display");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "server_tool_use");
        assert_eq!(blocks[0]["name"], "web_search");
        assert_eq!(blocks[0]["input"]["query"], "rust");
        assert_eq!(blocks[1]["type"], "web_search_tool_result");
        assert_eq!(blocks[1]["tool_use_id"], "call_1");
        let hits = blocks[1]["content"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["url"], "https://rust-lang.org/");
        assert_eq!(hits[0]["title"], "Rust");
        assert_eq!(blocks[2]["type"], "text");
    }

    #[test]
    fn web_search_blocks_empty_is_error_object() {
        let blocks = web_search_content_blocks("c2", "q", &json!([]), "none");
        assert_eq!(blocks[1]["content"]["error_code"], "no_results");
    }

    #[test]
    fn inject_tools() {
        let mut oai = json!({"messages":[]});
        inject_emulated_function_tools(&mut oai, true, true);
        let tools = oai["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn inject_idempotent() {
        let mut oai = json!({"messages":[]});
        inject_emulated_function_tools(&mut oai, true, false);
        inject_emulated_function_tools(&mut oai, true, false);
        assert_eq!(oai["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn no_server_tools_detected_on_bash_only() {
        let a = json!({"tools":[{"name":"Bash","input_schema":{"type":"object"}}]});
        assert!(!request_has_advisor(&a));
        assert!(!request_has_web_search(&a));
    }

    #[test]
    fn web_search_config_default_provider() {
        let c = WebSearchConfig::default();
        assert_eq!(c.provider, "duckduckgo");
        assert!(!c.enabled);
    }

    #[test]
    fn urlencoding_spaces() {
        assert!(urlencoding_lite("a b").contains("%20"));
        assert_eq!(urlencoding_lite("ok"), "ok");
    }

    #[test]
    fn searxng_results_extraction() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: "searxng".into(),
            base_url: Some("http://127.0.0.1:8888".into()),
            api_key: None,
            api_key_env: None,
            max_results: 3,
        };
        // Provider must route to searxng branch (no key required).
        assert_eq!(cfg.provider, "searxng");
        assert_eq!(cfg.base_url.as_deref(), Some("http://127.0.0.1:8888"));
    }

    #[test]
    fn empty_query_errors() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: "duckduckgo".into(),
            base_url: None,
            api_key: None,
            api_key_env: None,
            max_results: 3,
        };
        assert!(run_web_search(&cfg, "  ").is_err());
    }
}

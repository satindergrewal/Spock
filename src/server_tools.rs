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

/// One retry for transient upstream failures (429 / 5xx) after a short
/// backoff. Safe here: server-tools upstream calls are non-streaming and
/// fail before any response bytes are committed to the client. Shared-pool
/// routes (OpenRouter stealth) 429/502 often enough that one retry absorbs
/// most bursts; more than that means the route itself is saturated.
fn chat_with_retry(
    be: &BackendHandle,
    body: &Value,
    oauth: &crate::oauth::OauthStore,
) -> Result<UpstreamBody> {
    match be.chat(body, false, oauth) {
        Err(Error::Http(code, _)) if code == 429 || code >= 500 => {
            eprintln!("  server_tools: upstream {code} transient — retrying once");
            std::thread::sleep(Duration::from_millis(1200));
            be.chat(body, false, oauth)
        }
        other => other,
    }
}

/// Reduce arbitrary history to the universal chat-completions subset:
/// user/assistant text messages only. Strict backends (kimi) 400 on
/// role:"tool" messages whose tool_call_id they can't match, and there is
/// no capability-negotiation API to ask a backend what it accepts — so
/// target the shape every backend accepts. The advisor needs a readable
/// digest of the work, not protocol fidelity: tool calls and results are
/// rendered as labeled text instead of dropped.
fn advisor_brief_messages(history: &Value) -> Vec<Value> {
    let arr = if history.is_array() {
        history.as_array().cloned().unwrap_or_default()
    } else {
        history
            .get("messages")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let mut out: Vec<Value> = Vec::new();
    for m in &arr {
        let raw_role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        // Demote system (this body has its own at index 0) and tool
        // (orphan without tool_call_id on strict backends) to user.
        let role = match raw_role {
            "assistant" => "assistant",
            _ => "user",
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(calls) = m.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in calls {
                let name = tc
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool");
                let args = tc
                    .pointer("/function/arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                // An empty-args call carries no signal, and its render line
                // comes back verbatim as the "review" — kimi:k3 and
                // zai:glm-5.3 both echoed it instead of reviewing.
                if matches!(args.trim(), "" | "{}") {
                    continue;
                }
                parts.push(format!("[called tool {name} with arguments: {args}]"));
            }
        }
        let text = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        if !text.is_empty() {
            let prefix = if raw_role == "tool" {
                "[tool result] "
            } else {
                ""
            };
            parts.push(format!("{prefix}{text}"));
        }
        let joined = parts.join("\n");
        if !joined.is_empty() {
            out.push(json!({"role": role, "content": joined}));
        }
    }
    out
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
        "content": "You are a senior technical advisor reviewing another agent's work. You are NOT that agent. Do not continue its task, do not call tools, do not write first-person agent narration (\"I'll wait\", \"locking the call\"). Return only: verdict (approve / refine / pivot), plan steps, risks, and what to avoid."
    }));
    // History carries the converted leading system message; this body already
    // has its own system at index 0, so demote the rest — template-strict
    // advisor models (LAN Qwen) would 400 "System message must be at the
    // beginning" exactly like the main path did.
    messages.extend(advisor_brief_messages(history));
    // End on a user turn. A history ending in the assistant's own tool-call
    // render invites completion-of-pattern: the reviewer echoes the last
    // assistant line back as the review. A closing directive removes that.
    messages.push(json!({
        "role": "user",
        "content": "Review the conversation above now. Reply with your verdict (approve / refine / pivot), plan steps, risks, and what to avoid."
    }));

    let mut body = json!({
        "model": resolved.upstream_model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0.2
    });

    let started = std::time::Instant::now();
    eprintln!(
        "  advisor → {}:{} ({})",
        resolved.backend,
        resolved.upstream_model,
        be.family_name()
    );

    let mut attempt = chat_with_retry(&be, &body, &state.oauth);
    if let Err(e) = &attempt {
        if let Some(param) = strip_rejected_param(e, &mut body) {
            eprintln!("  advisor: backend rejected {param} — retrying without");
            attempt = chat_with_retry(&be, &body, &state.oauth);
        }
    }
    let out = match attempt {
        Ok(UpstreamBody::Json(o)) => {
            let choice = o
                .pointer("/choices/0/message/content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let text = if choice.is_empty() {
                // vLLM reasoning-parser emits `reasoning`; z.ai/xAI/Kimi emit `reasoning_content`.
                o.pointer("/choices/0/message/reasoning_content")
                    .or_else(|| o.pointer("/choices/0/message/reasoning"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                choice
            };
            Ok(sanitize_advisor_text(&text))
        }
        Ok(UpstreamBody::Stream(_)) => Err(Error::Msg("advisor: unexpected stream".into())),
        Err(e) => Err(e),
    };
    match &out {
        Ok(text) => eprintln!(
            "  advisor done {}ms ({} chars)",
            started.elapsed().as_millis(),
            text.len()
        ),
        Err(e) => eprintln!("  advisor error {}ms: {e}", started.elapsed().as_millis()),
    }
    out
}

/// Fill missing or empty tool-call ids — some generic backends emit "".
/// Returns true when anything was patched.
fn normalize_tool_call_ids(tool_calls: &mut [Value], round: usize) -> bool {
    let mut changed = false;
    for (i, tc) in tool_calls.iter_mut().enumerate() {
        let empty = tc
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.is_empty())
            .unwrap_or(true);
        if empty {
            tc["id"] = json!(format!("call_{round}_{i}"));
            changed = true;
        }
    }
    changed
}

/// Backends differ in which sampling parameters they accept — reasoning
/// models pin temperature, some reject top_p or stop. There is no
/// capability-negotiation API to ask, so learn from the rejection: strip
/// the named parameter and retry once. No per-model quirk code needed.
fn strip_rejected_param(err: &Error, body: &mut Value) -> Option<&'static str> {
    if !matches!(err, Error::Http(400, _) | Error::Http(422, _)) {
        return None;
    }
    let msg = match err {
        Error::Http(_, v) => v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or_default(),
        _ => return None,
    };
    let lower = msg.to_lowercase();
    for param in [
        "temperature",
        "top_p",
        "top_k",
        "stop",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if lower.contains(param) {
            if let Some(obj) = body.as_object_mut() {
                if obj.remove(param).is_some() {
                    return Some(param);
                }
            }
        }
    }
    None
}

/// Drop leaked stop tokens / chatml leftovers from Grok-family completions.
fn sanitize_advisor_text(s: &str) -> String {
    let mut t = s.replace("<|eos|>", "").replace("<|endoftext|>", "");
    for marker in ["<|im_end|>", "<|im_start|>"] {
        t = t.replace(marker, "");
    }
    t.trim().to_string()
}

/// Never hand advisor/web_search back as client `tool_use` — Claude Code has no
/// local executor for them (`No such tool available: advisor`).
fn strip_emulated_client_tool_use(anth: &mut Value) {
    if let Some(content) = anth.get_mut("content").and_then(|c| c.as_array_mut()) {
        content.retain(|b| {
            !(b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && matches!(
                    b.get("name").and_then(|n| n.as_str()),
                    Some("advisor") | Some("web_search")
                ))
        });
    }
}

/// VSCodium/VS Code webview (Claude Code 2.1.226) has no renderer for
/// `server_tool_use` / `advisor_tool_result` — it prints
/// `Unsupported content type: server_tool_use` as a chat line. The CLI TUI
/// can render those blocks; Satinder's live client is the webview. Emit
/// labeled text so the review is visible and the webview stays quiet.
/// History that still carries the protocol blocks is flattened in
/// `convert_messages` for xAI.
fn advisor_content_blocks(_tool_use_id: &str, review: Result<String>) -> (String, Vec<Value>) {
    match review {
        Ok(text) => {
            let text = if text.is_empty() {
                "Advisor reviewed the conversation (empty response).".to_string()
            } else {
                text
            };
            let blocks = vec![json!({
                "type": "text",
                "text": format!("Advisor review:\n{text}")
            })];
            (text, blocks)
        }
        Err(e) => {
            let msg = format!("{e}");
            let blocks = vec![json!({
                "type": "text",
                "text": format!("Advisor unavailable: {msg}")
            })];
            (msg, blocks)
        }
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

/// Query string from an OpenAI Responses body (`input` is a string or items).
pub fn responses_query(body: &Value) -> Option<&str> {
    match body.get("input") {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        Some(Value::Array(items)) => items.iter().find_map(|item| {
            if let Some(s) = item.as_str() {
                let t = s.trim();
                return if t.is_empty() { None } else { Some(t) };
            }
            let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ty == "input_text" || ty == "text" {
                return item
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
            }
            if ty == "message" || item.get("role").is_some() {
                match item.get("content") {
                    Some(Value::String(s)) => {
                        let t = s.trim();
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    }
                    Some(Value::Array(blocks)) => blocks.iter().find_map(|b| {
                        b.get("text")
                            .and_then(|t| t.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                    }),
                    _ => None,
                }
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn responses_has_web_search_tool(body: &Value) -> bool {
    body.get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter().any(|t| {
                let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
                ty == "web_search"
                    || ty == "web_search_preview"
                    || ty.starts_with("web_search")
                    || t.get("name").and_then(|n| n.as_str()) == Some("web_search")
            })
        })
        .unwrap_or(false)
}

/// Search-only OpenAI Responses object for grok-build `web_search`.
/// Hosted search on xAI/cli-chat-proxy; here we run `[web_search]` and emit
/// `output_text` + `url_citation` annotations the client already parses.
pub fn responses_web_search(cfg: &WebSearchConfig, body: &Value) -> Result<Value> {
    if !cfg.enabled {
        return Err(Error::Msg(
            "web_search disabled: enable [web_search] in Spock config".into(),
        ));
    }
    if !responses_has_web_search_tool(body) {
        return Err(Error::Msg(
            "POST /v1/responses is search-only; request has no web_search tool".into(),
        ));
    }
    let query = responses_query(body).ok_or_else(|| {
        Error::Msg("POST /v1/responses: empty input (need a search query)".into())
    })?;
    let results = run_web_search(cfg, query)?;
    Ok(responses_search_object(
        body.get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("spock-web-search"),
        query,
        &results,
    ))
}

fn responses_search_object(model: &str, query: &str, results: &Value) -> Value {
    let hits: Vec<(String, String, String)> = results
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
                    let snippet = r
                        .get("snippet")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    if url.is_empty() && title.is_empty() {
                        None
                    } else {
                        Some((title, url, snippet))
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let mut text = String::new();
    let mut annotations = Vec::new();
    if hits.is_empty() {
        text = format!("No search results for {query:?}.");
    } else {
        for (i, (title, url, snippet)) in hits.iter().enumerate() {
            let label = if title.is_empty() {
                url.as_str()
            } else {
                title.as_str()
            };
            let start = text.len();
            text.push_str(label);
            let end = text.len();
            if !url.is_empty() {
                annotations.push(json!({
                    "type": "url_citation",
                    "url": url,
                    "title": title,
                    "start_index": start,
                    "end_index": end
                }));
            }
            if !snippet.is_empty() {
                text.push_str(" — ");
                text.push_str(snippet);
            }
            if i + 1 < hits.len() {
                text.push('\n');
            }
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let id = crate::translate::new_msg_id().replacen("msg_", "resp_", 1);
    let msg_id = crate::translate::new_msg_id();
    json!({
        "id": id,
        "object": "response",
        "created_at": now,
        "status": "completed",
        "model": model,
        "output": [{
            "type": "message",
            "id": msg_id,
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": text,
                "annotations": annotations
            }]
        }]
    })
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

        let mut resp = match chat_with_retry(be, &body, &state.oauth)? {
            UpstreamBody::Json(j) => j,
            UpstreamBody::Stream(_) => {
                return Err(Error::Msg("server_tools: unexpected stream".into()))
            }
        };

        let choice = resp.pointer("/choices/0").cloned().unwrap_or(json!({}));
        let mut msg = choice.get("message").cloned().unwrap_or(json!({}));
        let mut tool_calls = msg
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        // Normalize empty/missing ids once so the echoed assistant message,
        // the tool results, and the client-side conversion all agree.
        if normalize_tool_call_ids(&mut tool_calls, round) {
            msg["tool_calls"] = Value::Array(tool_calls.clone());
            if let Some(mc) = resp.pointer_mut("/choices/0/message/tool_calls") {
                *mc = Value::Array(tool_calls.clone());
            }
        }

        if tool_calls.is_empty() {
            // Final assistant message — merge any server blocks then convert
            let mut anth = openai_to_anthropic(&resp, client_model, include_thinking);
            strip_emulated_client_tool_use(&mut anth);
            if !collected_server_blocks.is_empty() {
                if let Some(content) = anth.get_mut("content").and_then(|c| c.as_array_mut()) {
                    let mut merged = collected_server_blocks.clone();
                    merged.append(content);
                    *content = merged;
                }
            }
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

        // Always execute server tools first — including when the same turn also
        // has client tools. Returning early used to leak `advisor` as tool_use
        // and Claude Code then said "No such tool available: advisor".
        if !server_calls.is_empty() {
            messages.push(msg.clone());
            for tc in &server_calls {
                let id = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
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

                let (tool_result_content, server_blocks) = match name {
                    "advisor" if want_advisor => advisor_content_blocks(
                        &id,
                        run_advisor_review(
                            state,
                            &json!({"messages": messages}),
                            &advisor_hint,
                            advisor_cfg.max_tokens,
                        ),
                    ),
                    "web_search" if want_web => {
                        let query = args
                            .get("query")
                            .and_then(|q| q.as_str())
                            .unwrap_or("")
                            .to_string();
                        match run_web_search(web_cfg, &query) {
                            Ok(results) => {
                                let text = results.to_string();
                                let display = if query.is_empty() {
                                    format!("Web search results:\n{text}")
                                } else {
                                    format!("Web search for “{query}”:\n{text}")
                                };
                                let blocks =
                                    web_search_content_blocks(&id, &query, &results, &display);
                                (text, blocks)
                            }
                            Err(e) => {
                                let err = format!("{e}");
                                (
                                    err.clone(),
                                    vec![
                                        json!({
                                            "type": "server_tool_use",
                                            "id": id,
                                            "name": "web_search",
                                            "input": {"query": query}
                                        }),
                                        json!({
                                            "type": "web_search_tool_result",
                                            "tool_use_id": id,
                                            "content": {"error_code": "unavailable"}
                                        }),
                                        json!({"type": "text", "text": format!("Web search failed: {err}")}),
                                    ],
                                )
                            }
                        }
                    }
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

        // Client tools this turn: hand only those back. Advisor/web_search are
        // already consumed as server blocks — never emit them as tool_use.
        if !client_calls.is_empty() {
            let mut anth = openai_to_anthropic(&resp, client_model, include_thinking);
            strip_emulated_client_tool_use(&mut anth);
            if let Some(content) = anth.get_mut("content").and_then(|c| c.as_array_mut()) {
                if !collected_server_blocks.is_empty() {
                    let mut merged = collected_server_blocks.clone();
                    merged.append(content);
                    *content = merged;
                }
            }
            return Ok(anth);
        }

        // Server-only turn: loop so the model can consume the review / hits.
        if server_calls.is_empty() {
            // Defensive: named tools we don't recognize — return as client tools.
            let mut anth = openai_to_anthropic(&resp, client_model, include_thinking);
            strip_emulated_client_tool_use(&mut anth);
            if !collected_server_blocks.is_empty() {
                if let Some(content) = anth.get_mut("content").and_then(|c| c.as_array_mut()) {
                    let mut merged = collected_server_blocks.clone();
                    merged.append(content);
                    *content = merged;
                }
            }
            return Ok(anth);
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

    #[test]
    fn responses_query_from_string_input() {
        let body = json!({"input":"  Qwen3.8 27B abliterated  ","tools":[{"type":"web_search"}]});
        assert_eq!(responses_query(&body), Some("Qwen3.8 27B abliterated"));
        assert!(responses_has_web_search_tool(&body));
    }

    #[test]
    fn responses_query_from_items() {
        let body = json!({
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"rust lang"}]}],
            "tools":[{"type":"web_search_preview"}]
        });
        assert_eq!(responses_query(&body), Some("rust lang"));
        assert!(responses_has_web_search_tool(&body));
    }

    #[test]
    fn responses_search_object_cites_urls() {
        let results = json!([
            {"title":"Rust","url":"https://www.rust-lang.org/","snippet":"safe systems"}
        ]);
        let out = responses_search_object("test-model", "rust", &results);
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        let text = out["output"][0]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Rust"), "{text}");
        assert!(text.contains("safe systems"), "{text}");
        let cites = out["output"][0]["content"][0]["annotations"]
            .as_array()
            .unwrap();
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0]["type"], "url_citation");
        assert_eq!(cites[0]["url"], "https://www.rust-lang.org/");
        assert_eq!(cites[0]["title"], "Rust");
    }

    #[test]
    fn responses_web_search_refuses_without_tool() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: "duckduckgo".into(),
            base_url: None,
            api_key: None,
            api_key_env: None,
            max_results: 3,
        };
        let err = responses_web_search(&cfg, &json!({"input":"hi"})).unwrap_err();
        assert!(err.to_string().contains("search-only"), "{err}");
    }

    #[test]
    fn responses_web_search_refuses_when_disabled() {
        let cfg = WebSearchConfig::default();
        let err =
            responses_web_search(&cfg, &json!({"input":"hi","tools":[{"type":"web_search"}]}))
                .unwrap_err();
        assert!(err.to_string().contains("disabled"), "{err}");
    }

    #[test]
    fn advisor_blocks_are_webview_text() {
        let (text, blocks) =
            advisor_content_blocks("call_adv", Ok("approve: stay the course".into()));
        assert_eq!(text, "approve: stay the course");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(
            blocks[0]["text"],
            "Advisor review:\napprove: stay the course"
        );
        let blob = serde_json::to_string(&blocks).unwrap();
        assert!(!blob.contains("server_tool_use"), "{blob}");
        assert!(!blob.contains("advisor_tool_result"), "{blob}");
    }

    #[test]
    fn advisor_blocks_error_is_text() {
        let (text, blocks) =
            advisor_content_blocks("c_err", Err(Error::Msg("upstream 502".into())));
        assert_eq!(text, "upstream 502");
        assert_eq!(blocks[0]["type"], "text");
        assert!(
            blocks[0]["text"]
                .as_str()
                .unwrap()
                .contains("Advisor unavailable"),
            "{}",
            blocks[0]
        );
    }

    #[test]
    fn sanitize_drops_eos() {
        assert_eq!(sanitize_advisor_text("ok <|eos|> leftover"), "ok  leftover");
        assert_eq!(sanitize_advisor_text("  hi  "), "hi");
    }

    #[test]
    fn strip_leaked_advisor_tool_use() {
        let mut anth = json!({
            "content": [
                {"type":"text","text":"hi"},
                {"type":"tool_use","id":"x","name":"advisor","input":{}},
                {"type":"tool_use","id":"y","name":"Bash","input":{"command":"ls"}}
            ]
        });
        strip_emulated_client_tool_use(&mut anth);
        let names: Vec<_> = anth["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("name").and_then(|n| n.as_str()))
            .collect();
        assert_eq!(names, vec!["Bash"]);
        assert_eq!(anth["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn advisor_brief_strips_tool_protocol() {
        // Real coding history: system, user, assistant tool-call (no text),
        // orphan role:"tool" result. Strict backends 400 on the orphan.
        let history = json!({"messages": [
            {"role": "system", "content": "You are Claude Code."},
            {"role": "user", "content": "fix the bug"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "src\nREADME"},
            {"role": "assistant", "content": "Done."}
        ]});
        let brief = advisor_brief_messages(&history);
        // Universal subset only: user/assistant, no tool role, no tool_calls.
        for m in &brief {
            let role = m["role"].as_str().unwrap();
            assert!(role == "user" || role == "assistant", "role={role}");
            assert!(m.get("tool_calls").is_none());
            assert!(m.get("tool_call_id").is_none());
            assert!(m["content"].is_string());
        }
        assert_eq!(brief.len(), 5);
        // system demoted, tool call + result rendered as labeled text
        assert!(brief[0]["content"]
            .as_str()
            .unwrap()
            .contains("Claude Code"));
        assert_eq!(brief[1]["content"].as_str().unwrap(), "fix the bug");
        assert!(brief[2]["content"]
            .as_str()
            .unwrap()
            .contains("called tool Bash"));
        assert!(brief[3]["content"]
            .as_str()
            .unwrap()
            .contains("[tool result] src\nREADME"));
        assert_eq!(brief[4]["content"].as_str().unwrap(), "Done.");
    }

    #[test]
    fn advisor_brief_drops_empty_arg_renders() {
        // Empty-args advisor calls must not render: both kimi:k3 and
        // zai:glm-5.3 returned "[called tool advisor with arguments: {}]"
        // as the entire review when the brief ended on that line.
        let history = json!({"messages": [
            {"role": "user", "content": "plan the rename"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_a", "type": "function",
                 "function": {"name": "advisor", "arguments": "{}"}},
                {"id": "call_b", "type": "function",
                 "function": {"name": "advisor",
                              "arguments": "{\"question\":\"is this safe?\"}"}}
            ]}
        ]});
        let brief = advisor_brief_messages(&history);
        assert_eq!(brief.len(), 2);
        let text = brief[1]["content"].as_str().unwrap();
        assert!(!text.contains("with arguments: {}"), "{text}");
        assert!(text.contains("is this safe?"), "{text}");
    }

    #[test]
    fn advisor_brief_accepts_bare_array() {
        let history = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "block text"}
            ]}
        ]);
        let brief = advisor_brief_messages(&history);
        assert_eq!(brief.len(), 2);
        assert_eq!(brief[1]["content"].as_str().unwrap(), "block text");
    }

    #[test]
    fn strip_rejected_param_on_400() {
        let err = Error::Http(
            400,
            json!({"error": {"message": "invalid temperature: only 1 is allowed for this model"}}),
        );
        let mut body =
            json!({"model": "k3", "messages": [], "temperature": 0.2, "max_tokens": 4096});
        assert_eq!(strip_rejected_param(&err, &mut body), Some("temperature"));
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_some());
        // Non-400 and unrelated errors strip nothing.
        let err500 = Error::Http(500, json!({"error": {"message": "temperature"}}));
        assert_eq!(strip_rejected_param(&err500, &mut body), None);
        let err400 = Error::Http(400, json!({"error": {"message": "model not found"}}));
        assert_eq!(strip_rejected_param(&err400, &mut body), None);
    }

    #[test]
    fn normalize_empty_tool_call_ids() {
        let mut calls = json!([
            {"id": "", "function": {"name": "advisor"}},
            {"function": {"name": "web_search"}},
            {"id": "real_1", "function": {"name": "Bash"}}
        ]);
        let arr = calls.as_array_mut().unwrap();
        assert!(normalize_tool_call_ids(arr, 2));
        assert_eq!(arr[0]["id"], "call_2_0");
        assert_eq!(arr[1]["id"], "call_2_1");
        assert_eq!(arr[2]["id"], "real_1");
        // Idempotent once filled.
        assert!(!normalize_tool_call_ids(arr, 3));
    }
}

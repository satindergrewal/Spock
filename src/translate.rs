//! Anthropic Messages ↔ OpenAI chat.completions translation.

use crate::models::{is_reasoning_model, sanitize_upstream, stop_reason};
use serde_json::{json, Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

fn short_id(prefix: &str, n: usize) -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = t.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!(
        "{prefix}{:0width$x}",
        mix % (1u128 << (n * 4).min(64)),
        width = n
    )
}

pub fn blocks_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let mut parts = Vec::new();
    if let Some(arr) = content.as_array() {
        for b in arr {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                }
            }
        }
    }
    parts.join("\n")
}

fn advisor_result_text(b: &Value) -> Option<String> {
    let c = b.get("content")?;
    match c.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "advisor_result" => c
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        "advisor_tool_result_error" => {
            let code = c
                .get("error_code")
                .and_then(|t| t.as_str())
                .unwrap_or("unavailable");
            Some(format!("Advisor unavailable ({code})"))
        }
        "advisor_redacted_result" => Some("Advisor review (redacted).".into()),
        _ => {
            if let Some(s) = c.as_str() {
                let s = s.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            } else {
                None
            }
        }
    }
}

fn web_search_result_text(b: &Value) -> Option<String> {
    let c = b.get("content")?;
    if let Some(arr) = c.as_array() {
        let mut lines = Vec::new();
        for hit in arr {
            let title = hit.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let url = hit.get("url").and_then(|t| t.as_str()).unwrap_or("");
            if title.is_empty() && url.is_empty() {
                continue;
            }
            if title.is_empty() {
                lines.push(url.to_string());
            } else if url.is_empty() {
                lines.push(title.to_string());
            } else {
                lines.push(format!("{title} — {url}"));
            }
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    } else if let Some(code) = c.get("error_code").and_then(|t| t.as_str()) {
        Some(format!("Web search error ({code})"))
    } else {
        c.as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// Anthropic `image` content block → OpenAI `image_url` part.
/// Supports `source.type = base64 | url`. Returns None if unusable.
fn image_block_to_openai(b: &Value) -> Option<Value> {
    let src = b.get("source")?;
    let url = match src.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "base64" => {
            let mt = src
                .get("media_type")
                .and_then(|t| t.as_str())
                .unwrap_or("image/png");
            let data = src.get("data").and_then(|t| t.as_str()).unwrap_or("");
            if data.is_empty() {
                return None;
            }
            format!("data:{mt};base64,{data}")
        }
        // Anthropic url source, or missing type with a url field.
        _ => {
            let u = src.get("url").and_then(|t| t.as_str()).unwrap_or("");
            if u.is_empty() {
                return None;
            }
            u.to_string()
        }
    };
    Some(json!({
        "type": "image_url",
        "image_url": {"url": url}
    }))
}

/// Pull OpenAI image_url parts out of an Anthropic tool_result `content`
/// (string | block array). Images inside Read/tool results are how Claude Code
/// delivers path-pasted screenshots — must not be text-stripped.
fn collect_images_from_content(content: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(arr) = content.as_array() {
        for b in arr {
            if b.get("type").and_then(|t| t.as_str()) == Some("image") {
                if let Some(part) = image_block_to_openai(b) {
                    out.push(part);
                }
            }
        }
    }
    out
}

/// z.ai GLM-5.3 (and point releases) reject every non-`text` content part:
/// `messages.content.type is invalid, allowed values: ['text']`.
fn is_text_only_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m == "glm-5.3" || m.starts_with("glm-5.3-") || m.starts_with("glm-5.3:")
}

fn image_omitted_note(n: usize) -> String {
    if n == 1 {
        "[image omitted: this model is text-only]".into()
    } else {
        format!("[{n} images omitted: this model is text-only]")
    }
}

/// OpenAI tool message content: plain string when text-only; multipart array
/// when the Anthropic tool_result carried image blocks (vision).
fn tool_result_openai_content(content: &Value, is_error: bool, text_only: bool) -> Value {
    let mut text = blocks_text(content);
    if is_error && !text.is_empty() {
        text = format!("Error: {text}");
    } else if is_error && text.is_empty() {
        text = "Error".into();
    }
    let images = collect_images_from_content(content);
    if text_only {
        if !images.is_empty() {
            let note = image_omitted_note(images.len());
            text = if text.is_empty() {
                note
            } else {
                format!("{text}\n{note}")
            };
        }
        return json!(text);
    }
    if images.is_empty() {
        return json!(text);
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    } else {
        // Some upstreams want at least one text part alongside images.
        parts.push(json!({
            "type": "text",
            "text": if is_error {
                "Error: [image tool result]"
            } else {
                "[image tool result]"
            }
        }));
    }
    parts.extend(images);
    Value::Array(parts)
}

pub fn map_output_effort(effort: &str) -> Option<&'static str> {
    match effort.trim().to_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("xhigh"),
        _ => None,
    }
}

/// Priority: thinking disabled → omit (xAI rejects `reasoning_effort: "none"`);
/// output_config.effort; budget buckets; enabled → high.
pub fn thinking_effort(a: &Value) -> Option<String> {
    let t = a.get("thinking");
    if let Some(obj) = t.and_then(|v| v.as_object()) {
        let kind = obj.get("type").and_then(|v| v.as_str());
        // Claude Code Auto Mode classifier sends thinking: {type: disabled}.
        // Mapping that to reasoning_effort "none" makes xAI return 400:
        // "This model does not support `reasoning_effort` value `none`."
        // Omitting the field is the correct "no extra reasoning" signal.
        if kind.is_none() || kind == Some("disabled") {
            return None;
        }
    }

    if let Some(oc) = a.get("output_config").and_then(|v| v.as_object()) {
        if let Some(e) = oc.get("effort").and_then(|v| v.as_str()) {
            if let Some(m) = map_output_effort(e) {
                return Some(m.into());
            }
        }
    }

    let obj = t.and_then(|v| v.as_object())?;
    let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "enabled" && kind != "adaptive" {
        return None;
    }
    match obj.get("budget_tokens").and_then(|v| v.as_i64()) {
        None => Some("high".into()),
        Some(b) if b < 5000 => Some("low".into()),
        Some(b) if b < 15000 => Some("medium".into()),
        Some(b) if b < 40000 => Some("high".into()),
        Some(_) => Some("xhigh".into()),
    }
}

pub fn wants_thinking(a: &Value) -> bool {
    if let Some(obj) = a.get("thinking").and_then(|v| v.as_object()) {
        let kind = obj.get("type").and_then(|v| v.as_str());
        if kind.is_none() || kind == Some("disabled") {
            return false;
        }
        if kind == Some("enabled") || kind == Some("adaptive") {
            return true;
        }
    }
    if a.get("stop_sequences")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty())
    {
        return false;
    }
    true
}

/// Completions quirk controls stop forwarding and reasoning_effort.
/// Re-export so call sites can use `translate::CompletionsQuirk`.
pub use crate::oauth::registry::CompletionsQuirk;

/// Legacy name used in older call sites / tests.
pub type BackendFamily = CompletionsQuirk;

/// Public: Anthropic Messages → OpenAI `messages` array (no sampling fields).
/// Vision-preserving — llama-server / KV path. Chat Completions uses
/// `anthropic_to_openai`, which can strip images for text-only models.
pub fn openai_messages(a: &Value) -> Vec<Value> {
    convert_messages(a, false, true)
}

/// Tools for llama-server `/apply-template`. Anthropic `input_schema` tools are
/// converted; already-OpenAI `function` tools pass through. Empty → None.
pub fn tools_for_apply_template(a: &Value) -> Option<Value> {
    let tools = a.get("tools")?;
    let arr = tools.as_array()?;
    if arr.is_empty() {
        return None;
    }
    if arr.iter().any(|t| {
        t.get("type").and_then(|x| x.as_str()) == Some("function") || t.get("function").is_some()
    }) {
        return Some(tools.clone());
    }
    let mut obj = Map::new();
    apply_tools_and_choice(a, &mut obj);
    obj.get("tools").cloned()
}

fn convert_messages(a: &Value, text_only: bool, fold_system: bool) -> Vec<Value> {
    let mut msgs: Vec<Value> = Vec::new();
    let mut head_system = false;

    if let Some(sys) = a.get("system") {
        let text = blocks_text(sys);
        if !text.is_empty() {
            msgs.push(json!({"role": "system", "content": text}));
            head_system = true;
        }
    }

    let Some(arr) = a.get("messages").and_then(|v| v.as_array()) else {
        return msgs;
    };

    for m in arr {
        let mut role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        // Claude Code delivers some system-reminders as role:"system" messages
        // mid-conversation. Template-strict upstreams (Qwen3.5+ jinja on
        // SGLang/vLLM/llama.cpp) hard-400 with "System message must be at the
        // beginning" unless system is at index 0 — re-embed those reminders as
        // user turns in the client's own <system-reminder> idiom. Tolerant
        // upstreams (xAI/Kimi) keep the passthrough role.
        let mut sys_wrap = false;
        if fold_system && role == "system" {
            if head_system {
                role = "user";
                sys_wrap = true;
            } else {
                head_system = true;
            }
        }
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        if content.is_string() {
            let text = content.as_str().unwrap_or("");
            let text = if sys_wrap {
                reminder_wrap(text)
            } else {
                text.to_string()
            };
            msgs.push(json!({"role": role, "content": text}));
            continue;
        }
        let mut texts = Vec::new();
        let mut images = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        let mut reasoning = Vec::new();

        if let Some(blocks) = content.as_array() {
            for b in blocks {
                let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            texts.push(t.to_string());
                        }
                    }
                    "image" => {
                        if text_only {
                            texts.push(image_omitted_note(1));
                        } else if let Some(part) = image_block_to_openai(b) {
                            images.push(part);
                        }
                    }
                    "tool_use" => {
                        let id = b
                            .get("id")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| short_id("call_", 12));
                        let name = b.get("name").and_then(|t| t.as_str()).unwrap_or("");
                        let input = b.get("input").cloned().unwrap_or(json!({}));
                        let args = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": args}
                        }));
                    }
                    "tool_result" => {
                        // Claude Code Read on a .png/.jpg path returns image blocks inside
                        // tool_result.content. blocks_text() only keeps text → vision was
                        // silently dropped and models hallucinated screenshot contents.
                        let empty = Value::String(String::new());
                        let content = b.get("content").unwrap_or(&empty);
                        let is_error = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                        tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": b.get("tool_use_id").and_then(|t| t.as_str()).unwrap_or(""),
                            "content": tool_result_openai_content(content, is_error, text_only)
                        }));
                    }
                    "thinking" => {
                        if let Some(th) = b.get("thinking").and_then(|t| t.as_str()) {
                            if !th.is_empty() {
                                reasoning.push(th.to_string());
                            }
                        }
                    }
                    // Anthropic server-tool history. xAI/OpenAI-compat 400s on these
                    // block types ("Unsupported content type: server_tool_use"). Flatten
                    // the review into assistant text so the next turn still sees it.
                    "server_tool_use" => {
                        let name = b
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("server_tool");
                        texts.push(format!("[{name}]"));
                    }
                    "advisor_tool_result" => {
                        if let Some(t) = advisor_result_text(b) {
                            texts.push(format!("Advisor review:\n{t}"));
                        }
                    }
                    "web_search_tool_result" => {
                        if let Some(t) = web_search_result_text(b) {
                            texts.push(format!("Web search results:\n{t}"));
                        }
                    }
                    _ => {}
                }
            }
        }

        msgs.extend(tool_results);
        let text = texts.join("\n");
        if role == "assistant" {
            if !text.is_empty() || !tool_calls.is_empty() || !reasoning.is_empty() {
                let mut msg = Map::new();
                msg.insert("role".into(), json!("assistant"));
                msg.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    },
                );
                if !tool_calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                if !reasoning.is_empty() {
                    msg.insert("reasoning_content".into(), json!(reasoning.join("\n")));
                }
                msgs.push(Value::Object(msg));
            }
        } else if !images.is_empty() {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(json!({"type": "text", "text": text}));
            }
            parts.extend(images);
            msgs.push(json!({"role": role, "content": parts}));
        } else if !text.is_empty() {
            let text = if sys_wrap { reminder_wrap(&text) } else { text };
            msgs.push(json!({"role": role, "content": text}));
        }
    }
    msgs
}

/// Idempotent wrap matching Claude Code's own user-embedded reminder format.
fn reminder_wrap(t: &str) -> String {
    if t.starts_with("<system-reminder>") {
        t.to_string()
    } else {
        format!("<system-reminder>\n{t}\n</system-reminder>")
    }
}

fn apply_tools_and_choice(a: &Value, obj: &mut Map<String, Value>) {
    let mut oai_tools = Vec::new();
    if let Some(tools) = a.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            // Server tools (advisor_*, web_search_*, …) have no input_schema — strip for
            // OpenAI-compat upstreams. Phase-2 Spock handlers intercept those separately.
            if t.get("input_schema").is_none() {
                continue;
            }
            oai_tools.push(json!({
                "type": "function",
                "function": {
                    "name": t.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                    "description": t.get("description").and_then(|n| n.as_str()).unwrap_or(""),
                    "parameters": t.get("input_schema").cloned().unwrap_or(json!({"type":"object"}))
                }
            }));
        }
        if !oai_tools.is_empty() {
            obj.insert("tools".into(), Value::Array(oai_tools.clone()));
        }
    }

    // Never send tool_choice without tools — upstreams (Grok/Ollama) 400 with
    // "tool_choice set but no tools specified" (WebSearch nested call after strip).
    if oai_tools.is_empty() {
        return;
    }

    if let Some(tc) = a.get("tool_choice").and_then(|v| v.as_object()) {
        match tc.get("type").and_then(|t| t.as_str()) {
            Some("tool") => {
                let name = tc.get("name").and_then(|n| n.as_str()).unwrap_or("");
                // If forced tool was stripped (server tool), drop tool_choice entirely.
                if name.is_empty()
                    || !oai_tools
                        .iter()
                        .any(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some(name))
                {
                    return;
                }
                obj.insert(
                    "tool_choice".into(),
                    json!({
                        "type": "function",
                        "function": {"name": name}
                    }),
                );
            }
            Some("any") => {
                obj.insert("tool_choice".into(), json!("required"));
            }
            Some("none") => {
                obj.insert("tool_choice".into(), json!("none"));
            }
            Some("auto") => {
                obj.insert("tool_choice".into(), json!("auto"));
            }
            _ => {}
        }
    }
}

/// Anthropic Messages-only keys that must never reach OpenAI-compat / xAI chat.
/// `anthropic_to_openai` rebuilds the body, but server-tool loops and future
/// paths may clone the client request — strip defensively.
const ANTHROPIC_ONLY_KEYS: &[&str] = &[
    "betas",
    "context_management",
    "container",
    "mcp_servers",
    "anthropic_beta",
    "anthropic_version",
    "metadata", // Anthropic request metadata; not OpenAI chat
];

/// Drop Anthropic-only top-level keys. Safe on any Messages-shaped body.
pub fn strip_anthropic_only_fields(a: &mut Value) {
    if let Some(obj) = a.as_object_mut() {
        for k in ANTHROPIC_ONLY_KEYS {
            obj.remove(*k);
        }
    }
}

/// Apply Claude Code `context_management` edits client-side so long sessions
/// stay within OpenAI-compat context windows. Anthropic applies these on the
/// server; Grok/Ollama never see the field, so Spock must do the work.
///
/// Supported strategies (from Claude Code `apiMicrocompact.ts`):
/// - `clear_thinking_20251015` — drop older thinking blocks
/// - `clear_tool_uses_20250919` — blank older tool_result (and optionally tool_use input)
pub fn apply_context_management(a: &mut Value) {
    let edits = match a
        .get("context_management")
        .and_then(|c| c.get("edits"))
        .and_then(|e| e.as_array())
        .cloned()
    {
        Some(e) if !e.is_empty() => e,
        _ => return,
    };

    for edit in &edits {
        let kind = edit.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "clear_thinking_20251015" => apply_clear_thinking(a, edit),
            "clear_tool_uses_20250919" => apply_clear_tool_uses(a, edit),
            _ => {}
        }
    }
}

fn apply_clear_thinking(a: &mut Value, edit: &Value) {
    // keep: "all" | { type: "thinking_turns", value: N }
    let keep_all = edit.get("keep").and_then(|k| k.as_str()) == Some("all");
    if keep_all {
        return;
    }
    let keep_n = edit
        .pointer("/keep/value")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as usize;
    let keep_n = keep_n.max(1);

    let Some(messages) = a.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };

    // Collect indices of assistant messages that have thinking blocks (newest last).
    let mut thinking_turns: Vec<usize> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
            if arr.iter().any(|b| {
                matches!(
                    b.get("type").and_then(|t| t.as_str()),
                    Some("thinking") | Some("redacted_thinking")
                )
            }) {
                thinking_turns.push(i);
            }
        }
    }
    if thinking_turns.len() <= keep_n {
        return;
    }
    let drop_count = thinking_turns.len() - keep_n;
    let drop_idxs: std::collections::HashSet<usize> =
        thinking_turns.into_iter().take(drop_count).collect();

    for (i, msg) in messages.iter_mut().enumerate() {
        if !drop_idxs.contains(&i) {
            continue;
        }
        if let Some(arr) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
            arr.retain(|b| {
                !matches!(
                    b.get("type").and_then(|t| t.as_str()),
                    Some("thinking") | Some("redacted_thinking")
                )
            });
        }
    }
}

fn apply_clear_tool_uses(a: &mut Value, edit: &Value) {
    // Optional trigger: only run when estimated input tokens >= value.
    if let Some(trigger) = edit.pointer("/trigger/value").and_then(|v| v.as_u64()) {
        let est = count_tokens_estimate(a);
        if est < trigger {
            return;
        }
    }

    let clear_inputs = edit.get("clear_tool_inputs");
    let clear_all_inputs = clear_inputs.and_then(|v| v.as_bool()) == Some(true);
    let clear_input_tools: Option<Vec<String>> = clear_inputs.and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
    });

    let exclude: std::collections::HashSet<String> = edit
        .get("exclude_tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Keep last N tool_uses (by encounter order). Default: keep last 5.
    let keep_n = edit
        .pointer("/keep/value")
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as usize;
    let keep_n = keep_n.max(1);

    // Optional clear_at_least: if set, keep clearing until est drops by that much
    // (we approximate by clearing all but keep_n when over trigger — already gated).

    let Some(messages) = a.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };

    // Map tool_use_id → tool name; collect tool_use ids in order.
    let mut use_order: Vec<(String, String)> = Vec::new(); // (id, name)
    for msg in messages.iter() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
            for b in arr {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let id = b
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = b
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !id.is_empty() {
                        use_order.push((id, name));
                    }
                }
            }
        }
    }
    if use_order.len() <= keep_n {
        return;
    }

    let drop_ids: std::collections::HashSet<String> = use_order
        .iter()
        .take(use_order.len() - keep_n)
        .filter(|(_, name)| !exclude.contains(name))
        .map(|(id, _)| id.clone())
        .collect();
    if drop_ids.is_empty() {
        return;
    }

    // Blank tool_result content for dropped ids; optionally blank tool_use inputs.
    for msg in messages.iter_mut() {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let Some(arr) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for b in arr.iter_mut() {
            let ty = b
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if ty == "tool_result" {
                let id = b
                    .get("tool_use_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                if drop_ids.contains(&id) {
                    if let Some(obj) = b.as_object_mut() {
                        obj.insert(
                            "content".into(),
                            json!("[tool result cleared by Spock microcompact]"),
                        );
                    }
                }
            } else if ty == "tool_use" && role == "assistant" {
                let id = b
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = b
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                if !drop_ids.contains(&id) {
                    continue;
                }
                let should_clear = clear_all_inputs
                    || clear_input_tools
                        .as_ref()
                        .map(|list| list.iter().any(|n| n == &name))
                        .unwrap_or(false);
                if should_clear {
                    if let Some(obj) = b.as_object_mut() {
                        obj.insert("input".into(), json!({}));
                    }
                }
            }
        }
    }
}

/// Prepare a Messages request for non-Anthropic upstreams: apply microcompact,
/// then strip Anthropic-only fields. Call before `anthropic_to_openai`.
pub fn prepare_for_openai_compat(a: &mut Value) {
    apply_context_management(a);
    strip_anthropic_only_fields(a);
}

pub fn anthropic_to_openai(
    a: &Value,
    upstream_model: &str,
    family: BackendFamily,
    default_model_for_reasoning: &str,
) -> Value {
    let msgs = convert_messages(
        a,
        is_text_only_model(upstream_model),
        family == CompletionsQuirk::Generic,
    );
    let max_tokens = a.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(1024);

    let mut req = json!({
        "model": upstream_model,
        "messages": msgs,
        "max_tokens": max_tokens,
    });

    let reasoning = is_reasoning_model(upstream_model, default_model_for_reasoning);

    if let Some(obj) = req.as_object_mut() {
        if let Some(t) = a.get("temperature") {
            obj.insert("temperature".into(), t.clone());
        }
        if let Some(t) = a.get("top_p") {
            obj.insert("top_p".into(), t.clone());
        }

        // stop_sequences: drop for xAI reasoning; keep for generic/Kimi
        if let Some(stops) = a.get("stop_sequences").and_then(|v| v.as_array()) {
            if !stops.is_empty() {
                let keep = match family {
                    CompletionsQuirk::Generic | CompletionsQuirk::Kimi => true,
                    CompletionsQuirk::Xai => !reasoning,
                };
                if keep {
                    obj.insert("stop".into(), Value::Array(stops.clone()));
                }
            }
        }

        apply_tools_and_choice(a, obj);

        // reasoning_effort: xAI + Kimi + Generic (LAN llama.cpp / DeepSeek / Ollama
        // chat-completions). Kimi clamps xhigh/max → high. "none" stripped below.
        if matches!(
            family,
            CompletionsQuirk::Xai | CompletionsQuirk::Kimi | CompletionsQuirk::Generic
        ) {
            if let Some(effort) = thinking_effort(a) {
                let effort = if family == CompletionsQuirk::Kimi {
                    clamp_kimi_effort(&effort)
                } else {
                    effort
                };
                obj.insert("reasoning_effort".into(), json!(effort));
            }
        }
    }

    if family == CompletionsQuirk::Xai {
        sanitize_upstream(&mut req, reasoning);
    } else if matches!(family, CompletionsQuirk::Kimi | CompletionsQuirk::Generic) {
        // Drop effort "none" if present; keep stop for generic-like behavior.
        if let Some(obj) = req.as_object_mut() {
            if obj.get("reasoning_effort").and_then(|v| v.as_str()) == Some("none") {
                obj.remove("reasoning_effort");
            }
        }
    }

    req
}

fn clamp_kimi_effort(effort: &str) -> String {
    match effort {
        "xhigh" | "max" => "high".into(),
        other => other.to_string(),
    }
}

pub fn openai_to_anthropic(o: &Value, req_model: &str, include_thinking: bool) -> Value {
    let choice = o
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut content = Vec::new();

    if include_thinking {
        if let Some(r) = msg.get("reasoning_content").and_then(|t| t.as_str()) {
            if !r.is_empty() {
                content.push(json!({"type": "thinking", "thinking": r}));
            }
        }
    }
    if let Some(t) = msg.get("content").and_then(|t| t.as_str()) {
        if !t.is_empty() {
            content.push(json!({"type": "text", "text": t}));
        }
    }
    if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let fn_ = tc.get("function").cloned().unwrap_or(json!({}));
            let args_str = fn_
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let args: Value =
                serde_json::from_str(args_str).unwrap_or_else(|_| json!({"_raw": args_str}));
            let id = tc
                .get("id")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| short_id("toolu_", 12));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": fn_.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                "input": args
            }));
        }
    }

    let usage = o.get("usage").cloned().unwrap_or(json!({}));
    let details = usage
        .get("completion_tokens_details")
        .cloned()
        .unwrap_or(json!({}));
    let mut out_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if out_tokens == 0 {
        if let Some(r) = details.get("reasoning_tokens").and_then(|v| v.as_u64()) {
            out_tokens = r;
        }
    }

    let finish = choice.get("finish_reason").and_then(|f| f.as_str());
    let id = o
        .get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| short_id("msg_", 16));

    let mut usage_out = json!({
        "input_tokens": usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        "output_tokens": out_tokens
    });
    if let Some(cached) = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
    {
        usage_out["cache_read_input_tokens"] = json!(cached);
    }

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": req_model,
        "content": content,
        "stop_reason": stop_reason(finish),
        "stop_sequence": null,
        "usage": usage_out
    })
}

pub fn count_tokens_estimate(body: &Value) -> u64 {
    let messages = body.get("messages").cloned().unwrap_or(json!([]));
    let system = body
        .get("system")
        .map(|s| s.to_string())
        .unwrap_or_default();
    let blob = format!(
        "{}{}",
        serde_json::to_string(&messages).unwrap_or_default(),
        system
    );
    (blob.len() / 4) as u64
}

pub fn new_msg_id() -> String {
    short_id("msg_", 16)
}

pub fn new_tool_id() -> String {
    short_id("toolu_", 12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_max_to_xhigh() {
        assert_eq!(map_output_effort("max"), Some("xhigh"));
        assert_eq!(map_output_effort("low"), Some("low"));
    }

    #[test]
    fn thinking_disabled_omits_effort() {
        // Auto Mode classifier: thinking disabled must NOT map to "none".
        let a = json!({"thinking": {"type": "disabled"}, "output_config": {"effort": "high"}});
        assert_eq!(thinking_effort(&a), None);

        let o = anthropic_to_openai(
            &json!({
                "max_tokens": 64,
                "thinking": {"type": "disabled"},
                "messages": [{"role":"user","content":"hi"}],
                "tools": [{
                    "name": "classify_result",
                    "description": "c",
                    "input_schema": {"type":"object"}
                }],
                "tool_choice": {"type":"tool","name":"classify_result"}
            }),
            "grok-4.5",
            CompletionsQuirk::Xai,
            "grok-4.5",
        );
        assert!(
            o.get("reasoning_effort").is_none(),
            "got reasoning_effort={:?}",
            o.get("reasoning_effort")
        );
    }

    #[test]
    fn wants_thinking_stop_sequences() {
        assert!(!wants_thinking(&json!({"stop_sequences": ["</block>"]})));
        assert!(wants_thinking(&json!({})));
        assert!(wants_thinking(&json!({"thinking": {"type": "enabled"}})));
    }

    #[test]
    fn tool_schema_gate() {
        let a = json!({
            "max_tokens": 100,
            "messages": [{"role":"user","content":"hi"}],
            "tools": [
                {"name": "a", "description": "x"},
                {"name": "b", "description": "y", "input_schema": {"type":"object"}}
            ]
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        let tools = o["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "b");
    }

    #[test]
    fn tool_choice_dropped_when_all_tools_stripped() {
        // WebSearch-style: only server tool left → no tools, must not send tool_choice.
        let a = json!({
            "max_tokens": 100,
            "messages": [{"role":"user","content":"search"}],
            "tools": [{
                "type": "web_search_20250305",
                "name": "web_search"
            }],
            "tool_choice": {"type": "tool", "name": "web_search"}
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        assert!(o.get("tools").is_none(), "tools={:?}", o.get("tools"));
        assert!(
            o.get("tool_choice").is_none(),
            "tool_choice={:?}",
            o.get("tool_choice")
        );
    }

    #[test]
    fn tool_choice_kept_when_function_tools_present() {
        let a = json!({
            "max_tokens": 100,
            "messages": [{"role":"user","content":"hi"}],
            "tools": [{
                "name": "Bash",
                "description": "run",
                "input_schema": {"type":"object","properties":{}}
            }],
            "tool_choice": {"type": "auto"}
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        assert_eq!(o["tool_choice"], json!("auto"));
        assert_eq!(o["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn forced_tool_choice_requires_matching_function() {
        // Forced tool that was stripped must not leave tool_choice pointing at missing name.
        let a = json!({
            "max_tokens": 50,
            "messages": [{"role":"user","content":"x"}],
            "tools": [
                {"type":"advisor_20260301","name":"advisor","model":"fable"},
                {"name":"Bash","description":"sh","input_schema":{"type":"object"}}
            ],
            "tool_choice": {"type":"tool","name":"advisor"}
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        assert!(o.get("tools").is_some());
        // advisor stripped; Bash remains; forced advisor name missing → tool_choice dropped
        assert!(o.get("tool_choice").is_none(), "{:?}", o.get("tool_choice"));
    }

    #[test]
    fn generic_family_forwards_reasoning_effort() {
        // LAN DeepSeek / llama.cpp chat-completions — Grok sets effort, Spock must forward.
        let a = json!({
            "max_tokens": 10,
            "thinking": {"type":"enabled","budget_tokens":8000},
            "messages": [{"role":"user","content":"hi"}]
        });
        let o = anthropic_to_openai(&a, "deepseek-v4", CompletionsQuirk::Generic, "deepseek-v4");
        assert_eq!(
            o.get("reasoning_effort").and_then(|v| v.as_str()),
            Some("medium")
        );
    }

    #[test]
    fn generic_family_omits_disabled_thinking() {
        let a = json!({
            "max_tokens": 10,
            "thinking": {"type":"disabled"},
            "messages": [{"role":"user","content":"hi"}]
        });
        let o = anthropic_to_openai(&a, "deepseek-v4", CompletionsQuirk::Generic, "deepseek-v4");
        assert!(o.get("reasoning_effort").is_none());
    }

    #[test]
    fn tool_use_roundtrip_shape() {
        let a = json!({
            "max_tokens": 100,
            "messages": [{
                "role":"assistant",
                "content": [{
                    "type":"tool_use",
                    "id":"toolu_1",
                    "name":"Bash",
                    "input":{"command":"echo hi"}
                }]
            },{
                "role":"user",
                "content": [{
                    "type":"tool_result",
                    "tool_use_id":"toolu_1",
                    "content":"hi"
                }]
            }]
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        let msgs = o["messages"].as_array().unwrap();
        assert!(msgs.iter().any(|m| m.get("tool_calls").is_some()));
        assert!(msgs.iter().any(|m| m.get("role") == Some(&json!("tool"))));
    }

    #[test]
    fn count_tokens_nonzero() {
        let a = json!({"messages":[{"role":"user","content":"hello world from spock"}]});
        assert!(count_tokens_estimate(&a) > 0);
    }

    #[test]
    fn stop_dropped_on_reasoning_xai() {
        let a = json!({
            "max_tokens": 100,
            "messages": [{"role":"user","content":"hi"}],
            "stop_sequences": ["</block>"]
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        assert!(o.get("stop").is_none());
        // Family heuristic — 4.6 must not re-open the stop 400.
        let o46 = anthropic_to_openai(&a, "grok-4.6", CompletionsQuirk::Xai, "grok-4.5");
        assert!(o46.get("stop").is_none());
    }

    #[test]
    fn stop_kept_on_openai_compat() {
        let a = json!({
            "max_tokens": 100,
            "messages": [{"role":"user","content":"hi"}],
            "stop_sequences": ["</block>"]
        });
        let o = anthropic_to_openai(&a, "qwen2.5:14b", CompletionsQuirk::Generic, "grok-4.5");
        assert!(o.get("stop").is_some());
    }

    #[test]
    fn o2a_order_thinking_text() {
        let o = json!({
            "id": "x",
            "choices": [{
                "finish_reason": "stop",
                "message": {"reasoning_content": "think", "content": "hello"}
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        });
        let a = openai_to_anthropic(&o, "claude-opus-4-8", true);
        let c = a["content"].as_array().unwrap();
        assert_eq!(c[0]["type"], "thinking");
        assert_eq!(c[1]["type"], "text");
    }

    #[test]
    fn strip_anthropic_only_fields_drops_betas() {
        let mut a = json!({
            "model": "x",
            "betas": ["advisor-tool-2026", "context-management-2025"],
            "context_management": {"edits": []},
            "metadata": {"user_id": "u"},
            "messages": [{"role":"user","content":"hi"}]
        });
        strip_anthropic_only_fields(&mut a);
        assert!(a.get("betas").is_none());
        assert!(a.get("context_management").is_none());
        assert!(a.get("metadata").is_none());
        assert!(a.get("messages").is_some());
    }

    #[test]
    fn clear_thinking_keeps_last_n() {
        let mut a = json!({
            "messages": [
                {"role":"assistant","content":[{"type":"thinking","thinking":"t1"},{"type":"text","text":"a1"}]},
                {"role":"user","content":"u2"},
                {"role":"assistant","content":[{"type":"thinking","thinking":"t2"},{"type":"text","text":"a2"}]},
                {"role":"user","content":"u3"},
                {"role":"assistant","content":[{"type":"thinking","thinking":"t3"},{"type":"text","text":"a3"}]},
            ],
            "context_management": {
                "edits": [{
                    "type": "clear_thinking_20251015",
                    "keep": {"type": "thinking_turns", "value": 1}
                }]
            }
        });
        apply_context_management(&mut a);
        let msgs = a["messages"].as_array().unwrap();
        // first two thinking turns dropped; last kept
        let c0 = msgs[0]["content"].as_array().unwrap();
        assert!(c0.iter().all(|b| b["type"] != "thinking"), "{c0:?}");
        let c2 = msgs[2]["content"].as_array().unwrap();
        assert!(c2.iter().all(|b| b["type"] != "thinking"), "{c2:?}");
        let c4 = msgs[4]["content"].as_array().unwrap();
        assert!(c4.iter().any(|b| b["type"] == "thinking"), "{c4:?}");
    }

    #[test]
    fn clear_tool_uses_blanks_old_results() {
        let mut a = json!({
            "messages": [
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t1","content":"file1\nfile2\n".repeat(50)}
                ]},
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"pwd"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t2","content":"/tmp"}
                ]},
            ],
            "context_management": {
                "edits": [{
                    "type": "clear_tool_uses_20250919",
                    "keep": {"type": "tool_uses", "value": 1},
                    "clear_tool_inputs": true
                }]
            }
        });
        apply_context_management(&mut a);
        let msgs = a["messages"].as_array().unwrap();
        let r1 = &msgs[1]["content"][0];
        assert!(
            r1["content"].as_str().unwrap().contains("cleared"),
            "{:?}",
            r1["content"]
        );
        // kept tool_result intact
        assert_eq!(msgs[3]["content"][0]["content"], "/tmp");
        // cleared input on dropped tool_use
        assert_eq!(msgs[0]["content"][0]["input"], json!({}));
    }

    #[test]
    fn tools_for_apply_template_converts_anthropic() {
        let a = json!({
            "tools": [{
                "name": "Bash",
                "description": "run",
                "input_schema": {"type":"object","properties":{"command":{"type":"string"}}}
            }]
        });
        let t = tools_for_apply_template(&a).expect("tools");
        assert_eq!(t[0]["type"], "function");
        assert_eq!(t[0]["function"]["name"], "Bash");
    }

    #[test]
    fn tools_for_apply_template_keeps_openai() {
        let a = json!({
            "tools": [{
                "type": "function",
                "function": {"name":"Bash","parameters":{"type":"object"}}
            }]
        });
        let t = tools_for_apply_template(&a).expect("tools");
        assert_eq!(t[0]["function"]["name"], "Bash");
    }

    #[test]
    fn prepare_for_openai_compat_strips_after_microcompact() {
        let mut a = json!({
            "betas": ["x"],
            "context_management": {
                "edits": [{
                    "type": "clear_thinking_20251015",
                    "keep": {"type": "thinking_turns", "value": 1}
                }]
            },
            "messages": [
                {"role":"assistant","content":[{"type":"thinking","thinking":"old"},{"type":"text","text":"a"}]},
                {"role":"assistant","content":[{"type":"thinking","thinking":"new"},{"type":"text","text":"b"}]},
            ]
        });
        prepare_for_openai_compat(&mut a);
        assert!(a.get("betas").is_none());
        assert!(a.get("context_management").is_none());
        let msgs = a["messages"].as_array().unwrap();
        assert!(msgs[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["type"] != "thinking"));
    }

    #[test]
    fn user_message_image_becomes_openai_image_url() {
        let a = json!({
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what color?"},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "AAA"
                    }}
                ]
            }]
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        let content = o["messages"][0]["content"].as_array().expect("multipart");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"].as_str().unwrap(),
            "data:image/png;base64,AAA"
        );
    }

    #[test]
    fn tool_result_image_preserved_as_multipart() {
        // Path-pasted screenshots in Claude Code: Read → tool_result with image block.
        // Pre-fix Spock text-stripped these → model never saw pixels.
        let a = json!({
            "max_tokens": 32,
            "messages": [
                {"role": "user", "content": "look at file1.png"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read",
                     "input": {"file_path": "/tmp/file1.png"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "text", "text": "file1.png"},
                        {"type": "image", "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "iVBORw0KGgo="
                        }}
                    ]}
                ]}
            ]
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        let msgs = o["messages"].as_array().unwrap();
        // system none; user; assistant+tool_calls; tool
        let tool = msgs
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool message");
        assert_eq!(tool["tool_call_id"], "toolu_1");
        let parts = tool["content"].as_array().expect("multipart tool content");
        assert!(
            parts.iter().any(|p| p["type"] == "text"),
            "expected text part: {parts:?}"
        );
        let img = parts
            .iter()
            .find(|p| p["type"] == "image_url")
            .expect("image_url part");
        assert!(
            img["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"),
            "{img:?}"
        );
    }

    #[test]
    fn tool_result_text_only_stays_string() {
        let a = json!({
            "max_tokens": 32,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            ]
        });
        let o = anthropic_to_openai(&a, "grok-4.5", CompletionsQuirk::Xai, "grok-4.5");
        let tool = o["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap();
        assert_eq!(tool["content"], "ok");
    }

    #[test]
    fn server_tool_history_flattens_for_openai() {
        // Claude Code echoes advisor/web_search blocks on the next turn. xAI 400s
        // "Unsupported content type: server_tool_use" if we forward them raw.
        let a = json!({
            "max_tokens": 32,
            "messages": [{
                "role": "assistant",
                "content": [
                    {
                        "type": "server_tool_use",
                        "id": "srv_1",
                        "name": "advisor",
                        "input": {}
                    },
                    {
                        "type": "advisor_tool_result",
                        "tool_use_id": "srv_1",
                        "content": {"type": "advisor_result", "text": "approve: stay the course"}
                    },
                    {"type": "text", "text": "got it"}
                ]
            }]
        });
        let o = anthropic_to_openai(&a, "grok-4.6", CompletionsQuirk::Xai, "grok-4.6");
        let content = o["messages"][0]["content"]
            .as_str()
            .expect("string content");
        assert!(content.contains("[advisor]"), "{content}");
        assert!(content.contains("Advisor review:"), "{content}");
        assert!(content.contains("approve: stay the course"), "{content}");
        assert!(content.contains("got it"), "{content}");
        let blob = serde_json::to_string(&o).unwrap();
        assert!(!blob.contains("server_tool_use"), "{blob}");
        assert!(!blob.contains("advisor_tool_result"), "{blob}");
    }

    #[test]
    fn web_search_history_flattens_for_openai() {
        let a = json!({
            "max_tokens": 32,
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type":"server_tool_use","id":"w1","name":"web_search","input":{"query":"rust"}},
                    {"type":"web_search_tool_result","tool_use_id":"w1","content":[
                        {"type":"web_search_result","title":"Rust","url":"https://rust-lang.org/"}
                    ]}
                ]
            }]
        });
        let o = anthropic_to_openai(&a, "grok-4.6", CompletionsQuirk::Xai, "grok-4.6");
        let content = o["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("[web_search]"), "{content}");
        assert!(content.contains("https://rust-lang.org/"), "{content}");
        let blob = serde_json::to_string(&o).unwrap();
        assert!(!blob.contains("web_search_tool_result"), "{blob}");
    }

    #[test]
    fn image_url_source_type() {
        let a = json!({
            "max_tokens": 8,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": {"type": "url", "url": "https://example.com/a.png"}
                }]
            }]
        });
        let o = anthropic_to_openai(&a, "m", CompletionsQuirk::Generic, "m");
        let content = o["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["image_url"]["url"], "https://example.com/a.png");
    }

    fn glm53_image_body() -> Value {
        json!({
            "max_tokens": 32,
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "what color?"},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "AAA"
                    }}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read",
                     "input": {"file_path": "/tmp/file1.png"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "text", "text": "file1.png"},
                        {"type": "image", "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "iVBORw0KGgo="
                        }}
                    ]}
                ]}
            ]
        })
    }

    #[test]
    fn glm53_flattens_images_to_text() {
        let o = anthropic_to_openai(
            &glm53_image_body(),
            "glm-5.3",
            CompletionsQuirk::Generic,
            "glm-5.3",
        );
        let blob = serde_json::to_string(&o).unwrap();
        assert!(!blob.contains("image_url"), "{blob}");
        let msgs = o["messages"].as_array().unwrap();
        let user = msgs.iter().find(|m| m["role"] == "user").unwrap();
        let user_text = user["content"].as_str().expect("user content string");
        assert!(user_text.contains("what color?"), "{user_text}");
        assert!(user_text.contains("text-only"), "{user_text}");
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        let tool_text = tool["content"].as_str().expect("tool content string");
        assert!(tool_text.contains("file1.png"), "{tool_text}");
        assert!(tool_text.contains("text-only"), "{tool_text}");
    }

    #[test]
    fn glm53_point_release_also_text_only() {
        let o = anthropic_to_openai(
            &glm53_image_body(),
            "GLM-5.3-preview",
            CompletionsQuirk::Generic,
            "m",
        );
        let blob = serde_json::to_string(&o).unwrap();
        assert!(!blob.contains("image_url"), "{blob}");
    }

    #[test]
    fn glm52_keeps_vision_parts() {
        let o = anthropic_to_openai(
            &glm53_image_body(),
            "glm-5.2",
            CompletionsQuirk::Generic,
            "m",
        );
        let blob = serde_json::to_string(&o).unwrap();
        assert!(blob.contains("image_url"), "{blob}");
    }

    /// Claude Code sends some system-reminders as role:"system" messages
    /// mid-conversation. Qwen3.5+ jinja templates (SGLang/vLLM/llama.cpp)
    /// hard-400 "System message must be at the beginning" on those.
    fn mid_system_body() -> Value {
        json!({
            "max_tokens": 32,
            "system": [{"type": "text", "text": "top-level system"}],
            "messages": [
                {"role": "user", "content": "say hi"},
                {"role": "system", "content": "Available agent types: claude, Explore"}
            ]
        })
    }

    #[test]
    fn generic_folds_mid_system_to_user() {
        let o = anthropic_to_openai(
            &mid_system_body(),
            "qwen3.8-27b",
            CompletionsQuirk::Generic,
            "m",
        );
        let msgs = o["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert!(msgs[0]["content"]
            .as_str()
            .unwrap()
            .contains("top-level system"));
        // No second system message anywhere after index 0.
        assert!(msgs[1..].iter().all(|m| m["role"] != "system"), "{msgs:?}");
        let folded = msgs[2]["content"].as_str().unwrap();
        assert_eq!(msgs[2]["role"], "user");
        assert!(folded.starts_with("<system-reminder>"), "{folded}");
        assert!(folded.contains("Available agent types"), "{folded}");
    }

    #[test]
    fn generic_keeps_leading_system_in_messages() {
        let a = json!({
            "max_tokens": 32,
            "messages": [
                {"role": "system", "content": "lead"},
                {"role": "user", "content": "hi"}
            ]
        });
        let o = anthropic_to_openai(&a, "qwen3.8-27b", CompletionsQuirk::Generic, "m");
        let msgs = o["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "lead");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn xai_keeps_mid_system_passthrough() {
        let o = anthropic_to_openai(
            &mid_system_body(),
            "grok-4.5",
            CompletionsQuirk::Xai,
            "grok-4.5",
        );
        let msgs = o["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "system");
        assert_eq!(msgs[2]["content"], "Available agent types: claude, Explore");
    }

    #[test]
    fn generic_folds_mid_system_block_content() {
        let a = json!({
            "max_tokens": 32,
            "system": "top",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [
                    {"type": "text", "text": "reminder one"},
                    {"type": "text", "text": "reminder two"}
                ]}
            ]
        });
        let o = anthropic_to_openai(&a, "qwen3.8-27b", CompletionsQuirk::Generic, "m");
        let msgs = o["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "user");
        let folded = msgs[2]["content"].as_str().unwrap();
        assert!(folded.starts_with("<system-reminder>"), "{folded}");
        assert!(
            folded.contains("reminder one") && folded.contains("reminder two"),
            "{folded}"
        );
    }

    #[test]
    fn kv_path_openai_messages_folds_mid_system() {
        let msgs = openai_messages(&mid_system_body());
        assert_eq!(msgs[0]["role"], "system");
        assert!(msgs[1..].iter().all(|m| m["role"] != "system"), "{msgs:?}");
        assert_eq!(msgs[2]["role"], "user");
        assert!(
            msgs[2]["content"]
                .as_str()
                .unwrap()
                .starts_with("<system-reminder>"),
            "{:?}",
            msgs[2]
        );
    }
}

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

/// Priority: thinking disabled → none; output_config.effort; budget buckets; enabled → high.
pub fn thinking_effort(a: &Value) -> Option<String> {
    let t = a.get("thinking");
    if let Some(obj) = t.and_then(|v| v.as_object()) {
        let kind = obj.get("type").and_then(|v| v.as_str());
        if kind.is_none() || kind == Some("disabled") {
            return Some("none".into());
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

/// Backend family controls stop forwarding and reasoning_effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendFamily {
    Xai,
    Openai,
}

fn convert_messages(a: &Value) -> Vec<Value> {
    let mut msgs: Vec<Value> = Vec::new();

    if let Some(sys) = a.get("system") {
        let text = blocks_text(sys);
        if !text.is_empty() {
            msgs.push(json!({"role": "system", "content": text}));
        }
    }

    let Some(arr) = a.get("messages").and_then(|v| v.as_array()) else {
        return msgs;
    };

    for m in arr {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        if content.is_string() {
            msgs.push(json!({"role": role, "content": content}));
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
                        let src = b.get("source").cloned().unwrap_or(Value::Null);
                        let url = if src.get("type").and_then(|t| t.as_str()) == Some("base64") {
                            let mt = src
                                .get("media_type")
                                .and_then(|t| t.as_str())
                                .unwrap_or("image/png");
                            let data = src.get("data").and_then(|t| t.as_str()).unwrap_or("");
                            format!("data:{mt};base64,{data}")
                        } else {
                            src.get("url")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string()
                        };
                        images.push(json!({
                            "type": "image_url",
                            "image_url": {"url": url}
                        }));
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
                        let mut text =
                            blocks_text(b.get("content").unwrap_or(&Value::String(String::new())));
                        if b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false) {
                            text = format!("Error: {text}");
                        }
                        tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": b.get("tool_use_id").and_then(|t| t.as_str()).unwrap_or(""),
                            "content": text
                        }));
                    }
                    "thinking" => {
                        if let Some(th) = b.get("thinking").and_then(|t| t.as_str()) {
                            if !th.is_empty() {
                                reasoning.push(th.to_string());
                            }
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
            msgs.push(json!({"role": role, "content": text}));
        }
    }
    msgs
}

fn apply_tools_and_choice(a: &Value, obj: &mut Map<String, Value>) {
    if let Some(tools) = a.get("tools").and_then(|v| v.as_array()) {
        let mut oai_tools = Vec::new();
        for t in tools {
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
            obj.insert("tools".into(), Value::Array(oai_tools));
        }
    }

    if let Some(tc) = a.get("tool_choice").and_then(|v| v.as_object()) {
        match tc.get("type").and_then(|t| t.as_str()) {
            Some("tool") => {
                obj.insert(
                    "tool_choice".into(),
                    json!({
                        "type": "function",
                        "function": {"name": tc.get("name").and_then(|n| n.as_str()).unwrap_or("")}
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

pub fn anthropic_to_openai(
    a: &Value,
    upstream_model: &str,
    family: BackendFamily,
    default_model_for_reasoning: &str,
) -> Value {
    let msgs = convert_messages(a);
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

        // stop_sequences: drop for xAI reasoning; keep for OpenAI-compat
        if let Some(stops) = a.get("stop_sequences").and_then(|v| v.as_array()) {
            if !stops.is_empty() {
                let keep = match family {
                    BackendFamily::Openai => true,
                    BackendFamily::Xai => !reasoning,
                };
                if keep {
                    obj.insert("stop".into(), Value::Array(stops.clone()));
                }
            }
        }

        apply_tools_and_choice(a, obj);

        if family == BackendFamily::Xai {
            if let Some(effort) = thinking_effort(a) {
                obj.insert("reasoning_effort".into(), json!(effort));
            }
        }
    }

    if family == BackendFamily::Xai {
        sanitize_upstream(&mut req, reasoning);
    }

    req
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

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": req_model,
        "content": content,
        "stop_reason": stop_reason(finish),
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            "output_tokens": out_tokens
        }
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
    fn thinking_disabled_none() {
        let a = json!({"thinking": {"type": "disabled"}, "output_config": {"effort": "high"}});
        assert_eq!(thinking_effort(&a).as_deref(), Some("none"));
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
        let o = anthropic_to_openai(&a, "grok-4.5", BackendFamily::Xai, "grok-4.5");
        let tools = o["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "b");
    }

    #[test]
    fn stop_dropped_on_reasoning_xai() {
        let a = json!({
            "max_tokens": 100,
            "messages": [{"role":"user","content":"hi"}],
            "stop_sequences": ["</block>"]
        });
        let o = anthropic_to_openai(&a, "grok-4.5", BackendFamily::Xai, "grok-4.5");
        assert!(o.get("stop").is_none());
    }

    #[test]
    fn stop_kept_on_openai_compat() {
        let a = json!({
            "max_tokens": 100,
            "messages": [{"role":"user","content":"hi"}],
            "stop_sequences": ["</block>"]
        });
        let o = anthropic_to_openai(&a, "qwen2.5:14b", BackendFamily::Openai, "grok-4.5");
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
}

use serde_json::{json, Value};

/// Claude Code context-window hints like [1m] / [500k] — strip before upstream.
pub fn strip_ctx_suffix(model: &str) -> String {
    let m = model.trim();
    if let Some(open) = m.rfind('[') {
        if m.ends_with(']') && open < m.len() - 1 {
            let inner = &m[open + 1..m.len() - 1];
            if !inner.contains(']') && !inner.contains('[') {
                let base = m[..open].trim();
                if !base.is_empty() {
                    return base.to_string();
                }
            }
        }
    }
    m.to_string()
}

/// Role bucket for profile routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Haiku,
    Sonnet,
    Opus,
    Fable,
    Other,
}

pub fn detect_role(model: &str) -> Role {
    let m = strip_ctx_suffix(model).to_lowercase();
    if m.contains("haiku") {
        Role::Haiku
    } else if m.contains("opus") {
        Role::Opus
    } else if m.contains("fable") {
        Role::Fable
    } else if m.contains("sonnet") {
        Role::Sonnet
    } else {
        Role::Other
    }
}

/// Legacy single-backend map (used when profile has no explicit route).
#[allow(dead_code)]
pub fn map_model_legacy(model: Option<&str>, default: &str, small: &str) -> String {
    let base = strip_ctx_suffix(model.unwrap_or(""));
    if base.is_empty() {
        return default.to_string();
    }
    if base.starts_with("grok") {
        return base;
    }
    if base.contains("haiku") {
        return small.to_string();
    }
    default.to_string()
}

pub fn is_reasoning_model(model: &str, default_model: &str) -> bool {
    let m = strip_ctx_suffix(model).to_lowercase();
    if m.is_empty() {
        return false;
    }
    if m == "grok-4.5" || m == "grok-4.3" {
        return true;
    }
    if m.contains("reasoning") || m.contains("multi-agent") {
        return true;
    }
    if m == strip_ctx_suffix(default_model).to_lowercase() {
        return true;
    }
    false
}

/// Drop OpenAI params xAI rejects on reasoning models.
pub fn sanitize_upstream(req: &mut Value, is_reasoning: bool) {
    if !is_reasoning {
        return;
    }
    if let Some(obj) = req.as_object_mut() {
        obj.remove("stop");
        obj.remove("presence_penalty");
        obj.remove("frequency_penalty");
    }
}

pub fn model_card(id: &str, owned_by: &str) -> Value {
    json!({
        "id": id,
        "object": "model",
        "created": 0,
        "owned_by": owned_by,
        "display_name": id,
        "type": "model",
    })
}

pub const CLAUDE_ALIASES: &[&str] = &[
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-sonnet-4-5",
    "claude-haiku-4-5-20251001",
    "claude-haiku-4-5",
    "claude-fable-5",
    "claude-3-5-haiku-20241022",
    "claude-3-5-sonnet-20241022",
    "claude-3-7-sonnet-20250219",
];

pub fn alias_models(default_model: &str, small_model: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for mid in [default_model, small_model] {
        if mid.is_empty() {
            continue;
        }
        for cand in [
            mid.to_string(),
            format!("{mid}[1m]"),
            format!("{mid}[500k]"),
        ] {
            if seen.insert(cand.clone()) {
                out.push(model_card(&cand, "xai"));
            }
        }
    }
    for mid in CLAUDE_ALIASES {
        if seen.insert((*mid).to_string()) {
            out.push(model_card(mid, "spock-alias"));
        }
        let tagged = format!("{mid}[1m]");
        if seen.insert(tagged.clone()) {
            out.push(model_card(&tagged, "spock-alias"));
        }
    }
    out
}

pub fn stop_reason(finish: Option<&str>) -> &'static str {
    match finish {
        Some("stop") => "end_turn",
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        _ => "end_turn",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_suffixes() {
        assert_eq!(strip_ctx_suffix("grok-4.5[1m]"), "grok-4.5");
        assert_eq!(strip_ctx_suffix("grok-4.5[500k]"), "grok-4.5");
        assert_eq!(strip_ctx_suffix("grok-4.5"), "grok-4.5");
        assert_eq!(strip_ctx_suffix("claude-opus-4-8[1m]"), "claude-opus-4-8");
    }

    #[test]
    fn map_legacy() {
        assert_eq!(
            map_model_legacy(Some("grok-4.3"), "grok-4.5", "mini"),
            "grok-4.3"
        );
        assert_eq!(
            map_model_legacy(Some("grok-4.5[1m]"), "grok-4.5", "mini"),
            "grok-4.5"
        );
        assert_eq!(
            map_model_legacy(Some("claude-haiku-4-5"), "grok-4.5", "mini"),
            "mini"
        );
        assert_eq!(
            map_model_legacy(Some("claude-opus-4-8"), "grok-4.5", "mini"),
            "grok-4.5"
        );
    }

    #[test]
    fn reasoning_detect() {
        assert!(is_reasoning_model("grok-4.5", "grok-4.5"));
        assert!(is_reasoning_model("grok-4.3", "grok-4.5"));
        assert!(is_reasoning_model("foo-reasoning", "grok-4.5"));
        assert!(is_reasoning_model("grok-4.5[1m]", "other"));
        assert!(is_reasoning_model("custom", "custom"));
    }

    #[test]
    fn roles() {
        assert_eq!(detect_role("claude-haiku-4-5"), Role::Haiku);
        assert_eq!(detect_role("claude-opus-4-8[1m]"), Role::Opus);
        assert_eq!(detect_role("claude-fable-5"), Role::Fable);
        assert_eq!(detect_role("claude-sonnet-5"), Role::Sonnet);
        assert_eq!(detect_role("grok-4.5"), Role::Other);
    }

    #[test]
    fn aliases_include_opus() {
        let a = alias_models("grok-4.5", "grok-4.5");
        let ids: Vec<_> = a.iter().filter_map(|v| v["id"].as_str()).collect();
        assert!(ids.contains(&"claude-opus-4-8"));
        assert!(ids.contains(&"grok-4.5[1m]"));
    }

    #[test]
    fn sanitize_drops_stop() {
        let mut v = json!({"stop": ["x"], "presence_penalty": 1.0, "model": "m"});
        sanitize_upstream(&mut v, true);
        assert!(v.get("stop").is_none());
        assert!(v.get("presence_penalty").is_none());
        assert!(v.get("model").is_some());
    }
}

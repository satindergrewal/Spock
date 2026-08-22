use crate::config::CatalogEntry;
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

/// Bare id after `[1m]` / `xai:` strip. Grok 4+ reject `stop` (and a few
/// other OpenAI params). Match the family — pinning 4.3/4.5 already 400'd on 4.6.
fn is_xai_reasoning_family(bare: &str) -> bool {
    bare.starts_with("grok-4") || bare.starts_with("grok-5")
}

pub fn is_reasoning_model(model: &str, default_model: &str) -> bool {
    let m = strip_ctx_suffix(model).to_lowercase();
    if m.is_empty() {
        return false;
    }
    let bare = m.strip_prefix("xai:").unwrap_or(m.as_str());
    if is_xai_reasoning_family(bare) {
        return true;
    }
    if bare.contains("reasoning") || bare.contains("multi-agent") {
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
        // Safety net: never send reasoning_effort "none" (xAI 400).
        if obj.get("reasoning_effort").and_then(|v| v.as_str()) == Some("none") {
            obj.remove("reasoning_effort");
        }
    }
}

pub fn model_card(id: &str, owned_by: &str) -> Value {
    model_card_full(id, owned_by, None, None, None, None)
}

/// OpenAI-style model card plus fields Grok Build reads from `/v1/models`
/// (`context_window` / `contextWindow`, `model`, optional `name`,
/// `supportsReasoningEffort`).
pub fn model_card_full(
    id: &str,
    owned_by: &str,
    context_window: Option<u64>,
    name: Option<&str>,
    description: Option<&str>,
    supports_reasoning_effort: Option<bool>,
) -> Value {
    let display = name.unwrap_or(id);
    let mut card = json!({
        "id": id,
        "object": "model",
        "created": 0,
        "owned_by": owned_by,
        "display_name": display,
        "name": display,
        // Grok parse_remote_model_value prefers `model`, falls back to `id`.
        "model": id,
        "type": "model",
    });
    if let Some(obj) = card.as_object_mut() {
        if let Some(cw) = context_window.filter(|c| *c > 0) {
            obj.insert("context_window".into(), json!(cw));
            obj.insert("contextWindow".into(), json!(cw));
        }
        if let Some(d) = description.map(str::trim).filter(|s| !s.is_empty()) {
            obj.insert("description".into(), json!(d));
        }
        // Grok Build gates `/effort` on this flag (top-level or `_meta`).
        // Explicit catalog value wins; else heuristic on id/name.
        let supports = supports_reasoning_effort
            .unwrap_or_else(|| heuristic_supports_reasoning_effort(id, name));
        if supports {
            obj.insert("supportsReasoningEffort".into(), json!(true));
            obj.insert("supports_reasoning_effort".into(), json!(true));
        }
    }
    card
}

/// Cards for a non-empty catalog list. Local only — never probes backends.
/// Empty / duplicate / whitespace ids are dropped. Context comes from the
/// entry; missing windows stay unset (Grok defaults ~200k).
pub fn catalog_list_cards(entries: &[CatalogEntry]) -> Vec<Value> {
    let mut data = Vec::with_capacity(entries.len());
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries {
        let id = entry.id.trim();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        data.push(model_card_full(
            id,
            "spock-catalog",
            entry.context_window.filter(|c| *c > 0),
            entry.name.as_deref(),
            entry.description.as_deref(),
            entry.supports_reasoning_effort,
        ));
    }
    data
}

/// Conservative default when catalog omits `supports_reasoning_effort`.
/// xAI Grok, Kimi, DeepSeek (incl. LAN gguf paths) → true; plain local
/// chat models stay false so Grok doesn't fake a control that does nothing.
pub fn heuristic_supports_reasoning_effort(id: &str, name: Option<&str>) -> bool {
    let blob = format!("{} {}", id, name.unwrap_or("")).to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "grok", "xai:", "kimi", "deepseek", "dsv4", "ds-v4", "r1",
        // common reasoning-model tags
        "reasoner", "thinking",
    ];
    NEEDLES.iter().any(|n| blob.contains(n))
}

/// Pull a context-window hint out of a backend `/models` card if present.
/// List path no longer probes; kept for tests and `GET /v1/models/{id}` fallbacks.
#[allow(dead_code)]
pub fn context_window_from_card(card: &Value) -> Option<u64> {
    let obj = card.as_object()?;
    let keys = [
        "context_window",
        "contextWindow",
        "context_length",
        "contextLength",
        "max_model_len",
        "maxModelLen",
        "max_sequence_length",
    ];
    for k in keys {
        if let Some(n) = obj.get(k).and_then(|v| v.as_u64()).filter(|n| *n > 0) {
            return Some(n);
        }
        if let Some(n) = obj
            .get(k)
            .and_then(|v| v.as_f64())
            .map(|f| f as u64)
            .filter(|n| *n > 0)
        {
            return Some(n);
        }
    }
    // Some providers nest under `meta` / `_meta`.
    for nest in ["meta", "_meta"] {
        if let Some(n) = obj.get(nest).and_then(context_window_from_card) {
            return Some(n);
        }
    }
    None
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
        // Family, not a pinned id — this is the 4.6 stop-400 hole.
        assert!(is_reasoning_model("grok-4.6", "other"));
        assert!(is_reasoning_model("grok-4.6[1m]", "other"));
        assert!(is_reasoning_model("grok-4", "other"));
        assert!(is_reasoning_model("grok-4-fast", "other"));
        assert!(is_reasoning_model("xai:grok-4.6", "other"));
        assert!(is_reasoning_model("grok-5", "other"));
        // Pre-4 stays non-reasoning so `stop` still forwards.
        assert!(!is_reasoning_model("grok-3", "other"));
        assert!(!is_reasoning_model("grok-2", "other"));
    }

    #[test]
    fn sanitize_drops_stop_on_grok_4_6() {
        let mut v = json!({"stop": ["</block>"], "presence_penalty": 1.0, "model": "grok-4.6"});
        sanitize_upstream(&mut v, is_reasoning_model("grok-4.6", "grok-4.5"));
        assert!(v.get("stop").is_none());
        assert!(v.get("presence_penalty").is_none());
        assert_eq!(v["model"], "grok-4.6");
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
    fn catalog_list_is_local_only() {
        let entries = vec![
            CatalogEntry {
                id: "xai:grok-4.5".into(),
                name: Some("Grok 4.5".into()),
                description: None,
                context_window: Some(500_000),
                supports_reasoning_effort: None,
            },
            CatalogEntry {
                id: "  ".into(),
                name: None,
                description: None,
                context_window: None,
                supports_reasoning_effort: None,
            },
            CatalogEntry {
                id: "xai:grok-4.5".into(),
                name: Some("dup".into()),
                description: None,
                context_window: Some(1),
                supports_reasoning_effort: None,
            },
            CatalogEntry {
                id: "ollama:llama3.2".into(),
                name: Some("Llama".into()),
                description: None,
                context_window: Some(128_000),
                supports_reasoning_effort: None,
            },
        ];
        let cards = catalog_list_cards(&entries);
        assert_eq!(cards.len(), 2, "blank + duplicate ids dropped");
        assert_eq!(cards[0]["id"], "xai:grok-4.5");
        assert_eq!(cards[0]["context_window"], 500_000);
        assert_eq!(cards[0]["supportsReasoningEffort"], true);
        assert_eq!(cards[1]["id"], "ollama:llama3.2");
        assert!(cards[1].get("supportsReasoningEffort").is_none());
    }

    #[test]
    fn model_card_full_emits_context_for_grok() {
        let c = model_card_full(
            "xai:grok-4.5",
            "spock-catalog",
            Some(500_000),
            Some("Grok 4.5"),
            Some("native"),
            None,
        );
        assert_eq!(c["id"], "xai:grok-4.5");
        assert_eq!(c["model"], "xai:grok-4.5");
        assert_eq!(c["context_window"], 500_000);
        assert_eq!(c["contextWindow"], 500_000);
        assert_eq!(c["name"], "Grok 4.5");
        assert_eq!(c["description"], "native");
        assert_eq!(c["supportsReasoningEffort"], true);
    }

    #[test]
    fn model_card_deepseek_lan_advertises_effort() {
        let id = "lan:/mnt/nvme0/bigmodels/dsv4-flash/UD-Q4_K_XL/DeepSeek-V4-Flash-0731.gguf";
        let c = model_card_full(
            id,
            "spock-catalog",
            Some(500_000),
            Some("DeepSeek V4 Flash 0731 - LAN"),
            None,
            None,
        );
        assert_eq!(c["supportsReasoningEffort"], true);
    }

    #[test]
    fn model_card_plain_local_no_effort_by_default() {
        let c = model_card_full(
            "ollama:llama3.2",
            "spock-catalog",
            Some(128_000),
            Some("Llama 3.2"),
            None,
            None,
        );
        assert!(c.get("supportsReasoningEffort").is_none());
        // Explicit opt-in still works.
        let c2 = model_card_full(
            "ollama:llama3.2",
            "spock-catalog",
            None,
            None,
            None,
            Some(true),
        );
        assert_eq!(c2["supportsReasoningEffort"], true);
    }

    #[test]
    fn context_window_from_card_variants() {
        assert_eq!(
            context_window_from_card(&json!({"context_window": 128000})),
            Some(128000)
        );
        assert_eq!(
            context_window_from_card(&json!({"contextWindow": 256000})),
            Some(256000)
        );
        assert_eq!(
            context_window_from_card(&json!({"max_model_len": 65536})),
            Some(65536)
        );
        assert_eq!(
            context_window_from_card(&json!({"_meta": {"contextWindow": 1_000_000}})),
            Some(1_000_000)
        );
        assert_eq!(context_window_from_card(&json!({"id": "x"})), None);
    }

    #[test]
    fn sanitize_drops_reasoning_effort_none() {
        let mut v = json!({"reasoning_effort": "none", "stop": ["x"]});
        sanitize_upstream(&mut v, true);
        assert!(v.get("reasoning_effort").is_none());
        assert!(v.get("stop").is_none());
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

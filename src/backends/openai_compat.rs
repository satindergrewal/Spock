//! OpenAI-compatible chat completions + Anthropic Messages passthrough.

use crate::backends::UpstreamBody;
use crate::config::UA;
use crate::error::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

/// Connect timeout only; read timeout is per-socket-read (idle), not total wall clock.
/// Streaming chat can run 30–60+ minutes on slow LAN models — use a long idle window.
const CONNECT_SECS: u64 = 15;
/// Idle between reads for long streaming generations (llama-server slow decode).
const STREAM_IDLE_READ_SECS: u64 = 3600; // 1 hour between chunks before giving up
/// Non-stream / admin JSON calls (includes server-tools full completions).
const JSON_READ_SECS: u64 = 3600; // 1h — slow LAN non-stream can still be long

fn agent(timeout_secs: u64, user_agent: &str) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(CONNECT_SECS))
        // Per-read idle timeout. Must NOT use AgentBuilder::timeout() — that caps the
        // whole request including a multi-hour streaming body.
        .timeout_read(Duration::from_secs(timeout_secs))
        .user_agent(user_agent)
        .build()
}

fn apply_headers(mut req: ureq::Request, headers: &BTreeMap<String, String>) -> ureq::Request {
    for (k, v) in headers {
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        // Never let extra headers clobber Authorization we already set from the bearer.
        if k.eq_ignore_ascii_case("authorization") {
            continue;
        }
        req = req.set(k, v);
    }
    req
}

#[allow(clippy::too_many_arguments)]
pub fn chat(
    base_url: &str,
    api_key: Option<&str>,
    body: &Value,
    stream: bool,
    headers: &BTreeMap<String, String>,
    azure_deployment: Option<&str>,
    azure_api_version: Option<&str>,
    use_responses: bool,
    user_agent: Option<&str>,
) -> Result<UpstreamBody> {
    if use_responses {
        return Err(Error::Msg(
            "OpenAI Responses API is not implemented in Spock. Set use_responses_api=false \
             (default) and use Chat Completions."
                .into(),
        ));
    }
    let ua = user_agent.unwrap_or(UA);
    let base = base_url.trim_end_matches('/');
    let url = if let Some(dep) = azure_deployment {
        let ver = azure_api_version.unwrap_or("2024-06-01");
        format!("{base}/openai/deployments/{dep}/chat/completions?api-version={ver}")
    } else {
        format!("{base}/chat/completions")
    };
    // Streaming: long per-read idle so slow backends (e.g. ~11 tok/s LAN) can run for hours.
    // Non-stream: still generous (server-tools multi-round full completions).
    let agent = agent(
        if stream {
            STREAM_IDLE_READ_SECS
        } else {
            JSON_READ_SECS
        },
        ua,
    );
    let mut req = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set(
            "Accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
    if let Some(k) = api_key {
        if !k.is_empty() {
            if azure_deployment.is_some() {
                req = req.set("api-key", k);
            } else {
                req = req.set("Authorization", &format!("Bearer {k}"));
            }
        }
    }
    req = apply_headers(req, headers);
    let mut payload = body.clone();
    if stream {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
            // Many OpenAI-compat servers omit usage from streams unless asked.
            // Spock forwards prompt_tokens in message_delta so Claude Code's
            // context gauge / auto-compact can track real fill.
            obj.entry("stream_options".to_string())
                .or_insert_with(|| json!({"include_usage": true}));
        }
    }
    match req.send_json(payload) {
        Ok(resp) => {
            if stream {
                Ok(UpstreamBody::Stream(Box::new(resp.into_reader())))
            } else {
                Ok(UpstreamBody::Json(resp.into_json()?))
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or_else(
                |_| json!({"error": {"message": text.chars().take(500).collect::<String>()}}),
            );
            Err(Error::Http(code, v))
        }
        Err(e) => Err(Error::Msg(format!("openai chat: {e}"))),
    }
}

/// POST JSON to an arbitrary path on `base_url` (already the origin, no /v1 append).
pub fn post_json(
    base_url: &str,
    api_key: Option<&str>,
    path: &str,
    body: &Value,
    stream: bool,
    headers: &BTreeMap<String, String>,
    user_agent: Option<&str>,
) -> Result<UpstreamBody> {
    let ua = user_agent.unwrap_or(UA);
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let agent = agent(
        if stream {
            STREAM_IDLE_READ_SECS
        } else {
            JSON_READ_SECS
        },
        ua,
    );
    let mut req = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set(
            "Accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
    if let Some(k) = api_key {
        if !k.is_empty() {
            req = req.set("Authorization", &format!("Bearer {k}"));
        }
    }
    req = apply_headers(req, headers);
    let mut payload = body.clone();
    if stream {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
    }
    match req.send_json(payload) {
        Ok(resp) => {
            if stream {
                Ok(UpstreamBody::Stream(Box::new(resp.into_reader())))
            } else {
                Ok(UpstreamBody::Json(resp.into_json()?))
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or_else(
                |_| json!({"error": {"message": text.chars().take(500).collect::<String>()}}),
            );
            Err(Error::Http(code, v))
        }
        Err(e) => Err(Error::Msg(format!("native post {path}: {e}"))),
    }
}

pub fn get_json(
    base_url: &str,
    api_key: Option<&str>,
    path: &str,
    headers: &BTreeMap<String, String>,
    azure_deployment: Option<&str>,
    azure_api_version: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Value> {
    let ua = user_agent.unwrap_or(UA);
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let agent = agent(120, ua);
    let mut req = agent.get(&url).set("Accept", "application/json");
    if let Some(k) = api_key {
        if !k.is_empty() {
            if azure_deployment.is_some() {
                req = req.set("api-key", k);
            } else {
                req = req.set("Authorization", &format!("Bearer {k}"));
            }
        }
    }
    let _ = azure_api_version;
    req = apply_headers(req, headers);
    match req.call() {
        Ok(resp) => Ok(resp.into_json()?),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or_else(
                |_| json!({"error": {"message": text.chars().take(500).collect::<String>()}}),
            );
            Err(Error::Http(code, v))
        }
        Err(e) => Err(Error::Msg(format!("openai get: {e}"))),
    }
}

pub fn list_models(
    base_url: &str,
    api_key: Option<&str>,
    headers: &BTreeMap<String, String>,
    user_agent: Option<&str>,
) -> Result<Vec<String>> {
    let base = base_url.trim_end_matches('/');
    let models_err = match get_json(
        base_url, api_key, "/models", headers, None, None, user_agent,
    ) {
        Ok(v) => {
            let ids = extract_openai_model_ids(&v);
            if !ids.is_empty() {
                return Ok(ids);
            }
            None
        }
        Err(Error::Http(404, ref body)) => Some(format!(
            "GET {base}/models → 404: {}",
            extract_simple_err(body)
        )),
        Err(e) => {
            return Err(Error::Msg(format!(
                "model list failed at {base}/models: {e}"
            )));
        }
    };

    let root = ollama_root(base_url);
    let tags_url = format!("{root}/api/tags");
    let agent = agent(120, user_agent.unwrap_or(UA));
    match agent
        .get(&tags_url)
        .set("Accept", "application/json")
        .call()
    {
        Ok(resp) => {
            let v: Value = resp.into_json()?;
            let mut ids = extract_ollama_tag_ids(&v);
            ids.sort();
            ids.dedup();
            if ids.is_empty() {
                Err(Error::Msg(match models_err {
                    Some(m) => format!("{m}; also no models on {tags_url}"),
                    None => format!("no models on {base}/models or {tags_url}"),
                }))
            } else {
                Ok(ids)
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Err(Error::Msg(format!(
                "model list failed ({code}) at {tags_url}: {}",
                text.chars().take(200).collect::<String>()
            )))
        }
        Err(e) => Err(Error::Msg(match models_err {
            Some(m) => format!("{m}; Ollama tags also failed ({tags_url}): {e}"),
            None => format!("model list failed for {base_url}: {e}"),
        })),
    }
}

fn extract_simple_err(body: &Value) -> String {
    body.get("error")
        .and_then(|e| e.get("message").or_else(|| e.as_str().map(|_| e)))
        .and_then(|m| m.as_str())
        .map(|s| s.chars().take(200).collect())
        .unwrap_or_else(|| body.to_string().chars().take(200).collect())
}

fn extract_openai_model_ids(v: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
        for m in data {
            if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                if !id.is_empty() {
                    ids.push(id.to_string());
                }
            }
        }
    }
    if ids.is_empty() {
        if let Some(arr) = v.as_array() {
            for m in arr {
                if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                    ids.push(id.to_string());
                } else if let Some(s) = m.as_str() {
                    ids.push(s.to_string());
                }
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

fn extract_ollama_tag_ids(v: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(models) = v.get("models").and_then(|m| m.as_array()) {
        for m in models {
            if let Some(name) = m
                .get("name")
                .and_then(|n| n.as_str())
                .or_else(|| m.get("model").and_then(|n| n.as_str()))
            {
                if !name.is_empty() {
                    ids.push(name.to_string());
                }
            }
        }
    }
    ids
}

pub fn anthropic_messages(
    base_url: &str,
    api_key: Option<&str>,
    body: &Value,
    stream: bool,
) -> Result<UpstreamBody> {
    let base = base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };
    let agent = agent(
        if stream {
            STREAM_IDLE_READ_SECS
        } else {
            JSON_READ_SECS
        },
        UA,
    );
    let mut req = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("anthropic-version", "2023-06-01")
        .set(
            "Accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
    if let Some(k) = api_key {
        if !k.is_empty() {
            req = req.set("x-api-key", k);
        }
    }
    let mut payload = body.clone();
    if stream {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
    }
    match req.send_json(payload) {
        Ok(resp) => {
            if stream {
                Ok(UpstreamBody::Stream(Box::new(resp.into_reader())))
            } else {
                Ok(UpstreamBody::Json(resp.into_json()?))
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or_else(
                |_| json!({"error": {"message": text.chars().take(500).collect::<String>()}}),
            );
            Err(Error::Http(code, v))
        }
        Err(e) => Err(Error::Msg(format!("anthropic messages: {e}"))),
    }
}

fn ollama_root(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    if let Some(root) = b.strip_suffix("/v1") {
        root.to_string()
    } else {
        b.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_root_strips_v1() {
        assert_eq!(
            ollama_root("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434"
        );
    }

    #[test]
    fn parse_openai_models() {
        let v = json!({"data":[{"id":"qwen2.5:14b"},{"id":"llama3.2"}]});
        assert_eq!(
            extract_openai_model_ids(&v),
            vec!["llama3.2".to_string(), "qwen2.5:14b".to_string()]
        );
    }

    #[test]
    fn parse_ollama_tags() {
        let v = json!({"models":[{"name":"qwen2.5:14b"},{"name":"nomic-embed-text"}]});
        let ids = extract_ollama_tag_ids(&v);
        assert!(ids.contains(&"qwen2.5:14b".to_string()));
    }
}

use crate::backends::UpstreamBody;
use crate::config::UA;
use crate::error::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(timeout_secs))
        .user_agent(UA)
        .build()
}

fn apply_extra(mut req: ureq::Request, extra: &BTreeMap<String, String>) -> ureq::Request {
    for (k, v) in extra {
        let k = k.trim();
        if !k.is_empty() {
            req = req.set(k, v);
        }
    }
    req
}

pub fn chat(
    base_url: &str,
    api_key: Option<&str>,
    body: &Value,
    stream: bool,
    extra_headers: &BTreeMap<String, String>,
    azure_deployment: Option<&str>,
    azure_api_version: Option<&str>,
    use_responses: bool,
) -> Result<UpstreamBody> {
    if use_responses {
        return Err(Error::Msg(
            "OpenAI Responses API (use_responses_api=true) is not fully implemented yet; use Chat Completions (default)".into(),
        ));
    }
    let base = base_url.trim_end_matches('/');
    let url = if let Some(dep) = azure_deployment {
        let ver = azure_api_version.unwrap_or("2024-06-01");
        format!(
            "{base}/openai/deployments/{dep}/chat/completions?api-version={ver}"
        )
    } else {
        format!("{base}/chat/completions")
    };
    let agent = agent(600);
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
    req = apply_extra(req, extra_headers);
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
        Err(e) => Err(Error::Msg(format!("openai chat: {e}"))),
    }
}

pub fn get_json(
    base_url: &str,
    api_key: Option<&str>,
    path: &str,
    extra_headers: &BTreeMap<String, String>,
    azure_deployment: Option<&str>,
    azure_api_version: Option<&str>,
) -> Result<Value> {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let agent = agent(30);
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
    req = apply_extra(req, extra_headers);
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

/// List models for Settings discovery.
pub fn list_models(
    base_url: &str,
    api_key: Option<&str>,
    extra_headers: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let base = base_url.trim_end_matches('/');
    let models_err = match get_json(base_url, api_key, "/models", extra_headers, None, None) {
        Ok(v) => {
            let ids = extract_openai_model_ids(&v);
            if !ids.is_empty() {
                return Ok(ids);
            }
            None
        }
        Err(Error::Http(code, ref body)) if code == 404 => Some(format!(
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
    let agent = agent(30);
    match agent.get(&tags_url).set("Accept", "application/json").call() {
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

/// POST Anthropic /v1/messages passthrough (no OpenAI translation).
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
    let agent = agent(600);
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

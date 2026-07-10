use crate::backends::UpstreamBody;
use crate::config::UA;
use crate::error::{Error, Result};
use serde_json::{json, Value};
use std::time::Duration;

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(timeout_secs))
        .user_agent(UA)
        .build()
}

pub fn chat(
    base_url: &str,
    api_key: Option<&str>,
    body: &Value,
    stream: bool,
) -> Result<UpstreamBody> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
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
            req = req.set("Authorization", &format!("Bearer {k}"));
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
        Err(e) => Err(Error::Msg(format!("openai chat: {e}"))),
    }
}

pub fn get_json(base_url: &str, api_key: Option<&str>, path: &str) -> Result<Value> {
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
            req = req.set("Authorization", &format!("Bearer {k}"));
        }
    }
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
/// Tries OpenAI-compatible GET {base}/models first (llama-server, Ollama OpenAI shim).
/// Falls back to Ollama native GET /api/tags.
pub fn list_models(base_url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    if let Ok(v) = get_json(base_url, api_key, "/models") {
        let ids = extract_openai_model_ids(&v);
        if !ids.is_empty() {
            return Ok(ids);
        }
    }

    let root = ollama_root(base_url);
    let tags_url = format!("{root}/api/tags");
    let agent = agent(15);
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
                Err(Error::Msg("no models found on Ollama /api/tags".into()))
            } else {
                Ok(ids)
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Err(Error::Msg(format!(
                "model list failed ({code}): {}",
                text.chars().take(200).collect::<String>()
            )))
        }
        Err(e) => Err(Error::Msg(format!("model list failed for {base_url}: {e}"))),
    }
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
        assert_eq!(
            ollama_root("http://10.0.0.5:8080/v1"),
            "http://10.0.0.5:8080"
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

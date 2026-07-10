use crate::auth::TokenCache;
use crate::backends::UpstreamBody;
use crate::config::UA;
use crate::error::{Error, Result};
use serde_json::Value;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(timeout_secs))
        .user_agent(UA)
        .build()
}

/// Priority: backend api_key → env XAI_TOKEN (via TokenCache) → OAuth.
fn bearer(api_key: Option<&str>, tokens: &Arc<Mutex<TokenCache>>) -> Result<String> {
    if let Some(k) = api_key.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(k.to_string());
    }
    let mut guard = tokens
        .lock()
        .map_err(|_| Error::Msg("token cache lock poisoned".into()))?;
    guard.get(true)
}

pub fn chat(
    base_url: &str,
    api_key: Option<&str>,
    body: &Value,
    stream: bool,
    tokens: &Arc<Mutex<TokenCache>>,
) -> Result<UpstreamBody> {
    let token = bearer(api_key, tokens)?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let agent = agent(600);
    let req = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set(
            "Accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
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
                let v: Value = resp.into_json()?;
                Ok(UpstreamBody::Json(v))
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_else(|_| String::new());
            let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
                serde_json::json!({"error": {"message": text.chars().take(500).collect::<String>()}})
            });
            Err(Error::Http(code, v))
        }
        Err(e) => Err(Error::Msg(format!("xai chat: {e}"))),
    }
}

pub fn get_json(
    base_url: &str,
    path: &str,
    api_key: Option<&str>,
    tokens: &Arc<Mutex<TokenCache>>,
) -> Result<Value> {
    let token = bearer(api_key, tokens)?;
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let agent = agent(30);
    match agent
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .call()
    {
        Ok(resp) => Ok(resp.into_json()?),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
                serde_json::json!({"error": {"message": text.chars().take(500).collect::<String>()}})
            });
            Err(Error::Http(code, v))
        }
        Err(e) => Err(Error::Msg(format!("xai get: {e}"))),
    }
}

/// Drain a stream reader into lines for SSE parsing (helper).
#[allow(dead_code)]
pub fn read_sse_chunks(mut reader: impl Read) -> impl Iterator<Item = Value> {
    let mut buf = String::new();
    let mut raw = Vec::new();
    let _ = reader.read_to_end(&mut raw);
    buf.push_str(&String::from_utf8_lossy(&raw));
    let mut out = Vec::new();
    for line in buf.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.trim();
            if payload == "[DONE]" {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                out.push(v);
            }
        }
    }
    out.into_iter()
}

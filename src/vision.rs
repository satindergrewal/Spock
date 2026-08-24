//! Vision policy for text-only backends.
//!
//! Text-only upstreams hard-400 on image content, and clients re-send the
//! full transcript — one screenshot poisons every later request in the
//! session. Applied before the backend sees the request: image blocks are
//! replaced with a note (strip) or captioned by a small VL sidecar and
//! inlined as text (describe). Any sidecar failure degrades to strip; a
//! request must never die here.

use crate::config::VisionSection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

/// Bounded in-memory caption cache. The key mixes the image and the prompt,
/// so editing the prompt invalidates stale captions. Never persisted.
#[derive(Default)]
pub struct VisionCache {
    inner: Mutex<(VecDeque<String>, HashMap<String, String>)>,
}

impl VisionCache {
    pub fn get(&self, key: &str) -> Option<String> {
        self.inner.lock().unwrap().1.get(key).cloned()
    }

    pub fn put(&self, key: String, caption: String, cap: usize) {
        let mut g = self.inner.lock().unwrap();
        let existed = g.1.insert(key.clone(), caption).is_some();
        if !existed {
            g.0.push_back(key);
            while g.0.len() > cap {
                if let Some(old) = g.0.pop_front() {
                    g.1.remove(&old);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionAction {
    /// Backend is not text-only: leave images alone.
    Off,
    /// Replace images with the omission note.
    Strip,
    /// Caption via sidecar; per-image fallback to the note on any failure.
    Describe,
}

/// `model_flag` covers built-in matchers (glm-5.3); describe is offered only
/// to backends explicitly flagged `text_only`.
pub fn decide(backend_flag: bool, model_flag: bool, cfg: &VisionSection) -> VisionAction {
    if !backend_flag && !model_flag {
        return VisionAction::Off;
    }
    if backend_flag && cfg.describe_ready() {
        VisionAction::Describe
    } else {
        VisionAction::Strip
    }
}

/// Replace Anthropic `image` blocks (user content and tool_result content)
/// in place. Returns the number of images handled.
pub fn apply_anthropic(
    a: &mut Value,
    action: VisionAction,
    note: &str,
    cfg: &VisionSection,
    cache: &VisionCache,
) -> usize {
    if action == VisionAction::Off {
        return 0;
    }
    let mut handled = 0;
    let mut sidecar_alive = true;
    let Some(msgs) = a.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };
    for msg in msgs {
        let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "image" => {
                    let uri = data_uri_from_anthropic_source(block);
                    let text = replacement(&uri, action, note, cfg, cache, &mut sidecar_alive);
                    *block = json!({ "type": "text", "text": text });
                    handled += 1;
                }
                "tool_result" => {
                    let Some(sub) = block.get_mut("content").and_then(|c| c.as_array_mut()) else {
                        continue;
                    };
                    for sb in sub {
                        if sb.get("type").and_then(|t| t.as_str()) == Some("image") {
                            let uri = data_uri_from_anthropic_source(sb);
                            let text =
                                replacement(&uri, action, note, cfg, cache, &mut sidecar_alive);
                            *sb = json!({ "type": "text", "text": text });
                            handled += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    handled
}

/// Replace OpenAI `image_url` parts in message content arrays in place.
/// The OpenAI ingress forwards bodies verbatim, so this is not defensive —
/// it is the only rewrite that path gets.
pub fn apply_openai(
    b: &mut Value,
    action: VisionAction,
    note: &str,
    cfg: &VisionSection,
    cache: &VisionCache,
) -> usize {
    if action == VisionAction::Off {
        return 0;
    }
    let mut handled = 0;
    let mut sidecar_alive = true;
    let Some(msgs) = b.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };
    for msg in msgs {
        let Some(parts) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                let uri = part
                    .get("image_url")
                    .and_then(|u| u.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = replacement(&uri, action, note, cfg, cache, &mut sidecar_alive);
                *part = json!({ "type": "text", "text": text });
                handled += 1;
            }
        }
    }
    handled
}

fn data_uri_from_anthropic_source(block: &Value) -> String {
    let Some(src) = block.get("source") else {
        return String::new();
    };
    match src.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "base64" => {
            let media = src
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("image/png");
            let data = src.get("data").and_then(|d| d.as_str()).unwrap_or("");
            format!("data:{media};base64,{data}")
        }
        "url" => src
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn replacement(
    uri: &str,
    action: VisionAction,
    note: &str,
    cfg: &VisionSection,
    cache: &VisionCache,
    sidecar_alive: &mut bool,
) -> String {
    if action == VisionAction::Describe && !uri.is_empty() {
        let key = cache_key(uri, &cfg.prompt_effective());
        // Cache hits are free and must survive an open breaker — the
        // breaker gates network calls, not already-earned captions.
        if let Some(cap) = cache.get(&key) {
            return format!("[image described by vision sidecar: {cap}]");
        }
        if *sidecar_alive {
            match sidecar_caption(cfg, uri) {
                Ok(Some(cap)) => {
                    cache.put(key, cap.clone(), cfg.cache_max);
                    return format!("[image described by vision sidecar: {cap}]");
                }
                Ok(None) => {
                    eprintln!("  vision sidecar returned empty caption, stripping image");
                }
                Err(e) => {
                    // Transport/protocol failure proves the sidecar unhealthy
                    // for this request: strip the rest instead of burning a
                    // timeout each.
                    eprintln!("  vision sidecar failed, stripping remaining images: {e}");
                    *sidecar_alive = false;
                }
            }
        }
    }
    note.to_string()
}

fn sidecar_caption(cfg: &VisionSection, data_uri: &str) -> Result<Option<String>, String> {
    let base = cfg
        .sidecar_base_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/');
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(cfg.timeout_secs.max(1)))
        .build();
    let mut req = agent
        .post(&format!("{base}/chat/completions"))
        .set("User-Agent", crate::config::UA)
        .set("Content-Type", "application/json");
    if let Some(key) = cfg.resolve_key() {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let body = json!({
        "model": cfg.sidecar_model,
        "max_tokens": cfg.max_tokens,
        "temperature": 0.2,
        "top_k": 50,
        "repeat_penalty": 1.0,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": cfg.prompt_effective() },
                { "type": "image_url", "image_url": { "url": data_uri } },
            ],
        }],
    });
    let resp = req
        .send_json(body)
        .map_err(|e| format!("sidecar request: {e}"))?;
    let v: Value = resp.into_json().map_err(|e| format!("sidecar body: {e}"))?;
    let content = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"));
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    let text = text.trim().to_string();
    // Empty caption is a useless answer, not a dead sidecar: strip this
    // image but keep calling for the rest (a blank screenshot is legal).
    Ok(if text.is_empty() { None } else { Some(text) })
}

fn cache_key(uri: &str, prompt: &str) -> String {
    let mut h = Sha256::new();
    h.update(uri.as_bytes());
    h.update(b"|");
    h.update(prompt.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn img_block() -> Value {
        json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": "AAA" }
        })
    }

    fn anth_request() -> Value {
        json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "look" },
                    img_block(),
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": [
                        { "type": "text", "text": "shot" },
                        img_block(),
                    ]},
                ]},
            ]
        })
    }

    const NOTE: &str = "[image omitted: this backend is text-only]";

    #[test]
    fn strip_replaces_user_and_tool_result_images() {
        let mut a = anth_request();
        let cfg = VisionSection::default();
        let n = apply_anthropic(
            &mut a,
            VisionAction::Strip,
            NOTE,
            &cfg,
            &VisionCache::default(),
        );
        assert_eq!(n, 2);
        let s = a.to_string();
        assert!(!s.contains("\"image\""), "no image blocks may remain: {s}");
        assert!(s.contains(NOTE));
    }

    #[test]
    fn strip_openai_replaces_image_url_parts() {
        let mut b = json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "look" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAA" } },
                ]},
            ]
        });
        let cfg = VisionSection::default();
        let n = apply_openai(
            &mut b,
            VisionAction::Strip,
            NOTE,
            &cfg,
            &VisionCache::default(),
        );
        assert_eq!(n, 1);
        let s = b.to_string();
        assert!(
            !s.contains("image_url"),
            "no image_url parts may remain: {s}"
        );
    }

    #[test]
    fn off_leaves_images_alone() {
        let mut a = anth_request();
        let cfg = VisionSection::default();
        let n = apply_anthropic(
            &mut a,
            VisionAction::Off,
            NOTE,
            &cfg,
            &VisionCache::default(),
        );
        assert_eq!(n, 0);
        assert!(a.to_string().contains("\"image\""));
    }

    #[test]
    fn decide_matrix() {
        let mut cfg = VisionSection::default();
        assert_eq!(decide(false, false, &cfg), VisionAction::Off);
        assert_eq!(decide(true, false, &cfg), VisionAction::Strip);
        assert_eq!(decide(false, true, &cfg), VisionAction::Strip);
        // model-flag-only never describes, even with a live sidecar.
        cfg.mode = "describe".into();
        cfg.sidecar_base_url = Some("http://127.0.0.1:9/v1".into());
        cfg.sidecar_model = Some("vl".into());
        assert_eq!(decide(false, true, &cfg), VisionAction::Strip);
        assert_eq!(decide(true, false, &cfg), VisionAction::Describe);
        // describe without a full endpoint degrades to strip.
        cfg.sidecar_model = None;
        assert_eq!(decide(true, false, &cfg), VisionAction::Strip);
    }

    /// HTTP stub: reads each request body, cycles (status, body) responses,
    /// counts hits.
    fn stub_http(responses: Vec<(u16, String)>, hits: Arc<AtomicUsize>) -> u16 {
        let queue: Arc<Mutex<std::collections::VecDeque<(u16, String)>>> =
            Arc::new(Mutex::new(responses.into_iter().collect()));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        thread::spawn(move || {
            while let Ok((mut s, _)) = listener.accept() {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                // Read headers + content-length body, best effort.
                let header_end = loop {
                    match s.read(&mut tmp) {
                        Ok(0) => break None,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                                break Some(pos + 4);
                            }
                        }
                        Err(_) => break None,
                    }
                };
                if let Some(start) = header_end {
                    let headers = String::from_utf8_lossy(&buf[..start]).to_string();
                    let len = headers
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    while buf.len() < start + len {
                        match s.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            Err(_) => break,
                        }
                    }
                }
                hits.fetch_add(1, Ordering::SeqCst);
                let (status, resp) = {
                    let mut q = queue.lock().unwrap();
                    let x = q.pop_front().unwrap_or_else(|| (200, "{}".into()));
                    q.push_back(x.clone());
                    x
                };
                let body = resp.as_bytes();
                let head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body);
                let _ = s.flush();
            }
        });
        port
    }

    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    fn describe_cfg(port: u16) -> VisionSection {
        VisionSection {
            mode: "describe".into(),
            sidecar_base_url: Some(format!("http://127.0.0.1:{port}/v1")),
            sidecar_model: Some("vl".into()),
            ..VisionSection::default()
        }
    }

    #[test]
    fn describe_captions_and_caches() {
        let hits = Arc::new(AtomicUsize::new(0));
        let resp = r#"{"choices":[{"message":{"content":"dark scene, sun center"}}]}"#;
        let port = stub_http(vec![(200, resp.into())], hits.clone());
        let cfg = describe_cfg(port);
        let cache = VisionCache::default();

        let mut a = anth_request();
        let n = apply_anthropic(&mut a, VisionAction::Describe, NOTE, &cfg, &cache);
        assert_eq!(n, 2);
        let s = a.to_string();
        assert!(
            s.contains("[image described by vision sidecar: dark scene, sun center]"),
            "caption must be inlined: {s}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "second image is a cache hit"
        );

        // Same image again: still no sidecar traffic.
        let mut a2 = anth_request();
        apply_anthropic(&mut a2, VisionAction::Describe, NOTE, &cfg, &cache);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_caption_strips_that_image_only() {
        let hits = Arc::new(AtomicUsize::new(0));
        // Valid HTTP but empty caption: useless answer, not a dead sidecar.
        // Both images get their own (failed) call — breaker stays closed.
        let port = stub_http(
            vec![(200, r#"{"choices":[{"message":{"content":""}}]}"#.into())],
            hits.clone(),
        );
        let cfg = describe_cfg(port);
        let mut a = anth_request();
        let n = apply_anthropic(
            &mut a,
            VisionAction::Describe,
            NOTE,
            &cfg,
            &VisionCache::default(),
        );
        assert_eq!(n, 2);
        let s = a.to_string();
        assert!(!s.contains("\"image\""));
        assert!(s.contains(NOTE), "empty caption must degrade to strip: {s}");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "sidecar stays alive");
    }

    #[test]
    fn empty_caption_keeps_sidecar_alive_for_later_images() {
        let hits = Arc::new(AtomicUsize::new(0));
        // A: empty caption (strip). B: real caption (describe).
        let port = stub_http(
            vec![
                (200, r#"{"choices":[{"message":{"content":""}}]}"#.into()),
                (
                    200,
                    r#"{"choices":[{"message":{"content":"real caption"}}]}"#.into(),
                ),
            ],
            hits.clone(),
        );
        let cfg = describe_cfg(port);
        let mut a = json!({ "model": "m", "messages": [
            { "role": "user", "content": [
                { "type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "AAA" } } ] },
            { "role": "user", "content": [
                { "type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "BBB" } } ] },
        ]});
        let n = apply_anthropic(
            &mut a,
            VisionAction::Describe,
            NOTE,
            &cfg,
            &VisionCache::default(),
        );
        assert_eq!(n, 2);
        let s = a.to_string();
        assert!(
            s.contains("[image described by vision sidecar: real caption]"),
            "later image must still be described: {s}"
        );
        assert!(s.contains(NOTE));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn describe_falls_back_to_strip_on_dead_sidecar() {
        // Bind then drop: connection refused.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let cfg = describe_cfg(port);
        let mut a = anth_request();
        let n = apply_anthropic(
            &mut a,
            VisionAction::Describe,
            NOTE,
            &cfg,
            &VisionCache::default(),
        );
        assert_eq!(n, 2);
        assert!(a.to_string().contains(NOTE));
    }

    #[test]
    fn breaker_strips_remaining_after_first_failure() {
        let hits = Arc::new(AtomicUsize::new(0));
        // HTTP 500 = transport/protocol death. Both images are identical, so
        // without the breaker the second would retry and fail again.
        let port = stub_http(vec![(500, "{}".into())], hits.clone());
        let cfg = describe_cfg(port);
        let mut a = anth_request();
        let n = apply_anthropic(
            &mut a,
            VisionAction::Describe,
            NOTE,
            &cfg,
            &VisionCache::default(),
        );
        assert_eq!(n, 2);
        let s = a.to_string();
        assert!(s.contains(NOTE), "failure must degrade to strip: {s}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "one failed call strips the rest of the request"
        );
    }

    #[test]
    fn cache_hits_survive_open_breaker() {
        let hits = Arc::new(AtomicUsize::new(0));
        // Sidecar dies (HTTP 500) on the first image.
        let port = stub_http(vec![(500, "{}".into())], hits.clone());
        let cfg = describe_cfg(port);
        let cache = VisionCache::default();
        // Pre-seed the caption for image B.
        let key_b = cache_key("data:image/png;base64,BBB", &cfg.prompt_effective());
        cache.put(key_b, "cached caption".into(), 128);

        let mut a = json!({ "model": "m", "messages": [
            { "role": "user", "content": [
                { "type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "AAA" } } ] },
            { "role": "user", "content": [
                { "type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "BBB" } } ] },
        ]});
        let n = apply_anthropic(&mut a, VisionAction::Describe, NOTE, &cfg, &cache);
        assert_eq!(n, 2);
        let s = a.to_string();
        assert!(
            s.contains("[image described by vision sidecar: cached caption]"),
            "cached caption must survive an open breaker: {s}"
        );
        assert!(
            s.contains(NOTE),
            "uncached image strips after the failure: {s}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "only A hit the sidecar");
    }

    #[test]
    fn no_cap_when_sidecar_healthy() {
        let hits = Arc::new(AtomicUsize::new(0));
        let port = stub_http(
            vec![(
                200,
                r#"{"choices":[{"message":{"content":"shot"}}]}"#.into(),
            )],
            hits.clone(),
        );
        let cfg = describe_cfg(port);
        // Three distinct images: a healthy sidecar captions all of them.
        let mut a = json!({ "model": "m", "messages": [] });
        for d in ["AAA", "BBB", "CCC"] {
            a["messages"].as_array_mut().unwrap().push(json!({
                "role": "user",
                "content": [
                    { "type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": d } },
                ],
            }));
        }
        let n = apply_anthropic(
            &mut a,
            VisionAction::Describe,
            NOTE,
            &cfg,
            &VisionCache::default(),
        );
        assert_eq!(n, 3);
        let s = a.to_string();
        assert!(!s.contains(NOTE), "healthy sidecar must not strip: {s}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cache_key_changes_with_prompt() {
        let k1 = cache_key("data:x", "p1");
        let k2 = cache_key("data:x", "p2");
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_evicts_oldest() {
        let c = VisionCache::default();
        c.put("a".into(), "1".into(), 2);
        c.put("b".into(), "2".into(), 2);
        c.put("c".into(), "3".into(), 2);
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_some());
        assert!(c.get("c").is_some());
    }
}

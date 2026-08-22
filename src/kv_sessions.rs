//! llama-server ds4-ports named KV sessions.
//!
//! `/v1/chat/completions` does **not** parse `session_id` / `parent_session_id`.
//! Park, fork, and close go to native routes only. Missing routes or an unknown
//! session fail the Claude Code request — never a silent cold `/chat/completions`.

use crate::backends::UpstreamBody;
use crate::error::{Error, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Header: explicit master id (optional when `cache_control` marks a prefix).
pub const HDR_SESSION: &str = "x-spock-session";
/// Header: fork from this named master (defaults to the session id).
pub const HDR_PARENT: &str = "x-spock-parent-session";
/// Header: `POST /close_session` this id after (or instead of) generation.
pub const HDR_CLOSE: &str = "x-spock-close-session";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionHint {
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub close_session: Option<String>,
    pub has_cache_control: bool,
}

/// Strip `/v1` so native `/fork` hits llama-server, not a v1 404.
pub fn llama_origin(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    if let Some(o) = b.strip_suffix("/v1") {
        o.to_string()
    } else {
        b.to_string()
    }
}

pub fn take_session_hint(body: &mut Value, headers: &BTreeMap<String, String>) -> SessionHint {
    let header = |name: &str| {
        headers
            .get(name)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let mut session_id = header(HDR_SESSION);
    let mut parent_session_id = header(HDR_PARENT);
    let mut close_session = header(HDR_CLOSE);

    if let Some(obj) = body.as_object_mut() {
        if session_id.is_none() {
            session_id = take_opt_string(obj, "session_id");
        } else {
            obj.remove("session_id");
        }
        if parent_session_id.is_none() {
            parent_session_id = take_opt_string(obj, "parent_session_id");
        } else {
            obj.remove("parent_session_id");
        }
        if close_session.is_none() {
            close_session = take_opt_string(obj, "close_session");
            if close_session.is_none()
                && obj.get("close_session").and_then(|v| v.as_bool()) == Some(true)
            {
                obj.remove("close_session");
                close_session = session_id.clone();
            }
        } else {
            obj.remove("close_session");
        }
    }

    let has_cache_control = body_has_cache_control(body);
    SessionHint {
        session_id,
        parent_session_id,
        close_session,
        has_cache_control,
    }
}

fn take_opt_string(obj: &mut serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match obj.remove(key) {
        Some(Value::String(s)) => {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        Some(_) => None,
        None => None,
    }
}

pub fn body_has_cache_control(v: &Value) -> bool {
    match v {
        Value::Object(map) => {
            if map.contains_key("cache_control") {
                return true;
            }
            map.values().any(body_has_cache_control)
        }
        Value::Array(arr) => arr.iter().any(body_has_cache_control),
        _ => false,
    }
}

/// Messages through the last `cache_control` breakpoint (system + tools included).
/// `None` = no cache_control anywhere.
pub fn prefix_messages_openai(anthropic: &Value) -> Option<Vec<Value>> {
    if !body_has_cache_control(anthropic) {
        return None;
    }
    let mut cut = anthropic.clone();
    trim_after_last_cache_control(&mut cut);
    Some(crate::translate::openai_messages(&cut))
}

fn trim_after_last_cache_control(a: &mut Value) {
    let Some(obj) = a.as_object_mut() else {
        return;
    };
    // Keep system as-is (it is the shared prefix). Drop messages after the last
    // one that carries cache_control. If only system has it, drop all messages.
    let last_msg = obj
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|arr| {
            arr.iter()
                .enumerate()
                .rev()
                .find(|(_, m)| body_has_cache_control(m))
                .map(|(i, _)| i)
        });
    if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        match last_msg {
            Some(i) => {
                messages.truncate(i + 1);
            }
            None => {
                messages.clear();
            }
        }
    }
}

pub fn master_id_from_prefix(prefix_prompt: &str) -> String {
    let mut h = Sha256::new();
    h.update(prefix_prompt.as_bytes());
    let d = h.finalize();
    format!(
        "cc-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

/// Resolve the named master. Fail loud if a kv-sessions backend has no name.
pub fn resolve_master_id(hint: &SessionHint, prefix_prompt: Option<&str>) -> Result<String> {
    if let Some(id) = hint
        .parent_session_id
        .as_deref()
        .or(hint.session_id.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(id.to_string());
    }
    if hint.has_cache_control {
        if let Some(p) = prefix_prompt.map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(master_id_from_prefix(p));
        }
        return Err(Error::Msg(
            "kv_sessions: cache_control present but prefix prompt is empty — \
             cannot name a master. Set x-spock-session or fix the cached prefix."
                .into(),
        ));
    }
    Err(Error::Msg(
        "kv_sessions backend requires a named master: send x-spock-session / \
         session_id, or put cache_control on the shared prefix. Silent cold \
         prefill is disabled."
            .into(),
    ))
}

pub fn native_to_openai_chat(native: &Value, model: &str) -> Value {
    let content = native.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let stop = native.get("stop").and_then(|s| s.as_bool()).unwrap_or(true);
    let finish = if stop {
        match native.get("stop_type").and_then(|s| s.as_str()) {
            Some("limit") => "length",
            _ => "stop",
        }
    } else {
        ""
    };
    let prompt_tokens = native
        .get("tokens_evaluated")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion_tokens = native
        .get("tokens_predicted")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_n = native
        .pointer("/timings/cache_n")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let prompt_n = native.pointer("/timings/prompt_n").and_then(|v| v.as_u64());
    let id = native
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("cmpl-spock-kv");
    let mut usage = json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
        "prompt_tokens_details": { "cached_tokens": cache_n },
    });
    if let Some(pn) = prompt_n {
        usage["prompt_n"] = json!(pn);
        usage["cache_n"] = json!(cache_n);
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": if finish.is_empty() { Value::Null } else { json!(finish) },
        }],
        "usage": usage,
        "timings": native.get("timings").cloned().unwrap_or(json!({})),
    })
}

/// 404 / missing native route — never "try chat/completions instead".
pub fn missing_route_error(origin: &str, path: &str, detail: &str) -> Error {
    Error::Msg(format!(
        "kv_sessions: {origin}{path} is missing or not implemented ({detail}). \
         llama-server ds4-ports native session routes are required. \
         This request will not fall through to /v1/chat/completions."
    ))
}

pub fn is_missing_route(err: &Error) -> bool {
    match err {
        Error::Http(404, _) => true,
        Error::Http(405, _) => true,
        Error::Msg(s) => {
            s.contains("not implemented")
                || s.contains("missing or not implemented")
                || s.contains("File Not Found")
        }
        _ => false,
    }
}

/// Classify a `/fork` probe response. 400 = implemented. 404 = fail loud.
pub fn classify_fork_probe(result: Result<Value>) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(Error::Http(404, body)) => {
            Err(missing_route_error("", "/fork", &extract_http_msg(&body)))
        }
        Err(Error::Http(405, body)) => {
            Err(missing_route_error("", "/fork", &extract_http_msg(&body)))
        }
        Err(Error::Http(400, _)) => Ok(()),
        Err(Error::Http(code, _)) if (401..500).contains(&code) => Ok(()),
        Err(e) => Err(Error::Msg(format!(
            "kv_sessions: cannot reach native /fork ({e}). \
             Will not fall through to /v1/chat/completions."
        ))),
    }
}

/// Probe `/fork` once per backend. 400 = implemented. 404 stays sticky.
pub fn ensure_fork_route(
    state: &crate::state::AppState,
    be: &crate::backends::BackendHandle,
) -> Result<()> {
    {
        let cache = state
            .kv_fork_probe
            .lock()
            .map_err(|_| Error::Msg("kv probe lock".into()))?;
        if let Some(prev) = cache.get(&be.name) {
            return match prev {
                Ok(()) => Ok(()),
                Err(s) => Err(Error::Msg(s.clone())),
            };
        }
    }
    let probe = json!({});
    let classified = match be.native_post("/fork", &probe, false, &state.oauth) {
        Ok(UpstreamBody::Json(v)) => classify_fork_probe(Ok(v)),
        Ok(UpstreamBody::Stream(_)) => Ok(()),
        Err(e) => classify_fork_probe(Err(e)),
    };
    let store = match &classified {
        Ok(()) => Ok(()),
        Err(e) => Err(e.to_string()),
    };
    if let Ok(mut cache) = state.kv_fork_probe.lock() {
        cache.insert(be.name.clone(), store);
    }
    classified
}

/// Apply the server's chat template. Fail loud if the route is gone.
pub fn apply_chat_template(
    be: &crate::backends::BackendHandle,
    oauth: &crate::oauth::OauthStore,
    messages: &[Value],
    tools: Option<&Value>,
) -> Result<String> {
    let body = apply_template_body(messages, tools);
    match be.native_post("/apply-template", &body, false, oauth) {
        Ok(UpstreamBody::Json(v)) => prompt_from_apply_template(&v),
        Ok(UpstreamBody::Stream(_)) => Err(Error::Msg(
            "kv_sessions: /apply-template streamed — expected JSON".into(),
        )),
        Err(e) if is_missing_route(&e) => {
            Err(missing_route_error("", "/apply-template", &e.to_string()))
        }
        Err(e) => Err(Error::Msg(format!(
            "kv_sessions: /apply-template failed ({e}). \
             Will not fall through to /v1/chat/completions."
        ))),
    }
}

/// Park a named master (`n_predict=0`). Unknown session is not expected here.
pub fn park_master(
    be: &crate::backends::BackendHandle,
    oauth: &crate::oauth::OauthStore,
    prefix_prompt: &str,
    session_id: &str,
) -> Result<Value> {
    let body = park_body(prefix_prompt, session_id);
    match be.native_post("/completion", &body, false, oauth) {
        Ok(UpstreamBody::Json(v)) => Ok(v),
        Ok(UpstreamBody::Stream(_)) => Err(Error::Msg(
            "kv_sessions: /completion streamed on park — expected JSON".into(),
        )),
        Err(e) if is_missing_route(&e) => {
            Err(missing_route_error("", "/completion", &e.to_string()))
        }
        Err(e) => Err(Error::Msg(format!(
            "kv_sessions: park master '{session_id}' failed ({e}). \
             Will not fall through to /v1/chat/completions."
        ))),
    }
}

/// Fork from a named master. Unknown parent = pass the 400 through.
pub fn fork_child(
    be: &crate::backends::BackendHandle,
    oauth: &crate::oauth::OauthStore,
    full_prompt: &str,
    parent: &str,
    n_predict: u64,
    stream: bool,
    sampling: Option<&Value>,
) -> Result<UpstreamBody> {
    let mut body = fork_body(full_prompt, parent, n_predict, stream);
    if let Some(src) = sampling {
        copy_sampling(src, &mut body);
    }
    match be.native_post("/fork", &body, stream, oauth) {
        Ok(v) => Ok(v),
        Err(e) if is_missing_route(&e) => Err(missing_route_error("", "/fork", &e.to_string())),
        Err(Error::Http(400, body)) => Err(Error::Http(400, body)),
        Err(e) => Err(Error::Msg(format!(
            "kv_sessions: /fork parent='{parent}' failed ({e}). \
             Will not retry as a cold /v1/chat/completions."
        ))),
    }
}

pub fn close_session(
    be: &crate::backends::BackendHandle,
    oauth: &crate::oauth::OauthStore,
    session_id: &str,
) -> Result<Value> {
    let body = close_body(session_id);
    match be.native_post("/close_session", &body, false, oauth) {
        Ok(UpstreamBody::Json(v)) => {
            if v.get("success").and_then(|s| s.as_bool()) == Some(false) {
                let msg = v
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown session");
                return Err(Error::Http(
                    400,
                    json!({"error": {"message": format!("close_session '{session_id}': {msg}")}}),
                ));
            }
            Ok(v)
        }
        Ok(UpstreamBody::Stream(_)) => Err(Error::Msg(
            "kv_sessions: /close_session streamed — expected JSON".into(),
        )),
        Err(e) if is_missing_route(&e) => {
            Err(missing_route_error("", "/close_session", &e.to_string()))
        }
        Err(Error::Http(400, body)) => Err(Error::Http(400, body)),
        Err(e) => Err(Error::Msg(format!(
            "kv_sessions: /close_session '{session_id}' failed ({e}). \
             Will not fall through to /v1/chat/completions."
        ))),
    }
}

/// One CC turn: ensure /fork exists, park master if needed, /fork the child.
/// `close_only` skips generation.
pub fn run_turn(
    state: &crate::state::AppState,
    be: &crate::backends::BackendHandle,
    anthropic: &Value,
    hint: &SessionHint,
    stream: bool,
    n_predict: u64,
) -> Result<(String, UpstreamBody)> {
    ensure_fork_route(state, be)?;

    if hint.close_session.is_some()
        && hint.session_id.is_none()
        && hint.parent_session_id.is_none()
        && !hint.has_cache_control
    {
        let sid = hint.close_session.as_deref().unwrap();
        let v = close_session(be, &state.oauth, sid)?;
        return Ok((
            sid.to_string(),
            UpstreamBody::Json(native_to_openai_chat(
                &json!({
                    "content": "",
                    "stop": true,
                    "tokens_evaluated": 0,
                    "tokens_predicted": 0,
                    "timings": { "cache_n": 0, "prompt_n": 0 },
                    "closed": v,
                }),
                "closed",
            )),
        ));
    }

    let tools = crate::translate::tools_for_apply_template(anthropic);
    let full_msgs = crate::translate::openai_messages(anthropic);
    let full_prompt = apply_chat_template(be, &state.oauth, &full_msgs, tools.as_ref())?;

    let prefix_prompt = if let Some(prefix_msgs) = prefix_messages_openai(anthropic) {
        Some(apply_chat_template(
            be,
            &state.oauth,
            &prefix_msgs,
            tools.as_ref(),
        )?)
    } else {
        None
    };

    let master = resolve_master_id(hint, prefix_prompt.as_deref())?;
    let parent = hint
        .parent_session_id
        .as_deref()
        .unwrap_or(master.as_str())
        .to_string();

    // Park when we have a prefix (cache_control) or an explicit session_id
    // with no parent (this request *is* the master).
    let should_park =
        prefix_prompt.is_some() || (hint.session_id.is_some() && hint.parent_session_id.is_none());
    if should_park {
        let park_prompt = prefix_prompt.as_deref().unwrap_or(full_prompt.as_str());
        match park_master(be, &state.oauth, park_prompt, &master) {
            Ok(_) => {}
            Err(e) if is_missing_route(&e) => return Err(e),
            Err(e) => {
                // Park failed (maybe already parked). Still try /fork — unknown
                // parent will 400, which we pass through. Do not chat/completions.
                eprintln!("  kv-session park '{master}' note: {e}");
            }
        }
    }

    if hint.close_session.is_some() && n_predict == 0 && !hint.has_cache_control {
        let sid = hint.close_session.as_deref().unwrap();
        let v = close_session(be, &state.oauth, sid)?;
        return Ok((
            sid.to_string(),
            UpstreamBody::Json(json!({"ok": true, "closed": v})),
        ));
    }

    let child = fork_child(
        be,
        &state.oauth,
        &full_prompt,
        &parent,
        n_predict,
        stream,
        Some(anthropic),
    )?;
    let wrapped = wrap_native_stream(child, &format!("kv:{parent}"))?;
    if let UpstreamBody::Json(ref v) = wrapped {
        log_inherit(&parent, v);
    }
    if let Some(sid) = hint.close_session.as_deref() {
        close_session(be, &state.oauth, sid)?;
    }
    Ok((parent, wrapped))
}

pub fn extract_http_msg(body: &Value) -> String {
    body.pointer("/error/message")
        .and_then(|m| m.as_str())
        .or_else(|| body.get("message").and_then(|m| m.as_str()))
        .unwrap_or(&body.to_string())
        .chars()
        .take(300)
        .collect()
}

pub fn park_body(prefix_prompt: &str, session_id: &str) -> Value {
    json!({
        "prompt": prefix_prompt,
        "n_predict": 0,
        "session_id": session_id,
        "temperature": 0.0,
        "cache_prompt": false,
    })
}

pub fn fork_body(full_prompt: &str, parent: &str, n_predict: u64, stream: bool) -> Value {
    let mut b = json!({
        "prompt": full_prompt,
        "n_predict": n_predict,
        "parent_session_id": parent,
        "cache_prompt": false,
    });
    if stream {
        if let Some(obj) = b.as_object_mut() {
            obj.insert("stream".into(), json!(true));
        }
    }
    b
}

pub fn close_body(session_id: &str) -> Value {
    json!({ "session_id": session_id })
}

/// Copy sampling fields the native completion schema understands.
fn copy_sampling(src: &Value, dest: &mut Value) {
    let Some(obj) = dest.as_object_mut() else {
        return;
    };
    for (src_key, dest_key) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("top_k", "top_k"),
        ("min_p", "min_p"),
        ("repeat_penalty", "repeat_penalty"),
        ("seed", "seed"),
    ] {
        if let Some(v) = src.get(src_key) {
            obj.insert(dest_key.into(), v.clone());
        }
    }
    if let Some(stops) = src
        .get("stop_sequences")
        .or_else(|| src.get("stop"))
        .cloned()
    {
        obj.insert("stop".into(), stops);
    }
}

/// Convert one native SSE payload to an OpenAI chat-completion chunk.
pub fn native_chunk_to_openai(chunk: &Value, model: &str) -> Value {
    let content = chunk.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let stop = chunk.get("stop").and_then(|s| s.as_bool()).unwrap_or(false);
    let cache_n = chunk.pointer("/timings/cache_n").and_then(|v| v.as_u64());
    let prompt_n = chunk.pointer("/timings/prompt_n").and_then(|v| v.as_u64());
    let prompt_tokens = chunk
        .get("tokens_evaluated")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion_tokens = chunk
        .get("tokens_predicted")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mut out = json!({
        "id": "chatcmpl-spock-kv",
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{
            "index": 0,
            "delta": if content.is_empty() {
                json!({})
            } else {
                json!({ "content": content })
            },
            "finish_reason": if stop { json!("stop") } else { Value::Null },
        }],
    });
    if stop {
        let mut usage = json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        });
        if let Some(c) = cache_n {
            usage["prompt_tokens_details"] = json!({ "cached_tokens": c });
            usage["cache_n"] = json!(c);
        }
        if let Some(p) = prompt_n {
            usage["prompt_n"] = json!(p);
        }
        out["usage"] = usage;
        if let Some(t) = chunk.get("timings") {
            out["timings"] = t.clone();
        }
    }
    out
}

pub fn apply_template_body(messages: &[Value], tools: Option<&Value>) -> Value {
    let mut b = json!({ "messages": messages });
    if let Some(t) = tools {
        if t.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            if let Some(obj) = b.as_object_mut() {
                obj.insert("tools".into(), t.clone());
            }
        }
    }
    b
}

pub fn prompt_from_apply_template(v: &Value) -> Result<String> {
    v.get("prompt")
        .and_then(|p| p.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::Msg(format!(
                "kv_sessions: /apply-template returned no prompt ({})",
                v.to_string().chars().take(200).collect::<String>()
            ))
        })
}

/// Used by the stream pump: turn a native SSE line into an OpenAI `data:` line.
pub fn rewrite_native_sse_line(line: &str, model: &str) -> Option<String> {
    let line = line.trim();
    if !line.starts_with("data:") {
        return if line.is_empty() {
            Some(String::new())
        } else {
            Some(line.to_string())
        };
    }
    let payload = line[5..].trim();
    if payload.is_empty() || payload == "[DONE]" {
        return Some(line.to_string());
    }
    let chunk: Value = serde_json::from_str(payload).ok()?;
    if chunk.get("error").is_some() {
        return Some(format!("data: {payload}"));
    }
    let oai = native_chunk_to_openai(&chunk, model);
    Some(format!("data: {oai}"))
}

pub fn log_inherit(master: &str, native_or_oai: &Value) {
    let cache_n = native_or_oai
        .pointer("/timings/cache_n")
        .or_else(|| native_or_oai.pointer("/usage/cache_n"))
        .or_else(|| native_or_oai.pointer("/usage/prompt_tokens_details/cached_tokens"))
        .and_then(|v| v.as_u64());
    let prompt_n = native_or_oai
        .pointer("/timings/prompt_n")
        .or_else(|| native_or_oai.pointer("/usage/prompt_n"))
        .and_then(|v| v.as_u64());
    match (cache_n, prompt_n) {
        (Some(c), Some(p)) => {
            eprintln!("  kv-session master={master} cache_n={c} prompt_n={p}");
        }
        (Some(c), None) => {
            eprintln!("  kv-session master={master} cache_n={c}");
        }
        _ => {
            eprintln!("  kv-session master={master} (no timings yet)");
        }
    }
}

/// Wrap a native SSE reader so `stream_anthropic` sees OpenAI chat chunks.
pub struct NativeSseToOpenAi<R: std::io::Read> {
    inner: std::io::BufReader<R>,
    model: String,
    pending: Vec<u8>,
    done: bool,
}

impl<R: std::io::Read> NativeSseToOpenAi<R> {
    pub fn new(inner: R, model: impl Into<String>) -> Self {
        Self {
            inner: std::io::BufReader::new(inner),
            model: model.into(),
            pending: Vec::new(),
            done: false,
        }
    }
}

impl<R: std::io::Read> std::io::Read for NativeSseToOpenAi<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::BufRead;
        if self.done && self.pending.is_empty() {
            return Ok(0);
        }
        while self.pending.is_empty() && !self.done {
            let mut line = String::new();
            let n = self.inner.read_line(&mut line)?;
            if n == 0 {
                if !self.done {
                    self.pending.extend_from_slice(b"data: [DONE]\n\n");
                    self.done = true;
                }
                break;
            }
            if let Some(rewritten) = rewrite_native_sse_line(line.trim_end(), &self.model) {
                if rewritten.is_empty() {
                    continue;
                }
                self.pending.extend_from_slice(rewritten.as_bytes());
                self.pending.extend_from_slice(b"\n");
                if rewritten == "data: [DONE]" || rewritten.contains("\"finish_reason\":\"stop\"") {
                    if !rewritten.contains("[DONE]") {
                        self.pending.extend_from_slice(b"data: [DONE]\n\n");
                    }
                    self.done = true;
                }
            }
        }
        let n = self.pending.len().min(buf.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

pub fn wrap_native_stream(body: UpstreamBody, model: &str) -> Result<UpstreamBody> {
    match body {
        UpstreamBody::Json(v) => Ok(UpstreamBody::Json(native_to_openai_chat(&v, model))),
        UpstreamBody::Stream(r) => Ok(UpstreamBody::Stream(Box::new(NativeSseToOpenAi::new(
            r,
            model.to_string(),
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_strips_v1() {
        assert_eq!(
            llama_origin("http://10.0.0.5:8080/v1"),
            "http://10.0.0.5:8080"
        );
        assert_eq!(
            llama_origin("http://10.0.0.5:8080/v1/"),
            "http://10.0.0.5:8080"
        );
        assert_eq!(llama_origin("http://10.0.0.5:8080"), "http://10.0.0.5:8080");
    }

    #[test]
    fn hint_from_headers_wins() {
        let mut body = json!({"session_id": "body", "messages": []});
        let mut h = BTreeMap::new();
        h.insert(HDR_SESSION.into(), "hdr".into());
        let hint = take_session_hint(&mut body, &h);
        assert_eq!(hint.session_id.as_deref(), Some("hdr"));
        assert!(body.get("session_id").is_none());
    }

    #[test]
    fn cache_control_required_without_name() {
        let hint = SessionHint::default();
        let err = resolve_master_id(&hint, None).unwrap_err().to_string();
        assert!(err.contains("named master"), "{err}");
        assert!(
            err.contains("chat/completions") || err.contains("cold"),
            "{err}"
        );
    }

    #[test]
    fn cache_control_names_master() {
        let hint = SessionHint {
            has_cache_control: true,
            ..SessionHint::default()
        };
        let id = resolve_master_id(&hint, Some("SYSTEM PREFIX")).unwrap();
        assert!(id.starts_with("cc-"));
        assert_eq!(id, master_id_from_prefix("SYSTEM PREFIX"));
    }

    #[test]
    fn prefix_cuts_after_last_cache_control() {
        let a = json!({
            "system": [{"type":"text","text":"sys","cache_control":{"type":"ephemeral"}}],
            "messages": [
                {"role":"user","content":[{"type":"text","text":"one"}]},
                {"role":"user","content":[{"type":"text","text":"two","cache_control":{"type":"ephemeral"}}]},
                {"role":"user","content":[{"type":"text","text":"three"}]}
            ]
        });
        let prefix = prefix_messages_openai(&a).expect("cut");
        // system + two user turns (cut after last cache_control message)
        assert!(prefix.iter().any(|m| m["role"] == "system"));
        let users: Vec<_> = prefix.iter().filter(|m| m["role"] == "user").collect();
        assert_eq!(users.len(), 2, "{prefix:?}");
        assert!(users[1]["content"].as_str().unwrap().contains("two"));
    }

    #[test]
    fn no_cache_control_no_prefix() {
        let a = json!({"system":"hi","messages":[{"role":"user","content":"x"}]});
        assert!(prefix_messages_openai(&a).is_none());
    }

    #[test]
    fn native_usage_exposes_cache_n() {
        let native = json!({
            "content": "hi",
            "stop": true,
            "tokens_evaluated": 100,
            "tokens_predicted": 4,
            "timings": { "cache_n": 80, "prompt_n": 20 }
        });
        let oai = native_to_openai_chat(&native, "qwen");
        assert_eq!(oai["usage"]["prompt_tokens"], 100);
        assert_eq!(oai["usage"]["prompt_tokens_details"]["cached_tokens"], 80);
        assert_eq!(oai["usage"]["cache_n"], 80);
        assert_eq!(oai["usage"]["prompt_n"], 20);
        assert_eq!(oai["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn fork_probe_404_is_loud() {
        let err = classify_fork_probe(Err(Error::Http(404, json!({"error":"no"})))).unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("will not fall through") || s.contains("not fall through"),
            "{s}"
        );
        assert!(s.contains("/fork"), "{s}");
    }

    #[test]
    fn fork_probe_400_means_implemented() {
        classify_fork_probe(Err(Error::Http(
            400,
            json!({"error":{"message":"POST /fork needs parent_session_id"}}),
        )))
        .unwrap();
    }

    #[test]
    fn rewrite_native_sse_stop_has_usage() {
        let line = r#"data: {"content":"","stop":true,"tokens_evaluated":64,"tokens_predicted":3,"timings":{"cache_n":48,"prompt_n":16}}"#;
        let out = rewrite_native_sse_line(line, "m").unwrap();
        assert!(out.contains("cached_tokens"), "{out}");
        assert!(out.contains("\"cache_n\":48"), "{out}");
    }

    #[test]
    fn missing_route_never_mentions_fallback_as_action() {
        let e = missing_route_error("http://10.0.0.5:8080", "/fork", "404");
        let s = e.to_string();
        assert!(s.contains("will not fall through"));
        assert!(!s.contains("trying chat/completions"));
    }

    #[test]
    fn park_and_fork_bodies_are_native_not_openai() {
        let park = park_body("PREFIX", "master");
        assert_eq!(park["n_predict"], 0);
        assert_eq!(park["session_id"], "master");
        assert!(park.get("messages").is_none());
        let fork = fork_body("PREFIX suffix", "master", 16, false);
        assert_eq!(fork["parent_session_id"], "master");
        assert_eq!(fork["n_predict"], 16);
        assert!(fork.get("session_id").is_none());
        assert_eq!(close_body("master")["session_id"], "master");
    }

    #[test]
    fn openai_string_messages_pass_through() {
        // /v1/chat/completions wrap is already OpenAI-shaped; do not drop it.
        let a = json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"}
            ]
        });
        let msgs = crate::translate::openai_messages(&a);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["content"], "hi");
    }
}

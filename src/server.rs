//! Minimal threaded HTTP server (std only) speaking Anthropic + OpenAI shapes.

use crate::backends::{get_backend, UpstreamBody};
use crate::config::{EnvOverrides, DEFAULT_GROK_MODEL};
use crate::error::{anthropic_error, Error, Result};
use crate::models::{alias_models, catalog_list_cards, model_card, model_card_full, stop_reason};
use crate::route;
use crate::state::AppState;
use crate::translate::{
    anthropic_to_openai, count_tokens_estimate, new_msg_id, new_tool_id, openai_to_anthropic,
    prepare_for_openai_compat, wants_thinking, CompletionsQuirk,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub fn serve(state: AppState, shutdown: Arc<AtomicBool>) -> Result<()> {
    let addr = state.with_config(|c| c.bind_addr())?;
    let listener = TcpListener::bind(&addr)?;
    listener.set_nonblocking(true)?;
    let profile = state.with_config(|c| c.server.profile.clone())?;
    eprintln!("Spock proxy on http://{addr}");
    eprintln!("  profile: {profile}");
    eprintln!("  POST /v1/messages | /v1/chat/completions | /v1/responses | Ctrl-C to stop\n");

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let st = state.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, st) {
                        // broken pipe / reset — client gone. Do not swallow
                        // WouldBlock: that is the inherited-O_NONBLOCK bug
                        // configure_accepted_socket is supposed to kill.
                        if !matches!(&e, Error::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe
                            || io.kind() == std::io::ErrorKind::ConnectionReset)
                        {
                            eprintln!("  connection error: {e}");
                        }
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("  accept error: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    eprintln!("\nstopped");
    Ok(())
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
    headers: std::collections::BTreeMap<String, String>,
}

/// Listen socket is nonblocking so the accept loop can poll the shutdown flag.
/// On Darwin (and some BSDs) `accept()` copies `O_NONBLOCK` onto the new fd.
/// `read_exact` then returns `WouldBlock` / os error 35 as soon as the kernel
/// buffer is empty — common on large Claude Code POSTs and Grok Build streams.
/// The client sees `ECONNRESET` / reqwest `error sending request`. Linux does
/// not inherit; `set_nonblocking(false)` is a no-op there.
fn configure_accepted_socket(stream: &mut TcpStream) -> Result<()> {
    stream.set_nonblocking(false)?;
    // Idle timeouts only (reset on each successful read/write). Do not use
    // short total-request caps — LAN streaming generations can run 30–60+
    // minutes while still producing SSE deltas.
    let idle = Duration::from_secs(3600);
    stream.set_read_timeout(Some(idle))?;
    stream.set_write_timeout(Some(idle))?;
    let _ = stream.set_nodelay(true);
    Ok(())
}

fn handle_client(mut stream: TcpStream, state: AppState) -> Result<()> {
    configure_accepted_socket(&mut stream)?;
    let req = read_request(&mut stream)?;
    let path = req.path.split('?').next().unwrap_or(&req.path).to_string();
    eprintln!("  {} {path}", req.method);

    match (req.method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/health") => {
            let body = health_json(&state)?;
            write_json(&mut stream, 200, &body)?;
        }
        // Local admin API for the native macOS app (loopback only — we only bind 127.0.0.1)
        ("GET", "/spock/v1/config") => {
            handle_admin_get_config(&mut stream, &state)?;
        }
        ("PUT", "/spock/v1/config") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            handle_admin_put_config(&mut stream, &state, body)?;
        }
        ("POST", "/spock/v1/reload") => match state.reload_from_disk() {
            Ok(()) => write_json(
                &mut stream,
                200,
                &json!({"ok": true, "message": "reloaded"}),
            )?,
            Err(e) => write_json(
                &mut stream,
                400,
                &json!({"ok": false, "error": e.to_string()}),
            )?,
        },
        ("POST", "/spock/v1/profile") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            handle_admin_set_profile(&mut stream, &state, body)?;
        }
        ("POST", "/spock/v1/logout") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            handle_admin_logout(&mut stream, &state, body)?;
        }
        ("GET", "/spock/v1/status") => {
            handle_admin_status(&mut stream, &state)?;
        }
        ("GET", p) if p.starts_with("/spock/v1/backends/") && p.ends_with("/models") => {
            // /spock/v1/backends/{name}/models
            handle_admin_backend_models(&mut stream, &state, p)?;
        }
        ("GET", p) if p == "/v1/models" || p.starts_with("/v1/models/") => {
            handle_models(&mut stream, &state, p)?;
        }
        ("GET", p) if p.starts_with("/v1/language-models") => {
            handle_language_models(&mut stream, &state, p)?;
        }
        ("POST", "/v1/messages") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            handle_messages(&mut stream, &state, body, &req.headers)?;
        }
        ("POST", "/v1/messages/count_tokens") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let est = count_tokens_estimate(&body);
            write_json(&mut stream, 200, &json!({"input_tokens": est}))?;
        }
        ("POST", "/v1/chat/completions") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            handle_openai(&mut stream, &state, body, &req.headers)?;
        }
        ("POST", "/v1/responses") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            handle_responses(&mut stream, &state, body)?;
        }
        _ => {
            let (st, err) = anthropic_error(404, "not_found_error", &format!("not found: {path}"));
            write_json(&mut stream, st, &err)?;
        }
    }
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    let mut headers = std::collections::BTreeMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Request {
        method,
        path,
        body,
        headers,
    })
}

fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> Result<()> {
    let data = serde_json::to_vec(body)?;
    let reason = reason_phrase(status);
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&data)?;
    stream.flush()?;
    Ok(())
}

fn write_sse_headers(stream: &mut TcpStream) -> Result<()> {
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    stream.write_all(header.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn emit_sse(stream: &mut TcpStream, event: &str, data: &Value) -> Result<()> {
    let payload = serde_json::to_string(data)?;
    write!(stream, "event: {event}\ndata: {payload}\n\n")?;
    stream.flush()?;
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn health_json(state: &AppState) -> Result<Value> {
    let (profile, port, backends) = state.with_config(|c| {
        (
            c.server.profile.clone(),
            c.port_from_env_or_self(),
            c.backends.keys().cloned().collect::<Vec<_>>(),
        )
    })?;
    Ok(json!({
        "status": "ok",
        "version": crate::config::VERSION,
        "profile": profile,
        "port": port,
        "backends": backends,
        "model": DEFAULT_GROK_MODEL,
    }))
}

/// Clone a backend handle and drop the map lock immediately so long upstream
/// chats (LAN llama-server) cannot block config Save / reload / other writers.
fn take_backend(state: &AppState, name: &str) -> Result<crate::backends::BackendHandle> {
    let backends = state
        .backends
        .read()
        .map_err(|_| Error::Msg("backends lock".into()))?;
    get_backend(&backends, name).cloned()
}

fn handle_admin_get_config(stream: &mut TcpStream, state: &AppState) -> Result<()> {
    let cfg = state.snapshot_config()?;
    let doc = crate::settings::config_to_doc(&cfg);
    let v = serde_json::to_value(&doc)?;
    write_json(stream, 200, &v)
}

fn handle_admin_put_config(stream: &mut TcpStream, state: &AppState, body: Value) -> Result<()> {
    let doc: crate::settings::SettingsDoc = match serde_json::from_value(body) {
        Ok(d) => d,
        Err(e) => {
            return write_json(
                stream,
                400,
                &json!({"ok": false, "error": format!("invalid config json: {e}")}),
            );
        }
    };
    match crate::settings::doc_to_config(&doc) {
        Ok(mut cfg) => {
            // Preserve exact maps not shown in simple UI
            if let Ok(old) = state.snapshot_config() {
                for (name, prof) in cfg.profiles.iter_mut() {
                    if let Some(old_p) = old.profiles.get(name) {
                        if !old_p.exact.is_empty() {
                            prof.exact = old_p.exact.clone();
                        }
                    }
                }
                // Settings form has no kv_sessions checkbox — keep TOML value.
                for (name, be) in cfg.backends.iter_mut() {
                    if let Some(old_b) = old.backends.get(name) {
                        if let (
                            crate::config::BackendConfig::ApiKey { kv_sessions, .. },
                            crate::config::BackendConfig::ApiKey {
                                kv_sessions: keep, ..
                            },
                        ) = (be, old_b)
                        {
                            *kv_sessions = *keep;
                        }
                    }
                }
                // File-only [vision] knobs (prompt, sidecar keys, caption
                // tokens, cache size) are not in the form UI; keep the file's.
                cfg.vision.prompt = old.vision.prompt.clone();
                cfg.vision.sidecar_api_key = old.vision.sidecar_api_key.clone();
                cfg.vision.sidecar_api_key_env = old.vision.sidecar_api_key_env.clone();
                cfg.vision.max_tokens = old.vision.max_tokens;
                cfg.vision.cache_max = old.vision.cache_max;
            }
            match state.apply_and_save(cfg) {
                Ok(()) => write_json(
                    stream,
                    200,
                    &json!({
                        "ok": true,
                        "message": format!("saved · profile {}", doc.server.profile),
                        "config": crate::settings::config_to_doc(&state.snapshot_config()?)
                    }),
                ),
                Err(e) => write_json(stream, 500, &json!({"ok": false, "error": e.to_string()})),
            }
        }
        Err(e) => write_json(stream, 400, &json!({"ok": false, "error": e.to_string()})),
    }
}

fn handle_admin_set_profile(stream: &mut TcpStream, state: &AppState, body: Value) -> Result<()> {
    let name = body
        .get("profile")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return write_json(
            stream,
            400,
            &json!({"ok": false, "error": "missing profile"}),
        );
    }
    let mut cfg = state.snapshot_config()?;
    if let Err(e) = cfg.set_profile(&name) {
        return write_json(stream, 400, &json!({"ok": false, "error": e.to_string()}));
    }
    match state.apply_and_save(cfg) {
        Ok(()) => write_json(stream, 200, &json!({"ok": true, "profile": name})),
        Err(e) => write_json(stream, 500, &json!({"ok": false, "error": e.to_string()})),
    }
}

fn handle_admin_status(stream: &mut TcpStream, state: &AppState) -> Result<()> {
    let cfg = state.snapshot_config()?;
    let mut oauth = serde_json::Map::new();
    for p in crate::oauth::list_providers() {
        let key_set = cfg.oauth_config_key_set(p.id);
        let (present, source) = crate::oauth::status_for_provider(p.id, key_set);
        let mut entry = json!({
            "present": present,
            "source": source.as_str(),
            "label": p.label,
        });
        if let crate::oauth::AuthSource::Oauth {
            expires_at: Some(exp),
        } = source
        {
            entry
                .as_object_mut()
                .unwrap()
                .insert("expires_at".into(), json!(exp));
        }
        oauth.insert(p.id.to_string(), entry);
    }
    let providers: Vec<_> = crate::oauth::list_providers()
        .iter()
        .map(|p| json!({"id": p.id, "label": p.label}))
        .collect();
    write_json(
        stream,
        200,
        &json!({
            "ok": true,
            "version": crate::config::VERSION,
            "profile": cfg.server.profile,
            "port": cfg.port_from_env_or_self(),
            "bind": cfg.server.bind,
            "backends": cfg.backends.keys().cloned().collect::<Vec<_>>(),
            "profiles": cfg.profiles.keys().cloned().collect::<Vec<_>>(),
            "config_path": crate::config::config_path().display().to_string(),
            "oauth": oauth,
            "providers": providers,
            "last_upstream_error": state.last_error_snapshot().map(|e| json!({
                "message": e.message,
                "status": e.status,
                "at_unix": e.at_unix,
            })),
        }),
    )
}

fn handle_admin_logout(stream: &mut TcpStream, state: &AppState, body: Value) -> Result<()> {
    if body.get("all").and_then(|v| v.as_bool()).unwrap_or(false) {
        let cleared = crate::oauth::logout_all()?;
        state.oauth.clear_all_memory();
        return write_json(stream, 200, &json!({"ok": true, "cleared": cleared}));
    }
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(provider) = provider else {
        return write_json(
            stream,
            400,
            &json!({"ok": false, "error": "provider required (or {\"all\": true})"}),
        );
    };
    match crate::oauth::logout(provider) {
        Ok(removed) => {
            state.oauth.clear_memory(provider);
            write_json(
                stream,
                200,
                &json!({"ok": true, "provider": provider, "removed": removed}),
            )
        }
        Err(e) => write_json(stream, 400, &json!({"ok": false, "error": e.to_string()})),
    }
}

/// GET /spock/v1/backends/{name}/models — discover models for Settings pickers.
fn handle_admin_backend_models(stream: &mut TcpStream, state: &AppState, path: &str) -> Result<()> {
    // path: /spock/v1/backends/{name}/models
    let rest = path
        .strip_prefix("/spock/v1/backends/")
        .unwrap_or("")
        .strip_suffix("/models")
        .unwrap_or("");
    let name = urlencoding_decode(rest);
    if name.is_empty() {
        return write_json(
            stream,
            400,
            &json!({"ok": false, "error": "missing backend name"}),
        );
    }

    let be = match take_backend(state, &name) {
        Ok(b) => b,
        Err(e) => {
            return write_json(stream, 404, &json!({"ok": false, "error": e.to_string()}));
        }
    };

    match be.list_models(&state.oauth) {
        Ok(models) => write_json(
            stream,
            200,
            &json!({
                "ok": true,
                "backend": name,
                "kind": be.family_name(),
                "models": models,
                "count": models.len(),
            }),
        ),
        Err(e) => write_json(
            stream,
            502,
            &json!({
                "ok": false,
                "backend": name,
                "error": e.to_string(),
                "models": [],
            }),
        ),
    }
}

fn handle_language_models(stream: &mut TcpStream, state: &AppState, path: &str) -> Result<()> {
    // Prefer xai backend for language-models passthrough
    let be = {
        let backends = state
            .backends
            .read()
            .map_err(|_| Error::Msg("backends lock".into()))?;
        backends
            .get("xai")
            .or_else(|| backends.values().next())
            .cloned()
            .ok_or_else(|| Error::Msg("no backends".into()))?
    };
    let rest = path.strip_prefix("/v1").unwrap_or(path);
    match be.get_json(rest, &state.oauth) {
        Ok(v) => write_json(stream, 200, &v),
        Err(Error::Http(code, body)) => {
            let msg = extract_err_msg(&body);
            let (st, err) = anthropic_error(code, "api_error", &msg);
            write_json(stream, st, &err)
        }
        Err(e) => {
            let (st, err) = anthropic_error(500, "api_error", &e.to_string());
            write_json(stream, st, &err)
        }
    }
}

fn handle_models(stream: &mut TcpStream, state: &AppState, path: &str) -> Result<()> {
    let rest = path[strlen("/v1/models")..].trim_start_matches('/');
    let model_id = urlencoding_decode(rest);

    let env = EnvOverrides::from_env();
    let aliases = alias_models(&env.grok_model, &env.grok_small_model);

    if !model_id.is_empty() {
        // Prefer synthetic for anything we can route; try upstream for real ids
        if let Ok(resolved) = state.with_config(|c| route::resolve(c, &model_id))? {
            if let Ok(be) = take_backend(state, &resolved.backend) {
                let path = format!("/models/{}", resolved.upstream_model);
                if let Ok(mut card) = be.get_json(&path, &state.oauth) {
                    if let Some(obj) = card.as_object_mut() {
                        obj.insert("id".into(), json!(model_id));
                        obj.entry("display_name".to_string())
                            .or_insert_with(|| json!(model_id));
                        obj.entry("model".to_string())
                            .or_insert_with(|| json!(model_id));
                        // Catalog override wins over upstream card for context.
                        if let Some(cw) = state.with_config(|c| {
                            c.catalog
                                .entries
                                .iter()
                                .find(|e| e.id == model_id)
                                .and_then(|e| e.context_window)
                        })? {
                            if cw > 0 {
                                obj.insert("context_window".into(), json!(cw));
                                obj.insert("contextWindow".into(), json!(cw));
                            }
                        }
                    }
                    return write_json(stream, 200, &card);
                }
            }
        }
        // Catalog hit without upstream card.
        if let Some(entry) =
            state.with_config(|c| c.catalog.entries.iter().find(|e| e.id == model_id).cloned())?
        {
            return write_json(
                stream,
                200,
                &model_card_full(
                    &entry.id,
                    "spock-catalog",
                    entry.context_window,
                    entry.name.as_deref(),
                    entry.description.as_deref(),
                    entry.supports_reasoning_effort,
                ),
            );
        }
        return write_json(stream, 200, &model_card(&model_id, "spock"));
    }

    // List: when catalog is non-empty, emit ONLY those entries — no Claude
    // aliases, no bare grok tags, no upstream dump. Grok Build (and any
    // external picker) gets the shortlist as-is. Claude Code does not need
    // aliases in /v1/models; it routes role names via profiles on /v1/messages.
    // Empty catalog keeps the legacy merge (backends + aliases) below.
    let catalog_entries = state.with_config(|c| c.catalog.entries.clone())?;
    if !catalog_entries.is_empty() {
        // Local catalog only. Do not probe backends here — a hung LAN/Ollama
        // `/models` would miss Grok Build's ~5s catalog fetch and empty `/model`.
        let data = catalog_list_cards(&catalog_entries);
        return write_json(
            stream,
            200,
            &json!({
                "object": "list",
                "data": data
            }),
        );
    }

    // No catalog: legacy merge of all backend /models + aliases.
    let mut by_id: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    {
        let handles: Vec<_> = {
            let backends = state
                .backends
                .read()
                .map_err(|_| Error::Msg("backends lock".into()))?;
            backends.values().cloned().collect()
        };
        for be in &handles {
            if let Ok(raw) = be.get_json("/models", &state.oauth) {
                if let Some(data) = raw.get("data").and_then(|d| d.as_array()) {
                    for m in data {
                        if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                            by_id.insert(id.to_string(), m.clone());
                        }
                    }
                }
            }
        }
    }
    for card in aliases {
        if let Some(id) = card.get("id").and_then(|i| i.as_str()) {
            by_id.insert(id.to_string(), card);
        }
    }
    write_json(
        stream,
        200,
        &json!({
            "object": "list",
            "data": by_id.into_values().collect::<Vec<_>>()
        }),
    )
}

/// grok-build `web_search` POSTs OpenAI Responses (`{base}/responses` + hosted
/// `web_search` tool). Search-only: run `[web_search]`, return a completed
/// Responses object. Anything else is 400 — this is not a general Responses
/// proxy and must not fall through to chat/completions.
fn handle_responses(sock: &mut TcpStream, state: &AppState, body: Value) -> Result<()> {
    let web_cfg = {
        let c = state.snapshot_config()?;
        crate::server_tools::WebSearchConfig {
            enabled: c.web_search.enabled,
            provider: c.web_search.provider.clone(),
            base_url: c.web_search.base_url.clone(),
            api_key: c.web_search.api_key.clone(),
            api_key_env: c.web_search.api_key_env.clone(),
            max_results: c.web_search.max_results,
        }
    };
    match crate::server_tools::responses_web_search(&web_cfg, &body) {
        Ok(out) => {
            let q = crate::server_tools::responses_query(&body).unwrap_or("");
            eprintln!(
                "  responses web_search q={q:?} provider={}",
                web_cfg.provider
            );
            write_json(sock, 200, &out)
        }
        Err(e) => {
            let msg = e.to_string();
            eprintln!("  responses error: {msg}");
            let (st, ty) = if msg.contains("disabled")
                || msg.contains("search-only")
                || msg.contains("empty input")
            {
                (400, "invalid_request_error")
            } else {
                (502, "api_error")
            };
            let (http, err) = anthropic_error(st, ty, &msg);
            write_json(sock, http, &err)
        }
    }
}

/// Strip/caption image content for text-only backends before the request
/// reaches the backend (anthropic passthrough included). Never fails the
/// request: sidecar errors degrade to strip inside vision::apply_*.
fn apply_vision_policy(
    state: &AppState,
    body: &mut Value,
    backend_flag: bool,
    upstream_model: &str,
    anthropic_shape: bool,
) -> Result<()> {
    let model_flag = crate::translate::is_text_only_model(upstream_model);
    if !backend_flag && !model_flag {
        return Ok(());
    }
    let vision = state.with_config(|c| c.vision.clone())?;
    let action = crate::vision::decide(backend_flag, model_flag, &vision);
    let note = if backend_flag {
        "[image omitted: this backend is text-only]".to_string()
    } else {
        crate::translate::image_omitted_note(1)
    };
    let handled = if anthropic_shape {
        crate::vision::apply_anthropic(body, action, &note, &vision, &state.vision_cache)
    } else {
        crate::vision::apply_openai(body, action, &note, &vision, &state.vision_cache)
    };
    if handled > 0 {
        eprintln!(
            "  vision: {handled} image(s) {} for text-only {upstream_model}",
            if matches!(action, crate::vision::VisionAction::Describe) {
                "described/stripped"
            } else {
                "stripped"
            }
        );
    }
    Ok(())
}

fn handle_messages(
    sock: &mut TcpStream,
    state: &AppState,
    mut a: Value,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let client_model = a
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(DEFAULT_GROK_MODEL)
        .to_string();
    let resolved = match state.with_config(|c| route::resolve(c, &client_model))? {
        Ok(r) => r,
        Err(e) => {
            let (st, err) = anthropic_error(400, "invalid_request_error", &e.to_string());
            return write_json(sock, st, &err);
        }
    };

    let be = match take_backend(state, &resolved.backend) {
        Ok(b) => b,
        Err(e) => {
            let (st, err) = anthropic_error(400, "invalid_request_error", &e.to_string());
            return write_json(sock, st, &err);
        }
    };

    // Text-only backends: strip/caption images before any branch, so the
    // anthropic passthrough and the kv_sessions path are covered too.
    apply_vision_policy(
        state,
        &mut a,
        be.config.text_only(),
        &resolved.upstream_model,
        true,
    )?;

    let env = EnvOverrides::from_env();
    // Anthropic passthrough: forward Messages JSON with only model rewritten to upstream id.
    // Keep betas / context_management — real Anthropic understands them.
    if be.is_anthropic() {
        let mut body = a.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), json!(resolved.upstream_model));
        }
        let do_stream = a.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        eprintln!(
            "  route {} → {}:{} (anthropic-passthrough)",
            client_model, resolved.backend, resolved.upstream_model
        );
        return match be.chat(&body, do_stream, &state.oauth) {
            Ok(UpstreamBody::Json(o)) => write_json(sock, 200, &o),
            Ok(UpstreamBody::Stream(reader)) => {
                // Raw Anthropic SSE passthrough
                write_sse_headers(sock)?;
                pump_upstream_sse(sock, state, reader, "anthropic")
            }
            Err(e) => write_upstream_err(sock, state, e),
        };
    }

    // Microcompact + drop Anthropic-only keys before OpenAI-compat / xAI translation.
    prepare_for_openai_compat(&mut a);

    if be.config.kv_sessions() {
        return handle_kv_sessions(sock, state, &a, headers, &resolved, &be, false);
    }

    let oai = anthropic_to_openai(&a, &resolved.upstream_model, be.quirk, &env.grok_model);
    let include_thinking = wants_thinking(&a);
    let do_stream = a.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let advisor_cfg = {
        let c = state.snapshot_config()?;
        crate::server_tools::AdvisorConfig {
            enabled: c.advisor.enabled,
            model: c.advisor.model.clone(),
            max_tokens: c.advisor.max_tokens,
        }
    };
    let web_cfg = {
        let c = state.snapshot_config()?;
        crate::server_tools::WebSearchConfig {
            enabled: c.web_search.enabled,
            provider: c.web_search.provider.clone(),
            base_url: c.web_search.base_url.clone(),
            api_key: c.web_search.api_key.clone(),
            api_key_env: c.web_search.api_key_env.clone(),
            max_results: c.web_search.max_results,
        }
    };
    let use_server_tools = (advisor_cfg.enabled && crate::server_tools::request_has_advisor(&a))
        || (web_cfg.enabled && crate::server_tools::request_has_web_search(&a));

    // Server-tools MUST run for stream clients too. Claude Code WebSearch is a
    // client tool that opens a *nested* streaming Messages call with
    // tools:[{type:web_search_20250305}]. If we skip emulation on stream, the
    // schema is stripped, the model never searches, and WebSearch returns empty.
    // Upstream chat inside the loop stays non-stream; we still speak SSE to the
    // client, with keepalives so slow LAN rounds don't trip client idle timeouts.

    eprintln!(
        "  route {} → {}:{} ({}){}",
        client_model,
        resolved.backend,
        resolved.upstream_model,
        match be.quirk {
            CompletionsQuirk::Xai => "xai",
            CompletionsQuirk::Kimi => "kimi",
            CompletionsQuirk::Generic => "generic",
        },
        if use_server_tools && do_stream {
            " [server-tools+stream]"
        } else if use_server_tools {
            " [server-tools]"
        } else if do_stream {
            " [stream]"
        } else {
            ""
        }
    );

    // Server-tool emulation: multi-round non-stream upstream loop, then one
    // Anthropic JSON (or synthetic SSE of that JSON for stream clients).
    if use_server_tools {
        if do_stream {
            return run_server_tools_streaming(
                sock,
                state,
                &a,
                oai,
                &client_model,
                include_thinking,
                &advisor_cfg,
                &web_cfg,
                &be,
                &env,
            );
        }
        match crate::server_tools::run_with_server_tools(
            state,
            &a,
            oai,
            &client_model,
            include_thinking,
            &advisor_cfg,
            &web_cfg,
            &be,
            &env,
        ) {
            Ok(resp) => write_json(sock, 200, &resp),
            Err(e) => write_upstream_err(sock, state, e),
        }
    } else if !do_stream {
        // fall through handled below — keep structure with early return above only for server tools stream
        match be.chat(&oai, false, &state.oauth) {
            Ok(UpstreamBody::Json(o)) => {
                let resp = openai_to_anthropic(&o, &client_model, include_thinking);
                write_json(sock, 200, &resp)
            }
            Ok(UpstreamBody::Stream(_)) => {
                let (st, err) = anthropic_error(500, "api_error", "unexpected stream");
                write_json(sock, st, &err)
            }
            Err(e) => write_upstream_err(sock, state, e),
        }
    } else {
        match be.chat(&oai, true, &state.oauth) {
            Ok(UpstreamBody::Stream(reader)) => {
                let input_estimate = count_tokens_estimate(&a);
                stream_anthropic(
                    sock,
                    reader,
                    &client_model,
                    include_thinking,
                    input_estimate,
                    state,
                )
            }
            Ok(UpstreamBody::Json(o)) => {
                let resp = openai_to_anthropic(&o, &client_model, include_thinking);
                write_json(sock, 200, &resp)
            }
            Err(e) => write_upstream_err(sock, state, e),
        }
    }
}

fn handle_kv_sessions(
    sock: &mut TcpStream,
    state: &AppState,
    a: &Value,
    headers: &std::collections::BTreeMap<String, String>,
    resolved: &route::ResolvedRoute,
    be: &crate::backends::BackendHandle,
    openai_wire: bool,
) -> Result<()> {
    let mut work = a.clone();
    let hint = crate::kv_sessions::take_session_hint(&mut work, headers);
    let include_thinking = wants_thinking(a);
    let do_stream = a.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let n_predict = a.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(1024);
    eprintln!(
        "  route {} → {}:{} (kv-sessions native /fork parent={:?} session={:?})",
        resolved.client_model,
        resolved.backend,
        resolved.upstream_model,
        hint.parent_session_id,
        hint.session_id
    );
    match crate::kv_sessions::run_turn(state, be, &work, &hint, do_stream, n_predict) {
        Ok((_master, UpstreamBody::Json(o))) => {
            if openai_wire {
                write_json(sock, 200, &o)
            } else {
                let resp = openai_to_anthropic(&o, &resolved.client_model, include_thinking);
                write_json(sock, 200, &resp)
            }
        }
        Ok((_master, UpstreamBody::Stream(reader))) => {
            if openai_wire {
                write_sse_headers(sock)?;
                pump_upstream_sse(sock, state, reader, "kv-sessions")
            } else {
                let input_estimate = count_tokens_estimate(a);
                stream_anthropic(
                    sock,
                    reader,
                    &resolved.client_model,
                    include_thinking,
                    input_estimate,
                    state,
                )
            }
        }
        Err(e) => write_upstream_err(sock, state, e),
    }
}

/// Stream-client path for advisor/web_search: keepalive SSE while the multi-round
/// upstream loop runs, then emit the final Anthropic message as SSE events.
#[allow(clippy::too_many_arguments)]
fn run_server_tools_streaming(
    sock: &mut TcpStream,
    state: &AppState,
    a: &Value,
    oai: Value,
    client_model: &str,
    include_thinking: bool,
    advisor_cfg: &crate::server_tools::AdvisorConfig,
    web_cfg: &crate::server_tools::WebSearchConfig,
    be: &crate::backends::BackendHandle,
    env: &EnvOverrides,
) -> Result<()> {
    write_sse_headers(sock)?;

    // Background keepalives so Claude Code does not idle-timeout during long
    // LAN rounds (search + model thinking with stream:false upstream).
    let mut keepalive = sock.try_clone().ok();
    if let Some(ref mut ks) = keepalive {
        let _ = ks.set_write_timeout(Some(Duration::from_secs(5)));
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = stop.clone();
    let ping = std::thread::spawn(move || {
        while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(12));
            if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            if let Some(ref mut ks) = keepalive {
                // SSE comment — ignored by clients, resets idle timers.
                if write!(ks, ": spock-keepalive\n\n").is_err() || ks.flush().is_err() {
                    break;
                }
            }
        }
    });

    let result = crate::server_tools::run_with_server_tools(
        state,
        a,
        oai,
        client_model,
        include_thinking,
        advisor_cfg,
        web_cfg,
        be,
        env,
    );

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = ping.join();

    match result {
        Ok(resp) => stream_json_as_anthropic_sse_body(sock, &resp),
        Err(e) => {
            let msg = format!("spock server_tools: {e}");
            state.record_upstream_error(502, &msg);
            let (_st, err_body) = anthropic_error(502, "api_error", &msg);
            let _ = emit_sse(sock, "error", &err_body);
            let _ = emit_sse(sock, "message_stop", &json!({"type": "message_stop"}));
            Ok(())
        }
    }
}

/// Emit a completed Anthropic message as SSE events (for server-tool multi-round results).
/// Writes response headers first.
#[allow(dead_code)] // kept for non-keepalive callers / tests
fn stream_json_as_anthropic_sse(stream: &mut TcpStream, resp: &Value) -> Result<()> {
    write_sse_headers(stream)?;
    stream_json_as_anthropic_sse_body(stream, resp)
}

/// Body-only SSE emission — headers already written (e.g. keepalive path).
fn stream_json_as_anthropic_sse_body(stream: &mut TcpStream, resp: &Value) -> Result<()> {
    let msg_id = resp
        .get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(new_msg_id);
    let model = resp
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("spock");
    let start_usage = resp
        .get("usage")
        .cloned()
        .unwrap_or(json!({"input_tokens": 0, "output_tokens": 0}));
    emit_sse(
        stream,
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": start_usage
            }
        }),
    )?;
    let blocks = resp
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    for (i, block) in blocks.iter().enumerate() {
        let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        // Anthropic streaming clients (Claude Code) expect tool_use / server_tool_use
        // to start with empty input and receive arguments via input_json_delta.
        // Dumping the full input only in content_block_start leaves input={} →
        // "missing parameter" on every client tool when advisor/web_search force
        // this synthetic-SSE path.
        let start_block = match kind {
            "tool_use" | "server_tool_use" => {
                let mut b = block.clone();
                if let Some(obj) = b.as_object_mut() {
                    obj.insert("input".into(), json!({}));
                }
                b
            }
            _ => block.clone(),
        };
        emit_sse(
            stream,
            "content_block_start",
            &json!({"type":"content_block_start","index": i, "content_block": start_block}),
        )?;
        match kind {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        emit_sse(
                            stream,
                            "content_block_delta",
                            &json!({
                                "type":"content_block_delta",
                                "index": i,
                                "delta": {"type":"text_delta","text": text}
                            }),
                        )?;
                    }
                }
            }
            "thinking" => {
                if let Some(th) = block.get("thinking").and_then(|t| t.as_str()) {
                    if !th.is_empty() {
                        emit_sse(
                            stream,
                            "content_block_delta",
                            &json!({
                                "type":"content_block_delta",
                                "index": i,
                                "delta": {"type":"thinking_delta","thinking": th}
                            }),
                        )?;
                    }
                }
            }
            "tool_use" | "server_tool_use" => {
                let input = block.get("input").cloned().unwrap_or(json!({}));
                let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                if partial != "{}" {
                    emit_sse(
                        stream,
                        "content_block_delta",
                        &json!({
                            "type":"content_block_delta",
                            "index": i,
                            "delta": {"type":"input_json_delta","partial_json": partial}
                        }),
                    )?;
                }
            }
            // advisor_tool_result / web_search_tool_result / other full blocks:
            // start already carried the whole payload; no delta needed.
            _ => {}
        }
        emit_sse(
            stream,
            "content_block_stop",
            &json!({"type":"content_block_stop","index": i}),
        )?;
    }
    let stop = resp
        .get("stop_reason")
        .cloned()
        .unwrap_or(json!("end_turn"));
    let usage = resp
        .get("usage")
        .cloned()
        .unwrap_or(json!({"input_tokens":0,"output_tokens":0}));
    emit_sse(
        stream,
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop, "stop_sequence": null},
            "usage": usage
        }),
    )?;
    emit_sse(stream, "message_stop", &json!({"type": "message_stop"}))?;
    Ok(())
}

fn stream_anthropic(
    stream: &mut TcpStream,
    reader: Box<dyn Read + Send>,
    req_model: &str,
    include_thinking: bool,
    input_estimate: u64,
    state: &AppState,
) -> Result<()> {
    write_sse_headers(stream)?;
    let msg_id = new_msg_id();
    emit_sse(
        stream,
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": req_model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        }),
    )?;

    let mut block: Option<&str> = None;
    let mut index: i64 = -1;
    let mut chunks_out: u64 = 0;
    let mut finish: Option<String> = None;
    let mut usage = json!({});
    let mut mid_stream_err: Option<String> = None;
    // OpenAI tool_calls[].index → Anthropic content block index.
    // Qwen (and others) stream args across many deltas with empty id after the first
    // chunk. Opening a new tool_use on every id/name presence fragments one Bash call
    // into N broken blocks (Claude Code then runs empty/invalid commands).
    let mut tool_block_by_tc_index: std::collections::HashMap<i64, i64> =
        std::collections::HashMap::new();

    let buffered = BufReader::new(reader);
    for line in buffered.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                mid_stream_err = Some(format!("stream read error: {e}"));
                break;
            }
        };
        let line = line.trim();
        if !line.starts_with("data:") {
            continue;
        }
        let payload = line[5..].trim();
        if payload == "[DONE]" {
            break;
        }
        let chunk: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Some OpenAI-compat servers emit error objects mid-SSE after 200 headers.
        if let Some(err) = chunk.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| err.to_string());
            mid_stream_err = Some(label_mid_stream_upstream_error(&msg));
            break;
        }

        if let Some(u) = chunk.get("usage") {
            // Providers send "usage": null on every content chunk; keep the real one.
            if u.is_object() {
                usage = u.clone();
            }
        }
        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(json!({}));
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            finish = Some(fr.to_string());
        }
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));

        if include_thinking {
            if let Some(reasoning) = delta.get("reasoning_content").and_then(|t| t.as_str()) {
                if !reasoning.is_empty() {
                    if block != Some("thinking") {
                        if block.is_some() {
                            emit_sse(
                                stream,
                                "content_block_stop",
                                &json!({"type":"content_block_stop","index": index}),
                            )?;
                        }
                        index += 1;
                        block = Some("thinking");
                        emit_sse(
                            stream,
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": index,
                                "content_block": {"type": "thinking", "thinking": ""}
                            }),
                        )?;
                    }
                    emit_sse(
                        stream,
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "thinking_delta", "thinking": reasoning}
                        }),
                    )?;
                    chunks_out += 1;
                }
            }
        }

        if let Some(text) = delta.get("content").and_then(|t| t.as_str()) {
            if !text.is_empty() {
                if block != Some("text") {
                    if block.is_some() {
                        emit_sse(
                            stream,
                            "content_block_stop",
                            &json!({"type":"content_block_stop","index": index}),
                        )?;
                    }
                    index += 1;
                    block = Some("text");
                    emit_sse(
                        stream,
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    )?;
                }
                emit_sse(
                    stream,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                )?;
                chunks_out += 1;
            }
        }

        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                let fn_ = tc.get("function").cloned().unwrap_or(json!({}));
                let tc_index = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let id_raw = tc.get("id").and_then(|t| t.as_str()).unwrap_or("");
                let name_raw = fn_.get("name").and_then(|n| n.as_str()).unwrap_or("");
                // Start a new Anthropic tool_use block only once per OpenAI tool index.
                // Later deltas for the same index only carry argument fragments (often
                // with id:"" / no name) — must NOT open another block.
                let need_start = !tool_block_by_tc_index.contains_key(&tc_index)
                    && (!id_raw.is_empty() || !name_raw.is_empty());
                if need_start {
                    if block.is_some() {
                        emit_sse(
                            stream,
                            "content_block_stop",
                            &json!({"type":"content_block_stop","index": index}),
                        )?;
                    }
                    index += 1;
                    block = Some("tool");
                    tool_block_by_tc_index.insert(tc_index, index);
                    let id = if id_raw.is_empty() {
                        new_tool_id()
                    } else {
                        id_raw.to_string()
                    };
                    emit_sse(
                        stream,
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {
                                "type": "tool_use",
                                "id": id,
                                "name": name_raw,
                                "input": {}
                            }
                        }),
                    )?;
                }
                let Some(&block_idx) = tool_block_by_tc_index.get(&tc_index) else {
                    // Argument fragment before we ever saw id/name for this index —
                    // shouldn't happen on well-formed streams; skip rather than invent.
                    continue;
                };
                if let Some(args) = fn_.get("arguments").and_then(|a| a.as_str()) {
                    if !args.is_empty() {
                        emit_sse(
                            stream,
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": block_idx,
                                "delta": {"type": "input_json_delta", "partial_json": args}
                            }),
                        )?;
                        chunks_out += 1;
                    }
                }
            }
        }
    }

    if let Some(err_msg) = mid_stream_err {
        // Close open content block, then emit Anthropic error event so the IDE
        // shows a real failure instead of a silent truncated stream.
        if block.is_some() {
            let _ = emit_sse(
                stream,
                "content_block_stop",
                &json!({"type":"content_block_stop","index": index}),
            );
        }
        state.record_upstream_error(502, &err_msg);
        let (_st, err_body) = anthropic_error(502, "api_error", &format!("Spock {err_msg}"));
        let _ = emit_sse(stream, "error", &err_body);
        let _ = emit_sse(stream, "message_stop", &json!({"type": "message_stop"}));
        eprintln!("  mid-SSE error: {err_msg}");
        return Ok(());
    }

    if block.is_some() {
        emit_sse(
            stream,
            "content_block_stop",
            &json!({"type":"content_block_stop","index": index}),
        )?;
    }
    let out_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(chunks_out);
    // Claude Code's footer gauge and auto-compact trigger read
    // input_tokens + cache_* from the last assistant message's usage (merged
    // from message_delta). Zero here = gauge hidden, context silently overflows.
    let in_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(input_estimate);
    let mut delta_usage = json!({"output_tokens": out_tokens});
    if in_tokens > 0 {
        delta_usage["input_tokens"] = json!(in_tokens);
    }
    if let Some(cached) = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
    {
        delta_usage["cache_read_input_tokens"] = json!(cached);
    }
    emit_sse(
        stream,
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason(finish.as_deref()),
                "stop_sequence": null
            },
            "usage": delta_usage
        }),
    )?;
    emit_sse(stream, "message_stop", &json!({"type": "message_stop"}))?;
    Ok(())
}

fn handle_openai(
    sock: &mut TcpStream,
    state: &AppState,
    mut body: Value,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let client_model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(DEFAULT_GROK_MODEL)
        .to_string();
    let resolved = match state.with_config(|c| route::resolve(c, &client_model))? {
        Ok(r) => r,
        Err(e) => {
            let (st, err) = anthropic_error(400, "invalid_request_error", &e.to_string());
            return write_json(sock, st, &err);
        }
    };
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), json!(resolved.upstream_model));
    }

    let be = match take_backend(state, &resolved.backend) {
        Ok(b) => b,
        Err(e) => {
            let (st, err) = anthropic_error(400, "invalid_request_error", &e.to_string());
            return write_json(sock, st, &err);
        }
    };

    // Text-only backends: the OpenAI ingress forwards bodies verbatim, so
    // image_url parts must be rewritten here — there is no later translate.
    apply_vision_policy(
        state,
        &mut body,
        be.config.text_only(),
        &resolved.upstream_model,
        false,
    )?;

    if be.config.kv_sessions() {
        // OpenAI-shaped body: wrap as a fake Anthropic request so the same
        // native park/fork path runs. Fail loud — never chat/completions.
        let mut fake = json!({
            "model": resolved.upstream_model,
            "messages": body.get("messages").cloned().unwrap_or(json!([])),
            "max_tokens": body.get("max_tokens").cloned().unwrap_or(json!(1024)),
            "stream": body.get("stream").cloned().unwrap_or(json!(false)),
        });
        if let Some(sys) = body.get("system") {
            fake["system"] = sys.clone();
        }
        if let Some(tools) = body.get("tools") {
            fake["tools"] = tools.clone();
        }
        if let Some(obj) = body.as_object() {
            for k in [
                "session_id",
                "parent_session_id",
                "close_session",
                "temperature",
                "top_p",
                "top_k",
                "min_p",
                "repeat_penalty",
                "seed",
                "stop",
                "stop_sequences",
            ] {
                if let Some(v) = obj.get(k) {
                    fake[k] = v.clone();
                }
            }
        }
        return handle_kv_sessions(sock, state, &fake, headers, &resolved, &be, true);
    }

    if be.quirk == CompletionsQuirk::Xai {
        let env = EnvOverrides::from_env();
        let reasoning =
            crate::models::is_reasoning_model(&resolved.upstream_model, &env.grok_model);
        crate::models::sanitize_upstream(&mut body, reasoning);
    } else if matches!(be.quirk, CompletionsQuirk::Kimi | CompletionsQuirk::Generic) {
        // Never forward reasoning_effort "none" (some OpenAI-compat servers 400).
        if let Some(obj) = body.as_object_mut() {
            if obj.get("reasoning_effort").and_then(|v| v.as_str()) == Some("none") {
                obj.remove("reasoning_effort");
            }
        }
    }

    let stream_flag = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    eprintln!(
        "  route {} → {}:{} ({}) [openai{}]",
        client_model,
        resolved.backend,
        resolved.upstream_model,
        match be.quirk {
            CompletionsQuirk::Xai => "xai",
            CompletionsQuirk::Kimi => "kimi",
            CompletionsQuirk::Generic => "generic",
        },
        if stream_flag { "+stream" } else { "" }
    );
    match be.chat(&body, stream_flag, &state.oauth) {
        Ok(UpstreamBody::Json(v)) => write_json(sock, 200, &v),
        Ok(UpstreamBody::Stream(reader)) => {
            write_sse_headers(sock)?;
            pump_upstream_sse(sock, state, reader, "openai")
        }
        Err(e) => write_upstream_err(sock, state, e),
    }
}

/// Copy an upstream SSE body to the client. A silent `Err(_) => break` here
/// is what Grok Build reports as `reqwest error stream: error sending request`
/// — the socket just dies with no error event.
fn pump_upstream_sse(
    sock: &mut TcpStream,
    state: &AppState,
    mut reader: Box<dyn Read + Send>,
    label: &str,
) -> Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                sock.write_all(&buf[..n])?;
                sock.flush()?;
            }
            Err(e) => {
                let msg = format!("spock {label} stream: {e}");
                eprintln!("  {msg}");
                state.record_upstream_error(502, &msg);
                let payload = json!({"error": {"message": msg, "type": "api_error"}});
                if let Ok(s) = serde_json::to_string(&payload) {
                    let _ = write!(sock, "data: {s}\n\n");
                    let _ = sock.flush();
                }
                break;
            }
        }
    }
    Ok(())
}

fn write_upstream_err(stream: &mut TcpStream, state: &AppState, e: Error) -> Result<()> {
    match e {
        Error::Http(400, body) => {
            let raw = extract_err_msg(&body);
            let looks_kv = raw.contains("session")
                || raw.contains("kv_sessions")
                || raw.contains("/fork")
                || raw.contains("close_session");
            if looks_kv {
                // Unknown session / bad native schema: pass 400. Do not remap to 502
                // and do not retry cold.
                let msg = format!("Spock kv_sessions 400: {raw}");
                state.record_upstream_error(400, &msg);
                let (st, err) = anthropic_error(400, "invalid_request_error", &msg);
                write_json(stream, st, &err)
            } else {
                let (out_status, err_type, msg) = classify_upstream_http(400, &raw);
                state.record_upstream_error(out_status, &msg);
                let (st, err) = anthropic_error(out_status, err_type, &msg);
                write_json(stream, st, &err)
            }
        }
        Error::Http(code, body) => {
            let raw = extract_err_msg(&body);
            let (out_status, err_type, msg) = classify_upstream_http(code, &raw);
            state.record_upstream_error(out_status, &msg);
            let (st, err) = anthropic_error(out_status, err_type, &msg);
            write_json(stream, st, &err)
        }
        other => {
            let raw = other.to_string();
            let status = if raw.contains("named master")
                || raw.contains("cache_control")
                || raw.contains("session_id")
            {
                400
            } else {
                502
            };
            let msg = format!("spock backend error: {raw}");
            state.record_upstream_error(status, &msg);
            let err_type = if status == 400 {
                "invalid_request_error"
            } else {
                "api_error"
            };
            let (st, err) = anthropic_error(status, err_type, &msg);
            write_json(stream, st, &err)
        }
    }
}

/// Label mid-SSE `error` objects from OpenAI-compat backends so Claude Code / status
/// toasts don't look like Spock itself aborted the stream.
///
/// llama.cpp `common_chat_msg_diff::compute_diffs` throws
/// `"Invalid diff: now finding less tool calls!"` when a model retracts a partial
/// tool_call mid-stream (common on damaged quants). That is upstream — Spock only
/// forwards it.
fn label_mid_stream_upstream_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("invalid diff")
        || lower.contains("finding less tool calls")
        || lower.contains("tool call mismatch")
    {
        return format!("upstream stream error [llama-server tool-call parser, not Spock]: {raw}");
    }
    format!("upstream stream error: {raw}")
}

/// Map vendor HTTP failures to Anthropic-shaped errors that Claude Code (CLI + VSCodium)
/// will **show as text**, not misread as "log in to Anthropic".
///
/// xAI subscription/credit exhaustion often arrives as 403/402/429 with bodies mentioning
/// quota, credits, usage, or SuperGrok. Bare 401 opens the Anthropic login modal in the IDE —
/// always rewrite those. Prefer 502 + a loud message for quota so the IDE surfaces it the
/// same way the CLI status path reports config keys.
pub(crate) fn classify_upstream_http(code: u16, raw: &str) -> (u16, &'static str, String) {
    let lower = raw.to_ascii_lowercase();
    let looks_quota = code == 402
        || lower.contains("quota")
        || lower.contains("credit")
        || lower.contains("billing")
        || lower.contains("usage limit")
        || lower.contains("usage_limit")
        || lower.contains("exceeded")
        || lower.contains("out of")
        || lower.contains("insufficient")
        || lower.contains("payment")
        || lower.contains("supergrok")
        || lower.contains("spend limit")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("resource_exhausted");

    // Never look like bare Anthropic auth — clients open "login to Anthropic" on 401.
    if code == 401 {
        return (
            502,
            "authentication_error",
            format!(
                "Spock upstream 401 (NOT Anthropic login): {raw} — run: spock login <provider> (or set api_key on that oauth backend); Ollama cloud: sign in / plan"
            ),
        );
    }

    if looks_quota || code == 402 || code == 403 || code == 429 {
        let kind = if code == 429 && !looks_quota {
            "rate_limit"
        } else if code == 429 {
            "rate_limit_or_quota"
        } else {
            "quota_or_billing"
        };
        return (
            502,
            // rate_limit_error is recognized by Claude Code; keeps IDE from auth-modal path.
            "rate_limit_error",
            format!(
                "Spock upstream {code} ({kind}): {raw} — subscription/credits/rate limit on the **backend** (xAI / Ollama / etc.), not Anthropic. Add credits, wait, or switch Spock profile/model."
            ),
        );
    }

    (
        if (500..600).contains(&code) {
            code
        } else {
            502
        },
        "api_error",
        format!("Spock upstream {code}: {raw}"),
    )
}

fn extract_err_msg(body: &Value) -> String {
    if let Some(err) = body.get("error") {
        if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
            return m.to_string();
        }
        return err.to_string();
    }
    body.to_string()
}

fn strlen(s: &str) -> usize {
    s.len()
}

fn urlencoding_decode(s: &str) -> String {
    // minimal: replace %XX and +
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v as char);
                    i += 3;
                } else {
                    out.push('%');
                    i += 1;
                }
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod upstream_err_tests {
    use super::{classify_upstream_http, label_mid_stream_upstream_error};

    #[test]
    fn llama_tool_call_diff_is_labeled_upstream() {
        let msg = label_mid_stream_upstream_error(
            "Invalid diff: now finding less tool calls!\n  Previous (1):\n",
        );
        assert!(msg.contains("llama-server tool-call parser"), "{msg}");
        assert!(msg.contains("not Spock"), "{msg}");
        assert!(msg.contains("Invalid diff"), "{msg}");
    }

    #[test]
    fn generic_mid_stream_error_unchanged_prefix() {
        let msg = label_mid_stream_upstream_error("connection reset by peer");
        assert_eq!(msg, "upstream stream error: connection reset by peer");
    }

    #[test]
    fn quota_body_becomes_loud_502() {
        let (st, ty, msg) = classify_upstream_http(
            403,
            "You have exceeded your SuperGrok usage limit. Please add credits.",
        );
        assert_eq!(st, 502);
        assert_eq!(ty, "rate_limit_error");
        assert!(msg.contains("quota_or_billing"), "{msg}");
        assert!(
            msg.contains("NOT Anthropic") || msg.contains("not Anthropic"),
            "{msg}"
        );
        assert!(
            msg.contains("SuperGrok") || msg.contains("credits"),
            "{msg}"
        );
    }

    #[test]
    fn plain_401_not_anthropic_login() {
        let (st, ty, msg) = classify_upstream_http(401, "Invalid API key");
        assert_eq!(st, 502);
        assert_eq!(ty, "authentication_error");
        assert!(msg.contains("NOT Anthropic login"), "{msg}");
    }

    #[test]
    fn payment_required_402() {
        let (st, ty, msg) = classify_upstream_http(402, "Payment Required");
        assert_eq!(st, 502);
        assert_eq!(ty, "rate_limit_error");
        assert!(msg.contains("402"), "{msg}");
    }

    #[test]
    fn generic_500_passthrough_status() {
        let (st, ty, msg) = classify_upstream_http(503, "backend down");
        assert_eq!(st, 503);
        assert_eq!(ty, "api_error");
        assert!(msg.contains("503"), "{msg}");
    }

    #[test]
    fn rate_limit_429() {
        let (st, ty, msg) = classify_upstream_http(429, "Too Many Requests");
        assert_eq!(st, 502);
        assert_eq!(ty, "rate_limit_error");
        assert!(msg.contains("429"), "{msg}");
    }
}

/// Darwin `accept()` copies `O_NONBLOCK` from the listen socket. A body that
/// arrives after headers (Claude Code / Grok Build) then `read_exact`s into
/// WouldBlock and the client sees ECONNRESET / reqwest "error sending request".
#[cfg(test)]
mod accepted_socket_tests {
    use super::{configure_accepted_socket, read_request};
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    fn bind_nonblocking() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("listen nonblock");
        let port = listener.local_addr().expect("addr").port();
        (listener, port)
    }

    fn accept_one(listener: &TcpListener) -> TcpStream {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((s, _)) => return s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        panic!("accept timeout");
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("accept: {e}"),
            }
        }
    }

    fn delayed_post(configure: bool) -> std::result::Result<super::Request, crate::error::Error> {
        let (listener, port) = bind_nonblocking();
        let body = b"{\"model\":\"x\"}";
        let client = thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            let head = format!(
                "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            c.write_all(head.as_bytes()).expect("headers");
            c.flush().ok();
            // Gap that used to trip inherited O_NONBLOCK + read_exact.
            thread::sleep(Duration::from_millis(80));
            c.write_all(body).expect("body");
            c.flush().ok();
            thread::sleep(Duration::from_millis(80));
        });
        let mut server = accept_one(&listener);
        if configure {
            configure_accepted_socket(&mut server).expect("configure");
        }
        let req = read_request(&mut server);
        let _ = client.join();
        req
    }

    /// Control: inherited O_NONBLOCK + delayed body must fail, or the Darwin
    /// inherit theory is dead and this patch is the wrong first move.
    /// Darwin-only: Linux `accept()` does not inherit O_NONBLOCK, so the
    /// unconfigured read legitimately succeeds there and the control's
    /// premise is inverted.
    #[cfg(target_os = "macos")]
    #[test]
    fn delayed_post_body_fails_without_configure() {
        match delayed_post(false) {
            Err(crate::error::Error::Io(io)) => assert_eq!(
                io.kind(),
                std::io::ErrorKind::WouldBlock,
                "expected WouldBlock, got {io}"
            ),
            Err(other) => panic!("expected Io(WouldBlock), got {other}"),
            Ok(_) => panic!("inherit theory: delayed body must EAGAIN without configure"),
        }
    }

    #[test]
    fn delayed_post_body_is_fully_read() {
        let req = delayed_post(true).expect("read_request");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/messages");
        assert_eq!(req.body, b"{\"model\":\"x\"}");
    }
}

#[cfg(test)]
mod vision_policy_e2e {
    use crate::config::Config;
    use crate::state::AppState;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    }

    /// Read one request (headers + content-length body) and return the bytes.
    fn read_one_request(mut s: TcpStream) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            match s.read(&mut tmp) {
                Ok(0) => return buf,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                }
                Err(_) => return buf,
            }
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let len = headers
            .lines()
            .find_map(|l| {
                let l = l.to_ascii_lowercase();
                l.strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        while buf.len() < header_end + len {
            match s.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        buf
    }

    /// The product promise: a text_only backend never receives image content,
    /// end to end through serve().
    #[test]
    fn text_only_backend_never_sees_images() {
        let up_port = free_port();
        let sport = free_port();
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let cap2 = captured.clone();
        let upstream = TcpListener::bind(("127.0.0.1", up_port)).expect("bind upstream");
        std::thread::spawn(move || {
            if let Ok((s, _)) = upstream.accept() {
                let buf = read_one_request(s.try_clone().expect("clone"));
                *cap2.lock().unwrap() = buf;
                let body = br#"{"id":"x","object":"chat.completion","created":0,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let mut s = s;
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body);
            }
        });

        let toml = format!(
            r#"
[server]
bind = "127.0.0.1"
port = {sport}
profile = "main"

[backends.t]
type = "api_key"
base_url = "http://127.0.0.1:{up_port}/v1"
text_only = true

[profiles.main]
default = "t:m"
"#
        );
        let cfg: Config = toml::from_str(&toml).expect("config parses");
        let state = AppState::new(cfg);
        let shutdown = Arc::new(AtomicBool::new(false));
        let sh2 = shutdown.clone();
        std::thread::spawn(move || {
            let _ = crate::server::serve(state, sh2);
        });

        let mut sock = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", sport)) {
                sock = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut sock = sock.expect("server up");
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "t:m",
            "max_tokens": 50,
            "messages": [
                {"role":"user","content":[
                    {"type":"text","text":"look"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAA"}},
                ]},
            ],
        }))
        .expect("body");
        let head = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(head.as_bytes()).expect("write head");
        sock.write_all(&body).expect("write body");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !captured.lock().unwrap().is_empty() || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        shutdown.store(true, Ordering::SeqCst);
        let cap = captured.lock().unwrap().clone();
        let s = String::from_utf8_lossy(&cap).to_string();
        assert!(!s.is_empty(), "upstream must receive the request");
        assert!(
            !s.contains("image_url"),
            "text-only upstream saw image_url: {s}"
        );
        assert!(
            !s.contains(r#""type":"image""#),
            "text-only upstream saw image block: {s}"
        );
        assert!(
            s.contains("this backend is text-only"),
            "strip note must reach upstream: {s}"
        );
    }
}

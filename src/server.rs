//! Minimal threaded HTTP server (std only) speaking Anthropic + OpenAI shapes.

use crate::backends::{get_backend, UpstreamBody};
use crate::config::{EnvOverrides, DEFAULT_GROK_MODEL};
use crate::error::{anthropic_error, Error, Result};
use crate::models::{alias_models, model_card, stop_reason};
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
    eprintln!("  POST /v1/messages | /v1/chat/completions | Ctrl-C to stop\n");

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let st = state.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, st) {
                        // broken pipe etc. — ignore noisy clients
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
}

fn handle_client(mut stream: TcpStream, state: AppState) -> Result<()> {
    // Idle timeouts only (reset on each successful read/write). Do not use short
    // total-request caps — LAN streaming generations can run 30–60+ minutes while
    // still producing SSE deltas.
    let idle = Duration::from_secs(3600);
    stream.set_read_timeout(Some(idle))?;
    stream.set_write_timeout(Some(idle))?;
    let _ = stream.set_nodelay(true);
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
            handle_messages(&mut stream, &state, body)?;
        }
        ("POST", "/v1/messages/count_tokens") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let est = count_tokens_estimate(&body);
            write_json(&mut stream, 200, &json!({"input_tokens": est}))?;
        }
        ("POST", "/v1/chat/completions") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            handle_openai(&mut stream, &state, body)?;
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
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Request { method, path, body })
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
        if let crate::oauth::AuthSource::Oauth { expires_at } = source {
            if let Some(exp) = expires_at {
                entry.as_object_mut().unwrap().insert("expires_at".into(), json!(exp));
            }
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
                    }
                    return write_json(stream, 200, &card);
                }
            }
        }
        return write_json(stream, 200, &model_card(&model_id, "spock"));
    }

    // List: merge backends + aliases
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

fn handle_messages(sock: &mut TcpStream, state: &AppState, mut a: Value) -> Result<()> {
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
                let mut buf = [0u8; 8192];
                let mut r = reader;
                loop {
                    match r.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            sock.write_all(&buf[..n])?;
                            sock.flush()?;
                        }
                        Err(_) => break,
                    }
                }
                Ok(())
            }
            Err(e) => write_upstream_err(sock, state, e),
        };
    }

    // Microcompact + drop Anthropic-only keys before OpenAI-compat / xAI translation.
    prepare_for_openai_compat(&mut a);

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
    let use_server_tools = (advisor_cfg.enabled
        && crate::server_tools::request_has_advisor(&a))
        || (web_cfg.enabled && crate::server_tools::request_has_web_search(&a));

    // CRITICAL: the server-tools multi-round path forces stream:false upstream and
    // only emits SSE after the *entire* completion is done. On a slow LAN model
    // (~11 tok/s, 10–25 min turns) Claude Code sees zero bytes for ~600s and
    // reports "Request timed out" while llama-server is still generating.
    // When the client asked for streaming, always use real incremental SSE.
    // Advisor/web_search emulation still runs for non-stream clients.
    let use_server_tools = use_server_tools && !do_stream;

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
        if use_server_tools {
            " [server-tools]"
        } else if do_stream {
            " [stream]"
        } else {
            ""
        }
    );

    // Server-tool emulation runs a multi-round non-stream loop, then returns one Anthropic JSON
    // (or a synthetic SSE of that JSON for stream clients).
    if use_server_tools {
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
            Ok(resp) => {
                if do_stream {
                    return stream_json_as_anthropic_sse(sock, &resp);
                }
                return write_json(sock, 200, &resp);
            }
            Err(e) => return write_upstream_err(sock, state, e),
        }
    }

    if !do_stream {
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
                stream_anthropic(sock, reader, &client_model, include_thinking, state)
            }
            Ok(UpstreamBody::Json(o)) => {
                let resp = openai_to_anthropic(&o, &client_model, include_thinking);
                write_json(sock, 200, &resp)
            }
            Err(e) => write_upstream_err(sock, state, e),
        }
    }
}


/// Emit a completed Anthropic message as SSE events (for server-tool multi-round results).
fn stream_json_as_anthropic_sse(stream: &mut TcpStream, resp: &Value) -> Result<()> {
    write_sse_headers(stream)?;
    let msg_id = resp
        .get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(new_msg_id);
    let model = resp
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("spock");
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
                "usage": {"input_tokens": 0, "output_tokens": 0}
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
    let usage = resp.get("usage").cloned().unwrap_or(json!({"input_tokens":0,"output_tokens":0}));
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
            usage = u.clone();
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
                let tc_index = tc
                    .get("index")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
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
    emit_sse(
        stream,
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason(finish.as_deref()),
                "stop_sequence": null
            },
            "usage": {"output_tokens": out_tokens}
        }),
    )?;
    emit_sse(stream, "message_stop", &json!({"type": "message_stop"}))?;
    Ok(())
}

fn handle_openai(sock: &mut TcpStream, state: &AppState, mut body: Value) -> Result<()> {
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

    if be.quirk == CompletionsQuirk::Xai {
        let env = EnvOverrides::from_env();
        let reasoning =
            crate::models::is_reasoning_model(&resolved.upstream_model, &env.grok_model);
        crate::models::sanitize_upstream(&mut body, reasoning);
    }

    let stream_flag = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match be.chat(&body, stream_flag, &state.oauth) {
        Ok(UpstreamBody::Json(v)) => write_json(sock, 200, &v),
        Ok(UpstreamBody::Stream(mut reader)) => {
            write_sse_headers(sock)?;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        sock.write_all(&buf[..n])?;
                        sock.flush()?;
                    }
                    Err(_) => break,
                }
            }
            Ok(())
        }
        Err(e) => write_upstream_err(sock, state, e),
    }
}

fn write_upstream_err(stream: &mut TcpStream, state: &AppState, e: Error) -> Result<()> {
    match e {
        Error::Http(code, body) => {
            let raw = extract_err_msg(&body);
            let (out_status, err_type, msg) = classify_upstream_http(code, &raw);
            state.record_upstream_error(out_status, &msg);
            let (st, err) = anthropic_error(out_status, err_type, &msg);
            write_json(stream, st, &err)
        }
        other => {
            let msg = format!("spock backend error: {other}");
            state.record_upstream_error(502, &msg);
            let (st, err) = anthropic_error(502, "api_error", &msg);
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
        return format!(
            "upstream stream error [llama-server tool-call parser, not Spock]: {raw}"
        );
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
        if (500..600).contains(&code) { code } else { 502 },
        "api_error",
        format!("Spock upstream {code}: {raw}"),
    )
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
        assert!(msg.contains("NOT Anthropic") || msg.contains("not Anthropic"), "{msg}");
        assert!(msg.contains("SuperGrok") || msg.contains("credits"), "{msg}");
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

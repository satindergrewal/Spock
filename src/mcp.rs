//! MCP (Model Context Protocol) web-search server inside Spock — the same
//! listener/port as the Anthropic + OpenAI proxy, so external clients (ZCode
//! `type:"http"` or legacy `type:"sse"`) can call Spock's `[web_search]` engine
//! as `web_search`. Streamable HTTP (2025-06-18) + legacy SSE (2024-11-05).
//! Claude Code `/v1/messages` emulation and the `/v1/responses` shim stay
//! orthogonal — this is a third door, not a replacement.

use crate::config::VERSION;
use crate::error::{Error, Result};
use crate::server::{
    emit_sse, write_json, write_sse_headers, write_sse_stream_headers, write_status_only,
};
use crate::server_tools::{run_web_search, WebSearchConfig};
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];
const LEGACY_KEEPALIVE_SECS: u64 = 12;

/// Session-id counter for the legacy SSE hub. Process-local; ids never repeat.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
/// Legacy GET stream registry: sid → shared writer fd. One GET thread owns the
/// stream (incl. keepalives); a matching POST writes its JSON-RPC reply onto
/// that stream, since the 2024-11-05 SSE transport delivers responses on the
/// client's open GET stream, not on the POST's own HTTP body.
type LegacySink = Arc<Mutex<TcpStream>>;
static LEGACY_HUB: OnceLock<Mutex<BTreeMap<String, LegacySink>>> = OnceLock::new();

fn legacy_hub() -> &'static Mutex<BTreeMap<String, LegacySink>> {
    LEGACY_HUB.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn new_session_id() -> String {
    format!("sse-s{}", NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed))
}

/// Raw path keeps its query — server.rs strips `?` only for the match. The
/// legacy POST needs `sessionId` from it.
fn query_param(raw: &str, name: &str) -> Option<String> {
    let query = raw.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key.trim() == name {
                let v = value.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn allow_for(path: &str) -> &'static str {
    if path == "/mcp" || path == "/mcp/sse/messages" {
        "POST"
    } else {
        "GET"
    }
}

/// Route-arm entry: one guarded arm (prefix `/mcp`) owns its protocol-shaped
/// replies end-to-end — a stray `/mcp/*` path never falls to the Anthropic
/// default 404. Raw path is pre-query-strip; legacy POST reads sessionId there.
pub fn handle_mcp(
    stream: &mut TcpStream,
    state: &AppState,
    method: &str,
    raw_path: &str,
    body: &[u8],
    headers: &BTreeMap<String, String>,
) -> Result<()> {
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    match method {
        "POST" if path == "/mcp" => streamable_post(stream, state, body, headers),
        "POST" if path == "/mcp/sse/messages" => legacy_post(stream, state, body, raw_path),
        "GET" if path == "/mcp" => write_status_only(stream, 405, Some(("Allow", "POST"))),
        "GET" if path == "/mcp/sse" => legacy_open(stream),
        _ if path == "/mcp" || path == "/mcp/sse" || path == "/mcp/sse/messages" => {
            write_status_only(stream, 405, Some(("Allow", allow_for(path))))
        }
        _ => write_json(
            stream,
            404,
            &rpc_error(-32000, &format!("not found: {path}"), &Value::Null),
        ),
    }
}

/// Streamable HTTP POST (2025-06-18). Accept negotiation decides JSON versus
/// SSE (`event: message`) versus 406; a notification marker (status 204)
/// becomes a bodyless 204 regardless of transport. Parse failure is the only
/// HTTP 400 here — JSON-RPC method/params errors stay in 200 envelopes.
fn streamable_post(
    stream: &mut TcpStream,
    state: &AppState,
    body: &[u8],
    headers: &BTreeMap<String, String>,
) -> Result<()> {
    let accept = headers.get("accept").map(|s| s.as_str()).unwrap_or("");
    let choice = accept_wants(accept);
    if choice == AcceptChoice::Refuse {
        return write_status_only(stream, 406, None);
    }
    let parsed = match parse_body(body) {
        Some(v) => v,
        None => {
            let env = rpc_error(
                -32700,
                "parse error: request body is not JSON",
                &Value::Null,
            );
            return post_out(stream, choice, 400, &env);
        }
    };
    let mut exec = |args: &Value| live_web_exec(state, args);
    let (status, envelope) = rpc_out(&parsed, &mut exec);
    if status == 204 {
        return write_status_only(stream, 204, None);
    }
    post_out(stream, choice, status, &envelope)
}

/// Negotiated writer: JSON body, or one SSE `message` frame for the JSON-RPC
/// payload. Event-name convention is MCP's, not Anthropic's block tracker.
fn post_out(
    stream: &mut TcpStream,
    choice: AcceptChoice,
    status: u16,
    envelope: &Value,
) -> Result<()> {
    match choice {
        AcceptChoice::Sse => {
            write_sse_headers(stream)?;
            emit_sse(stream, "message", envelope)
        }
        _ => write_json(stream, status, envelope),
    }
}

fn parse_body(body: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(body).ok()
}

/// Client Accept must list `application/json` and `text/event-stream` in a
/// Streamable-HTTP POST; JSON is chosen when json is listed, SSE when only
/// event-stream is, else 406 Not Acceptable.
#[derive(Debug, PartialEq)]
enum AcceptChoice {
    Json,
    Sse,
    Refuse,
}

fn accept_wants(accept: &str) -> AcceptChoice {
    let a = accept.to_ascii_lowercase();
    if a.contains("application/json") {
        AcceptChoice::Json
    } else if a.contains("text/event-stream") {
        AcceptChoice::Sse
    } else {
        AcceptChoice::Refuse
    }
}

/// JSON-RPC core, shared by both transports. Injectable execution seam: the
/// live handler supplies `live_web_exec`; offline tests supply synthetic
/// results. Status 204 + `Value::Null` marks a notification (no `id` member)
/// — each transport turns it into its no-body acceptance (204 / legacy 202).
fn rpc_out(body: &Value, exec: &mut dyn FnMut(&Value) -> Value) -> (u16, Value) {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = body.get("method").and_then(|m| m.as_str()) else {
        return (
            200,
            rpc_error(-32600, "invalid request: method missing", &id),
        );
    };
    if body.get("id").is_none() || method.starts_with("notifications/") {
        return (204, Value::Null);
    }
    match method {
        "initialize" => (
            200,
            rpc_ok(
                &initialize_result(body.get("params").unwrap_or(&Value::Null)),
                &id,
            ),
        ),
        "ping" => (200, rpc_ok(&json!({}), &id)),
        "tools/list" => (200, rpc_ok(&tools_list_result(), &id)),
        "tools/call" => tool_call_out(body, exec, &id),
        _ => (
            200,
            rpc_error(-32601, &format!("method not found: {method}"), &id),
        ),
    }
}

fn tool_call_out(body: &Value, exec: &mut dyn FnMut(&Value) -> Value, id: &Value) -> (u16, Value) {
    let params = body.get("params").unwrap_or(&Value::Null);
    let Some(name) = params.pointer("/name").and_then(|n| n.as_str()) else {
        return (
            200,
            rpc_error(-32602, "invalid params: tools/call name missing", id),
        );
    };
    if name != "web_search" {
        return (
            200,
            rpc_error(
                -32602,
                &format!("invalid params: unknown tool '{name}'"),
                id,
            ),
        );
    }
    let args = params.get("arguments").unwrap_or(&Value::Null);
    if !args.is_object() {
        return (
            200,
            rpc_error(
                -32602,
                "invalid params: web_search arguments must be an object",
                id,
            ),
        );
    }
    if args.pointer("/query").and_then(|q| q.as_str()).is_none() {
        return (
            200,
            rpc_error(
                -32602,
                "invalid params: web_search query must be a string",
                id,
            ),
        );
    }
    (200, rpc_ok(&exec(args), id))
}

fn rpc_ok(result: &Value, id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(code: i64, message: &str, id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .pointer("/protocolVersion")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let proto = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested.to_string()
    } else {
        LATEST_PROTOCOL_VERSION.to_string()
    };
    json!({
        "protocolVersion": proto,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "spock", "version": VERSION}
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [{
            "name": "web_search",
            "title": "Web search",
            "description": "Search the public web. Provide a query string.",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Search query"}},
                "required": ["query"]
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": true}
        }]
    })
}

/// Live tool execution — the `[web_search]` gate mirrors the `/v1/responses`
/// shim: disabled refuses loudly, empty query and provider failures are tool
/// errors (`isError`), never RPC errors.
fn live_web_exec(state: &AppState, args: &Value) -> Value {
    let cfg = match state.snapshot_config() {
        Ok(c) => WebSearchConfig::from_section(&c.web_search),
        Err(e) => {
            return call_result_value(
                format!("Web search unavailable — config snapshot: {e}"),
                true,
            )
        }
    };
    if !cfg.enabled {
        return call_result_value(
            "Web search disabled: enable [web_search] in Spock config".to_string(),
            true,
        );
    }
    let query = args
        .pointer("/query")
        .and_then(|q| q.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    match run_web_search(&cfg, &query) {
        Ok(results) => call_result_value(web_search_hits_text(&results, &query), false),
        Err(e) => call_result_value(format!("Web search failed: {e}"), true),
    }
}

fn call_result_value(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// Readable hits for a model/tool result: title, url, snippet — the same
/// human-readable posture the Claude Code emulation keeps in transcripts.
fn web_search_hits_text(results: &Value, query: &str) -> String {
    let mut out = format!("Web search for “{query}”:");
    let mut n = 0usize;
    if let Some(hits) = results.as_array() {
        for hit in hits {
            let title = hit.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let url = hit.get("url").and_then(|t| t.as_str()).unwrap_or("");
            if title.is_empty() && url.is_empty() {
                continue;
            }
            n += 1;
            out.push_str(&format!("\n{n}. {title}"));
            if !url.is_empty() {
                out.push_str(&format!(" — {url}"));
            }
            if let Some(snippet) = hit.get("snippet").and_then(|s| s.as_str()) {
                if !snippet.is_empty() {
                    out.push_str(&format!("\n   {snippet}"));
                }
            }
        }
    }
    if n == 0 {
        out = format!("No search results for {query:?}.");
    }
    out
}

/// Legacy SSE transport (2024-11-05): GET opens the stream, first event is
/// `endpoint` carrying the POST URL + sessionId, then keepalives every 12s.
/// Responses from matching POSTs arrive as `message` events on THIS stream —
/// that is the legacy contract; the POST's own HTTP body stays 202.
fn legacy_open(stream: &mut TcpStream) -> Result<()> {
    let sid = new_session_id();
    let shared = Arc::new(Mutex::new(stream.try_clone()?));
    {
        let mut hub = legacy_hub()
            .lock()
            .map_err(|_| Error::Msg("mcp legacy hub lock".into()))?;
        hub.insert(sid.clone(), Arc::clone(&shared));
    }
    {
        let mut writer = shared
            .lock()
            .map_err(|_| Error::Msg("mcp legacy stream lock".into()))?;
        write_sse_stream_headers(&mut writer)?;
        emit_sse(
            &mut writer,
            "endpoint",
            &json!(format!("/mcp/sse/messages?sessionId={sid}")),
        )?;
    }
    let mut last_err: Option<Error> = None;
    let mut alive = true;
    while alive {
        thread::sleep(Duration::from_secs(LEGACY_KEEPALIVE_SECS));
        match shared.lock() {
            Err(_) => {
                last_err = Some(Error::Msg("mcp legacy stream lock".into()));
                alive = false;
            }
            Ok(mut writer) => {
                if let Err(e) = writer.write_all(b": spock-keepalive\n\n") {
                    last_err = Some(Error::Io(e));
                    alive = false;
                } else if let Err(e) = writer.flush() {
                    last_err = Some(Error::Io(e));
                    alive = false;
                }
            }
        }
    }
    match legacy_hub().lock() {
        Ok(mut hub) => {
            hub.remove(&sid);
        }
        Err(_) => eprintln!("  mcp legacy hub lock poisoned; sid={sid} not removed"),
    }
    eprintln!("  mcp legacy stream closed sid={sid}");
    match last_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn legacy_post(
    stream: &mut TcpStream,
    state: &AppState,
    body: &[u8],
    raw_path: &str,
) -> Result<()> {
    let sid = query_param(raw_path, "sessionId");
    let sink = {
        let hub = legacy_hub()
            .lock()
            .map_err(|_| Error::Msg("mcp legacy hub lock".into()))?;
        sid.as_ref().and_then(|s| hub.get(s).map(Arc::clone))
    };
    let Some(sink) = sink else {
        let msg = if sid.is_some() {
            format!(
                "unknown legacy session id: {}",
                sid.as_deref().unwrap_or("")
            )
        } else {
            "legacy POST missing sessionId".to_string()
        };
        return write_json(stream, 404, &rpc_error(-32001, &msg, &Value::Null));
    };
    // A parse-error envelope must NOT be fed back through rpc_out — that would
    // re-wrap the real -32700 as -32600 invalid request.
    let mut exec = |args: &Value| live_web_exec(state, args);
    let (envelope, is_notification) = match parse_body(body) {
        Some(v) => {
            let (status, env) = rpc_out(&v, &mut exec);
            (env, status == 204)
        }
        None => (
            rpc_error(
                -32700,
                "parse error: request body is not JSON",
                &Value::Null,
            ),
            false,
        ),
    };
    if !is_notification {
        // Reply delivery onto the GET stream never blocks the POST's own 202:
        // the stream may already be gone, and listeners stay loud (not silent).
        match sink.lock() {
            Ok(mut writer) => {
                if let Err(e) = emit_sse(&mut writer, "message", &envelope) {
                    eprintln!(
                        "  mcp legacy reply lost sid={} ({})",
                        sid.as_deref().unwrap_or(""),
                        e
                    );
                }
            }
            Err(_) => eprintln!(
                "  mcp legacy stream lock: reply skipped for sid={}",
                sid.as_deref().unwrap_or("")
            ),
        }
    }
    write_status_only(stream, 202, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_supported_or_latest() {
        let supported = initialize_result(&json!({"protocolVersion": "2025-03-26"}));
        assert_eq!(supported["protocolVersion"], "2025-03-26");
        let unknown = initialize_result(&json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(unknown["protocolVersion"], LATEST_PROTOCOL_VERSION);
        let absent = initialize_result(&Value::Null);
        assert_eq!(absent["protocolVersion"], LATEST_PROTOCOL_VERSION);
        assert_eq!(absent["capabilities"]["tools"]["listChanged"], json!(false));
        assert_eq!(absent["serverInfo"]["name"], "spock");
        assert_eq!(absent["serverInfo"]["version"], VERSION);
    }

    #[test]
    fn tools_list_single_web_search() {
        let out = tools_list_result();
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "web_search");
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], json!(true));
        assert_eq!(tools[0]["inputSchema"]["type"], "object");
    }

    #[test]
    fn accept_negotiation_matches_spec() {
        assert_eq!(
            accept_wants("application/json, text/event-stream"),
            AcceptChoice::Json
        );
        assert_eq!(accept_wants("text/event-stream"), AcceptChoice::Sse);
        assert_eq!(accept_wants(""), AcceptChoice::Refuse);
        assert_eq!(accept_wants("application/json"), AcceptChoice::Json);
        assert_eq!(accept_wants("text/HTML"), AcceptChoice::Refuse);
    }

    #[test]
    fn parse_bad_body_is_parse_error() {
        assert!(parse_body(br#"{"method":}"#).is_none());
        assert!(parse_body(br#"{"method": "ping", "id": 1}"#).is_some());
        let env = rpc_error(
            -32700,
            "parse error: request body is not JSON",
            &Value::Null,
        );
        assert_eq!(env["error"]["code"], -32700);
        assert_eq!(env["id"], Value::Null);
    }

    #[test]
    fn invalid_and_unknown_methods_keep_rpc_codes() {
        let (st, env) = rpc_out(&json!({"id": 3}), &mut |_| json!({})); // method missing
        assert_eq!(st, 200);
        assert_eq!(env["error"]["code"], -32600);
        assert_eq!(env["id"], 3);
        let (_, env) = rpc_out(&json!({"id": "a", "method": "prompts/list"}), &mut |_| {
            json!({})
        });
        assert_eq!(env["error"]["code"], -32601);
        assert!(env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("prompts/list"));
    }

    #[test]
    fn tool_call_param_errors() {
        let mut enc = |args: &Value| {
            let _ = args;
            call_result_value("never".into(), false)
        };
        let missing_name = rpc_out(
            &json!({"id": 1, "method": "tools/call", "params": {}}),
            &mut enc,
        );
        assert_eq!(missing_name.1["error"]["code"], -32602);
        let other_tool = rpc_out(
            &json!({"id": 1, "method": "tools/call",
             "params": {"name": "advisor", "arguments": {"query": "x"}}}),
            &mut enc,
        );
        assert_eq!(other_tool.1["error"]["code"], -32602);
        let args_scalar = rpc_out(
            &json!({"id": 1, "method": "tools/call",
             "params": {"name": "web_search", "arguments": 7}}),
            &mut enc,
        );
        assert_eq!(args_scalar.1["error"]["code"], -32602);
        let query_missing = rpc_out(
            &json!({"id": 1, "method": "tools/call",
             "params": {"name": "web_search", "arguments": {}}}),
            &mut enc,
        );
        assert_eq!(query_missing.1["error"]["code"], -32602);
    }

    #[test]
    fn tool_call_uses_injected_exec_seam_offline() {
        let mut exec = |args: &Value| {
            let q = args
                .pointer("/query")
                .and_then(|q| q.as_str())
                .unwrap_or("");
            call_result_value(
                web_search_hits_text(
                    &json!([{"title": "Rust", "url": "https://rust-lang.org", "snippet": "lang"}]),
                    q,
                ),
                false,
            )
        };
        let (st, env) = rpc_out(
            &json!({"id": 7, "method": "tools/call",
             "params": {"name": "web_search", "arguments": {"query": "rust"}}}),
            &mut exec,
        );
        assert_eq!(st, 200);
        assert_eq!(env["id"], 7);
        assert_eq!(env["result"]["isError"], false);
        let text = env["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("https://rust-lang.org"), "{text}");
        assert!(text.contains("rust"), "{text}");
    }

    #[test]
    fn call_result_error_shape_and_hits_text() {
        let ok = call_result_value("hits".into(), false);
        assert_eq!(ok["content"][0]["type"], "text");
        assert_eq!(ok["isError"], false);
        let err = call_result_value("Web search failed".to_string(), true);
        assert_eq!(err["isError"], true);
        let text = web_search_hits_text(
            &json!([{"title": "A", "url": "https://a.example.com", "snippet": "s"}]),
            "q",
        );
        assert!(text.contains("1. A"), "{text}");
        assert!(text.contains("https://a.example.com"), "{text}");
        assert!(text.contains("s"), "{text}");
        let empty = web_search_hits_text(&json!([]), "nothing");
        assert!(empty.contains("No search results"), "{empty}");
        assert!(empty.contains("nothing"), "{empty}");
    }

    #[test]
    fn notification_marker_is_no_body() {
        let (st, env) = rpc_out(&json!({"method": "notifications/initialized"}), &mut |_| {
            json!({})
        });
        assert_eq!(st, 204);
        assert_eq!(env, Value::Null);
        let (st2, env2) = rpc_out(
            &json!({"id": 9, "method": "notifications/initialized"}),
            &mut |_| json!({}),
        );
        assert_eq!(st2, 204); // notifications/* stays silent even with a stray id
        assert_eq!(env2, Value::Null);
    }

    #[test]
    fn legacy_query_param_and_session_ids() {
        assert_eq!(
            query_param("/mcp/sse/messages?a=1&sessionId=sse-s9&b=2", "sessionId"),
            Some("sse-s9".into())
        );
        assert_eq!(
            query_param("/mcp/sse/messages?sessionId=sse-s1", "sessionId"),
            Some("sse-s1".into())
        );
        assert_eq!(query_param("/mcp/sse/messages", "sessionId"), None);
        assert_eq!(
            query_param("/mcp/sse/messages?sessionId=", "sessionId"),
            None
        );
        let a = new_session_id();
        let b = new_session_id();
        assert!(a.starts_with("sse-s"), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn allow_header_matches_endpoint() {
        assert_eq!(allow_for("/mcp"), "POST");
        assert_eq!(allow_for("/mcp/sse"), "GET");
        assert_eq!(allow_for("/mcp/sse/messages"), "POST");
    }

    #[test]
    fn rpc_envelope_shapes() {
        let ok = rpc_ok(&json!({"content": []}), &json!(4));
        assert_eq!(ok["jsonrpc"], "2.0");
        assert_eq!(ok["id"], 4);
        let err = rpc_error(-32000, "not found: /mcp/foo", &Value::Null);
        assert_eq!(err["jsonrpc"], "2.0");
        assert_eq!(err["error"]["message"], "not found: /mcp/foo");
    }
}

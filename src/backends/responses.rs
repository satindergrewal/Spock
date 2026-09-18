//! Codex / ChatGPT subscription Responses API transport.
//!
//! The Codex consumer backend (`POST {base}/codex/responses`) speaks the OpenAI
//! Responses API and is streaming-only (it requires `store:false` +
//! `stream:true`). Spock's translate.rs / server_tools / kv all speak OpenAI
//! chat-completions shapes, so this module sits at the wire boundary and does
//! both directions:
//!
//!   completions body ──build──▶ Responses request ──POST──▶ upstream
//!   upstream SSE ──transliterate──▶ completions-shaped SSE / JSON
//!
//! The existing `sse::UpstreamEvents` / `openai_to_anthropic` / server-tool
//! loops consume the completions-shaped result unchanged — nothing else in
//! Spock needs to know about Responses.

#![allow(clippy::items_after_test_module)] // production fn tail sits after the test module — ordering-only lint, no behaviour.

use crate::backends::UpstreamBody;
use crate::error::{Error, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

const CONNECT_SECS: u64 = 15;
const STREAM_IDLE_READ_SECS: u64 = 3600;
const JSON_READ_SECS: u64 = 3600;
/// Spock's own UA — the codex backend keys routing on `originator`, not UA.
const CODEX_UA: &str = crate::config::UA;
/// Residency flag the Codex backend expects for account-scoped compute.
const RESIDENCY: &str = "us";
const ORIGINATOR: &str = "codex_cli_rs";

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(CONNECT_SECS))
        .timeout_read(Duration::from_secs(timeout_secs))
        .user_agent(CODEX_UA)
        .build()
}

fn rand_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).ok();
    let mut s = String::with_capacity(bytes * 2);
    for b in buf {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn conversation_id() -> String {
    format!("cp_{}", rand_hex(16))
}

/// Read `~/.codex/installation_id` if present (the Codex CLI/Desktop writes a
/// 36-char id there); otherwise synthesize a stable-ish one.
fn installation_id() -> String {
    if let Ok(p) = std::env::var("HOME") {
        let path = std::path::Path::new(&p).join(".codex/installation_id");
        if let Ok(t) = std::fs::read_to_string(&path) {
            let t = t.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    format!("inst-{}", rand_hex(10))
}

/// Extract an id from a JWT-shaped access token's claims. Returns None on any
/// parse failure — the header is optional and codex ignores an absent one for
/// API-key gateways.
fn chat_gpt_account_id(token: Option<&str>) -> Option<String> {
    use base64::engine::{general_purpose::URL_SAFE, general_purpose::URL_SAFE_NO_PAD};
    use base64::Engine;
    let token = token?;
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .or_else(|| URL_SAFE.decode(payload).ok())?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let acct = v
        .get("https://api.openai.com/auth")
        .and_then(|o| o.get("chatgpt_account_id"))
        .and_then(|x| x.as_str())?;
    Some(acct.to_string())
}

/// Send one chat request upstream. `stream` is what the *client* asked for;
/// the upstream is always streamed (Codex is stream-only), then either
/// transliterated live (stream clients) or collected into a JSON response
/// (non-stream clients), mirroring the codex-proxy behaviour.
#[allow(clippy::too_many_arguments)]
pub fn chat(
    base_url: &str,
    path: &str,
    token: Option<&str>,
    account_id: Option<&str>,
    body: &Value,
    stream: bool,
    extra_headers: &BTreeMap<String, String>,
) -> Result<UpstreamBody> {
    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let responses_body = completions_to_responses(body);
    let conv = conversation_id();
    let install = installation_id();

    let agent = agent(if stream {
        STREAM_IDLE_READ_SECS
    } else {
        JSON_READ_SECS
    });
    let mut req = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream");
    if let Some(tok) = token {
        if !tok.is_empty() {
            req = req.set("Authorization", &format!("Bearer {tok}"));
        }
    }
    // Subscription token path carries the account header; an explicit API-key
    // gateway does not (the codex-responses adapter deletes it there).
    let acct = account_id
        .map(str::to_string)
        .or_else(|| chat_gpt_account_id(token));
    if let Some(a) = acct {
        req = req.set("ChatGPT-Account-Id", &a);
    }
    req = req
        .set("originator", ORIGINATOR)
        .set("x-openai-internal-codex-residency", RESIDENCY)
        .set("x-client-request-id", &conv)
        .set("x-codex-installation-id", &install)
        .set("session_id", &conv)
        .set("session-id", &conv)
        .set("thread_id", &conv)
        .set("thread-id", &conv)
        .set("x-codex-window-id", &format!("{conv}:0"));
    // Extra configured headers never clobber what we just set (mirrors
    // openai_compat::apply_headers — Authorization and the codex context are
    // authoritative).
    for (k, v) in extra_headers {
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        if k.eq_ignore_ascii_case("authorization")
            || k.eq_ignore_ascii_case("originator")
            || k.eq_ignore_ascii_case("content-type")
        {
            continue;
        }
        req = req.set(k, v);
    }
    let resp = match req.send_json(responses_body) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            return Err(upstream_error(code, &text));
        }
        Err(e) => return Err(Error::Msg(format!("codex responses: {e}"))),
    };
    if stream {
        Ok(UpstreamBody::Stream(Box::new(ResponsesToCompletions::new(
            resp.into_reader(),
        ))))
    } else {
        let mut dec = ResponsesDecoder::new(resp.into_reader());
        let json = collect_completions(&mut dec);
        Ok(UpstreamBody::Json(json))
    }
}

fn upstream_error(code: u16, text: &str) -> Error {
    // Surface the upstream error verbatim (codex bodies are `{error:{...}}`
    // or plain text). A Cloudflare "Just a moment…" HTML means the endpoint
    // served a challenge — say so rather than failing cryptically.
    let v: Value = serde_json::from_str(text).unwrap_or_else(
        |_| json!({"error": {"message": text.chars().take(500).collect::<String>()}}),
    );
    let mut msg = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| text.chars().take(300).collect::<String>());
    if text.contains("Just a moment") {
        msg = "Cloudflare challenge on the Codex endpoint".to_string();
    }
    Error::Http(code, json!({"error": {"message": msg}}))
}

// ── Responses SSE → completions-shaped output ─────────────────────

/// Extracted, reduced view of one Codex Responses SSE event. Anything we do
/// not care about (keepalives, `response.in_progress`, content-part events,
/// web_search_call, …) maps to `Ignore`.
enum CodexEvent {
    ReasoningDelta(String),
    TextDelta(String),
    /// `output_item.added` for a function_call — carries the item id (used to
    /// resolve later delta/done events) and the call id + name.
    FunctionCallStart {
        item_id: Option<String>,
        call_id: String,
        name: String,
    },
    FunctionCallArgsDelta {
        item_id: Option<String>,
        call_id: String,
        delta: String,
    },
    FunctionCallArgsDone {
        item_id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
    },
    ImageGenDone {
        id: String,
        result: String,
        revised_prompt: Option<String>,
    },
    Completed {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
    },
    Error {
        message: String,
    },
    Ignore,
}

struct ResponsesDecoder<R: Read> {
    lines: std::io::Lines<BufReader<R>>,
}

impl<R: Read> ResponsesDecoder<R> {
    fn new(reader: R) -> Self {
        ResponsesDecoder {
            lines: BufReader::new(reader).lines(),
        }
    }

    /// Pull the next event. `Ok(None)` = clean EOF. A malformed event is
    /// skipped rather than killing the stream.
    fn next_event(&mut self) -> std::io::Result<Option<CodexEvent>> {
        loop {
            let line = match self.lines.next() {
                None => return Ok(None),
                Some(Err(e)) => return Err(e),
                Some(Ok(l)) => l,
            };
            let line = line.trim();
            if !line.starts_with("data:") {
                continue;
            }
            let payload = line[5..].trim();
            if payload.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let typ = v
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown")
                .to_string();
            let ev = match typ.as_str() {
                "response.reasoning_summary_text.delta"
                | "response.reasoning_summary_text.done" => {
                    // `.delta` carries the fragment; `.done` carries the whole
                    // text. Only forward deltas (the accumulator emits a full
                    // block either way and .done is redundant).
                    if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                        if !d.is_empty() {
                            CodexEvent::ReasoningDelta(d.to_string())
                        } else {
                            CodexEvent::Ignore
                        }
                    } else {
                        CodexEvent::Ignore
                    }
                }
                "response.output_text.delta" => match v.get("delta").and_then(|x| x.as_str()) {
                    Some(d) if !d.is_empty() => CodexEvent::TextDelta(d.to_string()),
                    _ => CodexEvent::Ignore,
                },
                "response.output_item.added" => {
                    let item = v.get("item").cloned().unwrap_or(json!({}));
                    match item.get("type").and_then(|t| t.as_str()) {
                        Some("function_call") => CodexEvent::FunctionCallStart {
                            item_id: item
                                .get("id")
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string()),
                            call_id: item
                                .get("call_id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("call_")
                                .to_string(),
                            name: item
                                .get("name")
                                .and_then(|x| x.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                        },
                        _ => CodexEvent::Ignore,
                    }
                }
                "response.function_call_arguments.delta" => CodexEvent::FunctionCallArgsDelta {
                    // The live wire keys delta/done events on the *item* id
                    // (`item_id`), not `call_id` — resolve via the item→call map.
                    item_id: Some(
                        v.get("item_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    call_id: v
                        .get("item_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    delta: v
                        .get("delta")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
                "response.function_call_arguments.done" => CodexEvent::FunctionCallArgsDone {
                    item_id: Some(
                        v.get("item_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    call_id: v
                        .get("item_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: v
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    arguments: v
                        .get("arguments")
                        .and_then(|x| x.as_str())
                        .unwrap_or("{}")
                        .to_string(),
                },
                "response.output_item.done" => {
                    let item = v.get("item").cloned().unwrap_or(json!({}));
                    if item.get("type").and_then(|t| t.as_str()) == Some("image_generation_call") {
                        CodexEvent::ImageGenDone {
                            id: item
                                .get("id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            result: item
                                .get("result")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            revised_prompt: item
                                .get("revised_prompt")
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string()),
                        }
                    } else {
                        CodexEvent::Ignore
                    }
                }
                "response.completed" => {
                    let u = v.get("response").cloned().unwrap_or(json!({}));
                    let usage = u.get("usage").cloned().unwrap_or(json!({}));
                    // Live shape: cached_tokens lives under input_tokens_details
                    // and reasoning_tokens under output_tokens_details — not at
                    // the usage top level.
                    let cached = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|x| x.as_u64())
                        .or_else(|| usage.get("cached_tokens").and_then(|x| x.as_u64()));
                    let reasoning = usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(|x| x.as_u64())
                        .or_else(|| usage.get("reasoning_tokens").and_then(|x| x.as_u64()));
                    CodexEvent::Completed {
                        input_tokens: usage.get("input_tokens").and_then(|x| x.as_u64()),
                        output_tokens: usage.get("output_tokens").and_then(|x| x.as_u64()),
                        cached_tokens: cached,
                        reasoning_tokens: reasoning,
                    }
                }
                "error" => {
                    let err = v.get("error").cloned().unwrap_or(json!({}));
                    CodexEvent::Error {
                        message: err
                            .get("message")
                            .and_then(|x| x.as_str())
                            .unwrap_or_else(|| {
                                err.get("code")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("codex error")
                            })
                            .to_string(),
                    }
                }
                "response.failed" => {
                    let err = v.get("error").cloned().unwrap_or(json!({}));
                    CodexEvent::Error {
                        message: err
                            .get("message")
                            .and_then(|x| x.as_str())
                            .unwrap_or("response.failed")
                            .to_string(),
                    }
                }
                _ => CodexEvent::Ignore,
            };
            return Ok(Some(ev));
        }
    }
}

/// Byte-level adapter: reads the upstream Responses SSE, transliterates each
/// event into a completions-shaped SSE chunk, and serves those bytes. The
/// existing `sse::UpstreamEvents` / `stream_anthropic` pump consumes the
/// output unchanged.
struct ResponsesToCompletions<R: Read> {
    decoder: ResponsesDecoder<R>,
    /// Bytes of already-produced completions SSE not yet read out.
    out: VecDeque<u8>,
    item_to_call: HashMap<String, String>,
    call_to_index: HashMap<String, i64>,
    next_index: i64,
    /// Set of calls whose argument deltas were streamed (a `.done` then skips
    /// re-emitting the full arguments — the accumulator already has them).
    deltas_forwarded: HashSet<String>,
    has_content: bool,
    has_tool_calls: bool,
    finished: bool,
    eof: bool,
}

impl<R: Read> ResponsesToCompletions<R> {
    fn new(reader: R) -> Self {
        ResponsesToCompletions {
            decoder: ResponsesDecoder::new(reader),
            out: VecDeque::new(),
            item_to_call: HashMap::new(),
            call_to_index: HashMap::new(),
            next_index: 0,
            deltas_forwarded: HashSet::new(),
            has_content: false,
            has_tool_calls: false,
            finished: false,
            eof: false,
        }
    }

    fn push(&mut self, chunk: Value) {
        let s = format!("data: {chunk}\n\n");
        for b in s.bytes() {
            self.out.push_back(b);
        }
    }

    /// Resolve the tool index for a delta/done: prefer the assigned call index
    /// via the item_id → call_id map, then the direct call_id key.
    fn resolve_index(&mut self, item_id: &Option<String>, call_id: &str) -> Option<i64> {
        if let Some(iid) = item_id {
            if let Some(call) = self.item_to_call.get(iid) {
                if let Some(i) = self.call_to_index.get(call) {
                    return Some(*i);
                }
            }
        }
        self.call_to_index.get(call_id).copied()
    }

    /// Handle one decoded event, emitting any completions chunks it implies.
    fn handle(&mut self, ev: CodexEvent) {
        let r: &mut Self = self;
        match ev {
            CodexEvent::ReasoningDelta(d) => {
                r.has_content = true;
                r.push(json!({"choices": [{"delta": {"reasoning_content": d}}]}));
            }
            CodexEvent::TextDelta(d) => {
                r.has_content = true;
                r.push(json!({"choices": [{"delta": {"content": d}}]}));
            }
            CodexEvent::FunctionCallStart {
                item_id,
                call_id,
                name,
            } => {
                r.has_tool_calls = true;
                r.has_content = true;
                let index = r.next_index;
                r.next_index += 1;
                r.call_to_index.insert(call_id.clone(), index);
                if let Some(iid) = &item_id {
                    r.item_to_call.insert(iid.clone(), call_id.clone());
                }
                r.push(json!({
                    "choices": [{"delta": {"tool_calls": [{
                        "index": index,
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name}
                    }]}}]
                }));
            }
            CodexEvent::FunctionCallArgsDelta {
                item_id,
                call_id,
                delta,
            } => {
                let Some(index) = r.resolve_index(&item_id, &call_id) else {
                    return;
                };
                r.deltas_forwarded.insert(call_id.clone());
                if !delta.is_empty() {
                    r.push(json!({
                        "choices": [{"delta": {"tool_calls": [{
                            "index": index,
                            "function": {"arguments": delta}
                        }]}}]
                    }));
                }
            }
            CodexEvent::FunctionCallArgsDone {
                item_id,
                call_id,
                name,
                arguments,
            } => {
                // A call is "started" if we already assigned it an index via
                // the item→call map or directly by call_id.
                let started = r.call_to_index.contains_key(&call_id)
                    || item_id
                        .as_ref()
                        .and_then(|iid| r.item_to_call.get(iid))
                        .map(|c| r.call_to_index.contains_key(c))
                        .unwrap_or(false);
                let index = match r.resolve_index(&item_id, &call_id) {
                    Some(i) => i,
                    None => {
                        let i = r.next_index;
                        r.next_index += 1;
                        r.call_to_index.insert(call_id.clone(), i);
                        if let Some(iid) = &item_id {
                            r.item_to_call.insert(iid.clone(), call_id.clone());
                        }
                        i
                    }
                };
                // Only emit the full arguments if no deltas were streamed for
                // this call — otherwise the accumulator already holds them and
                // re-emitting would double the argument string.
                if !r.deltas_forwarded.contains(&call_id) {
                    if !started {
                        r.push(json!({
                            "choices": [{"delta": {"tool_calls": [{
                                "index": index,
                                "id": if call_id.is_empty() { "call_" } else { &call_id },
                                "type": "function",
                                "function": {"name": name}
                            }]}}]
                        }));
                    }
                    if !arguments.is_empty() {
                        r.push(json!({
                            "choices": [{"delta": {"tool_calls": [{
                                "index": index,
                                "function": {"arguments": arguments}
                            }]}}]
                        }));
                    }
                }
            }
            CodexEvent::ImageGenDone {
                id,
                result,
                revised_prompt,
            } => {
                r.has_tool_calls = true;
                r.has_content = true;
                let index = r.next_index;
                r.next_index += 1;
                let mut input = json!({"result": result});
                if let Some(rp) = revised_prompt {
                    input["revised_prompt"] = json!(rp);
                }
                r.push(json!({
                    "choices": [{"delta": {"tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": {"name": "image_generation", "arguments": input.to_string()}
                    }]}}]
                }));
            }
            CodexEvent::Completed {
                input_tokens,
                output_tokens,
                cached_tokens,
                reasoning_tokens,
            } => {
                let input_tokens = input_tokens.unwrap_or(0);
                let output_tokens = output_tokens.unwrap_or(0);
                let cached = cached_tokens.unwrap_or(0);
                let uncached = input_tokens.saturating_sub(cached);
                let mut usage = json!({
                    "prompt_tokens": uncached,
                    "completion_tokens": output_tokens,
                });
                let mut det = json!({});
                if cached > 0 {
                    det["cached_tokens"] = json!(cached);
                }
                if !det.is_null() {
                    usage["prompt_tokens_details"] = det;
                }
                if let Some(rt) = reasoning_tokens {
                    if rt > 0 {
                        usage["completion_tokens_details"] = json!({"reasoning_tokens": rt});
                    }
                }
                if !r.has_content {
                    // Codex completed with no content — surface it like the
                    // codex-proxy rather than a silent empty stream.
                    r.push(json!({"choices": [{"delta": {"content": "[Error] Codex returned an empty response. Please retry."}}]}));
                }
                r.push(json!({"choices": [{"delta": {}}], "usage": usage}));
                let finish = if r.has_tool_calls {
                    "tool_calls"
                } else {
                    "stop"
                };
                r.push(json!({"choices": [{"delta": {}, "finish_reason": finish}]}));
                r.finished = true;
            }
            CodexEvent::Error { message } => {
                r.push(json!({"error": {"message": message}}));
                r.finished = true;
            }
            CodexEvent::Ignore => {}
        }
    }

    /// Drive decoding until `out` is non-empty or the stream ends.
    fn refill(&mut self) -> std::io::Result<()> {
        while self.out.is_empty() && !self.eof {
            let ev = match self.decoder.next_event()? {
                None => {
                    self.eof = true;
                    break;
                }
                Some(e) => e,
            };
            self.handle(ev);
        }
        if self.eof && self.out.is_empty() && !self.finished {
            let finish = if self.has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            };
            self.push(json!({"choices": [{"delta": {}, "finish_reason": finish}]}));
            self.finished = true;
        }
        Ok(())
    }
}

impl<R: Read> Read for ResponsesToCompletions<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.refill()?;
        let mut n = 0;
        while n < buf.len() {
            if let Some(b) = self.out.pop_front() {
                buf[n] = b;
                n += 1;
            } else {
                break;
            }
        }
        if n == 0 {
            // Signal EOF to the downstream SSE pump.
            Ok(0)
        } else {
            Ok(n)
        }
    }
}

/// Non-stream client path: drive the decoder to completion and return a
/// completions-shaped JSON response (openai_to_anthropic consumes it).
/// Codex is stream-only, so this re-streams upstream internally and buffers.
fn collect_completions(dec: &mut ResponsesDecoder<impl Read>) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut call_index: HashMap<String, i64> = HashMap::new();
    let mut item_to_call: HashMap<String, String> = HashMap::new();
    let mut next_index: i64 = 0;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cached_tokens = 0u64;
    let mut reasoning_tokens = 0u64;
    let mut saw_completed = false;

    while let Ok(Some(e)) = dec.next_event() {
        let ev = e;
        match ev {
            CodexEvent::ReasoningDelta(d) => reasoning.push_str(&d),
            CodexEvent::TextDelta(d) => text.push_str(&d),
            CodexEvent::FunctionCallStart {
                item_id,
                call_id,
                name,
            } => {
                let index = next_index;
                next_index += 1;
                call_index.insert(call_id.clone(), index);
                if let Some(iid) = &item_id {
                    item_to_call.insert(iid.clone(), call_id.clone());
                }
                tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": ""}
                }));
            }
            CodexEvent::FunctionCallArgsDelta {
                item_id,
                call_id,
                delta,
            } => {
                let index = resolve_index(&item_to_call, &call_index, &item_id, &call_id);
                if let Some(i) = index {
                    if let Some((_, cl)) = tool_calls
                        .iter_mut()
                        .enumerate()
                        .find(|(j, _)| *j as i64 == i)
                    {
                        append_args(cl, &delta);
                    }
                }
            }
            CodexEvent::FunctionCallArgsDone {
                item_id,
                call_id,
                name,
                arguments,
            } => {
                let index = resolve_index(&item_to_call, &call_index, &item_id, &call_id);
                match index {
                    Some(i) => {
                        if let Some((_, cl)) = tool_calls
                            .iter_mut()
                            .enumerate()
                            .find(|(j, _)| *j as i64 == i)
                        {
                            append_args(cl, &arguments);
                            if let Some(f) = cl.get_mut("function") {
                                if f.get("name")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .is_empty()
                                    && !name.is_empty()
                                {
                                    f["name"] = json!(name);
                                }
                            }
                        }
                    }
                    None => {
                        let index = next_index;
                        next_index += 1;
                        call_index.insert(call_id.clone(), index);
                        tool_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments}
                        }));
                    }
                }
            }
            CodexEvent::Completed {
                input_tokens: it,
                output_tokens: ot,
                cached_tokens: ct,
                reasoning_tokens: rt,
            } => {
                input_tokens = it.unwrap_or(0);
                output_tokens = ot.unwrap_or(0);
                cached_tokens = ct.unwrap_or(0);
                reasoning_tokens = rt.unwrap_or(0);
                saw_completed = true;
            }
            CodexEvent::ImageGenDone {
                id,
                result,
                revised_prompt,
            } => {
                let mut input = json!({"result": result});
                if let Some(rp) = revised_prompt {
                    input["revised_prompt"] = json!(rp);
                }
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": "image_generation", "arguments": input.to_string()}
                }));
            }
            CodexEvent::Error { message } => {
                return json!({
                    "id": format!("chatcmpl-{}", rand_hex(6)),
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": format!("[Codex error] {message}")},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 0, "completion_tokens": 0}
                });
            }
            CodexEvent::Ignore => {}
        }
    }

    let uncached = input_tokens.saturating_sub(cached_tokens);
    let has_tools = !tool_calls.is_empty();
    let finish = if has_tools { "tool_calls" } else { "stop" };
    let mut message = json!({"role": "assistant", "content": text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if has_tools {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let mut usage = json!({"prompt_tokens": uncached, "completion_tokens": output_tokens});
    if cached_tokens > 0 {
        usage["prompt_tokens_details"] = json!({"cached_tokens": cached_tokens});
    }
    if reasoning_tokens > 0 {
        usage["completion_tokens_details"] = json!({"reasoning_tokens": reasoning_tokens});
    }
    let ct = if saw_completed { finish } else { "stop" };
    json!({
        "id": format!("chatcmpl-{}", rand_hex(6)),
        "choices": [{"index": 0, "message": message, "finish_reason": ct}],
        "usage": usage,
    })
}

fn resolve_index(
    item_to_call: &HashMap<String, String>,
    call_index: &HashMap<String, i64>,
    item_id: &Option<String>,
    call_id: &str,
) -> Option<i64> {
    if let Some(iid) = item_id {
        if let Some(call) = item_to_call.get(iid) {
            if let Some(i) = call_index.get(call) {
                return Some(*i);
            }
        }
    }
    call_index.get(call_id).copied()
}

fn append_args(call: &mut Value, delta: &str) {
    if let Some(f) = call.get_mut("function") {
        let cur = f
            .get("arguments")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        f["arguments"] = json!(cur + delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a realistic Responses SSE body with both `event:` and `data:` lines.
    fn sse(events: &[(&str, Value)]) -> Vec<u8> {
        let mut s = String::new();
        for (typ, data) in events {
            s.push_str("event: ");
            s.push_str(typ);
            s.push_str("\ndata: ");
            s.push_str(&data.to_string());
            s.push_str("\n\n");
        }
        s.into_bytes()
    }

    #[test]
    fn completions_to_responses_maps_input_and_instructions() {
        let body = json!({
            "model": "gpt-5.5",
            "messages": [
                {"role": "system", "content": "You are TARS."},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "Bash", "arguments": "{\"cmd\":\"ls\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "files"},
            ],
            "tools": [{"type": "function", "function": {"name": "Bash", "description": "run"}}],
            "reasoning_effort": "high",
        });
        let req = completions_to_responses(&body);
        assert_eq!(req["model"], json!("gpt-5.5"));
        assert_eq!(req["instructions"], json!("You are TARS."));
        assert_eq!(req["store"], json!(false));
        assert_eq!(req["stream"], json!(true));
        // The leading system message is not an input item.
        let input = req["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[0]["content"], json!("hi"));
        assert_eq!(input[1]["type"], json!("function_call"));
        assert_eq!(input[1]["call_id"], json!("call_1"));
        assert_eq!(input[1]["name"], json!("Bash"));
        assert_eq!(input[2]["type"], json!("function_call_output"));
        assert_eq!(input[2]["call_id"], json!("call_1"));
        assert_eq!(input[2]["output"], json!("files"));
        assert_eq!(
            req["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        assert!(req["tools"].as_array().is_some());
    }

    #[test]
    fn reasoning_and_text_stream_become_anthropic_ready_events() {
        let feed = sse(&[
            (
                "response.reasoning_summary_text.delta",
                json!({"type": "response.reasoning_summary_text.delta", "delta": " think "}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "delta": "hello"}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 100, "output_tokens": 7, "input_tokens_details": {"cached_tokens": 40}, "output_tokens_details": {"reasoning_tokens": 3}}}}),
            ),
        ]);
        let mut conv = ResponsesToCompletions::new(std::io::Cursor::new(feed));
        let events: Vec<_> = crate::sse::UpstreamEvents::new(&mut conv).collect();
        let mut saw_reasoning = false;
        let mut saw_text = false;
        let mut saw_usage = false;
        let mut saw_finish = false;
        for e in events {
            let ev = e.unwrap();
            match ev {
                crate::sse::UpstreamEvent::Thinking(t) => {
                    assert!(t.contains("think"));
                    saw_reasoning = true;
                }
                crate::sse::UpstreamEvent::Text(t) => {
                    assert_eq!(t, "hello");
                    saw_text = true;
                }
                crate::sse::UpstreamEvent::Usage(u) => {
                    assert_eq!(u["prompt_tokens"], json!(60));
                    assert_eq!(u["completion_tokens"], json!(7));
                    saw_usage = true;
                }
                crate::sse::UpstreamEvent::Finish(f) => {
                    assert_eq!(f, "stop");
                    saw_finish = true;
                }
                _ => {}
            }
        }
        assert!(saw_reasoning);
        assert!(saw_text);
        assert!(saw_usage);
        assert!(saw_finish);
    }

    #[test]
    fn function_call_transliterates_to_completions_deltas() {
        let feed = sse(&[
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "id": "item_1", "call_id": "call_1", "name": "Bash"}}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type": "response.function_call_arguments.delta", "item_id": "item_1", "delta": "{\"cmd\":\"ls "}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"type": "response.function_call_arguments.done", "item_id": "item_1", "arguments": "{\"cmd\":\"ls -la\"}"}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 3, "cached_tokens": 0}}}),
            ),
        ]);
        let mut conv = ResponsesToCompletions::new(std::io::Cursor::new(feed));
        let events: Vec<_> = crate::sse::UpstreamEvents::new(&mut conv).collect();
        let mut saw_call = false;
        let mut saw_finish = false;
        for e in events {
            let ev = e.unwrap();
            match ev {
                crate::sse::UpstreamEvent::ToolCallFrag {
                    index, id, name, ..
                } => {
                    assert_eq!(index, 0);
                    if !name.is_empty() {
                        assert_eq!(name, "Bash");
                        assert_eq!(id, "call_1");
                    }
                    saw_call = true;
                }
                crate::sse::UpstreamEvent::Finish(f) => {
                    assert_eq!(f, "tool_calls");
                    saw_finish = true;
                }
                _ => {}
            }
        }
        assert!(saw_call);
        assert!(saw_finish);
    }

    #[test]
    fn collect_non_stream_builds_completions_json() {
        let feed = sse(&[
            (
                "response.reasoning_summary_text.delta",
                json!({"type": "response.reasoning_summary_text.delta", "delta": " plan "}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "delta": "hi"}),
            ),
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "id": "item_1", "call_id": "call_1", "name": "Bash"}}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"type": "response.function_call_arguments.done", "item_id": "item_1", "arguments": "{\"cmd\":\"ls\"}"}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 50, "output_tokens": 9, "input_tokens_details": {"cached_tokens": 20}}}}),
            ),
        ]);
        let mut dec = ResponsesDecoder::new(std::io::Cursor::new(feed));
        let out = collect_completions(&mut dec);
        let msg = &out["choices"][0]["message"];
        assert_eq!(msg["reasoning_content"], json!(" plan "));
        assert_eq!(msg["content"], json!("hi"));
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tc[0]["function"]["name"], json!("Bash"));
        assert!(tc[0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("\"cmd\":\"ls\""));
        assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(out["usage"]["prompt_tokens"], json!(30));
        assert_eq!(
            out["usage"]["prompt_tokens_details"]["cached_tokens"],
            json!(20)
        );
    }
}

// ── completions body → Responses request ──────────────────────────

/// Codex expects tools flattened: `{type:"function", name, description, parameters}`
/// with `name` at the top level. Spock's completions shape nests them under
/// `function` — rebuild the flat form the upstream requires. Unknown shapes are
/// forwarded as-is rather than silently dropped.
fn normalize_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for t in tools {
        if let Some(f) = t.get("function").and_then(|v| v.as_object()) {
            let mut nt = json!({
                "type": t.get("type").and_then(|x| x.as_str()).unwrap_or("function"),
                "name": f.get("name").and_then(|x| x.as_str()).unwrap_or(""),
            });
            if let Some(d) = f.get("description").and_then(|x| x.as_str()) {
                if !d.is_empty() {
                    nt["description"] = json!(d);
                }
            }
            if let Some(p) = f.get("parameters") {
                nt["parameters"] = p.clone();
            }
            out.push(nt);
        } else {
            out.push(t.clone());
        }
    }
    out
}

fn normalize_tool_choice(tc: &Value) -> Value {
    if let Some(s) = tc.as_str() {
        return json!(s);
    }
    if let Some(obj) = tc.as_object() {
        if let Some(fname) = obj
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            return json!({"type": "function", "name": fname});
        }
        if obj.get("name").and_then(|n| n.as_str()).is_some() {
            return tc.clone();
        }
        if let Some(t) = obj.get("type").and_then(|x| x.as_str()) {
            match t {
                "auto" => return json!("auto"),
                "any" => return json!("required"),
                "none" => return json!("none"),
                _ => {}
            }
        }
    }
    tc.clone()
}

/// Build an OpenAI Responses request from a chat-completions-shaped body.
/// Only the generation-relevant fields are forwarded — `max_tokens`,
/// `temperature`, `top_p`, `stop` are intentionally dropped because the Codex
/// backend 400s on some of them and decides its own context/output budget.
fn completions_to_responses(body: &Value) -> Value {
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("gpt-5.5");
    let mut instructions: Option<String> = None;
    let mut items: Vec<Value> = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for (idx, m) in msgs.iter().enumerate() {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            if role == "system" && idx == 0 {
                // The leading system message becomes `instructions` — codex
                // requires that field, and it is the only system that lands
                // here (Spock folds mid-conversation system reminders into
                // user turns, so index 0 is authoritative).
                instructions = message_text(m);
                continue;
            }
            items.extend(message_to_input_items(m, role));
        }
    }
    if items.is_empty() {
        items.push(json!({"role": "user", "content": ""}));
    }
    if instructions.is_none() {
        // Codex requires an instructions field — don't ship an empty string.
        instructions = Some("You are a helpful assistant.".into());
    }
    let mut req = json!({
        "model": model,
        "instructions": instructions,
        "input": items,
        "stream": true,
        "store": false,
    });
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let tools = normalize_tools(tools);
        if !tools.is_empty() {
            req["tools"] = Value::Array(tools);
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        req["tool_choice"] = normalize_tool_choice(tc);
    }
    // reasoning_effort "none" is dropped on the completions wire (see
    // translate.rs); the responses wire wants no reasoning field then.
    if let Some(e) = body.get("reasoning_effort").and_then(|v| v.as_str()) {
        if !e.is_empty() && e != "none" {
            req["reasoning"] = json!({"effort": e, "summary": "auto"});
        }
    }
    req
}

/// Extract plain text from a completions message (string or content array).
fn message_text(m: &Value) -> Option<String> {
    match m.get("content") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(arr)) => {
            let mut out = Vec::new();
            for p in arr {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                        if !t.is_empty() {
                            out.push(t.to_string());
                        }
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out.join("\n"))
            }
        }
        _ => None,
    }
}

/// One completions message → zero or more Responses input items.
fn message_to_input_items(m: &Value, role: &str) -> Vec<Value> {
    let mut items = Vec::new();
    // Text content (skipped if the message is tool-call-only, matching the
    // codex-proxy translator which does not emit an empty assistant item).
    // `tool` messages never get a plain content item — only the
    // function_call_output below.
    let text = message_text(m);
    let has_tool_calls = m
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if let Some(t) = text {
        if role == "user" || (role == "assistant" && !has_tool_calls) {
            items.push(message_content_item(m, role, t));
        }
    } else if !has_tool_calls && role != "tool" {
        items.push(json!({"role": role, "content": ""}));
    }

    if role == "assistant" {
        if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                let f = tc.get("function").cloned().unwrap_or(json!({}));
                let call_id = tc
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("call_")
                    .to_string();
                let name = f
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let arguments = f
                    .get("arguments")
                    .and_then(|x| x.as_str())
                    .unwrap_or("{}")
                    .to_string();
                items.push(json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments
                }));
            }
        }
    } else if role == "tool" {
        let call_id = m
            .get("tool_call_id")
            .and_then(|x| x.as_str())
            .unwrap_or("call_")
            .to_string();
        let output = message_text(m).unwrap_or_default();
        items.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }));
    }
    items
}

/// Build a `{role, content}` item, converting image_url parts to input_image.
fn message_content_item(m: &Value, role: &str, text: String) -> Value {
    // If the content is an array with image parts, build multimodal parts —
    // otherwise a plain string item.
    let mut parts: Vec<Value> = Vec::new();
    let mut has_image = false;
    if let Some(Value::Array(arr)) = m.get("content") {
        for p in arr {
            match p.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                        if !t.is_empty() {
                            parts.push(json!({"type": "input_text", "text": t}));
                        }
                    }
                }
                Some("image_url") => {
                    if let Some(u) = p
                        .get("image_url")
                        .and_then(|i| i.get("url"))
                        .and_then(|u| u.as_str())
                        .or_else(|| p.get("image_url").and_then(|i| i.as_str()))
                    {
                        parts.push(json!({"type": "input_image", "image_url": u}));
                        has_image = true;
                    }
                }
                _ => {}
            }
        }
    }
    if has_image {
        if !text.is_empty()
            && !parts
                .iter()
                .any(|p| p.get("type") == Some(&Value::String("input_text".into())))
        {
            parts.insert(0, json!({"type": "input_text", "text": text}));
        }
        json!({"role": role, "content": parts})
    } else {
        json!({"role": role, "content": text})
    }
}

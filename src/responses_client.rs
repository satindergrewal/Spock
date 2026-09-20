//! Client-facing OpenAI Responses wire ↔ Chat Completions translation.
//!
//! Codex Desktop/CLI custom providers speak **only** the Responses wire
//! (`wire_api = "responses"`; Chat Completions is removed in current codex-rs)
//! and POST `{base}/responses`, so `POST /v1/responses` must route a real
//! generation body through the routed backend. This module rebuilds the
//! Responses request into the chat-completions body the backend consumes and
//! rebuilds the upstream result back into Responses JSON / SSE events the
//! Codex client parses. It is the client-facing twin of `backends/responses.rs`
//! (upstream Codex wire: completions body → Responses request).

use crate::sse::{UpstreamEvent, UpstreamEvents};
use crate::translate;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// Responses request → chat-completions body, plus custom-tool names and the
/// tool→namespace map — outbound item types and Codex's `(namespace, name)`
/// rollout identity need both; namespaced bundles flatten eagerly.
pub fn responses_to_completions(body: &Value) -> (Value, HashSet<String>, HashMap<String, String>) {
    // Only generation-relevant fields forward: `store`, `include`, `text`,
    // `service_tier`, `prompt_cache_key`, `stream_options`, `client_metadata`,
    // `access_programs` are Responses-only and named backends decide their own
    // budget. Leading `instructions` is the authoritative system turn.
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instr) = body.get("instructions").and_then(|x| x.as_str()) {
        if !instr.trim().is_empty() {
            messages.push(json!({"role": "system", "content": instr}));
        }
    }
    if let Some(input) = body.get("input").and_then(|v| v.as_array()) {
        for item in input {
            messages.extend(input_item_to_messages(item));
        }
    }
    // Mid-history `role:"system"` items fold to user — Qwen3.5+ jinja 400s
    // when a system turn is not at index 0; folded history is the safe shape.
    fn fold_mid_system(msgs: &mut [Value]) {
        for m in msgs.iter_mut().skip(1) {
            if m.get("role").and_then(|r| r.as_str()) == Some("system") {
                m["role"] = json!("user");
            }
        }
    }
    fold_mid_system(&mut messages);

    let (tools, custom_tools, tool_namespaces) = chat_tools(body);
    let mut req = json!({"model": body.get("model").and_then(|m| m.as_str()).unwrap_or(""), "messages": messages});
    if !tools.is_empty() {
        req["tools"] = Value::Array(tools);
    }
    if let Some(tc) = body.get("tool_choice") {
        // The Responses wire carries a string (`"auto"`…) — the chat wire takes it verbatim.
        req["tool_choice"] = tc.clone();
    }
    if let Some(p) = body.get("parallel_tool_calls").and_then(|b| b.as_bool()) {
        req["parallel_tool_calls"] = json!(p);
    }
    if let Some(e) = responses_reasoning_effort(body.get("reasoning")) {
        req["reasoning_effort"] = json!(e);
    }
    req["stream"] = json!(body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false));
    (req, custom_tools, tool_namespaces)
}

/// One Responses input item → zero or more chat history messages.
fn input_item_to_messages(item: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if ty == "message" || (ty.is_empty() && item.get("role").is_some()) {
        let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        out.push(json!({"role": role, "content": content_parts_to_chat(item.get("content"))}));
    } else if ty == "function_call" || ty == "custom_tool_call" {
        // A tool call in history rides on a tool-only assistant message.
        let call_id = item
            .get("call_id")
            .and_then(|c| c.as_str())
            .unwrap_or("call_");
        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let arguments = if ty == "custom_tool_call" {
            item.get("input").and_then(|x| x.as_str()).unwrap_or("")
        } else {
            item.get("arguments")
                .and_then(|x| x.as_str())
                .unwrap_or("{}")
        };
        if !name.is_empty() {
            out.push(json!({
                "role": "assistant", "content": "",
                "tool_calls": [{"id": call_id, "type": "function", "function": {"name": name, "arguments": arguments}}]
            }));
        }
    } else if ty == "function_call_output"
        || ty == "custom_tool_call_output"
        || ty == "mcp_tool_call_output"
    {
        let call_id = item
            .get("call_id")
            .and_then(|c| c.as_str())
            .unwrap_or("call_");
        out.push(json!({"role": "tool", "tool_call_id": call_id, "content": output_payload_text(item.get("output"))}));
    }
    // `reasoning` / `agent_message` / audio-bearing or encrypted items skip:
    // a chat backend without a replay lane accepts the same transcript minus them.
    out
}

/// Responses content items → chat `message.content` (string or multimodal parts).
fn content_parts_to_chat(content: Option<&Value>) -> Value {
    if let Some(Value::String(s)) = content {
        return json!(s);
    }
    let mut parts: Vec<Value> = Vec::new();
    if let Some(arr) = content.and_then(|v| v.as_array()) {
        for p in arr {
            match p.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "input_text" | "output_text" | "text" => {
                    if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                        if !t.is_empty() {
                            parts.push(json!({"type": "text", "text": t}));
                        }
                    }
                }
                "input_image" => {
                    let url = p
                        .get("image_url")
                        .and_then(|x| x.as_str())
                        .or_else(|| p.pointer("/image/image_url").and_then(|x| x.as_str()));
                    if let Some(u) = url {
                        let mut part = json!({"type": "image_url", "image_url": {"url": u}});
                        if let Some(d) = p.get("detail").and_then(|x| x.as_str()) {
                            part["image_url"]["detail"] = json!(d);
                        }
                        parts.push(part);
                    }
                }
                // `file_id` refs and `input_audio` have no chat-completions shape
                // on most OpenAI-compat servers — forwarding them verbatim 400s.
                _ => {}
            }
        }
    }
    if parts.is_empty() {
        Value::String(String::new())
    } else {
        Value::Array(parts)
    }
}

/// Responses `tools` → chat nested function entries + custom-tool names +
/// tool-name → namespace map. **Namespace bundles flatten** — vLLM/DS4 validate
/// every upstream entry as literal `function`, so `{type:"namespace",…}` and any
/// named non-function entry converts instead of forwarding raw; the namespace
/// keeps Codex's `(namespace, name)` rollout identity on outbound items.
fn chat_tools(body: &Value) -> (Vec<Value>, HashSet<String>, HashMap<String, String>) {
    let mut out = Vec::new();
    let mut custom = HashSet::new();
    let mut namespaces = HashMap::new();
    if let Some(arr) = body.get("tools").and_then(|v| v.as_array()) {
        for t in arr {
            let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let name = t
                .get("name")
                .and_then(|n| n.as_str())
                .or_else(|| t.pointer("/function/name").and_then(|n| n.as_str()))
                .unwrap_or("");
            if ty == "namespace" {
                let ns = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if let Some(inner) = t.get("tools").and_then(|x| x.as_array()) {
                    for sub in inner {
                        let subty = sub.get("type").and_then(|x| x.as_str()).unwrap_or("");
                        let subname = sub
                            .get("name")
                            .and_then(|n| n.as_str())
                            .or_else(|| sub.pointer("/function/name").and_then(|n| n.as_str()))
                            .unwrap_or("");
                        if !subname.is_empty() && !ns.is_empty() {
                            namespaces.insert(subname.to_string(), ns.to_string());
                        }
                        if subty == "custom" {
                            if let Some(b) = chat_function_tool(sub) {
                                custom.insert(subname.to_string());
                                out.push(b);
                            }
                        } else if let Some(b) = chat_function_tool(sub) {
                            out.push(b);
                        }
                    }
                }
            } else if ty == "custom" {
                if let Some(built) = chat_function_tool(t) {
                    custom.insert(name.to_string());
                    out.push(built);
                }
            } else if ty == "function" || tool_is_web_search(t) {
                if let Some(built) = chat_function_tool(t) {
                    out.push(built);
                }
            } else if let Some(built) = chat_function_tool(t) {
                // Hosted/unknown named tools convert too — DS4's literal gate
                // never tolerates a raw non-function entry.
                out.push(built);
            } else {
                out.push(t.clone());
            }
        }
    }
    (out, custom, namespaces)
}

fn tool_is_web_search(t: &Value) -> bool {
    let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
    ty == "web_search"
        || ty == "web_search_preview"
        || ty.starts_with("web_search")
        || t.get("name").and_then(|n| n.as_str()) == Some("web_search")
        || t.pointer("/function/name").and_then(|n| n.as_str()) == Some("web_search")
}

/// Flat Responses tool → nested chat function tool. `parameters` is synthesized
/// for forms that omit it (hosted web_search, custom grammar tools).
fn chat_function_tool(t: &Value) -> Option<Value> {
    // Codex posts flat Responses tools (`name` at top level); older/pastiche
    // clients nest Chat-Completions form — read both, never drop the name.
    let name = t
        .get("name")
        .and_then(|n| n.as_str())
        .or_else(|| t.pointer("/function/name").and_then(|n| n.as_str()))?;
    if name.is_empty() {
        return None;
    }
    let mut f = json!({"name": name});
    if let Some(d) = t
        .get("description")
        .and_then(|x| x.as_str())
        .or_else(|| t.pointer("/function/description").and_then(|x| x.as_str()))
    {
        if !d.is_empty() {
            f["description"] = json!(d);
        }
    }
    f["parameters"] = t
        .get("parameters")
        .or_else(|| t.pointer("/function/parameters"))
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    Some(json!({"type": "function", "function": f}))
}

fn responses_reasoning_effort(r: Option<&Value>) -> Option<String> {
    let obj = r.and_then(|v| v.as_object())?;
    let e = obj.get("effort").and_then(|x| x.as_str())?.trim();
    if e.is_empty() || e == "none" {
        return None;
    }
    Some(e.to_string())
}

/// Upstream chat-completions message → Responses output items (message text,
/// function_call / custom_tool_call items). Reasoning stays a stream lane.
pub fn completions_json_to_responses(
    o: &Value,
    client_model: &str,
    input_tokens: u64,
    custom_tools: &HashSet<String>,
    tool_namespaces: &HashMap<String, String>,
) -> Value {
    let message = o
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .cloned()
        .unwrap_or_else(json_empty);
    let text = chat_message_text(message.get("content"));
    let mut out_items: Vec<Value> = Vec::new();
    if !text.is_empty() {
        out_items.push(json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }));
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let f = tc.get("function").cloned().unwrap_or(json!({}));
            let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let call_id = tc.get("id").and_then(|x| x.as_str()).unwrap_or("call_");
            let args = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
            if name.is_empty() {
                continue;
            }
            let item_id = translate::short_id("item_", 12);
            let mut item = if custom_tools.contains(name) {
                json!({"type": "custom_tool_call", "id": item_id, "call_id": call_id, "name": name, "input": args})
            } else {
                json!({"type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": args})
            };
            if let Some(ns) = tool_namespaces.get(name) {
                item["namespace"] = json!(ns);
            }
            out_items.push(item);
        }
    }
    let out_len = (text.len() + message_text_len(message.get("reasoning_content"))) / 4;
    let usage = responses_usage_from_chat(o.get("usage"), input_tokens, out_len as u64);
    let mut resp = json!({
        "id": translate::short_id("resp_", 16),
        "object": "response",
        "created_at": unix_secs(),
        "status": "completed",
        "model": client_model,
        "output": Value::Array(out_items),
        "usage": usage,
    });
    if !text.is_empty() {
        resp["output_text"] = json!(text);
    }
    resp
}

/// Plain text from a chat message content (string or text-block array).
fn chat_message_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for b in arr {
                if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn message_text_len(t: Option<&Value>) -> usize {
    t.and_then(|v| v.as_str()).unwrap_or("").len()
}

/// Chat usage → Responses nested usage (Codex `ResponseCompletedUsage` parse:
/// `input_tokens`, `input_tokens_details.cached_tokens`, `output_tokens`,
/// `output_tokens_details.reasoning_tokens`, `total_tokens` are required ints).
fn responses_usage_from_chat(u: Option<&Value>, est_in: u64, est_out: u64) -> Value {
    let obj = u.and_then(|v| v.as_object());
    let input = obj
        .and_then(|o| o.get("prompt_tokens").and_then(|x| x.as_u64()))
        .unwrap_or(est_in);
    let output = obj
        .and_then(|o| o.get("completion_tokens").and_then(|x| x.as_u64()))
        .unwrap_or(est_out);
    let cached = obj
        .and_then(|o| o.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let reasoning = obj
        .and_then(|o| o.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": input + output,
    })
}

fn json_empty() -> Value {
    Value::Object(serde_json::Map::new())
}

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Codex-parsable `response.failed` payload (`{error:{message,code,type}}`).
pub fn responses_failed_event(msg: &str) -> Value {
    json!({
        "type": "response.failed",
        "response": {
            "id": translate::short_id("resp_", 16),
            "status": "failed",
            "error": {"message": msg, "code": "invalid_request_error", "type": "invalid_request_error"}
        }
    })
}

// ── upstream chat SSE → client Responses SSE ──────────────────────

/// Byte-level adapter: reads the upstream chat-completions SSE, transliterates
/// each delta into a Responses SSE event Codex's `codx-api` parser consumes
/// (`response.created`, output text delta, output item added/done, completed),
/// and serves those bytes — `pump_upstream_sse` copies them unchanged.
pub struct ChatSseToResponses<R: Read> {
    events: UpstreamEvents<R>,
    /// Bytes of already-produced Responses SSE not yet read out.
    out: VecDeque<u8>,
    custom_tools: HashSet<String>,
    /// tool-name → namespace for outbound tool items — Codex rollout identity is
    /// `(namespace, name)`; namespaced bundles flatten but keep the pair.
    namespaces: HashMap<String, String>,
    response_id: String,
    message_item_id: String,
    model: String,
    /// index → (item_id, call_id, name) for streamed tool calls.
    started: HashMap<i64, (String, String, String)>,
    /// Indices whose `output_item.added` already went out — one announcement per call
    /// (vLLM fragments repeat id/name; Codex never gets duplicate added events).
    announced: HashSet<i64>,
    /// index → concatenated argument fragments (withheld until `.done` —
    /// Codex ignores `function_call_arguments.delta/done` and invokes on item done).
    args_by_index: HashMap<i64, String>,
    /// Upstream `choices[0].finish_reason` (end-turn signal for completion).
    finish: Option<String>,
    usage: Option<Value>,
    input_estimate: u64,
    /// Accumulated non-tool output chars for an output-token estimate.
    out_chars: usize,
    /// Accumulated assistant text / thinking for the terminal message item —
    /// Codex's UI commits assistant output on `output_item.done` message events.
    text_acc: String,
    reason_acc: String,
    /// The message `output_item.added` already went out once (first text delta).
    message_announced: bool,
    has_content: bool,
    has_tool_calls: bool,
    completed: bool,
    eof: bool,
}

impl<R: Read> ChatSseToResponses<R> {
    pub fn new(
        reader: R,
        custom_tools: HashSet<String>,
        model: String,
        input_estimate: u64,
    ) -> Self {
        let mut s = ChatSseToResponses {
            events: UpstreamEvents::new(reader),
            out: VecDeque::new(),
            custom_tools,
            namespaces: HashMap::new(),
            response_id: translate::short_id("resp_", 16),
            message_item_id: translate::short_id("msg_", 16),
            model,
            started: HashMap::new(),
            announced: HashSet::new(),
            args_by_index: HashMap::new(),
            finish: None,
            usage: None,
            input_estimate,
            out_chars: 0,
            text_acc: String::new(),
            reason_acc: String::new(),
            message_announced: false,
            has_content: false,
            has_tool_calls: false,
            completed: false,
            eof: false,
        };
        // `response.created` first: Codex reads its id for turn bookkeeping.
        s.push(json!({
            "type": "response.created",
            "response": {
                "id": s.response_id,
                "model": s.model,
                "object": "response",
                "status": "in_progress",
                "created_at": unix_secs()
            }
        }));
        s
    }

    /// Attach the tool→namespace map read off the request (namespaced bundles).
    pub fn with_namespaces(mut self, namespaces: HashMap<String, String>) -> Self {
        self.namespaces = namespaces;
        self
    }

    fn push(&mut self, chunk: Value) {
        let s = format!("data: {chunk}\n\n");
        for b in s.bytes() {
            self.out.push_back(b);
        }
    }

    fn note_frag(
        &mut self,
        index: i64,
        id: &str,
        name: &str,
        arguments: &str,
    ) -> (String, String, String) {
        let mut ids = self.started.get(&index).cloned().unwrap_or_else(|| {
            let item_id = translate::short_id("item_", 12);
            let call_id = if id.is_empty() {
                translate::new_tool_id()
            } else {
                id.to_string()
            };
            (item_id, call_id, name.to_string())
        });
        // First fragment can omit id/name on this index; later fragments fill them
        // without re-emitting a second `output_item.added` for one call.
        if ids.2.is_empty() && !name.is_empty() {
            ids.2 = name.to_string();
        }
        if ids.1.is_empty() && !id.is_empty() {
            let call_id = if id.is_empty() {
                translate::new_tool_id()
            } else {
                id.to_string()
            };
            ids.1 = call_id;
        }
        self.started.insert(index, ids.clone());
        self.args_by_index
            .entry(index)
            .or_default()
            .push_str(arguments);
        ids
    }

    /// Handle one upstream event, emitting any Responses chunks it implies.
    fn handle(&mut self, ev: UpstreamEvent) {
        match ev {
            UpstreamEvent::Thinking(d) => {
                self.has_content = true;
                self.out_chars += d.chars().count();
                self.reason_acc.push_str(&d);
                // Codex's reasoning lane is keyed on `reasoning_summary_text.delta`
                // (summary_index required); the model's own thinking becomes the summary.
                self.push(json!({"type": "response.reasoning_summary_text.delta", "delta": d, "summary_index": 0}));
            }
            UpstreamEvent::Text(d) => {
                self.has_content = true;
                self.out_chars += d.chars().count();
                self.text_acc.push_str(&d);
                // Real wire announces the message item before text deltas — Codex
                // app-server commits the answer on the matching `.done`.
                if !self.message_announced {
                    self.message_announced = true;
                    self.push(json!({
                        "type": "response.output_item.added",
                        "item": {
                            "type": "message", "id": self.message_item_id, "role": "assistant",
                            "content": [{"type": "output_text", "text": ""}]
                        }
                    }));
                }
                self.push(json!({"type": "response.output_text.delta", "delta": d, "item_id": self.message_item_id, "content_index": 0}));
            }
            UpstreamEvent::ToolCallFrag {
                index,
                id,
                name,
                arguments,
            } => {
                self.has_tool_calls = true;
                let (item_id, call_id, name) = self.note_frag(index, &id, &name, &arguments);
                // ONE announcement per index: repeated id/name fragments must not
                // resend `output_item.added` (Codex tracks items_added per turn).
                // A name-less first fragment waits; a later named fragment announces.
                if !name.is_empty() && !self.announced.contains(&index) {
                    self.announced.insert(index);
                    let mut item = if self.custom_tools.contains(name.as_str()) {
                        json!({"type": "custom_tool_call", "id": item_id, "call_id": call_id, "name": name, "input": ""})
                    } else {
                        json!({"type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": ""})
                    };
                    if let Some(ns) = self.namespaces.get(&name) {
                        item["namespace"] = json!(ns);
                    }
                    self.push(json!({"type": "response.output_item.added", "item": item}));
                }
            }
            UpstreamEvent::Usage(u) => {
                self.usage = Some(u.clone());
            }
            UpstreamEvent::Finish(f) => {
                self.finish = Some(f.clone());
                if f == "tool_calls" {
                    self.emit_tool_items_done();
                }
            }
        }
    }

    /// At `finish_reason:"tool_calls"`, complete calls arrive as `output_item.done`
    /// with the full arguments/input — Codex invokes on the done item.
    fn emit_tool_items_done(&mut self) {
        let started = std::mem::take(&mut self.started);
        let args_done = std::mem::take(&mut self.args_by_index);
        for (index, (item_id, call_id, name)) in started {
            if name.is_empty() {
                continue;
            }
            let args = args_done
                .get(&index)
                .map(|s| completed_json_args(s.as_str()))
                .unwrap_or_else(|| "{}".to_string());
            let mut item = if self.custom_tools.contains(name.as_str()) {
                json!({"type": "custom_tool_call", "id": item_id, "call_id": call_id, "name": name, "input": args})
            } else {
                json!({"type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": args})
            };
            if let Some(ns) = self.namespaces.get(&name) {
                item["namespace"] = json!(ns);
            }
            self.push(json!({"type": "response.output_item.done", "item": item}));
        }
        self.started.clear();
        self.args_by_index.clear();
    }

    /// Terminal `response.completed` with nested usage Codex parses; end turn
    /// follows the upstream finish reason (tool rounds are false).
    fn finalize(&mut self) {
        if self.completed {
            return;
        }
        // Empty stream guard: Codex app-server needs committed text, so the note
        // is streamed AND carried on the terminal message item.
        if !self.has_content && !self.has_tool_calls {
            self.text_acc = "[Error] upstream returned an empty response.".into();
            self.message_announced = true;
            self.push(json!({"type": "response.output_text.delta", "delta": self.text_acc.clone(), "item_id": self.message_item_id, "content_index": 0}));
        }
        // Reasoning item done: the thinking lane commits on the item too.
        if !self.reason_acc.is_empty() {
            self.push(json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "reasoning",
                    "id": translate::short_id("item_", 12),
                    "summary": [{"type": "summary_text", "text": self.reason_acc.clone()}]
                }
            }));
        }
        // Assistant message item done: the app's committed answer bubble
        // (`phase: final_answer` on a clean stop; tool rounds leave it absent).
        if self.has_content {
            let mut item = json!({
                "type": "message",
                "id": self.message_item_id,
                "role": "assistant",
                "content": [{"type": "output_text", "text": self.text_acc.clone()}]
            });
            if !self.has_tool_calls {
                item["phase"] = json!("final_answer");
            }
            self.push(json!({"type": "response.output_item.done", "item": item}));
        }
        let out_est = (self.out_chars / 4) as u64;
        let usage = responses_usage_from_chat(self.usage.as_ref(), self.input_estimate, out_est);
        let end_turn = self.finish.as_deref() == Some("stop");
        self.push(json!({
            "type": "response.completed",
            "response": {
                "id": self.response_id,
                "model": self.model,
                "status": "completed",
                "usage": usage,
                "end_turn": end_turn
            }
        }));
        self.completed = true;
    }

    /// Mid-stream upstream error → `response.failed` then EOF (Codex surfaces
    /// the message instead of "stream closed before response.completed").
    fn fail(&mut self, msg: &str) {
        self.push(responses_failed_event(msg));
        self.completed = true;
        self.started.clear();
        self.args_by_index.clear();
    }

    fn refill(&mut self) -> std::io::Result<()> {
        while self.out.is_empty() && !self.eof {
            let ev = match self.events.next() {
                None => {
                    self.eof = true;
                    break;
                }
                Some(r) => r,
            };
            match ev {
                Ok(e) => self.handle(e),
                Err(e) => {
                    self.fail(&e.to_string());
                    self.eof = true;
                    break;
                }
            }
        }
        if self.eof && self.out.is_empty() && !self.completed {
            self.finalize();
        }
        Ok(())
    }
}

impl<R: Read> Read for ChatSseToResponses<R> {
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
            Ok(0)
        } else {
            Ok(n)
        }
    }
}

/// Best-effort complete a streamed function-call argument string: some vLLM
/// parsers end fragments mid-quote/mid-object; Codex `Session::handle_function_call`
/// parses arguments as JSON, so closing the tail keeps the tool round-trip alive.
fn completed_json_args(raw: &str) -> String {
    if raw.is_empty() {
        return "{}".into();
    }
    if serde_json::from_str::<Value>(raw).is_ok() {
        return raw.to_string();
    }
    let mut s = raw.to_string();
    if !s.ends_with('"') {
        s.push('"');
    }
    if s.starts_with('{') && !s.ends_with('}') {
        s.push('}');
    }
    if s.starts_with('[') && !s.ends_with(']') {
        s.push(']');
    }
    if serde_json::from_str::<Value>(&s).is_ok() {
        s
    } else {
        raw.to_string()
    }
}

fn output_payload_text(o: Option<&Value>) -> String {
    match o {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for p in arr {
                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        Some(obj @ Value::Object(_)) => {
            if let Some(s) = obj.get("content").and_then(|x| x.as_str()) {
                return s.to_string();
            }
            if let Some(arr) = obj
                .get("content_items")
                .or_else(|| obj.get("content"))
                .and_then(|x| x.as_array())
            {
                let mut parts: Vec<String> = Vec::new();
                for p in arr {
                    if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                        if !t.is_empty() {
                            parts.push(t.to_string());
                        }
                    }
                }
                return parts.join("\n");
            }
            obj.to_string()
        }
        None | Some(_) => String::new(),
    }
}

/// Test-only here-then-tails-none: production fns sit before the module tests.
#[cfg(test)]
mod tests {
    use super::*;

    fn sse(events: &[(&str, Value)]) -> Vec<u8> {
        let mut s = String::new();
        for (_typ, data) in events {
            s.push_str("data: ");
            s.push_str(&data.to_string());
            s.push_str("\n\n");
        }
        s.push_str("data: [DONE]\n\n");
        s.into_bytes()
    }

    fn drain<R: Read>(r: R) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        let mut reader = r;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }

    #[test]
    fn responses_body_maps_to_completions_body() {
        let body = json!({
            "model": "dsv41_box:deepseek-v4.1-flash",
            "instructions": "You are TARS.",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"function_call","call_id":"call_1","name":"Bash","arguments":"{\"cmd\":\"ls\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"files"}
            ],
            "tools": [{"type":"function","name":"Bash","description":"run",
                       "parameters":{"type":"object","properties":{"cmd":{"type":"string"}}}}],
            "reasoning": {"effort":"low","summary":"auto"},
            "stream": true,
            "store": false
        });
        let (req, custom, _ns) = responses_to_completions(&body);
        assert!(custom.is_empty());
        assert_eq!(req["model"], json!("dsv41_box:deepseek-v4.1-flash"));
        assert_eq!(req["stream"], json!(true));
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], json!("system"));
        assert_eq!(msgs[0]["content"], json!("You are TARS."));
        assert_eq!(msgs[1]["role"], json!("user"));
        assert_eq!(msgs[1]["content"][0]["text"], json!("hi"));
        assert_eq!(msgs[2]["role"], json!("assistant"));
        assert_eq!(msgs[2]["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], json!("Bash"));
        assert_eq!(msgs[3]["role"], json!("tool"));
        assert_eq!(msgs[3]["tool_call_id"], json!("call_1"));
        assert_eq!(msgs[3]["content"], json!("files"));
        assert_eq!(req["tools"][0]["type"], json!("function"));
        assert_eq!(req["tools"][0]["function"]["name"], json!("Bash"));
        assert_eq!(req["reasoning_effort"], json!("low"));
    }

    #[test]
    fn mid_history_system_folds_to_user() {
        let body = json!({
            "instructions": "top",
            "input": [
                {"type":"message","role":"user","content":"a"},
                {"type":"message","role":"system","content":"mid reminder"},
                {"type":"message","role":"user","content":"b"}
            ]
        });
        let (req, _custom, _ns) = responses_to_completions(&body);
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], json!("system"));
        assert_eq!(msgs[1]["role"], json!("user"));
        assert_eq!(msgs[1]["content"], json!("a"));
        assert_eq!(msgs[2]["role"], json!("user"));
        // the mid system turn folded — jinja systems-at-index-0 rule
        assert_eq!(msgs[2]["content"], json!("mid reminder"));
        assert_eq!(msgs[3]["content"], json!("b"));
    }

    #[test]
    fn input_image_becomes_chat_image_url() {
        let body = json!({
            "input": [{"type":"message","role":"user","content":[
                {"type":"input_text","text":"look"},
                {"type":"input_image","image_url":"https://example.com/a.png","detail":"high"}
            ]}]
        });
        let (req, _custom, _ns) = responses_to_completions(&body);
        let parts = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], json!("text"));
        assert_eq!(parts[1]["type"], json!("image_url"));
        assert_eq!(
            parts[1]["image_url"]["url"],
            json!("https://example.com/a.png")
        );
        assert_eq!(parts[1]["image_url"]["detail"], json!("high"));
    }

    #[test]
    fn custom_tool_is_nested_and_remembered() {
        let body = json!({
            "tools": [{"type":"custom","name":"apply_patch"}]
        });
        let (req, custom, _ns) = responses_to_completions(&body);
        assert_eq!(req["tools"][0]["type"], json!("function"));
        assert_eq!(req["tools"][0]["function"]["name"], json!("apply_patch"));
        assert!(custom.contains("apply_patch"));
    }

    #[test]
    fn namespace_bundle_flattens_and_records_group() {
        let body = json!({
            "tools": [{
                "type": "namespace", "name": "documents", "description": "docs",
                "tools": [{
                    "type": "function", "name": "read_doc", "description": "read it",
                    "parameters": { "type": "object", "properties": { } }
                },
                {
                    "type": "custom", "name": "patch_doc"
                }]
            }]
        });
        let (req, custom, ns) = responses_to_completions(&body);
        let tools = req["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["function"]["name"], json!("read_doc"));
        assert_eq!(tools[1]["function"]["name"], json!("patch_doc"));
        assert!(custom.contains("patch_doc"));
        assert_eq!(ns.get("read_doc").map(|s| s.as_str()), Some("documents"));
        assert_eq!(ns.get("patch_doc").map(|s| s.as_str()), Some("documents"));
    }

    #[test]
    fn sse_tool_item_carries_namespace() {
        let frag = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{ "index": 0, "id": "call_1", "function": { "name": "read_doc", "arguments": "{}" } }]
                }
            }]
        });
        let done = json!({
            "choices": [{
                "delta": {}, "finish_reason": "tool_calls"
            }]
        });
        let feed = sse(&[("frag", frag), ("done", done)]);
        let bytes = drain(
            ChatSseToResponses::new(
                std::io::Cursor::new(feed),
                HashSet::new(),
                "client".into(),
                0,
            )
            .with_namespaces(HashMap::from([(
                "read_doc".to_string(),
                "documents".to_string(),
            )])),
        );
        let text = String::from_utf8(bytes).unwrap_or_default();
        let chunks: Vec<Value> = text
            .lines()
            .filter(|l| l.starts_with("data:"))
            .filter_map(|l| serde_json::from_str::<Value>(l[5..].trim()).ok())
            .collect();
        let added = chunks
            .iter()
            .find(|v| {
                v.get("type").and_then(|t| t.as_str()) == Some("response.output_item.added")
                    && v["item"]["type"] == json!("function_call")
            })
            .unwrap();
        assert_eq!(added["item"]["name"], json!("read_doc"));
        assert_eq!(added["item"]["namespace"], json!("documents"));
        let done = chunks
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("response.output_item.done"))
            .unwrap();
        assert_eq!(done["item"]["namespace"], json!("documents"));
    }

    #[test]
    fn nested_chat_shaped_tools_build_and_keep_web_marker() {
        let body = json!({
            "tools": [
                { "type": "function", "function": { "name": "Bash", "parameters": { "type": "object" } } },
                { "type": "function", "function": { "name": "web_search" } }
            ]
        });
        let (req, _custom, _ns) = responses_to_completions(&body);
        assert_eq!(req["tools"][0]["function"]["name"], json!("Bash"));
        assert_eq!(
            req["tools"][0]["function"]["parameters"]["type"],
            json!("object")
        );
        assert_eq!(req["tools"][1]["function"]["name"], json!("web_search"));
    }

    #[test]
    fn completions_json_becomes_responses_object() {
        let o = json!({
            "choices":[{"message":{"role":"assistant","content":"It equals four."}}],
            "usage":{"prompt_tokens":90,"completion_tokens":12}
        });
        let resp = completions_json_to_responses(
            &o,
            "dsv41_box:deepseek-v4.1-flash",
            100,
            &HashSet::new(),
            &HashMap::new(),
        );
        assert_eq!(resp["object"], json!("response"));
        assert_eq!(resp["status"], json!("completed"));
        assert_eq!(resp["model"], json!("dsv41_box:deepseek-v4.1-flash"));
        assert_eq!(resp["output_text"], json!("It equals four."));
        assert_eq!(resp["output"][0]["type"], json!("message"));
        assert_eq!(
            resp["output"][0]["content"][0]["type"],
            json!("output_text")
        );
        assert_eq!(resp["usage"]["input_tokens"], json!(90));
        assert_eq!(resp["usage"]["output_tokens"], json!(12));
        assert_eq!(resp["usage"]["total_tokens"], json!(102));
        assert_eq!(
            resp["usage"]["input_tokens_details"]["cached_tokens"],
            json!(0)
        );
        assert_eq!(
            resp["usage"]["output_tokens_details"]["reasoning_tokens"],
            json!(0)
        );
        assert!(resp["id"].as_str().unwrap().starts_with("resp_"));
    }

    #[test]
    fn sse_commits_assistant_message_done_for_ui() {
        let text_chunk = json!({
            "choices": [{
                "delta": {
                    "content": "Hello! How can I help you today?"
                }
            }]
        });
        let stop = json!({
            "choices": [{
                "delta": {}, "finish_reason": "stop"
            }]
        });
        let feed = sse(&[("text", text_chunk), ("stop", stop)]);
        let bytes = drain(ChatSseToResponses::new(
            std::io::Cursor::new(feed),
            HashSet::new(),
            "client".into(),
            0,
        ));
        let text = String::from_utf8(bytes).unwrap_or_default();
        let chunks: Vec<Value> = text
            .lines()
            .filter(|l| l.starts_with("data:"))
            .filter_map(|l| serde_json::from_str::<Value>(l[5..].trim()).ok())
            .collect();
        let types: Vec<&str> = chunks
            .iter()
            .filter_map(|v| v.get("type").and_then(|t| t.as_str()))
            .collect();
        assert!(types.contains(&"response.created"));
        assert!(types.contains(&"response.output_item.added"));
        assert!(types.contains(&"response.output_text.delta"));
        assert!(types.contains(&"response.output_item.done"));
        assert!(types.contains(&"response.completed"));
        assert_eq!(types.last().copied(), Some("response.completed"));
        let msg_added = chunks
            .iter()
            .find(|v| {
                v.get("type").and_then(|t| t.as_str()) == Some("response.output_item.added")
                    && v["item"]["type"] == json!("message")
            })
            .unwrap();
        assert_eq!(
            msg_added["item"]["content"][0]["type"],
            json!("output_text")
        );
        let msg_done = chunks
            .iter()
            .find(|v| {
                v.get("type").and_then(|t| t.as_str()) == Some("response.output_item.done")
                    && v["item"]["type"] == json!("message")
            })
            .unwrap();
        assert_eq!(
            msg_done["item"]["content"][0]["text"],
            json!("Hello! How can I help you today?")
        );
        assert_eq!(msg_done["item"]["role"], json!("assistant"));
        assert_eq!(msg_done["item"]["phase"], json!("final_answer"));
        let completed = chunks
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("response.completed"))
            .unwrap();
        assert_eq!(completed["response"]["end_turn"], json!(true));
    }

    #[test]
    fn sse_transliterates_text_tools_and_completion() {
        let text_chunk = json!({
            "choices": [{
                "delta": {
                    "content": "hello"
                }
            }]
        });
        let tool1 = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0, "id": "call_1",
                        "function": {"name": "Bash", "arguments": "{\"cmd\":\"ls " }
                    }]
                }
            }]
        });
        let tool2 = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": "-la\"}" }
                    }]
                }
            }]
        });
        let done = json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        });
        let usage = json!({
            "choices": [{
                "delta": {}
            }],
            "usage": {
                "prompt_tokens": 120, "completion_tokens": 20
            }
        });
        let feed = sse(&[
            ("text", text_chunk),
            ("tool", tool1),
            ("tool2", tool2),
            ("done", done),
            ("usage", usage),
        ]);
        let bytes = drain(ChatSseToResponses::new(
            std::io::Cursor::new(feed),
            HashSet::new(),
            "client".into(),
            100,
        ));
        let text = String::from_utf8(bytes.clone()).unwrap_or_default();
        let chunks: Vec<Value> = text
            .lines()
            .filter(|l| l.starts_with("data:"))
            .filter_map(|l| serde_json::from_str::<Value>(l[5..].trim()).ok())
            .collect();
        let types: Vec<&str> = chunks
            .iter()
            .filter_map(|v| v.get("type").and_then(|t| t.as_str()))
            .collect();
        assert!(types.contains(&"response.created"));
        assert!(types.contains(&"response.output_text.delta"));
        assert!(types.contains(&"response.output_item.added"));
        assert!(types.contains(&"response.output_item.done"));
        assert_eq!(
            chunks
                .iter()
                .filter(|v| v.get("type").and_then(|t| t.as_str())
                    == Some("response.output_item.added")
                    && v["item"]["type"] == json!("message"))
                .count(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|v| v.get("type").and_then(|t| t.as_str())
                    == Some("response.output_item.added")
                    && v["item"]["type"] == json!("function_call"))
                .count(),
            1
        );
        assert!(types.contains(&"response.completed"));
        assert_eq!(types.last().copied(), Some("response.completed"));
        let text_delta = chunks
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("response.output_text.delta"))
            .unwrap();
        assert_eq!(text_delta["delta"], json!("hello"));
        assert!(text_delta["item_id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(text_delta["content_index"], json!(0));
        let added = chunks
            .iter()
            .find(|v| {
                v.get("type").and_then(|t| t.as_str()) == Some("response.output_item.added")
                    && v["item"]["type"] == json!("function_call")
            })
            .unwrap();
        assert_eq!(added["item"]["type"], json!("function_call"));
        assert_eq!(added["item"]["call_id"], json!("call_1"));
        assert_eq!(added["item"]["name"], json!("Bash"));
        assert!(added["item"]["id"].as_str().unwrap().starts_with("item_"));
        let done = chunks
            .iter()
            .find(|v| {
                v.get("type").and_then(|t| t.as_str()) == Some("response.output_item.done")
                    && v["item"]["type"] == json!("function_call")
            })
            .unwrap();
        assert_eq!(done["item"]["arguments"], json!("{\"cmd\":\"ls -la\"}"));
        let msg_done = chunks
            .iter()
            .find(|v| {
                v.get("type").and_then(|t| t.as_str()) == Some("response.output_item.done")
                    && v["item"]["type"] == json!("message")
            })
            .unwrap();
        assert_eq!(msg_done["item"]["content"][0]["text"], json!("hello"));
        // tool rounds leave phase absent (tool execution follows the item)
        assert_eq!(msg_done["item"]["phase"], json!(null));
        let completed = chunks
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("response.completed"))
            .unwrap();
        assert!(completed["response"]["id"]
            .as_str()
            .unwrap()
            .starts_with("resp_"));
        assert_eq!(completed["response"]["usage"]["input_tokens"], json!(120));
        assert_eq!(completed["response"]["usage"]["output_tokens"], json!(20));
        assert_eq!(completed["response"]["usage"]["total_tokens"], json!(140));
        assert_eq!(
            completed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
            json!(0)
        );
        assert_eq!(
            completed["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
            json!(0)
        );
        assert_eq!(completed["response"]["end_turn"], json!(false));
    }

    #[test]
    fn unterminated_arg_fragments_closed_at_done() {
        let unfinished = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0, "id": "call_u",
                        "function": { "name": "Bash", "arguments": "{\"cmd\":\"ls" }
                    }]
                }
            }]
        });
        let finish_chunk = json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        });
        let feed = sse(&[("frag", unfinished), ("done", finish_chunk)]);
        let bytes = drain(ChatSseToResponses::new(
            std::io::Cursor::new(feed),
            HashSet::new(),
            "client".into(),
            0,
        ));
        let text = String::from_utf8(bytes).unwrap_or_default();
        let chunks: Vec<Value> = text
            .lines()
            .filter(|l| l.starts_with("data:"))
            .filter_map(|l| serde_json::from_str::<Value>(l[5..].trim()).ok())
            .collect();
        let done = chunks
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("response.output_item.done"))
            .unwrap();
        assert_eq!(done["item"]["arguments"], json!("{\"cmd\":\"ls\"}"));
    }

    #[test]
    fn sse_reasoning_uses_summary_lane_with_index() {
        let think = json!({
            "choices": [{
                "delta": {
                    "reasoning_content": " plan "
                }
            }]
        });
        let feed = sse(&[("think", think)]);
        let bytes = drain(ChatSseToResponses::new(
            std::io::Cursor::new(feed),
            HashSet::new(),
            "client".into(),
            0,
        ));
        let text = String::from_utf8(bytes).unwrap_or_default();
        let chunks: Vec<Value> = text
            .lines()
            .filter(|l| l.starts_with("data:"))
            .filter_map(|l| serde_json::from_str::<Value>(l[5..].trim()).ok())
            .collect();
        let summary = chunks
            .iter()
            .find(|v| {
                v.get("type").and_then(|t| t.as_str())
                    == Some("response.reasoning_summary_text.delta")
            })
            .unwrap();
        assert_eq!(summary["delta"], json!(" plan "));
        assert_eq!(summary["summary_index"], json!(0));
    }

    #[test]
    fn mid_stream_error_becomes_failed_event_then_eof() {
        let partial = json!({
            "choices": [{
                "delta": {
                    "content": "partial"
                }
            }]
        });
        let boom = json!({
            "error": {
                "message": "boom"
            }
        });
        let feed = sse(&[("text", partial), ("boom", boom)]);
        let bytes = drain(ChatSseToResponses::new(
            std::io::Cursor::new(feed),
            HashSet::new(),
            "client".into(),
            5,
        ));
        let text = String::from_utf8(bytes).unwrap_or_default();
        assert!(text.contains("response.failed"), "{text}");
        assert!(text.contains("\"message\":\"boom\""), "{text}");
        assert!(!text.contains("response.completed"), "{text}");
    }

    #[test]
    fn empty_stream_completion_keeps_codex_error_text() {
        let silence = json!({
            "choices": [{
                "delta": {}, "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 4, "completion_tokens": 0
            }
        });
        let feed = sse(&[("silence", silence)]);
        let bytes = drain(ChatSseToResponses::new(
            std::io::Cursor::new(feed),
            HashSet::new(),
            "client".into(),
            4,
        ));
        let text = String::from_utf8(bytes).unwrap_or_default();
        assert!(text.contains("empty response"), "{text}");
        assert!(text.contains("response.completed"), "{text}");
    }
}

//! Shared upstream-SSE parsing and Anthropic block emission for both
//! `/v1/messages` streaming paths (generic passthrough and server-tools
//! emulation). One parser, two emission policies: the generic path forwards
//! tool_call fragments live; the server-tools path withholds them until the
//! round completes so emulated (advisor / web_search) calls can be executed
//! server-side instead of leaking to the client as `tool_use`.

use crate::error::{Error, Result};
use crate::translate::new_tool_id;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};

/// One normalized event from an upstream OpenAI-compatible SSE stream.
#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamEvent {
    /// delta.reasoning_content (z.ai/xAI/Kimi) or delta.reasoning (vLLM parser)
    Thinking(String),
    /// delta.content as a plain string
    Text(String),
    /// One fragment of a streamed tool call, keyed by the OpenAI index.
    /// id/name are usually only present on the first fragment for an index.
    ToolCallFrag {
        index: i64,
        id: String,
        name: String,
        arguments: String,
    },
    /// choices[0].finish_reason (last one seen wins)
    Finish(String),
    /// chunk.usage when it is a non-null object
    Usage(Value),
}

/// Iterator over `data:` lines of an upstream chat-completions SSE body.
/// One upstream chunk can carry several things at once (reasoning + content,
/// multiple tool_call fragments, finish_reason + usage) — all of them are
/// yielded, in chunk order. Mid-stream error objects and read failures
/// surface as `Err(msg)` and end iteration — callers decide how to label and
/// emit them.
pub struct UpstreamEvents<R: Read> {
    lines: std::io::Lines<BufReader<R>>,
    pending: std::collections::VecDeque<UpstreamEvent>,
    done: bool,
}

impl<R: Read> UpstreamEvents<R> {
    pub fn new(reader: R) -> Self {
        UpstreamEvents {
            lines: BufReader::new(reader).lines(),
            pending: std::collections::VecDeque::new(),
            done: false,
        }
    }
}

impl<R: Read> Iterator for UpstreamEvents<R> {
    type Item = Result<UpstreamEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if let Some(ev) = self.pending.pop_front() {
                return Some(Ok(ev));
            }
            let line = match self.lines.next() {
                None => return None,
                Some(Ok(l)) => l,
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(Error::Msg(format!("stream read error: {e}"))));
                }
            };
            let line = line.trim();
            if !line.starts_with("data:") {
                continue;
            }
            let payload = line[5..].trim();
            if payload == "[DONE]" {
                self.done = true;
                return None;
            }
            let chunk: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Some OpenAI-compat servers emit error objects mid-SSE after
            // 200 headers — surface, don't silently truncate. Iteration ends
            // here: callers must not consume bytes past an upstream error.
            if let Some(err) = chunk.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| err.to_string());
                self.done = true;
                self.pending.clear();
                return Some(Err(Error::Msg(msg)));
            }
            if let Some(u) = chunk.get("usage") {
                if u.is_object() {
                    self.pending.push_back(UpstreamEvent::Usage(u.clone()));
                }
            }
            let choice = chunk
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or_else(json_empty);
            if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                self.pending
                    .push_back(UpstreamEvent::Finish(fr.to_string()));
            }
            let delta = choice.get("delta").cloned().unwrap_or_else(json_empty);
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(|t| t.as_str())
            {
                if !reasoning.is_empty() {
                    self.pending
                        .push_back(UpstreamEvent::Thinking(reasoning.to_string()));
                }
            }
            if let Some(text) = delta.get("content").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    self.pending
                        .push_back(UpstreamEvent::Text(text.to_string()));
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let fn_ = tc.get("function").cloned().unwrap_or_else(json_empty);
                    self.pending.push_back(UpstreamEvent::ToolCallFrag {
                        index: tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0),
                        id: tc
                            .get("id")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: fn_
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string(),
                        arguments: fn_
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .unwrap_or("")
                            .to_string(),
                    });
                }
            }
            // Loop: drain what this chunk produced (or read the next line if
            // the chunk carried nothing usable — keepalives, empty deltas).
        }
    }
}

fn json_empty() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Assembles streamed tool_call fragments into whole calls, keyed by the
/// OpenAI tool index. Mirrors the rule the generic passthrough uses: one
/// upstream index is one call; id/name arrive on the first fragment, argument
/// string fragments concatenate across the rest.
#[derive(Default)]
pub struct ToolCallAccumulator {
    calls: std::collections::BTreeMap<i64, PartialCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartialCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCallAccumulator {
    pub fn push(&mut self, index: i64, id: &str, name: &str, arguments: &str) {
        let e = self.calls.entry(index).or_insert_with(|| PartialCall {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
        });
        if !id.is_empty() && e.id.is_empty() {
            e.id = id.to_string();
        }
        if !name.is_empty() && e.name.is_empty() {
            e.name = name.to_string();
        }
        e.arguments.push_str(arguments);
    }

    /// Complete calls in index order. Unusable fragments (no name ever seen)
    /// are dropped — the same shape both existing paths tolerate. Missing ids
    /// are filled so the echoed assistant message, tool results, and any
    /// client-side `tool_use` block agree.
    pub fn finish(self) -> Vec<PartialCall> {
        self.calls
            .into_values()
            .filter(|c| !c.name.is_empty())
            .map(|mut c| {
                if c.id.is_empty() {
                    c.id = new_tool_id();
                }
                c
            })
            .collect()
    }
}

/// Where a streamed Anthropic message writes its events, plus the executor
/// "gap" brackets: while Spock runs a slow emulated tool (advisor review,
/// web search) the main thread emits nothing, and only then may a socket
/// keepalive ping thread write. Implementations that don't need keepalives
/// use the no-op defaults.
pub trait SseSink {
    fn event(&mut self, name: &str, data: &Value) -> Result<()>;
    fn begin_gap(&mut self) {}
    fn end_gap(&mut self) {}
}

/// Tracks the currently open content block and the block index across one
/// Anthropic message — including across multiple upstream rounds in the
/// server-tools path, where thinking/text from later rounds must continue
/// the same message's index sequence.
pub struct BlockTracker {
    index: i64,
    open: Option<&'static str>,
}

impl BlockTracker {
    pub fn new() -> Self {
        BlockTracker {
            index: -1,
            open: None,
        }
    }

    fn ensure(&mut self, sink: &mut dyn SseSink, kind: &'static str, empty: Value) -> Result<()> {
        if self.open != Some(kind) {
            self.close(sink)?;
            self.index += 1;
            self.open = Some(kind);
            sink.event(
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": self.index,
                    "content_block": empty
                }),
            )?;
        }
        Ok(())
    }

    pub fn ensure_thinking(&mut self, sink: &mut dyn SseSink) -> Result<()> {
        self.ensure(
            sink,
            "thinking",
            serde_json::json!({"type": "thinking", "thinking": ""}),
        )
    }

    pub fn ensure_text(&mut self, sink: &mut dyn SseSink) -> Result<()> {
        self.ensure(
            sink,
            "text",
            serde_json::json!({"type": "text", "text": ""}),
        )
    }

    /// Mark a tool block as open (generic passthrough opens tool_use blocks
    /// live while streaming fragments).
    pub fn mark_tool(&mut self) -> i64 {
        self.index += 1;
        self.open = Some("tool");
        self.index
    }

    pub fn close(&mut self, sink: &mut dyn SseSink) -> Result<()> {
        if self.open.take().is_some() {
            sink.event(
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": self.index}),
            )?;
        }
        Ok(())
    }

    /// Index of the most recently opened block (callers emit deltas with it).
    pub fn index(&self) -> i64 {
        self.index
    }

    /// Emit a complete content block (server_tool_use / web_search_tool_result /
    /// tool_use with full input / text) at the next index, closing any open
    /// streamed block first.
    pub fn emit_standalone_block(&mut self, sink: &mut dyn SseSink, block: &Value) -> Result<()> {
        self.close(sink)?;
        self.index += 1;
        let index = self.index;
        let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        // Claude Code expects tool_use / server_tool_use to start with empty
        // input and receive arguments via input_json_delta. Dumping the full
        // input only in content_block_start leaves input={} → "missing
        // parameter" on every client tool.
        let start_block = match kind {
            "tool_use" | "server_tool_use" => {
                let mut b = block.clone();
                if let Some(obj) = b.as_object_mut() {
                    obj.insert("input".into(), serde_json::json!({}));
                }
                b
            }
            _ => block.clone(),
        };
        sink.event(
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": start_block
            }),
        )?;
        emit_block_payload(sink, index, block)?;
        sink.event(
            "content_block_stop",
            &serde_json::json!({"type": "content_block_stop", "index": index}),
        )?;
        Ok(())
    }
}

/// Delta payload for one complete content block at `index`:
/// text/thinking get a single delta; tool blocks get one concatenated
/// input_json_delta carrying the full input object.
pub fn emit_block_payload(sink: &mut dyn SseSink, index: i64, block: &Value) -> Result<()> {
    let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "text" => {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    sink.event(
                        "content_block_delta",
                        &serde_json::json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "text_delta", "text": text}
                        }),
                    )?;
                }
            }
        }
        "thinking" => {
            if let Some(th) = block.get("thinking").and_then(|t| t.as_str()) {
                if !th.is_empty() {
                    sink.event(
                        "content_block_delta",
                        &serde_json::json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "thinking_delta", "thinking": th}
                        }),
                    )?;
                }
            }
        }
        "tool_use" | "server_tool_use" => {
            let input = block.get("input").cloned().unwrap_or(serde_json::json!({}));
            let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
            if partial != "{}" {
                sink.event(
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": partial}
                    }),
                )?;
            }
        }
        // advisor_tool_result / web_search_tool_result / other full blocks:
        // start already carried the whole payload; no delta needed.
        _ => {}
    }
    Ok(())
}

/// Label mid-stream upstream failures. llama-server's tool-call parser has
/// known failure modes that look like Spock bugs — name the culprit.
pub fn label_mid_stream_upstream_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("invalid diff")
        || lower.contains("finding less tool calls")
        || lower.contains("tool call mismatch")
    {
        return format!("upstream stream error [llama-server tool-call parser, not Spock]: {raw}");
    }
    format!("upstream stream error: {raw}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(body: &str) -> String {
        body.lines().map(|l| format!("data: {l}\n\n")).collect()
    }

    struct Rec {
        events: Vec<(String, Value)>,
    }
    impl SseSink for Rec {
        fn event(&mut self, name: &str, data: &Value) -> Result<()> {
            self.events.push((name.to_string(), data.clone()));
            Ok(())
        }
    }
    fn rec() -> Rec {
        Rec { events: Vec::new() }
    }

    #[test]
    fn parses_vllm_reasoning_then_content() {
        let body = sse(r#"
            {"choices":[{"delta":{"reasoning":"think "}}]}
            {"choices":[{"delta":{"reasoning":"hard"}}]}
            {"choices":[{"delta":{"content":"Hi"}}]}
            {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3}}
            [DONE]
            "#);
        let evs: Vec<UpstreamEvent> = UpstreamEvents::new(body.as_bytes())
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(
            evs,
            vec![
                UpstreamEvent::Thinking("think ".into()),
                UpstreamEvent::Thinking("hard".into()),
                UpstreamEvent::Text("Hi".into()),
                UpstreamEvent::Usage(serde_json::json!({"prompt_tokens":5,"completion_tokens":3})),
                UpstreamEvent::Finish("stop".into()),
            ]
        );
    }

    #[test]
    fn reasoning_content_preferred_over_reasoning() {
        let body = sse(r#"{"choices":[{"delta":{"reasoning":"a","reasoning_content":"b"}}]}"#);
        let evs: Vec<UpstreamEvent> = UpstreamEvents::new(body.as_bytes())
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(evs, vec![UpstreamEvent::Thinking("b".into())]);
    }

    #[test]
    fn fragmented_tool_call_assembles_one_call() {
        let body = sse(r#"
            {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"Bash","arguments":"{\"comm"}}]}}]}
            {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}}]}
            {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}
            "#);
        let mut acc = ToolCallAccumulator::default();
        for ev in UpstreamEvents::new(body.as_bytes()) {
            if let UpstreamEvent::ToolCallFrag {
                index,
                id,
                name,
                arguments,
            } = ev.unwrap()
            {
                acc.push(index, &id, &name, &arguments);
            }
        }
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].name, "Bash");
        assert_eq!(calls[0].arguments, "{\"command\":\"ls\"}");
    }

    #[test]
    fn missing_ids_filled_uniquely() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(0, "", "advisor", "{}");
        acc.push(1, "", "web_search", "{\"query\":\"rust\"}");
        let calls = acc.finish();
        assert_ne!(calls[0].id, calls[1].id);
        assert!(calls[0].id.starts_with("toolu_") || !calls[0].id.is_empty());
    }

    #[test]
    fn nameless_fragments_dropped() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(0, "call_1", "", "{\"x\":1}");
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn mid_stream_error_object_surfaces() {
        let body = sse(r#"
            {"choices":[{"delta":{"content":"partial"}}]}
            {"error":{"message":"boom"}}
            {"choices":[{"delta":{"content":"never"}}]}
            "#);
        let mut it = UpstreamEvents::new(body.as_bytes());
        assert!(matches!(it.next(), Some(Ok(UpstreamEvent::Text(_)))));
        let err = it.next().unwrap().unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        assert!(it.next().is_none());
    }

    #[test]
    fn one_chunk_many_events_all_yielded() {
        // vLLM emits reasoning+content in one chunk; finish+usage share a
        // chunk; tool_calls can batch two calls. Nothing may be dropped.
        let body = sse(r#"
            {"choices":[{"delta":{"reasoning":"hmm","content":"answer"}}]}
            {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"advisor","arguments":"{}"}},{"index":1,"id":"b","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]}}]}
            {"choices":[{"delta":{}}],"usage":{"prompt_tokens":9,"completion_tokens":4},"finish":null}
            "#);
        let evs: Vec<UpstreamEvent> = UpstreamEvents::new(body.as_bytes())
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(
            evs,
            vec![
                UpstreamEvent::Thinking("hmm".into()),
                UpstreamEvent::Text("answer".into()),
                UpstreamEvent::ToolCallFrag {
                    index: 0,
                    id: "a".into(),
                    name: "advisor".into(),
                    arguments: "{}".into()
                },
                UpstreamEvent::ToolCallFrag {
                    index: 1,
                    id: "b".into(),
                    name: "Bash".into(),
                    arguments: "{\"command\":\"ls\"}".into()
                },
                UpstreamEvent::Usage(serde_json::json!({"prompt_tokens":9,"completion_tokens":4})),
            ]
        );
    }

    #[test]
    fn done_and_non_data_lines_terminate_or_skip() {
        let body = ": keepalive\n\ndata: [DONE]\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        assert!(UpstreamEvents::new(body.as_bytes()).next().is_none());
    }

    #[test]
    fn tracker_streams_blocks_with_continuous_index() {
        let mut sink = rec();
        let mut t = BlockTracker::new();
        t.ensure_thinking(&mut sink).unwrap();
        sink.event("content_block_delta", &serde_json::json!({"index": 0}))
            .unwrap();
        t.ensure_text(&mut sink).unwrap();
        t.close(&mut sink).unwrap();
        let names: Vec<&str> = sink.events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_stop"
            ]
        );
        let idx: Vec<i64> = sink
            .events
            .iter()
            .map(|(_, d)| d["index"].as_i64().unwrap_or(-99))
            .collect();
        assert_eq!(idx, vec![0, 0, 0, 1, 1]);
    }

    #[test]
    fn standalone_tool_block_uses_input_json_delta() {
        let mut sink = rec();
        let mut t = BlockTracker::new();
        t.ensure_text(&mut sink).unwrap();
        t.emit_standalone_block(
            &mut sink,
            &serde_json::json!({
                "type": "tool_use", "id": "call_1", "name": "Bash",
                "input": {"command": "ls"}
            }),
        )
        .unwrap();
        t.ensure_text(&mut sink).unwrap();
        let text = serde_json::to_string(&sink.events).unwrap();
        // start carries empty input, payload arrives via input_json_delta
        assert!(text.contains("\"input\":{}"), "{text}");
        assert!(text.contains("input_json_delta"), "{text}");
        let args_delta = sink
            .events
            .iter()
            .find(|(n, _)| n == "content_block_delta")
            .map(|(_, d)| d["delta"]["partial_json"].as_str().unwrap())
            .unwrap();
        assert_eq!(args_delta, "{\"command\":\"ls\"}");
        // block indexes: 0 text, 1 tool, 2 text — continuous, tool closed between
        let idx: Vec<i64> = sink
            .events
            .iter()
            .map(|(_, d)| d["index"].as_i64().unwrap_or(-99))
            .collect();
        assert_eq!(idx, vec![0, 0, 1, 1, 1, 2], "{text}");
    }
}

#!/usr/bin/env python3
"""Anthropic-API-compatible proxy backed by your Grok subscription.

Serves http://localhost:8048 and translates Anthropic Messages API calls
into xAI chat completions, authenticated with the OAuth token from
grok_test.py (run `python3 grok_test.py` once to log in).

Endpoints:
  POST /v1/messages               Anthropic Messages API (stream + non-stream)
  POST /v1/messages/count_tokens  rough estimate
  POST /v1/chat/completions       OpenAI-style passthrough (your curl works)
  GET  /v1/models                 available models (xAI passthrough)
  GET  /v1/models/{id}            single model details
  GET  /v1/language-models        xAI extended list: pricing, capabilities

Point Anthropic tooling at it:
  export ANTHROPIC_BASE_URL=http://localhost:8048
  export ANTHROPIC_AUTH_TOKEN=dummy   # proxy ignores it; xAI OAuth is used

Env overrides: PORT, GROK_MODEL (default grok-4.5),
GROK_SMALL_MODEL (for *haiku* background calls, defaults to GROK_MODEL),
XAI_API_BASE (upstream, for testing), XAI_TOKEN (skip OAuth, e.g. API key).
"""

import json
import os
import re
import shutil
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import grok_test as auth

PORT = int(os.environ.get("PORT", "8048"))
API_BASE = os.environ.get("XAI_API_BASE", "https://api.x.ai/v1")
DEFAULT_MODEL = os.environ.get("GROK_MODEL", "grok-4.5")
SMALL_MODEL = os.environ.get("GROK_SMALL_MODEL", DEFAULT_MODEL)

STOP_MAP = {"stop": "end_turn", "length": "max_tokens", "tool_calls": "tool_use"}
# Claude Code appends [1m]/[200k]/[500k] etc. as a context-window hint — not an
# xAI model id. Strip before upstream; keep the original id in Anthropic responses.
CTX_SUFFIX_RE = re.compile(r"\[[^\]]*\]$")

# IDs Claude Code / Auto Mode may request. Listed on GET /v1/models so the client
# does not treat them as "temporarily unavailable" when ANTHROPIC_BASE_URL is us.
CLAUDE_ALIASES = (
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-sonnet-4-5",
    "claude-haiku-4-5-20251001",
    "claude-haiku-4-5",
    "claude-fable-5",
    "claude-3-5-haiku-20241022",
    "claude-3-5-sonnet-20241022",
    "claude-3-7-sonnet-20250219",
)

_token = {"value": None, "until": 0}


def token():
    if os.environ.get("XAI_TOKEN"):
        return os.environ["XAI_TOKEN"]
    now = time.time()
    if _token["value"] and now < _token["until"]:
        return _token["value"]
    _token["value"] = auth.get_access_token()
    saved = auth.load_tokens() or {}
    _token["until"] = saved.get("expires_at", now + 300) - 120
    return _token["value"]


def strip_ctx_suffix(model):
    """Remove Claude Code context hints like [1m] / [1] / [500k]."""
    if not model:
        return model
    return CTX_SUFFIX_RE.sub("", model).strip() or model


def map_model(model):
    base = strip_ctx_suffix(model)
    if base and base.startswith("grok"):
        return base
    if base and "haiku" in base:
        return SMALL_MODEL
    return DEFAULT_MODEL


def model_card(model_id, owned_by="spox"):
    return {
        "id": model_id,
        "object": "model",
        "created": 0,
        "owned_by": owned_by,
        # Claude Code uses this for Auto Mode / model picker availability.
        "display_name": model_id,
        "type": "model",
    }


def alias_models():
    """Synthetic entries so Claude Code sees its usual model names as available."""
    out = []
    seen = set()
    # Context-window flavoured ids (client-side only; stripped before xAI).
    for mid in (DEFAULT_MODEL, SMALL_MODEL):
        if not mid:
            continue
        for cand in (mid, f"{mid}[1m]", f"{mid}[500k]"):
            if cand not in seen:
                seen.add(cand)
                out.append(model_card(cand, owned_by="xai"))
    for mid in CLAUDE_ALIASES:
        if mid not in seen:
            seen.add(mid)
            out.append(model_card(mid, owned_by="spox-alias"))
        tagged = f"{mid}[1m]"
        if tagged not in seen:
            seen.add(tagged)
            out.append(model_card(tagged, owned_by="spox-alias"))
    return out


class UpstreamError(Exception):
    def __init__(self, status, body):
        super().__init__(f"upstream {status}")
        self.status, self.body = status, body


def upstream_request(path, body=None, stream=False):
    headers = {
        "Accept": "text/event-stream" if stream else "application/json",
        "Authorization": f"Bearer {token()}",
        "User-Agent": auth.UA,
    }
    data = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    req = urllib.request.Request(f"{API_BASE}{path}", data=data, headers=headers)
    try:
        return urllib.request.urlopen(req, timeout=600)
    except urllib.error.HTTPError as e:
        text = e.read().decode(errors="replace")
        try:
            parsed = json.loads(text)
        except json.JSONDecodeError:
            parsed = {"error": {"message": text[:500]}}
        raise UpstreamError(e.code, parsed) from e


def upstream(body, stream=False):
    return upstream_request("/chat/completions", body, stream)


def sse_chunks(resp):
    for raw in resp:
        line = raw.strip()
        if not line.startswith(b"data:"):
            continue
        payload = line[5:].strip()
        if payload == b"[DONE]":
            break
        try:
            yield json.loads(payload)
        except json.JSONDecodeError:
            continue


# ---- Anthropic <-> OpenAI translation ----

def blocks_text(content):
    if isinstance(content, str):
        return content
    parts = []
    for b in content or []:
        if isinstance(b, dict) and b.get("type") == "text":
            parts.append(b.get("text", ""))
    return "\n".join(parts)


def thinking_effort(a):
    """Map Anthropic thinking config → xAI reasoning_effort, if any."""
    t = a.get("thinking")
    if not isinstance(t, dict):
        return None
    kind = t.get("type")
    if kind in (None, "disabled"):
        return "none"
    if kind not in ("enabled", "adaptive"):
        return None
    budget = t.get("budget_tokens")
    if not isinstance(budget, int):
        return "high"
    if budget < 5000:
        return "low"
    if budget < 15000:
        return "medium"
    return "high"


def anthropic_to_openai(a):
    msgs = []
    system = blocks_text(a.get("system"))
    if system:
        msgs.append({"role": "system", "content": system})

    for m in a.get("messages", []):
        role, content = m.get("role"), m.get("content")
        if isinstance(content, str):
            msgs.append({"role": role, "content": content})
            continue
        texts, images, tool_calls, tool_results, reasoning = [], [], [], [], []
        for b in content or []:
            kind = b.get("type")
            if kind == "text":
                texts.append(b.get("text", ""))
            elif kind == "image":
                src = b.get("source", {})
                if src.get("type") == "base64":
                    url = f"data:{src.get('media_type', 'image/png')};base64,{src.get('data', '')}"
                else:
                    url = src.get("url", "")
                images.append({"type": "image_url", "image_url": {"url": url}})
            elif kind == "tool_use":
                tool_calls.append({
                    "id": b.get("id", "call_" + uuid.uuid4().hex[:12]),
                    "type": "function",
                    "function": {"name": b.get("name", ""),
                                 "arguments": json.dumps(b.get("input", {}))},
                })
            elif kind == "tool_result":
                text = blocks_text(b.get("content", ""))
                if b.get("is_error"):
                    text = "Error: " + text
                tool_results.append({"role": "tool",
                                     "tool_call_id": b.get("tool_use_id", ""),
                                     "content": text})
            elif kind == "thinking":
                # Round-trip prior thinking as xAI reasoning_content.
                if b.get("thinking"):
                    reasoning.append(b["thinking"])
            # redacted_thinking has no recoverable text — skip

        msgs.extend(tool_results)
        text = "\n".join(texts)
        if role == "assistant":
            if text or tool_calls or reasoning:
                msg = {"role": "assistant", "content": text or None}
                if tool_calls:
                    msg["tool_calls"] = tool_calls
                if reasoning:
                    msg["reasoning_content"] = "\n".join(reasoning)
                msgs.append(msg)
        elif images:
            parts = ([{"type": "text", "text": text}] if text else []) + images
            msgs.append({"role": "user", "content": parts})
        elif text:
            msgs.append({"role": "user", "content": text})

    req = {"model": map_model(a.get("model")), "messages": msgs,
           "max_tokens": a.get("max_tokens", 1024)}
    for key in ("temperature", "top_p"):
        if key in a:
            req[key] = a[key]
    if a.get("stop_sequences"):
        req["stop"] = a["stop_sequences"]
    if a.get("tools"):
        req["tools"] = [
            {"type": "function",
             "function": {"name": t.get("name", ""),
                          "description": t.get("description", ""),
                          "parameters": t.get("input_schema", {"type": "object"})}}
            for t in a["tools"] if isinstance(t, dict) and "input_schema" in t
        ]
    tc = a.get("tool_choice") or {}
    kind = tc.get("type")
    if kind == "tool":
        req["tool_choice"] = {"type": "function", "function": {"name": tc.get("name", "")}}
    elif kind == "any":
        req["tool_choice"] = "required"
    elif kind == "none":
        req["tool_choice"] = "none"
    elif kind == "auto":
        req["tool_choice"] = "auto"
    effort = thinking_effort(a)
    if effort is not None:
        req["reasoning_effort"] = effort
    return req


def openai_to_anthropic(o, req_model):
    choice = (o.get("choices") or [{}])[0]
    msg = choice.get("message", {})
    content = []
    # Anthropic order: thinking → text → tool_use
    reasoning = msg.get("reasoning_content")
    if reasoning:
        content.append({"type": "thinking", "thinking": reasoning})
    if msg.get("content"):
        content.append({"type": "text", "text": msg["content"]})
    for tc in msg.get("tool_calls") or []:
        fn = tc.get("function", {})
        try:
            args = json.loads(fn.get("arguments") or "{}")
        except json.JSONDecodeError:
            args = {"_raw": fn.get("arguments")}
        content.append({"type": "tool_use",
                        "id": tc.get("id") or "toolu_" + uuid.uuid4().hex[:12],
                        "name": fn.get("name", ""), "input": args})
    usage = o.get("usage") or {}
    details = usage.get("completion_tokens_details") or {}
    out = usage.get("completion_tokens", 0)
    # Prefer total completion tokens; reasoning is already inside that count on xAI.
    if not out and details.get("reasoning_tokens"):
        out = details["reasoning_tokens"]
    return {
        "id": o.get("id") or "msg_" + uuid.uuid4().hex[:16],
        "type": "message",
        "role": "assistant",
        "model": req_model,
        "content": content,
        "stop_reason": STOP_MAP.get(choice.get("finish_reason"), "end_turn"),
        "stop_sequence": None,
        "usage": {"input_tokens": usage.get("prompt_tokens", 0),
                  "output_tokens": out},
    }


# ---- HTTP server ----

class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        print(f"  {self.command} {self.path} — {args[1] if len(args) > 1 else ''}")

    def _json(self, status, obj):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _error(self, status, message, err_type="api_error"):
        self._json(status, {"type": "error",
                            "error": {"type": err_type, "message": message}})

    def handle_models(self, path):
        """Serve /v1/models[/{id}] with xAI list + Claude/Grok aliases.

        Auto Mode asks for claude-opus-4-8 (and friends). Messages already map
        those to Grok, but a 404 on GET /v1/models/{id} makes Claude Code treat
        the classifier model as 'temporarily unavailable'.
        """
        rest = path[len("/v1/models"):].lstrip("/")
        model_id = urllib.parse.unquote(rest) if rest else ""

        if model_id:
            # Prefer a synthetic card for anything we can map; only hit xAI for
            # bare upstream ids so aliases never 404.
            upstream_id = map_model(model_id)
            try:
                data = upstream_request(f"/models/{urllib.parse.quote(upstream_id, safe='')}").read()
                card = json.loads(data)
                # Echo the id the client asked for (incl. [1m] / claude-*).
                if isinstance(card, dict):
                    card = dict(card)
                    card["id"] = model_id
                    if "display_name" not in card:
                        card["display_name"] = model_id
                return self._json(200, card)
            except UpstreamError:
                return self._json(200, model_card(model_id))

        # List: xAI models + our aliases (dedup by id, aliases win display).
        try:
            raw = json.loads(upstream_request("/models").read())
        except UpstreamError as e:
            # Still advertise aliases so Auto Mode has something to resolve.
            raw = {"object": "list", "data": []}
            detail = e.body.get("error", e.body) if isinstance(e.body, dict) else e.body
            print(f"  models upstream failed: {detail}")
        data = list(raw.get("data") or [])
        by_id = {m.get("id"): m for m in data if isinstance(m, dict) and m.get("id")}
        for card in alias_models():
            by_id[card["id"]] = card
        raw["object"] = raw.get("object") or "list"
        raw["data"] = list(by_id.values())
        return self._json(200, raw)

    def do_GET(self):
        path = self.path.split("?")[0]
        try:
            if path in ("/", "/health"):
                self._json(200, {"status": "ok", "backend": API_BASE, "model": DEFAULT_MODEL})
            elif path == "/v1/models" or path.startswith("/v1/models/"):
                self.handle_models(path)
            elif path.startswith("/v1/language-models"):
                data = upstream_request(path[len("/v1"):]).read()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            else:
                self._error(404, f"not found: {path}", "not_found_error")
        except UpstreamError as e:
            detail = e.body.get("error", e.body)
            msg = detail.get("message", str(detail)) if isinstance(detail, dict) else str(detail)
            self._error(e.status, msg)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        try:
            body = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError:
            return self._error(400, "invalid JSON body", "invalid_request_error")
        path = self.path.split("?")[0]
        try:
            if path == "/v1/messages":
                self.handle_messages(body)
            elif path == "/v1/messages/count_tokens":
                est = len(json.dumps(body.get("messages", [])) + str(body.get("system", ""))) // 4
                self._json(200, {"input_tokens": est})
            elif path == "/v1/chat/completions":
                self.handle_openai(body)
            else:
                self._error(404, f"not found: {path}", "not_found_error")
        except UpstreamError as e:
            detail = e.body.get("error", e.body)
            msg = detail.get("message", str(detail)) if isinstance(detail, dict) else str(detail)
            if e.status == 401:
                msg += " — token rejected; re-auth with: python3 grok_test.py --logout && python3 grok_test.py"
            self._error(e.status, msg)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as e:  # noqa: BLE001
            self._error(500, f"{type(e).__name__}: {e}")

    def handle_openai(self, body):
        body["model"] = map_model(body.get("model"))
        resp = upstream(body, stream=bool(body.get("stream")))
        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            shutil.copyfileobj(resp, self.wfile)
        else:
            data = resp.read()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    def handle_messages(self, a):
        oai = anthropic_to_openai(a)
        req_model = a.get("model", DEFAULT_MODEL)
        if not a.get("stream"):
            o = json.loads(upstream(oai).read())
            return self._json(200, openai_to_anthropic(o, req_model))

        oai["stream"] = True
        resp = upstream(oai, stream=True)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def emit(event, data):
            self.wfile.write(f"event: {event}\ndata: {json.dumps(data)}\n\n".encode())
            self.wfile.flush()

        emit("message_start", {"type": "message_start", "message": {
            "id": "msg_" + uuid.uuid4().hex[:16], "type": "message",
            "role": "assistant", "model": req_model, "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 0, "output_tokens": 0}}})

        block, index, chunks_out, finish, usage = None, -1, 0, None, {}
        for chunk in sse_chunks(resp):
            if chunk.get("usage"):
                usage = chunk["usage"]
            choice = (chunk.get("choices") or [{}])[0]
            delta = choice.get("delta") or {}
            if choice.get("finish_reason"):
                finish = choice["finish_reason"]

            # xAI streams reasoning first as delta.reasoning_content, then content.
            reasoning = delta.get("reasoning_content")
            if reasoning:
                if block != "thinking":
                    if block:
                        emit("content_block_stop", {"type": "content_block_stop", "index": index})
                    index, block = index + 1, "thinking"
                    emit("content_block_start", {"type": "content_block_start", "index": index,
                                                 "content_block": {"type": "thinking", "thinking": ""}})
                emit("content_block_delta", {"type": "content_block_delta", "index": index,
                                             "delta": {"type": "thinking_delta", "thinking": reasoning}})
                chunks_out += 1

            if delta.get("content"):
                if block != "text":
                    if block:
                        emit("content_block_stop", {"type": "content_block_stop", "index": index})
                    index, block = index + 1, "text"
                    emit("content_block_start", {"type": "content_block_start", "index": index,
                                                 "content_block": {"type": "text", "text": ""}})
                emit("content_block_delta", {"type": "content_block_delta", "index": index,
                                             "delta": {"type": "text_delta", "text": delta["content"]}})
                chunks_out += 1

            for tc in delta.get("tool_calls") or []:
                fn = tc.get("function") or {}
                if tc.get("id") or fn.get("name"):
                    if block:
                        emit("content_block_stop", {"type": "content_block_stop", "index": index})
                    index, block = index + 1, "tool"
                    emit("content_block_start", {"type": "content_block_start", "index": index,
                                                 "content_block": {"type": "tool_use",
                                                                   "id": tc.get("id") or "toolu_" + uuid.uuid4().hex[:12],
                                                                   "name": fn.get("name", ""), "input": {}}})
                if fn.get("arguments"):
                    emit("content_block_delta", {"type": "content_block_delta", "index": index,
                                                 "delta": {"type": "input_json_delta",
                                                           "partial_json": fn["arguments"]}})
                    chunks_out += 1

        if block:
            emit("content_block_stop", {"type": "content_block_stop", "index": index})
        out_tokens = usage.get("completion_tokens", chunks_out)
        emit("message_delta", {"type": "message_delta",
                               "delta": {"stop_reason": STOP_MAP.get(finish, "end_turn"),
                                         "stop_sequence": None},
                               "usage": {"output_tokens": out_tokens}})
        emit("message_stop", {"type": "message_stop"})


def main():
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"Grok proxy (Anthropic-compatible) on http://localhost:{PORT}")
    print(f"  upstream: {API_BASE}  default model: {DEFAULT_MODEL}")
    print(f"  POST /v1/messages | /v1/chat/completions | Ctrl-C to stop\n")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nstopped")


if __name__ == "__main__":
    main()

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
import shutil
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import grok_test as auth

PORT = int(os.environ.get("PORT", "8048"))
API_BASE = os.environ.get("XAI_API_BASE", "https://api.x.ai/v1")
DEFAULT_MODEL = os.environ.get("GROK_MODEL", "grok-4.5")
SMALL_MODEL = os.environ.get("GROK_SMALL_MODEL", DEFAULT_MODEL)

STOP_MAP = {"stop": "end_turn", "length": "max_tokens", "tool_calls": "tool_use"}

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


def map_model(model):
    if model and model.startswith("grok"):
        return model
    if model and "haiku" in model:
        return SMALL_MODEL
    return DEFAULT_MODEL


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
        texts, images, tool_calls, tool_results = [], [], [], []
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
            # thinking / redacted_thinking blocks are dropped

        msgs.extend(tool_results)
        text = "\n".join(texts)
        if role == "assistant":
            if text or tool_calls:
                msg = {"role": "assistant", "content": text or None}
                if tool_calls:
                    msg["tool_calls"] = tool_calls
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
    return req


def openai_to_anthropic(o, req_model):
    choice = (o.get("choices") or [{}])[0]
    msg = choice.get("message", {})
    content = []
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
    return {
        "id": o.get("id") or "msg_" + uuid.uuid4().hex[:16],
        "type": "message",
        "role": "assistant",
        "model": req_model,
        "content": content,
        "stop_reason": STOP_MAP.get(choice.get("finish_reason"), "end_turn"),
        "stop_sequence": None,
        "usage": {"input_tokens": usage.get("prompt_tokens", 0),
                  "output_tokens": usage.get("completion_tokens", 0)},
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

    def do_GET(self):
        path = self.path.split("?")[0]
        try:
            if path in ("/", "/health"):
                self._json(200, {"status": "ok", "backend": API_BASE, "model": DEFAULT_MODEL})
            elif path.startswith(("/v1/models", "/v1/language-models")):
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
        emit("message_delta", {"type": "message_delta",
                               "delta": {"stop_reason": STOP_MAP.get(finish, "end_turn"),
                                         "stop_sequence": None},
                               "usage": {"output_tokens": usage.get("completion_tokens", chunks_out)}})
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

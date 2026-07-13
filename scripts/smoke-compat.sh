#!/usr/bin/env bash
# Spock smoke after Claude Code or Spock upgrade. Loopback only.
set -euo pipefail
BASE="${SPOCK_BASE:-http://127.0.0.1:8048}"

echo "== health =="
curl -sf "$BASE/health" | tee /tmp/spock-smoke-health.json
echo

echo "== status auth =="
curl -sf "$BASE/spock/v1/status" | tee /tmp/spock-smoke-status.json
echo

echo "== models (aliases present?) =="
curl -sf "$BASE/v1/models" | python3 -c "import json,sys; d=json.load(sys.stdin); ids=[x.get('id') for x in d.get('data',[])];
print('count', len(ids));
print('opus', any(i and 'opus' in i for i in ids));
print('haiku', any(i and 'haiku' in i for i in ids))"

echo "== simple messages =="
curl -sf "$BASE/v1/messages" \
  -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"claude-opus-4-8","max_tokens":32,"messages":[{"role":"user","content":"Say OK"}]}' \
  | python3 -c "import json,sys; d=json.load(sys.stdin); assert not d.get('error'), d; print('stop', d.get('stop_reason'))"

echo "== tool_choice hygiene (server tool only — must not 400) =="
curl -sf "$BASE/v1/messages" \
  -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"claude-opus-4-8","max_tokens":32,"tools":[{"type":"web_search_20250305","name":"web_search"}],"tool_choice":{"type":"tool","name":"web_search"},"messages":[{"role":"user","content":"hi"}]}' \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print('ok-or-text', bool(d.get('content') or d.get('error')))"

echo "SMOKE OK"

#!/usr/bin/env python3
"""Link a Grok subscription to the xAI API via OAuth device-code login.

Same flow OpenClaw uses (RFC 8628, xAI's shared "Grok Build" OAuth client):
  1. First run: prints a URL + short code, you approve in any browser
     while logged in to your Grok/X account. No API key involved.
  2. Tokens are cached in ~/.config/grok-test/auth.json (chmod 600)
     and auto-refreshed on later runs.
  3. Sends a chat completion to https://api.x.ai/v1 and prints the reply.

Usage:
  python3 grok_test.py                          # default test prompt
  python3 grok_test.py "explain gossipsub"      # your own prompt
  python3 grok_test.py --model grok-4.3 "hi"    # pick a model
  python3 grok_test.py --logout                 # forget cached tokens
"""

import json
import os
import stat
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser

# xAI's shared OAuth client (the one grok-cli / OpenClaw use; consent screen
# may say "Grok Build"). Public client — no secret, that's how device flow works.
CLIENT_ID = "b1a00492-073a-47ea-816f-4c329264a828"
SCOPE = "openid profile email offline_access grok-cli:access api:access"
DISCOVERY_URL = "https://auth.x.ai/.well-known/openid-configuration"
API_BASE = "https://api.x.ai/v1"
DEFAULT_MODEL = "grok-4.3"
DEVICE_GRANT = "urn:ietf:params:oauth:grant-type:device_code"
AUTH_FILE = os.path.expanduser("~/.config/grok-test/auth.json")
UA = "grok-test/0.1"


def http_json(url, form=None, bearer=None, body=None):
    headers = {"Accept": "application/json", "User-Agent": UA}
    data = None
    if form is not None:
        headers["Content-Type"] = "application/x-www-form-urlencoded"
        data = urllib.parse.urlencode(form).encode()
    elif body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    if bearer:
        headers["Authorization"] = f"Bearer {bearer}"
    req = urllib.request.Request(url, data=data, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {}


def save_tokens(tokens):
    os.makedirs(os.path.dirname(AUTH_FILE), exist_ok=True)
    if "expires_in" in tokens:
        tokens["expires_at"] = time.time() + tokens["expires_in"]
    with open(AUTH_FILE, "w") as f:
        json.dump(tokens, f, indent=2)
    os.chmod(AUTH_FILE, stat.S_IRUSR | stat.S_IWUSR)


def load_tokens():
    try:
        with open(AUTH_FILE) as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def device_login(endpoints):
    status, dc = http_json(
        endpoints["device_authorization_endpoint"],
        form={"client_id": CLIENT_ID, "scope": SCOPE},
    )
    if status != 200:
        sys.exit(f"device code request failed ({status}): {dc}")

    url = dc.get("verification_uri_complete") or dc["verification_uri"]
    print("\n  Open this URL in your browser (logged in to your Grok/X account):")
    print(f"\n    {url}")
    print(f"\n  Code: {dc['user_code']}\n")
    webbrowser.open(url)

    interval = dc.get("interval", 5)
    deadline = time.time() + dc.get("expires_in", 300)
    print("  Waiting for approval", end="", flush=True)
    while time.time() < deadline:
        time.sleep(max(interval, 1))
        print(".", end="", flush=True)
        status, tok = http_json(
            endpoints["token_endpoint"],
            form={
                "grant_type": DEVICE_GRANT,
                "client_id": CLIENT_ID,
                "device_code": dc["device_code"],
            },
        )
        if status == 200:
            print(" approved.\n")
            return tok
        err = tok.get("error")
        if err == "authorization_pending":
            continue
        if err == "slow_down":
            interval += 5
            continue
        sys.exit(f"\nauthorization failed: {err or status}")
    sys.exit("\ndevice code expired — run again")


def refresh(endpoints, tokens):
    status, tok = http_json(
        endpoints["token_endpoint"],
        form={
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": tokens["refresh_token"],
        },
    )
    if status != 200:
        return None
    tok.setdefault("refresh_token", tokens["refresh_token"])
    return tok


def get_access_token():
    status, disco = http_json(DISCOVERY_URL)
    if status != 200:
        sys.exit(f"OAuth discovery failed ({status})")

    tokens = load_tokens()
    if tokens and time.time() < tokens.get("expires_at", 0) - 60:
        return tokens["access_token"]
    if tokens and tokens.get("refresh_token"):
        fresh = refresh(disco, tokens)
        if fresh:
            save_tokens(fresh)
            return fresh["access_token"]
        print("  Token refresh failed — logging in again.")
    tokens = device_login(disco)
    save_tokens(tokens)
    return tokens["access_token"]


def main():
    args = sys.argv[1:]
    if "--logout" in args:
        try:
            os.remove(AUTH_FILE)
            print("Logged out (cached tokens removed).")
        except FileNotFoundError:
            print("Nothing to remove.")
        return
    model = DEFAULT_MODEL
    if "--model" in args:
        i = args.index("--model")
        model = args[i + 1]
        del args[i : i + 2]
    prompt = " ".join(args) or "how are you doing? what model are you and what can you do?"

    token = get_access_token()
    print(f"  Model: {model}\n  Prompt: {prompt}\n")
    status, resp = http_json(
        f"{API_BASE}/chat/completions",
        bearer=token,
        body={
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 256,
        },
    )
    if status != 200:
        sys.exit(f"chat completion failed ({status}): {json.dumps(resp, indent=2)}")

    print("─" * 60)
    print(resp["choices"][0]["message"]["content"].strip())
    print("─" * 60)
    usage = resp.get("usage", {})
    print(f"[{resp.get('model', model)}] tokens: {usage.get('prompt_tokens', '?')} in / "
          f"{usage.get('completion_tokens', '?')} out")


if __name__ == "__main__":
    main()

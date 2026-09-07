//! Generic OAuth 2.0 device-code + refresh (RFC 8628), optional PKCE S256.

use crate::error::{Error, Result};
use crate::oauth::registry::{request_headers, AuthEndpoints, DeviceCtx, ProviderDef};
use crate::oauth::store::{now_secs, save_tokens, TokenSet};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::thread;
use std::time::Duration;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// RFC 7636 PKCE pair (S256).
fn generate_pkce() -> Result<(String, String)> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| Error::Msg(format!("getrandom: {e}")))?;
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

#[derive(Debug, Deserialize)]
struct Discovery {
    device_authorization_endpoint: String,
    token_endpoint: String,
}

struct Endpoints {
    device_authorization: String,
    token: String,
}

fn agent(user_agent: &str) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .user_agent(user_agent)
        .build()
}

fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form_post(
    agent: &ureq::Agent,
    url: &str,
    form: &[(&str, &str)],
    extra: &std::collections::BTreeMap<String, String>,
) -> Result<(u16, Value)> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding_lite(k), urlencoding_lite(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut req = agent
        .post(url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", "application/json");
    for (k, v) in extra {
        // Don't let extra override Content-Type.
        if k.eq_ignore_ascii_case("content-type") {
            continue;
        }
        req = req.set(k, v);
    }
    match req.send_string(&body) {
        Ok(resp) => {
            let status = resp.status();
            let v: Value = resp.into_json().unwrap_or(Value::Null);
            Ok((status, v))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let v: Value = resp.into_json().unwrap_or(Value::Null);
            Ok((code, v))
        }
        Err(e) => Err(Error::Msg(format!("http: {e}"))),
    }
}

fn get_json(agent: &ureq::Agent, url: &str) -> Result<(u16, Value)> {
    match agent.get(url).set("Accept", "application/json").call() {
        Ok(resp) => {
            let status = resp.status();
            let v: Value = resp.into_json().unwrap_or(Value::Null);
            Ok((status, v))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let v: Value = resp.into_json().unwrap_or(Value::Null);
            Ok((code, v))
        }
        Err(e) => Err(Error::Msg(format!("http: {e}"))),
    }
}

fn resolve_endpoints(provider: &ProviderDef, agent: &ureq::Agent) -> Result<Endpoints> {
    match provider.auth {
        AuthEndpoints::Discovery { url } => {
            let (status, v) = get_json(agent, url)?;
            if status != 200 {
                return Err(Error::Auth(format!(
                    "{} OAuth discovery failed ({status})",
                    provider.id
                )));
            }
            let d: Discovery = serde_json::from_value(v)
                .map_err(|e| Error::Auth(format!("discovery parse: {e}")))?;
            Ok(Endpoints {
                device_authorization: d.device_authorization_endpoint,
                token: d.token_endpoint,
            })
        }
        AuthEndpoints::Fixed { device_auth, token } => Ok(Endpoints {
            device_authorization: device_auth.to_string(),
            token: token.to_string(),
        }),
    }
}

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        ("open", vec![url.to_string()])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C".into(), "start".into(), url.to_string()])
    } else {
        ("xdg-open", vec![url.to_string()])
    };
    let _ = std::process::Command::new(cmd.0).args(&cmd.1).spawn();
}

fn token_set_from_json(tok: Value) -> Result<TokenSet> {
    let access = tok
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Auth("missing access_token".into()))?
        .to_string();
    let refresh = tok
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let expires_in = tok
        .get("expires_in")
        .and_then(|v| v.as_f64())
        // Some gateways return expires_in as integer-like string.
        .or_else(|| {
            tok.get("expires_in")
                .and_then(|v| v.as_i64())
                .map(|i| i as f64)
        })
        .or_else(|| {
            tok.get("expires_in")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
        });
    // Absolute expires_at if provided (seconds or ms).
    let expires_at_abs = tok
        .get("expires_at")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            tok.get("expires_at")
                .and_then(|v| v.as_i64())
                .map(|i| i as f64)
        })
        // Qwen oauth_creds.json uses expiry_date as ms epoch.
        .or_else(|| {
            tok.get("expiry_date").and_then(|v| v.as_f64()).or_else(|| {
                tok.get("expiry_date")
                    .and_then(|v| v.as_i64())
                    .map(|i| i as f64)
            })
        })
        .map(crate::oauth::store::normalize_expires_at);
    let expires_at = expires_in
        .map(|e| now_secs() + e)
        .or(expires_at_abs)
        // Default 1h so Proxy mode doesn't treat brand-new tokens as expired.
        .or(Some(now_secs() + 3600.0));
    let mut set = TokenSet {
        access_token: access,
        refresh_token: refresh,
        expires_in: expires_in.or(Some(3600.0)),
        expires_at,
        extra: Default::default(),
    };
    // Preserve unknown fields lightly.
    if let Some(obj) = tok.as_object() {
        for (k, v) in obj {
            if matches!(
                k.as_str(),
                "access_token" | "refresh_token" | "expires_in" | "expires_at"
            ) {
                continue;
            }
            set.extra.insert(k.clone(), v.clone());
        }
    }
    Ok(set)
}

/// Interactive device login. Opens browser when `open` is true.
pub fn device_login(provider: &ProviderDef, open: bool) -> Result<TokenSet> {
    let ctx = DeviceCtx::current();
    let headers = request_headers(provider, &ctx);
    let agent = agent(provider.user_agent);
    let endpoints = resolve_endpoints(provider, &agent)?;

    // Owned PKCE strings so form slices can borrow them for the whole login.
    let pkce = if provider.pkce {
        Some(generate_pkce()?)
    } else {
        None
    };
    let (code_verifier, code_challenge) = match &pkce {
        Some((v, c)) => (Some(v.as_str()), Some(c.as_str())),
        None => (None, None),
    };

    let mut form: Vec<(&str, &str)> = vec![("client_id", provider.client_id)];
    if let Some(scope) = provider.scope {
        form.push(("scope", scope));
    }
    if let Some(ch) = code_challenge {
        form.push(("code_challenge", ch));
        form.push(("code_challenge_method", "S256"));
    }
    let (status, dc) = form_post(&agent, &endpoints.device_authorization, &form, &headers)?;
    if status != 200 {
        return Err(Error::Auth(format!(
            "{} device code request failed ({status}): {dc}",
            provider.id
        )));
    }
    let device_code = dc["device_code"]
        .as_str()
        .ok_or_else(|| {
            // The auth.openai.com device endpoint sits behind a Cloudflare
            // managed challenge that plain HTTP cannot pass (the body is HTML,
            // parsed as Null). Give a real remedy instead of a bare parse error.
            let hint = if provider.id == "openai" {
                " — auth.openai.com is behind a Cloudflare challenge; log in through the \
                 Codex app/CLI (`codex login`) so ~/.codex/auth.json is fresh and Spock \
                 imports it, or skip OAuth and set an api_key"
            } else {
                ""
            };
            Error::Auth(format!("missing device_code from {} device auth{hint}", provider.id))
        })?
        .to_string();
    let user_code = dc["user_code"].as_str().unwrap_or("?").to_string();
    let url = dc
        .get("verification_uri_complete")
        .and_then(|v| v.as_str())
        .or_else(|| dc.get("verification_uri").and_then(|v| v.as_str()))
        .ok_or_else(|| Error::Auth("missing verification_uri".into()))?
        .to_string();

    eprintln!("\n  {} — open this URL in your browser:\n", provider.label);
    eprintln!("    {url}");
    eprintln!("\n  Code: {user_code}\n");
    if open {
        open_browser(&url);
    }

    let mut interval = dc["interval"].as_f64().unwrap_or(5.0).max(1.0);
    let expires_in = dc["expires_in"].as_f64().unwrap_or(300.0);
    let deadline = now_secs() + expires_in;

    eprint!("  Waiting for approval");
    let _ = std::io::stderr().flush();
    while now_secs() < deadline {
        thread::sleep(Duration::from_secs_f64(interval));
        eprint!(".");
        let _ = std::io::stderr().flush();
        let mut token_form: Vec<(&str, &str)> = vec![
            ("grant_type", DEVICE_GRANT),
            ("client_id", provider.client_id),
            ("device_code", &device_code),
        ];
        if let Some(v) = code_verifier {
            token_form.push(("code_verifier", v));
        }
        let (st, tok) = form_post(&agent, &endpoints.token, &token_form, &headers)?;
        if st == 200 {
            eprintln!(" approved.\n");
            return token_set_from_json(tok);
        }
        // Transient gateway errors while waiting for user approval (Qwen has 504'd here).
        if (500..600).contains(&st) {
            interval = (interval + 2.0).min(15.0);
            continue;
        }
        let err = tok.get("error").and_then(|e| e.as_str()).unwrap_or("");
        if err == "authorization_pending" {
            continue;
        }
        if err == "slow_down" {
            interval += 5.0;
            continue;
        }
        if err == "expired_token" {
            return Err(Error::Auth("device code expired — run again".into()));
        }
        return Err(Error::Auth(format!(
            "authorization failed: {} ({st})",
            if err.is_empty() {
                st.to_string()
            } else {
                err.to_string()
            }
        )));
    }
    Err(Error::Auth("device code expired — run again".into()))
}

/// Refresh access token. Returns None if no refresh token or refresh rejected.
pub fn refresh(provider: &ProviderDef, tokens: &TokenSet) -> Result<Option<TokenSet>> {
    let Some(rt) = tokens.refresh_token.as_deref() else {
        return Ok(None);
    };
    let ctx = DeviceCtx::current();
    let headers = request_headers(provider, &ctx);
    let agent = agent(provider.user_agent);
    let endpoints = resolve_endpoints(provider, &agent)?;
    let (st, tok) = form_post(
        &agent,
        &endpoints.token,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", provider.client_id),
            ("refresh_token", rt),
        ],
        &headers,
    )?;
    if st != 200 {
        return Ok(None);
    }
    let mut set = token_set_from_json(tok)?;
    if set.refresh_token.is_none() {
        set.refresh_token = tokens.refresh_token.clone();
    }
    // Preserve Qwen resource_url / endpoint if refresh omits them.
    for key in ["resource_url", "endpoint"] {
        if !set.extra.contains_key(key) {
            if let Some(v) = tokens.extra.get(key) {
                set.extra.insert(key.to_string(), v.clone());
            }
        }
    }
    Ok(Some(set))
}

/// Percent-encode a query value (space → %20, not `+` — OpenAI's auth server
/// rejects `+`-encoded spaces).
fn query_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).ok();
    use std::fmt::Write as _;
    let mut s = String::new();
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

fn query_get(query: &str, key: &str) -> Option<String> {
    let q = query.split_once('?').map(|(_, q)| q).unwrap_or(query);
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

/// Login via the browser **authorization-code PKCE** flow. A real browser is
/// the user agent, so any Cloudflare challenge passes; Spock then exchanges
/// the code at the token endpoint (which is not challenged) and saves the
/// tokens. No third-party CLI or app is required — used for providers that
/// carry an `authorize_url` (currently OpenAI/Codex).
pub fn pkce_login(provider: &ProviderDef, open: bool) -> Result<TokenSet> {
    use std::io::{BufRead as _, Write as _};
    use std::net::TcpListener;

    let port = provider.callback_port.max(1);
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let mut vbuf = vec![0u8; 32];
    getrandom::getrandom(&mut vbuf).ok();
    let code_verifier = URL_SAFE_NO_PAD.encode(&vbuf);
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let state = rand_hex(10);

    let authorize_url = provider
        .authorize_url
        .unwrap_or("https://auth.openai.com/oauth/authorize");
    let params = [
        ("response_type", "code"),
        ("client_id", provider.client_id),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", provider.scope.unwrap_or("openid profile email offline_access")),
        ("code_challenge", code_challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state.as_str()),
        ("originator", "codex_cli_rs"),
    ];
    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", query_encode(k), query_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let url = format!("{authorize_url}?{qs}");

    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        Error::Auth(format!(
            "could not bind localhost:{port} for the OAuth callback (is something else on it?): {e}"
        ))
    })?;
    listener.set_nonblocking(true).ok();

    eprintln!("\n  {} — a browser window should open for sign-in.\n", provider.label);
    if open {
        open_browser(&url);
    } else {
        eprintln!("    {url}\n");
    }

    let deadline = now_secs() + 300.0;
    let stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if now_secs() > deadline {
                    return Err(Error::Auth(
                        "OAuth callback timed out — no browser approval received".into(),
                    ));
                }
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(Error::Auth(format!("OAuth callback accept error: {e}"))),
        }
    };

    let mut rd = std::io::BufReader::new(stream);
    let mut req_line = String::new();
    rd.read_line(&mut req_line).ok();
    let target = req_line.split(' ').nth(1).unwrap_or("/").to_string();
    let mut resp = rd.into_inner();
    let _ = resp.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
          <html><body><h2>Spock</h2><p>Signed in. You can close this window and return to the terminal.</p></body></html>",
    );

    let code = query_get(&target, "code")
        .ok_or_else(|| Error::Auth("OAuth callback carried no `code`".into()))?
        .to_string();
    if let Some(s) = query_get(&target, "state") {
        if s != state {
            return Err(Error::Auth("OAuth callback state mismatch (stale page?)".into()));
        }
    }

    let ctx = DeviceCtx::current();
    let headers = request_headers(provider, &ctx);
    let agent = agent(provider.user_agent);
    let endpoints = resolve_endpoints(provider, &agent)?;
    let (st, tok) = form_post(
        &agent,
        &endpoints.token,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", provider.client_id),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", code_verifier.as_str()),
        ],
        &headers,
    )?;
    if st != 200 {
        return Err(Error::Auth(format!(
            "{} token exchange failed ({st}): {tok}",
            provider.id
        )));
    }
    token_set_from_json(tok)
}

/// Full interactive login + save. Providers with an `authorize_url` use the
/// browser PKCE flow; the rest use the device-code flow.
pub fn login_and_save(provider: &ProviderDef, open: bool) -> Result<TokenSet> {
    let mut set = if provider.authorize_url.is_some() {
        pkce_login(provider, open)?
    } else {
        device_login(provider, open)?
    };
    save_tokens(provider.id, &mut set)?;
    Ok(set)
}

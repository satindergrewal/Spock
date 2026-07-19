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
        AuthEndpoints::Fixed {
            device_auth,
            token,
        } => Ok(Endpoints {
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
        .or_else(|| tok.get("expires_at").and_then(|v| v.as_i64()).map(|i| i as f64))
        // Qwen oauth_creds.json uses expiry_date as ms epoch.
        .or_else(|| {
            tok.get("expiry_date")
                .and_then(|v| v.as_f64())
                .or_else(|| tok.get("expiry_date").and_then(|v| v.as_i64()).map(|i| i as f64))
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
    let (status, dc) = form_post(
        &agent,
        &endpoints.device_authorization,
        &form,
        &headers,
    )?;
    if status != 200 {
        return Err(Error::Auth(format!(
            "{} device code request failed ({status}): {dc}",
            provider.id
        )));
    }
    let device_code = dc["device_code"]
        .as_str()
        .ok_or_else(|| Error::Auth("missing device_code".into()))?
        .to_string();
    let user_code = dc["user_code"].as_str().unwrap_or("?").to_string();
    let url = dc
        .get("verification_uri_complete")
        .and_then(|v| v.as_str())
        .or_else(|| dc.get("verification_uri").and_then(|v| v.as_str()))
        .ok_or_else(|| Error::Auth("missing verification_uri".into()))?
        .to_string();

    eprintln!(
        "\n  {} — open this URL in your browser:\n",
        provider.label
    );
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

/// Full interactive login + save.
pub fn login_and_save(provider: &ProviderDef, open: bool) -> Result<TokenSet> {
    let mut set = device_login(provider, open)?;
    save_tokens(provider.id, &mut set)?;
    Ok(set)
}

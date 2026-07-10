//! xAI OAuth device-code flow (RFC 8628). Token path matches Python Spock.

use crate::config::{auth_path, UA};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<f64>,
    #[serde(default)]
    pub expires_at: Option<f64>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct Discovery {
    device_authorization_endpoint: String,
    token_endpoint: String,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .user_agent(UA)
        .build()
}

pub fn load_tokens() -> Option<TokenSet> {
    let path = auth_path();
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_tokens(tokens: &mut TokenSet) -> Result<()> {
    if let Some(exp_in) = tokens.expires_in {
        tokens.expires_at = Some(now_secs() + exp_in);
    }
    let path = auth_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(tokens)?;
    {
        let mut f = fs::File::create(&path)?;
        f.write_all(&data)?;
    }
    set_mode_600(&path);
    Ok(())
}

#[cfg(unix)]
fn set_mode_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_mode_600(_path: &Path) {}

pub fn logout() -> Result<bool> {
    let path = auth_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn form_post(agent: &ureq::Agent, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value)> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding_lite(k), urlencoding_lite(v)))
        .collect::<Vec<_>>()
        .join("&");
    match agent
        .post(url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", "application/json")
        .send_string(&body)
    {
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

/// Minimal application/x-www-form-urlencoded (no extra crate).
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

fn discover(agent: &ureq::Agent) -> Result<Discovery> {
    let (status, v) = get_json(agent, DISCOVERY_URL)?;
    if status != 200 {
        return Err(Error::Auth(format!("OAuth discovery failed ({status})")));
    }
    serde_json::from_value(v).map_err(|e| Error::Auth(format!("discovery parse: {e}")))
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

fn device_login(agent: &ureq::Agent, endpoints: &Discovery, open: bool) -> Result<TokenSet> {
    let (status, dc) = form_post(
        agent,
        &endpoints.device_authorization_endpoint,
        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
    )?;
    if status != 200 {
        return Err(Error::Auth(format!(
            "device code request failed ({status}): {dc}"
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

    eprintln!("\n  Open this URL in your browser (logged in to your Grok/X account):\n");
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
        let (st, tok) = form_post(
            agent,
            &endpoints.token_endpoint,
            &[
                ("grant_type", DEVICE_GRANT),
                ("client_id", CLIENT_ID),
                ("device_code", &device_code),
            ],
        )?;
        if st == 200 {
            eprintln!(" approved.\n");
            let set: TokenSet = serde_json::from_value(tok)
                .map_err(|e| Error::Auth(format!("token parse: {e}")))?;
            return Ok(set);
        }
        let err = tok.get("error").and_then(|e| e.as_str()).unwrap_or("");
        if err == "authorization_pending" {
            continue;
        }
        if err == "slow_down" {
            interval += 5.0;
            continue;
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

fn refresh(
    agent: &ureq::Agent,
    endpoints: &Discovery,
    tokens: &TokenSet,
) -> Result<Option<TokenSet>> {
    let Some(rt) = tokens.refresh_token.as_deref() else {
        return Ok(None);
    };
    let (st, tok) = form_post(
        agent,
        &endpoints.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", rt),
        ],
    )?;
    if st != 200 {
        return Ok(None);
    }
    let mut set: TokenSet =
        serde_json::from_value(tok).map_err(|e| Error::Auth(format!("refresh parse: {e}")))?;
    if set.refresh_token.is_none() {
        set.refresh_token = tokens.refresh_token.clone();
    }
    Ok(Some(set))
}

/// Resolve a bearer access token (refresh / device login as needed).
pub fn get_access_token(open_browser_on_login: bool) -> Result<String> {
    if let Ok(t) = std::env::var("XAI_TOKEN") {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let agent = agent();
    let disco = discover(&agent)?;
    if let Some(tokens) = load_tokens() {
        let exp = tokens.expires_at.unwrap_or(0.0);
        if now_secs() < exp - 60.0 && !tokens.access_token.is_empty() {
            return Ok(tokens.access_token);
        }
        if tokens.refresh_token.is_some() {
            if let Some(mut fresh) = refresh(&agent, &disco, &tokens)? {
                save_tokens(&mut fresh)?;
                return Ok(fresh.access_token);
            }
            eprintln!("  Token refresh failed — logging in again.");
        }
    }
    let mut tokens = device_login(&agent, &disco, open_browser_on_login)?;
    save_tokens(&mut tokens)?;
    Ok(tokens.access_token)
}

/// In-memory cache used by the proxy (120s cushion before expires_at).
#[derive(Default)]
pub struct TokenCache {
    value: Option<String>,
    until: f64,
}

impl TokenCache {
    pub fn get(&mut self, open_on_login: bool) -> Result<String> {
        if let Ok(t) = std::env::var("XAI_TOKEN") {
            if !t.is_empty() {
                return Ok(t);
            }
        }
        let now = now_secs();
        if let Some(ref v) = self.value {
            if now < self.until {
                return Ok(v.clone());
            }
        }
        let token = get_access_token(open_on_login)?;
        let until = load_tokens()
            .and_then(|t| t.expires_at)
            .unwrap_or(now + 300.0)
            - 120.0;
        self.value = Some(token.clone());
        self.until = until;
        Ok(token)
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.value = None;
        self.until = 0.0;
    }
}

//! Per-provider OAuth token files + in-memory cache.

use crate::error::{Error, Result};
use crate::oauth::registry::{get_provider, ProviderDef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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

pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Normalize expires_at that may be milliseconds (kimi-cli / pi interop).
pub fn normalize_expires_at(expires_at: f64) -> f64 {
    // > year ~2286 in seconds, or any value that looks like ms epoch.
    if expires_at > 10_000_000_000.0 {
        expires_at / 1000.0
    } else {
        expires_at
    }
}

pub fn token_path_for(provider: &ProviderDef) -> PathBuf {
    crate::config::config_dir().join(provider.token_file)
}

pub fn load_tokens(provider_id: &str) -> Option<TokenSet> {
    let p = get_provider(provider_id)?;
    let path = token_path_for(p);
    if let Some(set) = load_tokens_file(&path) {
        return Some(set);
    }
    // Legacy import paths (relative to home).
    let home = crate::config::home_dir();
    for rel in p.legacy_token_paths {
        let legacy = home.join(rel);
        if let Some(mut set) = load_tokens_file(&legacy) {
            // Persist into Spock path for next time.
            let _ = save_tokens(provider_id, &mut set);
            return Some(set);
        }
    }
    None
}

fn load_tokens_file(path: &Path) -> Option<TokenSet> {
    let text = fs::read_to_string(path).ok()?;
    let mut set: TokenSet = serde_json::from_str(&text).ok()?;
    if let Some(exp) = set.expires_at {
        set.expires_at = Some(normalize_expires_at(exp));
    }
    // Some stores use `access` / `refresh` instead of *_token.
    if set.access_token.is_empty() {
        if let Some(a) = set.extra.get("access").and_then(|v| v.as_str()) {
            set.access_token = a.to_string();
        }
    }
    if set.refresh_token.is_none() {
        if let Some(r) = set.extra.get("refresh").and_then(|v| v.as_str()) {
            set.refresh_token = Some(r.to_string());
        }
    }
    if set.access_token.is_empty() {
        return None;
    }
    Some(set)
}

pub fn save_tokens(provider_id: &str, tokens: &mut TokenSet) -> Result<()> {
    let p = get_provider(provider_id)
        .ok_or_else(|| Error::Auth(format!("unknown provider '{provider_id}'")))?;
    if let Some(exp_in) = tokens.expires_in {
        tokens.expires_at = Some(now_secs() + exp_in);
    }
    if let Some(exp) = tokens.expires_at {
        tokens.expires_at = Some(normalize_expires_at(exp));
    }
    let path = token_path_for(p);
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

pub fn clear_tokens(provider_id: &str) -> Result<Vec<PathBuf>> {
    let p = get_provider(provider_id)
        .ok_or_else(|| Error::Auth(format!("unknown provider '{provider_id}'")))?;
    let mut removed = Vec::new();

    let path = token_path_for(p);
    match fs::remove_file(&path) {
        Ok(()) => removed.push(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    // Also delete legacy import paths so logout is final and login won't
    // resurrect a stale token from an older installation.
    let home = crate::config::home_dir();
    for rel in p.legacy_token_paths {
        let legacy = home.join(rel);
        match fs::remove_file(&legacy) {
            Ok(()) => removed.push(legacy),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    Ok(removed)
}

#[cfg(unix)]
pub fn set_mode_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn set_mode_600(_path: &Path) {}

#[derive(Default, Clone)]
struct MemEntry {
    access: String,
    until: f64,
}

/// Multi-provider token cache. Locks are short; network happens outside.
pub struct OauthStore {
    memory: Mutex<HashMap<String, MemEntry>>,
    /// Per-provider single-flight for refresh.
    flight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Default for OauthStore {
    fn default() -> Self {
        Self {
            memory: Mutex::new(HashMap::new()),
            flight: Mutex::new(HashMap::new()),
        }
    }
}

impl OauthStore {
    pub fn clear_memory(&self, provider_id: &str) {
        if let Ok(mut g) = self.memory.lock() {
            g.remove(provider_id);
        }
    }

    pub fn clear_all_memory(&self) {
        if let Ok(mut g) = self.memory.lock() {
            g.clear();
        }
    }

    pub fn flight_lock(&self, provider_id: &str) -> Arc<Mutex<()>> {
        let mut g = self.flight.lock().expect("flight lock");
        g.entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn memory_get(&self, provider_id: &str) -> Option<String> {
        let now = now_secs();
        let g = self.memory.lock().ok()?;
        let e = g.get(provider_id)?;
        if now < e.until && !e.access.is_empty() {
            Some(e.access.clone())
        } else {
            None
        }
    }

    fn memory_set(&self, provider_id: &str, access: String, expires_at: Option<f64>) {
        let until = expires_at.unwrap_or(now_secs() + 300.0) - 120.0;
        if let Ok(mut g) = self.memory.lock() {
            g.insert(
                provider_id.to_string(),
                MemEntry {
                    access,
                    until,
                },
            );
        }
    }

    /// Env token for provider (first non-empty registry env key).
    pub fn env_token(provider: &ProviderDef) -> Option<String> {
        for k in provider.env_token_keys {
            if let Ok(v) = std::env::var(k) {
                let t = v.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    /// Cache snapshot used after login/refresh.
    pub fn put_tokens(&self, provider_id: &str, tokens: &TokenSet) {
        self.memory_set(
            provider_id,
            tokens.access_token.clone(),
            tokens.expires_at,
        );
    }
}

/// Auth source for status UI.
#[derive(Debug, Clone)]
pub enum AuthSource {
    ConfigApiKey,
    Env,
    Oauth { expires_at: Option<f64> },
    None,
}

impl AuthSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthSource::ConfigApiKey => "config_api_key",
            AuthSource::Env => "env",
            AuthSource::Oauth { .. } => "oauth",
            AuthSource::None => "none",
        }
    }
}

pub fn status_for_provider(
    provider_id: &str,
    config_api_key_set: bool,
) -> (bool, AuthSource) {
    let Some(p) = get_provider(provider_id) else {
        return (false, AuthSource::None);
    };
    if config_api_key_set {
        return (true, AuthSource::ConfigApiKey);
    }
    if OauthStore::env_token(p).is_some() {
        return (true, AuthSource::Env);
    }
    match load_tokens(provider_id) {
        Some(t) if !t.access_token.is_empty() => (
            true,
            AuthSource::Oauth {
                expires_at: t.expires_at,
            },
        ),
        _ => (false, AuthSource::None),
    }
}


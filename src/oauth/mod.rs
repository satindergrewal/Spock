//! Multi-provider OAuth (device code + refresh) for Spock backends.

pub mod device;
pub mod registry;
pub mod store;

pub use registry::{get_provider, list_providers, provider_ids_csv};
pub use store::{
    clear_tokens, load_tokens, resource_base_url, save_tokens, status_for_provider, AuthSource,
    OauthStore, TokenSet,
};

use crate::error::{Error, Result};
use store::now_secs;

/// When resolving a bearer token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Never open a browser; refresh only. Used by the proxy.
    Proxy,
    /// Interactive device login allowed. Used by `spock login`.
    Login { open_browser: bool },
}

/// Interactive login for a registry provider.
pub fn login(provider_id: &str, open_browser: bool) -> Result<TokenSet> {
    let p = get_provider(provider_id).ok_or_else(|| {
        Error::Auth(format!(
            "unknown provider '{provider_id}' (known: {})",
            provider_ids_csv()
        ))
    })?;
    if let Some(tokens) = load_tokens(p.id) {
        let exp = tokens.expires_at.unwrap_or(0.0);
        if now_secs() < exp - 60.0 && !tokens.access_token.is_empty() {
            return Ok(tokens);
        }
    }
    device::login_and_save(p, open_browser)
}

/// Clear tokens for one provider (Spock file + legacy import paths).
pub fn logout(provider_id: &str) -> Result<bool> {
    let _p = get_provider(provider_id).ok_or_else(|| {
        Error::Auth(format!(
            "unknown provider '{provider_id}' (known: {})",
            provider_ids_csv()
        ))
    })?;
    let removed = clear_tokens(provider_id)?;
    Ok(!removed.is_empty())
}

pub fn logout_all() -> Result<Vec<String>> {
    let mut cleared = Vec::new();
    for p in list_providers() {
        let removed = clear_tokens(p.id)?;
        if !removed.is_empty() {
            cleared.push(p.id.to_string());
        }
    }
    Ok(cleared)
}

/// Resolve bearer access token for `provider_id`.
///
/// Priority:
/// 1. backend config `api_key` (explicit escape hatch)
/// 2. in-memory cache (only if not near expiry)
/// 3. on-disk OAuth tokens (+ **always refresh** when near/past expiry)
/// 4. env keys (`KIMI_TOKEN`, `XAI_TOKEN`, …) — after disk so a stale env key
///    cannot shadow a fresh `spock login`
/// 5. interactive device login only if `AccessMode::Login`
///
/// Kimi Code access tokens are short-lived (~900s). Without proactive refresh,
/// `/models` and chat return 401 "API Key appears to be invalid or may have expired".
pub fn access_token(
    store: &OauthStore,
    provider_id: &str,
    config_api_key: Option<&str>,
    mode: AccessMode,
) -> Result<String> {
    let p = get_provider(provider_id).ok_or_else(|| {
        Error::Auth(format!(
            "unknown provider '{provider_id}' (known: {})",
            provider_ids_csv()
        ))
    })?;

    if let Some(k) = config_api_key.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(k.to_string());
    }

    // Memory hit only if still safely before expiry. Near-expiry falls through
    // so we refresh under the single-flight lock.
    if let Some(t) = store.memory_get(p.id) {
        return Ok(t);
    }

    let flight = store.flight_lock(p.id);
    let _guard = flight
        .lock()
        .map_err(|_| Error::Msg("oauth flight lock poisoned".into()))?;

    if let Some(t) = store.memory_get(p.id) {
        return Ok(t);
    }

    if let Some(tokens) = load_tokens(p.id) {
        if token_still_usable(&tokens) {
            store.put_tokens(p.id, &tokens);
            return Ok(tokens.access_token);
        }

        // Near or past expiry: prefer refresh over sending a dead access token.
        if tokens.refresh_token.is_some() {
            match device::refresh(p, &tokens) {
                Ok(Some(mut fresh)) => {
                    // Preserve refresh token if server omits rotation.
                    if fresh.refresh_token.is_none() {
                        fresh.refresh_token = tokens.refresh_token.clone();
                    }
                    if let Err(e) = save_tokens(p.id, &mut fresh) {
                        // Still use the fresh access token this request even if disk write fails.
                        eprintln!("  warning: could not save {} tokens: {e}", p.id);
                    }
                    store.put_tokens(p.id, &fresh);
                    return Ok(fresh.access_token);
                }
                Ok(None) => {
                    // Refresh rejected — only keep access if not clearly expired.
                    if !tokens.access_token.is_empty() && !token_clearly_expired(&tokens) {
                        store.put_tokens(p.id, &tokens);
                        return Ok(tokens.access_token);
                    }
                    store.clear_memory(p.id);
                    // Leave disk file for diagnosis; next Login rewrites it.
                }
                Err(e) => {
                    // Network blip on refresh: if access still not clearly expired, try it.
                    if !tokens.access_token.is_empty() && !token_clearly_expired(&tokens) {
                        store.put_tokens(p.id, &tokens);
                        return Ok(tokens.access_token);
                    }
                    return Err(Error::Auth(format!(
                        "{} token refresh failed: {e} — run: spock login {}",
                        p.label, p.id
                    )));
                }
            }
        } else if !tokens.access_token.is_empty() && !token_clearly_expired(&tokens) {
            store.put_tokens(p.id, &tokens);
            return Ok(tokens.access_token);
        }
    }

    if let Some(k) = OauthStore::env_token(p) {
        return Ok(k);
    }

    match mode {
        AccessMode::Proxy => Err(Error::Auth(format!(
            "{} not authenticated — run: spock login {}",
            p.label, p.id
        ))),
        AccessMode::Login { open_browser } => {
            let set = device::login_and_save(p, open_browser)?;
            store.put_tokens(p.id, &set);
            Ok(set.access_token)
        }
    }
}

/// True when we should send the access token without refreshing first.
/// Refresh ~2 minutes early — Kimi access tokens are ~15 min.
fn token_still_usable(tokens: &TokenSet) -> bool {
    if tokens.access_token.is_empty() {
        return false;
    }
    match tokens.expires_at {
        // Missing expiry: treat as usable (will 401 and surface if wrong).
        None => true,
        Some(exp) => now_secs() < exp - 120.0,
    }
}

fn token_clearly_expired(tokens: &TokenSet) -> bool {
    match tokens.expires_at {
        None => false,
        Some(exp) => now_secs() >= exp,
    }
}

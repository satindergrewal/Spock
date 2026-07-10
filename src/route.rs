use crate::config::{Config, ProfileConfig};
use crate::error::{Error, Result};
use crate::models::{detect_role, strip_ctx_suffix, Role};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedRoute {
    pub backend: String,
    pub upstream_model: String,
    /// Original client-facing model id (may include [1m]).
    pub client_model: String,
}

/// Parse "backend:model" — model may contain colons (ollama tags).
pub fn parse_route_spec(spec: &str) -> Result<(String, String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(Error::Msg(
            "empty route (set backend:model, e.g. ollama:kimi-k2.7-code:cloud)".into(),
        ));
    }
    let Some((backend, model)) = spec.split_once(':') else {
        return Err(Error::Msg(format!(
            "invalid route '{spec}' (want backend:model)"
        )));
    };
    if backend.is_empty() || model.is_empty() {
        return Err(Error::Msg(format!("invalid route '{spec}'")));
    }
    Ok((backend.to_string(), model.to_string()))
}

/// Treat blank / whitespace-only profile fields as unset.
fn nonempty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

pub fn resolve(cfg: &Config, client_model: &str) -> Result<ResolvedRoute> {
    let profile = cfg.active_profile()?;
    let stripped = strip_ctx_suffix(client_model);
    let client = if client_model.is_empty() {
        stripped.clone()
    } else {
        client_model.to_string()
    };

    // 1. exact keys on profile
    if let Some(spec) = profile
        .exact
        .get(&stripped)
        .map(|s| s.as_str())
        .and_then(|s| nonempty(Some(s)))
    {
        return make_route(spec, &client);
    }
    if let Some(spec) = profile
        .exact
        .get(client_model)
        .map(|s| s.as_str())
        .and_then(|s| nonempty(Some(s)))
    {
        return make_route(spec, &client);
    }

    // 2. role buckets — empty strings fall through (UI often saves "" for unused roles)
    let role_spec = match detect_role(&stripped) {
        Role::Haiku => nonempty(profile.haiku.as_deref()),
        Role::Sonnet => nonempty(profile.sonnet.as_deref()),
        Role::Opus => nonempty(profile.opus.as_deref()),
        Role::Fable => nonempty(profile.fable.as_deref()),
        Role::Other => None,
    };
    if let Some(spec) = role_spec {
        return make_route(spec, &client);
    }

    // 3. profile default (including bare grok-* client ids — no xAI auto-passthrough)
    if let Some(spec) = nonempty(profile.default.as_deref()) {
        return make_route(spec, &client);
    }

    Err(Error::Msg(format!(
        "no route for model '{client_model}' in profile '{}' — set default or a role row (haiku/sonnet/opus/fable)",
        cfg.server.profile
    )))
}

fn make_route(spec: &str, client: &str) -> Result<ResolvedRoute> {
    let (backend, upstream_model) = parse_route_spec(spec)?;
    Ok(ResolvedRoute {
        backend,
        upstream_model,
        client_model: client.to_string(),
    })
}

/// For listing / docs.
pub fn profile_summary(p: &ProfileConfig) -> String {
    let mut parts = Vec::new();
    if let Some(d) = &p.default {
        parts.push(format!("default={d}"));
    }
    if let Some(h) = &p.haiku {
        parts.push(format!("haiku={h}"));
    }
    if let Some(s) = &p.sonnet {
        parts.push(format!("sonnet={s}"));
    }
    if let Some(o) = &p.opus {
        parts.push(format!("opus={o}"));
    }
    if let Some(f) = &p.fable {
        parts.push(format!("fable={f}"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendConfig, ProfileConfig, ServerConfig};
    use std::collections::BTreeMap;

    fn sample() -> Config {
        let mut backends = BTreeMap::new();
        backends.insert(
            "xai".into(),
            BackendConfig::Xai {
                base_url: "https://api.x.ai/v1".into(),
                api_key: None,
            },
        );
        backends.insert(
            "ollama".into(),
            BackendConfig::Openai {
                base_url: "http://127.0.0.1:11434/v1".into(),
                api_key: None,
            },
        );
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "hybrid".into(),
            ProfileConfig {
                default: Some("ollama:glm-5.2:cloud".into()),
                haiku: Some("ollama:kimi-k2.7-code:cloud".into()),
                sonnet: Some("ollama:kimi-k2.7-code:cloud".into()),
                opus: Some("xai:grok-4.5".into()),
                fable: Some("ollama:glm-5.2:cloud".into()),
                exact: BTreeMap::new(),
            },
        );
        Config {
            server: ServerConfig {
                bind: "127.0.0.1".into(),
                port: 8048,
                profile: "hybrid".into(),
            },
            backends,
            profiles,
        }
    }

    #[test]
    fn haiku_to_ollama() {
        let cfg = sample();
        let r = resolve(&cfg, "claude-haiku-4-5").unwrap();
        assert_eq!(r.backend, "ollama");
        assert_eq!(r.upstream_model, "kimi-k2.7-code:cloud");
    }

    #[test]
    fn opus_to_xai() {
        let cfg = sample();
        let r = resolve(&cfg, "claude-opus-4-8[1m]").unwrap();
        assert_eq!(r.backend, "xai");
        assert_eq!(r.upstream_model, "grok-4.5");
        assert_eq!(r.client_model, "claude-opus-4-8[1m]");
    }

    #[test]
    fn grok_client_id_uses_profile_default_not_xai_passthrough() {
        let cfg = sample();
        let r = resolve(&cfg, "grok-4.5[1m]").unwrap();
        assert_eq!(r.backend, "ollama");
        assert_eq!(r.upstream_model, "glm-5.2:cloud");
    }

    #[test]
    fn fable_role_to_ollama() {
        let cfg = sample();
        let r = resolve(&cfg, "claude-fable-5").unwrap();
        assert_eq!(r.backend, "ollama");
        assert_eq!(r.upstream_model, "glm-5.2:cloud");
    }

    #[test]
    fn empty_role_falls_through_to_default() {
        let mut cfg = sample();
        cfg.profiles.get_mut("hybrid").unwrap().fable = Some("".into());
        let r = resolve(&cfg, "claude-fable-5").unwrap();
        assert_eq!(r.backend, "ollama");
        assert_eq!(r.upstream_model, "glm-5.2:cloud");
    }

    #[test]
    fn parse_colon_model() {
        let (b, m) = parse_route_spec("ollama:qwen2.5:14b").unwrap();
        assert_eq!(b, "ollama");
        assert_eq!(m, "qwen2.5:14b");
    }
}

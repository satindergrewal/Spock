//! OAuth provider registry — single extension point for device-login vendors.

use crate::config::{DEFAULT_XAI_BASE, UA, VERSION};
use std::collections::BTreeMap;

/// How chat-completions translation should treat this upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionsQuirk {
    Xai,
    Kimi,
    Generic,
}

#[derive(Debug, Clone, Copy)]
pub enum AuthEndpoints {
    /// OpenID discovery (xAI).
    Discovery { url: &'static str },
    /// Fixed device + token endpoints (Kimi Code).
    Fixed {
        device_auth: &'static str,
        token: &'static str,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderDef {
    pub id: &'static str,
    pub label: &'static str,
    pub client_id: &'static str,
    pub auth: AuthEndpoints,
    pub scope: Option<&'static str>,
    pub default_base_url: &'static str,
    pub user_agent: &'static str,
    pub quirk: CompletionsQuirk,
    /// Filename under `~/.config/spock/`.
    pub token_file: &'static str,
    pub env_token_keys: &'static [&'static str],
    /// Older token paths to import once if Spock file missing.
    pub legacy_token_paths: &'static [&'static str],
    pub header_style: HeaderStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderStyle {
    /// Default Spock UA only.
    Default,
    /// Kimi Code coding API gate (KimiCLI UA + X-Msh-*).
    KimiCode,
}

pub const PROVIDERS: &[ProviderDef] = &[
    ProviderDef {
        id: "xai",
        label: "xAI (Grok)",
        client_id: "b1a00492-073a-47ea-816f-4c329264a828",
        auth: AuthEndpoints::Discovery {
            url: "https://auth.x.ai/.well-known/openid-configuration",
        },
        scope: Some("openid profile email offline_access grok-cli:access api:access"),
        default_base_url: DEFAULT_XAI_BASE,
        user_agent: UA,
        quirk: CompletionsQuirk::Xai,
        token_file: "oauth-xai.json",
        env_token_keys: &["XAI_TOKEN"],
        legacy_token_paths: &[".config/grok-test/auth.json"],
        header_style: HeaderStyle::Default,
    },
    ProviderDef {
        id: "kimi",
        label: "Kimi Code",
        client_id: "17e5f671-d194-4dfb-9706-5516cb48c098",
        auth: AuthEndpoints::Fixed {
            device_auth: "https://auth.kimi.com/api/oauth/device_authorization",
            token: "https://auth.kimi.com/api/oauth/token",
        },
        scope: None,
        default_base_url: "https://api.kimi.com/coding/v1",
        user_agent: "KimiCLI/1.44.0",
        quirk: CompletionsQuirk::Kimi,
        token_file: "oauth-kimi.json",
        env_token_keys: &["KIMI_TOKEN", "KIMI_API_KEY", "KIMI_CODE_TOKEN", "KIMI_CODER_API_KEY"],
        legacy_token_paths: &[".kimi/credentials/kimi-code.json"],
        header_style: HeaderStyle::KimiCode,
    },
];

pub fn list_providers() -> &'static [ProviderDef] {
    PROVIDERS
}

pub fn get_provider(id: &str) -> Option<&'static ProviderDef> {
    let id = id.trim();
    PROVIDERS.iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

pub fn known_provider_ids() -> Vec<&'static str> {
    PROVIDERS.iter().map(|p| p.id).collect()
}

pub fn provider_ids_csv() -> String {
    known_provider_ids().join(", ")
}

/// Stable device id for Kimi-style headers (shared file).
pub fn device_id_path() -> std::path::PathBuf {
    crate::config::config_dir().join("device-id")
}

pub fn load_or_create_device_id() -> String {
    use std::fs;
    let path = device_id_path();
    if let Ok(s) = fs::read_to_string(&path) {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    // Prefer kimi-cli device id if present.
    if let Ok(home) = std::env::var("HOME") {
        let legacy = std::path::PathBuf::from(home).join(".kimi/device_id");
        if let Ok(s) = fs::read_to_string(legacy) {
            let t = s.trim();
            if !t.is_empty() {
                let _ = save_device_id(t);
                return t.to_string();
            }
        }
    }
    let id = format!("{:x}", simple_uuid_u128());
    let _ = save_device_id(&id);
    id
}

fn save_device_id(id: &str) -> crate::error::Result<()> {
    use std::fs;
    use std::io::Write;
    let path = device_id_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::File::create(&path)?;
    f.write_all(id.as_bytes())?;
    crate::oauth::store::set_mode_600(&path);
    Ok(())
}

fn simple_uuid_u128() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix with address entropy of a stack value.
    let stack = &t as *const _ as u128;
    t ^ stack.rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15
}

pub struct DeviceCtx {
    pub device_id: String,
    pub hostname: String,
    pub os_model: String,
    pub os_version: String,
}

impl DeviceCtx {
    pub fn current() -> Self {
        let hostname = hostname_lite();
        let (os_model, os_version) = os_info_lite();
        Self {
            device_id: load_or_create_device_id(),
            hostname,
            os_model,
            os_version,
        }
    }
}

fn hostname_lite() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // macOS/Linux
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "spock".into())
        })
}

fn os_info_lite() -> (String, String) {
    let release = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let machine = std::process::Command::new("uname")
        .arg("-m")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    #[cfg(target_os = "macos")]
    {
        return (format!("macOS {release} {machine}"), release);
    }
    #[cfg(target_os = "linux")]
    {
        return (format!("Linux {release} {machine}"), release);
    }
    #[cfg(target_os = "windows")]
    {
        return (format!("Windows {release} {machine}"), release);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        (format!("unknown {release} {machine}"), release)
    }
}

/// Extra request headers for chat/OAuth for this provider.
pub fn request_headers(provider: &ProviderDef, ctx: &DeviceCtx) -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    match provider.header_style {
        HeaderStyle::Default => {
            h.insert("User-Agent".into(), provider.user_agent.into());
        }
        HeaderStyle::KimiCode => {
            h.insert("User-Agent".into(), provider.user_agent.into());
            h.insert("X-Msh-Platform".into(), "kimi_cli".into());
            h.insert("X-Msh-Version".into(), "1.44.0".into());
            h.insert("X-Msh-Device-Id".into(), ctx.device_id.clone());
            h.insert(
                "X-Msh-Device-Name".into(),
                ascii_header(&ctx.hostname, "spock"),
            );
            h.insert(
                "X-Msh-Device-Model".into(),
                ascii_header(&ctx.os_model, "unknown"),
            );
            h.insert(
                "X-Msh-Os-Version".into(),
                ascii_header(&ctx.os_version, "unknown"),
            );
        }
    }
    let _ = VERSION; // keep linked
    h
}

fn ascii_header(value: &str, fallback: &str) -> String {
    let s: String = value
        .chars()
        .filter(|c| {
            let u = *c as u32;
            (0x20..=0x7e).contains(&u)
        })
        .collect();
    let t = s.trim();
    if t.is_empty() {
        fallback.into()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_include_xai_and_kimi() {
        assert!(get_provider("xai").is_some());
        assert!(get_provider("KIMI").is_some());
        assert!(get_provider("nope").is_none());
        assert_eq!(list_providers().len(), 2);
    }

    #[test]
    fn kimi_headers_have_msh() {
        let p = get_provider("kimi").unwrap();
        let ctx = DeviceCtx {
            device_id: "abc".into(),
            hostname: "host".into(),
            os_model: "macOS".into(),
            os_version: "25".into(),
        };
        let h = request_headers(p, &ctx);
        assert_eq!(h.get("User-Agent").map(String::as_str), Some("KimiCLI/1.44.0"));
        assert_eq!(h.get("X-Msh-Platform").map(String::as_str), Some("kimi_cli"));
        assert_eq!(h.get("X-Msh-Device-Id").map(String::as_str), Some("abc"));
    }
}

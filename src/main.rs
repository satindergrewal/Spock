mod auth;
mod backends;
mod cli;
mod config;
mod error;
mod models;
mod route;
mod server;
mod server_tools;
mod settings;
mod state;
mod translate;
mod tray;

use auth::{get_access_token, load_tokens, logout};
use cli::{parse, print_help, Command};
use config::{
    auth_path, config_path, Config, EnvOverrides, CHAT_DEFAULT_MODEL, DEFAULT_XAI_BASE, UA, VERSION,
};
use error::Result;
use serde_json::json;
use state::AppState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // If launched as GUI with no args, enter app mode
    if args.is_empty() && !atty_stdin() {
        args.push("app".into());
    }
    let cmd = parse(&args);
    match cmd {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Version => {
            println!("spock {VERSION}");
            Ok(())
        }
        Command::Login { no_open } => {
            let token = get_access_token(!no_open)?;
            println!(
                "Logged in. Token cached at {} ({}…)",
                auth_path().display(),
                token.chars().take(12).collect::<String>()
            );
            Ok(())
        }
        Command::Logout => {
            if logout()? {
                println!("Logged out (cached tokens removed).");
            } else {
                println!("Nothing to remove.");
            }
            Ok(())
        }
        Command::Chat { model, prompt } => cmd_chat(model, prompt),
        Command::Status => cmd_status(),
        Command::Reload => {
            let path = config_path();
            let cfg = Config::load(&path)?;
            println!(
                "config ok: profile={} backends={}",
                cfg.server.profile,
                cfg.backends.len()
            );
            Ok(())
        }
        Command::Serve { port } => {
            let mut cfg = Config::load_or_init()?;
            if let Some(p) = port {
                cfg.server.port = p;
            }
            // PORT env still wins inside bind_addr
            let state = AppState::new(cfg);
            let shutdown = Arc::new(AtomicBool::new(false));
            let sh = shutdown.clone();
            ctrlc_handler(sh);
            server::serve(state, shutdown)
        }
        Command::App => {
            let cfg = Config::load_or_init()?;
            let state = AppState::new(cfg);
            tray::run_app(state)
        }
    }
}

fn cmd_chat(model: Option<String>, prompt: String) -> Result<()> {
    let model = model.unwrap_or_else(|| CHAT_DEFAULT_MODEL.to_string());
    let token = get_access_token(true)?;
    let base = std::env::var("XAI_API_BASE").unwrap_or_else(|_| DEFAULT_XAI_BASE.to_string());
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    println!("  Model: {model}\n  Prompt: {prompt}\n");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(UA)
        .build();
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 256
    });
    match agent
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_json(body)
    {
        Ok(resp) => {
            let v: serde_json::Value = resp.into_json()?;
            let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
            println!("{}", "─".repeat(60));
            println!("{}", content.trim());
            println!("{}", "─".repeat(60));
            let usage = &v["usage"];
            println!(
                "[{}] tokens: {} in / {} out",
                v.get("model").and_then(|m| m.as_str()).unwrap_or(&model),
                usage
                    .get("prompt_tokens")
                    .and_then(|t| t.as_u64())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
                usage
                    .get("completion_tokens")
                    .and_then(|t| t.as_u64())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
            );
            Ok(())
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Err(error::Error::Msg(format!(
                "chat completion failed ({code}): {text}"
            )))
        }
        Err(e) => Err(error::Error::Msg(format!("chat: {e}"))),
    }
}

fn cmd_status() -> Result<()> {
    let cfg = Config::load_or_init()?;
    println!("Spock {VERSION}");
    println!("  config:  {}", config_path().display());
    println!("  tokens:  {}", auth_path().display());
    println!("  profile: {}", cfg.server.profile);
    println!("  listen:  {}", cfg.bind_addr());
    println!("  backends:");
    for (name, be) in &cfg.backends {
        println!("    {name}: {} ({})", be.kind_name(), be.base_url());
    }
    if let Ok(p) = cfg.active_profile() {
        println!("  routes:  {}", route::profile_summary(p));
    }
    // Same priority as proxy: config api_key → XAI_TOKEN → OAuth file.
    let has_cfg_key = cfg.backends.values().any(|b| {
        matches!(
            b,
            config::BackendConfig::Xai {
                api_key: Some(k),
                ..
            } if !k.trim().is_empty()
        )
    });
    if has_cfg_key {
        println!("  xAI auth: config_api_key (beats OAuth)");
    } else if EnvOverrides::from_env().xai_token.is_some() {
        println!("  xAI auth: XAI_TOKEN env");
    } else {
        match load_tokens() {
            Some(t) => {
                let exp = t.expires_at.unwrap_or(0.0);
                println!("  xAI auth: oauth (expires_at={exp:.0})");
            }
            None => println!("  xAI auth: none (set api_key on xai backend, or: spock login)"),
        }
    }
    // live health if proxy up
    let port = cfg.port_from_env_or_self();
    let url = format!("http://127.0.0.1:{port}/health");
    match ureq::get(&url)
        .timeout(std::time::Duration::from_secs(1))
        .call()
    {
        Ok(resp) => {
            let v: serde_json::Value = resp.into_json().unwrap_or(json!({}));
            println!("  proxy:   up {}", v);
        }
        Err(_) => println!("  proxy:   down"),
    }
    Ok(())
}

fn ctrlc_handler(shutdown: Arc<AtomicBool>) {
    // std-only: spawn a thread that waits is hard without signal crate.
    // Use a simple approach — on Unix, set a flag via ctrlc is not in std.
    // Document Ctrl-C kills process; for clean stop, tray Quit or kill.
    // Attempt: ignore — OS will SIGINT the process. Nonblocking accept loop
    // means kill is fine. Optionally install via libc — skip for minimal deps.
    let _ = shutdown;
    let _ = Ordering::SeqCst;
}

fn atty_stdin() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        unsafe { isatty(std::io::stdin().as_raw_fd()) != 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

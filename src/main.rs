mod oauth;
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

use cli::{parse, print_help, Command};
use config::{
    config_path, Config, CHAT_DEFAULT_MODEL, DEFAULT_XAI_BASE, UA, VERSION,
};
use oauth::{AccessMode, OauthStore};
use error::Result;
use serde_json::json;
use state::AppState;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
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
        Command::Login { provider, no_open } => {
            let p = oauth::get_provider(&provider).ok_or_else(|| {
                error::Error::Auth(format!(
                    "unknown provider '{provider}' (known: {})",
                    oauth::provider_ids_csv()
                ))
            })?;
            // Already logged in?
            if let Some(tokens) = oauth::load_tokens(p.id) {
                let exp = tokens.expires_at.unwrap_or(0.0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                if now < exp - 60.0 && !tokens.access_token.is_empty() {
                    println!(
                        "Already logged in to {} (OAuth). Token cached ({}…)",
                        p.label,
                        tokens.access_token.chars().take(12).collect::<String>()
                    );
                    println!("  Logout first (`spock logout {}`) for a fresh device login.", p.id);
                    return Ok(());
                }
            }
            if oauth::store::OauthStore::env_token(p).is_some() {
                println!(
                    "{}: env token is set ({}) — OAuth login not used.",
                    p.label,
                    p.env_token_keys.join("/")
                );
                return Ok(());
            }
            let tokens = oauth::login(p.id, !no_open)?;
            println!(
                "Logged in to {}. Token cached ({}…)",
                p.label,
                tokens.access_token.chars().take(12).collect::<String>()
            );
            Ok(())
        }
        Command::Logout { provider, all } => {
            if all {
                let cleared = oauth::logout_all()?;
                if cleared.is_empty() {
                    println!("Nothing to remove.");
                } else {
                    println!("Logged out: {}", cleared.join(", "));
                }
                return Ok(());
            }
            let provider = provider.expect("provider");
            if oauth::logout(&provider)? {
                println!("Logged out of {provider} (cached tokens removed).");
            } else {
                println!("Nothing to remove for {provider}.");
            }
            Ok(())
        }
        Command::Providers { json } => {
            if json {
                let arr: Vec<_> = oauth::list_providers()
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "id": p.id,
                            "label": p.label,
                            "default_base_url": p.default_base_url,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                for p in oauth::list_providers() {
                    println!("{:<8}  {}  ({})", p.id, p.label, p.default_base_url);
                }
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
        Command::Serve { port, log_file } => {
            if let Some(path) = log_file {
                install_log_file(&path)?;
            }
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

/// Redirect stdout+stderr into `path` for `tail -f` (Unix).
fn install_log_file(path: &str) -> Result<()> {
    let p = PathBuf::from(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new().create(true).append(true).open(&p)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let fd = file.as_raw_fd();
        let mut log = unsafe { std::fs::File::from_raw_fd(libc_dup(fd)) };
        let _ = writeln!(log, "\n--- spock log start unix={} ---", epoch_secs());
        let (r, w) = pipe_pair().map_err(|e| error::Error::Msg(format!("log pipe: {e}")))?;
        if unsafe { libc_dup2(w.as_raw_fd(), 2) } < 0 {
            return Err(error::Error::Msg("dup2 stderr failed".into()));
        }
        if unsafe { libc_dup2(w.as_raw_fd(), 1) } < 0 {
            return Err(error::Error::Msg("dup2 stdout failed".into()));
        }
        drop(w);
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(r);
            let mut line = String::new();
            loop {
                line.clear();
                match std::io::BufRead::read_line(&mut reader, &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let _ = log.write_all(line.as_bytes());
                        let _ = log.flush();
                    }
                    Err(_) => break,
                }
            }
        });
        std::mem::forget(file);
        eprintln!("logging to {}", p.display());
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        eprintln!("--log-file is only supported on Unix; ignoring {path}");
        Ok(())
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn pipe_pair() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0i32; 2];
    let rc = unsafe { libc_pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    })
}

#[cfg(unix)]
extern "C" {
    #[link_name = "pipe"]
    fn libc_pipe(fds: *mut i32) -> i32;
    #[link_name = "dup"]
    fn libc_dup(fd: i32) -> i32;
    #[link_name = "dup2"]
    fn libc_dup2(old: i32, new: i32) -> i32;
}

fn cmd_chat(model: Option<String>, prompt: String) -> Result<()> {
    let model = model.unwrap_or_else(|| CHAT_DEFAULT_MODEL.to_string());
    let store = OauthStore::default();
    let token = oauth::access_token(&store, "xai", None, AccessMode::Login { open_browser: true })?;
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
    println!("  profile: {}", cfg.server.profile);
    println!("  listen:  {}", cfg.bind_addr());
    println!("  backends:");
    for (name, be) in &cfg.backends {
        println!("    {name}: {} ({})", be.kind_name(), be.base_url());
    }
    if let Ok(p) = cfg.active_profile() {
        println!("  routes:  {}", route::profile_summary(p));
    }
    println!(
        "  advisor:    {}",
        if cfg.advisor.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  web_search: {}",
        if cfg.web_search.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  oauth:");
    for p in oauth::list_providers() {
        let key_set = cfg.oauth_config_key_set(p.id);
        let (present, source) = oauth::status_for_provider(p.id, key_set);
        let extra = match &source {
            oauth::AuthSource::Oauth { expires_at: Some(e) } => format!(" expires_at={e:.0}"),
            _ => String::new(),
        };
        println!(
            "    {}: {} ({}){}",
            p.id,
            if present { "yes" } else { "no" },
            source.as_str(),
            extra
        );
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
    // last upstream error from live proxy if available
    let status_url = format!("http://127.0.0.1:{port}/spock/v1/status");
    if let Ok(resp) = ureq::get(&status_url)
        .timeout(std::time::Duration::from_secs(1))
        .call()
    {
        if let Ok(v) = resp.into_json::<serde_json::Value>() {
            if let Some(err) = v.get("last_upstream_error") {
                if !err.is_null() {
                    println!(
                        "  last_err: {}",
                        err.get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or(&err.to_string())
                    );
                }
            }
        }
    }
    Ok(())
}

fn ctrlc_handler(shutdown: Arc<AtomicBool>) {
    // std-only: Ctrl-C kills process; nonblocking accept loop is fine.
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

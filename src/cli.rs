//! Hand-rolled argv (no clap).

#[derive(Debug)]
pub enum Command {
    Serve {
        port: Option<u16>,
    },
    App,
    Login {
        no_open: bool,
    },
    Logout,
    Chat {
        model: Option<String>,
        prompt: String,
    },
    Status,
    Reload,
    Version,
    Help,
}

pub fn parse(args: &[String]) -> Command {
    if args.is_empty() {
        return Command::Help;
    }
    // GUI launch on macOS often passes -psn_...
    if args.iter().any(|a| a.starts_with("-psn_")) {
        return Command::App;
    }
    match args[0].as_str() {
        "serve" => {
            let mut port = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--port" || args[i] == "-p" {
                    if let Some(v) = args.get(i + 1) {
                        port = v.parse().ok();
                        i += 2;
                        continue;
                    }
                }
                i += 1;
            }
            Command::Serve { port }
        }
        "app" => Command::App,
        "login" => {
            let no_open = args.iter().any(|a| a == "--no-open");
            Command::Login { no_open }
        }
        "logout" => Command::Logout,
        "chat" => {
            let mut model = None;
            let mut prompt_parts = Vec::new();
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--model" || args[i] == "-m" {
                    if let Some(v) = args.get(i + 1) {
                        model = Some(v.clone());
                        i += 2;
                        continue;
                    }
                }
                prompt_parts.push(args[i].clone());
                i += 1;
            }
            let prompt = if prompt_parts.is_empty() {
                "how are you doing? what model are you and what can you do?".into()
            } else {
                prompt_parts.join(" ")
            };
            Command::Chat { model, prompt }
        }
        "status" => Command::Status,
        "reload" => Command::Reload,
        "-V" | "--version" | "version" => Command::Version,
        "-h" | "--help" | "help" => Command::Help,
        other => {
            eprintln!("unknown command: {other}");
            Command::Help
        }
    }
}

pub fn print_help() {
    eprintln!(
        "\
Spock {ver} — multi-backend Anthropic-compatible proxy

Usage:
  spock serve [--port N]     Start headless proxy (127.0.0.1)
  spock app                  Open Spock.app (macOS menu bar + chat)
  spock login [--no-open]    xAI OAuth device login
  spock logout               Forget cached xAI tokens
  spock chat [prompt] [-m model]
  spock status               Active profile + backends
  spock reload               Re-read ~/.config/spock/config.toml
  spock -V                   Version

Config:  ~/.config/spock/config.toml
Tokens:  ~/.config/grok-test/auth.json
macOS:   ./packaging/macos/build-app.sh → dist/Spock.app
",
        ver = crate::config::VERSION
    );
}

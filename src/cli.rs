//! Hand-rolled argv (no clap).

#[derive(Debug)]
pub enum Command {
    Serve {
        port: Option<u16>,
        log_file: Option<String>,
    },
    App,
    Login {
        provider: String,
        no_open: bool,
    },
    Logout {
        provider: Option<String>,
        all: bool,
    },
    Providers {
        json: bool,
    },
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
    if args.iter().any(|a| a.starts_with("-psn_")) {
        return Command::App;
    }
    match args[0].as_str() {
        "serve" => {
            let mut port = None;
            let mut log_file = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--port" || args[i] == "-p" {
                    if let Some(v) = args.get(i + 1) {
                        port = v.parse().ok();
                        i += 2;
                        continue;
                    }
                }
                if args[i] == "--log-file" || args[i] == "-l" {
                    if let Some(v) = args.get(i + 1) {
                        log_file = Some(v.clone());
                        i += 2;
                        continue;
                    }
                }
                i += 1;
            }
            Command::Serve { port, log_file }
        }
        "app" => Command::App,
        "login" => {
            let no_open = args.iter().any(|a| a == "--no-open");
            let provider = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned();
            match provider {
                Some(p) => Command::Login {
                    provider: p,
                    no_open,
                },
                None => {
                    eprintln!(
                        "usage: spock login <provider> [--no-open]\n  providers: {}",
                        crate::oauth::provider_ids_csv()
                    );
                    Command::Help
                }
            }
        }
        "logout" => {
            let all = args.iter().any(|a| a == "--all");
            let provider = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned();
            if !all && provider.is_none() {
                eprintln!(
                    "usage: spock logout <provider> | spock logout --all\n  providers: {}",
                    crate::oauth::provider_ids_csv()
                );
                return Command::Help;
            }
            Command::Logout { provider, all }
        }
        "providers" => {
            let json = args.iter().any(|a| a == "--json");
            Command::Providers { json }
        }
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
  spock serve [--port N] [--log-file PATH]
  spock app
  spock login <provider> [--no-open]   OAuth device login ({providers})
  spock logout <provider> | --all
  spock providers [--json]             List OAuth providers (offline)
  spock chat [prompt] [-m model]       xAI helper (uses xai oauth)
  spock status
  spock reload
  spock -V

Config:  ~/.config/spock/config.toml
OAuth:   ~/.config/spock/oauth-<provider>.json
",
        ver = crate::config::VERSION,
        providers = crate::oauth::provider_ids_csv()
    );
}

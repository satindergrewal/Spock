//! `spock app` — open the native macOS Spock.app (SwiftUI menu bar).
//! Proxy itself is headless (`spock serve`).

use crate::state::AppState;

pub fn run_app(_state: AppState) -> crate::error::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // Prefer installed app, then dist build next to binary / cwd
        let candidates = [
            "/Applications/Spock.app".to_string(),
            format!(
                "{}/Spock.app",
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.display().to_string()))
                    .unwrap_or_default()
            ),
            "dist/Spock.app".into(),
            "../dist/Spock.app".into(),
        ];
        for path in candidates {
            if path.is_empty() {
                continue;
            }
            let p = std::path::Path::new(&path);
            if p.exists() {
                let status = std::process::Command::new("open").arg(p).status()?;
                if status.success() {
                    eprintln!("opened {}", p.display());
                    return Ok(());
                }
            }
        }
        Err(crate::error::Error::Msg(
            "Spock.app not found. Build with: ./packaging/macos/build-app.sh\n\
             Or run headless: spock serve"
                .into(),
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = _state;
        Err(crate::error::Error::Msg(
            "spock app is macOS-only; use: spock serve".into(),
        ))
    }
}

# Spock macOS app (SwiftUI)

Native menu bar agent + Settings window. Talks to the Rust proxy over:

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | readiness |
| GET | `/spock/v1/status` | profile, auth, paths |
| GET | `/spock/v1/config` | full settings JSON |
| PUT | `/spock/v1/config` | save & hot-apply |
| POST | `/spock/v1/profile` | switch profile |
| POST | `/spock/v1/reload` | re-read config.toml |
| POST | `/spock/v1/logout` | clear xAI tokens |

Build everything with:

```bash
./packaging/macos/build-app.sh
open dist/Spock.app
```

The app embeds `spock-proxy` (the Rust binary) next to the Swift executable and starts it on launch if nothing is listening on :8048.

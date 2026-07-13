import Foundation
import AppKit

/// Coarse proxy health for tray icon color.
enum ProxyStatus: Equatable {
    case starting
    case running
    case stopped
    case error

    var menuLabel: String {
        switch self {
        case .starting: return "starting…"
        case .running: return "running"
        case .stopped: return "stopped"
        case .error: return "error"
        }
    }

    /// Menu bar icon tint (dot / monochrome hand).
    var iconColor: NSColor {
        switch self {
        case .starting: return .systemOrange
        case .running: return .systemGreen
        case .stopped: return .systemGray
        case .error: return .systemRed
        }
    }
}

/// Talks to the local Rust proxy admin API on 127.0.0.1.
final class AppModel: ObservableObject {
    static let shared = AppModel()

    @Published var proxyUp = false
    @Published var proxyStatus: ProxyStatus = .stopped
    @Published var port: Int = 8048
    @Published var profile: String = "xai-only"
    @Published var profiles: [String] = []
    @Published var backends: [BackendRow] = []
    @Published var profileRows: [ProfileRow] = []
    @Published var bind: String = "127.0.0.1"
    @Published var configPath: String = ""
    @Published var authPresent = false
    /// Live proxy report: config_api_key | env_XAI_TOKEN | oauth | none
    @Published var authSource: String = "none"
    @Published var statusMessage: String = ""
    @Published var statusIsError = false
    /// Discovered models per backend name: ["ollama": ["qwen2.5:14b", …]]
    @Published var discoveredModels: [String: [String]] = [:]
    @Published var discoveringBackend: String? = nil

    // Server-tool emulation (Spock 0.2.0+) — saved under [advisor] / [web_search]
    @Published var advisorEnabled = false
    @Published var advisorModel = ""
    @Published var advisorMaxTokens: Int = 4096
    @Published var webSearchEnabled = false
    @Published var webSearchProvider = "duckduckgo"
    @Published var webSearchBaseURL = "http://127.0.0.1:8888"
    @Published var webSearchApiKey = ""
    @Published var webSearchApiKeyEnv = ""
    @Published var webSearchMaxResults: Int = 5

    /// Last upstream error from proxy (quota / 401 / mid-SSE) for Settings toast.
    @Published var lastUpstreamError: String = ""
    @Published var lastUpstreamErrorAt: TimeInterval = 0
    private var lastSeenErrorAt: TimeInterval = 0

    /// Tray should repaint when status color changes.
    var onStatusChange: (() -> Void)?

    private var child: Process?
    private var weStartedProxy = false
    private var lastIconStatus: ProxyStatus?

    private var baseURL: URL {
        URL(string: "http://127.0.0.1:\(port)")!
    }

    func startProxyIfNeeded() {
        if healthOK() {
            setProxyStatus(.running)
            return
        }
        setProxyStatus(.starting)
        guard let bin = findSpockBinary() else {
            setStatus("spock binary not found next to app", error: true)
            setProxyStatus(.error)
            return
        }
        let proc = Process()
        proc.executableURL = bin
        // Prefer ~/Library/Logs/Spock/spock.log for easy `tail -f`
        let logDir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Logs/Spock", isDirectory: true)
        try? FileManager.default.createDirectory(at: logDir, withIntermediateDirectories: true)
        let log = logDir.appendingPathComponent("spock.log")
        proc.arguments = ["serve", "--port", "\(port)", "--log-file", log.path]
        // Also capture any pre-log-file stderr into the same path.
        FileManager.default.createFile(atPath: log.path, contents: nil)
        if let fh = try? FileHandle(forWritingTo: log) {
            proc.standardOutput = fh
            proc.standardError = fh
        }
        do {
            try proc.run()
            child = proc
            weStartedProxy = true
            for _ in 0..<40 {
                if healthOK() {
                    setProxyStatus(.running)
                    setStatus("Proxy started", error: false)
                    return
                }
                Thread.sleep(forTimeInterval: 0.1)
            }
            setStatus("Proxy failed to start — see \(log.path)", error: true)
            setProxyStatus(.error)
        } catch {
            setStatus("Failed to start proxy: \(error.localizedDescription)", error: true)
            setProxyStatus(.error)
        }
    }

    func stopProxyIfWeStartedIt() {
        guard weStartedProxy, let child else { return }
        child.terminate()
        self.child = nil
        setProxyStatus(.stopped)
    }

    func refresh() {
        refreshQuiet()
        loadConfig()
    }

    func refreshQuiet() {
        let up = healthOK()
        proxyUp = up
        if up {
            // Proxy is healthy. If our tracked child died but something else is
            // still serving (hot-swap, manual restart, second instance), clear
            // the stale Process handle — do NOT show "Proxy process exited".
            if weStartedProxy, let child, !child.isRunning {
                self.child = nil
                weStartedProxy = false
            }
            setProxyStatus(.running)
            if let status = getJSON(path: "/spock/v1/status") {
                // Only sync profile from proxy if it differs — avoids fighting the picker mid-gesture.
                // setProfile writes proxy first; this is the source of truth after that.
                if let p = status["profile"] as? String, p != profile {
                    profile = p
                }
                if let port = status["port"] as? Int { self.port = port }
                if let bind = status["bind"] as? String { self.bind = bind }
                if let ps = status["profiles"] as? [String] { profiles = ps.sorted() }
                if let path = status["config_path"] as? String { configPath = path }
                if let auth = status["xai_auth"] as? [String: Any] {
                    authPresent = (auth["present"] as? Bool) ?? false
                    authSource = (auth["source"] as? String) ?? (authPresent ? "unknown" : "none")
                } else {
                    authPresent = false
                    authSource = "none"
                }
                // Surface last upstream error as a Settings toast once per new event.
                if let err = status["last_upstream_error"] as? [String: Any],
                   let msg = err["message"] as? String, !msg.isEmpty {
                    let at = (err["at_unix"] as? Double) ?? 0
                    lastUpstreamError = msg
                    lastUpstreamErrorAt = at
                    if at > lastSeenErrorAt {
                        lastSeenErrorAt = at
                        setStatus(msg, error: true)
                    }
                }
            }
        } else {
            if weStartedProxy, let child, child.isRunning {
                setProxyStatus(.starting)
            } else if weStartedProxy, let child, !child.isRunning {
                setProxyStatus(.error)
                setStatus("Proxy process exited — reopen Spock or run: spock serve", error: true)
                self.child = nil
                weStartedProxy = false
            } else if statusIsError {
                setProxyStatus(.error)
            } else {
                setProxyStatus(.stopped)
            }
        }
    }

    private func setProxyStatus(_ status: ProxyStatus) {
        let changed = proxyStatus != status
        proxyStatus = status
        proxyUp = (status == .running)
        if changed || lastIconStatus != status {
            lastIconStatus = status
            DispatchQueue.main.async { [weak self] in
                self?.onStatusChange?()
            }
        }
    }

    func loadConfig() {
        guard let raw = getJSON(path: "/spock/v1/config") else { return }
        if let server = raw["server"] as? [String: Any] {
            if let p = server["profile"] as? String { profile = p }
            if let port = server["port"] as? Int { self.port = port }
            if let bind = server["bind"] as? String { self.bind = bind }
        }
        if let path = raw["config_path"] as? String { configPath = path }

        if let arr = raw["backends"] as? [[String: Any]] {
            backends = arr.map { b in
                BackendRow(
                    id: UUID(),
                    name: b["name"] as? String ?? "",
                    type: b["type"] as? String ?? "openai",
                    baseURL: b["base_url"] as? String ?? "",
                    apiKey: b["api_key"] as? String ?? ""
                )
            }
        }
        if let arr = raw["profiles"] as? [[String: Any]] {
            profileRows = arr.map { p in
                ProfileRow(
                    id: UUID(),
                    name: p["name"] as? String ?? "",
                    defaultRoute: p["default"] as? String ?? "",
                    haiku: p["haiku"] as? String ?? "",
                    sonnet: p["sonnet"] as? String ?? "",
                    opus: p["opus"] as? String ?? "",
                    fable: p["fable"] as? String ?? ""
                )
            }
            profiles = profileRows.map(\.name).sorted()
        }
        if let adv = raw["advisor"] as? [String: Any] {
            advisorEnabled = adv["enabled"] as? Bool ?? false
            advisorModel = adv["model"] as? String ?? ""
            if let mt = adv["max_tokens"] as? Int {
                advisorMaxTokens = mt > 0 ? mt : 4096
            } else if let mt = adv["max_tokens"] as? Double {
                advisorMaxTokens = mt > 0 ? Int(mt) : 4096
            }
        } else {
            advisorEnabled = false
            advisorModel = ""
            advisorMaxTokens = 4096
        }
        if let ws = raw["web_search"] as? [String: Any] {
            webSearchEnabled = ws["enabled"] as? Bool ?? false
            let prov = (ws["provider"] as? String ?? "duckduckgo").trimmingCharacters(in: .whitespacesAndNewlines)
            webSearchProvider = prov.isEmpty ? "duckduckgo" : prov
            let bu = (ws["base_url"] as? String ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            webSearchBaseURL = bu.isEmpty ? "http://127.0.0.1:8888" : bu
            webSearchApiKey = ws["api_key"] as? String ?? ""
            webSearchApiKeyEnv = ws["api_key_env"] as? String ?? ""
            if let mr = ws["max_results"] as? Int {
                webSearchMaxResults = mr > 0 ? mr : 5
            } else if let mr = ws["max_results"] as? Double {
                webSearchMaxResults = mr > 0 ? Int(mr) : 5
            }
        } else {
            webSearchEnabled = false
            webSearchProvider = "duckduckgo"
            webSearchBaseURL = "http://127.0.0.1:8888"
            webSearchApiKey = ""
            webSearchApiKeyEnv = ""
            webSearchMaxResults = 5
        }
    }

    func saveConfig() {
        // Trim keys/URLs so whitespace-only doesn't look "set" then fall through to OAuth.
        for i in backends.indices {
            backends[i].name = backends[i].name.trimmingCharacters(in: .whitespacesAndNewlines)
            backends[i].baseURL = backends[i].baseURL.trimmingCharacters(in: .whitespacesAndNewlines)
            backends[i].apiKey = backends[i].apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        }
        advisorModel = advisorModel.trimmingCharacters(in: .whitespacesAndNewlines)
        webSearchApiKey = webSearchApiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        webSearchApiKeyEnv = webSearchApiKeyEnv.trimmingCharacters(in: .whitespacesAndNewlines)
        webSearchBaseURL = webSearchBaseURL.trimmingCharacters(in: .whitespacesAndNewlines)
        let prov = webSearchProvider.trimmingCharacters(in: .whitespacesAndNewlines)
        webSearchProvider = prov.isEmpty ? "duckduckgo" : prov
        if webSearchBaseURL.isEmpty { webSearchBaseURL = "http://127.0.0.1:8888" }
        if advisorMaxTokens <= 0 { advisorMaxTokens = 4096 }
        if webSearchMaxResults <= 0 { webSearchMaxResults = 5 }

        let body: [String: Any] = [
            "version": "0.2.0",
            "config_path": configPath,
            "server": [
                "bind": bind,
                "port": port,
                "profile": profile,
            ],
            "backends": backends.map { b in
                [
                    "name": b.name,
                    "type": b.type,
                    "base_url": b.baseURL,
                    "api_key": b.apiKey,
                    "api_key_env": "",
                    "extra_headers_text": "",
                ] as [String: Any]
            },
            "profiles": profileRows.map { p in
                [
                    "name": p.name,
                    "default": p.defaultRoute,
                    "haiku": p.haiku,
                    "sonnet": p.sonnet,
                    "opus": p.opus,
                    "fable": p.fable,
                ] as [String: Any]
            },
            "advisor": [
                "enabled": advisorEnabled,
                "model": advisorModel,
                "max_tokens": advisorMaxTokens,
            ] as [String: Any],
            "web_search": [
                "enabled": webSearchEnabled,
                "provider": webSearchProvider,
                "base_url": webSearchBaseURL,
                "api_key": webSearchApiKey,
                "api_key_env": webSearchApiKeyEnv,
                "max_results": webSearchMaxResults,
            ] as [String: Any],
        ]
        if let resp = putJSON(path: "/spock/v1/config", body: body) {
            if let ok = resp["ok"] as? Bool, ok {
                loadConfig()
                // Re-read live auth source so UI proves key beat OAuth.
                refreshQuiet()
                let xaiKeySet = backends.contains {
                    $0.type == "xai" && !$0.apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                }
                var msg = (resp["message"] as? String) ?? "Saved"
                if xaiKeySet {
                    msg += " · xAI auth: \(authSource)"
                    if authSource != "config_api_key" {
                        setStatus(
                            "\(msg) — expected config_api_key (got \(authSource)). Check xai row key + Save.",
                            error: true
                        )
                        return
                    }
                } else {
                    msg += " · xAI auth: \(authSource)"
                }
                setStatus(msg, error: false)
            } else {
                setStatus((resp["error"] as? String) ?? "Save failed", error: true)
            }
        } else {
            setStatus("Save failed — is the proxy running?", error: true)
        }
    }

    /// Human label for status pill / settings.
    var authSourceLabel: String {
        switch authSource {
        case "config_api_key": return "API key (config)"
        case "env_XAI_TOKEN", "env": return "API key (env XAI_TOKEN)"
        case "oauth": return "OAuth (subscription)"
        case "none", "": return "none"
        default: return authSource
        }
    }

    func setProfile(_ name: String) {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else { return }
        // Optimistic local update (picker already set this; keep it stable across refresh)
        profile = name
        if let resp = postJSON(path: "/spock/v1/profile", body: ["profile": name]) {
            if let ok = resp["ok"] as? Bool, ok {
                setStatus("Active profile → \(name)", error: false)
            } else {
                let err = resp["error"] as? String ?? "Profile switch failed"
                setStatus(err, error: true)
                // Reload truth from proxy if switch rejected
                refreshQuiet()
            }
        } else {
            setStatus("Profile switch failed — proxy down?", error: true)
        }
    }

    func reloadFromDisk() {
        if let resp = postJSON(path: "/spock/v1/reload", body: [:]) {
            if let ok = resp["ok"] as? Bool, ok {
                setStatus("Reloaded from disk", error: false)
                loadConfig()
            } else {
                setStatus((resp["error"] as? String) ?? "Reload failed", error: true)
            }
        }
    }

    func loginXAI() {
        // OAuth device flow only — does NOT install a console API key.
        // If config already has api_key, warn so users don't think Login "fixes" keys.
        let hasConfigKey = backends.contains {
            $0.type == "xai" && !$0.apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
        if hasConfigKey || authSource == "config_api_key" {
            setStatus(
                "xAI API key is already configured (source: \(authSourceLabel)). Login is OAuth-only and is not needed for API keys.",
                error: false
            )
            // Still allow OAuth if they insist? Prefer not to surprise — skip device flow.
            return
        }
        guard let bin = findSpockBinary() else {
            setStatus("spock binary not found", error: true)
            return
        }
        let escaped = bin.path.replacingOccurrences(of: "\"", with: "\\\"")
        let script = """
        tell application "Terminal"
          activate
          do script "\\"\(escaped)\\" login; echo; echo Done — OAuth only. For console API key: Settings → xai → API key → Save & Apply."
        end tell
        """
        if let apple = NSAppleScript(source: script) {
            var err: NSDictionary?
            apple.executeAndReturnError(&err)
            if let err {
                setStatus("Login launch failed: \(err)", error: true)
            } else {
                setStatus("OAuth login started in Terminal (not API key)", error: false)
            }
        }
    }

    func logoutXAI() {
        _ = postJSON(path: "/spock/v1/logout", body: [:])
        refreshQuiet()
        let note: String
        if authSource == "config_api_key" {
            note = "OAuth cleared · API key in config still active"
        } else {
            note = "Logged out of xAI OAuth"
        }
        setStatus(note, error: false)
    }

    func addBackend() {
        // Unique default name so a second "Add backend" doesn't collide with existing "ollama".
        var n = 2
        var name = "lan"
        let taken = Set(backends.map(\.name))
        if taken.contains(name) {
            while taken.contains("lan-\(n)") { n += 1 }
            name = "lan-\(n)"
        }
        backends.append(
            BackendRow(
                id: UUID(),
                name: name,
                type: "openai",
                baseURL: "http://127.0.0.1:8080/v1",
                apiKey: ""
            )
        )
    }

    func removeBackend(_ row: BackendRow) {
        backends.removeAll { $0.id == row.id }
    }

    func addProfile() {
        profileRows.append(
            ProfileRow(
                id: UUID(),
                name: "custom",
                defaultRoute: "xai:grok-4.5",
                haiku: "",
                sonnet: "",
                opus: "",
                fable: ""
            )
        )
    }

    func removeProfile(_ row: ProfileRow) {
        profileRows.removeAll { $0.id == row.id }
        profiles = profileRows.map(\.name).sorted()
    }

    /// Fetch models from a backend (Ollama / llama-server / xAI) via Spock admin API.
    /// Uses the **live proxy** config — click Save & Apply first if you just changed the base URL.
    func fetchModels(forBackend name: String) {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else {
            setStatus("Backend name is empty", error: true)
            return
        }
        // Prefer discovering against the URL currently shown in the form (may be unsaved).
        // Admin API only knows *saved* backends, so we Save is still recommended for new names.
        let encoded = name.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? name
        discoveringBackend = name
        setStatus("Fetching models from \(name)…", error: false)

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            // Ensure proxy is up
            if !self.healthOK() {
                DispatchQueue.main.async {
                    self.discoveringBackend = nil
                    self.setStatus("Proxy is down — cannot fetch models", error: true)
                }
                return
            }
            let path = "/spock/v1/backends/\(encoded)/models"
            let resp = self.getJSON(path: path)
            DispatchQueue.main.async {
                self.discoveringBackend = nil
                guard let resp else {
                    self.setStatus(
                        "Fetch failed for \(name). Is proxy up? Save & Apply if this backend is new.",
                        error: true
                    )
                    return
                }
                if let ok = resp["ok"] as? Bool, ok, let models = resp["models"] as? [String] {
                    self.discoveredModels[name] = models
                    if models.isEmpty {
                        self.setStatus("\(name) returned 0 models", error: true)
                    } else {
                        self.setStatus("Fetched \(models.count) model(s) from \(name)", error: false)
                    }
                } else {
                    let err = resp["error"] as? String ?? "unknown error"
                    self.discoveredModels[name] = []
                    // Common case: backend name not in saved config yet
                    if err.contains("unknown backend") {
                        self.setStatus(
                            "Unknown backend “\(name)” on proxy — Save & Apply, then Fetch again",
                            error: true
                        )
                    } else {
                        self.setStatus("\(name): \(err)", error: true)
                    }
                }
            }
        }
    }

    /// All discovered models as route strings: "backend:model"
    var routeSuggestions: [String] {
        var out: [String] = []
        for (backend, models) in discoveredModels {
            for m in models {
                out.append("\(backend):\(m)")
            }
        }
        if !out.contains(where: { $0.hasPrefix("xai:") }) {
            out.append(contentsOf: ["xai:grok-4.5", "xai:grok-4.3"])
        }
        return out.sorted()
    }

    private func healthOK() -> Bool {
        var req = URLRequest(url: baseURL.appendingPathComponent("health"))
        req.timeoutInterval = 0.8
        let sem = DispatchSemaphore(value: 0)
        var ok = false
        URLSession.shared.dataTask(with: req) { _, resp, _ in
            ok = (resp as? HTTPURLResponse)?.statusCode == 200
            sem.signal()
        }.resume()
        _ = sem.wait(timeout: .now() + 1.0)
        return ok
    }

    private func getJSON(path: String) -> [String: Any]? {
        // path may include query; build carefully
        let url: URL?
        if path.hasPrefix("http") {
            url = URL(string: path)
        } else {
            url = URL(string: path, relativeTo: baseURL)?.absoluteURL
        }
        guard let url else { return nil }
        var req = URLRequest(url: url)
        req.timeoutInterval = 15
        return syncJSON(req)
    }

    private func putJSON(path: String, body: [String: Any]) -> [String: Any]? {
        guard let url = URL(string: path, relativeTo: baseURL) else { return nil }
        var req = URLRequest(url: url)
        req.httpMethod = "PUT"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: body)
        req.timeoutInterval = 5
        return syncJSON(req)
    }

    private func postJSON(path: String, body: [String: Any]) -> [String: Any]? {
        guard let url = URL(string: path, relativeTo: baseURL) else { return nil }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: body)
        req.timeoutInterval = 5
        return syncJSON(req)
    }

    private func syncJSON(_ req: URLRequest) -> [String: Any]? {
        let sem = DispatchSemaphore(value: 0)
        var result: [String: Any]?
        URLSession.shared.dataTask(with: req) { data, _, _ in
            defer { sem.signal() }
            guard let data,
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            else { return }
            result = obj
        }.resume()
        _ = sem.wait(timeout: .now() + 6)
        return result
    }

    private func findSpockBinary() -> URL? {
        if let exe = Bundle.main.executableURL {
            let dir = exe.deletingLastPathComponent()
            for name in ["spock-proxy", "spock"] {
                let cand = dir.appendingPathComponent(name)
                if FileManager.default.isExecutableFile(atPath: cand.path) {
                    return cand
                }
            }
        }
        let path = [
            "/usr/local/bin/spock",
            "/opt/homebrew/bin/spock",
            FileManager.default.currentDirectoryPath + "/target/release/spock",
            NSString(string: "~/Documents/GitHub/Spock/target/release/spock").expandingTildeInPath,
        ]
        for p in path {
            if FileManager.default.isExecutableFile(atPath: p) {
                return URL(fileURLWithPath: p)
            }
        }
        return nil
    }

    private func setStatus(_ msg: String, error: Bool) {
        statusMessage = msg
        statusIsError = error
    }

    func dismissStatus() {
        statusMessage = ""
        statusIsError = false
    }
}

struct BackendRow: Identifiable, Equatable {
    let id: UUID
    var name: String
    var type: String
    var baseURL: String
    var apiKey: String
}

struct ProfileRow: Identifiable, Equatable {
    let id: UUID
    var name: String
    var defaultRoute: String
    var haiku: String
    var sonnet: String
    var opus: String
    var fable: String
}

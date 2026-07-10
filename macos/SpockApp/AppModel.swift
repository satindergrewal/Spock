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
    @Published var statusMessage: String = ""
    @Published var statusIsError = false
    /// Discovered models per backend name: ["ollama": ["qwen2.5:14b", …]]
    @Published var discoveredModels: [String: [String]] = [:]
    @Published var discoveringBackend: String? = nil

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
        proc.arguments = ["serve", "--port", "\(port)"]
        let log = FileManager.default.temporaryDirectory.appendingPathComponent("spock-app.log")
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
            if weStartedProxy, let child, !child.isRunning {
                setProxyStatus(.error)
                setStatus("Proxy process exited", error: true)
            } else {
                setProxyStatus(.running)
            }
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
                }
            }
        } else {
            if weStartedProxy, let child, child.isRunning {
                setProxyStatus(.starting)
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
    }

    func saveConfig() {
        let body: [String: Any] = [
            "version": "0.1.0",
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
        ]
        if let resp = putJSON(path: "/spock/v1/config", body: body) {
            if let ok = resp["ok"] as? Bool, ok {
                setStatus((resp["message"] as? String) ?? "Saved", error: false)
                loadConfig()
            } else {
                setStatus((resp["error"] as? String) ?? "Save failed", error: true)
            }
        } else {
            setStatus("Save failed — is the proxy running?", error: true)
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
        guard let bin = findSpockBinary() else {
            setStatus("spock binary not found", error: true)
            return
        }
        let escaped = bin.path.replacingOccurrences(of: "\"", with: "\\\"")
        let script = """
        tell application "Terminal"
          activate
          do script "\\"\(escaped)\\" login; echo; echo Done — you can close this window."
        end tell
        """
        if let apple = NSAppleScript(source: script) {
            var err: NSDictionary?
            apple.executeAndReturnError(&err)
            if let err {
                setStatus("Login launch failed: \(err)", error: true)
            } else {
                setStatus("Login started in Terminal", error: false)
            }
        }
    }

    func logoutXAI() {
        _ = postJSON(path: "/spock/v1/logout", body: [:])
        authPresent = false
        setStatus("Logged out of xAI", error: false)
    }

    func addBackend() {
        backends.append(
            BackendRow(
                id: UUID(),
                name: "ollama",
                type: "openai",
                baseURL: "http://127.0.0.1:11434/v1",
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

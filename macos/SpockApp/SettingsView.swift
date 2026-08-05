import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    serverSection
                    backendsSection
                    profilesSection
                    catalogSection
                    serverToolsSection
                }
                .padding(20)
            }
        }
        .frame(minWidth: 760, minHeight: 600)
        .onAppear { model.refresh() }
    }

    private var header: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Spock Settings")
                        .font(.headline)
                    Text(model.configPath.isEmpty ? "127.0.0.1:\(model.port)" : model.configPath)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer()
                if !model.statusMessage.isEmpty && !model.statusIsError {
                    Text(model.statusMessage)
                        .font(.caption)
                        .foregroundStyle(.green)
                        .lineLimit(2)
                        .frame(maxWidth: 280, alignment: .trailing)
                }
                Button("Reload") { model.reloadFromDisk() }
                Button("Save & Apply") { model.saveConfig() }
                    .keyboardShortcut("s", modifiers: .command)
                    .buttonStyle(.borderedProminent)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            if model.statusIsError && !model.statusMessage.isEmpty {
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                    Text(model.statusMessage)
                        .font(.caption)
                        .foregroundStyle(.primary)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Button("Dismiss") {
                        model.dismissStatus()
                    }
                    .buttonStyle(.borderless)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
                .background(Color.red.opacity(0.08))
            }
        }
    }

    private var serverSection: some View {
        GroupBox("Server") {
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    labeled("Bind") {
                        TextField("127.0.0.1", text: $model.bind)
                            .textFieldStyle(.roundedBorder)
                            .frame(width: 140)
                    }
                    labeled("Port") {
                        TextField("8048", value: $model.port, format: .number)
                            .textFieldStyle(.roundedBorder)
                            .frame(width: 80)
                    }
                    labeled("Active profile") {
                        Picker("", selection: $model.profile) {
                            ForEach(model.profileRows.map(\.name), id: \.self) { name in
                                Text(name).tag(name)
                            }
                        }
                        .labelsHidden()
                        .frame(minWidth: 160)
                        // Persist immediately so the 2s tray refresh doesn't snap back
                        // to whatever the proxy still has on disk (usually xai-only).
                        .onChange(of: model.profile) { newValue in
                            model.setProfile(newValue)
                        }
                    }
                    Spacer()
                    statusPill
                }
                Text("Non-loopback binds are forced to 127.0.0.1 by the proxy. Port changes need a restart.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .padding(8)
        }
    }

    private var statusPill: some View {
        HStack(spacing: 6) {
            Circle()
                .fill(model.proxyUp ? Color.green : Color.red)
                .frame(width: 8, height: 8)
            Text(model.proxyUp ? "proxy up" : "proxy down")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text("· xAI: \(model.authSourceLabel)")
                .font(.caption)
                .foregroundStyle(authSourceColor)
                .help(authSourceHelp)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(Color.secondary.opacity(0.12), in: Capsule())
    }

    private var authSourceColor: Color {
        switch model.authSource {
        case "config_api_key", "env_XAI_TOKEN", "env": return .green
        case "oauth": return .orange
        default: return .secondary
        }
    }

    private var authSourceHelp: String {
        switch model.authSource {
        case "config_api_key":
            return "Using [backends.xai] api_key from config (beats OAuth)."
        case "env_XAI_TOKEN", "env":
            return "Using XAI_TOKEN environment variable."
        case "oauth":
            return "Using SuperGrok OAuth tokens. Paste a console API key in the xai row and Save & Apply to override."
        default:
            return "No xAI credentials. Set API key on the xai backend + Save & Apply, or menu Login… for OAuth."
        }
    }

    private var backendsSection: some View {
        GroupBox("Backends") {
            VStack(alignment: .leading, spacing: 8) {
                ForEach($model.backends) { $b in
                    VStack(alignment: .leading, spacing: 6) {
                        HStack(spacing: 8) {
                            TextField("name", text: $b.name)
                                .textFieldStyle(.roundedBorder)
                                .frame(width: 100)
                            Picker("", selection: $b.type) {
                                Text("OAuth").tag("oauth")
                                Text("API Key").tag("api_key")
                                Text("Anthropic").tag("anthropic")
                            }
                            .labelsHidden()
                            .frame(width: 100)
                            .onChange(of: b.type) { _, newVal in
                                if newVal == "oauth" {
                                    if b.provider.isEmpty {
                                        b.provider = "xai"
                                    }
                                    if b.baseURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                                        b.baseURL = "https://api.x.ai/v1"
                                    }
                                } else {
                                    // API Key / Anthropic are not OAuth — never keep a stale provider chip.
                                    b.provider = ""
                                }
                            }
                            // Provider only for OAuth. API Key backends (Qwen Token Plan, Ollama, …) have no provider.
                            if b.type == "oauth" {
                                Picker("", selection: $b.provider) {
                                    Text("xai").tag("xai")
                                    Text("kimi").tag("kimi")
                                    if !b.provider.isEmpty && b.provider != "xai" && b.provider != "kimi" {
                                        Text(b.provider).tag(b.provider)
                                    }
                                }
                                .labelsHidden()
                                .frame(width: 90)
                                .onChange(of: b.provider) { _, newVal in
                                    let url = b.baseURL.trimmingCharacters(in: .whitespacesAndNewlines)
                                    if url.isEmpty
                                        || url.contains("api.x.ai")
                                        || url.contains("api.kimi.com")
                                    {
                                        if newVal == "kimi" {
                                            b.baseURL = "https://api.kimi.com/coding/v1"
                                        } else if newVal == "xai" {
                                            b.baseURL = "https://api.x.ai/v1"
                                        }
                                    }
                                }
                            }
                            TextField("base URL", text: $b.baseURL)
                                .textFieldStyle(.roundedBorder)
                            Group {
                                if b.type == "oauth" {
                                    SecureField("API key (optional escape hatch)", text: $b.apiKey)
                                        .textFieldStyle(.roundedBorder)
                                        .frame(minWidth: 200)
                                        .help("Optional. When set, beats OAuth for this backend. Prefer menu Login for subscription tokens.")
                                } else {
                                    SecureField("api key (optional)", text: $b.apiKey)
                                        .textFieldStyle(.roundedBorder)
                                        .frame(minWidth: 160)
                                        .help("Bearer for API Key backends / Anthropic key")
                                }
                            }
                            Button {
                                // Use saved name if empty field mid-edit
                                let name = b.name.trimmingCharacters(in: .whitespaces)
                                guard !name.isEmpty else { return }
                                model.fetchModels(forBackend: name)
                            } label: {
                                if model.discoveringBackend == b.name {
                                    ProgressView()
                                        .controlSize(.small)
                                } else {
                                    Label("Fetch models", systemImage: "arrow.triangle.2.circlepath")
                                }
                            }
                            .disabled(model.discoveringBackend != nil || b.name.isEmpty)
                            .help("Query this backend for model IDs (Save & Apply first if URL changed)")

                            Button(role: .destructive) {
                                model.removeBackend(b)
                            } label: {
                                Image(systemName: "minus.circle.fill")
                            }
                            .buttonStyle(.borderless)
                        }

                        if b.type == "oauth" {
                            Text(oauthKeyHint(for: b))
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                        }

                        if let models = model.discoveredModels[b.name], !models.isEmpty {
                            Text("\(models.count) model(s): \(models.prefix(8).joined(separator: ", "))\(models.count > 8 ? "…" : "")")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(2)
                        }
                    }
                    .padding(.vertical, 2)
                }
                HStack(alignment: .top) {
                    Button {
                        model.addBackend()
                    } label: {
                        Label("Add backend", systemImage: "plus.circle")
                    }
                    Text("OAuth: pick provider (xai/kimi) + menu Login. Optional API key field beats OAuth. API Key: OpenAI-compatible base …/v1 + key. Anthropic: Messages passthrough. Save before Fetch.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(8)
        }
    }

    private var profilesSection: some View {
        GroupBox("Profiles & routes") {
            VStack(alignment: .leading, spacing: 8) {
                Text("Route format: backend:model  e.g. xai:grok-4.5 or ollama:qwen2.5:14b — pick from fetched models or type freely")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Grid(alignment: .leading, horizontalSpacing: 8, verticalSpacing: 8) {
                    GridRow {
                        Text("Name").font(.caption).foregroundStyle(.secondary)
                        Text("default").font(.caption).foregroundStyle(.secondary)
                        Text("haiku").font(.caption).foregroundStyle(.secondary)
                        Text("sonnet").font(.caption).foregroundStyle(.secondary)
                        Text("opus").font(.caption).foregroundStyle(.secondary)
                        Text("fable").font(.caption).foregroundStyle(.secondary)
                        Text("").font(.caption)
                    }
                    ForEach($model.profileRows) { $p in
                        GridRow {
                            TextField("name", text: $p.name)
                                .textFieldStyle(.roundedBorder)
                                .frame(width: 90)
                            routeField(text: $p.defaultRoute, placeholder: "xai:…")
                            routeField(text: $p.haiku, placeholder: "")
                            routeField(text: $p.sonnet, placeholder: "")
                            routeField(text: $p.opus, placeholder: "")
                            routeField(text: $p.fable, placeholder: "")
                            Button(role: .destructive) {
                                model.removeProfile(p)
                            } label: {
                                Image(systemName: "minus.circle.fill")
                            }
                            .buttonStyle(.borderless)
                        }
                    }
                }

                HStack {
                    Button {
                        model.addProfile()
                    } label: {
                        Label("Add profile", systemImage: "plus.circle")
                    }
                    if !model.discoveredModels.isEmpty {
                        Text("Suggestions loaded for: \(model.discoveredModels.keys.sorted().joined(separator: ", "))")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .padding(8)
        }
    }

    /// Curated shortlist for Grok Build / external pickers via GET /v1/models.
    /// Orthogonal to Profiles (Claude Code 5-slot map).
    private var catalogSection: some View {
        GroupBox("Catalog (Grok Build / external pickers)") {
            VStack(alignment: .leading, spacing: 8) {
                Text("Shortlist served on GET /v1/models. Ids are backend:model. Empty catalog = dump every backend model (noisy). Profiles above stay Claude Code only.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)

                Grid(alignment: .leading, horizontalSpacing: 8, verticalSpacing: 8) {
                    GridRow {
                        Text("id").font(.caption).foregroundStyle(.secondary)
                        Text("name").font(.caption).foregroundStyle(.secondary)
                        Text("description").font(.caption).foregroundStyle(.secondary)
                        Text("context").font(.caption).foregroundStyle(.secondary)
                        Text("").font(.caption)
                    }
                    ForEach($model.catalogRows) { $e in
                        GridRow {
                            routeField(text: $e.routeId, placeholder: "xai:grok-4.5")
                                .frame(minWidth: 180)
                            TextField("display name", text: $e.name)
                                .textFieldStyle(.roundedBorder)
                                .frame(minWidth: 100)
                            TextField("optional", text: $e.description)
                                .textFieldStyle(.roundedBorder)
                                .frame(minWidth: 100)
                            TextField("500000", text: $e.contextWindow)
                                .textFieldStyle(.roundedBorder)
                                .frame(width: 90)
                                .help("Tokens. Leave blank to discover from backend / Grok default (~200k).")
                            Button(role: .destructive) {
                                model.removeCatalogEntry(e)
                            } label: {
                                Image(systemName: "minus.circle.fill")
                            }
                            .buttonStyle(.borderless)
                        }
                    }
                }

                HStack(alignment: .center, spacing: 12) {
                    Button {
                        model.addCatalogEntry()
                    } label: {
                        Label("Add entry", systemImage: "plus.circle")
                    }
                    if !model.routeSuggestions.isEmpty {
                        Menu {
                            ForEach(model.routeSuggestions, id: \.self) { route in
                                Button(route) {
                                    model.addRouteToCatalog(route)
                                }
                            }
                        } label: {
                            Label("Add from fetched models", systemImage: "plus.magnifyingglass")
                        }
                        .help("Fetch models on a backend row first, then pick routes here.")
                    }
                    Text("context = tokens (e.g. 500000). Save & Apply publishes to Grok Build.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .padding(8)
        }
    }

    /// Spock-native server tools (advisor + web search). Not Claude Code / VSCodium settings.
    private var serverToolsSection: some View {
        GroupBox("Server tools (Spock)") {
            VStack(alignment: .leading, spacing: 14) {
                Text("Emulates Anthropic server tools on Spock. Off by default. Save & Apply writes [advisor] / [web_search] in config.toml.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)

                // Advisor
                VStack(alignment: .leading, spacing: 8) {
                    Toggle("Enable advisor", isOn: $model.advisorEnabled)
                        .help("When Claude Code sends advisor_20260301, Spock runs a nested review on another model route.")
                    HStack(spacing: 12) {
                        labeled("Advisor model (optional)") {
                            // Same route picker style as Profiles & routes
                            routeField(
                                text: $model.advisorModel,
                                placeholder: "backend:model or leave empty"
                            )
                            .frame(minWidth: 280)
                            .disabled(!model.advisorEnabled)
                        }
                        labeled("Max tokens") {
                            TextField("4096", value: $model.advisorMaxTokens, format: .number)
                                .textFieldStyle(.roundedBorder)
                                .frame(width: 90)
                                .disabled(!model.advisorEnabled)
                        }
                    }
                    Text("Pick a fetched backend:model (Fetch models first), or leave empty for Claude tools[].model / fable / profile default.")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }

                Divider()

                // Web search
                VStack(alignment: .leading, spacing: 8) {
                    Toggle("Enable web search", isOn: $model.webSearchEnabled)
                        .help("Emulates web_search_20250305 for Claude Code WebSearch nested calls.")
                    HStack(spacing: 12) {
                        labeled("Provider") {
                            Picker("", selection: $model.webSearchProvider) {
                                Text("searxng (local, recommended)").tag("searxng")
                                Text("duckduckgo (no key, limited)").tag("duckduckgo")
                                Text("brave").tag("brave")
                                Text("serper").tag("serper")
                            }
                            .labelsHidden()
                            .frame(minWidth: 220)
                            .disabled(!model.webSearchEnabled)
                        }
                        labeled("Max results") {
                            TextField("5", value: $model.webSearchMaxResults, format: .number)
                                .textFieldStyle(.roundedBorder)
                                .frame(width: 70)
                                .disabled(!model.webSearchEnabled)
                        }
                    }
                    if model.webSearchProvider == "searxng" || model.webSearchProvider == "searx" {
                        labeled("SearXNG base URL") {
                            TextField("http://127.0.0.1:8888", text: $model.webSearchBaseURL)
                                .textFieldStyle(.roundedBorder)
                                .frame(minWidth: 300)
                                .disabled(!model.webSearchEnabled)
                                .help("Root of your SearXNG instance (no /search path). JSON format must be enabled.")
                        }
                    }
                    if model.webSearchProvider == "brave" || model.webSearchProvider == "serper" {
                        HStack(spacing: 12) {
                            labeled("API key") {
                                SecureField("API key", text: $model.webSearchApiKey)
                                    .textFieldStyle(.roundedBorder)
                                    .frame(minWidth: 200)
                                    .disabled(!model.webSearchEnabled)
                            }
                            labeled("Or env var name") {
                                TextField(
                                    model.webSearchProvider == "brave" ? "BRAVE_API_KEY" : "SERPER_API_KEY",
                                    text: $model.webSearchApiKeyEnv
                                )
                                .textFieldStyle(.roundedBorder)
                                .frame(minWidth: 160)
                                .disabled(!model.webSearchEnabled)
                            }
                        }
                    }
                    Text(webSearchHelp)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(8)
        }
    }

    private var webSearchHelp: String {
        switch model.webSearchProvider {
        case "searxng", "searx":
            return "Uses your local SearXNG JSON API (no cloud key). Ensure format=json is allowed. Best option if you already run SearXNG."
        case "duckduckgo":
            return "Keyless DuckDuckGo Instant Answer — works with only Enable, but often thin results. Prefer SearXNG if you have it."
        case "brave":
            return "Brave Search API — set API key or BRAVE_API_KEY."
        case "serper":
            return "Serper.dev Google proxy — set API key or SERPER_API_KEY."
        default:
            return "Choose a provider and Save & Apply."
        }
    }

    /// Text field + optional menu of discovered backend:model routes.
    private func routeField(text: Binding<String>, placeholder: String) -> some View {
        HStack(spacing: 4) {
            TextField(placeholder, text: text)
                .textFieldStyle(.roundedBorder)
            if !model.routeSuggestions.isEmpty {
                // Hide the system Menu disclosure triangle — it stacks on top of our
                // SF Symbol and reads as a stray odd down-arrow next to the field.
                Menu {
                    ForEach(model.routeSuggestions, id: \.self) { route in
                        Button(route) { text.wrappedValue = route }
                    }
                } label: {
                    Image(systemName: "chevron.down.circle.fill")
                        .symbolRenderingMode(.hierarchical)
                        .foregroundStyle(.secondary)
                        .font(.system(size: 14))
                        .frame(width: 20, height: 20)
                        .contentShape(Rectangle())
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .help("Pick a fetched model")
            }
        }
    }

    private func labeled<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption)
                .foregroundStyle(.secondary)
            content()
        }
    }

    private func oauthKeyHint(for b: BackendRow) -> String {
        let keySet = !b.apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let prov = b.provider.isEmpty ? "provider" : b.provider
        let st = model.oauthStatus[prov]
        let src = st?.source ?? "none"
        if keySet {
            return "Key in form beats OAuth after Save & Apply. Live \(prov): \(src)."
        }
        switch src {
        case "config_api_key":
            return "Proxy uses a saved config API key for \(prov)."
        case "oauth":
            return "Proxy on \(prov) OAuth. Optional key field overrides after Save & Apply. Menu Login for device flow."
        case "env":
            return "Proxy using env token for \(prov)."
        default:
            return "No \(prov) credentials yet — menu Login… or paste API key + Save & Apply."
        }
    }
}

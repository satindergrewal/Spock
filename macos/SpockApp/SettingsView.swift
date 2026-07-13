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
            return "No xAI credentials. Set API key on the xai backend + Save & Apply, or menu Login xAI… for OAuth."
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
                                Text("xai").tag("xai")
                                Text("openai").tag("openai")
                            }
                            .labelsHidden()
                            .frame(width: 100)
                            TextField("base URL", text: $b.baseURL)
                                .textFieldStyle(.roundedBorder)
                            Group {
                                if b.type == "xai" {
                                    SecureField("xAI console API key", text: $b.apiKey)
                                        .textFieldStyle(.roundedBorder)
                                        .frame(minWidth: 200)
                                        .help("From console.x.ai. Save & Apply required. Overrides OAuth. Menu “Login xAI” is OAuth only — not this key.")
                                } else {
                                    SecureField("api key (optional)", text: $b.apiKey)
                                        .textFieldStyle(.roundedBorder)
                                        .frame(minWidth: 160)
                                        .help("Optional Bearer for OpenAI-compatible backends")
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

                        if b.type == "xai" {
                            Text(xaiKeyHint(for: b))
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
                    Text("xai: paste console API key → Save & Apply (priority: config key → XAI_TOKEN → OAuth). Menu Login xAI = OAuth only. openai: Ollama / llama-server / LAN. Base URL is …/v1 not …/v1/models. Save before Fetch.")
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

    private func xaiKeyHint(for b: BackendRow) -> String {
        let keySet = !b.apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        if keySet {
            return "Key in form — click Save & Apply. Live proxy: \(model.authSourceLabel)."
        }
        switch model.authSource {
        case "config_api_key":
            return "Proxy is using a saved config API key. Field may be empty until Reload if you edited TOML by hand."
        case "oauth":
            return "Proxy is on OAuth. Paste console API key here + Save & Apply to override (do not use menu Login for keys)."
        case "env_XAI_TOKEN", "env":
            return "Proxy is using XAI_TOKEN from the environment."
        default:
            return "No key yet. Paste console.x.ai API key → Save & Apply. Menu Login xAI is OAuth subscription only."
        }
    }
}

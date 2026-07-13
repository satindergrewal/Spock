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
                }
                .padding(20)
            }
        }
        .frame(minWidth: 760, minHeight: 600)
        .onAppear { model.refresh() }
    }

    private var header: some View {
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
            if !model.statusMessage.isEmpty {
                Text(model.statusMessage)
                    .font(.caption)
                    .foregroundStyle(model.statusIsError ? .red : .green)
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

    /// Text field + optional menu of discovered backend:model routes.
    private func routeField(text: Binding<String>, placeholder: String) -> some View {
        HStack(spacing: 2) {
            TextField(placeholder, text: text)
                .textFieldStyle(.roundedBorder)
            if !model.routeSuggestions.isEmpty {
                Menu {
                    ForEach(model.routeSuggestions, id: \.self) { route in
                        Button(route) { text.wrappedValue = route }
                    }
                } label: {
                    Image(systemName: "chevron.down.circle")
                        .foregroundStyle(.secondary)
                }
                .menuStyle(.borderlessButton)
                .frame(width: 22)
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

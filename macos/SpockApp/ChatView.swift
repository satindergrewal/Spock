import SwiftUI

/// Grok-style chat: dark bubble thread, streaming assistant replies via Spock → xAI/Ollama.
struct ChatView: View {
    @EnvironmentObject var model: AppModel
    @StateObject private var chat = ChatSession()

    var body: some View {
        VStack(spacing: 0) {
            chatHeader
            Divider()
            messageList
            Divider()
            composer
        }
        .frame(minWidth: 520, minHeight: 640)
        .background(Color(nsColor: .windowBackgroundColor))
        .onAppear {
            chat.port = model.port
            chat.modelId = chatModelId
        }
        .onChange(of: model.profile) { _ in
            chat.modelId = chatModelId
        }
    }

    private var chatModelId: String {
        if model.profile == "local-only" {
            if let route = model.profileRows.first(where: { $0.name == model.profile })?.defaultRoute {
                let parts = route.split(separator: ":", maxSplits: 1)
                if parts.count == 2 { return String(parts[1]) }
            }
            return "qwen2.5:14b"
        }
        return "grok-4.5"
    }

    private var chatHeader: some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text("Chat")
                    .font(.headline)
                Text("via Spock · \(model.profile) · \(chat.modelId)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Picker("Model", selection: $chat.modelId) {
                Text("grok-4.5").tag("grok-4.5")
                Text("grok-4.3").tag("grok-4.3")
                Text("claude-opus-4-8").tag("claude-opus-4-8")
                Text("claude-sonnet-5").tag("claude-sonnet-5")
                Text("claude-haiku-4-5").tag("claude-haiku-4-5")
            }
            .labelsHidden()
            .frame(width: 160)
            Button("Clear") {
                chat.clear()
            }
            .disabled(chat.messages.isEmpty || chat.isStreaming)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private var messageList: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    if chat.messages.isEmpty {
                        emptyState
                    }
                    ForEach(chat.messages) { msg in
                        MessageBubble(message: msg)
                            .id(msg.id)
                    }
                    if chat.isStreaming && chat.messages.last?.role == .assistant
                        && (chat.messages.last?.text.isEmpty ?? true)
                    {
                        HStack {
                            ProgressView()
                                .controlSize(.small)
                            Text("Thinking…")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        .padding(.leading, 12)
                    }
                }
                .padding(16)
            }
            .onChange(of: chat.messages.count) { _ in
                if let last = chat.messages.last {
                    withAnimation { proxy.scrollTo(last.id, anchor: .bottom) }
                }
            }
            .onChange(of: chat.messages.last?.text) { _ in
                if let last = chat.messages.last {
                    proxy.scrollTo(last.id, anchor: .bottom)
                }
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            Image(systemName: "bubble.left.and.bubble.right")
                .font(.system(size: 36))
                .foregroundStyle(.secondary)
            Text("Chat with your routed models")
                .font(.title3.weight(.semibold))
            Text("Same Spock backends as Claude Code — Grok, Ollama, or both.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 48)
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 10) {
            TextField("Message Grok…", text: $chat.draft, axis: .vertical)
                .textFieldStyle(.plain)
                .lineLimit(1...8)
                .padding(12)
                .background(Color.secondary.opacity(0.12), in: RoundedRectangle(cornerRadius: 14))
                .onSubmit {
                    if !chat.isStreaming { Task { await chat.send() } }
                }

            Button {
                Task { await chat.send() }
            } label: {
                Image(systemName: chat.isStreaming ? "stop.circle.fill" : "arrow.up.circle.fill")
                    .font(.system(size: 30))
                    .symbolRenderingMode(.hierarchical)
            }
            .buttonStyle(.plain)
            .disabled(chat.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !chat.isStreaming)
            .keyboardShortcut(.return, modifiers: .command)
        }
        .padding(14)
    }
}

struct MessageBubble: View {
    let message: ChatMessage

    var body: some View {
        HStack {
            if message.role == .user { Spacer(minLength: 40) }
            VStack(alignment: message.role == .user ? .trailing : .leading, spacing: 4) {
                Text(message.role == .user ? "You" : "Spock")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.secondary)
                Text(message.text.isEmpty && message.role == .assistant ? "…" : message.text)
                    .textSelection(.enabled)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
                    .background(bubbleColor, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                if let thinking = message.thinking, !thinking.isEmpty {
                    DisclosureGroup("Thinking") {
                        Text(thinking)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                    .font(.caption)
                }
            }
            if message.role == .assistant { Spacer(minLength: 40) }
        }
    }

    private var bubbleColor: Color {
        message.role == .user
            ? Color.accentColor.opacity(0.25)
            : Color.secondary.opacity(0.12)
    }
}

enum ChatRole {
    case user, assistant
}

struct ChatMessage: Identifiable, Equatable {
    let id: UUID
    let role: ChatRole
    var text: String
    var thinking: String?
}

@MainActor
final class ChatSession: ObservableObject {
    @Published var messages: [ChatMessage] = []
    @Published var draft: String = ""
    @Published var isStreaming = false
    @Published var modelId: String = "grok-4.5"
    var port: Int = 8048

    private var streamTask: Task<Void, Never>?

    func clear() {
        streamTask?.cancel()
        messages = []
        isStreaming = false
    }

    func send() async {
        if isStreaming {
            streamTask?.cancel()
            isStreaming = false
            return
        }
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        draft = ""
        messages.append(ChatMessage(id: UUID(), role: .user, text: text, thinking: nil))
        let assistantId = UUID()
        messages.append(ChatMessage(id: assistantId, role: .assistant, text: "", thinking: nil))
        isStreaming = true

        streamTask = Task {
            await streamCompletion(assistantId: assistantId)
            isStreaming = false
        }
        await streamTask?.value
    }

    private func streamCompletion(assistantId: UUID) async {
        // Build Anthropic-style history for /v1/messages
        var apiMessages: [[String: Any]] = []
        for m in messages where m.id != assistantId {
            apiMessages.append([
                "role": m.role == .user ? "user" : "assistant",
                "content": m.text,
            ])
        }

        let body: [String: Any] = [
            "model": modelId,
            "max_tokens": 4096,
            "stream": true,
            "messages": apiMessages,
        ]

        guard let url = URL(string: "http://127.0.0.1:\(port)/v1/messages"),
              let data = try? JSONSerialization.data(withJSONObject: body)
        else {
            updateAssistant(assistantId, text: "Failed to build request", thinking: nil)
            return
        }

        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("text/event-stream", forHTTPHeaderField: "Accept")
        req.httpBody = data
        req.timeoutInterval = 600

        do {
            let (bytes, response) = try await URLSession.shared.bytes(for: req)
            if let http = response as? HTTPURLResponse, http.statusCode >= 400 {
                var errData = Data()
                for try await b in bytes { errData.append(b) }
                let msg = String(data: errData, encoding: .utf8) ?? "HTTP \(http.statusCode)"
                updateAssistant(assistantId, text: msg, thinking: nil)
                return
            }

            var text = ""
            var thinking = ""
            var eventName = ""

            for try await line in bytes.lines {
                if Task.isCancelled { break }
                if line.hasPrefix("event:") {
                    eventName = line.dropFirst(6).trimmingCharacters(in: .whitespaces)
                    continue
                }
                guard line.hasPrefix("data:") else { continue }
                let payload = line.dropFirst(5).trimmingCharacters(in: .whitespaces)
                if payload == "[DONE]" { break }
                guard let d = payload.data(using: .utf8),
                      let obj = try? JSONSerialization.jsonObject(with: d) as? [String: Any]
                else { continue }

                if eventName == "content_block_delta" || obj["type"] as? String == "content_block_delta" {
                    if let delta = obj["delta"] as? [String: Any] {
                        let dtype = delta["type"] as? String ?? ""
                        if dtype == "text_delta", let t = delta["text"] as? String {
                            text += t
                            updateAssistant(assistantId, text: text, thinking: thinking.isEmpty ? nil : thinking)
                        } else if dtype == "thinking_delta", let t = delta["thinking"] as? String {
                            thinking += t
                            updateAssistant(assistantId, text: text, thinking: thinking)
                        }
                    }
                }
            }
            if text.isEmpty && thinking.isEmpty {
                updateAssistant(assistantId, text: "(empty response)", thinking: nil)
            }
        } catch is CancellationError {
            // stop
        } catch {
            updateAssistant(assistantId, text: "Error: \(error.localizedDescription)", thinking: nil)
        }
    }

    private func updateAssistant(_ id: UUID, text: String, thinking: String?) {
        if let i = messages.firstIndex(where: { $0.id == id }) {
            messages[i].text = text
            messages[i].thinking = thinking
        }
    }
}

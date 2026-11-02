import Foundation
import FoundationModels

/// llama.cpp's `llama-server` through its OpenAI-compatible chat endpoint
/// (llamacpp-backend.md). Provider translation only: the service owns
/// admission, tools, the key and cancellation. The helper never holds the
/// server key; with the service relay it sends the relay token instead.
public struct LlamaCppLanguageModel: LanguageModel, Sendable {
    public enum Failure: Error, Equatable, Sendable {
        case invalidConfiguration, unsupported, unavailable, invalidResponse, limit, incomplete
    }
    /// Discovery replies (`/v1/models`, `/props` with its chat template).
    static let discoveryBytes = 1_048_576
    /// One SSE line before decoding.
    static let lineBytes = 65_536
    /// Gathered arguments of one call; the service applies its 64,000-byte rule.
    static let argumentBytes = 65_536
    /// Error bodies are read to this bound and parsed for `error.type` only.
    static let errorBytes = 4_096

    public struct Settings: Hashable, Sendable {
        public let endpoint: URL
        public let name: String
        /// The service relay's 32-byte token, sent as the bearer credential.
        public let token: Data?
        public init(name: String, endpoint: URL, token: Data? = nil) throws {
            guard validModelName(name), loopbackEndpoint(endpoint), token.map({ $0.count == 32 }) ?? true
            else { throw Failure.invalidConfiguration }
            self.name = name; self.endpoint = endpoint; self.token = token
        }
        func request(_ path: String) -> URLRequest {
            var base = endpoint.absoluteString
            if base.hasSuffix("/") { base.removeLast() }
            var request = URLRequest(url: URL(string: base + path)!)
            if let token {
                request.setValue("Bearer " + token.map { String(format: "%02x", $0) }.joined(),
                    forHTTPHeaderField: "Authorization")
            }
            return request
        }
    }
    public let settings: Settings
    public let supportsTools: Bool
    public let contextTokens: UInt32
    public var thinkingControls: ThinkingControls = []
    /// The template takes `reasoning_effort` (`chat_template_caps`).
    public var reasoningEffort = false
    var inputObserver: OllamaInputObserver? = nil
    var usageObserver: InferenceUsageObserver? = nil
    var unregisteredToolHandler: UnregisteredToolReporter? = nil
    public var supportsReasoning: Bool { !thinkingControls.isEmpty }
    public var capabilities: LanguageModelCapabilities {
        .init((supportsTools ? [.toolCalling] : []) + (supportsReasoning ? [.reasoning] : []))
    }
    public var executorConfiguration: Settings { settings }

    // MARK: Discovery

    /// `GET /v1/models`, then `GET /props?model=`; the router loads the
    /// model for the second. No time limit: each ends on its reply, an
    /// error, cancellation, helper exit or shutdown.
    public static func discover(_ settings: Settings) async throws -> Self {
        let session = OllamaLanguageModel.session()
        defer { session.invalidateAndCancel() }
        return try await withTaskCancellationHandler {
            let models = try await fetch(session, settings.request("/v1/models"))
            try listed(settings.name, data: models)
            let props = try await fetch(session, settings.request("/props?model=" + percentEncoded(settings.name)))
            return try checked(settings, props: props)
        } onCancel: { session.invalidateAndCancel() }
    }

    static func fetch(_ session: URLSession, _ request: URLRequest) async throws -> Data {
        let (bytes, response) = try await session.bytes(for: request)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw Failure.unavailable }
        var data = Data()
        for try await byte in bytes {
            try Task.checkCancellation()
            guard data.count < discoveryBytes else { throw Failure.limit }
            data.append(byte)
        }
        return data
    }

    /// Unreserved characters stay; every other byte is percent-encoded, so
    /// the relay's path rule needs no other character.
    static func percentEncoded(_ name: String) -> String {
        name.utf8.map { byte -> String in
            switch byte {
            case 0x30...0x39, 0x41...0x5A, 0x61...0x7A, 0x2D, 0x2E, 0x5F, 0x7E: return String(UnicodeScalar(byte))
            default: return String(format: "%%%02X", byte)
            }
        }.joined()
    }

    /// The name must be an `id` or an alias of a listed model.
    static func listed(_ name: String, data: Data) throws {
        struct Entry: Decodable { let id: String; let aliases: [String]? }
        struct List: Decodable { let data: [Entry] }
        guard data.count <= discoveryBytes, let list = try? JSONDecoder().decode(List.self, from: data)
        else { throw Failure.invalidResponse }
        guard list.data.contains(where: { $0.id == name || ($0.aliases ?? []).contains(name) })
        else { throw Failure.unavailable }
    }

    /// The window is `default_generation_settings.n_ctx` (P3); the training
    /// length in `/v1/models` is never used. Tools need
    /// `chat_template_caps.supports_tool_calls`; a template naming
    /// `enable_thinking` thinks (LCD4).
    static func checked(_ settings: Settings, props data: Data) throws -> Self {
        struct Generation: Decodable { let n_ctx: Double? }
        struct Caps: Decodable { let supports_tool_calls: Bool?; let supports_reasoning_effort: Bool? }
        struct Props: Decodable {
            let default_generation_settings: Generation?
            let chat_template_caps: Caps?
            let chat_template: String?
        }
        guard data.count <= discoveryBytes, let props = try? JSONDecoder().decode(Props.self, from: data)
        else { throw Failure.invalidResponse }
        guard let window = props.default_generation_settings?.n_ctx, window.isFinite,
            window.rounded() == window, window > 512, window <= Double(UInt32.max) else { throw Failure.unavailable }
        let thinks = props.chat_template?.contains("enable_thinking") == true
        let effort = props.chat_template_caps?.supports_reasoning_effort == true
        var controls: ThinkingControls = thinks ? [.off, .on] : []
        if thinks && effort { controls.formUnion([.low, .medium, .high]) }
        return Self(settings: settings, supportsTools: props.chat_template_caps?.supports_tool_calls == true,
            contextTokens: UInt32(window), thinkingControls: controls, reasoningEffort: thinks && effort)
    }

    // MARK: Requests

    struct Message: Encodable, Equatable {
        var role: String
        var content: String
        var tool_calls: [Call]? = nil
        var tool_call_id: String? = nil
        struct Call: Encodable, Equatable {
            var id: String
            var type = "function"
            var function: Function
            struct Function: Encodable, Equatable { var name: String; var arguments: String }
        }
    }
    struct ToolSpec: Encodable {
        var type = "function"
        var function: Function
        struct Function: Encodable { var name: String; var description: String; var parameters: OllamaLanguageModel.JSON }
    }
    struct Request: Encodable {
        var model: String
        var messages: [Message]
        var tools: [ToolSpec]?
        var stream = true
        var stream_options = StreamOptions()
        var chat_template_kwargs: TemplateArguments?
        var reasoning_effort: String?
        struct StreamOptions: Encodable { var include_usage = true }
        struct TemplateArguments: Encodable { var enable_thinking: Bool }
    }

    /// Positional call IDs over the whole message list, `call00001` first
    /// (LCD3): stable across requests, so the prefix cache holds, and nine
    /// letters and digits, as Mistral templates require.
    static func messages(_ transcript: Transcript) throws -> [Message] {
        func text(_ segments: [Transcript.Segment]) throws -> String {
            try segments.map { segment in
                switch segment {
                case .text(let text): return text.content
                case .structure(let content): return content.content.jsonString
                default: throw Failure.unsupported
                }
            }.joined()
        }
        var ids: [String: String] = [:]
        var count = 0
        return try transcript.compactMap { entry in
            switch entry {
            case .reasoning: return nil
            case .instructions(let v): return Message(role: "system", content: try text(v.segments))
            case .prompt(let v): return Message(role: "user", content: try text(v.segments))
            case .response(let v): return Message(role: "assistant", content: try text(v.segments))
            case .toolCalls(let calls):
                return Message(role: "assistant", content: "", tool_calls: calls.map { call in
                    count += 1
                    let id = String(format: "call%05d", count)
                    ids[call.id] = id
                    return .init(id: id, function: .init(name: call.toolName, arguments: call.arguments.jsonString))
                })
            case .toolOutput(let output):
                guard let id = ids[output.id] else { throw Failure.unsupported }
                return Message(role: "tool", content: try text(output.segments), tool_call_id: id)
            default: throw Failure.unsupported
            }
        }
    }

    /// Off sends `enable_thinking: false`; on sends `true`; a level sends
    /// `true` and, to a template that takes it, `reasoning_effort` (LCD4).
    static func thinking(_ level: ContextOptions.ReasoningLevel?, effort: Bool)
        -> (Request.TemplateArguments?, String?) {
        switch level {
        case nil: return (nil, nil)
        case .custom(let value)?: return (.init(enable_thinking: value != "no_think"), nil)
        case .light?: return (.init(enable_thinking: true), effort ? "low" : nil)
        case .moderate?: return (.init(enable_thinking: true), effort ? "medium" : nil)
        case .deep?: return (.init(enable_thinking: true), effort ? "high" : nil)
        @unknown default: return (nil, nil)
        }
    }

    /// A refusal's body: `exceed_context_size_error` is a full window (P9);
    /// everything else is the provider's failure.
    static func refusal(_ body: Data) -> any Error {
        struct Body: Decodable { struct Inner: Decodable { let type: String? }; let error: Inner? }
        if let value = try? JSONDecoder().decode(Body.self, from: body),
            value.error?.type == "exceed_context_size_error" { return BackendFailure(.contextLimit) }
        return Failure.unavailable
    }

    // MARK: Stream

    struct Chunk: Decodable {
        var choices: [Choice]?
        var usage: Usage?
        var error: OllamaLanguageModel.JSON?
        struct Choice: Decodable { var index: Int?; var delta: Delta?; var finish_reason: String? }
        struct Delta: Decodable {
            var role: String?
            var content: String?
            var reasoning_content: String?
            var reasoning: String?
            var tool_calls: [CallDelta]?
        }
        struct CallDelta: Decodable { var index: Int; var id: String?; var function: FunctionDelta? }
        struct FunctionDelta: Decodable { var name: String?; var arguments: String? }
        struct Usage: Decodable {
            var prompt_tokens: Int?
            var completion_tokens: Int?
            var prompt_tokens_details: Details?
            struct Details: Decodable { var cached_tokens: Int? }
        }
    }
    /// What one SSE line produced.
    enum Event: Equatable {
        case text(String), thinking(String), usage(input: Int?, output: Int?, cached: Int?), finished(String), done
    }
    struct Call: Equatable { var name: String; var arguments: String }

    /// One request's stream. Lines are bounded before decoding; call
    /// fragments are gathered by index until the choice finishes.
    struct StreamState {
        var contextTokens = 0
        var line = Data()
        var calls: [Int: Call] = [:]
        var finish: String?
        var done = false
        mutating func consume(_ byte: UInt8) throws -> [Event] {
            // After `[DONE]` only line ends may follow.
            guard !done else {
                if byte == 10 || byte == 13 { return [] }
                throw Failure.invalidResponse
            }
            if byte != 10 {
                guard line.count < lineBytes else { throw Failure.limit }
                line.append(byte); return []
            }
            defer { line.removeAll(keepingCapacity: true) }
            return try record()
        }
        mutating func record() throws -> [Event] {
            var text = String(decoding: line, as: UTF8.self)
            if text.hasSuffix("\r") { text.removeLast() }
            if text.isEmpty || text.hasPrefix(":") { return [] }
            if text.hasPrefix("error:") { throw Failure.unavailable }
            guard text.hasPrefix("data:") else { return [] }
            var payload = text.dropFirst(5)
            if payload.first == " " { payload = payload.dropFirst() }
            if payload == "[DONE]" {
                guard finish != nil else { throw Failure.incomplete }
                done = true
                return [.done]
            }
            guard let chunk = try? JSONDecoder().decode(Chunk.self, from: Data(payload.utf8)) else {
                throw Failure.invalidResponse
            }
            guard chunk.error == nil else { throw Failure.unavailable }
            var events: [Event] = []
            let choices = chunk.choices ?? []
            guard choices.count <= 1 else { throw Failure.invalidResponse }
            if let choice = choices.first {
                guard (choice.index ?? 0) == 0, finish == nil else { throw Failure.invalidResponse }
                if let delta = choice.delta {
                    guard delta.role.map({ $0 == "assistant" }) ?? true else { throw Failure.invalidResponse }
                    if let thought = delta.reasoning_content ?? delta.reasoning, !thought.isEmpty {
                        events.append(.thinking(thought))
                    }
                    if let content = delta.content, !content.isEmpty { events.append(.text(content)) }
                    for fragment in delta.tool_calls ?? [] {
                        guard (0..<Int(Limits.toolProposals)).contains(fragment.index) else { throw Failure.limit }
                        var call = calls[fragment.index] ?? Call(name: "", arguments: "")
                        if let name = fragment.function?.name, !name.isEmpty {
                            guard call.name.isEmpty || call.name == name else { throw Failure.invalidResponse }
                            call.name = name
                        }
                        call.arguments += fragment.function?.arguments ?? ""
                        guard call.arguments.utf8.count <= argumentBytes else { throw Failure.limit }
                        calls[fragment.index] = call
                    }
                }
                if let reason = choice.finish_reason {
                    finish = reason
                    events.append(.finished(reason))
                }
            }
            if let usage = chunk.usage {
                guard usage.prompt_tokens.map({ $0 >= 0 }) ?? true,
                    usage.completion_tokens.map({ (0...contextTokens).contains($0) }) ?? true,
                    usage.prompt_tokens_details?.cached_tokens.map({ $0 >= 0 }) ?? true
                else { throw Failure.invalidResponse }
                events.append(.usage(input: usage.prompt_tokens, output: usage.completion_tokens,
                    cached: usage.prompt_tokens_details?.cached_tokens))
            }
            return events
        }
        /// The gathered calls in index order; each needs a name.
        func orderedCalls() throws -> [Call] {
            let ordered = calls.sorted { $0.key < $1.key }.map(\.value)
            guard ordered.allSatisfy({ !$0.name.isEmpty }) else { throw Failure.invalidResponse }
            return ordered
        }
        /// EOF: a last line without its newline is read; the caller then
        /// requires `[DONE]` after a finish.
        mutating func flush() throws -> [Event] {
            guard !done, !line.isEmpty else { return [] }
            defer { line.removeAll() }
            return try record()
        }
    }

    /// The outcome after `[DONE]` (LC-F1): `stop` and `tool_calls` settle;
    /// `length` is a full window; anything else is a protocol fault.
    static func outcome(_ finish: String?) throws {
        switch finish {
        case "stop", "tool_calls": return
        case "length": throw BackendFailure(.contextLimit)
        default: throw BackendFailure(.protocolFault)
        }
    }

    public struct Executor: LanguageModelExecutor {
        public typealias Configuration = Settings
        public typealias Model = LlamaCppLanguageModel
        private let settings: Settings
        public init(configuration: Settings) throws { settings = configuration }
        nonisolated(nonsending) public func respond(
            to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel
        ) async throws {
            guard settings == model.settings, request.schema == nil,
                request.enabledToolDefinitions.isEmpty || model.supportsTools else { throw Failure.unsupported }
            let tools = try request.enabledToolDefinitions.map { tool in
                ToolSpec(function: .init(name: tool.name, description: tool.description,
                    parameters: try JSONDecoder().decode(OllamaLanguageModel.JSON.self,
                        from: JSONEncoder().encode(tool.parameters))))
            }
            let registered = Set(request.enabledToolDefinitions.map(\.name))
            let (template, effort) = model.supportsReasoning
                ? LlamaCppLanguageModel.thinking(request.contextOptions.reasoningLevel, effort: model.reasoningEffort)
                : (nil, nil)
            let body = try JSONEncoder().encode(Request(model: settings.name,
                messages: try messages(request.transcript), tools: tools.isEmpty ? nil : tools,
                chat_template_kwargs: template, reasoning_effort: effort))
            let session = OllamaLanguageModel.session()
            defer { session.invalidateAndCancel() }
            try Task.checkCancellation()
            await model.inputObserver?.begin(request.id, capacity: model.contextTokens)
            try await withTaskCancellationHandler {
                var http = settings.request("/v1/chat/completions")
                http.httpMethod = "POST"; http.httpBody = body
                http.setValue("application/json", forHTTPHeaderField: "Content-Type")
                let (bytes, response) = try await session.bytes(for: http)
                let status = (response as? HTTPURLResponse)?.statusCode ?? 0
                guard status == 200 else {
                    var error = Data()
                    for try await byte in bytes {
                        try Task.checkCancellation()
                        guard error.count < errorBytes else { break }
                        error.append(byte)
                    }
                    throw status == 400 ? refusal(error) : Failure.unavailable
                }
                var state = StreamState(contextTokens: Int(model.contextTokens))
                var usage: (input: Int?, output: Int?, cached: Int?)?
                func handle(_ events: [Event]) async throws {
                    for event in events {
                        try Task.checkCancellation()
                        switch event {
                        case .thinking(let text):
                            if model.supportsReasoning {
                                await channel.send(.reasoning(action: .appendText(text, tokenCount: 0)))
                            }
                        case .text(let text):
                            await channel.send(.response(action: .appendText(text, tokenCount: 0)))
                        case .finished(let reason):
                            guard reason == "stop" || reason == "tool_calls" else { continue }
                            var ordinal = 0
                            for call in try state.orderedCalls() {
                                guard model.supportsTools else { throw Failure.unsupported }
                                guard registered.contains(call.name) else {
                                    if let report = model.unregisteredToolHandler {
                                        await report(ToolProposal(name: call.name, arguments: call.arguments,
                                            source: .providerJson))
                                    }
                                    throw BackendFailure(.protocolFault)
                                }
                                ordinal += 1
                                await channel.send(.toolCalls(action: .toolCall(
                                    id: "\(request.id.uuidString.lowercased())-\(ordinal)", name: call.name,
                                    action: .appendArguments(call.arguments, tokenCount: 0))))
                            }
                        case .usage(let input, let output, let cached):
                            usage = (input, output, cached)
                        case .done:
                            if let usage {
                                try await model.usageObserver?.measure(request.id, input: usage.input, output: usage.output)
                                if let input = usage.input, let output = usage.output {
                                    await channel.send(.response(action: .updateUsage(
                                        input: .init(totalTokenCount: input, cachedTokenCount: min(usage.cached ?? 0, input)),
                                        output: .init(totalTokenCount: output, reasoningTokenCount: 0))))
                                }
                                if state.finish == "stop" || state.finish == "tool_calls" || state.finish == "length" {
                                    try await model.inputObserver?.complete(request.id, inputTokens: usage.input,
                                        capacity: model.contextTokens)
                                }
                            }
                        }
                    }
                }
                do {
                    for try await byte in bytes {
                        try Task.checkCancellation()
                        try await handle(try state.consume(byte))
                        if state.done { break }
                    }
                    if !state.done { try await handle(try state.flush()) }
                    guard state.done else { throw Failure.incomplete }
                } catch Failure.invalidResponse { throw BackendFailure(.protocolFault) }
                try outcome(state.finish)
            } onCancel: { session.invalidateAndCancel() }
        }
    }
}

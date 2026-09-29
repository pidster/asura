import Foundation
import FoundationModels

/// Provider translation only. The service owns admission, tools and the outer deadline.
public struct OllamaLanguageModel: LanguageModel, Sendable {
    public enum Failure: Error, Equatable, Sendable {
        case invalidConfiguration, unsupported, unavailable, invalidResponse, limit, incomplete
    }
    public struct Settings: Hashable, Sendable {
        public let endpoint: URL
        public let name: String
        public init(name: String, endpoint: URL = URL(string: "http://127.0.0.1:11434")!) throws {
            guard !name.isEmpty, name.utf8.count <= 1024,
                !name.unicodeScalars.contains(where: { CharacterSet.whitespacesAndNewlines.contains($0) || CharacterSet.controlCharacters.contains($0) }),
                let url = URLComponents(url: endpoint, resolvingAgainstBaseURL: false),
                endpoint.absoluteString.utf8.count <= 2048,
                let host = url.host, !host.isEmpty,
                (url.scheme == "https" ||
                    (url.scheme == "http" && ["127.0.0.1", "[::1]", "::1"].contains(host))),
                url.user == nil, url.password == nil, url.query == nil, url.fragment == nil,
                url.path.isEmpty || url.path == "/", url.port.map({ (1...65535).contains($0) }) ?? true
            else { throw Failure.invalidConfiguration }
            self.name = name; self.endpoint = endpoint
        }
    }
    public let settings: Settings
    public let supportsTools: Bool
    public let contextTokens: UInt32
    public var localToolModel: String? = nil
    public var supportsReasoning = false
    public var capabilities: LanguageModelCapabilities {
        .init((supportsTools ? [.toolCalling] : []) + (supportsReasoning ? [.reasoning] : []))
    }
    public var executorConfiguration: Settings { settings }

    /// Only a checked runtime response can create a model; no implicit network discovery.
    public static func discover(_ settings: Settings) async throws -> Self {
        let session = session(seconds: 5)
        defer { session.invalidateAndCancel() }
        return try await withTaskCancellationHandler {
            var request = URLRequest(url: settings.endpoint.appending(path: "api/show"))
            request.httpMethod = "POST"
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(["model": settings.name])
            let (bytes, response) = try await session.bytes(for: request)
            guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw Failure.unavailable }
            var data = Data()
            for try await byte in bytes {
                try Task.checkCancellation()
                guard data.count < 65_536 else { throw Failure.limit }
                data.append(byte)
            }
            var model = try checked(settings, data: data)
            if model.supportsTools, let local = localReference(settings.name),
                ["127.0.0.1", "::1", "[::1]"].contains(settings.endpoint.host ?? "") {
                // Locality uncertainty disables tools only; ordinary discovery errors
                // above still fail. The service outer Hello deadline bounds all probes.
                do {
                    let version = try await metadata(session, request: URLRequest(url: settings.endpoint.appending(path: "api/version")), limit: 1024)
                    struct Version: Decodable { let version: String }
                    let value = try JSONDecoder().decode(Version.self, from: version)
                    if qualifiedLocalRoutingVersion(value.version) {
                        var localRequest = request
                        localRequest.httpBody = try JSONEncoder().encode(["model": local])
                        let localData = try await metadata(session, request: localRequest, limit: 65_536)
                        if localMetadata(data), localMetadata(localData) {
                            model.localToolModel = local
                        }
                    }
                } catch is CancellationError { throw CancellationError() }
                catch { try Task.checkCancellation() }
            }
            return model
        } onCancel: { session.invalidateAndCancel() }
    }

    static func metadata(_ session: URLSession, request: URLRequest, limit: Int) async throws -> Data {
        let (bytes, response) = try await session.bytes(for: request)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw Failure.unavailable }
        var data = Data()
        for try await byte in bytes {
            try Task.checkCancellation()
            guard data.count < limit else { throw Failure.limit }
            data.append(byte)
        }
        return data
    }

    static func qualifiedLocalRoutingVersion(_ version: String) -> Bool {
        let parts = version.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "0", parts[1] == "34",
            let patch = UInt32(parts[2]) else { return false }
        return patch >= 4
    }

    static func localReference(_ name: String) -> String? {
        let lower = name.lowercased()
        guard !lower.hasSuffix(":cloud"), !lower.hasSuffix("-cloud") else { return nil }
        let base = lower.hasSuffix(":local") ? String(name.dropLast(6)) : name
        guard !base.isEmpty, !base.lowercased().hasSuffix(":local"),
            !base.lowercased().hasSuffix(":cloud"), !base.lowercased().hasSuffix("-cloud") else { return nil }
        let leaf = base.split(separator: "/").last ?? ""
        // Explicit tag plus source suffix cannot be interpreted as an ordinary
        // single-tag model reference by older daemons.
        return (leaf.contains(":") ? base : base + ":latest") + ":local"
    }

    static func localMetadata(_ data: Data) -> Bool {
        struct Location: Decodable { let remote_host: String?; let remote_model: String? }
        guard let value = try? JSONDecoder().decode(Location.self, from: data) else { return false }
        return (value.remote_host ?? "").isEmpty && (value.remote_model ?? "").isEmpty
    }

    static func checked(_ settings: Settings, data: Data) throws -> Self {
        struct Shown: Decodable { let capabilities: [String]; let model_info: [String: JSON]? }
        guard data.count <= 65_536 else { throw Failure.limit }
        let shown = try JSONDecoder().decode(Shown.self, from: data)
        guard shown.capabilities.contains("completion") else { throw Failure.unsupported }
        let capacities = (shown.model_info ?? [:]).compactMap { key, value -> UInt32? in
            guard key.hasSuffix(".context_length"), case .number(let number) = value,
                number.isFinite, number.rounded() == number, number > 512,
                number <= Double(UInt32.max) else { return nil }
            return UInt32(number)
        }
        guard let capacity = capacities.min() else { throw Failure.unavailable }
        return Self(settings: settings, supportsTools: shown.capabilities.contains("tools"),
            contextTokens: min(capacity, 8192), supportsReasoning: shown.capabilities.contains("thinking"))
    }

    private final class RedirectGuard: NSObject, URLSessionTaskDelegate, Sendable {
        func urlSession(_ session: URLSession, task: URLSessionTask,
                        willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
                        completionHandler: @escaping @Sendable (URLRequest?) -> Void) { completionHandler(nil) }
    }
    static func session(seconds: TimeInterval) -> URLSession {
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = seconds
        config.timeoutIntervalForResource = seconds
        config.httpShouldSetCookies = false
        config.httpCookieStorage = nil
        config.urlCredentialStorage = nil
        config.urlCache = nil
        config.connectionProxyDictionary = [:]
        config.httpMaximumConnectionsPerHost = 1
        return URLSession(configuration: config, delegate: RedirectGuard(), delegateQueue: nil)
    }

    enum JSON: Codable, Sendable, Equatable {
        case object([String: JSON]), array([JSON]), string(String), number(Double), bool(Bool), null
        init(from decoder: any Decoder) throws {
            let value = try decoder.singleValueContainer()
            if value.decodeNil() { self = .null }
            else if let v = try? value.decode(Bool.self) { self = .bool(v) }
            else if let v = try? value.decode(String.self) { self = .string(v) }
            else if let v = try? value.decode(Double.self) { self = .number(v) }
            else if let v = try? value.decode([JSON].self) { self = .array(v) }
            else { self = .object(try value.decode([String: JSON].self)) }
        }
        func encode(to encoder: any Encoder) throws {
            var value = encoder.singleValueContainer()
            switch self {
            case .object(let v): try value.encode(v)
            case .array(let v): try value.encode(v)
            case .string(let v): try value.encode(v)
            case .number(let v): try value.encode(v)
            case .bool(let v): try value.encode(v)
            case .null: try value.encodeNil()
            }
        }
    }
    struct Message: Codable, Sendable, Equatable {
        var role: String
        var content: String
        var tool_calls: [Call]?
        var tool_name: String?
        struct Call: Codable, Sendable, Equatable {
            var function: Function
            struct Function: Codable, Sendable, Equatable { var name: String; var arguments: JSON }
        }
    }
    struct ToolSpec: Encodable {
        var type = "function"
        var function: Function
        struct Function: Encodable { var name: String; var description: String; var parameters: JSON }
    }
    struct Request: Encodable {
        var model: String
        var messages: [Message]
        var tools: [ToolSpec]?
        var stream = true
        var truncate = false
        var shift = false
        var options: Options
        struct Options: Encodable { var num_ctx: UInt32; var num_predict: Int }
    }
    struct Chunk: Decodable {
        var message: Message?
        var done: Bool?
        var done_reason: String?
        var error: String?
        var prompt_eval_count: Int?
        var eval_count: Int?
    }

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
        return try transcript.map { entry in
            switch entry {
            case .instructions(let v): return Message(role: "system", content: try text(v.segments))
            case .prompt(let v): return Message(role: "user", content: try text(v.segments))
            case .response(let v): return Message(role: "assistant", content: try text(v.segments))
            case .toolCalls(let calls):
                return Message(role: "assistant", content: "", tool_calls: try calls.map { call in
                    .init(function: .init(name: call.toolName,
                        arguments: try JSONDecoder().decode(JSON.self, from: Data(call.arguments.jsonString.utf8))))
                })
            case .toolOutput(let output):
                return Message(role: "tool", content: try text(output.segments), tool_name: output.toolName)
            default: throw Failure.unsupported
            }
        }
    }

    /// Record accumulation is bounded before JSON decoding, including missing-newline responses.
    struct StreamState {
        var maximumTokens = 2048
        var line = Data()
        var total = 0
        var textBytes = 0
        var calls = 0
        var done = false
        mutating func consume(_ byte: UInt8) throws -> Chunk? {
            guard !done, total < 4 * 1024 * 1024 else { throw Failure.limit }
            total += 1
            if byte != 10 {
                guard line.count < 65_536 else { throw Failure.limit }
                line.append(byte); return nil
            }
            if line.isEmpty { return nil }
            return try record()
        }
        mutating func record() throws -> Chunk {
            let value = try JSONDecoder().decode(Chunk.self, from: line)
            line.removeAll(keepingCapacity: true)
            guard value.error == nil else { throw Failure.unavailable }
            guard value.prompt_eval_count.map({ $0 >= 0 }) ?? true,
                  value.eval_count.map({ (0...maximumTokens).contains($0) }) ?? true else { throw Failure.invalidResponse }
            if let message = value.message {
                guard message.role == "assistant" else { throw Failure.invalidResponse }
                textBytes += message.content.utf8.count
                calls += message.tool_calls?.count ?? 0
                guard textBytes <= Limits.output, calls <= 8 else { throw Failure.limit }
                for call in message.tool_calls ?? [] {
                    guard !call.function.name.isEmpty, call.function.name.utf8.count <= 128,
                          case .object = call.function.arguments,
                          try JSONEncoder().encode(call.function.arguments).count <= 16_384
                    else { throw Failure.invalidResponse }
                }
            }
            done = value.done == true
            return value
        }
        mutating func finish() throws -> Chunk? {
            let final = line.isEmpty ? nil : try record()
            guard done else { throw Failure.incomplete }
            return final
        }
    }

    /// Text is delivered before finish classification, including a length-limited
    /// final record. Unknown reasons cannot manufacture successful completion.
    static func deliver(_ chunk: Chunk, maximumTokens: Int,
        emit: (Chunk) async throws -> Void) async throws {
        try await emit(chunk)
        guard chunk.done == true else { return }
        switch chunk.done_reason {
        case "stop": return
        case "length": throw BackendFailure(.outputLimit)
        case nil, "":
            if let count = chunk.eval_count, count >= maximumTokens {
                throw BackendFailure(.outputLimit)
            }
            throw Failure.invalidResponse
        default: throw Failure.invalidResponse
        }
    }

    static func deliverToExecutor(_ chunk: Chunk, maximumTokens: Int,
        emit: (Chunk) async throws -> Void) async throws {
        do { try await deliver(chunk, maximumTokens: maximumTokens, emit: emit) }
        catch Failure.invalidResponse { throw BackendFailure(.protocolFault) }
    }

    public struct Executor: LanguageModelExecutor {
        public typealias Configuration = Settings
        public typealias Model = OllamaLanguageModel
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
                    parameters: try JSONDecoder().decode(JSON.self, from: JSONEncoder().encode(tool.parameters))))
            }
            guard let maximumTokens = request.generationOptions.maximumResponseTokens,
                  (1...2048).contains(maximumTokens) else { throw Failure.limit }
            let hasTools = !request.enabledToolDefinitions.isEmpty
            guard !hasTools || model.localToolModel != nil else { throw Failure.unsupported }
            let body = try JSONEncoder().encode(Request(model: model.localToolModel ?? settings.name,
                messages: try messages(request.transcript), tools: tools.isEmpty ? nil : tools,
                options: .init(num_ctx: model.contextTokens, num_predict: maximumTokens)))
            guard body.count <= 262_144 else { throw Failure.limit }
            let session = session(seconds: 60)
            defer { session.invalidateAndCancel() }
            try await withTaskCancellationHandler {
                var http = URLRequest(url: settings.endpoint.appending(path: "api/chat"))
                http.httpMethod = "POST"; http.httpBody = body
                http.setValue("application/json", forHTTPHeaderField: "Content-Type")
                let (bytes, response) = try await session.bytes(for: http)
                guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw Failure.unavailable }
                var state = StreamState(maximumTokens: maximumTokens)
                var ordinal = 0
                func emit(_ chunk: Chunk) async throws {
                    try Task.checkCancellation()
                    if let message = chunk.message {
                        if !message.content.isEmpty {
                            await channel.send(.response(action: .appendText(message.content, tokenCount: 0)))
                        }
                        for call in message.tool_calls ?? [] {
                            guard model.supportsTools else { throw Failure.unsupported }
                            ordinal += 1
                            let arguments = try JSONEncoder().encode(call.function.arguments)
                            await channel.send(.toolCalls(action: .toolCall(
                                id: "\(request.id.uuidString.lowercased())-\(ordinal)", name: call.function.name,
                                action: .appendArguments(String(decoding: arguments, as: UTF8.self), tokenCount: 0))))
                        }
                    }
                    if chunk.done == true, let input = chunk.prompt_eval_count, let output = chunk.eval_count {
                        await channel.send(.response(action: .updateUsage(
                            input: .init(totalTokenCount: input, cachedTokenCount: 0),
                            output: .init(totalTokenCount: output, reasoningTokenCount: 0))))
                    }
                }
                for try await byte in bytes {
                    try Task.checkCancellation()
                    if let chunk = try state.consume(byte) { try await deliverToExecutor(chunk, maximumTokens: maximumTokens, emit: emit) }
                    if state.done { break }
                }
                if let final = try state.finish() { try await deliverToExecutor(final, maximumTokens: maximumTokens, emit: emit) }
            } onCancel: { session.invalidateAndCancel() }
        }
    }
}

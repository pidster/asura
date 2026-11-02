import Foundation
import Darwin
import FoundationModels


struct UnexpectedBackendCancellation: Error { let taskCancelled: Bool }

/// The bridged domain of the system model's private `GenerativeError`.
let generativeErrorDomain = "com.apple.GenerativeFunctionsFoundation.GenerativeError"
/// The bridged code of its prompt `tooManyTokens` case (measured on macOS 27,
/// Xcode 27.1, build 26A428; context-composer.md, CC4 prerequisites).
let generativeTooManyTokensCode = 4_050_000

/// True when `error` means the context window is full. The SDK reports a full
/// window as `LanguageModelError.contextSizeExceeded` when its own admission
/// check sees the input. Inside a turn, for example when the transcript holds
/// tool calls and tool outputs, the system model reports it as its private
/// `GenerativeError` with the prompt case `tooManyTokens`. Both are
/// `context_limit` (loop-closure-probes.md, Q6). The private error is matched
/// by its bridged domain and its code or case label, never by its text.
func isContextOverflow(_ error: any Error) -> Bool {
    if let sdk = error as? LanguageModelError, case .contextSizeExceeded = sdk { return true }
    let bridged = error as NSError
    guard bridged.domain == generativeErrorDomain else { return false }
    return bridged.code == generativeTooManyTokensCode || reflectsCase(error, label: "tooManyTokens", depth: 4)
}

/// A bounded walk over an error's reflected children for one case label.
private func reflectsCase(_ value: Any, label: String, depth: Int) -> Bool {
    guard depth > 0 else { return false }
    for child in Mirror(reflecting: value).children.prefix(16) {
        if child.label == label { return true }
        if reflectsCase(child.value, label: label, depth: depth - 1) { return true }
    }
    return false
}

/// Fixed classifications only; provider descriptions may contain private inputs.
func modelFailureDiagnostic(_ error: any Error) -> String {
    let kind: String
    if !(error is LanguageModelError), isContextOverflow(error) {
        kind = "context_limit"
    } else if let sdk = error as? LanguageModelError {
        switch sdk {
        case .contextSizeExceeded: kind = "context_limit"
        case .rateLimited: kind = "rate_limited"
        case .guardrailViolation: kind = "guardrail"
        case .refusal: kind = "refusal"
        case .unsupportedCapability: kind = "unsupported_capability"
        case .unsupportedTranscriptContent: kind = "unsupported_transcript"
        case .unsupportedGenerationGuide: kind = "unsupported_guide"
        case .unsupportedLanguageOrLocale: kind = "unsupported_language"
        case .timeout: kind = "timeout"
        @unknown default: kind = "unknown_sdk"
        }
    } else if let parsing = error as? GeneratedContent.ParsingError {
        let bytes = Array(parsing.rawContent.utf8.prefix(16_385))
        let size = bytes.count
        let shape: String
        if size == 0 { shape = "empty" }
        else if size > 16_384 { shape = "oversized" }
        else if let value = try? JSONSerialization.jsonObject(with: Data(bytes), options: [.fragmentsAllowed]) {
            if value is [String: Any] { shape = "object" }
            else if value is [Any] { shape = "array" }
            else { shape = "scalar" }
        } else { shape = "invalid_json" }
        return "asura_model_failure generated_content_\(shape) \(min(size, 16_385))\n"
    }
    else if error is DecodingError { kind = "decoding" }
    else if let callback = error as? LanguageModelSession.ToolCallError {
        let code: Int
        switch callback.underlyingError {
        case let backend as BackendFailure: code = 100 + backend.reason.rawValue
        case let helper as HelperError:
            switch helper {
            case .protocolFault: code = 201
            case .limit: code = 202
            case .closed: code = 203
            case .unavailable: code = 204
            case .contextLimit: code = 205
            case .timeout: code = 206
            }
        case is DecodingError: code = 300
        case is CancellationError: code = 400
        default: code = 0
        }
        return "asura_model_failure tool_callback \(code)\n"
    }
    else if error is LanguageModelSession.Error { kind = "session_state" }
    else if let helper = error as? HelperError {
        switch helper {
        case .protocolFault: kind = "helper_protocol"
        case .limit: kind = "helper_limit"
        case .closed: kind = "helper_closed"
        case .unavailable: kind = "helper_unavailable"
        case .contextLimit: kind = "helper_context"
        case .timeout: kind = "helper_timeout"
        }
    } else if let unexpected = error as? UnexpectedBackendCancellation {
        kind = unexpected.taskCancelled ? "task_cancelled" : "provider_cancelled"
    } else if error is CancellationError { kind = "cancelled" }
    else if error is BackendFailure { kind = "backend_failure" }
    else { kind = "unclassified" }
    return "asura_model_failure \(kind) \((error as NSError).code)\n"
}
func reportModelFailure(_ error: any Error) {
    let bytes = Array(modelFailureDiagnostic(error).utf8)
    guard bytes.count < 256 else { return }
    let flags = fcntl(STDERR_FILENO, F_GETFL)
    guard flags >= 0, fcntl(STDERR_FILENO, F_SETFL, flags | O_NONBLOCK) == 0 else { return }
    // A single nonblocking pipe write; diagnostics never hold up cancellation.
    _ = bytes.withUnsafeBytes { Darwin.write(STDERR_FILENO, $0.baseAddress, $0.count) }
}

public enum ContextCapacitySource: UInt32, Sendable {
    case system = 1, coreai = 2, mlx = 3, ollama = 4, llamacpp = 5
}

public struct BackendStatus: Sendable {
    public let contextTokens: UInt32?
    public let reportedContextTokens: UInt32?
    public let contextSource: ContextCapacitySource?
    public let modelName: String?
    public let supportsTools: Bool
    public let localToolDestination: Bool
    public let capabilityProfile: CapabilityProfile?
    /// Thinking forms the model accepts (Hello field 23, thinking-level.md).
    public let thinkingControls: ThinkingControls
    public init(contextTokens: UInt32?, reportedContextTokens: UInt32? = nil,
        contextSource: ContextCapacitySource? = nil, modelName: String? = nil,
        supportsTools: Bool = false, localToolDestination: Bool = false,
        capabilityProfile: CapabilityProfile? = nil, thinkingControls: ThinkingControls = []) {
        self.contextTokens = contextTokens; self.reportedContextTokens = reportedContextTokens
        self.contextSource = contextSource; self.modelName = modelName
        self.supportsTools = supportsTools; self.localToolDestination = localToolDestination
        self.capabilityProfile = capabilityProfile; self.thinkingControls = thinkingControls
    }
}
public struct Snapshot: Sendable {
    public let text: String
    /// The whole thinking text so far; nil leaves it unchanged (thinking-level.md).
    public let thinking: String?
    public let usageTokens: UInt64?
    public let inputContext: (tokens: UInt32, capacity: UInt32)?
    public let inferenceStartedOrdinal: UInt32?
    public let inferenceUsage: RequestUsage?
    public init(text: String, thinking: String? = nil, usageTokens: UInt64? = nil, inputContext: (tokens: UInt32, capacity: UInt32)? = nil,
        inferenceStartedOrdinal: UInt32? = nil, inferenceUsage: RequestUsage? = nil) {
        self.text = text; self.thinking = thinking; self.usageTokens = usageTokens; self.inputContext = inputContext
        self.inferenceStartedOrdinal = inferenceStartedOrdinal; self.inferenceUsage = inferenceUsage
    }
}
public struct BackendFailure: Error, Sendable {
    public let reason: Reason
    public init(_ reason: Reason) { self.reason = reason }
}
public protocol ModelBackend: Sendable {
    func status() async -> BackendStatus
    func generate(_ input: ModelInput, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws
    /// As `generate`, with the thinking form the service sent
    /// (thinking-level.md). Backends without thinking controls ignore it.
    func generate(_ input: ModelInput, thinking: ThinkingRequest,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws
}
extension ModelBackend {
    public func generate(_ input: ModelInput, thinking: ThinkingRequest,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await generate(input, snapshot: snapshot)
    }
}

/// System inference adapter. Tool callbacks proxy to the service; no host IO.
public struct SystemBackend: ToolModelBackend {
    public init() {}
    public func status() async -> BackendStatus {
        let model = SystemLanguageModel.default
        guard case .available = model.availability,
            let capacity = UInt32(exactly: model.contextSize), capacity > 512 else {
            return BackendStatus(contextTokens: nil)
        }
        return BackendStatus(contextTokens: capacity, reportedContextTokens: capacity,
            contextSource: .system, modelName: model.variant.displayName,
            supportsTools: model.capabilities.contains(.toolCalling),
            capabilityProfile: CapabilityProfile(native: model.capabilities, provenance: .framework),
            thinkingControls: Self.thinkingControls(model.capabilities))
    }
    /// The framework's own levels when the system model reasons; its off
    /// form is unverified, so off is not offered (thinking-level.md).
    static func thinkingControls(_ capabilities: LanguageModelCapabilities) -> ThinkingControls {
        capabilities.contains(.reasoning) ? [.low, .medium, .high] : []
    }

    public static func history(_ input: ModelInput, tools: [any Tool] = []) -> [Transcript.Entry] {
        func segment(_ text: String) -> [Transcript.Segment] { [.text(.init(content: text))] }
        // The instructions entry declares the tools, as the SDK's own session
        // writes it; the system model reads the declarations from it.
        var entries: [Transcript.Entry] = [
            .instructions(.init(segments: segment(input.instructions),
                toolDefinitions: tools.map { Transcript.ToolDefinition(tool: $0) }))
        ]
        for turn in input.history {
            if turn.role == .user {
                entries.append(.prompt(.init(segments: segment(turn.text))))
            } else {
                entries.append(.response(.init(segments: segment(turn.text))))
            }
        }
        return entries
    }
    /// The whole request as transcript entries (CC4): instructions with the
    /// declared tools, history, the prompt, then the continuation as tool
    /// calls and tool outputs. A call whose arguments are not JSON is a
    /// service fault.
    public static func transcript(_ input: ModelInput, tools: [any Tool]) throws -> [Transcript.Entry] {
        var entries = history(input, tools: tools)
        entries.append(.prompt(.init(segments: [.text(.init(content: input.prompt))])))
        for turn in input.continuation {
            switch turn.role {
            case .assistant:
                let calls = try turn.toolCalls.map { call -> Transcript.ToolCall in
                    guard let arguments = try? GeneratedContent(json: call.arguments) else {
                        throw BackendFailure(.protocolFault)
                    }
                    return Transcript.ToolCall(id: call.callID, toolName: call.name, arguments: arguments)
                }
                entries.append(.toolCalls(.init(calls)))
            case .tool:
                entries.append(.toolOutput(.init(id: turn.callID, toolName: turn.toolName,
                    segments: [.text(.init(content: turn.text))])))
            default:
                throw BackendFailure(.protocolFault)
            }
        }
        return entries
    }
    /// The calls of the last `toolCalls` entry of a preserved transcript, in
    /// response order; `nil` when there is none (an SDK regression, CC4).
    static func proposals(_ transcript: Transcript) -> [ToolProposal]? {
        for entry in transcript.reversed() {
            if case .toolCalls(let calls) = entry {
                let proposals = calls.map {
                    ToolProposal(name: $0.toolName, arguments: $0.arguments.jsonString, source: .generatedContent)
                }
                return proposals.isEmpty ? nil : proposals
            }
        }
        return nil
    }

    private func engine() throws -> FoundationBackend<SystemLanguageModel> {
        let model = SystemLanguageModel.default
        guard case .available = model.availability else { throw BackendFailure(.modelUnavailable) }
        return FoundationBackend(model: model, contextTokens: UInt32(exactly: model.contextSize),
            contextSource: .system,
            modelName: model.variant.displayName, supportsTools: true, reportsUsage: true,
            capabilityProfile: CapabilityProfile(native: model.capabilities, provenance: .framework),
            tokenCounter: { entries, tools in
                let transcript = try await model.tokenCount(for: entries)
                let schemas = try await model.tokenCount(for: tools)
                return transcript + schemas
            }, thinkingControls: Self.thinkingControls(model.capabilities))
    }
    public func generate(_ input: ModelInput, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await engine().generate(input, snapshot: snapshot)
    }
    public func generate(_ input: ModelInput, thinking: ThinkingRequest,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await engine().generate(input, thinking: thinking, snapshot: snapshot)
    }
    public func generateWithTools(_ input: ModelInput, thinking: ThinkingRequest, request: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws -> [ToolProposal] {
        try await engine().generateWithTools(input, thinking: thinking, request: request, snapshot: snapshot)
    }
}

public typealias NativeTokenCounter = @Sendable ([Transcript.Entry], [any Tool]) async throws -> Int

/// One SDK conversation engine shared by all admitted native/custom adapters.
public struct FoundationBackend<Model: LanguageModel>: ToolModelBackend {
    /// Builds the per-request model with its observers. The reporter takes a
    /// call the SDK cannot dispatch (TCR-D17); it is nil when tools are disabled.
    public typealias InstrumentationFactory = @Sendable (InferenceUsageObserver,
        @escaping @Sendable (Snapshot) async throws -> Void, UnregisteredToolReporter?) -> Model
    public let model: Model
    public let capabilityProfile: CapabilityProfile
    private let contextTokens: UInt32?
    private let reportedContextTokens: UInt32?
    private let contextSource: ContextCapacitySource?
    private let modelName: String
    private let supportsTools: Bool
    private let localToolDestination: Bool
    private let reportsUsage: Bool
    private let tokenCounter: NativeTokenCounter?
    private let instrumentationFactory: InstrumentationFactory?
    /// True for adapters that replace a nil response limit with their own small
    /// default (CoreAI, MLX); they receive the context window instead.
    private let responseTokensFromWindow: Bool
    /// Thinking forms the model accepts; only reasoning models report any.
    private let thinkingControls: ThinkingControls
    public init(model: Model, contextTokens: UInt32?,
        reportedContextTokens: UInt32? = nil, contextSource: ContextCapacitySource? = nil,
        modelName: String, supportsTools: Bool,
        reportsUsage: Bool = false, localToolDestination: Bool = false, capabilityProfile: CapabilityProfile? = nil, tokenCounter: NativeTokenCounter? = nil,
        instrumentationFactory: InstrumentationFactory? = nil, responseTokensFromWindow: Bool = false,
        thinkingControls: ThinkingControls = []) {
        self.model = model; self.contextTokens = contextTokens
        self.reportedContextTokens = reportedContextTokens ?? contextTokens
        self.contextSource = contextSource; self.modelName = modelName
        let profile = capabilityProfile ?? CapabilityProfile(native: model.capabilities, provenance: .runtime)
        self.capabilityProfile = profile
        self.supportsTools = supportsTools && model.capabilities.contains(.toolCalling) && profile.support(.toolCalling) == .supported
        self.localToolDestination = localToolDestination
        self.reportsUsage = reportsUsage; self.tokenCounter = tokenCounter
        self.instrumentationFactory = instrumentationFactory
        self.responseTokensFromWindow = responseTokensFromWindow
        // Controls need declared reasoning support.
        self.thinkingControls = profile.support(.reasoning) == .supported ? thinkingControls : []
    }
    public func status() async -> BackendStatus {
        BackendStatus(contextTokens: contextTokens, reportedContextTokens: reportedContextTokens,
            contextSource: contextSource, modelName: modelName, supportsTools: supportsTools,
            localToolDestination: localToolDestination, capabilityProfile: capabilityProfile,
            thinkingControls: thinkingControls)
    }
    public func generate(_ input: ModelInput, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        _ = try await generateSession(input, thinking: .none, request: 1, tools: false, snapshot: snapshot)
    }
    public func generate(_ input: ModelInput, thinking: ThinkingRequest,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        _ = try await generateSession(input, thinking: thinking, request: 1, tools: false, snapshot: snapshot)
    }
    public func generateWithTools(_ input: ModelInput, thinking: ThinkingRequest, request: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws -> [ToolProposal] {
        guard supportsTools else { throw BackendFailure(.modelUnavailable) }
        try capabilityProfile.require([.toolCalling])
        return try await generateSession(input, thinking: thinking, request: request, tools: true, snapshot: snapshot)
    }
    /// One request: one fresh session, one inference (CC4). The session holds
    /// only the instructions; the executor wrapper replaces the request
    /// transcript with the whole input. With tools, every project tool throws
    /// `StopForCaller`, so a response with calls ends the session; the calls
    /// are read from the preserved transcript and returned, never run.
    private func generateSession(_ input: ModelInput, thinking: ThinkingRequest, request: UInt32, tools enabled: Bool,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws -> [ToolProposal] {
        // The service sends only a reported form; anything else is a fault.
        guard thinkingControls.accepts(thinking) else { throw BackendFailure(.protocolFault) }
        try Task.checkCancellation()
        let tools = enabled ? makeProjectTools() : []
        let entries = try SystemBackend.transcript(input, tools: tools)
        let requestUsage = InferenceUsageObserver(request: request, snapshot: snapshot)
        let unregistered = ProposalLog()
        let reporter: UnregisteredToolReporter? = enabled ? { @Sendable proposal in await unregistered.append(proposal) } : nil
        let generationModel = instrumentationFactory?(requestUsage, snapshot, reporter) ?? model
        // No Asura output cap (owner, 2026-10-19). Adapters whose nil means a
        // small library default receive the window, which cannot bind first.
        let responseTokens = responseTokensFromWindow ? contextTokens.map { Int($0) } : nil
        let boundedModel = BoundedToolModel(base: generationModel, responseTokens: responseTokens,
            transcript: Transcript(entries: entries), usageObserver: requestUsage)
        do {
            if let tokenCounter {
                // The instructions entry declares the tools, so the entries hold
                // the schemas once; they are not counted again.
                let count = try await tokenCounter(entries, [])
                try Task.checkCancellation()
                guard let measured = UInt32(exactly: count) else { throw BackendFailure(.contextLimit) }
                if let capacity = contextTokens {
                    // The composer reserves answer room; here the exact count
                    // must leave room for at least one output token.
                    guard measured < capacity else { throw BackendFailure(.contextLimit) }
                    try await snapshot(Snapshot(text: "", inputContext: (measured, capacity)))
                }
            }
            let session = LanguageModelSession(model: boundedModel, tools: tools,
                transcript: Transcript(entries: Array(entries.prefix(1))))
            // A throwing stop tool must leave its toolCalls entry in place (CC4).
            session.transcriptErrorHandlingPolicy = .preserveTranscript
            // The SDK session owns inference; retain it through every async stream exit.
            defer { withExtendedLifetime(session) {} }
            // The thinking form the service sent; none leaves the model's default.
            let contextOptions = ContextOptions(reasoningLevel: ThinkingControls.reasoningLevel(thinking))
            let stream = session.streamResponse(to: input.prompt, options: GenerationOptions(maximumResponseTokens: responseTokens), contextOptions: contextOptions)
            do {
                for try await value in stream {
                    try Task.checkCancellation()
                    // A count above the counter validity bound stays unknown.
                    let total = reportsUsage ? UInt64(exactly: value.usage.output.totalTokenCount)
                        .flatMap { $0 <= Limits.countedTokens ? $0 : nil } : nil
                    let thought = thinkingText(value.transcriptEntries)
                    try await snapshot(Snapshot(text: value.content, thinking: thought.isEmpty ? nil : thought,
                        usageTokens: enabled ? nil : total))
                }
            } catch let call as LanguageModelSession.ToolCallError where call.underlyingError is StopForCaller {
                if let failure = await boundedModel.gate.recordedFailure() { throw failure }
                guard let proposals = SystemBackend.proposals(session.transcript) else {
                    reportStopToolTranscript()
                    throw BackendFailure(.internalError)
                }
                return proposals
            }
            if let failure = await boundedModel.gate.recordedFailure() { throw failure }
            return []
        } catch {
            try Task.checkCancellation()
            var failure: BackendFailure?
            if let recorded = await boundedModel.gate.recordedFailure() { failure = recorded }
            else if let typed = error as? BackendFailure { failure = typed }
            else if isContextOverflow(error) {
                // A full window inside a turn is a context limit (CC4 prerequisite).
                reportModelFailure(error)
                failure = BackendFailure(.contextLimit)
            }
            if let failure {
                // An unregistered call is sent for its record before the
                // request fails (TCR-D17, revised by CC4).
                let reported = await unregistered.all()
                if !reported.isEmpty { throw ProposedThenFailed(proposals: reported, failure: failure) }
                throw failure
            }
            guard let error = error as? LanguageModelError else { throw error }
            reportModelFailure(error)
            switch error {
            case .contextSizeExceeded: throw BackendFailure(.contextLimit)
            case .guardrailViolation, .refusal: throw BackendFailure(.refusal)
            case .timeout: throw BackendFailure(.timeout)
            default: throw BackendFailure(.internalError)
            }
        }
    }
}

/// Proposals the SDK could not dispatch, sent before the request fails
/// (TCR-D17). At most `Limits.toolProposals` are kept.
actor ProposalLog {
    private var items: [ToolProposal] = []
    func append(_ proposal: ToolProposal) {
        if items.count < Int(Limits.toolProposals) { items.append(proposal) }
    }
    func all() -> [ToolProposal] { items }
}
/// A request that reported proposals and then failed (TCR-D17): the helper
/// sends the proposals, then `Terminal` failed with `failure`.
public struct ProposedThenFailed: Error, Sendable {
    public let proposals: [ToolProposal]
    public let failure: BackendFailure
}
/// The SDK no longer kept the stop tool's `toolCalls` entry (CC4 regression).
func reportStopToolTranscript() {
    let bytes = Array("asura_model_failure stop_tool_transcript 0\n".utf8)
    let flags = fcntl(STDERR_FILENO, F_GETFL)
    guard flags >= 0, fcntl(STDERR_FILENO, F_SETFL, flags | O_NONBLOCK) == 0 else { return }
    _ = bytes.withUnsafeBytes { Darwin.write(STDERR_FILENO, $0.baseAddress, $0.count) }
}

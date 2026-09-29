import Foundation
import Darwin
import FoundationModels


struct UnexpectedBackendCancellation: Error { let taskCancelled: Bool }

/// Fixed classifications only; provider descriptions may contain private inputs.
func modelFailureDiagnostic(_ error: any Error) -> String {
    let kind: String
    if let sdk = error as? LanguageModelError {
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
        case let backend as BackendFailure: code = backend.memoryArgumentDiagnostic ?? (100 + backend.reason.rawValue)
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

public struct BackendStatus: Sendable {
    public let contextTokens: UInt32?
    public let modelName: String?
    public let supportsTools: Bool
    public let localToolDestination: Bool
    public let capabilityProfile: CapabilityProfile?
    public init(contextTokens: UInt32?, modelName: String? = nil, supportsTools: Bool = false, localToolDestination: Bool = false, capabilityProfile: CapabilityProfile? = nil) { self.contextTokens = contextTokens; self.modelName = modelName; self.supportsTools = supportsTools; self.localToolDestination = localToolDestination; self.capabilityProfile = capabilityProfile }
}
public struct Snapshot: Sendable {
    public let text: String
    public let usageTokens: UInt64?
    public let inputContext: (tokens: UInt32, capacity: UInt32)?
    public init(text: String, usageTokens: UInt64? = nil, inputContext: (tokens: UInt32, capacity: UInt32)? = nil) {
        self.text = text; self.usageTokens = usageTokens; self.inputContext = inputContext
    }
}
public struct BackendFailure: Error, Sendable {
    public let reason: Reason
    let memoryArgumentDiagnostic: Int?
    public init(_ reason: Reason) { self.reason = reason; self.memoryArgumentDiagnostic = nil }
    init(memoryCreateFaults: UInt8) {
        self.reason = .inputLimit
        self.memoryArgumentDiagnostic = 1000 + Int(memoryCreateFaults & 127)
    }
    init(memoryReadWriteFields: UInt8) {
        self.reason = .inputLimit
        self.memoryArgumentDiagnostic = 2000 + Int(memoryReadWriteFields & 3)
    }
}
public protocol ModelBackend: Sendable {
    func status() async -> BackendStatus
    func generate(_ input: ModelInput, maximumTokens: UInt32, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws
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
        return BackendStatus(contextTokens: capacity, modelName: model.variant.displayName, supportsTools: model.capabilities.contains(.toolCalling), capabilityProfile: CapabilityProfile(native: model.capabilities, provenance: .framework))
    }

    public static func history(_ input: ModelInput) -> [Transcript.Entry] {
        func segment(_ text: String) -> [Transcript.Segment] { [.text(.init(content: text))] }
        var entries: [Transcript.Entry] = [
            .instructions(.init(segments: segment(input.instructions), toolDefinitions: []))
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

    private func engine() throws -> FoundationBackend<SystemLanguageModel> {
        let model = SystemLanguageModel.default
        guard case .available = model.availability else { throw BackendFailure(.modelUnavailable) }
        return FoundationBackend(model: model, contextTokens: UInt32(exactly: model.contextSize),
            modelName: model.variant.displayName, supportsTools: true, reportsUsage: true,
            capabilityProfile: CapabilityProfile(native: model.capabilities, provenance: .framework),
            tokenCounter: { entries, tools in
                let transcript = try await model.tokenCount(for: entries)
                let schemas = try await model.tokenCount(for: tools)
                return transcript + schemas
            })
    }
    public func generate(_ input: ModelInput, maximumTokens: UInt32, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await engine().generate(input, maximumTokens: maximumTokens, snapshot: snapshot)
    }
    public func generateWithTools(_ input: ModelInput, maximumTokens: UInt32, handler: @escaping ToolHandler,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await engine().generateWithTools(input, maximumTokens: maximumTokens, handler: handler, snapshot: snapshot)
    }
}

public typealias NativeTokenCounter = @Sendable ([Transcript.Entry], [any Tool]) async throws -> Int

/// One SDK conversation engine shared by all admitted native/custom adapters.
public struct FoundationBackend<Model: LanguageModel>: ToolModelBackend {
    public let model: Model
    public let capabilityProfile: CapabilityProfile
    private let contextTokens: UInt32?
    private let modelName: String
    private let supportsTools: Bool
    private let localToolDestination: Bool
    private let reportsUsage: Bool
    private let tokenCounter: NativeTokenCounter?
    public init(model: Model, contextTokens: UInt32?, modelName: String, supportsTools: Bool,
        reportsUsage: Bool = false, localToolDestination: Bool = false, capabilityProfile: CapabilityProfile? = nil, tokenCounter: NativeTokenCounter? = nil) {
        self.model = model; self.contextTokens = contextTokens; self.modelName = modelName
        let profile = capabilityProfile ?? CapabilityProfile(native: model.capabilities, provenance: .runtime)
        self.capabilityProfile = profile
        self.supportsTools = supportsTools && model.capabilities.contains(.toolCalling) && profile.support(.toolCalling) == .supported
        self.localToolDestination = localToolDestination
        self.reportsUsage = reportsUsage; self.tokenCounter = tokenCounter
    }
    public func status() async -> BackendStatus {
        BackendStatus(contextTokens: contextTokens, modelName: modelName, supportsTools: supportsTools, localToolDestination: localToolDestination, capabilityProfile: capabilityProfile)
    }
    public func generate(_ input: ModelInput, maximumTokens: UInt32, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await generateSession(input, maximumTokens: maximumTokens, handler: nil, snapshot: snapshot)
    }
    public func generateWithTools(_ input: ModelInput, maximumTokens: UInt32, handler: @escaping ToolHandler,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        guard supportsTools else { throw BackendFailure(.modelUnavailable) }
        try capabilityProfile.require([.toolCalling])
        try await generateSession(input, maximumTokens: maximumTokens, handler: handler, snapshot: snapshot)
    }
    private func generateSession(_ input: ModelInput, maximumTokens: UInt32, handler: ToolHandler?,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try Task.checkCancellation()
        let entries = SystemBackend.history(input)
        let current = Transcript.Entry.prompt(.init(segments: [.text(.init(content: input.prompt))]))
        let boundedModel = BoundedToolModel(base: model, maximumTokens: Int(maximumTokens), toolsEnabled: handler != nil)
        do {
            let guardedHandler: ToolHandler?
            if let handler {
                guardedHandler = { @Sendable (call: ToolArguments) async throws -> ToolResult in
                    try await boundedModel.budget.requireSuccessfulInference()
                    return try await handler(call)
                }
            } else {
                guardedHandler = nil
            }
            let tools = guardedHandler.map { makeProjectTools($0, budget: boundedModel.budget) } ?? []
            if let tokenCounter {
                let count = try await tokenCounter(entries + [current], tools)
                try Task.checkCancellation()
                guard let measured = UInt32(exactly: count) else { throw BackendFailure(.contextLimit) }
                if let capacity = contextTokens {
                    guard capacity >= maximumTokens, measured <= capacity - maximumTokens else { throw BackendFailure(.contextLimit) }
                    try await snapshot(Snapshot(text: "", inputContext: (measured, capacity)))
                }
            }
            let session = LanguageModelSession(model: boundedModel, tools: tools,
                transcript: Transcript(entries: entries))
            // The SDK session owns inference; retain it through every async stream exit.
            defer { withExtendedLifetime(session) {} }
            let contextOptions = ContextOptions(reasoningLevel: capabilityProfile.reasoningDisabled ? .custom("no_think") : nil)
            let stream = session.streamResponse(to: input.prompt, options: GenerationOptions(maximumResponseTokens: Int(maximumTokens)), contextOptions: contextOptions)
            for try await value in stream {
                try Task.checkCancellation()
                guard value.content.utf8.count <= Limits.output else { throw BackendFailure(.outputLimit) }
                let total = reportsUsage ? UInt64(exactly: value.usage.output.totalTokenCount) : nil
                if let total, total > UInt64(maximumTokens) { throw BackendFailure(.outputLimit) }
                try await snapshot(Snapshot(text: value.content, usageTokens: handler == nil ? total : nil))
            }
            if let failure = await boundedModel.budget.recordedFailure() { throw failure }
        } catch {
            try Task.checkCancellation()
            if let failure = await boundedModel.budget.recordedFailure() { throw failure }
            if let failure = error as? BackendFailure { throw failure }
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

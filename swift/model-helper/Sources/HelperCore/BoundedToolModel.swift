import Foundation
import FoundationModels

/// The state of one helper request: one SDK session and exactly one
/// inference (context-composer.md, CC4). The service owns the request loop;
/// a second dispatch in one session is a protocol fault. No output cap
/// exists (owner, 2026-10-19; turn-usage-helper.md, request admission).
public actor RequestGate {
    private var admitted = false
    private var failure: BackendFailure?
    public init() {}
    public func record(_ error: BackendFailure) {
        if failure == nil { failure = error }
    }
    public func recordedFailure() -> BackendFailure? { failure }
    /// Admits the session's single inference.
    public func admit() throws {
        try Task.checkCancellation()
        if let failure { throw failure }
        guard !admitted else {
            let error = BackendFailure(.protocolFault)
            record(error)
            throw error
        }
        admitted = true
    }
    func dispatched() -> Bool { admitted }
}

/// Intercepts the SDK's inference dispatch without adding a model loop. It
/// replaces the request transcript with `transcript`, the request's full
/// input (instructions, history, prompt, continuation), because no SDK call
/// continues from a transcript that ends in a tool output (CC4; the
/// executor transcript override of loop closure decision 6). It sets no
/// Asura output cap: `responseTokens` is `nil`, or the window for adapters
/// whose `nil` means a small library default.
public struct BoundedToolModel<Base: LanguageModel>: LanguageModel {
    public let base: Base
    public let gate: RequestGate
    public let usageObserver: InferenceUsageObserver?
    public let responseTokens: Int?
    public let transcript: Transcript?
    public init(base: Base, responseTokens: Int? = nil, transcript: Transcript? = nil,
        usageObserver: InferenceUsageObserver? = nil) {
        self.base = base; self.usageObserver = usageObserver; self.responseTokens = responseTokens
        self.transcript = transcript
        self.gate = RequestGate()
    }
    public var capabilities: LanguageModelCapabilities { base.capabilities }
    public var executorConfiguration: Base.Executor.Configuration { base.executorConfiguration }

    public struct Executor: LanguageModelExecutor {
        public typealias Model = BoundedToolModel<Base>
        public typealias Configuration = Base.Executor.Configuration
        private let underlying: Base.Executor
        public init(configuration: Configuration) throws {
            underlying = try Base.Executor(configuration: configuration)
        }
        /// A full context window ends the turn as a context limit, not an
        /// internal error (loop closure; owner, 2026-10-18). The system
        /// model's mid-turn `GenerativeError` overflow is one too (CC4
        /// prerequisite; `isContextOverflow`).
        static func typed(_ error: any Error) -> BackendFailure {
            if let failure = error as? BackendFailure { return failure }
            if isContextOverflow(error) { return BackendFailure(.contextLimit) }
            return BackendFailure(.internalError)
        }
        public func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            try await model.gate.admit()
            var bounded = request
            bounded.generationOptions.maximumResponseTokens = model.responseTokens
            if let transcript = model.transcript { bounded.transcript = transcript }
            do { try await model.usageObserver?.begin(request.id) }
            catch {
                await model.gate.record((error as? BackendFailure) ?? BackendFailure(.internalError))
                throw error
            }
            var failure: (any Error)?
            do { try await underlying.respond(to: bounded, model: model.base, streamingInto: channel) }
            catch { failure = error }
            do { try await model.usageObserver?.settle(request.id, succeeded: failure == nil) }
            catch { if failure == nil { failure = error } }
            if let failure {
                await model.gate.record(Self.typed(failure))
                // Preserve typed failures while letting the SDK drain already buffered text.
                if !(failure is BackendFailure) { throw failure }
            }
        }
    }
}

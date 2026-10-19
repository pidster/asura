import Foundation
import FoundationModels

/// Request state belongs to one admitted turn, even if the framework caches executors.
/// A turn has no request count and no token total (owner, 2026-10-18), and a
/// request has no output cap (owner, 2026-10-19; turn-usage-helper.md, request admission).
public actor ToolRoundBudget {
    private var used = 0
    private let toolsEnabled: Bool
    private var failure: BackendFailure?
    private var inferenceActive = false
    private var waiters: [UUID: CheckedContinuation<Void, any Error>] = [:]
    public init(toolsEnabled: Bool = true) {
        self.toolsEnabled = toolsEnabled
    }
    public func record(_ error: BackendFailure) {
        if failure == nil { failure = error }
    }
    public func recordedFailure() -> BackendFailure? { failure }
    public func finishInference(_ error: BackendFailure? = nil) {
        if let error { record(error) }
        inferenceActive = false
        let pending = waiters.values
        waiters.removeAll()
        for waiter in pending {
            if let failure { waiter.resume(throwing: failure) }
            else { waiter.resume() }
        }
    }
    public func requireSuccessfulInference() async throws {
        try Task.checkCancellation()
        if let failure { throw failure }
        guard inferenceActive else { return }
        let id = UUID()
        try await withTaskCancellationHandler { () async throws -> Void in
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, any Error>) in
                if Task.isCancelled { continuation.resume(throwing: CancellationError()) }
                else if waiters.count >= 8 {
                    let error = BackendFailure(.outputLimit)
                    record(error)
                    continuation.resume(throwing: error)
                }
                else { waiters[id] = continuation }
            }
        } onCancel: {
            Task { await self.cancelWaiter(id) }
        }
        try Task.checkCancellation()
        if let failure { throw failure }
    }
    /// A native proposal waits for its inference to settle. The session owns
    /// the ordinal policy: it sends the over-limit proposal for its record and
    /// then ends the turn (TCR-D6).
    public func reserveNativeProposal() async throws {
        try await requireSuccessfulInference()
        guard toolsEnabled else {
            let error = BackendFailure(.outputLimit)
            record(error)
            throw error
        }
    }
    private func cancelWaiter(_ id: UUID) {
        waiters.removeValue(forKey: id)?.resume(throwing: CancellationError())
    }
    func pendingCallbacks() -> Int { waiters.count }
    /// Admits one request. Tool-enabled turns admit any number of requests;
    /// plain text admits one, because it has no tool continuation and a second
    /// dispatch would repeat the answer (owner, 2026-10-18). No output cap is
    /// allocated (owner, 2026-10-19).
    public func reserve() throws {
        try Task.checkCancellation()
        if let failure { throw failure }
        guard toolsEnabled || used == 0 else {
            let error = BackendFailure(.outputLimit)
            record(error)
            throw error
        }
        guard !inferenceActive else {
            let error = BackendFailure(.protocolFault); record(error); throw error
        }
        used = used == Int.max ? used : used + 1
        inferenceActive = true
    }

}

/// Intercepts every SDK inference dispatch without adding another model loop.
/// It sets no Asura output cap: `responseTokens` is `nil` for providers that
/// then write until the model stops or the window is full, or the context
/// window for adapters whose `nil` means a small library default (owner,
/// 2026-10-19; turn-usage-helper.md, request admission).
public struct BoundedToolModel<Base: LanguageModel>: LanguageModel {
    public let base: Base
    public let budget: ToolRoundBudget
    public let usageObserver: InferenceUsageObserver?
    public let responseTokens: Int?
    public init(base: Base, responseTokens: Int? = nil, toolsEnabled: Bool = true, usageObserver: InferenceUsageObserver? = nil) {
        self.base = base; self.usageObserver = usageObserver; self.responseTokens = responseTokens
        self.budget = ToolRoundBudget(toolsEnabled: toolsEnabled)
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
        /// internal error (loop closure; owner, 2026-10-18).
        static func typed(_ error: any Error) -> BackendFailure {
            if let failure = error as? BackendFailure { return failure }
            if let error = error as? LanguageModelError, case .contextSizeExceeded = error {
                return BackendFailure(.contextLimit)
            }
            return BackendFailure(.internalError)
        }
        public func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            try await model.budget.reserve()
            var bounded = request
            bounded.generationOptions.maximumResponseTokens = model.responseTokens
            do { try await model.usageObserver?.begin(request.id) }
            catch {
                await model.budget.finishInference((error as? BackendFailure) ?? BackendFailure(.internalError))
                throw error
            }
            var failure: (any Error)?
            do { try await underlying.respond(to: bounded, model: model.base, streamingInto: channel) }
            catch { failure = error }
            do { try await model.usageObserver?.settle(request.id, succeeded: failure == nil) }
            catch { if failure == nil { failure = error } }
            if let failure {
                await model.budget.finishInference(Self.typed(failure))
                // Preserve typed failures while letting the SDK drain already buffered text.
                if !(failure is BackendFailure) { throw failure }
            } else {
                await model.budget.finishInference()
            }
        }
    }
}

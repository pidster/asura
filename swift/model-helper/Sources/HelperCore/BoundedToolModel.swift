import Foundation
import FoundationModels

public enum ToolRoundLimits {
    public static let firstAllocation = 1024
    public static let continuationAllocation = 512
    public static let maximumRequests = 64
}

/// Budget state belongs to one admitted turn, even if the framework caches executors.
public actor ToolRoundBudget {
    private var used = 0
    private var remaining: Int
    private let toolsEnabled: Bool
    private var failure: BackendFailure?
    private var inferenceActive = false
    private var activeAllocation: Int?
    private var waiters: [UUID: CheckedContinuation<Void, any Error>] = [:]
    public init(maximumTokens: Int = 2048, toolsEnabled: Bool = true) {
        remaining = min(max(0, maximumTokens), 2048)
        self.toolsEnabled = toolsEnabled
    }
    public func record(_ error: BackendFailure) {
        if failure == nil { failure = error }
    }
    public func recordedFailure() -> BackendFailure? { failure }
    public func finishInference(_ error: BackendFailure? = nil, measuredOutput: UInt64? = nil) {
        if let error { record(error) }
        if let measuredOutput, let allocation = activeAllocation, error == nil, failure == nil {
            if measuredOutput <= UInt64(allocation) { remaining += allocation - Int(measuredOutput) }
            else { record(BackendFailure(.protocolFault)) }
        }
        activeAllocation = nil
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
    /// the ordinal policy: it sends the ninth proposal for its record and then
    /// ends the turn (TCR-D6).
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
    public func reserve(requestedMaximum: Int? = nil) throws -> Int {
        try Task.checkCancellation()
        if let failure { throw failure }
        guard remaining > 0, used < (toolsEnabled ? ToolRoundLimits.maximumRequests : 1) else {
            let error = BackendFailure(.outputLimit)
            record(error)
            throw error
        }
        guard !inferenceActive else {
            let error = BackendFailure(.protocolFault); record(error); throw error
        }
        let requested = requestedMaximum ?? remaining
        guard requested > 0 else { throw BackendFailure(.outputLimit) }
        let passLimit = toolsEnabled ? (used == 0 ? ToolRoundLimits.firstAllocation : ToolRoundLimits.continuationAllocation) : remaining
        let allocation = min(remaining, passLimit, requested)
        used += 1
        remaining -= allocation
        inferenceActive = true; activeAllocation = allocation
        return allocation
    }

}

/// Intercepts every SDK inference dispatch without adding another model loop.
/// Use only with providers whose executor enforces maximumResponseTokens.
public struct BoundedToolModel<Base: LanguageModel>: LanguageModel {
    public let base: Base
    public let budget: ToolRoundBudget
    public let usageObserver: InferenceUsageObserver?
    public init(base: Base, maximumTokens: Int = 2048, toolsEnabled: Bool = true, usageObserver: InferenceUsageObserver? = nil) {
        self.base = base; self.usageObserver = usageObserver
        self.budget = ToolRoundBudget(maximumTokens: maximumTokens, toolsEnabled: toolsEnabled)
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
        public func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            let allocation = try await model.budget.reserve(requestedMaximum: request.generationOptions.maximumResponseTokens)
            var bounded = request
            bounded.generationOptions.maximumResponseTokens = allocation
            do { try await model.usageObserver?.begin(request.id, cap: allocation) }
            catch {
                await model.budget.finishInference((error as? BackendFailure) ?? BackendFailure(.internalError))
                throw error
            }
            var failure: (any Error)?
            do { try await underlying.respond(to: bounded, model: model.base, streamingInto: channel) }
            catch { failure = error }
            var measuredOutput: UInt64?
            do { measuredOutput = try await model.usageObserver?.settle(request.id, succeeded: failure == nil) }
            catch { if failure == nil { failure = error } }
            if let failure {
                await model.budget.finishInference((failure as? BackendFailure) ?? BackendFailure(.internalError))
                // Preserve typed failures while letting the SDK drain already buffered text.
                if !(failure is BackendFailure) { throw failure }
            } else {
                await model.budget.finishInference(measuredOutput: measuredOutput)
            }
        }
    }
}

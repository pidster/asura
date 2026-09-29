import Foundation
import FoundationModels

public enum ToolRoundLimits {
    public static let allocations = [1024, 512, 512]
}

/// Budget state belongs to one admitted turn, even if the framework caches executors.
public actor ToolRoundBudget {
    private var used = 0
    private var nativeProposals = 0
    private var remaining: Int
    private let toolsEnabled: Bool
    private var failure: BackendFailure?
    private var inferenceActive = false
    private var waiters: [UUID: CheckedContinuation<Void, any Error>] = [:]
    public init(maximumTokens: Int = 2048, toolsEnabled: Bool = true) {
        remaining = min(max(0, maximumTokens), 2048)
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
    public func reserveNativeProposal() async throws {
        try await requireSuccessfulInference()
        guard toolsEnabled, nativeProposals < 8 else {
            let error = BackendFailure(.outputLimit)
            record(error)
            throw error
        }
        nativeProposals += 1
    }
    private func cancelWaiter(_ id: UUID) {
        waiters.removeValue(forKey: id)?.resume(throwing: CancellationError())
    }
    func pendingCallbacks() -> Int { waiters.count }
    public func reserve() throws -> Int {
        try Task.checkCancellation()
        if let failure { throw failure }
        guard remaining > 0, used < (toolsEnabled ? ToolRoundLimits.allocations.count : 1) else {
            let error = BackendFailure(.outputLimit)
            record(error)
            throw error
        }
        let allocation = min(remaining, toolsEnabled ? ToolRoundLimits.allocations[used] : remaining)
        used += 1
        remaining -= allocation
        inferenceActive = true
        return allocation
    }

}

/// Intercepts every SDK inference dispatch without adding another model loop.
/// Use only with providers whose executor enforces maximumResponseTokens.
public struct BoundedToolModel<Base: LanguageModel>: LanguageModel {
    public let base: Base
    public let budget: ToolRoundBudget
    public init(base: Base, maximumTokens: Int = 2048, toolsEnabled: Bool = true) {
        self.base = base
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
            let allocation = try await model.budget.reserve()
            var bounded = request
            bounded.generationOptions.maximumResponseTokens = min(
                request.generationOptions.maximumResponseTokens ?? allocation,
                allocation)
            do {
                try await underlying.respond(to: bounded, model: model.base, streamingInto: channel)
                await model.budget.finishInference()
            } catch let error as BackendFailure {
                await model.budget.finishInference(error)
                // Let the SDK drain buffered text. The guarded native callback
                // and every later inference dispatch reject this retained error.
            } catch {
                await model.budget.finishInference(BackendFailure(.internalError))
                throw error
            }
        }
    }
}

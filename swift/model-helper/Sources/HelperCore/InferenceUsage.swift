import Foundation

/// One request's measured usage. Missing counters remain unknown, including cached input.
public struct RequestUsage: Sendable, Equatable {
    public let ordinal: UInt32
    public let inputTokens: UInt64?
    public let outputTokens: UInt64?
    public let cachedInputTokens: UInt64?
    public let complete: Bool
    public init(ordinal: UInt32, inputTokens: UInt64? = nil, outputTokens: UInt64? = nil,
        cachedInputTokens: UInt64? = nil, complete: Bool = false) {
        self.ordinal = ordinal; self.inputTokens = inputTokens; self.outputTokens = outputTokens
        self.cachedInputTokens = cachedInputTokens; self.complete = complete
    }
    var wire: Asura_Model_V1_InferenceUsage {
        var result = Asura_Model_V1_InferenceUsage()
        result.requestOrdinal = ordinal; result.complete = complete
        if let inputTokens { result.inputTokens = inputTokens }
        if let outputTokens { result.outputTokens = outputTokens }
        if let cachedInputTokens { result.cachedInputTokens = cachedInputTokens }
        return result
    }
}

/// A turn-local observer. It owns one active request tuple and never owns model scheduling.
public actor InferenceUsageObserver {
    private struct Active {
        let id: UUID
        let ordinal: UInt32
        let cap: Int
        var input: UInt64?
        var output: UInt64?
        var cached: UInt64?
        var measured = false
    }
    private var ordinal: UInt32 = 0
    private var active: Active?
    private let snapshot: @Sendable (Snapshot) async throws -> Void
    public init(snapshot: @escaping @Sendable (Snapshot) async throws -> Void) { self.snapshot = snapshot }

    func begin(_ id: UUID, cap: Int) async throws {
        try Task.checkCancellation()
        // No request count per turn (owner, 2026-10-18); UInt32 exhaustion is a fault.
        guard active == nil, ordinal < UInt32.max, (1...Int(Limits.maximumResponseTokens)).contains(cap) else {
            throw BackendFailure(.protocolFault)
        }
        ordinal += 1
        active = Active(id: id, ordinal: ordinal, cap: cap)
        try await snapshot(Snapshot(text: "", inferenceStartedOrdinal: ordinal))
    }
    func measure(_ id: UUID, input: Int?, output: Int?, cached: Int? = nil) throws {
        guard var current = active, current.id == id, !current.measured else { throw BackendFailure(.protocolFault) }
        func checked(_ value: Int?) throws -> UInt64? {
            guard let value else { return nil }
            guard (0...1_000_000_000).contains(value) else { throw BackendFailure(.protocolFault) }
            return UInt64(value)
        }
        let measuredInput = try checked(input)
        let measuredOutput = try checked(output)
        let measuredCached = try checked(cached)
        guard measuredOutput.map({ $0 <= UInt64(current.cap) }) ?? true,
            measuredCached.map({ measuredInput != nil && $0 <= measuredInput! }) ?? true else {
            throw BackendFailure(.protocolFault)
        }
        current.input = measuredInput; current.output = measuredOutput; current.cached = measuredCached
        current.measured = true; active = current
    }
    /// Settlement is awaited before inference waiters can admit tools or another request.
    /// It releases no tokens: a turn has no token total (owner, 2026-10-18).
    func settle(_ id: UUID, succeeded: Bool) async throws {
        guard let current = active, current.id == id else { throw BackendFailure(.protocolFault) }
        active = nil
        let report = RequestUsage(ordinal: current.ordinal, inputTokens: current.input,
            outputTokens: current.output, cachedInputTokens: current.cached,
            complete: succeeded && current.input != nil && current.output != nil)
        try await snapshot(Snapshot(text: "", inferenceUsage: report))
    }
}

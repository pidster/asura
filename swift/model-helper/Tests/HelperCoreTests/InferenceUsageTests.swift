import Foundation
import FoundationModels
import Testing
@testable import HelperCore

private actor UsageSnapshots {
    var reports: [RequestUsage] = []
    var starts: [UInt32] = []
    var texts: [String] = []
    func record(_ value: Snapshot) {
        if let report = value.inferenceUsage { reports.append(report) }
        if let ordinal = value.inferenceStartedOrdinal { starts.append(ordinal) }
        if !value.text.isEmpty { texts.append(value.text) }
    }
}

@Test func measuredUsagePreservesZeroAndMissingCachedCount() async throws {
    let snapshots = UsageSnapshots()
    let observer = InferenceUsageObserver { await snapshots.record($0) }
    let request = UUID()
    try await observer.begin(request, cap: 512)
    try await observer.measure(request, input: 0, output: 0)
    #expect(try await observer.settle(request, succeeded: true) == 0)
    #expect(await snapshots.starts == [1])
    #expect(await snapshots.reports == [RequestUsage(ordinal: 1, inputTokens: 0, outputTokens: 0, complete: true)])
}

@Test func missingAndFailedUsageDoNotInventCountersOrRefunds() async throws {
    let snapshots = UsageSnapshots()
    let observer = InferenceUsageObserver { await snapshots.record($0) }
    let unknown = UUID()
    try await observer.begin(unknown, cap: 512)
    #expect(try await observer.settle(unknown, succeeded: true) == nil)
    let partial = UUID()
    try await observer.begin(partial, cap: 512)
    try await observer.measure(partial, input: nil, output: 5)
    #expect(try await observer.settle(partial, succeeded: true) == 5)
    let failed = UUID()
    try await observer.begin(failed, cap: 512)
    try await observer.measure(failed, input: 10, output: 7, cached: 3)
    #expect(try await observer.settle(failed, succeeded: false) == nil)
    #expect(await snapshots.reports == [RequestUsage(ordinal: 1),
        RequestUsage(ordinal: 2, outputTokens: 5),
        RequestUsage(ordinal: 3, inputTokens: 10, outputTokens: 7, cachedInputTokens: 3)])
}

@Test func usageRejectsWrongRequestOversizedOutputAndDuplicates() async throws {
    let snapshots = UsageSnapshots()
    let observer = InferenceUsageObserver { await snapshots.record($0) }
    let request = UUID()
    try await observer.begin(request, cap: 20)
    await #expect(throws: BackendFailure.self) { try await observer.measure(UUID(), input: 1, output: 1) }
    await #expect(throws: BackendFailure.self) { try await observer.measure(request, input: 1, output: 21) }
    await #expect(throws: BackendFailure.self) { try await observer.measure(request, input: 1_000_000_001, output: 1) }
    await #expect(throws: BackendFailure.self) { try await observer.measure(request, input: 3, output: 1, cached: 4) }
    try await observer.measure(request, input: 3, output: 1)
    await #expect(throws: BackendFailure.self) { try await observer.measure(request, input: 3, output: 1) }
    _ = try await observer.settle(request, succeeded: true)
    await #expect(throws: BackendFailure.self) { _ = try await observer.settle(request, succeeded: true) }
    #expect(await snapshots.reports.count == 1)
}

@Test func unknownAndFailedAllocationsRemainConservativelyCharged() async throws {
    let unknown = ToolRoundBudget()
    #expect(try await unknown.reserve() == 1024); await unknown.finishInference()
    #expect(try await unknown.reserve() == 512); await unknown.finishInference()
    #expect(try await unknown.reserve() == 512); await unknown.finishInference()
    await #expect(throws: BackendFailure.self) { _ = try await unknown.reserve() }
    let failed = ToolRoundBudget(maximumTokens: 512)
    _ = try await failed.reserve()
    await failed.finishInference(BackendFailure(.cancelled), measuredOutput: 1)
    await #expect(throws: BackendFailure.self) { _ = try await failed.reserve() }
    let oversized = ToolRoundBudget(maximumTokens: 20)
    _ = try await oversized.reserve()
    await oversized.finishInference(measuredOutput: 21)
    #expect(await oversized.recordedFailure()?.reason == Reason.protocolFault)
    await #expect(throws: BackendFailure.self) { _ = try await oversized.reserve() }
}

private actor FourPassProbe {
    var passes = 0
    var caps: [Int] = []
    var hostCalls = 0
    func next(_ cap: Int) -> Int { passes += 1; caps.append(cap); return passes }
    func hostCalled() { hostCalls += 1 }
}
private struct FourPassModel: LanguageModel {
    let probe: FourPassProbe
    var usage: InferenceUsageObserver?
    var capabilities: LanguageModelCapabilities { .init([.toolCalling]) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Configuration = Int
        typealias Model = FourPassModel
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            let cap = try #require(request.generationOptions.maximumResponseTokens)
            let pass = await model.probe.next(cap)
            if pass <= 3 {
                await channel.send(.toolCalls(action: .toolCall(id: "fixture-\(pass)", name: "service",
                    action: .appendArguments(#"{"command":"tools"}"#, tokenCount: 10))))
            } else {
                await channel.send(.response(action: .appendText("fourth final response", tokenCount: 17)))
            }
            try await model.usage?.measure(request.id, input: 100 + pass, output: pass <= 3 ? 10 : 17)
        }
    }
}

@Test func threeMeasuredToolPassesAllowFourthFinalResponseWithinTurnReservation() async throws {
    let probe = FourPassProbe()
    let snapshots = UsageSnapshots()
    let base = FourPassModel(probe: probe)
    let engine = FoundationBackend(model: base, contextTokens: 8192,
        modelName: "measured-fixture", supportsTools: true,
        instrumentationFactory: { usage, _, _ in var model = base; model.usage = usage; return model })
    var input = ModelInput(); input.instructions = "fixture"; input.prompt = "fixture"
    try await engine.generateWithTools(input, maximumTokens: 2048, handler: { proposal in
        guard proposal.name == "service", proposal.arguments.contains("tools") else { throw HelperError.protocolFault }
        await probe.hostCalled()
        return ToolResult(status: .success, text: "fixture tools")
    }, snapshot: { await snapshots.record($0) })
    #expect(await probe.passes == 4)
    #expect(await probe.hostCalls == 3)
    #expect(await probe.caps == [1024, 512, 512, 512])
    #expect(await snapshots.starts == [1, 2, 3, 4])
    let reports = await snapshots.reports
    #expect(reports.map(\.ordinal) == [1, 2, 3, 4])
    #expect(reports.allSatisfy { $0.complete })
    #expect(reports.compactMap(\.outputTokens).reduce(0, +) == 47)
    #expect(await snapshots.texts.contains("fourth final response"))
}

@Test func usageCodecKeepsUnknownDistinctFromZeroAndEnforcesBounds() throws {
    var message = Envelope(); message.operationID = Data(repeating: 1, count: 16); message.generation = 1
    message.body = .inferenceUsage(RequestUsage(ordinal: 1).wire)
    try Wire.validate(message, from: .helper)
    #expect(!message.inferenceUsage.hasInputTokens && !message.inferenceUsage.hasOutputTokens)
    message.body = .inferenceUsage(RequestUsage(ordinal: 1, inputTokens: 0, outputTokens: 0, complete: true).wire)
    try Wire.validate(message, from: .helper)
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .service) }
    message.inferenceUsage.cachedInputTokens = 1
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .helper) }
    message.inferenceUsage.clearCachedInputTokens(); message.inferenceUsage.outputTokens = 1_000_000_001
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .helper) }
}

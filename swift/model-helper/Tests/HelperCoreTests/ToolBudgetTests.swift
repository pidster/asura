import Foundation
import FoundationModels
import Testing
@testable import HelperCore

private actor DispatchProbe {
    var limits: [Int?] = []
    func record(_ value: Int?) { limits.append(value) }
}
private struct ProbeModel: LanguageModel {
    let probe: DispatchProbe
    var capabilities: LanguageModelCapabilities { .init([]) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Model = ProbeModel
        typealias Configuration = Int
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: ProbeModel,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            await model.probe.record(request.generationOptions.maximumResponseTokens)
        }
    }
}
/// UH10 (owner, 2026-10-18): a tool turn dispatches more than the former 64
/// requests and more than the former 2,048-token total; each request keeps its cap.
@Test func toolInferenceRequestsHaveNoCountOrTurnTokenTotal() async throws {
    let probe = DispatchProbe()
    let model = BoundedToolModel(base: ProbeModel(probe: probe))
    let executor = try BoundedToolModel<ProbeModel>.Executor(configuration: 0)
    let request = LanguageModelExecutorGenerationRequest(id: UUID(), transcript: Transcript(entries: []),
        enabledTools: [], generationOptions: GenerationOptions(maximumResponseTokens: 2048),
        contextOptions: ContextOptions(), metadata: [:])
    for _ in 0..<100 {
        try await executor.respond(to: request, model: model, streamingInto: .init())
    }
    let limits = await probe.limits
    #expect(limits == [1024] + Array(repeating: 512, count: 99))
    #expect(limits.compactMap { $0 }.reduce(0, +) > 2048)
    #expect(await model.budget.recordedFailure() == nil)
    let secondTurn = BoundedToolModel(base: ProbeModel(probe: probe))
    try await executor.respond(to: request, model: secondTurn, streamingInto: .init())
    #expect(await probe.limits.last == 1024)
}

@Test func genericEngineDeniesUnadvertisedToolsBeforeProviderDispatch() async throws {
    let probe = DispatchProbe()
    let engine = FoundationBackend(model: ProbeModel(probe: probe), contextTokens: nil,
        modelName: "unqualified", supportsTools: false)
    let status = await engine.status()
    #expect(status.contextTokens == nil && !status.supportsTools)
    do {
        try await engine.generateWithTools(ModelInput(), maximumTokens: 512,
            handler: { _ in Issue.record("unsupported tool reached host"); throw HelperError.unavailable },
            snapshot: { _ in Issue.record("unsupported provider emitted output") })
        Issue.record("unsupported tools were accepted")
    } catch let failure as BackendFailure { #expect(failure.reason == .modelUnavailable) }
    #expect(await probe.limits.isEmpty)
}

@Test func genericEngineChecksNativeInputCountBeforeDispatch() async throws {
    let probe = DispatchProbe()
    let engine = FoundationBackend(model: ProbeModel(probe: probe), contextTokens: 4096,
        modelName: "counted", supportsTools: true, tokenCounter: { _, _ in 4096 })
    var input = ModelInput(); input.instructions = "test"; input.prompt = "test"
    do {
        try await engine.generate(input, maximumTokens: 512, snapshot: { _ in
            Issue.record("overfull context emitted output")
        })
        Issue.record("overfull context reached inference")
    } catch let failure as BackendFailure { #expect(failure.reason == .contextLimit) }
    #expect(await probe.limits.isEmpty)
}

@Test func diagnosticDoesNotIncludeProviderDescriptionsOrMetadata() {
    let secret = "PRIVATE_PROMPT_DO_NOT_LOG"
    let error = NSError(domain: secret, code: 42, userInfo: [NSLocalizedDescriptionKey: secret])
    #expect(modelFailureDiagnostic(error) == "asura_model_failure unclassified 42\n")
    #expect(!modelFailureDiagnostic(BackendFailure(.internalError)).contains(secret))
    let sdk = LanguageModelError.contextSizeExceeded(.init(contextSize: 100, tokenCount: 101,
        debugDescription: secret, metadata: ["prompt": secret]))
    #expect(modelFailureDiagnostic(sdk).hasPrefix("asura_model_failure context_limit "))
    #expect(!modelFailureDiagnostic(sdk).contains(secret))
}


@Test func adapterCannotAdvertiseToolsMissingFromNativeModel() async throws {
    let probe = DispatchProbe()
    let engine = FoundationBackend(model: ProbeModel(probe: probe), contextTokens: 4096,
        modelName: "native-text-only", supportsTools: true)
    #expect(!(await engine.status()).supportsTools)
    do {
        try await engine.generateWithTools(ModelInput(), maximumTokens: 512,
            handler: { _ in Issue.record("unsupported native tools reached host"); throw HelperError.unavailable },
            snapshot: { _ in Issue.record("unsupported native tools emitted output") })
        Issue.record("adapter flag overrode native capabilities")
    } catch let failure as BackendFailure { #expect(failure.reason == .modelUnavailable) }
    #expect(await probe.limits.isEmpty)
}


@Test func generatedContentFailureDiagnosticNeverIncludesRawContent() {
    let secret = "PRIVATE_GENERATED_CONTENT"
    let error = GeneratedContent.ParsingError(rawContent: secret, debugDescription: secret)
    let diagnostic = modelFailureDiagnostic(error)
    #expect(diagnostic.hasPrefix("asura_model_failure generated_content_invalid_json "))
    #expect(!diagnostic.contains(secret))
}


/// UH13: a small caller cap bounds every request; the requests together can
/// exceed it, because no turn total exists (owner, 2026-10-18).
@Test func smallCallerCapsBoundEveryRequestWithoutATurnTotal() async throws {
    let probe = DispatchProbe()
    let model = BoundedToolModel(base: ProbeModel(probe: probe))
    let executor = try BoundedToolModel<ProbeModel>.Executor(configuration: 0)
    let request = LanguageModelExecutorGenerationRequest(id: UUID(), transcript: Transcript(entries: []),
        enabledTools: [], generationOptions: GenerationOptions(maximumResponseTokens: 20),
        contextOptions: ContextOptions(), metadata: [:])
    for _ in 0..<200 { try await executor.respond(to: request, model: model, streamingInto: .init()) }
    #expect(await probe.limits == Array(repeating: 20, count: 200))
    #expect(await probe.limits.compactMap { $0 }.reduce(0, +) == 4000)
}


@Test func parsingDiagnosticsClassifyOnlyBoundedShape() {
    for (raw, shape) in [("", "empty"), ("{}", "object"), ("[]", "array"), ("123", "scalar"),
                         ("{", "invalid_json"), (String(repeating: "x", count: 16_385), "oversized")] {
        let error = GeneratedContent.ParsingError(rawContent: raw, debugDescription: "PRIVATE")
        let diagnostic = modelFailureDiagnostic(error)
        #expect(diagnostic == "asura_model_failure generated_content_\(shape) \(min(raw.utf8.count, 16_385))\n")
        #expect(!diagnostic.contains("PRIVATE"))
    }
}


@Test func callerCapLimitsEveryExecutorPassWithoutATurnTotal() async throws {
    let probe = DispatchProbe()
    let model = BoundedToolModel(base: ProbeModel(probe: probe), maximumTokens: 300)
    let executor = try BoundedToolModel<ProbeModel>.Executor(configuration: 0)
    let request = LanguageModelExecutorGenerationRequest(id: UUID(), transcript: Transcript(entries: []),
        enabledTools: [], generationOptions: GenerationOptions(maximumResponseTokens: 2048),
        contextOptions: ContextOptions(), metadata: [:])
    for _ in 0..<3 { try await executor.respond(to: request, model: model, streamingInto: .init()) }
    #expect(await probe.limits == [300, 300, 300])
    #expect(await model.budget.recordedFailure() == nil)
}

/// UH14: plain text dispatches once at the caller cap.
@Test func plainTextUsesOneCallerCapAndCannotDispatchTwice() async throws {
    let budget = ToolRoundBudget(maximumTokens: 2048, toolsEnabled: false)
    #expect(try await budget.reserve() == 2048)
    await #expect(throws: BackendFailure.self) { try await budget.reserve() }
    let separate = ToolRoundBudget(maximumTokens: 7, toolsEnabled: false)
    #expect(try await separate.reserve() == 7)
}

private struct TypedLimitModel: LanguageModel {
    var emitTool = false
    var capabilities: LanguageModelCapabilities { .init(emitTool ? [.toolCalling] : []) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Model = TypedLimitModel
        typealias Configuration = Int
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: TypedLimitModel,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            await channel.send(.response(action: .appendText("final partial content", tokenCount: 3)))
            if model.emitTool {
                await channel.send(.toolCalls(action: .toolCall(id: "failed-call", name: "service",
                    action: .appendArguments(#"{"command":"status"}"#, tokenCount: 2))))
            }
            throw BackendFailure(.outputLimit)
        }
    }
}
private actor PartialSnapshots {
    var texts: [String] = []
    func add(_ text: String) { texts.append(text) }
}
@Test func sdkWrappedExecutorLimitRetainsTypedFailureAndPartialText() async throws {
    let snapshots = PartialSnapshots()
    let engine = FoundationBackend(model: TypedLimitModel(), contextTokens: 8192,
        modelName: "limit-fixture", supportsTools: false)
    var input = ModelInput(); input.instructions = "test"; input.prompt = "test"
    do {
        try await engine.generate(input, maximumTokens: 2048, snapshot: { await snapshots.add($0.text) })
        Issue.record("executor limit incorrectly completed")
    } catch let error as BackendFailure { #expect(error.reason == .outputLimit) }
    #expect(await snapshots.texts.contains("final partial content"))
}

@Test func nativeContextCheckReservesRequestedOutputTokens() async throws {
    let probe = DispatchProbe()
    let engine = FoundationBackend(model: ProbeModel(probe: probe), contextTokens: 4096,
        modelName: "counted", supportsTools: false, tokenCounter: { _, _ in 2500 })
    do {
        try await engine.generate(ModelInput(), maximumTokens: 2048, snapshot: { _ in
            Issue.record("insufficient output capacity emitted content")
        })
        Issue.record("insufficient output capacity reached inference")
    } catch let error as BackendFailure { #expect(error.reason == .contextLimit) }
    #expect(await probe.limits.isEmpty)
}


private func waitForCallbacks(_ count: Int, in budget: ToolRoundBudget) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(1))
    while await budget.pendingCallbacks() != count && ContinuousClock.now < deadline {
        try await Task.sleep(for: .milliseconds(1))
    }
    #expect(await budget.pendingCallbacks() == count)
}
@Test func failedInferenceRejectsWaitingCallbacksAndFurtherPasses() async throws {
    let budget = ToolRoundBudget()
    _ = try await budget.reserve()
    let callback = Task { try await budget.requireSuccessfulInference() }
    defer { callback.cancel() }
    try await waitForCallbacks(1, in: budget)
    await budget.finishInference(BackendFailure(.outputLimit))
    do { try await callback.value; Issue.record("failed pass authorized callback") }
    catch let error as BackendFailure { #expect(error.reason == .outputLimit) }
    #expect(await budget.pendingCallbacks() == 0)
    await #expect(throws: BackendFailure.self) { try await budget.reserve() }
}
@Test func callbackGateCancellationDoesNotRetainWaiters() async throws {
    let budget = ToolRoundBudget(); _ = try await budget.reserve()
    let callback = Task { try await budget.requireSuccessfulInference() }
    try await waitForCallbacks(1, in: budget)
    callback.cancel()
    await #expect(throws: CancellationError.self) { try await callback.value }
    #expect(await budget.pendingCallbacks() == 0)
    await budget.finishInference()
    try await budget.requireSuccessfulInference()
}
@Test func sdkFailureNeverExecutesProposedNativeTool() async throws {
    let calls = PartialSnapshots()
    let snapshots = PartialSnapshots()
    let engine = FoundationBackend(model: TypedLimitModel(emitTool: true), contextTokens: 8192,
        modelName: "failed-tool-fixture", supportsTools: true)
    var input = ModelInput(); input.instructions = "test"; input.prompt = "test"
    do {
        try await engine.generateWithTools(input, maximumTokens: 2048, handler: { _ in
            await calls.add("host effect")
            return ToolResult(status: .success, text: "must not execute")
        }, snapshot: { await snapshots.add($0.text) })
        Issue.record("failed inference completed")
    } catch let error as BackendFailure { #expect(error.reason == .outputLimit) }
    #expect(await calls.texts.isEmpty)
    #expect(await snapshots.texts.contains("final partial content"))
}


@Test func ninthPendingNativeCallbackFailsBeforeHostEffects() async throws {
    let budget = ToolRoundBudget(); _ = try await budget.reserve()
    var tasks: [Task<Void, any Error>] = []
    defer { for task in tasks { task.cancel() } }
    for count in 1...8 {
        tasks.append(Task { try await budget.requireSuccessfulInference() })
        try await waitForCallbacks(count, in: budget)
    }
    await #expect(throws: BackendFailure.self) { try await budget.requireSuccessfulInference() }
    await budget.finishInference()
    for task in tasks {
        do { try await task.value; Issue.record("overloaded inference authorized callback") }
        catch let error as BackendFailure { #expect(error.reason == .outputLimit) }
    }
    #expect(await budget.pendingCallbacks() == 0)
}
@Test func cancellationBeforeCallbackRegistrationRetainsNothing() async throws {
    let budget = ToolRoundBudget(); _ = try await budget.reserve()
    let task = Task { try await budget.requireSuccessfulInference() }
    task.cancel()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await budget.pendingCallbacks() == 0)
    await budget.finishInference()
}

@Test func wrappedToolDiagnosticsOnlyExposeClosedErrorCodes() {
    let secret = "PRIVATE_TOOL_DESCRIPTION_AND_ARGUMENTS"
    let tool = ServiceTool { _ in throw HelperError.unavailable }
    let cases: [(any Error, Int)] = [
        (BackendFailure(.inputLimit), 100 + Reason.inputLimit.rawValue),
        (BackendFailure(.outputLimit), 100 + Reason.outputLimit.rawValue),
        (HelperError.protocolFault, 201), (HelperError.limit, 202),
        (HelperError.closed, 203), (HelperError.unavailable, 204),
        (HelperError.contextLimit, 205), (HelperError.timeout, 206),
        (DecodingError.dataCorrupted(.init(codingPath: [], debugDescription: secret)), 300),
        (CancellationError(), 400),
        (NSError(domain: secret, code: 999, userInfo: [NSLocalizedDescriptionKey: secret]), 0)
    ]
    for (underlying, code) in cases {
        let wrapped = LanguageModelSession.ToolCallError(tool: tool, underlyingError: underlying)
        let diagnostic = modelFailureDiagnostic(wrapped)
        #expect(diagnostic == "asura_model_failure tool_callback \(code)\n")
        #expect(!diagnostic.contains(secret))
        #expect(!diagnostic.contains(tool.name))
        #expect(diagnostic.utf8.count < 256)
    }
    let nested = LanguageModelSession.ToolCallError(tool: tool, underlyingError: HelperError.protocolFault)
    #expect(modelFailureDiagnostic(LanguageModelSession.ToolCallError(tool: tool, underlyingError: nested))
        == "asura_model_failure tool_callback 0\n")
}

private actor ContextProbe {
    var passes = 0
    func next() -> Int { passes += 1; return passes }
}
/// The first request proposes a tool; the second finds the context window full.
private struct FullContextModel: LanguageModel {
    let probe: ContextProbe
    var capabilities: LanguageModelCapabilities { .init([.toolCalling]) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Configuration = Int
        typealias Model = FullContextModel
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            if await model.probe.next() == 1 {
                await channel.send(.toolCalls(action: .toolCall(id: "fill", name: "service",
                    action: .appendArguments(#"{"command":"tools"}"#, tokenCount: 10))))
                return
            }
            throw LanguageModelError.contextSizeExceeded(.init(contextSize: 100, tokenCount: 101,
                debugDescription: "full", metadata: [:]))
        }
    }
}
/// Loop closure (owner, 2026-10-18): with no request count or token total, a
/// context window that fills mid-turn ends the turn with a context-limit
/// failure; it does not hang or dispatch another request.
@Test func contextWindowFullMidTurnFailsWithContextLimit() async throws {
    let probe = ContextProbe()
    let engine = FoundationBackend(model: FullContextModel(probe: probe), contextTokens: 8192,
        modelName: "full-context-fixture", supportsTools: true)
    var input = ModelInput(); input.instructions = "fixture"; input.prompt = "fixture"
    do {
        try await engine.generateWithTools(input, maximumTokens: 2048, handler: { _ in
            ToolResult(status: .success, text: "fixture tools")
        }, snapshot: { _ in })
        Issue.record("full context completed")
    } catch let failure as BackendFailure { #expect(failure.reason == .contextLimit) }
    #expect(await probe.passes == 2)
}

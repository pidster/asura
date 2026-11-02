import Foundation
import FoundationModels
import Testing
@testable import HelperCore

/// CC4 prerequisite (context-composer.md, CC4 prerequisites; loop-closure-probes.md Q6):
/// the system model reports a mid-turn window overflow as its private
/// `GenerativeError`, not `contextSizeExceeded`. The helper maps both to
/// `context_limit`.

/// An executor that fails every dispatch with one fixed error.
private struct FailingModel: LanguageModel {
    let error: NSError
    var capabilities: LanguageModelCapabilities { .init([]) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Model = FailingModel
        typealias Configuration = Int
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: FailingModel,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            throw model.error
        }
    }
}

private func generateFailure(_ error: NSError) async -> Reason? {
    let engine = FoundationBackend(model: FailingModel(error: error), contextTokens: 8192,
        modelName: "failing", supportsTools: false)
    var input = ModelInput(); input.instructions = "test"; input.prompt = "test"
    do {
        try await engine.generate(input, snapshot: { _ in })
        return nil
    } catch let failure as BackendFailure {
        return failure.reason
    } catch {
        // Not a typed failure: never a context limit.
        return Reason.helperFailed
    }
}

@Test func generativeTooManyTokensIsContextLimit() async {
    let overflow = NSError(domain: generativeErrorDomain, code: generativeTooManyTokensCode)
    #expect(isContextOverflow(overflow))
    #expect(BoundedToolModel<FailingModel>.Executor.typed(overflow).reason == .contextLimit)
    #expect(await generateFailure(overflow) == .contextLimit)
    #expect(modelFailureDiagnostic(overflow).hasPrefix("asura_model_failure context_limit "))
}

@Test func otherGenerativeErrorsStayInternal() async {
    let other = NSError(domain: generativeErrorDomain, code: 4_010_000)
    #expect(!isContextOverflow(other))
    #expect(BoundedToolModel<FailingModel>.Executor.typed(other).reason == .internalError)
    #expect(await generateFailure(other) == .internalError)
    let unrelated = NSError(domain: "org.example.Other", code: generativeTooManyTokensCode)
    #expect(!isContextOverflow(unrelated))
}

@Test func sdkContextSizeExceededStaysContextLimit() {
    let sdk = LanguageModelError.contextSizeExceeded(.init(contextSize: 100, tokenCount: 101,
        debugDescription: "", metadata: [:]))
    #expect(isContextOverflow(sdk))
    #expect(BoundedToolModel<FailingModel>.Executor.typed(sdk).reason == .contextLimit)
}

// MARK: - Native (system model), opt-in: ASURA_TEST_NATIVE_SYSTEM=1

private let nativeSystem = ProcessInfo.processInfo.environment["ASURA_TEST_NATIVE_SYSTEM"] == "1"

/// Replaces each request's transcript with an oversized one that holds tool
/// calls and tool outputs, as a request late in a tool turn holds them.
/// Records the provider's error before the helper classifies it.
private final class Captured: @unchecked Sendable {
    private let lock = NSLock()
    private var value: (any Error)?
    func set(_ error: any Error) { lock.lock(); value = error; lock.unlock() }
    func get() -> (any Error)? { lock.lock(); defer { lock.unlock() }; return value }
}
private struct OversizedTurnModel<Base: LanguageModel>: LanguageModel {
    let base: Base
    let transcript: Transcript
    let captured: Captured
    var capabilities: LanguageModelCapabilities { base.capabilities }
    var executorConfiguration: Base.Executor.Configuration { base.executorConfiguration }
    struct Executor: LanguageModelExecutor {
        typealias Model = OversizedTurnModel<Base>
        typealias Configuration = Base.Executor.Configuration
        let underlying: Base.Executor
        init(configuration: Configuration) throws { underlying = try Base.Executor(configuration: configuration) }
        func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            var replaced = request
            replaced.transcript = model.transcript
            do { try await underlying.respond(to: replaced, model: model.base, streamingInto: channel) }
            catch { model.captured.set(error); throw error }
        }
    }
}
private struct ListTool: Tool {
    typealias Arguments = GeneratedContent
    let name = "exec_cmd"
    let description = "Run one shell command in the project and return its output."
    var parameters: GenerationSchema { Args.generationSchema }
    @Generable struct Args { @Guide(description: "Command passed to /bin/sh -c") var command: String }
    func call(arguments: GeneratedContent) async throws -> String { "" }
}

@Test(.enabled(if: nativeSystem)) func nativeSystemMidTurnOverflowIsContextLimit() async throws {
    let system = SystemLanguageModel.default
    guard case .available = system.availability else {
        Issue.record("ASURA_TEST_NATIVE_SYSTEM=1 but the system model is unavailable")
        return
    }
    let window = system.contextSize
    let output = (0..<160).map { "src/module_\($0).rs:\($0 * 7): fn helper_\($0)(x: u32) -> u32 { x + \($0) }" }
        .joined(separator: "\n")
    var entries: [Transcript.Entry] = [
        .instructions(.init(segments: [.text(.init(content: "You are a coding assistant."))],
            toolDefinitions: [Transcript.ToolDefinition(tool: ListTool())])),
        .prompt(.init(segments: [.text(.init(content: "Find parse_config."))]))]
    for step in 1...4 {
        let id = "call_\(step)"
        entries.append(.toolCalls(.init([.init(id: id, toolName: "exec_cmd",
            arguments: try GeneratedContent(json: #"{"command":"rg -n parse_config"}"#))])))
        entries.append(.toolOutput(.init(id: id, toolName: "exec_cmd", segments: [.text(.init(content: output))])))
    }
    let counted = try await system.tokenCount(for: entries)
    #expect(counted > window, "the fixture must exceed the \(window)-token window; it has \(counted)")
    let captured = Captured()
    let model = OversizedTurnModel(base: system, transcript: Transcript(entries: entries), captured: captured)
    // A tool turn: the request enables the project tools, as every
    // request after a tool call does. Without enabled tools the system
    // model reports the same overflow as `contextSizeExceeded`.
    let engine = FoundationBackend(model: model, contextTokens: UInt32(window),
        modelName: "system", supportsTools: true)
    var input = ModelInput(); input.instructions = "test"; input.prompt = "test"
    do {
        try await engine.generateWithTools(input, handler: { _ in
            Issue.record("an overflowing request reached a tool")
            return ToolResult(status: .unavailable, text: "")
        }, snapshot: { _ in })
        Issue.record("an oversized mid-turn request completed")
    } catch let failure as BackendFailure {
        #expect(failure.reason == .contextLimit)
    }
    // The provider error itself is the private overflow, not the SDK case.
    let raw = try #require(captured.get())
    #expect(!(raw is LanguageModelError), "the SDK now reports this overflow as LanguageModelError")
    #expect((raw as NSError).domain == generativeErrorDomain)
    #expect(isContextOverflow(raw))
}

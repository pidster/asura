import Foundation
import FoundationModels
import Testing
@testable import HelperCore

/// TCR-Q1 SDK qualification (deterministic, no model): a scripted executor
/// emits each malformed form against the real four tools, whose arguments are
/// `GeneratedContent`. The test records whether `Tool.call` was reached (the
/// service would then record the call) or the SDK failed first (proof limit,
/// owner decision 2). It asserts the measured table so an SDK change is seen.
private actor Q1Probe {
    var passes = 0
    var proposals: [ToolProposal] = []
    func next() -> Int { passes += 1; return passes }
    func received(_ proposal: ToolProposal) -> ToolResult {
        proposals.append(proposal)
        return ToolResult(status: .invalidArguments, text: "invalid_arguments: recorded")
    }
}
private struct Q1Model: LanguageModel {
    let probe: Q1Probe
    let name: String
    let arguments: String
    var capabilities: LanguageModelCapabilities { .init([.toolCalling]) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Model = Q1Model
        typealias Configuration = Int
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            if await model.probe.next() == 1 {
                await channel.send(.toolCalls(action: .toolCall(id: "q1", name: model.name,
                    action: .appendArguments(model.arguments, tokenCount: 4))))
            } else {
                await channel.send(.response(action: .appendText("done", tokenCount: 1)))
            }
        }
    }
}

struct Q1Case: Sendable, CustomStringConvertible {
    let form: String
    let name: String
    let arguments: String
    /// Measured on Xcode 27.1 SDK (macOS 27): the JSON value Tool.call received
    /// (nil: the SDK failed before Tool.call with a generated-content parse error).
    let reaches: String?
    var description: String { form }
}
private let q1Cases = [
    Q1Case(form: "missing_field", name: "project", arguments: #"{"path":"README.md"}"#,
        reaches: #"{"path":"README.md"}"#),
    Q1Case(form: "wrong_type", name: "project", arguments: #"{"command":"read_file","path":"README.md","offset":"zero"}"#,
        reaches: #"{"command":"read_file","path":"README.md","offset":"zero"}"#),
    Q1Case(form: "unknown_command", name: "project", arguments: #"{"command":"delete_file","path":"README.md"}"#,
        reaches: #"{"command":"delete_file","path":"README.md"}"#),
    Q1Case(form: "unexpected_field", name: "service", arguments: #"{"command":"status","extra":true}"#,
        reaches: #"{"command":"status","extra":true}"#),
    Q1Case(form: "invalid_json", name: "project", arguments: #"{"command":"read_file","path":"#,
        reaches: #"{"command":"read_file"}"#),
    Q1Case(form: "non_json_text", name: "project", arguments: "read the readme please", reaches: nil),
    Q1Case(form: "duplicate_key", name: "service", arguments: #"{"command":"status","command":"tools"}"#,
        reaches: #"{"command":"status"}"#),
    Q1Case(form: "unregistered_name", name: "delete_everything", arguments: #"{"path":"/"}"#, reaches: nil)
]

private func measure(_ value: Q1Case) async -> (reached: Bool, outcome: String, forwarded: String?) {
    let probe = Q1Probe()
    let backend = FoundationBackend(model: Q1Model(probe: probe, name: value.name, arguments: value.arguments),
        contextTokens: 8192, modelName: "q1-fixture", supportsTools: true)
    var input = ModelInput(); input.instructions = "q1"; input.prompt = "q1"
    var outcome = "complete"
    do {
        try await backend.generateWithTools(input, maximumTokens: 2048,
            handler: { await probe.received($0) }, snapshot: { _ in })
    } catch let error as BackendFailure {
        outcome = "backend_failure:\(error.reason)"
    } catch {
        outcome = modelFailureDiagnostic(error).trimmingCharacters(in: .whitespacesAndNewlines)
    }
    let proposals = await probe.proposals
    return (!proposals.isEmpty, outcome, proposals.first?.arguments)
}

@Test(arguments: q1Cases)
func tcrQ1MalformedFormsAgainstGeneratedContentTools(_ value: Q1Case) async throws {
    let result = await measure(value)
    print("TCR-Q1 form=\(value.form) reached_tool_call=\(result.reached) outcome=\(result.outcome) forwarded=\(result.forwarded ?? "-")")
    if let expected = value.reaches {
        // The SDK re-serializes: member order and whitespace differ from the
        // model text, an incomplete trailing member is dropped and a duplicate
        // key keeps its first value. Compare the JSON values.
        #expect(result.reached && result.outcome == "complete")
        let seen = try JSONSerialization.jsonObject(with: Data(try #require(result.forwarded).utf8)) as? NSDictionary
        let wanted = try JSONSerialization.jsonObject(with: Data(expected.utf8)) as? NSDictionary
        #expect(seen == wanted)
    } else {
        #expect(!result.reached)
        #expect(result.outcome.hasPrefix("asura_model_failure generated_content_invalid_json"))
    }
}

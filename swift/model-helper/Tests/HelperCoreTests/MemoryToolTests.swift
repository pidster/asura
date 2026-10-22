import Foundation
import FoundationModels
import Testing
@testable import HelperCore

private let noteID = "0123456789abcdef0123456789abcdef"

private actor DefinitionProbe {
    var names: [[String]] = []
    func record(_ value: [String]) { names.append(value) }
}
private struct DefinitionModel: LanguageModel {
    let probe: DefinitionProbe
    var capabilities: LanguageModelCapabilities { .init([.toolCalling]) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Configuration = Int
        typealias Model = DefinitionModel
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            await model.probe.record(request.enabledToolDefinitions.map(\.name))
            await channel.send(.response(action: .appendText("done", tokenCount: 1)))
        }
    }
}
@Test(arguments: ["system", "coreai:fixture", "mlx:fixture", "ollama:fixture"])
func disabledToolsNeverAdvertiseMemoryDefinitions(selector: String) async throws {
    let probe = DefinitionProbe()
    let backend = FoundationBackend(model: DefinitionModel(probe: probe), contextTokens: 4096,
        modelName: selector, supportsTools: true)
    var input = ModelInput(); input.instructions = "Answer briefly."; input.prompt = "Hello"
    try await backend.generate(input, snapshot: { _ in })
    #expect(await probe.names == [[]])
    try await backend.generateWithTools(input, handler: { _ in Issue.record("scripted model did not request a tool"); throw HelperError.unavailable },
        snapshot: { _ in })
    let names = await probe.names
    #expect(names.count == 2)
    #expect(Set(names[1]) == Set(["project", "memory", "service", "exec_cmd", "job"]))
}

@Test func generatedMemoryCreateSchemaAndDecodingPreserveWriteFields() throws {
    let tool = MemoryTool { _ in throw HelperError.unavailable }
    let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
    let properties = try #require(schema["properties"] as? [String: Any])
    #expect(Set(properties.keys) == Set(["command", "version", "after", "offset", "limit", "body", "source_version"]))
    if ProcessInfo.processInfo.environment["ASURA_TEST_DUMP_MEMORY_SCHEMA"] == "1" {
        let encoded = try JSONSerialization.data(withJSONObject: schema, options: [.prettyPrinted, .sortedKeys])
        print("ASURA_MEMORY_SCHEMA_BEGIN\n" + String(decoding: encoded, as: UTF8.self) + "\nASURA_MEMORY_SCHEMA_END")
    }
    // All three identifiers/text fields must permit generated strings, not null-only values.
    for name in ["body", "source_version", "version"] {
        let property = try #require(properties[name] as? [String: Any])
        #expect(property["type"] as? String == "string")
        #expect(property["enum"] == nil && property["const"] == nil)
    }
    #expect(try #require(schema["required"] as? [String]) == ["command"])
    let exact = " note é\n\0 body "
    for source in [nil, noteID] as [String?] {
        var object: [String: Any] = ["command": "create_note", "body": exact]
        if let source { object["source_version"] = source }
        let data = try JSONSerialization.data(withJSONObject: object)
        let decoded = try MemoryTool.Generation.Arguments(GeneratedContent(json: String(decoding: data, as: UTF8.self)))
        #expect(decoded.command == .create_note)
        #expect(decoded.body == exact)
        #expect(decoded.source_version == source)
        #expect(decoded.version == nil && decoded.after == nil && decoded.offset == nil && decoded.limit == nil)
    }
    let absent = try MemoryTool.Generation.Arguments(GeneratedContent(json: #"{"command":"create_note"}"#))
    #expect(absent.body == nil && absent.source_version == nil)
}

/// The SDK may correct a rejected memory call: the rejected proposal reaches
/// the service unread, the service text returns to the model, and the next
/// proposal is the corrected one. No token total or request count exists
/// (owner, 2026-10-18): the session's proposal limit bounds the loop, which
/// this probe applies as `HelperSession.requestTool` does.
private actor MemoryCorrectionProbe {
    var dispatches = 0
    var proposals: [ToolProposal] = []
    func next() -> Int { dispatches += 1; return dispatches }
    func received(_ proposal: ToolProposal) throws -> ToolResult {
        proposals.append(proposal)
        if proposals.count == Int(Limits.toolProposals) { throw BackendFailure(.outputLimit) }
        if proposal.arguments.contains("corrected note") {
            return ToolResult(status: .success, text: "created")
        }
        return ToolResult(status: .invalidArguments, text: "invalid_arguments: unexpected_field")
    }
}
private struct CorrectingMemoryModel: LanguageModel {
    let probe: MemoryCorrectionProbe
    let alwaysInvalid: Bool
    var capabilities: LanguageModelCapabilities { .init([.toolCalling]) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Model = CorrectingMemoryModel
        typealias Configuration = Int
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            let step = await model.probe.next()
            if step == 1 || model.alwaysInvalid {
                await channel.send(.toolCalls(action: .toolCall(id: "invalid-\(step)", name: "memory",
                    action: .appendArguments(#"{"command":"create_note","version":"unused","limit":4096}"#, tokenCount: 8))))
            } else if step == 2 {
                await channel.send(.toolCalls(action: .toolCall(id: "corrected", name: "memory",
                    action: .appendArguments(#"{"command":"create_note","body":"corrected note"}"#, tokenCount: 8))))
            } else {
                await channel.send(.response(action: .appendText("note created", tokenCount: 2)))
            }
        }
    }
}
@Test(arguments: [false, true])
func sdkCanCorrectMemoryArgumentsWithinTheProposalLimit(alwaysInvalid: Bool) async throws {
    let probe = MemoryCorrectionProbe()
    let backend = FoundationBackend(model: CorrectingMemoryModel(probe: probe, alwaysInvalid: alwaysInvalid),
        contextTokens: 8192, modelName: "correction-fixture", supportsTools: true)
    var input = ModelInput(); input.instructions = "Use memory."; input.prompt = "Store a note."
    do {
        try await backend.generateWithTools(input, handler: { try await probe.received($0) }, snapshot: { _ in })
        #expect(!alwaysInvalid)
    } catch let error as LanguageModelSession.ToolCallError {
        // The over-limit proposal ends the turn through the tool callback.
        #expect(alwaysInvalid && (error.underlyingError as? BackendFailure)?.reason == .outputLimit)
    }
    #expect(await probe.dispatches == (alwaysInvalid ? Int(Limits.toolProposals) : 3))
    let proposals = await probe.proposals
    #expect(proposals.allSatisfy { $0.name == "memory" && $0.source == .generatedContent })
    // The rejected text reaches the service exactly as generated (no Swift parsing).
    let first = try JSONSerialization.jsonObject(with: Data(try #require(proposals.first).arguments.utf8)) as? NSDictionary
    #expect(first == ["command": "create_note", "version": "unused", "limit": 4096] as NSDictionary)
    #expect(proposals.count == (alwaysInvalid ? Int(Limits.toolProposals) : 2))
}

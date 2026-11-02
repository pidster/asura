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
    let proposals = try await backend.generateWithTools(input, thinking: .none, request: 1, snapshot: { _ in })
    #expect(proposals.isEmpty)
    let names = await probe.names
    #expect(names.count == 2)
    #expect(Set(names[1]) == Set(["project", "memory", "service", "exec_cmd", "job"]))
}

@Test func generatedMemoryCreateSchemaAndDecodingPreserveWriteFields() throws {
    let tool = MemoryTool()
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

import Foundation
import FoundationModels
import Testing
@testable import HelperCore

private actor ForwardedCalls {
    var calls: [ToolProposal] = []
    func receive(_ proposal: ToolProposal) -> ToolResult {
        calls.append(proposal)
        return ToolResult(status: .success, text: "canonical result")
    }
}

/// TCR-U15: each grouped tool sends its own name and the exact `jsonString` of
/// the generated content, with source generated_content, and returns the
/// service text. Nothing is read, defaulted or validated in Swift.
@Test func groupedToolsForwardNameAndExactJSONString() async throws {
    let calls = ForwardedCalls()
    let handler: ToolHandler = { await calls.receive($0) }
    let inputs: [(any Tool, String)] = [
        (ProjectTool(handler: handler), #"{"command":"read_file","path":"file"}"#),
        (ProjectTool(handler: handler), #"{"command":"list_directory","offset":0}"#),
        (MemoryTool(handler: handler), #"{"command":"create_note","body":"  note\nbody é\u0000 ","source_version":"0123456789abcdef0123456789abcdef"}"#),
        (MemoryTool(handler: handler), #"{"command":"get_note"}"#),
        (ServiceTool(handler: handler), #"{"command":"tools","limit":1}"#),
        (ExecCmdTool(handler: handler), #"{"command":"printf fixture","cwd":"src/../other"}"#)
    ]
    var expected: [ToolProposal] = []
    for (tool, json) in inputs {
        let content = try GeneratedContent(json: json)
        let output = try await forward(tool, content)
        #expect(output == "canonical result\n[truncated: false]")
        expected.append(ToolProposal(name: tool.name, arguments: content.jsonString, source: .generatedContent))
    }
    #expect(await calls.calls == expected)
    #expect(await calls.calls.map(\.name) == ["project", "project", "memory", "memory", "service", "exec_cmd"])
}

private func forward(_ tool: any Tool, _ content: GeneratedContent) async throws -> String {
    switch tool {
    case let value as ProjectTool: return try await value.call(arguments: content)
    case let value as MemoryTool: return try await value.call(arguments: content)
    case let value as ServiceTool: return try await value.call(arguments: content)
    case let value as ExecCmdTool: return try await value.call(arguments: content)
    default: throw HelperError.protocolFault
    }
}

/// TCR-U15: a rejection returns `Tool failed: <status>`, a newline and the
/// service text (TCR-D18), so the model can correct the call.
@Test func rejectedResultReturnsServiceText() async throws {
    let tool = ProjectTool { _ in ToolResult(status: .invalidArguments, text: "invalid_arguments: missing_field") }
    let output = try await tool.call(arguments: GeneratedContent(json: #"{"command":"read_file"}"#))
    #expect(output == "Tool failed: invalidArguments\ninvalid_arguments: missing_field")
    #expect(ToolResult(status: .limit, text: "").rendered == "Tool failed: limit")
    #expect(ToolResult(status: .timeout, text: "stdout: partial", truncated: true).rendered
        == "Tool failed: timeout\nstdout: partial\n[truncated: true]")
}

@Test func groupedSchemasAdvertiseOnlyTypedCommands() throws {
    let tools = makeProjectTools { _ in throw HelperError.unavailable }
    #expect(tools.map(\.name) == ["project", "memory", "service", "exec_cmd"])
    let commands = [["read_file", "list_directory"], ["list_notes", "get_note", "note_sources", "create_note"], ["status", "tools", "audit"]]
    for (tool, expected) in zip(tools.prefix(3), commands) {
        let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
        let properties = try #require(schema["properties"] as? [String: Any])
        let required = try #require(schema["required"] as? [String])
        #expect(required == ["command"])
        let command = try #require(properties["command"] as? [String: Any])
        #expect(Set(try #require(command["enum"] as? [String])) == Set(expected))
    }
    let exec = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tools[3].parameters)) as? [String: Any])
    #expect(Set((try #require(exec["properties"] as? [String: Any])).keys) == ["command", "cwd"])
    #expect(Set(exec["required"] as? [String] ?? []) == ["command"])
}

/// The schemas are the earlier typed generation schemas, unchanged: the
/// grouped tools' parameters equal the schema of their generation type.
@Test func groupedParametersKeepTheirGenerationSchemas() throws {
    let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
    let handler: ToolHandler = { _ in throw HelperError.unavailable }
    #expect(try encoder.encode(ProjectTool(handler: handler).parameters)
        == encoder.encode(ProjectTool.Generation.Arguments.generationSchema))
    #expect(try encoder.encode(MemoryTool(handler: handler).parameters)
        == encoder.encode(MemoryTool.Generation.Arguments.generationSchema))
    #expect(try encoder.encode(ServiceTool(handler: handler).parameters)
        == encoder.encode(ServiceTool.Generation.Arguments.generationSchema))
    #expect(try encoder.encode(ExecCmdTool(handler: handler).parameters)
        == encoder.encode(ExecCmdTool.Generation.Arguments.generationSchema))
    #expect(projectToolSchemaBytes() > 0)
}

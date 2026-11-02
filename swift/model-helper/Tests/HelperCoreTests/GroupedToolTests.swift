import Foundation
import FoundationModels
import Testing
@testable import HelperCore

/// CC4: every grouped tool is a stop tool. Its call throws `StopForCaller`
/// whatever the arguments, so the helper never runs a tool; the service
/// renders every result (context-composer.md, CC4).
@Test func groupedToolsAreStopToolsAndNeverRun() async throws {
    let inputs: [(any Tool, String)] = [
        (ProjectTool(), #"{"command":"read_file","path":"file"}"#),
        (MemoryTool(), #"{"command":"create_note","body":"  note\nbody é\u0000 "}"#),
        (ServiceTool(), #"{"command":"tools","limit":1}"#),
        (ExecCmdTool(), #"{"command":"printf fixture","cwd":"src/../other","yield_seconds":31}"#),
        (JobTool(), #"{"command":"list","before":0,"text":"unexpected"}"#)
    ]
    for (tool, json) in inputs {
        await #expect(throws: StopForCaller.self) { _ = try await forward(tool, GeneratedContent(json: json)) }
    }
}

private func forward(_ tool: any Tool, _ content: GeneratedContent) async throws -> String {
    switch tool {
    case let value as ProjectTool: return try await value.call(arguments: content)
    case let value as MemoryTool: return try await value.call(arguments: content)
    case let value as ServiceTool: return try await value.call(arguments: content)
    case let value as ExecCmdTool: return try await value.call(arguments: content)
    case let value as JobTool: return try await value.call(arguments: content)
    default: throw HelperError.protocolFault
    }
}

@Test func groupedSchemasAdvertiseOnlyTypedCommands() throws {
    let tools = makeProjectTools()
    #expect(tools.map(\.name) == ["project", "memory", "service", "exec_cmd", "job"])
    let commands = [["read_file", "list_directory"], ["list_notes", "get_note", "note_sources", "create_note"], ["status", "tools", "audit"],
        [], ["list", "status", "read", "wait", "cancel", "note"]]
    for (tool, expected) in zip(tools, commands) where !expected.isEmpty {
        let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
        let properties = try #require(schema["properties"] as? [String: Any])
        let required = try #require(schema["required"] as? [String])
        #expect(required == ["command"])
        let command = try #require(properties["command"] as? [String: Any])
        #expect(Set(try #require(command["enum"] as? [String])) == Set(expected))
    }
    let exec = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tools[3].parameters)) as? [String: Any])
    #expect(Set((try #require(exec["properties"] as? [String: Any])).keys) == ["command", "cwd", "yield_seconds"])
    #expect(Set(exec["required"] as? [String] ?? []) == ["command"])
}

/// The schemas are the earlier typed generation schemas, unchanged: the
/// grouped tools' parameters equal the schema of their generation type.
@Test func groupedParametersKeepTheirGenerationSchemas() throws {
    let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
    #expect(try encoder.encode(ProjectTool().parameters)
        == encoder.encode(ProjectTool.Generation.Arguments.generationSchema))
    #expect(try encoder.encode(MemoryTool().parameters)
        == encoder.encode(MemoryTool.Generation.Arguments.generationSchema))
    #expect(try encoder.encode(ServiceTool().parameters)
        == encoder.encode(ServiceTool.Generation.Arguments.generationSchema))
    #expect(try encoder.encode(ExecCmdTool().parameters)
        == encoder.encode(ExecCmdTool.Generation.Arguments.generationSchema))
    #expect(try encoder.encode(JobTool().parameters)
        == encoder.encode(JobTool.Generation.Arguments.generationSchema))
    #expect(projectToolSchemaBytes() > 0)
}

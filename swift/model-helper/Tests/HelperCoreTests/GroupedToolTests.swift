import Foundation
import FoundationModels
import Testing
@testable import HelperCore

private actor GroupedCalls {
    var calls: [String] = []
    func receive(_ arguments: ToolArguments) -> ToolResult {
        switch arguments {
        case .readFile(let path, let offset, let limit): calls.append("read:\(path):\(offset):\(limit)")
        case .listDirectory(let path): calls.append("list:\(path)")
        case .memoryListNotes(let after, let limit): calls.append("notes:\(after ?? "none"):\(limit)")
        case .memoryGetNote(let version, let offset, let limit): calls.append("note:\(version):\(offset):\(limit)")
        case .memoryNoteSources(let version): calls.append("sources:\(version)")
        case .observeStatus: calls.append("status")
        case .listTools: calls.append("tools")
        case .readAudit(let limit): calls.append("audit:\(limit)")
        default: calls.append("unexpected")
        }
        return ToolResult(status: .success, text: "canonical result")
    }
}
@Test func groupedCommandsUseCanonicalOperationsAndDefaults() async throws {
    let calls = GroupedCalls()
    let handler: ToolHandler = { await calls.receive($0) }
    let project = ProjectTool(handler: handler)
    _ = try await project.call(arguments: .init(command: .read_file, path: "file", offset: nil, limit: nil))
    _ = try await project.call(arguments: .init(command: .list_directory, path: nil, offset: nil, limit: nil))
    let memory = MemoryTool(handler: handler)
    _ = try await memory.call(arguments: .init(command: .list_notes, version: nil, after: nil, offset: nil, limit: nil))
    _ = try await memory.call(arguments: .init(command: .get_note, version: "note", after: nil, offset: nil, limit: nil))
    _ = try await memory.call(arguments: .init(command: .note_sources, version: "note", after: nil, offset: nil, limit: nil))
    let service = ServiceTool(handler: handler)
    for command in [ServiceTool.Command.status, .tools, .audit] {
        _ = try await service.call(arguments: .init(command: command, limit: nil))
    }
    #expect(await calls.calls == ["read:file:0:4096", "list:.", "notes:none:8", "note:note:0:4096", "sources:note", "status", "tools", "audit:16"])
}
@Test func groupedInvalidCombinationsNeverReachService() async throws {
    let handler: ToolHandler = { _ in Issue.record("invalid command reached service"); throw HelperError.protocolFault }
    let memory = MemoryTool(handler: handler)
    for args in [
        MemoryTool.Arguments(command: .list_notes, version: "wrong", after: nil, offset: nil, limit: nil),
        .init(command: .get_note, version: nil, after: nil, offset: nil, limit: nil),
        .init(command: .get_note, version: "note", after: "wrong", offset: nil, limit: nil),
        .init(command: .note_sources, version: "note", after: nil, offset: 0, limit: nil),
        .init(command: .get_note, version: "note", after: nil, offset: -1, limit: nil)
    ] { await #expect(throws: BackendFailure.self) { try await memory.call(arguments: args) } }
    let service = ServiceTool(handler: handler)
    await #expect(throws: BackendFailure.self) { try await service.call(arguments: .init(command: .tools, limit: 1)) }
    let project = ProjectTool(handler: handler)
    await #expect(throws: BackendFailure.self) { try await project.call(arguments: .init(command: .list_directory, path: nil, offset: 0, limit: nil)) }
    await #expect(throws: BackendFailure.self) { try await project.call(arguments: .init(command: .read_file, path: nil, offset: nil, limit: nil)) }
}
@Test func groupedSchemasAdvertiseOnlyTypedCommands() throws {
    let tools = makeProjectTools { _ in throw HelperError.unavailable }
    #expect(tools.map(\.name) == ["project", "memory", "service", "shell"])
    let commands = [["read_file", "list_directory"], ["list_notes", "get_note", "note_sources", "create_note"], ["status", "tools", "audit"]]
    for (tool, expected) in zip(tools.prefix(3), commands) {
        let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
        let properties = try #require(schema["properties"] as? [String: Any])
        let required = try #require(schema["required"] as? [String])
        #expect(required == ["command"])
        let command = try #require(properties["command"] as? [String: Any])
        #expect(Set(try #require(command["enum"] as? [String])) == Set(expected))
    }
}

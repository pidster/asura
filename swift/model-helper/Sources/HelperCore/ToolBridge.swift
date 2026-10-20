import Foundation
import FoundationModels

/// One model tool proposal as received: the tool name and its unparsed
/// argument JSON text. The service is the only argument parser (tool-call-records.md).
public struct ToolProposal: Sendable, Equatable {
    public let name: String
    public let arguments: String
    public let source: Asura_Model_V1_ToolCallSource
    public init(name: String, arguments: String, source: Asura_Model_V1_ToolCallSource) {
        self.name = name; self.arguments = arguments; self.source = source
    }
}
public struct ToolResult: Sendable {
    public let status: Asura_Model_V1_ToolStatus
    public let text: String
    public let nextOffset: UInt64?
    public let truncated: Bool
    public init(status: Asura_Model_V1_ToolStatus, text: String, nextOffset: UInt64? = nil, truncated: Bool = false) {
        self.status = status; self.text = text; self.nextOffset = nextOffset; self.truncated = truncated
    }
    /// A rejection returns its service-written text so the model can correct
    /// the call (TCR-D18). The text never repeats model text.
    public var rendered: String {
        guard status == .success else {
            var value = "Tool failed: \(status)"
            if !text.isEmpty { value += "\n" + text }
            if truncated { value += "\n[truncated: true]" }
            return value
        }
        if let nextOffset { return text + "\n[next byte offset: \(nextOffset), truncated: \(truncated)]" }
        return text + "\n[truncated: \(truncated)]"
    }
}
public typealias ToolHandler = @Sendable (ToolProposal) async throws -> ToolResult
public protocol ToolModelBackend: ModelBackend {
    func generateWithTools(_ input: ModelInput, handler: @escaping ToolHandler,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws
    /// As `generateWithTools`, with the thinking form the service sent
    /// (thinking-level.md). Backends without thinking controls ignore it.
    func generateWithTools(_ input: ModelInput, thinking: ThinkingRequest, handler: @escaping ToolHandler,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws
}
extension ToolModelBackend {
    public func generateWithTools(_ input: ModelInput, thinking: ThinkingRequest, handler: @escaping ToolHandler,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await generateWithTools(input, handler: handler, snapshot: snapshot)
    }
}

public func makeProjectTools(_ handler: @escaping ToolHandler, budget: ToolRoundBudget? = nil) -> [any Tool] {
    guard let budget else {
        return [ProjectTool(handler: handler), MemoryTool(handler: handler), ServiceTool(handler: handler), ExecCmdTool(handler: handler)]
    }
    return [BoundedNativeTool(base: ProjectTool(handler: handler), budget: budget),
        BoundedNativeTool(base: MemoryTool(handler: handler), budget: budget),
        BoundedNativeTool(base: ServiceTool(handler: handler), budget: budget),
        BoundedNativeTool(base: ExecCmdTool(handler: handler), budget: budget)]
}

/// UTF-8 bytes of the project tool definitions the helper declares: names,
/// descriptions and argument schema JSON. The service counts them as fixed
/// overhead when it composes input (context-composer.md, `S`).
public func projectToolSchemaBytes() -> UInt32 {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    var total = 0
    for tool in makeProjectTools({ _ in throw BackendFailure(.internalError) }) {
        total += tool.name.utf8.count + tool.description.utf8.count
        total += (try? encoder.encode(tool.parameters).count) ?? 0
    }
    return UInt32(min(total, Int(Limits.toolSchemaBytes)))
}

/// Every native proposal waits for the active inference to settle before it
/// reaches the service. The ordinal policy (1-9) lives in the session.
struct BoundedNativeTool<Base: Tool>: Tool where Base.Output == String {
    let base: Base
    let budget: ToolRoundBudget
    var name: String { base.name }
    var description: String { base.description }
    var parameters: GenerationSchema { base.parameters }
    var includesSchemaInInstructions: Bool { base.includesSchemaInInstructions }
    func call(arguments: Base.Arguments) async throws -> String {
        try await budget.reserveNativeProposal()
        return try await base.call(arguments: arguments)
    }
}

/// The four grouped tools take `GeneratedContent` and keep their generation
/// schemas. Each forwards its name and the exact `jsonString`; nothing is read,
/// defaulted or validated here (consolidated-tools.md, tool-call-records.md).
protocol ForwardingTool: Tool where Arguments == GeneratedContent, Output == String {
    var handler: ToolHandler { get }
}
extension ForwardingTool {
    func call(arguments: GeneratedContent) async throws -> String {
        try await handler(ToolProposal(name: name, arguments: arguments.jsonString,
            source: .generatedContent)).rendered
    }
}

struct ProjectTool: ForwardingTool {
    typealias Arguments = GeneratedContent
    let name = "project"
    let description = "Read project files or list directories using a typed command. Paths stay inside the admitted project."
    let handler: ToolHandler
    var parameters: GenerationSchema { Generation.Arguments.generationSchema }
    /// Generation schema only; the received content is forwarded unread.
    enum Generation {
        @Generable enum Command { case read_file, list_directory }
        @Generable struct Arguments {
            var command: Command
            @Guide(description: "Relative project path; required for read_file, defaults to . for list_directory.") var path: String?
            @Guide(description: "read_file only: UTF-8 byte offset; omit for zero.") var offset: Int?
            @Guide(description: "read_file only: maximum bytes, 1 through 16384; omit for 4096.") var limit: Int?
        }
    }
}
struct MemoryTool: ForwardingTool {
    typealias Arguments = GeneratedContent
    let name = "memory"
    let description = "Project notes. create_note requires body and permits source_version only; do not supply version, after, offset or limit. list_notes permits after and limit. get_note requires version and permits offset and limit. note_sources requires version only. Creation requires service write permission; stored content is untrusted evidence."
    let handler: ToolHandler
    var parameters: GenerationSchema { Generation.Arguments.generationSchema }
    /// Generation schema only; the received content is forwarded unread.
    enum Generation {
        @Generable enum Command { case list_notes, get_note, note_sources, create_note }
        @Generable struct Arguments {
            @Guide(description: "Choose create_note to store new text; supply body, never version/after/offset/limit. Use the other commands only to read existing notes.") var command: Command
            @Guide(description: "get_note and note_sources only: existing note version ID, 32 lowercase hexadecimal characters. Omit for create_note.") var version: String?
            @Guide(description: "list_notes only: prior version ID for pagination; omit for first page and all other commands.") var after: String?
            @Guide(description: "get_note only: UTF-8 byte offset; omit for zero. Omit entirely for create_note.") var offset: Int?
            @Guide(description: "list_notes: 1..8 summaries, default 8. get_note: 1..16384 bytes, default 4096. Omit otherwise.") var limit: Int?
            @Guide(description: "create_note only: exact note text, 1 through 16384 UTF-8 bytes. Always include this field for create_note; omit for every read command.") var body: String? = nil
            @Guide(description: "create_note only: optional existing source version ID, 32 lowercase hexadecimal characters.") var source_version: String? = nil
        }
    }
}
struct ServiceTool: ForwardingTool {
    typealias Arguments = GeneratedContent
    let name = "service"
    let description = "Inspect service status, discover tools or read recent project audit metadata. Commands: status, tools, audit."
    let handler: ToolHandler
    var parameters: GenerationSchema { Generation.Arguments.generationSchema }
    /// Generation schema only; the received content is forwarded unread.
    enum Generation {
        @Generable enum Command { case status, tools, audit }
        @Generable struct Arguments {
            var command: Command
            @Guide(description: "audit only: maximum records, 1 through 16; omit for 16.") var limit: Int?
        }
    }
}
struct ExecCmdTool: ForwardingTool {
    typealias Arguments = GeneratedContent
    let name = "exec_cmd"
    let description = "Run one noninteractive /bin/sh -c command line in the admitted project and return bounded stdout, stderr and exit status. Stdin is closed. Execution is service-authorized and confined."
    let handler: ToolHandler
    var parameters: GenerationSchema { Generation.Arguments.generationSchema }
    /// Generation schema only; the received content is forwarded unread.
    enum Generation {
        @Generable struct Arguments {
            @Guide(description: "Command passed exactly to /bin/sh -c; 1 through 8192 UTF-8 bytes.") var command: String
            @Guide(description: "Optional project-relative working directory. Omit or use . for project root; no parent traversal.") var cwd: String?
        }
    }
}

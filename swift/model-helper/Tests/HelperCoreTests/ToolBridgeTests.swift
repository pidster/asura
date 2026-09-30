import Darwin
import Foundation
import Testing
@testable import HelperCore

private struct ToolScript: ToolModelBackend {
    let arguments: ToolArguments
    let source: ContextCapacitySource
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096,
        reportedContextTokens: 4096, contextSource: source, supportsTools: true,
        capabilityProfile: try! CapabilityProfile(mask: 1, provenance: .runtime)) }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        throw HelperError.unavailable
    }
    func generateWithTools(_ input: ModelInput, maximumTokens: UInt32, handler: @escaping ToolHandler,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await snapshot(Snapshot(text: "", inputContext: (123, 4096)))
        let result = try await handler(arguments)
        try await snapshot(Snapshot(text: result.text, usageTokens: 1))
    }
}

@Test(arguments: [false, true], ["system", "coreai:fixture", "mlx:fixture", "ollama:fixture"])
func toolCallbackWaitsForServiceResultOrCancellation(cancel: Bool, selector: String) async throws {
    try await toolCallbackJourney(cancel: cancel, selector: selector, arguments: .readFile(path: "README.md", offset: 0, limit: 1024))
}

@Test(arguments: ["system", "coreai:fixture", "mlx:fixture", "ollama:fixture"])
func memoryCallbacksUseSameServiceResultAndCancellationBridge(selector: String) async throws {
    for cancel in [false, true] {
        for arguments in [.memoryCreateNote(body: " exact\nbody ", sourceVersion: nil),
            .memoryCreateNote(body: "", sourceVersion: "invalid"),
            .memoryCreateNote(body: "note", sourceVersion: "0123456789abcdef0123456789abcdef"),
            .memoryListNotes(after: nil, limit: 8),
            .memoryListNotes(after: "", limit: 0),
            .memoryGetNote(version: "0123456789abcdef0123456789abcdef", offset: 4, limit: 32),
            .memoryNoteSources(version: "0123456789abcdef0123456789abcdef"), .listTools, .readAudit(limit: 16), .shell(command: "printf fixture", cwd: ".", timeoutSeconds: 30)] as [ToolArguments] {
            try await toolCallbackJourney(cancel: cancel, selector: selector, arguments: arguments)
        }
    }
}

private func toolCallbackJourney(cancel: Bool, selector: String, arguments: ToolArguments) async throws {
    var fds: [Int32] = [-1, -1]
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
    let helper = try Transport(fd: fds[0]); let service = try Transport(fd: fds[1])
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, factory: { selected, assets, endpoint, capabilities in
        #expect(selected == selector && assets == "/tmp/model-fixture" && endpoint == nil && capabilities == nil)
        let source: ContextCapacitySource
        if selected.hasPrefix("coreai:") { source = .coreai }
        else if selected.hasPrefix("mlx:") { source = .mlx }
        else if selected.hasPrefix("ollama:") { source = .ollama }
        else { source = .system }
        return ToolScript(arguments: arguments, source: source)
    }, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    defer { running.cancel() }
    func scoped(_ body: Envelope.OneOf_Body) -> Envelope {
        var value = Envelope(); value.operationID = Data(repeating: 7, count: 16)
        value.generation = 1; value.body = body; return value
    }
    var replies = service.frames.makeAsyncIterator()
    var hello = Asura_Model_V1_Hello()
    hello.buildID = identity; hello.schemaDigest = identity; hello.maxFrameBytes = 65_536
    hello.availability = .unknown; hello.capabilities = 0; hello.reason = .none
    hello.selectedModel = selector; hello.assetRoot = "/tmp/model-fixture"
    var helloEnvelope = Envelope(); helloEnvelope.body = .hello(hello)
    try await service.send(helloEnvelope)
    let greeting = try Wire.decode(#require(try await replies.next()), from: .helper)
    #expect(greeting.hello.selectedModel == selector && greeting.hello.capabilities == 3)
    var input = ModelInput(); input.instructions = ""; input.prompt = "read file"
    let data = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = selector; begin.inputBytes = UInt64(data.count)
    begin.deadlineRemainingMs = 2_000; begin.maxResponseTokens = 512; begin.enableProjectTools = true
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = data
    try await service.send(scoped(.chunk(chunk)))
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(data.count)
    try await service.send(scoped(.inputEnd(end))); _ = try await replies.next()
    var credit = Asura_Model_V1_Credit(); credit.transferID = 2; credit.direction = .output
    credit.acceptedBytes = 0; credit.grantedBytes = 65_536
    try await service.send(scoped(.start(.init())))
    try await service.send(scoped(.credit(credit)))
    let measurement = try Wire.decode(#require(try await replies.next()), from: .helper)
    #expect(measurement.contextMeasured.inputTokens == 123)
    #expect(measurement.contextMeasured.capacityTokens == 4096)
    let call = try Wire.decode(#require(try await replies.next()), from: .helper)
    #expect(call.toolCall.ordinal == 1)
    switch arguments {
    case .shell(let command, let cwd, let timeout):
        #expect(call.toolCall.shell.command == command)
        #expect(call.toolCall.shell.hasCwd == (cwd != nil) && call.toolCall.shell.cwd == (cwd ?? ""))
        #expect(call.toolCall.shell.timeoutSeconds == timeout)
    case .readAudit(let limit):
        #expect(call.toolCall.readAudit.hasLimit && call.toolCall.readAudit.limit == limit)
    case .listTools:
        guard case .listTools = call.toolCall.arguments else { Issue.record("wrong inventory argument variant"); return }
    case .readFile(let path, let offset, let limit):
        #expect(call.toolCall.readFile.path == path && call.toolCall.readFile.offset == offset && call.toolCall.readFile.limit == limit)
    case .memoryListNotes(let after, let limit):
        #expect(call.toolCall.memoryListNotes.hasAfter == (after != nil))
        #expect(call.toolCall.memoryListNotes.after == (after ?? "") && call.toolCall.memoryListNotes.limit == limit)
    case .memoryGetNote(let version, let offset, let limit):
        #expect(call.toolCall.memoryGetNote.version == version && call.toolCall.memoryGetNote.offset == offset && call.toolCall.memoryGetNote.limit == limit)
    case .memoryCreateNote(let body, let source):
        #expect(call.toolCall.memoryCreateNote.hasBody && call.toolCall.memoryCreateNote.body == body)
        #expect(call.toolCall.memoryCreateNote.hasSourceVersion == (source != nil))
        #expect(call.toolCall.memoryCreateNote.sourceVersion == (source ?? ""))
    case .memoryNoteSources(let version): #expect(call.toolCall.memoryNoteSources.version == version)
    default: Issue.record("unexpected scripted tool")
    }
    if cancel {
        var message = Asura_Model_V1_Cancel(); message.reason = .user
        try await service.send(scoped(.cancel(message)))
    } else {
        var result = Asura_Model_V1_ToolResult(); result.ordinal = 1; result.status = .success
        result.text = "observed file content"; result.nextOffset = 21; result.truncated = false
        try await service.send(scoped(.toolResult(result)))
    }
    var observed = Data(); var terminal: Asura_Model_V1_Terminal?
    while let frame = try await replies.next() {
        let message = try Wire.decode(frame, from: .helper)
        switch message.body {
        case .chunk(let value): observed.append(value.data)
        case .terminal(let value): terminal = value
        default: break
        }
        if terminal != nil { break }
    }
    #expect(terminal?.outcome == (cancel ? .cancelled : .complete))
    #expect(cancel ? observed.isEmpty : String(data: observed, encoding: .utf8) == "observed file content")
    service.close(); helper.close(); await running.value
}

@Test func toolWireRejectsWrongDirectionsAndUnknownNestedFields() throws {
    var args = Asura_Model_V1_ProjectReadFile(); args.path = "file"; args.offset = 0; args.limit = 1024
    var call = Asura_Model_V1_ToolCall(); call.ordinal = 1; call.readFile = args
    var envelope = Envelope(); envelope.operationID = Data(repeating: 1, count: 16)
    envelope.generation = 1; envelope.body = .toolCall(call)
    try Wire.validate(envelope, from: .helper)
    #expect(throws: (any Error).self) { try Wire.validate(envelope, from: .service) }
    var raw = try args.serializedData(); raw.append(contentsOf: [0xa0, 0x06, 0x01])
    call.readFile = try Asura_Model_V1_ProjectReadFile(serializedBytes: raw)
    envelope.body = .toolCall(call)
    #expect(throws: (any Error).self) { _ = try Wire.decode(envelope.serializedData(), from: .helper) }
}

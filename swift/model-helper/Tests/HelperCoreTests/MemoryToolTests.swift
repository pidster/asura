import Foundation
import FoundationModels
import Testing
@testable import HelperCore

private let noteID = "0123456789abcdef0123456789abcdef"
private func memoryEnvelope(_ arguments: Asura_Model_V1_ToolCall.OneOf_Arguments) -> Envelope {
    var call = Asura_Model_V1_ToolCall(); call.ordinal = 1; call.arguments = arguments
    var message = Envelope(); message.operationID = Data(repeating: 1, count: 16)
    message.generation = 1; message.body = .toolCall(call)
    return message
}

@Test func memoryWirePresenceBoundsAndSemanticRejections() throws {
    var list = Asura_Model_V1_MemoryListNotes(); list.limit = 8
    try Wire.validate(memoryEnvelope(.memoryListNotes(list)), from: .helper)
    list.after = ""; list.limit = 0
    try Wire.validate(memoryEnvelope(.memoryListNotes(list)), from: .helper)
    #expect(list.hasAfter)
    list.after = String(repeating: "é", count: 512); list.limit = UInt32.max
    try Wire.validate(memoryEnvelope(.memoryListNotes(list)), from: .helper)
    list.after += "x"
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryListNotes(list)), from: .helper) }
    list.clearAfter(); list.clearLimit()
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryListNotes(list)), from: .helper) }

    var get = Asura_Model_V1_MemoryGetNote(); get.version = ""; get.offset = UInt64.max; get.limit = UInt32.max
    try Wire.validate(memoryEnvelope(.memoryGetNote(get)), from: .helper)
    get.clearOffset()
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryGetNote(get)), from: .helper) }
    get.offset = 0; get.clearLimit()
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryGetNote(get)), from: .helper) }
    get.limit = 1; get.clearVersion()
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryGetNote(get)), from: .helper) }
    get.version = String(repeating: "x", count: 1025)
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryGetNote(get)), from: .helper) }

    var sources = Asura_Model_V1_MemoryNoteSources(); sources.version = ""
    try Wire.validate(memoryEnvelope(.memoryNoteSources(sources)), from: .helper)
    sources.clearVersion()
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryNoteSources(sources)), from: .helper) }
    sources.version = String(repeating: "x", count: 1025)
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryNoteSources(sources)), from: .helper) }
}

@Test func memoryWireRejectsWrongDirectionAndUnknownNestedFields() throws {
    var list = Asura_Model_V1_MemoryListNotes(); list.limit = 8
    var get = Asura_Model_V1_MemoryGetNote(); get.version = noteID; get.offset = 0; get.limit = 16_384
    var sources = Asura_Model_V1_MemoryNoteSources(); sources.version = noteID
    for arguments in [.memoryListNotes(list), .memoryGetNote(get), .memoryNoteSources(sources)] as [Asura_Model_V1_ToolCall.OneOf_Arguments] {
        let message = memoryEnvelope(arguments)
        let data = try message.serializedData()
        #expect(try Wire.decode(data, from: .helper) == message)
        #expect(throws: HelperError.self) { try Wire.decode(data, from: .service) }
    }
    let unknown = Data([0xa0, 0x06, 0x01])
    list = try .init(serializedBytes: list.serializedData() + unknown)
    get = try .init(serializedBytes: get.serializedData() + unknown)
    sources = try .init(serializedBytes: sources.serializedData() + unknown)
    for arguments in [.memoryListNotes(list), .memoryGetNote(get), .memoryNoteSources(sources)] as [Asura_Model_V1_ToolCall.OneOf_Arguments] {
        #expect(throws: HelperError.self) { try Wire.decode(memoryEnvelope(arguments).serializedData(), from: .helper) }
    }
}

@Test func nativeMemorySchemasAndBoundedInvalidArgumentsUseSharedHandler() async throws {
    let handler: ToolHandler = { arguments in
        switch arguments {
        case .memoryListNotes(let after, let limit): #expect(after == "" && limit == 0)
        case .memoryGetNote(let version, let offset, let limit): #expect(version == "invalid" && offset == 0 && limit == 0)
        case .memoryNoteSources(let version): #expect(version == "")
        default: Issue.record("unexpected native memory argument")
        }
        return ToolResult(status: .invalidArguments, text: "", truncated: false)
    }
    #expect(makeProjectTools(handler).map(\.name) == ["project", "memory", "service", "shell"])
    let tools: [any Tool] = [MemoryListNotesTool(handler: handler), MemoryGetNoteTool(handler: handler), MemoryNoteSourcesTool(handler: handler)]
    for tool in tools {
        let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
        let properties = try #require(schema["properties"] as? [String: Any])
        let required = try #require(schema["required"] as? [String])
        #expect(!properties.keys.contains("project") && !properties.keys.contains("path"))
        switch tool.name {
        case "memory_list_notes": #expect(Set(properties.keys) == Set(["after", "limit"]) && required == ["limit"])
        case "memory_get_note": #expect(Set(required) == Set(["version", "offset", "limit"]))
        case "memory_note_sources": #expect(required == ["version"])
        default: Issue.record("unexpected tool schema")
        }
    }
    let list = try #require(tools[0] as? MemoryListNotesTool)
    let get = try #require(tools[1] as? MemoryGetNoteTool)
    let sources = try #require(tools[2] as? MemoryNoteSourcesTool)
    _ = try await list.call(arguments: .init(after: "", limit: 0))
    _ = try await get.call(arguments: .init(version: "invalid", offset: 0, limit: 0))
    _ = try await sources.call(arguments: .init(version: ""))
    await #expect(throws: BackendFailure.self) { try await list.call(arguments: .init(after: nil, limit: -1)) }
    await #expect(throws: BackendFailure.self) { try await get.call(arguments: .init(version: noteID, offset: -1, limit: 1)) }
    await #expect(throws: BackendFailure.self) { try await sources.call(arguments: .init(version: String(repeating: "x", count: 1025))) }
}

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
    try await backend.generate(input, maximumTokens: 512, snapshot: { _ in })
    #expect(await probe.names == [[]])
    try await backend.generateWithTools(input, maximumTokens: 512,
        handler: { _ in Issue.record("scripted model did not request a tool"); throw HelperError.unavailable },
        snapshot: { _ in })
    let names = await probe.names
    #expect(names.count == 2)
    #expect(Set(names[1]) == Set(["project", "memory", "service", "shell"]))
}

@Test func inventoryToolHasEmptySchemaAndRequiresServiceResult() async throws {
    let tool = ServiceListToolsTool { arguments in
        guard case .listTools = arguments else { throw HelperError.protocolFault }
        return ToolResult(status: .success, text: "service_list_tools: inventory", truncated: false)
    }
    let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
    let properties = try #require(schema["properties"] as? [String: Any])
    #expect(properties.isEmpty)
    #expect((schema["required"] as? [String] ?? []).isEmpty)
    #expect(try await tool.call(arguments: .init()) == "service_list_tools: inventory\n[truncated: false]")
    let message = memoryEnvelope(.listTools(.init()))
    #expect(try Wire.decode(message.serializedData(), from: .helper) == message)
    #expect(throws: HelperError.self) { try Wire.decode(message.serializedData(), from: .service) }
    let unknown = try Asura_Model_V1_ListTools(serializedBytes: Data([8, 1]))
    #expect(throws: HelperError.self) { try Wire.decode(memoryEnvelope(.listTools(unknown)).serializedData(), from: .helper) }
}

@Test func auditToolDefaultsBoundedLimitAndRejectsUnknownWire() async throws {
    let tool=ServiceReadAuditTool { arguments in
        guard case .readAudit(let limit)=arguments else {throw HelperError.protocolFault}
        #expect(limit == 16)
        return ToolResult(status: .success, text: "recent metadata")
    }
    #expect(try await tool.call(arguments: .init(limit:nil)).contains("recent metadata"))
    #expect(try await tool.call(arguments: .init(limit:16)).contains("recent metadata"))
    for limit in [0,17,-1] {await #expect(throws: BackendFailure.self) {try await tool.call(arguments:.init(limit:limit))}}
    var args=Asura_Model_V1_AuditRead();args.limit=16
    let message=memoryEnvelope(.readAudit(args))
    #expect(try Wire.decode(message.serializedData(),from:.helper)==message)
    #expect(throws:HelperError.self) {try Wire.decode(message.serializedData(),from:.service)}
    for limit in [UInt32(0),17] {args.limit=limit;#expect(throws:HelperError.self) {try Wire.decode(memoryEnvelope(.readAudit(args)).serializedData(),from:.helper)}}
    #expect(throws:HelperError.self) {try Wire.decode(memoryEnvelope(.readAudit(.init())).serializedData(),from:.helper)}
    let unknown=try Asura_Model_V1_AuditRead(serializedBytes:Data([8,16,16,1]))
    #expect(throws:HelperError.self) {try Wire.decode(memoryEnvelope(.readAudit(unknown)).serializedData(),from:.helper)}
}

@Test func createNoteWirePreservesExactBodyAndBoundsSemanticRejections() throws {
    for body in [" exact\nbody \0", "", String(repeating: "é", count: 8192)] {
        for source in [nil, "", noteID, String(repeating: "é", count: 512)] as [String?] {
            var args = Asura_Model_V1_MemoryCreateNote(); args.body = body
            if let source { args.sourceVersion = source }
            let message = memoryEnvelope(.memoryCreateNote(args))
            #expect(try Wire.decode(message.serializedData(), from: .helper) == message)
            #expect(throws: HelperError.self) { try Wire.decode(message.serializedData(), from: .service) }
        }
    }
    var args = Asura_Model_V1_MemoryCreateNote()
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryCreateNote(args)), from: .helper) }
    args.body = String(repeating: "é", count: 8192) + "x"
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryCreateNote(args)), from: .helper) }
    args.body = "body"; args.sourceVersion = String(repeating: "é", count: 512) + "x"
    #expect(throws: HelperError.self) { try Wire.validate(memoryEnvelope(.memoryCreateNote(args)), from: .helper) }
    args.clearSourceVersion()
    let unknown = try Asura_Model_V1_MemoryCreateNote(serializedBytes: args.serializedData() + Data([0xa0, 0x06, 1]))
    #expect(throws: HelperError.self) { try Wire.decode(memoryEnvelope(.memoryCreateNote(unknown)).serializedData(), from: .helper) }
}

@Test func groupedCreateUsesSharedHandlerAndRejectsUnrelatedFields() async throws {
    let exact = "  note\nbody é\0 "
    let tool = MemoryTool { arguments in
        guard case .memoryCreateNote(let body, let source) = arguments else { throw HelperError.protocolFault }
        #expect(body == exact && source == noteID)
        return ToolResult(status: .success, text: "operation=fixture")
    }
    let valid = MemoryTool.Arguments(command: .create_note, version: nil, after: nil, offset: nil, limit: nil,
        body: exact, source_version: noteID)
    #expect(try await tool.call(arguments: valid).contains("operation=fixture"))
    let reject = MemoryTool { _ in Issue.record("invalid creation reached service"); throw HelperError.protocolFault }
    var cases: [MemoryTool.Arguments] = []
    var invalid = valid; invalid.body = nil; cases.append(invalid)
    invalid = valid; invalid.body = String(repeating: "é", count: 8192) + "x"; cases.append(invalid)
    invalid = valid; invalid.source_version = String(repeating: "x", count: 1025); cases.append(invalid)
    invalid = valid; invalid.version = noteID; cases.append(invalid)
    invalid = valid; invalid.after = noteID; cases.append(invalid)
    invalid = valid; invalid.offset = 0; cases.append(invalid)
    invalid = valid; invalid.limit = 1; cases.append(invalid)
    for command in [MemoryTool.Command.list_notes, .get_note, .note_sources] {
        invalid = valid; invalid.command = command; cases.append(invalid)
        invalid.body = nil; cases.append(invalid)
    }
    for args in cases { await #expect(throws: BackendFailure.self) { try await reject.call(arguments: args) } }
    let boundedInvalid = MemoryTool { arguments in
        guard case .memoryCreateNote(let body, let source) = arguments else { throw HelperError.protocolFault }
        #expect(body.isEmpty && source == "invalid")
        return ToolResult(status: .invalidArguments, text: "")
    }
    #expect(try await boundedInvalid.call(arguments: .init(command: .create_note, version: nil, after: nil,
        offset: nil, limit: nil, body: "", source_version: "invalid")).contains("Tool failed"))
}

@Test func memoryCreationDiagnosticsExposeOnlyFailedSchemaFlags() async throws {
    let secret = "PRIVATE_BODY_OR_SOURCE_DO_NOT_LOG"
    let tool = MemoryTool { _ in Issue.record("invalid memory arguments reached service"); throw HelperError.protocolFault }
    let valid = MemoryTool.Arguments(command: .create_note, version: nil, after: nil, offset: nil, limit: nil,
        body: secret, source_version: nil)
    var cases: [(MemoryTool.Arguments, Int)] = []
    var args = valid; args.body = nil; cases.append((args, 1001))
    args = valid; args.body = String(repeating: secret, count: 1000); cases.append((args, 1002))
    args = valid; args.source_version = String(repeating: secret, count: 1000); cases.append((args, 1004))
    args = valid; args.version = secret; cases.append((args, 1008))
    args = valid; args.after = secret; cases.append((args, 1016))
    args = valid; args.offset = 0; cases.append((args, 1032))
    args = valid; args.limit = 0; cases.append((args, 1064))
    args = valid; args.body = nil; args.version = secret; args.limit = 0; cases.append((args, 1073))
    args = valid; args.command = .list_notes; cases.append((args, 2001))
    args.body = nil; args.source_version = secret; cases.append((args, 2002))
    args.body = secret; cases.append((args, 2003))
    for (arguments, code) in cases {
        do {
            _ = try await tool.call(arguments: arguments)
            Issue.record("invalid memory proposal succeeded")
        } catch let error as BackendFailure {
            #expect(error.reason == .inputLimit)
            let diagnostic = modelFailureDiagnostic(LanguageModelSession.ToolCallError(tool: tool, underlyingError: error))
            #expect(diagnostic == "asura_model_failure tool_callback \(code)\n")
            #expect(!diagnostic.contains(secret))
        }
    }
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
        let decoded = try MemoryTool.Arguments(GeneratedContent(json: String(decoding: data, as: UTF8.self)))
        #expect(decoded.command == .create_note)
        #expect(decoded.body == exact)
        #expect(decoded.source_version == source)
        #expect(decoded.version == nil && decoded.after == nil && decoded.offset == nil && decoded.limit == nil)
    }
    let absent = try MemoryTool.Arguments(GeneratedContent(json: #"{"command":"create_note"}"#))
    #expect(absent.body == nil && absent.source_version == nil)
}

@Test func memoryCorrectionIsBoundedAndNeverDispatchesRejectedFields() async throws {
    let budget = ToolRoundBudget()
    let tool = BoundedNativeTool(base: MemoryTool { _ in
        Issue.record("invalid proposal reached host"); throw HelperError.protocolFault
    }, budget: budget)
    let invalid = MemoryTool.Arguments(command: .create_note, version: "PRIVATE_REJECTED_ID", after: nil,
        offset: nil, limit: 4096, body: nil, source_version: nil)
    for _ in 0..<8 {
        let feedback = try await tool.call(arguments: invalid)
        #expect(feedback.contains("create_note requires body"))
        #expect(!feedback.contains("PRIVATE_REJECTED_ID"))
        #expect(feedback.utf8.count <= 512)
    }
    await #expect(throws: BackendFailure.self) { try await tool.call(arguments: invalid) }
    #expect(await budget.recordedFailure()?.reason == .outputLimit)
    let failed = ToolRoundBudget()
    _ = try await failed.reserve()
    let pending = Task {
        try await BoundedNativeTool(base: MemoryTool { _ in throw HelperError.protocolFault }, budget: failed)
            .call(arguments: invalid)
    }
    await failed.finishInference(BackendFailure(.outputLimit))
    await #expect(throws: BackendFailure.self) { try await pending.value }
    let cancelled = Task {
        withUnsafeCurrentTask { $0?.cancel() }
        return try await BoundedNativeTool(base: MemoryTool { _ in throw HelperError.protocolFault }, budget: ToolRoundBudget())
            .call(arguments: invalid)
    }
    await #expect(throws: CancellationError.self) { try await cancelled.value }
}

private actor MemoryCorrectionProbe {
    var dispatches = 0
    var hostCalls = 0
    func next() -> Int { dispatches += 1; return dispatches }
    func received(_ arguments: ToolArguments) -> ToolResult {
        guard case .memoryCreateNote(let body, let source) = arguments else {
            Issue.record("unexpected corrected operation")
            return ToolResult(status: .invalidArguments, text: "")
        }
        #expect(body == "corrected note" && source == nil)
        hostCalls += 1
        return ToolResult(status: .success, text: "created")
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
func sdkCanCorrectMemoryArgumentsWithinExistingInferenceBudget(alwaysInvalid: Bool) async throws {
    let probe = MemoryCorrectionProbe()
    let backend = FoundationBackend(model: CorrectingMemoryModel(probe: probe, alwaysInvalid: alwaysInvalid),
        contextTokens: 8192, modelName: "correction-fixture", supportsTools: true)
    var input = ModelInput(); input.instructions = "Use memory."; input.prompt = "Store a note."
    do {
        try await backend.generateWithTools(input, maximumTokens: 2048,
            handler: { await probe.received($0) }, snapshot: { _ in })
        #expect(!alwaysInvalid)
    } catch let error as BackendFailure {
        #expect(alwaysInvalid && error.reason == .outputLimit)
    }
    #expect(await probe.dispatches == 3)
    #expect(await probe.hostCalls == (alwaysInvalid ? 0 : 1))
}

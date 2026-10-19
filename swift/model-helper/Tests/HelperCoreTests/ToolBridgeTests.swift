import Darwin
import Foundation
import Testing
@testable import HelperCore

private struct ToolScript: ToolModelBackend {
    let proposals: [ToolProposal]
    let source: ContextCapacitySource
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096,
        reportedContextTokens: 4096, contextSource: source, supportsTools: true,
        capabilityProfile: try! CapabilityProfile(mask: 1, provenance: .runtime)) }
    func generate(_ input: ModelInput, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        throw HelperError.unavailable
    }
    func generateWithTools(_ input: ModelInput, handler: @escaping ToolHandler,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await snapshot(Snapshot(text: "", inputContext: (123, 4096)))
        var text = ""
        for proposal in proposals { text = try await handler(proposal).text }
        try await snapshot(Snapshot(text: text, usageTokens: 1))
    }
}

/// TCR-U15 (transport part): every provider path forwards the name and the
/// exact argument text with its source; the result text returns unchanged.
@Test(arguments: [false, true], ["system", "coreai:fixture", "mlx:fixture", "ollama:fixture"])
func toolCallbackWaitsForServiceResultOrCancellation(cancel: Bool, selector: String) async throws {
    try await toolCallbackJourney(cancel: cancel, selector: selector,
        proposal: ToolProposal(name: "project", arguments: #"{"command":"read_file","path":"README.md"}"#,
            source: .generatedContent))
}

@Test(arguments: ["system", "coreai:fixture", "mlx:fixture", "ollama:fixture"])
func everyToolNameAndMalformedTextUseTheSameServiceBridge(selector: String) async throws {
    for cancel in [false, true] {
        for proposal in [
            ToolProposal(name: "memory", arguments: #"{"command":"create_note","body":" exact\nbody "}"#, source: .generatedContent),
            ToolProposal(name: "memory", arguments: #"{"command":"get_note"}"#, source: .generatedContent),
            ToolProposal(name: "service", arguments: #"{"command":"audit","limit":99}"#, source: .generatedContent),
            ToolProposal(name: "exec_cmd", arguments: #"{"command":"printf fixture","cwd":"."}"#, source: .generatedContent),
            ToolProposal(name: "project", arguments: "not json {", source: .providerJson),
            ToolProposal(name: "unregistered_tool", arguments: "", source: .providerJson)
        ] {
            try await toolCallbackJourney(cancel: cancel, selector: selector, proposal: proposal)
        }
    }
}

/// TCR-U15 (owner, 2026-10-18): 200 proposals run with 4,000-byte results
/// (800,000 bytes, far above the removed 65,536-byte turn total; the test's
/// service transport sends results on its 4,096-byte control queue); the 201st is sent with ordinal 201 and, after its
/// LIMIT result, the turn fails with outputLimit; no further proposal is sent.
@Test func overLimitProposalIsSentThenTheTurnFailsWithOutputLimit() async throws {
    #expect(Limits.recordedToolCalls == 200 && Limits.toolProposals == 201)
    let proposals = (1...(Limits.toolProposals + 1)).map { _ in
        ToolProposal(name: "service", arguments: #"{"command":"status"}"#, source: .generatedContent)
    }
    let channel = try await openToolSession(selector: "system", proposals: proposals)
    defer { channel.close() }
    let full = String(repeating: "x", count: 4_000)
    for ordinal in UInt32(1)...Limits.toolProposals {
        let call = try await channel.nextToolCall()
        #expect(call.ordinal == ordinal && call.name == "service" && call.source == .generatedContent)
        let over = ordinal == Limits.toolProposals
        var result = Asura_Model_V1_ToolResult(); result.ordinal = ordinal
        result.status = over ? .limit : .success; result.text = over ? "limit" : full
        result.truncated = false
        try await channel.send(.toolResult(result))
    }
    let terminal = try await channel.terminal()
    #expect(terminal.outcome == .failed && terminal.reason == .outputLimit)
    #expect(channel.unexpectedToolCalls == 0)
}

/// The helper rejects a non-LIMIT result for the over-limit ordinal as a
/// protocol fault; recorded ordinals accept any status.
@Test func overLimitResultMustBeLimit() throws {
    var recorded = Asura_Model_V1_ToolResult(); recorded.ordinal = Limits.recordedToolCalls
    recorded.status = .success; recorded.text = ""; recorded.truncated = false
    var accepted = Envelope(); accepted.operationID = Data(repeating: 7, count: 16); accepted.generation = 1
    accepted.body = .toolResult(recorded)
    try Wire.validate(accepted, from: .service)
    var result = Asura_Model_V1_ToolResult(); result.ordinal = Limits.toolProposals; result.status = .success
    result.text = ""; result.truncated = false
    var message = Envelope(); message.operationID = Data(repeating: 7, count: 16); message.generation = 1
    message.body = .toolResult(result)
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .service) }
    result.status = .limit; message.body = .toolResult(result)
    try Wire.validate(message, from: .service)
    result.ordinal = Limits.toolProposals + 1; message.body = .toolResult(result)
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .service) }
}

/// TCR-U15: a name over 256 bytes and argument text over 64,000 bytes are sent
/// as sizes only (known-size exhaustion); the text never crosses the wire.
@Test func oversizedNameAndArgumentsSendSizesOnly() async throws {
    let longName = String(repeating: "n", count: 257)
    let longArguments = #"{"command":"create_note","body":""# + String(repeating: "é", count: 32_000) + #""}"#
    let channel = try await openToolSession(selector: "system", proposals: [
        ToolProposal(name: longName, arguments: #"{}"#, source: .generatedContent),
        ToolProposal(name: "memory", arguments: longArguments, source: .generatedContent),
        ToolProposal(name: String(repeating: "m", count: 256),
            arguments: String(repeating: "x", count: 64_000), source: .generatedContent)])
    defer { channel.close() }
    let first = try await channel.nextToolCall()
    #expect(!first.hasName && first.nameBytes == 257 && first.arguments == "{}" && !first.hasArgumentsBytes)
    try await channel.reply(1, .invalidArguments, "invalid_arguments: name_over_limit")
    let second = try await channel.nextToolCall()
    #expect(second.name == "memory" && !second.hasArguments
        && second.argumentsBytes == UInt32(longArguments.utf8.count))
    try await channel.reply(2, .invalidArguments, "invalid_arguments: arguments_over_limit")
    let third = try await channel.nextToolCall()
    #expect(third.name.utf8.count == 256 && third.arguments.utf8.count == 64_000)
    try await channel.reply(3, .success, "ok")
    let terminal = try await channel.terminal()
    #expect(terminal.outcome == .complete)
}

private func toolCallbackJourney(cancel: Bool, selector: String, proposal: ToolProposal) async throws {
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
        return ToolScript(proposals: [proposal], source: source)
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
    hello.availability = .unknown; hello.reason = .none
    hello.selectedModel = selector; hello.assetRoot = "/tmp/model-fixture"
    var helloEnvelope = Envelope(); helloEnvelope.body = .hello(hello)
    try await service.send(helloEnvelope)
    let greeting = try Wire.decode(#require(try await replies.next()), from: .helper)
    #expect(greeting.hello.selectedModel == selector && greeting.hello.supportedCapabilities & 1 != 0)
    var input = ModelInput(); input.instructions = ""; input.prompt = "read file"
    let data = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = selector; begin.inputBytes = UInt64(data.count)
    begin.enableProjectTools = true
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
    #expect(call.toolCall.name == proposal.name && call.toolCall.arguments == proposal.arguments)
    #expect(call.toolCall.source == proposal.source)
    #expect(!call.toolCall.hasNameBytes && !call.toolCall.hasArgumentsBytes)
    #expect(call.toolCall.unknownFields.data.isEmpty)
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

@Test func toolWireRejectsWrongDirectionsBoundsAndUnknownFields() throws {
    var call = Asura_Model_V1_ToolCall(); call.ordinal = 1; call.name = "project"
    call.arguments = #"{"command":"read_file"}"#; call.source = .generatedContent
    var envelope = Envelope(); envelope.operationID = Data(repeating: 1, count: 16)
    envelope.generation = 1; envelope.body = .toolCall(call)
    try Wire.validate(envelope, from: .helper)
    #expect(throws: (any Error).self) { try Wire.validate(envelope, from: .service) }
    func rejected(_ change: (inout Asura_Model_V1_ToolCall) -> Void) {
        var bad = call; change(&bad); envelope.body = .toolCall(bad)
        #expect(throws: HelperError.self) { try Wire.validate(envelope, from: .helper) }
    }
    func accepted(_ change: (inout Asura_Model_V1_ToolCall) -> Void) throws {
        var good = call; change(&good); envelope.body = .toolCall(good)
        try Wire.validate(envelope, from: .helper)
    }
    try accepted { $0.ordinal = Limits.toolProposals }
    try accepted { $0.arguments = "" }
    try accepted { $0.arguments = String(repeating: "x", count: 64_000) }
    try accepted { $0.name = String(repeating: "n", count: 256) }
    try accepted { $0.clearName(); $0.nameBytes = 257 }
    try accepted { $0.clearArguments(); $0.argumentsBytes = 64_001 }
    try accepted { $0.source = .providerJson }
    rejected { $0.ordinal = 0 }
    rejected { $0.ordinal = Limits.toolProposals + 1 }
    rejected { $0.name = "" }
    rejected { $0.name = String(repeating: "n", count: 257) }
    rejected { $0.arguments = String(repeating: "x", count: 64_001) }
    rejected { $0.clearName() }
    rejected { $0.clearArguments() }
    rejected { $0.nameBytes = 300 }
    rejected { $0.argumentsBytes = 70_000 }
    rejected { $0.clearName(); $0.nameBytes = 256 }
    rejected { $0.clearArguments(); $0.argumentsBytes = 64_000 }
    rejected { $0.clearSource() }
    rejected { $0.source = .unspecified }
    // Removed typed argument field 10 (read_file) is now unknown and rejected.
    envelope.body = .toolCall(call)
    var raw = try call.serializedData(); raw.append(contentsOf: [0x52, 0x00])
    let stale = try Asura_Model_V1_ToolCall(serializedBytes: raw)
    envelope.body = .toolCall(stale)
    #expect(throws: (any Error).self) { _ = try Wire.decode(envelope.serializedData(), from: .helper) }
}

/// A scripted session over a socket pair: the service side of one turn.
final class ToolSessionChannel: @unchecked Sendable {
    let helper: Transport
    let service: Transport
    var replies: AsyncThrowingStream<Data, any Error>.AsyncIterator
    let running: Task<Void, Never>
    var unexpectedToolCalls = 0
    init(helper: Transport, service: Transport, running: Task<Void, Never>) {
        self.helper = helper; self.service = service; self.running = running
        replies = service.frames.makeAsyncIterator()
    }
    func scoped(_ body: Envelope.OneOf_Body) -> Envelope {
        var value = Envelope(); value.operationID = Data(repeating: 7, count: 16)
        value.generation = 1; value.body = body; return value
    }
    func send(_ body: Envelope.OneOf_Body) async throws { try await service.send(scoped(body)) }
    func next() async throws -> Envelope { try Wire.decode(#require(try await replies.next()), from: .helper) }
    func nextToolCall() async throws -> Asura_Model_V1_ToolCall {
        while true {
            let message = try await next()
            if case .toolCall(let call) = message.body { return call }
            if case .terminal = message.body { throw HelperError.closed }
        }
    }
    func reply(_ ordinal: UInt32, _ status: Asura_Model_V1_ToolStatus, _ text: String) async throws {
        var result = Asura_Model_V1_ToolResult(); result.ordinal = ordinal; result.status = status
        result.text = text; result.truncated = false
        try await send(.toolResult(result))
    }
    func terminal() async throws -> Asura_Model_V1_Terminal {
        while true {
            let message = try await next()
            switch message.body {
            case .terminal(let value): return value
            case .toolCall: unexpectedToolCalls += 1
            default: break
            }
        }
    }
    func close() { service.close(); helper.close(); running.cancel() }
}

/// Opens a turn with tools enabled whose backend proposes `proposals` in order.
func openToolSession(selector: String, proposals: [ToolProposal]) async throws -> ToolSessionChannel {
    var fds: [Int32] = [-1, -1]
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
    let helper = try Transport(fd: fds[0]); let service = try Transport(fd: fds[1])
    let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, factory: { _, _, _, _ in
        ToolScript(proposals: proposals, source: .system)
    }, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    let channel = ToolSessionChannel(helper: helper, service: service, running: running)
    var hello = Asura_Model_V1_Hello()
    hello.buildID = identity; hello.schemaDigest = identity; hello.maxFrameBytes = 65_536
    hello.availability = .unknown; hello.reason = .none; hello.selectedModel = selector
    var helloEnvelope = Envelope(); helloEnvelope.body = .hello(hello)
    try await service.send(helloEnvelope)
    _ = try await channel.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "tools"
    let data = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = selector; begin.inputBytes = UInt64(data.count)
    begin.enableProjectTools = true
    try await channel.send(.begin(begin)); _ = try await channel.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = data
    try await channel.send(.chunk(chunk))
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(data.count)
    try await channel.send(.inputEnd(end)); _ = try await channel.next()
    var credit = Asura_Model_V1_Credit(); credit.transferID = 2; credit.direction = .output
    credit.acceptedBytes = 0; credit.grantedBytes = 65_536
    try await channel.send(.start(.init()))
    try await channel.send(.credit(credit))
    return channel
}

import Darwin
import Foundation
import Testing
@testable import HelperCore

// CC4 helper session cases (context-composer.md, CC-S3 and CC4 wire changes):
// one inference per Begin; a response's calls go out as ToolCall frames
// after its text, then Terminal outcome 4; the session waits for the next
// Begin. No ToolResult exists.

/// Each request's scripted reply, by request ordinal; records what it received.
actor RequestScript {
    var replies: [ScriptedStep]
    var inputs: [ModelInput] = []
    var requests: [UInt32] = []
    init(_ replies: [ScriptedStep]) { self.replies = replies }
    func next(_ input: ModelInput, request: UInt32) -> ScriptedStep {
        inputs.append(input); requests.append(request)
        return replies.isEmpty ? .answer("done") : replies.removeFirst()
    }
}
enum ScriptedStep: Sendable {
    case answer(String)
    case calls(String, [ToolProposal])
}
struct ToolScript: ToolModelBackend {
    let script: RequestScript
    let source: ContextCapacitySource
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096,
        reportedContextTokens: 4096, contextSource: source, supportsTools: true,
        capabilityProfile: try! CapabilityProfile(mask: 1, provenance: .runtime)) }
    func generate(_ input: ModelInput, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        throw HelperError.unavailable
    }
    func generateWithTools(_ input: ModelInput, thinking: ThinkingRequest, request: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws -> [ToolProposal] {
        try await snapshot(Snapshot(text: "", inputContext: (123, 4096)))
        switch await script.next(input, request: request) {
        case .answer(let text):
            try await snapshot(Snapshot(text: text, usageTokens: 1))
            return []
        case .calls(let text, let proposals):
            if !text.isEmpty { try await snapshot(Snapshot(text: text)) }
            return proposals
        }
    }
}

/// The service side of one helper session over a socket pair.
final class ToolSessionChannel: @unchecked Sendable {
    let helper: Transport
    let service: Transport
    var replies: AsyncThrowingStream<Data, any Error>.AsyncIterator
    let running: Task<Void, Never>
    var selector = "system"
    /// Revisions received; the current output transfer is `revision + 2`.
    var revision: UInt64 = 0
    /// The newest output transfer credited.
    var credited: UInt64 = 0
    var text = Data()
    private var pending = Data()
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
    /// Begin, input, Ready and Start of request `ordinal`; credits the
    /// current output transfer once.
    func request(_ ordinal: UInt32, _ input: ModelInput, tools: Bool = true,
        thinking: ThinkingRequest? = nil) async throws {
        let data = try input.serializedData()
        var begin = Asura_Model_V1_Begin(); begin.model = selector; begin.inputBytes = UInt64(data.count)
        begin.requestOrdinal = ordinal
        if tools { begin.enableProjectTools = true }
        if let thinking { begin.thinking = thinking }
        try await send(.begin(begin)); _ = try await next()
        var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
        chunk.ordinal = 0; chunk.revision = 0; chunk.data = data
        try await send(.chunk(chunk))
        var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(data.count)
        try await send(.inputEnd(end))
        let ready = try await next()
        guard case .ready = ready.body else { throw HelperError.protocolFault }
        try await send(.start(.init()))
        try await credit()
    }
    /// Credits the current output transfer once, as the service does.
    func credit() async throws {
        let transfer = revision + 2
        guard credited < transfer else { return }
        var credit = Asura_Model_V1_Credit(); credit.transferID = transfer; credit.direction = .output
        credit.acceptedBytes = 0; credit.grantedBytes = 65_536
        credited = transfer
        try await send(.credit(credit))
    }
    /// Frames until the request's Terminal: its calls, the terminal and the
    /// response text as the service holds it. Each revision gets the next credit.
    func collect() async throws -> (calls: [Asura_Model_V1_ToolCall], terminal: Asura_Model_V1_Terminal) {
        var calls: [Asura_Model_V1_ToolCall] = []
        while true {
            let message = try await next()
            switch message.body {
            case .chunk(let value): pending.append(value.data)
            case .snapshotEnd(let value):
                text.removeSubrange(Int(value.offset)..<text.count)
                text.append(pending); pending.removeAll()
                revision = value.revision
                try? await credit()
            case .toolCall(let call): calls.append(call)
            case .terminal(let value): return (calls, value)
            default: break
            }
        }
    }
    var response: String { String(decoding: text, as: UTF8.self) }
    func close() { service.close(); helper.close(); running.cancel() }
}

/// Opens a session whose backend follows `script`, after the Hello exchange.
func openSession(selector: String = "system", backend: any ModelBackend) async throws -> ToolSessionChannel {
    var selector = selector
    if let script = backend as? ToolScript, selector == "system" {
        switch script.source {
        case .coreai: selector = "coreai:fixture"
        case .mlx: selector = "mlx:fixture"
        case .ollama: selector = "ollama:fixture"
        default: break
        }
    }
    var fds: [Int32] = [-1, -1]
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
    let helper = try Transport(fd: fds[0]); let service = try Transport(fd: fds[1])
    let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, factory: { _, _, _, _ in backend },
        buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    let channel = ToolSessionChannel(helper: helper, service: service, running: running)
    channel.selector = selector
    var hello = Asura_Model_V1_Hello()
    hello.buildID = identity; hello.schemaDigest = identity; hello.maxFrameBytes = 65_536
    hello.availability = .unknown; hello.reason = .none; hello.selectedModel = selector
    var helloEnvelope = Envelope(); helloEnvelope.body = .hello(hello)
    try await service.send(helloEnvelope)
    _ = try await channel.next()
    return channel
}

private func prompt(_ text: String) -> ModelInput {
    var input = ModelInput(); input.instructions = ""; input.prompt = text; return input
}
private func proposal(_ name: String, _ arguments: String, _ source: Asura_Model_V1_ToolCallSource = .generatedContent) -> ToolProposal {
    ToolProposal(name: name, arguments: arguments, source: source)
}

/// CC-S2: request 1 proposes two calls: ToolCall positions 1 and 2 after the
/// text, then Terminal outcome 4 with 2 calls. Request 2 carries the
/// continuation to the backend and completes. One inference per Begin; the
/// backend sees the service's request ordinals.
@Test func requestLoopRunsOneInferencePerBegin() async throws {
    let script = RequestScript([
        .calls("checking", [proposal("exec_cmd", #"{"command":"ls"}"#), proposal("job", #"{"command":"list"}"#)]),
        .answer("final answer")
    ])
    let channel = try await openSession(backend: ToolScript(script: script, source: .system))
    defer { channel.close() }
    try await channel.request(1, prompt("list files"))
    let first = try await channel.collect()
    #expect(first.calls.map(\.ordinal) == [1, 2])
    #expect(first.calls.map(\.name) == ["exec_cmd", "job"])
    #expect(first.calls.map(\.arguments) == [#"{"command":"ls"}"#, #"{"command":"list"}"#])
    #expect(first.terminal.outcome == .toolCalls && first.terminal.toolCalls == 2 && first.terminal.reason == .none)
    #expect(channel.response == "checking")
    var next = prompt("list files")
    next.continuation = continuation([
        (id: "call_1", name: "exec_cmd", arguments: #"{"command":"ls"}"#, output: "job: 1"),
        (id: "call_2", name: "job", arguments: #"{"command":"list"}"#, output: "- none")])
    try await channel.request(2, next)
    let second = try await channel.collect()
    #expect(second.calls.isEmpty)
    #expect(second.terminal.outcome == .complete && !second.terminal.hasToolCalls)
    #expect(channel.response == "final answer")
    #expect(await script.requests == [1, 2])
    #expect(await script.inputs.last?.continuation == next.continuation)
}

/// CC-S2: a cancel between requests ends the session with Terminal cancelled.
@Test func cancelBetweenRequestsEndsTheSession() async throws {
    let script = RequestScript([.calls("", [proposal("job", #"{"command":"list"}"#)])])
    let channel = try await openSession(backend: ToolScript(script: script, source: .ollama))
    defer { channel.close() }
    try await channel.request(1, prompt("go"))
    #expect(try await channel.collect().terminal.outcome == .toolCalls)
    var cancel = Asura_Model_V1_Cancel(); cancel.reason = .user
    try await channel.send(.cancel(cancel))
    let terminal = try await channel.collect().terminal
    #expect(terminal.outcome == .cancelled && terminal.reason == .cancelled)
    #expect(await script.requests == [1])
}

/// CC-S2: a later Begin must carry the next ordinal and repeat the first
/// request's tools; otherwise the helper faults and closes the channel.
@Test(arguments: [(UInt32(3), true), (UInt32(1), true), (UInt32(2), false)])
func laterBeginMustBeTheNextToolRequest(ordinal: UInt32, tools: Bool) async throws {
    let script = RequestScript([.calls("", [proposal("job", #"{"command":"list"}"#)])])
    let channel = try await openSession(backend: ToolScript(script: script, source: .mlx))
    defer { channel.close() }
    try await channel.request(1, prompt("go"))
    #expect(try await channel.collect().terminal.outcome == .toolCalls)
    let data = try prompt("go").serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = channel.selector; begin.inputBytes = UInt64(data.count)
    begin.requestOrdinal = ordinal
    if tools { begin.enableProjectTools = true }
    try await channel.send(.begin(begin))
    var frames: [Envelope] = []
    while let raw = try await channel.replies.next() { frames.append(try Wire.decode(raw, from: .helper)) }
    #expect(!frames.contains { if case .credit = $0.body { true } else { false } })
    #expect(await script.requests == [1])
}

/// Every provider source sends the name and exact argument text unread,
/// malformed and unregistered ones included (TCR-U15, transport part).
@Test(arguments: [ContextCapacitySource.system, .coreai, .mlx, .ollama])
func everyProposalIsSentUnread(source: ContextCapacitySource) async throws {
    let proposals = [
        proposal("memory", #"{"command":"create_note","body":" exact\nbody "}"#),
        proposal("service", #"{"command":"audit","limit":99}"#),
        proposal("project", "not json {", .providerJson),
        proposal("unregistered_tool", "", .providerJson)
    ]
    let script = RequestScript([.calls("", proposals)])
    let channel = try await openSession(backend: ToolScript(script: script, source: source))
    defer { channel.close() }
    try await channel.request(1, prompt("go"))
    let result = try await channel.collect()
    #expect(result.calls.count == proposals.count)
    for (call, expected) in zip(result.calls, proposals) {
        #expect(call.name == expected.name && call.arguments == expected.arguments && call.source == expected.source)
        #expect(!call.hasNameBytes && !call.hasArgumentsBytes && call.unknownFields.data.isEmpty)
    }
    #expect(result.terminal.toolCalls == UInt32(proposals.count))
}

/// A response with more than 201 calls sends the first 201; the service then
/// records the 201st as the over-limit call and ends the turn (CC4).
@Test func aRequestSendsAtMost201Calls() async throws {
    #expect(Limits.recordedToolCalls == 200 && Limits.toolProposals == 201)
    let proposals = (1...(Limits.toolProposals + 5)).map { _ in proposal("service", #"{"command":"status"}"#) }
    var fds: [Int32] = [-1, -1]
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
    let helper = try Transport(fd: fds[0])
    let raw = RawServiceSide(fd: fds[1])
    defer { helper.close(); raw.close() }
    let identity = Data(repeating: 1, count: 32)
    let script = RequestScript([.calls("", proposals)])
    let session = HelperSession(transport: helper, factory: { _, _, _, _ in ToolScript(script: script, source: .system) },
        buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    defer { running.cancel() }
    var hello = Asura_Model_V1_Hello()
    hello.buildID = identity; hello.schemaDigest = identity; hello.maxFrameBytes = 65_536
    hello.availability = .unknown; hello.reason = .none; hello.selectedModel = "system"
    var helloEnvelope = Envelope(); helloEnvelope.body = .hello(hello)
    try raw.send(helloEnvelope)
    _ = try await raw.wait { if case .hello = $0.body { true } else { false } }
    let data = try prompt("go").serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"; begin.inputBytes = UInt64(data.count)
    begin.requestOrdinal = 1; begin.enableProjectTools = true
    try raw.send(raw.scoped(.begin(begin)))
    _ = try await raw.wait { if case .credit = $0.body { true } else { false } }
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = data
    try raw.send(raw.scoped(.chunk(chunk)))
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(data.count)
    try raw.send(raw.scoped(.inputEnd(end)))
    _ = try await raw.wait { if case .ready = $0.body { true } else { false } }
    try raw.send(raw.scoped(.start(.init())))
    let terminal = try await raw.wait { if case .terminal = $0.body { true } else { false } }.terminal
    let calls = raw.received.compactMap { frame -> Asura_Model_V1_ToolCall? in
        if case .toolCall(let call) = frame.body { return call } else { return nil }
    }
    #expect(calls.count == Int(Limits.toolProposals))
    #expect(calls.map(\.ordinal) == Array(1...Limits.toolProposals))
    #expect(terminal.outcome == .toolCalls && terminal.toolCalls == Limits.toolProposals)
}

/// The service side of a socket pair with blocking writes and a reader thread
/// that feeds the frame decoder 256 bytes at a time, so a burst of frames
/// never exceeds the decoder's per-feed bound (test harness only).
final class RawServiceSide: @unchecked Sendable {
    private let fd: Int32
    private let lock = NSLock()
    private var frames: [Envelope] = []
    private var ended = false
    init(fd: Int32) {
        self.fd = fd
        let thread = Thread { [self] in
            var decoder = FrameDecoder()
            var buffer = [UInt8](repeating: 0, count: 256)
            while true {
                let count = Darwin.read(fd, &buffer, buffer.count)
                guard count > 0, let decoded = try? decoder.feed(Data(buffer.prefix(count))) else {
                    lock.withLock { ended = true }; return
                }
                for frame in decoded {
                    guard let message = try? Wire.decode(frame, from: .helper) else { lock.withLock { ended = true }; return }
                    lock.withLock { frames.append(message) }
                }
            }
        }
        thread.start()
    }
    var received: [Envelope] { lock.withLock { frames } }
    func scoped(_ body: Envelope.OneOf_Body) -> Envelope {
        var value = Envelope(); value.operationID = Data(repeating: 7, count: 16)
        value.generation = 1; value.body = body; return value
    }
    func send(_ message: Envelope) throws {
        let frame = try Wire.frame(message)
        try frame.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                let count = Darwin.write(fd, raw.baseAddress! + offset, raw.count - offset)
                guard count > 0 else { throw HelperError.closed }
                offset += count
            }
        }
    }
    /// The first received frame matching `match`; test-only polling with a bound.
    func wait(_ match: (Envelope) -> Bool) async throws -> Envelope {
        for _ in 0..<5_000 {
            if let found = received.first(where: match) { return found }
            if lock.withLock({ ended }) { break }
            try await Task.sleep(for: .milliseconds(1))
        }
        throw HelperError.closed
    }
    func close() { Darwin.shutdown(fd, SHUT_RDWR); Darwin.close(fd) }
}

/// TCR-U15: a name over 256 bytes and argument text over 64,000 bytes are sent
/// as sizes only (known-size exhaustion); the text never crosses the wire.
@Test func oversizedNameAndArgumentsSendSizesOnly() async throws {
    let longName = String(repeating: "n", count: 257)
    let longArguments = #"{"command":"create_note","body":""# + String(repeating: "é", count: 32_000) + #""}"#
    let script = RequestScript([.calls("", [
        proposal(longName, #"{}"#), proposal("memory", longArguments),
        proposal(String(repeating: "m", count: 256), String(repeating: "x", count: 64_000))])])
    let channel = try await openSession(backend: ToolScript(script: script, source: .system))
    defer { channel.close() }
    try await channel.request(1, prompt("go"))
    let calls = try await channel.collect().calls
    #expect(!calls[0].hasName && calls[0].nameBytes == 257 && calls[0].arguments == "{}" && !calls[0].hasArgumentsBytes)
    #expect(calls[1].name == "memory" && !calls[1].hasArguments && calls[1].argumentsBytes == UInt32(longArguments.utf8.count))
    #expect(calls[2].name.utf8.count == 256 && calls[2].arguments.utf8.count == 64_000)
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
    var raw = try call.serializedData(); raw.append(contentsOf: [0x52, 0x00])
    let stale = try Asura_Model_V1_ToolCall(serializedBytes: raw)
    envelope.body = .toolCall(stale)
    #expect(throws: (any Error).self) { _ = try Wire.decode(envelope.serializedData(), from: .helper) }
}

/// CC4 wire: Begin needs its request ordinal; outcome 4 needs 1 to 201 calls
/// and reason none; no other outcome carries calls; envelope field 21 (the
/// removed ToolResult) is unknown.
@Test func requestLoopWireShapes() throws {
    var begin = Asura_Model_V1_Begin(); begin.model = "system"; begin.inputBytes = 1
    var envelope = Envelope(); envelope.operationID = Data(repeating: 1, count: 16); envelope.generation = 1
    envelope.body = .begin(begin)
    #expect(throws: HelperError.self) { try Wire.validate(envelope, from: .service) }
    begin.requestOrdinal = 2; envelope.body = .begin(begin)
    try Wire.validate(envelope, from: .service)
    func terminal(_ outcome: Asura_Model_V1_Outcome, _ reason: Reason, _ calls: UInt32?) -> Envelope {
        var end = Asura_Model_V1_Terminal(); end.outcome = outcome; end.reason = reason
        end.lastRevision = 1; end.count = 0; end.totalBytes = 0; end.usageKnown = false
        if let calls { end.toolCalls = calls }
        var value = envelope; value.body = .terminal(end); return value
    }
    try Wire.validate(terminal(.toolCalls, .none, 1), from: .helper)
    try Wire.validate(terminal(.toolCalls, .none, Limits.toolProposals), from: .helper)
    for bad in [terminal(.toolCalls, .none, nil), terminal(.toolCalls, .none, 0),
        terminal(.toolCalls, .none, Limits.toolProposals + 1), terminal(.toolCalls, .protocolFault, 1),
        terminal(.complete, .none, 1), terminal(.failed, .protocolFault, 1)] {
        #expect(throws: HelperError.self) { try Wire.validate(bad, from: .helper) }
    }
    var bytes = try Envelope.with { $0.operationID = Data(repeating: 7, count: 16); $0.generation = 1 }.serializedData()
    bytes.append(contentsOf: [0xaa, 0x01, 0x02, 0x08, 0x01])
    #expect(throws: (any Error).self) { _ = try Wire.decode(bytes, from: .service) }
}

/// CC4 input: the continuation is paired assistant and tool entries with
/// matching call IDs; history holds no tool fields.
@Test func continuationStructureIsValidated() throws {
    var input = ModelInput(); input.instructions = "i"; input.prompt = "p"
    input.continuation = continuation([(id: "call_1", name: "exec_cmd", arguments: "{}", output: "x"),
        (id: "call_2", name: "job", arguments: "{}", output: "y")])
    #expect(try Wire.input(input.serializedData()) == input)
    var swapped = input; swapped.continuation.swapAt(1, 2)
    var missing = input; missing.continuation.removeLast()
    var spoken = input; spoken.continuation[0].text = "words"
    var badID = input; badID.continuation[0].toolCalls[0].callID = "call 1"; badID.continuation[1].callID = "call 1"
    var history = input; history.continuation = []
    var turn = Asura_Model_V1_HistoryTurn(); turn.role = .assistant; turn.text = "a"; turn.callID = "call_1"
    history.history = [turn]
    for bad in [swapped, missing, spoken, badID, history] {
        #expect(throws: (any Error).self) { _ = try Wire.input(bad.serializedData()) }
    }
}

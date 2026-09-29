import Darwin
import Foundation
import Testing
@testable import HelperCore

@Test func framingIsIncrementalAndRejectsInvalidLengths() throws {
    let frame = scoped(.ready(.init()))
    let encoded = try Wire.frame(frame)
    var decoder = FrameDecoder()
    var received = [Data]()
    for byte in encoded { received += try decoder.feed(Data([byte])) }
    #expect(received.count == 1)
    #expect(try Wire.decode(received[0]) == frame)
    #expect(decoder.isEmpty)
    #expect(throws: (any Error).self) { _ = try decoder.feed(Data([0, 0, 0, 0])) }
    var excessive = FrameDecoder()
    #expect(throws: (any Error).self) { _ = try excessive.feed(Data([0, 1, 0, 1])) }
}

@Test func unknownAndDuplicateFieldsAreRejected() throws {
    let frame = scoped(.ready(.init()))
    var bytes = try frame.serializedData()
    bytes.append(contentsOf: [0xa0, 0x06, 0x01]) // Unknown envelope field 100.
    #expect(throws: (any Error).self) { _ = try Wire.decode(bytes) }
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    var duplicate = try input.serializedData()
    duplicate.append(contentsOf: [0x1a, 0x01, 0x78]) // Duplicate prompt.
    #expect(throws: (any Error).self) { _ = try Wire.input(duplicate) }
    var nested = Asura_Model_V1_HistoryTurn()
    nested.role = .user; nested.text = "one"
    var nestedBytes = try nested.serializedData()
    nestedBytes.append(contentsOf: [0xa0, 0x06, 0x01])
    input.history = [try Asura_Model_V1_HistoryTurn(serializedBytes: nestedBytes)]
    #expect(throws: (any Error).self) { _ = try Wire.input(input.serializedData()) }
}

@Test func rolesAndBoundsArePreservedWithoutSDKCalls() throws {
    var input = ModelInput(); input.instructions = "instructions"; input.prompt = "prompt"
    var user = Asura_Model_V1_HistoryTurn(); user.role = .user; user.text = "literal assistant: text"
    var assistant = Asura_Model_V1_HistoryTurn(); assistant.role = .assistant; assistant.text = "answer"
    input.history = [user, assistant]
    let decoded = try Wire.input(input.serializedData())
    #expect(decoded == input)
    let history = SystemBackend.history(decoded)
    #expect(history.count == 3)
    if case .prompt = history[1] {} else { Issue.record("user role lost") }
    if case .response = history[2] {} else { Issue.record("assistant role lost") }
    input.prompt = String(repeating: "x", count: 32_769)
    #expect(throws: (any Error).self) { _ = try Wire.input(input.serializedData()) }
}

private actor Scripted: ModelBackend {
    var calls = 0
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096) }
    func generate(_ input: ModelInput, maximumTokens: UInt32, snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        calls += 1
        try await snapshot(Snapshot(text: "first"))
        await Task.yield()
        try await snapshot(Snapshot(text: "replacement", usageTokens: 12))
    }
}

private func pair() throws -> (Transport, Transport) {
    var fds: [Int32] = [-1, -1]
    guard socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0 else { throw HelperError.unavailable }
    do { return try (Transport(fd: fds[0]), Transport(fd: fds[1])) }
    catch { Darwin.close(fds[0]); Darwin.close(fds[1]); throw error }
}
private func hello(_ identity: Data) -> Envelope {
    var value = Asura_Model_V1_Hello()
    value.buildID = identity; value.schemaDigest = identity; value.maxFrameBytes = 65_536
    value.availability = .unknown; value.capabilities = 0; value.reason = .none
    var frame = Envelope(); frame.body = .hello(value); return frame
}
private func scoped(_ body: Envelope.OneOf_Body) -> Envelope {
    var frame = Envelope(); frame.operationID = Data(repeating: 7, count: 16)
    frame.generation = 1; frame.body = body; return frame
}
private func outputCredit(_ id: UInt64) -> Envelope {
    var credit = Asura_Model_V1_Credit()
    credit.transferID = id; credit.direction = .output; credit.acceptedBytes = 0; credit.grantedBytes = 65_536
    return scoped(.credit(credit))
}

@Test func realSocketPipelineRequiresStartAndPublishesReplacement() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let backend = Scripted()
    let session = HelperSession(transport: helper, backend: backend, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    defer { running.cancel() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity))
    let reply = try Wire.decode(#require(try await replies.next()))
    #expect(reply.hello.availability == .available)
    #expect(reply.hello.contextTokens == 4096)
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let data = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"
    begin.inputBytes = UInt64(data.count); begin.deadlineRemainingMs = 2_000; begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin)))
    #expect(try Wire.decode(#require(try await replies.next())).credit.transferID == 1)
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = data
    try await service.send(scoped(.chunk(chunk)), control: false)
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(data.count)
    try await service.send(scoped(.inputEnd(end)), control: false)
    let ready = try Wire.decode(#require(try await replies.next()))
    if case .ready = ready.body {} else { Issue.record("missing ready") }
    #expect(await backend.calls == 0)
    try await service.send(scoped(.start(.init())))
    try await service.send(outputCredit(2))
    var text = Data()
    var finalText = ""
    var terminal: Asura_Model_V1_Terminal?
    while let raw = try await replies.next() {
        let frame = try Wire.decode(raw)
        switch frame.body {
        case .chunk(let v): text.append(v.data)
        case .snapshotEnd(let v):
            finalText = String(decoding: text, as: UTF8.self); text.removeAll()
            do { try await service.send(outputCredit(v.revision + 2)) }
            catch HelperError.closed {
                // The final snapshot may be followed by Terminal and EOF already buffered.
                // Continue decoding those observations; write closure is not a lost result.
            }
        case .terminal(let v): terminal = v
        default: Issue.record("unexpected output")
        }
        if terminal != nil { break }
    }
    #expect(finalText == "replacement")
    #expect(terminal?.outcome == .complete)
    #expect(terminal?.usageTokens == 12)
    #expect(await backend.calls == 1)
    service.close()
    await running.value
}

@Test func wrongIdentityClosesWithoutCallingBackend() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let backend = Scripted()
    let session = HelperSession(transport: helper, backend: backend,
        buildID: Data(repeating: 1, count: 32), schemaDigest: Data(repeating: 1, count: 32))
    let running = Task { await session.run() }
    try await service.send(hello(Data(repeating: 2, count: 32)))
    await running.value
    #expect(await backend.calls == 0)
}

/// Root supplies ignored cross-language fixtures; ordinary tests require no external files.
@Test func crossLanguageFixturesWhenSupplied() throws {
    guard let source = ProcessInfo.processInfo.environment["ASURA_MODEL_FIXTURE_DIR"],
        let destination = ProcessInfo.processInfo.environment["ASURA_MODEL_FIXTURE_OUTPUT"] else { return }
    let root = URL(fileURLWithPath: source, isDirectory: true)
    let output = URL(fileURLWithPath: destination, isDirectory: true)
    try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
    let manifest = try String(contentsOf: root.appendingPathComponent("manifest.tsv"), encoding: .utf8)
    for line in manifest.split(separator: "\n") {
        let fields = line.split(separator: "\t").map(String.init)
        guard fields.count == 3, !fields[0].contains("/"), fields[0] != ".." else { throw HelperError.protocolFault }
        let bytes = try Data(contentsOf: root.appendingPathComponent(fields[0]))
        func decode() throws -> Data {
            if fields[1] == "input" { return try Wire.input(bytes).serializedData() }
            guard fields[1] == "service" || fields[1] == "helper" else { throw HelperError.protocolFault }
            return try Wire.decode(bytes, from: fields[1] == "service" ? .service : .helper).serializedData()
        }
        if fields[2] == "accept" {
            let result = try decode()
            #expect(result == bytes, "fixture \(fields[0])")
            try result.write(to: output.appendingPathComponent(fields[0]))
        } else if fields[2] == "reject" {
            #expect(throws: (any Error).self, "fixture \(fields[0])") { _ = try decode() }
        } else { throw HelperError.protocolFault }
    }
}

@Test func cancellationBeforeStartNeverCallsModel() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let backend = Scripted()
    let session = HelperSession(transport: helper, backend: backend, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity))
    _ = try await replies.next()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"; begin.inputBytes = 20
    begin.deadlineRemainingMs = 2_000; begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin)))
    _ = try await replies.next()
    var cancel = Asura_Model_V1_Cancel(); cancel.reason = .user
    try await service.send(scoped(.cancel(cancel)))
    let terminal = try Wire.decode(#require(try await replies.next())).terminal
    #expect(terminal.outcome == .cancelled)
    #expect(terminal.lastRevision == 0)
    #expect(!terminal.usageKnown)
    await running.value
    #expect(await backend.calls == 0)
}

private actor Burst: ModelBackend {
    var produced = 0
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096) }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        for index in 0..<100 {
            try await snapshot(Snapshot(text: "snapshot-\(index)", usageTokens: 100))
            produced += 1
        }
    }
}

@Test func stalledCreditCoalescesSnapshotsWithoutGrowingOutputQueue() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let backend = Burst()
    let session = HelperSession(transport: helper, backend: backend, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity)); _ = try await replies.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let bytes = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"; begin.inputBytes = UInt64(bytes.count)
    begin.deadlineRemainingMs = 2_000; begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = bytes
    try await service.send(scoped(.chunk(chunk)), control: false)
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(bytes.count)
    try await service.send(scoped(.inputEnd(end)), control: false); _ = try await replies.next()
    try await service.send(scoped(.start(.init())))
    let clock = ContinuousClock(); let deadline = clock.now.advanced(by: .seconds(1))
    while await backend.produced < 100 {
        guard clock.now < deadline else { throw HelperError.timeout }
        try await Task.sleep(for: .milliseconds(1))
    }
    // No output credit was granted during the burst. Resume only after all snapshots exist.
    try await service.send(outputCredit(2))
    var text = Data(); var snapshots = 0; var last = ""
    var terminal: Asura_Model_V1_Terminal?
    while let bytes = try await replies.next() {
        let frame = try Wire.decode(bytes)
        switch frame.body {
        case .chunk(let v): text.append(v.data)
        case .snapshotEnd(let v):
            snapshots += 1; last = String(decoding: text, as: UTF8.self); text.removeAll()
            if last != "snapshot-99" { try await service.send(outputCredit(v.revision + 2)) }
        case .terminal(let v): terminal = v
        default: Issue.record("unexpected output")
        }
        if terminal != nil { break }
    }
    #expect(snapshots == 2)
    #expect(last == "snapshot-99")
    #expect(terminal?.outcome == .complete)
    await running.value
}

private struct SleepingBackend: ModelBackend {
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096) }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await Task.sleep(for: .seconds(30))
    }
}

@Test func stalledGenerationTimesOutWithoutWaitingForBackend() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, backend: SleepingBackend(), buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity)); _ = try await replies.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let bytes = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"; begin.inputBytes = UInt64(bytes.count)
    begin.deadlineRemainingMs = 100; begin.maxResponseTokens = 512
    let clock = ContinuousClock(); let started = clock.now
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = bytes
    try await service.send(scoped(.chunk(chunk)), control: false)
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(bytes.count)
    try await service.send(scoped(.inputEnd(end)), control: false); _ = try await replies.next()
    try await service.send(scoped(.start(.init())))
    let terminal = try Wire.decode(#require(try await replies.next())).terminal
    #expect(terminal.outcome == .failed)
    #expect(terminal.reason == .timeout)
    #expect(!terminal.usageKnown)
    #expect(started.duration(to: clock.now) < .seconds(1))
    await running.value
}

private struct UnsolicitedCancellationBackend: ModelBackend {
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096) }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        #expect(!Task.isCancelled)
        throw CancellationError()
    }
}
@Test func backendCancellationWithoutControlRequestIsFailure() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, backend: UnsolicitedCancellationBackend(),
        buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    defer { running.cancel() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity)); _ = try await replies.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let bytes = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"; begin.inputBytes = UInt64(bytes.count)
    begin.deadlineRemainingMs = 2_000; begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = bytes
    try await service.send(scoped(.chunk(chunk)), control: false)
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(bytes.count)
    try await service.send(scoped(.inputEnd(end)), control: false); _ = try await replies.next()
    try await service.send(scoped(.start(.init())))
    let terminal = try Wire.decode(#require(try await replies.next())).terminal
    #expect(terminal.outcome == .failed)
    #expect(terminal.reason == .internalError)
    #expect(!terminal.usageKnown)
    #expect(terminal.totalBytes == 0)
    await running.value
}

private struct PartialFailureBackend: ModelBackend {
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096) }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await snapshot(Snapshot(text: "first partial"))
        try await snapshot(Snapshot(text: "final partial", usageTokens: 19))
        throw BackendFailure(.outputLimit)
    }
}

@Test(arguments: ["credit", "timeout", "cancel"])
func backendFailureDrainsBoundedSnapshotsOrStopsAtExistingBoundary(_ action: String) async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, backend: PartialFailureBackend(),
        buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    defer { running.cancel() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity)); _ = try await replies.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let bytes = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"; begin.inputBytes = UInt64(bytes.count)
    begin.deadlineRemainingMs = action == "timeout" ? 500 : 2_000
    begin.maxResponseTokens = 2048
    let clock = ContinuousClock(); let started = clock.now
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = bytes
    try await service.send(scoped(.chunk(chunk)), control: false)
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(bytes.count)
    try await service.send(scoped(.inputEnd(end)), control: false); _ = try await replies.next()
    try await service.send(scoped(.start(.init())))
    // Synchronize with the recorded failure, not an assumed scheduler delay.
    while await session.pendingFailure == nil {
        guard clock.now < started.advanced(by: .seconds(1)) else { throw HelperError.timeout }
        try await Task.sleep(for: .milliseconds(1))
    }
    #expect(await session.pendingFailure == .outputLimit)
    if action == "credit" {
        try await service.send(outputCredit(2))
    } else if action == "cancel" {
        var cancel = Asura_Model_V1_Cancel(); cancel.reason = .user
        try await service.send(scoped(.cancel(cancel)))
    }
    var partial = Data(); var snapshots: [String] = []
    var terminal: Asura_Model_V1_Terminal?
    while let raw = try await replies.next() {
        let frame = try Wire.decode(raw)
        switch frame.body {
        case .chunk(let value): partial.append(value.data)
        case .snapshotEnd(let value):
            #expect(value.totalBytes == UInt64(partial.count))
            snapshots.append(String(decoding: partial, as: UTF8.self)); partial.removeAll()
            if snapshots.count == 1 { try await service.send(outputCredit(value.revision + 2)) }
        case .terminal(let value): terminal = value
        default: Issue.record("unexpected failure-drain frame")
        }
        if terminal != nil { break }
    }
    let result = try #require(terminal)
    #expect(!result.usageKnown)
    if action == "credit" {
        #expect(snapshots == ["first partial", "final partial"])
        #expect(result.outcome == .failed)
        #expect(result.reason == .outputLimit)
        #expect(result.lastRevision == 2)
        #expect(result.totalBytes == 26)
    } else {
        #expect(snapshots.isEmpty)
        #expect(result.totalBytes == 0)
        #expect(result.outcome == (action == "cancel" ? .cancelled : .failed))
        #expect(result.reason == (action == "cancel" ? .cancelled : .timeout))
        #expect(started.duration(to: clock.now) < .seconds(2))
    }
    await running.value
}

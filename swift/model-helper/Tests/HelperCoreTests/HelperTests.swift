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
    // The prompt field carries the service's context block; only the total bounds it.
    input.prompt = String(repeating: "x", count: 32_769)
    #expect(try Wire.input(input.serializedData()) == input)
    input.history = Array(repeating: user, count: Limits.historyEntries + 1)
    #expect(throws: (any Error).self) { _ = try Wire.input(input.serializedData()) }
}

private actor Scripted: ModelBackend {
    var calls = 0
    let capacity: UInt32
    init(capacity: UInt32 = 4096) { self.capacity = capacity }
    func status() async -> BackendStatus { BackendStatus(contextTokens: capacity, reportedContextTokens: capacity, contextSource: .system) }
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
private func availableHello(selector: String, capacity: UInt32, reported: UInt32,
    source: ContextCapacitySource) -> Envelope {
    var value = Asura_Model_V1_Hello()
    value.buildID = Data(repeating: 1, count: 32)
    value.schemaDigest = Data(repeating: 1, count: 32)
    value.maxFrameBytes = 65_536
    value.selectedModel = selector
    value.availability = .available
    value.capabilities = 1
    value.reason = .none
    value.contextTokens = capacity
    value.reportedContextTokens = reported
    value.contextSource = source.rawValue
    var frame = Envelope(); frame.body = .hello(value); return frame
}

@Test func contextCapacityRequiresCompleteProviderMatchedEvidence() throws {
    let valid = availableHello(selector: "ollama:example", capacity: 8192,
        reported: 32768, source: .ollama)
    try Wire.validate(valid, from: .helper)
    var missingReported = valid
    missingReported.hello.clearReportedContextTokens()
    #expect(throws: HelperError.protocolFault) { try Wire.validate(missingReported, from: .helper) }
    var missingSource = valid
    missingSource.hello.clearContextSource()
    #expect(throws: HelperError.protocolFault) { try Wire.validate(missingSource, from: .helper) }
    var tooSmall = valid
    tooSmall.hello.reportedContextTokens = 4096
    #expect(throws: HelperError.protocolFault) { try Wire.validate(tooSmall, from: .helper) }
    var wrongSource = valid
    wrongSource.hello.contextSource = ContextCapacitySource.system.rawValue
    #expect(throws: HelperError.protocolFault) { try Wire.validate(wrongSource, from: .helper) }
    var request = hello(Data(repeating: 1, count: 32))
    request.hello.reportedContextTokens = 8192
    request.hello.contextSource = ContextCapacitySource.system.rawValue
    #expect(throws: HelperError.protocolFault) { try Wire.validate(request, from: .service) }
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
    #expect(reply.hello.reportedContextTokens == 4096)
    #expect(reply.hello.contextSource == ContextCapacitySource.system.rawValue)
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

/// CC-U16 and CC-U18 (helper side): input above 64 KiB arrives under a sliding
/// credit window; a Begin above capacity times 8 bytes is refused before allocation.
@Test func windowSizedInputUsesSlidingCredit() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let backend = Scripted(capacity: 100_000)
    let session = HelperSession(transport: helper, backend: backend, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    defer { running.cancel() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity))
    let reply = try Wire.decode(#require(try await replies.next()))
    #expect(reply.hello.availability == .available)
    #expect(!reply.hello.hasToolSchemaBytes)
    var input = ModelInput(); input.instructions = String(repeating: "x", count: 200_000); input.prompt = "fixture"
    let data = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"
    begin.inputBytes = UInt64(data.count); begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin)))
    let first = try Wire.decode(#require(try await replies.next())).credit
    #expect(first.acceptedBytes == 0 && first.grantedBytes == Limits.creditWindow)
    var granted = first.grantedBytes
    var offset = 0
    var ordinal: UInt64 = 0
    while offset < data.count {
        let end = min(offset + Limits.chunk, data.count)
        #expect(UInt64(end) <= granted)
        var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
        chunk.ordinal = ordinal; chunk.revision = 0; chunk.data = data.subdata(in: offset..<end)
        try await service.send(scoped(.chunk(chunk)), control: false)
        offset = end; ordinal += 1
        if offset < data.count {
            let credit = try Wire.decode(#require(try await replies.next())).credit
            #expect(credit.acceptedBytes == UInt64(offset))
            #expect(credit.grantedBytes == UInt64(offset) + Limits.creditWindow)
            granted = credit.grantedBytes
        }
    }
    var end = Asura_Model_V1_InputEnd(); end.count = ordinal; end.totalBytes = UInt64(data.count)
    try await service.send(scoped(.inputEnd(end)), control: false)
    let ready = try Wire.decode(#require(try await replies.next()))
    if case .ready = ready.body {} else { Issue.record("missing ready") }
    service.close()
    await running.value
}

@Test func beginAboveCapacityBoundIsRefusedBeforeAllocation() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let backend = Scripted(capacity: 1000)
    let session = HelperSession(transport: helper, backend: backend, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity))
    _ = try Wire.decode(#require(try await replies.next()))
    var begin = Asura_Model_V1_Begin(); begin.model = "system"
    begin.inputBytes = 8_001; begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin)))
    var credited = false
    while let raw = try await replies.next() {
        if case .credit = try Wire.decode(raw).body { credited = true }
    }
    #expect(!credited)
    await running.value
    #expect(await backend.calls == 0)
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
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096, reportedContextTokens: 4096, contextSource: .system) }
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

/// HW-03: a supplied `deadline_remaining_ms` arms no timer; a stalled
/// generation runs past it and ends only on cancellation.
@Test func suppliedGenerationDeadlineArmsNoTimerAndCancelEndsStall() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let timers = ManualSessionTimer()
    let backend = CancellableGeneration()
    let session = HelperSession(transport: helper, backend: backend, buildID: identity,
        schemaDigest: identity, timerSleep: { await timers.sleep($0) })
    let running = Task { await session.run() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity)); _ = try await replies.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let bytes = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"; begin.inputBytes = UInt64(bytes.count)
    begin.deadlineRemainingMs = 100; begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = bytes
    try await service.send(scoped(.chunk(chunk)), control: false)
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(bytes.count)
    try await service.send(scoped(.inputEnd(end)), control: false); _ = try await replies.next()
    try await service.send(scoped(.start(.init())))
    let limit = ContinuousClock.now.advanced(by: .seconds(1))
    while !(await backend.started) {
        guard ContinuousClock.now < limit else { throw HelperError.timeout }
        await Task.yield()
    }
    // Past the supplied 100 ms: no timer was armed and generation continues.
    try await Task.sleep(for: .milliseconds(300))
    #expect(await timers.count() == 0)
    #expect(!(await backend.cancelled))
    var cancel = Asura_Model_V1_Cancel(); cancel.reason = .user
    try await service.send(scoped(.cancel(cancel)))
    let terminal = try Wire.decode(#require(try await replies.next())).terminal
    #expect(terminal.outcome == .cancelled)
    #expect(terminal.reason == .cancelled)
    #expect(!terminal.usageKnown)
    // Only the kept 250 ms final drain is armed after the terminal.
    #expect(await timers.count() == 1)
    await timers.fireAll()
    await running.value
}

private struct UnsolicitedCancellationBackend: ModelBackend {
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096, reportedContextTokens: 4096, contextSource: .system) }
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
    func status() async -> BackendStatus { BackendStatus(contextTokens: 4096, reportedContextTokens: 4096, contextSource: .system) }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await snapshot(Snapshot(text: "first partial"))
        try await snapshot(Snapshot(text: "final partial", usageTokens: 19))
        throw BackendFailure(.outputLimit)
    }
}

@Test(arguments: ["credit", "cancel"])
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
    // Supplied but ignored: it arms no timer (owner, 2026-10-16).
    begin.deadlineRemainingMs = 500
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
        #expect(result.outcome == .cancelled)
        #expect(result.reason == .cancelled)
        #expect(started.duration(to: clock.now) < .seconds(2))
    }
    await running.value
}

private actor ManualSessionTimer {
    private var pending: [CheckedContinuation<Void, Never>] = []
    func sleep(_ milliseconds: UInt32) async {
        // Deliberately deliver cancelled timers too: the session revision must reject them.
        await withCheckedContinuation { pending.append($0) }
    }
    func count() -> Int { pending.count }
    func fireAll() {
        let batch = pending; pending.removeAll()
        for continuation in batch { continuation.resume() }
    }
}

private actor CancellableGeneration: ModelBackend {
    private(set) var started = false
    private(set) var cancelled = false
    func status() async -> BackendStatus {
        BackendStatus(contextTokens: 4096, reportedContextTokens: 4096, contextSource: .system)
    }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        started = true
        do { try await Task.sleep(for: .seconds(30)) }
        catch { cancelled = true; throw error }
    }
}

/// HW-01: Hello, Begin, input transfer and the Ready-to-Start handoff arm no
/// timer; generation then stays cancellable.
@Test func handshakeArmsNoTimerAndGenerationRemainsCancellable() async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let timers = ManualSessionTimer()
    let backend = CancellableGeneration()
    let session = HelperSession(transport: helper, backend: backend, buildID: identity,
        schemaDigest: identity, timerSleep: { await timers.sleep($0) })
    let running = Task { await session.run() }
    var replies = service.frames.makeAsyncIterator()
    try await Task.sleep(for: .milliseconds(20))
    #expect(await timers.count() == 0, "Hello wait armed a timer")
    try await service.send(hello(identity)); _ = try await replies.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let bytes = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"
    begin.inputBytes = UInt64(bytes.count); begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = bytes
    try await service.send(scoped(.chunk(chunk)), control: false)
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(bytes.count)
    try await service.send(scoped(.inputEnd(end)), control: false); _ = try await replies.next()
    try await Task.sleep(for: .milliseconds(20))
    #expect(await timers.count() == 0, "a handshake step armed a timer")
    try await service.send(scoped(.start(.init())))
    let limit = ContinuousClock.now.advanced(by: .seconds(1))
    while !(await backend.started) {
        guard ContinuousClock.now < limit else { throw HelperError.timeout }
        await Task.yield()
    }
    #expect(await timers.count() == 0)
    #expect(!(await backend.cancelled))
    var cancel = Asura_Model_V1_Cancel(); cancel.reason = .user
    try await service.send(scoped(.cancel(cancel)))
    let terminal = try Wire.decode(#require(try await replies.next())).terminal
    #expect(terminal.outcome == .cancelled)
    #expect(terminal.reason == .cancelled)
    await timers.fireAll()
    await running.value
    let settlementLimit = ContinuousClock.now.advanced(by: .seconds(1))
    while !(await backend.cancelled) && ContinuousClock.now < settlementLimit { await Task.yield() }
    #expect(await backend.cancelled)
}

@Test func beginDeadlineIsOptionalButExplicitValuesRemainBounded() throws {
    var begin = Asura_Model_V1_Begin(); begin.model = "system"
    begin.inputBytes = 10; begin.maxResponseTokens = 512
    try Wire.validate(scoped(.begin(begin)), from: .service)
    for valid in [UInt32(1), 60_000] {
        begin.deadlineRemainingMs = valid
        try Wire.validate(scoped(.begin(begin)), from: .service)
    }
    for invalid in [UInt32(0), 60_001] {
        begin.deadlineRemainingMs = invalid
        #expect(throws: HelperError.self) { try Wire.validate(scoped(.begin(begin)), from: .service) }
    }
}

/// HW-01/HW-02: a stalled transfer or Ready-to-Start handoff stays pending
/// with no timer and ends on the service's cancel, before generation starts.
@Test(arguments: [false, true])
func stalledTransferAndReadyHandoffWaitUntilCancel(ready: Bool) async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let timers = ManualSessionTimer()
    let backend = CancellableGeneration()
    let session = HelperSession(transport: helper, backend: backend, buildID: identity,
        schemaDigest: identity, timerSleep: { await timers.sleep($0) })
    let running = Task { await session.run() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity)); _ = try await replies.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let bytes = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"
    begin.inputBytes = UInt64(bytes.count); begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    if ready {
        var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
        chunk.ordinal = 0; chunk.revision = 0; chunk.data = bytes
        try await service.send(scoped(.chunk(chunk)), control: false)
        var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(bytes.count)
        try await service.send(scoped(.inputEnd(end)), control: false); _ = try await replies.next()
    }
    try await Task.sleep(for: .milliseconds(50))
    #expect(await timers.count() == 0)
    var cancel = Asura_Model_V1_Cancel(); cancel.reason = .user
    try await service.send(scoped(.cancel(cancel)))
    let terminal = try Wire.decode(#require(try await replies.next())).terminal
    #expect(terminal.outcome == .cancelled)
    #expect(terminal.reason == .cancelled)
    #expect(!(await backend.started))
    await timers.fireAll()
    await running.value
}

private struct LateMeasured: ModelBackend {
    let duplicate: Bool
    func status() async -> BackendStatus {
        BackendStatus(contextTokens: 4096, reportedContextTokens: 4096, contextSource: .system)
    }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await snapshot(Snapshot(text: "preserved response"))
        try await snapshot(Snapshot(text: "", inputContext: (700, 4096)))
        if duplicate { try await snapshot(Snapshot(text: "", inputContext: (700, 4096))) }
    }
}

@Test(arguments: [false, true])
func lateInputMeasurementPreservesOutputAndRejectsDuplicate(_ duplicate: Bool) async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let backend = LateMeasured(duplicate: duplicate)
    let session = HelperSession(transport: helper, backend: backend, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    defer { running.cancel() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity))
    let reply = try Wire.decode(#require(try await replies.next()))
    #expect(reply.hello.availability == .available)
    #expect(reply.hello.contextTokens == 4096)
    #expect(reply.hello.reportedContextTokens == 4096)
    #expect(reply.hello.contextSource == ContextCapacitySource.system.rawValue)
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
    try await service.send(scoped(.start(.init())))
    try await service.send(outputCredit(2))
    var text = Data()
    var finalText = ""
    var measurementCount = 0
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
        case .contextMeasured(let v):
            measurementCount += 1
            #expect(v.inputTokens == 700)
            #expect(v.capacityTokens == 4096)
        case .terminal(let v): terminal = v
        default: Issue.record("unexpected output")
        }
        if terminal != nil { break }
    }
    #expect(finalText == "preserved response")
    #expect(measurementCount == 1)
    #expect(terminal?.outcome == (duplicate ? .failed : .complete))

    service.close()
    await running.value
}

private actor UsageSocketBackend: ModelBackend {
    let mode: String
    private(set) var started = false
    private(set) var lateReportRejected = false
    init(mode: String) { self.mode = mode }
    func status() async -> BackendStatus {
        BackendStatus(contextTokens: 4096, reportedContextTokens: 4096, contextSource: .system)
    }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        try await snapshot(Snapshot(text: "", inferenceStartedOrdinal: 1))
        started = true
        if mode == "cancel" {
            do { try await Task.sleep(for: .seconds(30)) }
            catch {
                do { try await snapshot(Snapshot(text: "", inferenceUsage: RequestUsage(ordinal: 1))) }
                catch is CancellationError { lateReportRejected = true }
                throw CancellationError()
            }
        } else {
            try await snapshot(Snapshot(text: "complete response", usageTokens: 4))
            let report = RequestUsage(ordinal: 1, inputTokens: 0, outputTokens: 4, complete: true)
            try await snapshot(Snapshot(text: "", inferenceUsage: report))
            if mode == "duplicate" { try await snapshot(Snapshot(text: "", inferenceUsage: report)) }
        }
    }
}

@Test(arguments: ["complete", "duplicate", "cancel"])
func usageReportsPreserveTextAndCancellationHasOneReportBeforeTerminal(mode: String) async throws {
    let (helper, service) = try pair()
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let backend = UsageSocketBackend(mode: mode)
    let session = HelperSession(transport: helper, backend: backend, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    var replies = service.frames.makeAsyncIterator()
    try await service.send(hello(identity)); _ = try await replies.next()
    var input = ModelInput(); input.instructions = ""; input.prompt = "fixture"
    let bytes = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "system"
    begin.inputBytes = UInt64(bytes.count); begin.maxResponseTokens = 512
    try await service.send(scoped(.begin(begin))); _ = try await replies.next()
    var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
    chunk.ordinal = 0; chunk.revision = 0; chunk.data = bytes
    try await service.send(scoped(.chunk(chunk)), control: false)
    var end = Asura_Model_V1_InputEnd(); end.count = 1; end.totalBytes = UInt64(bytes.count)
    try await service.send(scoped(.inputEnd(end)), control: false); _ = try await replies.next()
    try await service.send(scoped(.start(.init())))
    if mode == "cancel" {
        let limit = ContinuousClock.now.advanced(by: .seconds(1))
        while !(await backend.started) {
            guard ContinuousClock.now < limit else { throw HelperError.timeout }
            await Task.yield()
        }
        var cancel = Asura_Model_V1_Cancel(); cancel.reason = .user
        try await service.send(scoped(.cancel(cancel)))
    } else { try await service.send(outputCredit(2)) }
    var reports: [Asura_Model_V1_InferenceUsage] = []
    var text = Data()
    var terminal: Asura_Model_V1_Terminal?
    while let raw = try await replies.next() {
        let frame = try Wire.decode(raw)
        switch frame.body {
        case .inferenceUsage(let report): reports.append(report)
        case .chunk(let value): text.append(value.data)
        case .snapshotEnd: break
        case .terminal(let value):
            #expect(reports.count == 1)
            terminal = value
        default: Issue.record("unexpected inference usage output")
        }
        if terminal != nil { break }
    }
    let report = try #require(reports.first)
    #expect(report.requestOrdinal == 1)
    if mode == "cancel" {
        #expect(!report.complete && !report.hasInputTokens && !report.hasOutputTokens)
        #expect(terminal?.outcome == .cancelled)
        #expect(text.isEmpty)
    } else {
        #expect(report.complete && report.hasInputTokens && report.inputTokens == 0 && report.outputTokens == 4)
        #expect(String(decoding: text, as: UTF8.self) == "complete response")
        #expect(terminal?.lastRevision == 1)
        #expect(terminal?.outcome == (mode == "duplicate" ? .failed : .complete))
    }
    await running.value
    if mode == "cancel" {
        let limit = ContinuousClock.now.advanced(by: .seconds(1))
        while !(await backend.lateReportRejected) && ContinuousClock.now < limit { await Task.yield() }
        #expect(await backend.lateReportRejected)
    }
}

/// CC-I13 (helper side): the reported tool overhead covers every declared tool.
@Test func toolSchemaBytesCoverDeclaredTools() {
    let bytes = projectToolSchemaBytes()
    let names = makeProjectTools({ _ in throw BackendFailure(.internalError) })
        .reduce(0) { $0 + $1.name.utf8.count + $1.description.utf8.count }
    #expect(Int(bytes) > names)
    #expect(bytes <= Limits.toolSchemaBytes)
    #expect(projectToolSchemaBytes() == bytes)
}

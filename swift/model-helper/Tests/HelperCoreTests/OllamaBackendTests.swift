import Foundation
import FoundationModels
import Testing
@testable import HelperCore

@Test func ollamaEndpointRequiresTLSRemotelyAndRejectsCredentialsOrRedirectTargets() throws {
    for text in ["http://example.com:11434", "http://localhost:11434",
                 "http://127.0.0.1/api", "http://user:secret@127.0.0.1", "http://127.0.0.1/?token=a",
                 "http://127.0.0.1/#a", "file:///tmp/model", "http://192.168.1.1:11434"] {
        #expect(throws: OllamaLanguageModel.Failure.self) {
            try OllamaLanguageModel.Settings(name: "model:tag", endpoint: URL(string: text)!)
        }
    }
    for text in ["https://models.example:8443", "https://localhost:11434", "https://10.0.0.1"] {
        #expect(throws: OllamaLanguageModel.Failure.self) {
            try OllamaLanguageModel.Settings(name: "example", endpoint: URL(string: text)!)
        }
    }
    let loopback = try OllamaLanguageModel.Settings(name: "example", endpoint: URL(string: "https://127.0.0.1:8443")!)
    #expect(loopback.endpoint.scheme == "https")
    let settings = try OllamaLanguageModel.Settings(name: "Granite:8B")
    #expect(settings.name == "Granite:8B")
    #expect(settings.endpoint.host == "127.0.0.1")
    for name in ["", "model name", "model\n", String(repeating: "a", count: 1025)] {
        #expect(throws: OllamaLanguageModel.Failure.self) { try OllamaLanguageModel.Settings(name: name) }
    }
}

@Test func ollamaCapabilitiesNeedRuntimeEvidenceAndCompletion() throws {
    let settings = try OllamaLanguageModel.Settings(name: "example")
    #expect(throws: OllamaLanguageModel.Failure.self) {
        try OllamaLanguageModel.checked(settings, data: Data(#"{"capabilities":["embedding"]}"#.utf8))
    }
    let text = try OllamaLanguageModel.checked(settings, data: Data(#"{"capabilities":["completion"],"model_info":{"test.context_length":32768}}"#.utf8))
    #expect(!text.supportsTools)
    #expect(text.reportedContextTokens == 32768)
    #expect(text.contextTokens == 32768)
    let tools = try OllamaLanguageModel.checked(settings, data: Data(#"{"capabilities":["completion","tools"],"model_info":{"test.context_length":8192}}"#.utf8))
    #expect(tools.supportsTools)
    #expect(!tools.capabilities.contains(.guidedGeneration))
}

@Test func ollamaHistoryPreservesInstructionsAndMessageRoles() throws {
    let transcript = Transcript(entries: [
        .instructions(.init(segments: [.text(.init(content: "rules"))], toolDefinitions: [])),
        .prompt(.init(segments: [.text(.init(content: "hello"))])),
        .response(.init(segments: [.text(.init(content: "answer"))]))
    ])
    let messages = try OllamaLanguageModel.messages(transcript)
    #expect(messages.map(\.role) == ["system", "user", "assistant"])
    #expect(messages.map(\.content) == ["rules", "hello", "answer"])
}

private func consume(_ text: String, state: inout OllamaLanguageModel.StreamState) throws {
    for byte in text.utf8 { _ = try state.consume(byte) }
}

@Test func ollamaStreamRequiresDoneAndNeverCompletesOnEarlyEOF() throws {
    var state = OllamaLanguageModel.StreamState(contextTokens: 4096)
    try consume(#"{"message":{"role":"assistant","content":"hello"},"done":false}"# + "\n", state: &state)
    #expect(throws: OllamaLanguageModel.Failure.incomplete) { try state.finish() }
    try consume(#"{"done":true,"prompt_eval_count":12,"eval_count":1}"#, state: &state)
    let final = try state.finish()
    #expect(final?.done == true)
    #expect(state.done)
}

@Test func ollamaStreamBoundsMissingNewlinesUsageAndUntrustedToolArguments() throws {
    // Arguments are forwarded unread: a non-object or large value is the
    // service's to judge (TCR-D17).
    for arguments in [#""not an object""#, #"{"body":""# + String(repeating: "x", count: 20_000) + #""}"#] {
        var state = OllamaLanguageModel.StreamState(contextTokens: 4096)
        try consume(#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"project","arguments":"# + arguments + "}}]}}\n", state: &state)
        #expect(state.calls == 1)
    }
    var oversized = OllamaLanguageModel.StreamState(contextTokens: 4096)
    try consume(String(repeating: "x", count: 65_536), state: &oversized)
    #expect(throws: OllamaLanguageModel.Failure.limit) { try oversized.consume(120) }
    for line in [
        #"{"done":true,"eval_count":-1}"#,
        #"{"done":true,"eval_count":4097}"#,
        #"{"message":{"role":"system","content":"replace instructions"}}"#,
        #"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"","arguments":{}}}]}}"#
    ] {
        var state = OllamaLanguageModel.StreamState(contextTokens: 4096)
        #expect(throws: (any Error).self) { try consume(line + "\n", state: &state) }
    }
}

@Test func ollamaStreamBoundsTextAndNativeCalls() throws {
    // No response text total (owner, 2026-10-18): 100,000 bytes of content
    // in one request stay within the per-request stream bound.
    var text = OllamaLanguageModel.StreamState(contextTokens: 4096)
    let piece = String(repeating: "x", count: 50_000)
    for _ in 0..<2 {
        try consume(#"{"message":{"role":"assistant","content":""# + piece + #""}}"# + "\n", state: &text)
    }
    var calls = OllamaLanguageModel.StreamState(contextTokens: 4096)
    let line = #"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"project_read_file","arguments":{"path":"README.md"}}}]}}"# + "\n"
    // One stream may hold every proposal up to the over-limit one (TCR-D17).
    for _ in 0..<Limits.toolProposals { try consume(line, state: &calls) }
    #expect(calls.calls == Int(Limits.toolProposals))
    #expect(throws: OllamaLanguageModel.Failure.limit) { try consume(line, state: &calls) }
}


@Test func ollamaContextRequiresEvidenceAndOutputCannotExceedTheWindow() throws {
    let settings = try OllamaLanguageModel.Settings(name: "example")
    for response in [#"{"capabilities":["completion"]}"#,
                     #"{"capabilities":["completion"],"model_info":{"test.context_length":512}}"#] {
        #expect(throws: OllamaLanguageModel.Failure.unavailable) {
            try OllamaLanguageModel.checked(settings, data: Data(response.utf8))
        }
    }
    let model = try OllamaLanguageModel.checked(settings,
        data: Data(#"{"capabilities":["completion"],"model_info":{"a.context_length":4096,"b.context_length":32768}}"#.utf8))
    #expect(model.contextTokens == 4096)
    #expect(model.reportedContextTokens == 4096)
    // Output above num_ctx is impossible, so it is invalid; no cap applies below it.
    var state = OllamaLanguageModel.StreamState(contextTokens: 4096)
    #expect(throws: OllamaLanguageModel.Failure.invalidResponse) {
        try consume(#"{"done":true,"eval_count":4097}"# + "\n", state: &state)
    }
    var full = OllamaLanguageModel.StreamState(contextTokens: 4096)
    try consume(#"{"done":true,"done_reason":"length","eval_count":4000}"# + "\n", state: &full)
}


/// UH13 (owner, 2026-10-19): every request sends num_predict -1, Ollama's
/// infinite generation, so no Asura or Modelfile cap applies.
@Test func ollamaRequestPreservesContextAndSendsNoOutputCap() throws {
    let request = OllamaLanguageModel.Request(model: "example", messages: [],
        options: .init(num_ctx: 4096))
    let data = try JSONEncoder().encode(request)
    let value = try JSONSerialization.jsonObject(with: data) as! [String: Any]
    #expect(value["truncate"] as? Bool == false)
    #expect(value["shift"] as? Bool == false)
    let options = value["options"] as! [String: Any]
    #expect(options["num_predict"] as? Int == -1)
    #expect(options["num_ctx"] as? Int == 4096)
}


@Test func ollamaFinishReasonClassifiesAfterDeliveringFinalContent() async throws {
    // UH16: a length stop is a full window (owner, 2026-10-19).
    for (reason, count, expected) in [
        ("stop", 3000, "complete"), ("length", 3000, "context"),
        ("", 3000, "invalid"), ("", 12, "invalid"),
        ("unexpected", 3000, "invalid"), ("load", 0, "invalid")
    ] {
        let json = "{\"message\":{\"role\":\"assistant\",\"content\":\"final chunk\"},\"done\":true,\"done_reason\":\"\(reason)\",\"eval_count\":\(count)}"
        let chunk = try JSONDecoder().decode(OllamaLanguageModel.Chunk.self, from: Data(json.utf8))
        var emitted: [String] = []
        var outcome = "complete"
        do {
            try await OllamaLanguageModel.deliver(chunk) { value in
                emitted.append(value.message?.content ?? "")
            }
        } catch let error as BackendFailure {
            #expect(error.reason == .contextLimit); outcome = "context"
        } catch let error as OllamaLanguageModel.Failure {
            #expect(error == .invalidResponse); outcome = "invalid"
        }
        #expect(emitted == ["final chunk"])
        #expect(outcome == expected)
    }
}

@Test func ollamaMissingReasonNeverInventsNaturalCompletion() async throws {
    // No request cap exists, so no output count can stand in for a reason.
    for json in [#"{"done":true,"eval_count":2048}"#, #"{"done":true,"eval_count":6}"#, #"{"done":true}"#] {
        let chunk = try JSONDecoder().decode(OllamaLanguageModel.Chunk.self, from: Data(json.utf8))
        do {
            try await OllamaLanguageModel.deliver(chunk) { _ in }
            Issue.record("ambiguous completion accepted")
        } catch let error as BackendFailure { Issue.record("unexpected typed failure \(error.reason)") }
        catch let error as OllamaLanguageModel.Failure { #expect(error == .invalidResponse) }
    }
}

@Test func ollamaNativeToolStopRemainsNaturalCompletion() async throws {
    let json = #"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"service_status","arguments":{}}}]},"done":true,"done_reason":"stop","eval_count":20}"#
    var state = OllamaLanguageModel.StreamState(contextTokens: 4096)
    var chunk: OllamaLanguageModel.Chunk?
    for byte in (json + "\n").utf8 { if let value = try state.consume(byte) { chunk = value } }
    let final = try #require(chunk)
    var calls = 0
    try await OllamaLanguageModel.deliver(final) { calls += $0.message?.tool_calls?.count ?? 0 }
    #expect(calls == 1)
}


@Test func ambiguousOllamaFinishBecomesTypedProtocolFailureAfterFinalText() async throws {
    let chunk = try JSONDecoder().decode(OllamaLanguageModel.Chunk.self,
        from: Data(#"{"message":{"role":"assistant","content":"partial"},"done":true,"done_reason":"unknown"}"#.utf8))
    var emitted = ""
    do {
        try await OllamaLanguageModel.deliverToExecutor(chunk) { emitted += $0.message?.content ?? "" }
        Issue.record("ambiguous response completed")
    } catch let error as BackendFailure { #expect(error.reason == .protocolFault) }
    #expect(emitted == "partial")
}

/// HW-04 unit: show, chat and tags share one session factory with no request
/// inactivity or resource time limit (owner, 2026-10-16).
@Test func hw04OllamaRequestsHaveNoTimeLimit() {
    let session = OllamaLanguageModel.session()
    defer { session.invalidateAndCancel() }
    #expect(session.configuration.timeoutIntervalForRequest == .infinity)
    #expect(session.configuration.timeoutIntervalForResource == .infinity)
    #expect(session.configuration.httpMaximumConnectionsPerHost == 1)
    #expect(session.configuration.urlCredentialStorage == nil)
    #expect(session.configuration.urlCache == nil)
}

private actor InputMeasurements {
    var values: [(UInt32, UInt32)] = []
    func record(_ value: Snapshot) { if let context = value.inputContext { values.append((context.tokens, context.capacity)) } }
}

@Test(arguments: [0, 700, 8192])
func ollamaInputMeasurementUsesOnlyFirstRequest(_ count: Int) async throws {
    let measurements = InputMeasurements()
    let observer = OllamaInputObserver { await measurements.record($0) }
    let first = UUID(), later = UUID()
    await observer.begin(first, capacity: 8192)
    await observer.begin(later, capacity: 8192)
    try await observer.complete(later, inputTokens: 100, capacity: 8192)
    try await observer.complete(first, inputTokens: count, capacity: 8192)
    try await observer.complete(first, inputTokens: 5, capacity: 8192)
    #expect(await measurements.values.count == 1)
    #expect(await measurements.values.first?.0 == UInt32(count))
}

@Test(arguments: [nil, -1, 8193, Int.max] as [Int?])
func ollamaMissingOrInvalidFirstMeasurementNeverUsesLaterToolPass(_ count: Int?) async throws {
    let measurements = InputMeasurements()
    let observer = OllamaInputObserver { await measurements.record($0) }
    let first = UUID(), later = UUID()
    await observer.begin(first, capacity: 8192)
    try await observer.complete(first, inputTokens: count, capacity: 8192)
    await observer.begin(later, capacity: 8192)
    try await observer.complete(later, inputTokens: 100, capacity: 8192)
    #expect(await measurements.values.isEmpty)
}

@Test func ollamaMeasurementRequiresMatchingCapacityAndSuppressesCancellation() async throws {
    let measurements = InputMeasurements()
    let observer = OllamaInputObserver { await measurements.record($0) }
    let first = UUID()
    await observer.begin(first, capacity: 8192)
    try await observer.complete(first, inputTokens: 10, capacity: 4096)
    #expect(await measurements.values.isEmpty)
    let cancelledObserver = OllamaInputObserver { await measurements.record($0) }
    await cancelledObserver.begin(first, capacity: 8192)
    let task = Task {
        withUnsafeCurrentTask { $0?.cancel() }
        try await cancelledObserver.complete(first, inputTokens: 10, capacity: 8192)
    }
    do { try await task.value; Issue.record("cancelled measurement admitted") }
    catch is CancellationError {}
    #expect(await measurements.values.isEmpty)
}

@Test(arguments: ["length", "unknown", "", "missing"])
func ollamaFinalMeasurementMapsLengthToContextLimitAndRejectsUnverifiedReasons(_ reason: String) async throws {
    let measurements = InputMeasurements()
    let observer = OllamaInputObserver { await measurements.record($0) }
    let request = UUID()
    await observer.begin(request, capacity: 8192)
    let chunk = OllamaLanguageModel.Chunk(message: .init(role: "assistant", content: "final partial"),
        done: true, done_reason: reason == "missing" ? nil : reason,
        prompt_eval_count: 700, eval_count: 2048)
    var text = ""
    do {
        try await OllamaLanguageModel.deliverMeasuredToExecutor(chunk, requestID: request, capacity: 8192, observer: observer) { text += $0.message?.content ?? "" }
        Issue.record("final record incorrectly completed")
    } catch let failure as BackendFailure {
        #expect(failure.reason == (reason == "length" ? .contextLimit : .protocolFault))
    }
    #expect(text == "final partial")
    #expect(await measurements.values.count == (reason == "length" ? 1 : 0))
    if reason == "length" { #expect(await measurements.values.first?.0 == 700) }
}

/// TL-04 to TL-07 (Ollama): controls come from `/api/show` (verified shape,
/// Ollama 0.40.5); a thinking model without `thinking.values` takes booleans;
/// a model without the capability gets nothing.
@Test func ollamaThinkingControlsComeFromShowMetadata() throws {
    let settings = try OllamaLanguageModel.Settings(name: "example")
    func controls(_ json: String) throws -> (Bool, ThinkingControls) {
        let model = try OllamaLanguageModel.checked(settings, data: Data(json.utf8))
        return (model.capabilities.contains(.reasoning), model.thinkingControls)
    }
    let qwen = try controls(#"{"capabilities":["completion","tools","thinking"],"model_info":{"a.context_length":4096},"thinking":{"values":[false,true],"default":true}}"#)
    #expect(qwen.0 && qwen.1 == [.off, .on])
    let granite = try controls(#"{"capabilities":["completion","thinking"],"model_info":{"a.context_length":4096}}"#)
    #expect(granite.0 && granite.1 == [.off, .on])
    let graded = try controls(#"{"capabilities":["completion","thinking"],"model_info":{"a.context_length":4096},"thinking":{"values":["low","medium","high","max"]}}"#)
    #expect(graded.1 == [.low, .medium, .high])
    let llama = try controls(#"{"capabilities":["completion","tools"],"model_info":{"a.context_length":4096},"thinking":{"values":[false],"default":false}}"#)
    #expect(!llama.0 && llama.1 == [])
}

/// TL-01, TL-04 to TL-06 and TL-10 (Ollama): the request carries `think`
/// only for a level, in the provider's form; reasoning entries never return
/// to the model.
@Test func ollamaRequestSendsThinkOnlyForALevelAndDropsReasoningHistory() throws {
    #expect(OllamaLanguageModel.think(nil) == nil)
    #expect(OllamaLanguageModel.think(.custom("no_think")) == .bool(false))
    #expect(OllamaLanguageModel.think(.custom("think")) == .bool(true))
    #expect(OllamaLanguageModel.think(.light) == .string("low"))
    #expect(OllamaLanguageModel.think(.moderate) == .string("medium"))
    #expect(OllamaLanguageModel.think(.deep) == .string("high"))
    func body(_ think: OllamaLanguageModel.JSON?) throws -> [String: Any] {
        let data = try JSONEncoder().encode(OllamaLanguageModel.Request(model: "m", messages: [], tools: nil,
            think: think, options: .init(num_ctx: 4096)))
        return try JSONSerialization.jsonObject(with: data) as! [String: Any]
    }
    #expect(try body(nil)["think"] == nil)
    #expect(try body(.bool(false))["think"] as? Bool == false)
    #expect(try body(.string("high"))["think"] as? String == "high")
    let transcript = Transcript(entries: [
        .prompt(.init(segments: [.text(.init(content: "hello"))])),
        .reasoning(.init(segments: [.text(.init(content: "secret plan"))])),
        .response(.init(segments: [.text(.init(content: "answer"))]))
    ])
    let messages = try OllamaLanguageModel.messages(transcript)
    #expect(messages.map(\.role) == ["user", "assistant"])
    #expect(messages.allSatisfy { $0.thinking == nil && !$0.content.contains("secret") })
    // Streamed thinking decodes beside the content.
    var state = OllamaLanguageModel.StreamState(contextTokens: 4096)
    var chunk: OllamaLanguageModel.Chunk?
    for byte in (#"{"message":{"role":"assistant","content":"","thinking":"hmm"},"done":false}"# + "\n").utf8 {
        if let value = try state.consume(byte) { chunk = value }
    }
    #expect(chunk?.message?.thinking == "hmm")
}


/// FW-U1 and FW-U2 (owner, 2026-10-20): the request window is the model's
/// full reported maximum, the smallest valid key, and is sent as num_ctx.
@Test func ollamaRequestWindowIsTheReportedMaximum() throws {
    let settings = try OllamaLanguageModel.Settings(name: "example")
    for json in [#"{"capabilities":["completion"],"model_info":{"granite.context_length":131072}}"#,
                 #"{"capabilities":["completion"],"model_info":{"a.context_length":262144,"b.context_length":131072}}"#] {
        let model = try OllamaLanguageModel.checked(settings, data: Data(json.utf8))
        #expect(model.contextTokens == 131_072)
        #expect(model.reportedContextTokens == 131_072)
    }
    let data = try JSONEncoder().encode(OllamaLanguageModel.Request(model: "example", messages: [],
        options: .init(num_ctx: 131_072)))
    let value = try JSONSerialization.jsonObject(with: data) as! [String: Any]
    #expect((value["options"] as! [String: Any])["num_ctx"] as? Int == 131_072)
    #expect(value["truncate"] as? Bool == false && value["shift"] as? Bool == false)
}

/// FW-U3 (2026-10-20): the stream has no byte total. More than 4 MiB of
/// records within the window is accepted; one record above 64 KiB is not.
@Test func ollamaStreamHasNoByteTotalWithinTheWindow() throws {
    var state = OllamaLanguageModel.StreamState(contextTokens: 131_072)
    let record = #"{"message":{"role":"assistant","content":""# + String(repeating: "x", count: 60_000) + #""},"done":false}"# + "\n"
    for _ in 0..<80 { try consume(record, state: &state) }
    try consume(#"{"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":80}"# + "\n", state: &state)
    #expect(state.done)
    var long = OllamaLanguageModel.StreamState(contextTokens: 131_072)
    try consume(String(repeating: "x", count: 65_536), state: &long)
    #expect(throws: OllamaLanguageModel.Failure.limit) { try long.consume(120) }
}

/// Runs one Ollama turn through a real helper session against `server`,
/// streaming `input` under the sliding input credit.
private func ollamaTurn(_ server: OllamaFixtureServer, input: ModelInput)
    async throws -> (Asura_Model_V1_Hello, Asura_Model_V1_Terminal) {
    var fds: [Int32] = [-1, -1]
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
    let helper = try Transport(fd: fds[0]); let service = try Transport(fd: fds[1])
    let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, factory: { selected, root, endpoint, capabilities in
        try await ProviderFactory.make(selected, assetRoot: root, endpoint: endpoint, capabilities: capabilities)
    }, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    let channel = ToolSessionChannel(helper: helper, service: service, running: running)
    defer { channel.close() }
    var hello = Asura_Model_V1_Hello()
    hello.buildID = identity; hello.schemaDigest = identity; hello.maxFrameBytes = 65_536
    hello.availability = .unknown; hello.reason = .none; hello.selectedModel = "ollama:fixture"
    hello.endpoint = "http://127.0.0.1:\(server.port)"
    var helloEnvelope = Envelope(); helloEnvelope.body = .hello(hello)
    try await service.send(helloEnvelope)
    let greeting = try await channel.next().hello
    let data = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "ollama:fixture"; begin.inputBytes = UInt64(data.count)
    try await channel.send(.begin(begin)); _ = try await channel.next()
    var offset = 0
    var ordinal: UInt64 = 0
    while offset < data.count {
        let end = min(offset + Limits.chunk, data.count)
        var chunk = Asura_Model_V1_Chunk(); chunk.transferID = 1; chunk.direction = .input
        chunk.ordinal = ordinal; chunk.revision = 0; chunk.data = data.subdata(in: offset..<end)
        try await service.send(channel.scoped(.chunk(chunk)), control: false)
        offset = end; ordinal += 1
        if offset < data.count { _ = try await channel.next() }
    }
    var end = Asura_Model_V1_InputEnd(); end.count = ordinal; end.totalBytes = UInt64(data.count)
    try await service.send(channel.scoped(.inputEnd(end)), control: false); _ = try await channel.next()
    var credit = Asura_Model_V1_Credit(); credit.transferID = 2; credit.direction = .output
    credit.acceptedBytes = 0; credit.grantedBytes = 65_536
    try await channel.send(.start(.init()))
    try await channel.send(.credit(credit))
    return (greeting, try await channel.terminal())
}

private let fullWindowShow = #"{"capabilities":["completion"],"model_info":{"granite.context_length":131072}}"#

/// FW-I1 (owner, 2026-10-20): a helper discovering a 131,072-token model
/// reports that window twice and sends a body above the removed 262,144-byte
/// bound with num_ctx 131,072; the turn completes.
@Test func ollamaFullWindowBodyAboveFormerBoundIsSent() async throws {
    let server = try OllamaFixtureServer(chats: [], show: fullWindowShow)
    server.start()
    defer { server.close() }
    var input = ModelInput(); input.instructions = String(repeating: "x", count: 300_000); input.prompt = "window"
    let (hello, terminal) = try await ollamaTurn(server, input: input)
    #expect(hello.availability == .available)
    #expect(hello.contextTokens == 131_072 && hello.reportedContextTokens == 131_072)
    #expect(terminal.outcome == .complete)
    let body = try #require(server.chatBodies.last)
    #expect(body.count > 262_144)
    let value = try JSONSerialization.jsonObject(with: body) as! [String: Any]
    #expect((value["options"] as! [String: Any])["num_ctx"] as? Int == 131_072)
    #expect(value["truncate"] as? Bool == false)
}

/// FW-I2 (2026-10-20): the daemon's over-window refusal (HTTP 400, the
/// verified Ollama 0.40.5 text) and an error record in the stream each fail
/// the turn as a provider failure after one request, with no retry.
@Test(arguments: [
    #"!400 {"error":"request (140000 tokens) exceeds the available context size (131072 tokens), try increasing it"}"#,
    #"{"error":"model failed to load, this may be due to resource limitations"}"#
])
func ollamaWindowErrorsFailTheTurnAsProviderFailure(_ reply: String) async throws {
    let server = try OllamaFixtureServer(chats: [reply], show: fullWindowShow)
    server.start()
    defer { server.close() }
    var input = ModelInput(); input.instructions = ""; input.prompt = "window"
    let (_, terminal) = try await ollamaTurn(server, input: input)
    #expect(terminal.outcome == .failed && terminal.reason == .internalError)
    #expect(server.chatCount == 1)
}

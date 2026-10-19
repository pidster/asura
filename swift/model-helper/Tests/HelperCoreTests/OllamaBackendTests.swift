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
    #expect(text.contextTokens == 8192)
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

@Test func ollamaStreamBoundsAggregateAndTextAndNativeCalls() throws {
    var aggregate = OllamaLanguageModel.StreamState(contextTokens: 4096)
    aggregate.total = 4 * 1024 * 1024
    #expect(throws: OllamaLanguageModel.Failure.limit) { try aggregate.consume(10) }
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

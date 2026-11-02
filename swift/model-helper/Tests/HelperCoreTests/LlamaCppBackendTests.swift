import Darwin
import Foundation
import FoundationModels
import Testing
@testable import HelperCore

// llamacpp-backend.md: LC-U1 to LC-U5 (unit), LC-I1 to LC-I3 (loopback fixture).

private let fixtureProps = #"{"default_generation_settings":{"n_ctx":8192},"chat_template_caps":{"supports_tool_calls":true,"supports_reasoning_effort":false},"chat_template":"{% if enable_thinking %}<think>{% endif %}"}"#
private let fixtureModels = #"{"data":[{"id":"fixture","aliases":["fixture-alias"],"status":{"value":"loaded"},"context_length":262144}],"object":"list"}"#

/// SSE text from chunk JSON lines, ending with `[DONE]`.
private func sse(_ chunks: [String], done: Bool = true) -> String {
    chunks.map { "data: \($0)\n\n" }.joined() + (done ? "data: [DONE]\n\n" : "")
}
private func delta(_ body: String, finish: String? = nil) -> String {
    #"{"choices":[{"index":0,"finish_reason":"# + (finish.map { "\"\($0)\"" } ?? "null") + #","delta":"# + body + "}]}"
}
private let usage = #"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":100}}}"#

/// A loopback llama-server fixture. Chat replies are scripted: an entry that
/// starts with `!<status> ` is sent with that status. It keeps each chat
/// body and each request's Authorization header. Test-only.
final class LlamaFixtureServer: @unchecked Sendable {
    let port: UInt16
    private let socket: Int32
    private let lock = NSLock()
    private var chats: [String]
    private let props: String
    private let models: String
    private var bodies: [Data] = []
    private var authorizations: [String] = []
    private var paths: [String] = []
    init(chats: [String], props: String = fixtureProps, models: String = fixtureModels) throws {
        self.chats = chats; self.props = props; self.models = models
        let listener = Darwin.socket(AF_INET, SOCK_STREAM, 0)
        guard listener >= 0 else { throw HelperError.unavailable }
        var address = sockaddr_in()
        address.sin_family = sa_family_t(AF_INET)
        address.sin_addr.s_addr = inet_addr("127.0.0.1")
        var length = socklen_t(MemoryLayout<sockaddr_in>.size)
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(listener, $0, length) }
        }
        guard bound == 0, listen(listener, 8) == 0 else { Darwin.close(listener); throw HelperError.unavailable }
        _ = withUnsafeMutablePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(listener, $0, &length) }
        }
        socket = listener
        port = UInt16(bigEndian: address.sin_port)
        Thread { [weak self] in self?.serve() }.start()
    }
    var endpoint: URL { URL(string: "http://127.0.0.1:\(port)")! }
    var chatBodies: [Data] { lock.withLock { bodies } }
    var seenAuthorizations: [String] { lock.withLock { authorizations } }
    var seenPaths: [String] { lock.withLock { paths } }
    func close() { Darwin.shutdown(socket, SHUT_RDWR); Darwin.close(socket) }
    private func serve() {
        while true {
            let client = accept(socket, nil, nil)
            guard client >= 0 else { return }
            handle(client)
            Darwin.close(client)
        }
    }
    private func handle(_ client: Int32) {
        var request = Data()
        var buffer = [UInt8](repeating: 0, count: 65_536)
        var header = ""
        var body = Data()
        while true {
            let count = read(client, &buffer, buffer.count)
            guard count > 0 else { return }
            request.append(contentsOf: buffer[..<count])
            guard let end = request.range(of: Data("\r\n\r\n".utf8)) else { continue }
            header = String(decoding: request[..<end.lowerBound], as: UTF8.self)
            let length = header.split(separator: "\r\n").compactMap { line -> Int? in
                let parts = line.split(separator: ":", maxSplits: 1)
                guard parts.count == 2, parts[0].lowercased() == "content-length" else { return nil }
                return Int(parts[1].trimmingCharacters(in: .whitespaces))
            }.first ?? 0
            if request.count >= end.upperBound + length {
                body = request.subdata(in: end.upperBound..<(end.upperBound + length))
                break
            }
        }
        let first = header.split(separator: "\r\n").first.map(String.init) ?? ""
        let authorization = header.split(separator: "\r\n").first { $0.lowercased().hasPrefix("authorization:") }
            .map { String($0.dropFirst("authorization:".count)).trimmingCharacters(in: .whitespaces) } ?? ""
        let path = first.split(separator: " ").dropFirst().first.map(String.init) ?? ""
        lock.withLock { authorizations.append(authorization); paths.append(path) }
        var status = "200 OK"
        var reply: String
        var type = "application/json"
        if first.hasPrefix("GET /v1/models ") { reply = models }
        else if first.hasPrefix("GET /props?model=") { reply = props }
        else if first.hasPrefix("POST /v1/chat/completions ") {
            reply = lock.withLock {
                bodies.append(body)
                return chats.isEmpty ? sse([delta(#"{"content":"done"}"#, finish: "stop"), usage]) : chats.removeFirst()
            }
            if reply.hasPrefix("!") {
                let code = reply.dropFirst().prefix(3)
                status = "\(code) Refused"
                reply = String(reply.dropFirst(5))
            } else { type = "text/event-stream" }
        } else { status = "404 Not Found"; reply = "{}" }
        let response = Data("HTTP/1.1 \(status)\r\nContent-Type: \(type)\r\nContent-Length: \(reply.utf8.count)\r\nConnection: close\r\n\r\n\(reply)".utf8)
        response.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                let count = write(client, raw.baseAddress! + offset, raw.count - offset)
                guard count > 0 else { return }
                offset += count
            }
        }
    }
}

private func consume(_ text: String, _ state: inout LlamaCppLanguageModel.StreamState) throws -> [LlamaCppLanguageModel.Event] {
    var events: [LlamaCppLanguageModel.Event] = []
    for byte in text.utf8 { events += try state.consume(byte) }
    return events
}

// LC-U1
@Test func llamacppSettingsKeepTheLoopbackRuleAndTokenSize() throws {
    for text in ["http://localhost:8934", "http://192.168.1.2:8934", "http://127.0.0.1/v1", "http://u:p@127.0.0.1"] {
        #expect(throws: LlamaCppLanguageModel.Failure.self) {
            try LlamaCppLanguageModel.Settings(name: "m", endpoint: URL(string: text)!)
        }
    }
    #expect(throws: LlamaCppLanguageModel.Failure.self) {
        try LlamaCppLanguageModel.Settings(name: "m", endpoint: URL(string: "http://127.0.0.1:8934")!, token: Data(count: 16))
    }
    #expect(throws: LlamaCppLanguageModel.Failure.self) {
        try LlamaCppLanguageModel.Settings(name: "bad name", endpoint: URL(string: "http://127.0.0.1:8934")!)
    }
    let settings = try LlamaCppLanguageModel.Settings(name: "ggml-org/gemma:Q8_0",
        endpoint: URL(string: "http://127.0.0.1:9931")!, token: Data(repeating: 0xab, count: 32))
    let request = settings.request("/v1/models")
    #expect(request.url?.absoluteString == "http://127.0.0.1:9931/v1/models")
    #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer " + String(repeating: "ab", count: 32))
    #expect(LlamaCppLanguageModel.percentEncoded("ggml-org/gemma:Q8_0") == "ggml-org%2Fgemma%3AQ8_0")
}

// LC-U2
@Test func llamacppDiscoveryUsesTheServerWindowAndDeclaredCaps() throws {
    let settings = try LlamaCppLanguageModel.Settings(name: "fixture", endpoint: URL(string: "http://127.0.0.1:1")!)
    try LlamaCppLanguageModel.listed("fixture", data: Data(fixtureModels.utf8))
    try LlamaCppLanguageModel.listed("fixture-alias", data: Data(fixtureModels.utf8))
    #expect(throws: LlamaCppLanguageModel.Failure.unavailable) {
        try LlamaCppLanguageModel.listed("other", data: Data(fixtureModels.utf8))
    }
    #expect(throws: LlamaCppLanguageModel.Failure.invalidResponse) {
        try LlamaCppLanguageModel.listed("fixture", data: Data("[]".utf8))
    }
    let model = try LlamaCppLanguageModel.checked(settings, props: Data(fixtureProps.utf8))
    // The server's n_ctx, never /v1/models context_length (P3).
    #expect(model.contextTokens == 8192 && model.supportsTools)
    #expect(model.thinkingControls == [.off, .on] && !model.reasoningEffort)
    let effort = try LlamaCppLanguageModel.checked(settings, props: Data(
        #"{"default_generation_settings":{"n_ctx":65536},"chat_template_caps":{"supports_tool_calls":false,"supports_reasoning_effort":true},"chat_template":"enable_thinking"}"#.utf8))
    #expect(!effort.supportsTools && effort.thinkingControls == .all && effort.reasoningEffort)
    let plain = try LlamaCppLanguageModel.checked(settings, props: Data(#"{"default_generation_settings":{"n_ctx":4096}}"#.utf8))
    #expect(plain.thinkingControls.isEmpty && !plain.capabilities.contains(.reasoning))
    for bad in [#"{"default_generation_settings":{"n_ctx":512}}"#, #"{"default_generation_settings":{}}"#,
                #"{"role":"router","default_generation_settings":{"params":null,"n_ctx":0}}"#] {
        #expect(throws: LlamaCppLanguageModel.Failure.unavailable) {
            try LlamaCppLanguageModel.checked(settings, props: Data(bad.utf8))
        }
    }
    #expect(throws: LlamaCppLanguageModel.Failure.invalidResponse) {
        try LlamaCppLanguageModel.checked(settings, props: Data(repeating: 32, count: LlamaCppLanguageModel.discoveryBytes + 1))
    }
}

// LC-U3
@Test func llamacppMessagesUsePositionalCallIDsAndNeverSendThinking() throws {
    let transcript = Transcript(entries: [
        .instructions(.init(segments: [.text(.init(content: "rules"))], toolDefinitions: [])),
        .prompt(.init(segments: [.text(.init(content: "list"))])),
        .toolCalls(.init([Transcript.ToolCall(id: "call_7", toolName: "exec_cmd", arguments: try GeneratedContent(json: #"{"command":"ls"}"#))])),
        .toolOutput(.init(id: "call_7", toolName: "exec_cmd", segments: [.text(.init(content: "a"))])),
        .toolCalls(.init([Transcript.ToolCall(id: "call_8", toolName: "exec_cmd", arguments: try GeneratedContent(json: "{}")),
                          Transcript.ToolCall(id: "call_9", toolName: "job_list", arguments: try GeneratedContent(json: "{}"))])),
        .toolOutput(.init(id: "call_9", toolName: "job_list", segments: [.text(.init(content: "none"))])),
        .toolOutput(.init(id: "call_8", toolName: "exec_cmd", segments: [.text(.init(content: "b"))])),
    ])
    let messages = try LlamaCppLanguageModel.messages(transcript)
    #expect(messages.map(\.role) == ["system", "user", "assistant", "tool", "assistant", "tool", "tool"])
    #expect(messages[2].tool_calls?.map(\.id) == ["call00001"])
    #expect(messages[3].tool_call_id == "call00001")
    #expect(messages[4].tool_calls?.map(\.id) == ["call00002", "call00003"])
    #expect(messages[5].tool_call_id == "call00003" && messages[6].tool_call_id == "call00002")
    #expect(messages[2].tool_calls?.first?.function.arguments.contains("ls") == true)
    // An output without its call cannot be named.
    #expect(throws: LlamaCppLanguageModel.Failure.unsupported) {
        try LlamaCppLanguageModel.messages(Transcript(entries: [
            .toolOutput(.init(id: "x", toolName: "t", segments: [.text(.init(content: ""))]))]))
    }
    let (off, none) = LlamaCppLanguageModel.thinking(.custom("no_think"), effort: true)
    #expect(off?.enable_thinking == false && none == nil)
    let (on, level) = LlamaCppLanguageModel.thinking(.deep, effort: true)
    #expect(on?.enable_thinking == true && level == "high")
    #expect(LlamaCppLanguageModel.thinking(.light, effort: false).1 == nil)
    #expect(LlamaCppLanguageModel.thinking(nil, effort: true).0 == nil)
}

// LC-U4
@Test func llamacppStreamGathersCallsAndRequiresDone() throws {
    var state = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
    var events = try consume(sse([
        delta(#"{"role":"assistant","content":null}"#),
        delta(#"{"reasoning_content":"hm"}"#),
        delta(#"{"content":"Hi"}"#),
        delta(#"{"tool_calls":[{"index":0,"id":"PQ","type":"function","function":{"name":"exec_cmd","arguments":"{\"com"}}]}"#),
        delta(#"{"tool_calls":[{"index":1,"id":"R","function":{"name":"job_list","arguments":"{}"}}]}"#),
        delta(#"{"tool_calls":[{"index":0,"function":{"arguments":"mand\":\"ls\"}"}}]}"#),
        delta("{}", finish: "tool_calls"),
        usage,
    ], done: false), &state)
    #expect(events == [.thinking("hm"), .text("Hi"), .finished("tool_calls"), .usage(input: 120, output: 7, cached: 100)])
    #expect(try state.orderedCalls() == [.init(name: "exec_cmd", arguments: #"{"command":"ls"}"#), .init(name: "job_list", arguments: "{}")])
    events = try consume("data: [DONE]\n\n", &state)
    #expect(events == [.done] && state.done)
    #expect(throws: LlamaCppLanguageModel.Failure.invalidResponse) { _ = try consume("data: {}\n", &state) }

    // [DONE] without a finish, EOF without [DONE].
    var early = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
    #expect(throws: LlamaCppLanguageModel.Failure.incomplete) { _ = try consume("data: [DONE]\n", &early) }
    var cut = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
    _ = try consume(sse([delta(#"{"content":"x"}"#, finish: "stop")], done: false), &cut)
    #expect(try cut.flush().isEmpty && !cut.done)
    // A last [DONE] without a newline still ends the stream.
    var tail = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
    _ = try consume(sse([delta("{}", finish: "stop")], done: false) + "data: [DONE]", &tail)
    #expect(try tail.flush() == [.done] && tail.done)

    // Bounds and invalid shapes.
    var long = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
    _ = try consume(String(repeating: "x", count: LlamaCppLanguageModel.lineBytes), &long)
    #expect(throws: LlamaCppLanguageModel.Failure.limit) { _ = try long.consume(120) }
    var arguments = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
    let big = String(repeating: "a", count: LlamaCppLanguageModel.argumentBytes + 1)
    #expect(throws: LlamaCppLanguageModel.Failure.limit) {
        _ = try consume("data: " + delta(#"{"tool_calls":[{"index":0,"function":{"name":"t","arguments":""# + big + #""}}]}"#) + "\n", &arguments)
    }
    for line in [
        delta(#"{"role":"system","content":"x"}"#),
        #"{"choices":[{"index":1,"delta":{"content":"x"}}]}"#,
        #"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":8193}}"#,
        "not json",
    ] {
        var state = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
        #expect(throws: LlamaCppLanguageModel.Failure.invalidResponse) { _ = try consume("data: " + line + "\n", &state) }
    }
    for line in ["data: " + #"{"error":{"message":"boom"}}"#, #"error: {"message":"boom"}"#] {
        var state = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
        #expect(throws: LlamaCppLanguageModel.Failure.unavailable) { _ = try consume(line + "\n", &state) }
    }
    var index = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
    #expect(throws: LlamaCppLanguageModel.Failure.limit) {
        _ = try consume("data: " + delta(#"{"tool_calls":[{"index":201,"function":{"name":"t"}}]}"#) + "\n", &index)
    }
    var unnamed = LlamaCppLanguageModel.StreamState(contextTokens: 8192)
    _ = try consume("data: " + delta(#"{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}"#) + "\n", &unnamed)
    #expect(throws: LlamaCppLanguageModel.Failure.invalidResponse) { _ = try unnamed.orderedCalls() }
}

// LC-U5
@Test func llamacppRefusalsAndFinishReasonsMapToAsuraFailures() throws {
    let full = LlamaCppLanguageModel.refusal(Data(#"{"error":{"code":400,"type":"exceed_context_size_error","n_ctx":8192}}"#.utf8))
    #expect((full as? BackendFailure)?.reason == .contextLimit)
    let missing = LlamaCppLanguageModel.refusal(Data(#"{"error":{"type":"invalid_request_error","message":"model 'x' not found"}}"#.utf8))
    #expect(missing as? LlamaCppLanguageModel.Failure == .unavailable)
    #expect(LlamaCppLanguageModel.refusal(Data("<html>".utf8)) as? LlamaCppLanguageModel.Failure == .unavailable)
    try LlamaCppLanguageModel.outcome("stop")
    try LlamaCppLanguageModel.outcome("tool_calls")
    #expect { try LlamaCppLanguageModel.outcome("length") } throws: { ($0 as? BackendFailure)?.reason == .contextLimit }
    #expect { try LlamaCppLanguageModel.outcome("content_filter") } throws: { ($0 as? BackendFailure)?.reason == .protocolFault }
}

// MARK: Helper sessions against the fixture (LC-I1 to LC-I3)

private func llamaSession(_ server: LlamaFixtureServer, token: Data? = nil) async throws -> (ToolSessionChannel, Asura_Model_V1_Hello) {
    var fds: [Int32] = [-1, -1]
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0)
    let helper = try Transport(fd: fds[0]); let service = try Transport(fd: fds[1])
    let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, factory: { selected, root, endpoint, capabilities, token in
        try await ProviderFactory.make(selected, assetRoot: root, endpoint: endpoint, capabilities: capabilities,
            endpointToken: token)
    }, buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    let channel = ToolSessionChannel(helper: helper, service: service, running: running)
    channel.selector = "llamacpp:fixture"
    var hello = Asura_Model_V1_Hello()
    hello.buildID = identity; hello.schemaDigest = identity; hello.maxFrameBytes = 65_536
    hello.availability = .unknown; hello.reason = .none; hello.selectedModel = "llamacpp:fixture"
    hello.endpoint = "http://127.0.0.1:\(server.port)"
    if let token { hello.endpointToken = token }
    var envelope = Envelope(); envelope.body = .hello(hello)
    try await service.send(envelope)
    let greeting = try await channel.next()
    return (channel, greeting.hello)
}

// LC-I1
@Test func llamacppHelperStreamsTextAndReportsTheServerWindow() async throws {
    let server = try LlamaFixtureServer(chats: [
        sse([delta(#"{"content":"Hello"}"#), delta(#"{"content":" there"}"#, finish: "stop"), usage]),
    ])
    defer { server.close() }
    let (channel, hello) = try await llamaSession(server)
    defer { channel.close() }
    #expect(hello.availability == .available && hello.contextTokens == 8192 && hello.reportedContextTokens == 8192)
    #expect(hello.contextSource == ContextCapacitySource.llamacpp.rawValue && hello.modelName == "llamacpp:fixture")
    #expect(hello.supportedCapabilities & 1 != 0 && hello.thinkingControls == ThinkingControls([.off, .on]).rawValue)
    var input = ModelInput(); input.instructions = "rules"; input.prompt = "hi"
    try await channel.request(1, input)
    let result = try await channel.collect()
    #expect(result.terminal.outcome == .complete && channel.response == "Hello there")
    let first = try #require(server.chatBodies.first)
    let body = try #require(JSONSerialization.jsonObject(with: first) as? [String: Any])
    #expect(body["model"] as? String == "fixture" && body["stream"] as? Bool == true)
    #expect(body["max_tokens"] == nil && (body["stream_options"] as? [String: Bool])?["include_usage"] == true)
    #expect(server.seenAuthorizations.allSatisfy { $0.isEmpty })
}

@Test func llamacppHelperMapsAFullWindowAndRefusals() async throws {
    let server = try LlamaFixtureServer(chats: [
        "!400 " + #"{"error":{"code":400,"type":"exceed_context_size_error","n_prompt_tokens":9000,"n_ctx":8192}}"#,
    ])
    defer { server.close() }
    let (channel, _) = try await llamaSession(server)
    defer { channel.close() }
    var input = ModelInput(); input.instructions = ""; input.prompt = "long"
    try await channel.request(1, input)
    let result = try await channel.collect()
    #expect(result.terminal.outcome == .failed && result.terminal.reason == .contextLimit)

    let length = try LlamaFixtureServer(chats: [sse([delta(#"{"content":"cut"}"#, finish: "length"), usage])])
    defer { length.close() }
    let (second, _) = try await llamaSession(length)
    defer { second.close() }
    try await second.request(1, input)
    let filled = try await second.collect()
    #expect(filled.terminal.outcome == .failed && filled.terminal.reason == .contextLimit)

    let refused = try LlamaFixtureServer(chats: ["!401 " + #"{"error":{"code":401,"message":"Invalid API Key"}}"#])
    defer { refused.close() }
    let (third, _) = try await llamaSession(refused)
    defer { third.close() }
    try await third.request(1, input)
    let denied = try await third.collect()
    #expect(denied.terminal.outcome == .failed && denied.terminal.reason != .contextLimit)
}

@Test func llamacppHelperIsUnavailableWhenTheModelIsNotListed() async throws {
    let server = try LlamaFixtureServer(chats: [], models: #"{"data":[{"id":"other"}]}"#)
    defer { server.close() }
    let (channel, hello) = try await llamaSession(server)
    defer { channel.close() }
    #expect(hello.availability == .unavailable && hello.reason == .modelUnavailable)
}

// LC-I2: stage B, positional IDs and stable earlier messages.
@Test func llamacppTwoRequestsReturnCallsAndKeepEarlierMessagesStable() async throws {
    let server = try LlamaFixtureServer(chats: [
        sse([delta(#"{"tool_calls":[{"index":0,"id":"zz","type":"function","function":{"name":"exec_cmd","arguments":"{\"command\":\"ls\"}"}}]}"#),
             delta("{}", finish: "tool_calls"), usage]),
        sse([delta(#"{"content":"two files"}"#, finish: "stop"), usage]),
    ])
    defer { server.close() }
    let (channel, _) = try await llamaSession(server)
    defer { channel.close() }
    var input = ModelInput(); input.instructions = "instructions"; input.prompt = "list files"
    try await channel.request(1, input)
    let first = try await channel.collect()
    #expect(first.terminal.outcome == .toolCalls && first.calls.map(\.name) == ["exec_cmd"])
    let arguments = try #require(first.calls.first?.arguments)
    let parsed = try JSONSerialization.jsonObject(with: Data(arguments.utf8)) as? [String: String]
    #expect(parsed == ["command": "ls"])
    input.continuation = continuation([(id: "call_1", name: "exec_cmd", arguments: #"{"command":"ls"}"#, output: "a\nb")])
    try await channel.request(2, input)
    let second = try await channel.collect()
    #expect(second.terminal.outcome == .complete && channel.response == "two files")
    func messages(_ body: Data) throws -> [[String: Any]] {
        let object = try #require(JSONSerialization.jsonObject(with: body) as? [String: Any])
        #expect((object["tools"] as? [[String: Any]])?.isEmpty == false)
        return try #require(object["messages"] as? [[String: Any]])
    }
    let bodies = server.chatBodies
    let one = try messages(bodies[0]); let two = try messages(bodies[1])
    #expect(one.count == 2 && two.count == 4)
    for (a, b) in zip(one, two) { #expect(NSDictionary(dictionary: a) == NSDictionary(dictionary: b)) }
    let call = try #require((two[2]["tool_calls"] as? [[String: Any]])?.first)
    #expect(call["id"] as? String == "call00001")
    #expect(two[3]["role"] as? String == "tool" && two[3]["tool_call_id"] as? String == "call00001"
        && two[3]["content"] as? String == "a\nb")
}

// LC-I3: the relay token is the bearer credential of every request.
@Test func llamacppHelperSendsTheRelayTokenOnEveryRequest() async throws {
    let server = try LlamaFixtureServer(chats: [])
    defer { server.close() }
    let token = Data((0..<32).map { UInt8($0) })
    let (channel, hello) = try await llamaSession(server, token: token)
    defer { channel.close() }
    #expect(hello.availability == .available)
    var input = ModelInput(); input.instructions = ""; input.prompt = "hi"
    try await channel.request(1, input)
    _ = try await channel.collect()
    let expected = "Bearer " + token.map { String(format: "%02x", $0) }.joined()
    #expect(server.seenAuthorizations.count == 3 && server.seenAuthorizations.allSatisfy { $0 == expected })
    #expect(server.seenPaths == ["/v1/models", "/props?model=fixture", "/v1/chat/completions"])
}

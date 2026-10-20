import Darwin
import Foundation
import Testing
@testable import HelperCore

/// A loopback Ollama fixture: answers /api/show for discovery and serves one
/// scripted /api/chat line per request. A chat entry that starts with
/// `!400 ` is sent with that HTTP status and the rest as its body. Every chat
/// request body is kept. Test-only; bounded by the test.
final class OllamaFixtureServer: @unchecked Sendable {
    let port: UInt16
    private let socket: Int32
    private let lock = NSLock()
    private var chats: [String]
    private let show: String
    private(set) var chatRequests = 0
    private var bodies: [Data] = []
    private var thread: Thread?
    init(chats: [String],
         show: String = #"{"capabilities":["completion","tools"],"model_info":{"fixture.context_length":16384}}"#) throws {
        self.chats = chats; self.show = show
        let listener = Darwin.socket(AF_INET, SOCK_STREAM, 0)
        guard listener >= 0 else { throw HelperError.unavailable }
        var address = sockaddr_in()
        address.sin_family = sa_family_t(AF_INET)
        address.sin_addr.s_addr = inet_addr("127.0.0.1")
        address.sin_port = 0
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
    }
    func start() {
        let thread = Thread { [weak self] in self?.serve() }
        self.thread = thread
        thread.start()
    }
    var chatCount: Int { lock.withLock { chatRequests } }
    var chatBodies: [Data] { lock.withLock { bodies } }
    func close() { Darwin.shutdown(socket, SHUT_RDWR); Darwin.close(socket) }
    private func serve() {
        while true {
            let client = accept(socket, nil, nil)
            guard client >= 0 else { return }
            handle(client)
            Darwin.close(client)
        }
    }
    private func head(_ data: Data) -> String { String(decoding: data.prefix(64), as: UTF8.self) }
    private func handle(_ client: Int32) {
        var request = Data()
        var buffer = [UInt8](repeating: 0, count: 65_536)
        while true {
            let count = read(client, &buffer, buffer.count)
            guard count > 0 else { return }
            request.append(contentsOf: buffer[..<count])
            guard let end = request.range(of: Data("\r\n\r\n".utf8)) else { continue }
            let header = String(decoding: request[..<end.lowerBound], as: UTF8.self)
            let length = header.split(separator: "\r\n").compactMap { line -> Int? in
                let parts = line.split(separator: ":", maxSplits: 1)
                guard parts.count == 2, parts[0].lowercased() == "content-length" else { return nil }
                return Int(parts[1].trimmingCharacters(in: .whitespaces))
            }.first ?? 0
            if request.count >= end.upperBound + length {
                let start = end.upperBound
                lock.withLock { if head(request).hasPrefix("POST /api/chat ") { bodies.append(request.subdata(in: start..<(start + length))) } }
                break
            }
        }
        let text = head(request)
        var body: String
        let type: String
        var status = "200 OK"
        if text.hasPrefix("POST /api/show ") {
            body = show
            type = "application/json"
        } else if text.hasPrefix("POST /api/chat ") {
            body = lock.withLock {
                chatRequests += 1
                return chats.isEmpty
                    ? #"{"message":{"role":"assistant","content":"done"},"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":1}"#
                    : chats.removeFirst()
            }
            if body.hasPrefix("!400 ") { status = "400 Bad Request"; body.removeFirst(5) }
            body += "\n"
            type = "application/x-ndjson"
        } else { return }
        let response = Data("HTTP/1.1 \(status)\r\nContent-Type: \(type)\r\nContent-Length: \(body.utf8.count)\r\nConnection: close\r\n\r\n\(body)".utf8)
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

/// TCR-I09 helper part: the Ollama adapter receives a name that is not
/// registered. The helper sends a ToolCall with that name, the argument JSON and
/// source provider_json; it receives status 3; then the turn fails with
/// protocolFault. The SDK never dispatches the call (no second chat request).
@Test func ollamaUnregisteredNameIsSentForItsRecordThenFailsTheTurn() async throws {
    let server = try OllamaFixtureServer(chats: [
        #"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"delete_everything","arguments":{"path":"/"}}}]},"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":2}"#
    ])
    server.start()
    defer { server.close() }
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
    let greeting = try await channel.next()
    #expect(greeting.hello.availability == .available && greeting.hello.supportedCapabilities & 1 != 0)
    var input = ModelInput(); input.instructions = ""; input.prompt = "tools"
    let data = try input.serializedData()
    var begin = Asura_Model_V1_Begin(); begin.model = "ollama:fixture"; begin.inputBytes = UInt64(data.count)
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
    let call = try await channel.nextToolCall()
    #expect(call.ordinal == 1 && call.name == "delete_everything" && call.source == .providerJson)
    #expect(call.arguments == #"{"path":"\/"}"# || call.arguments == #"{"path":"/"}"#)
    try await channel.reply(1, .invalidArguments, "unknown_tool: unregistered_name")
    let terminal = try await channel.terminal()
    #expect(terminal.outcome == .failed && terminal.reason == .protocolFault)
    #expect(channel.unexpectedToolCalls == 0)
    #expect(server.chatCount == 1)
}

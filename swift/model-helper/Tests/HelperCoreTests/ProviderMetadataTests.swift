import Darwin
import Foundation
import Testing
@testable import HelperCore

@Test func providerDisplayMetadataObeysByteBoundaryWithoutChangingSelector() {
    let boundary = "mlx:" + String(repeating: "é", count: 126)
    #expect(boundary.utf8.count == 256)
    #expect(ProviderMetadata.displayName(boundary) == boundary)
    let selector = boundary + "é"
    #expect(selector.utf8.count == 258)
    #expect(ProviderMetadata.displayName(selector) == nil)
    #expect(ProviderMetadata.displayName(String(repeating: "a", count: 256)) != nil)
    #expect(ProviderMetadata.displayName(String(repeating: "a", count: 257)) == nil)
    for name in ["", "model\nname", "model\u{0}name"] {
        #expect(ProviderMetadata.displayName(name) == nil)
    }
    #expect(ProviderMetadata.displayName(nil) == nil)
}


private struct NamedProvider: ModelBackend {
    let name: String
    func status() async -> BackendStatus { .init(contextTokens: 8192, modelName: name) }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
        snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        throw BackendFailure(.modelUnavailable)
    }
}

@Test func longProviderSelectorSurvivesHelloAndBeginWithoutOversizedDisplayMetadata() async throws {
    var descriptors: [Int32] = [-1, -1]
    try #require(socketpair(AF_UNIX, SOCK_STREAM, 0, &descriptors) == 0)
    let helper = try Transport(fd: descriptors[0])
    let service = try Transport(fd: descriptors[1])
    defer { helper.close(); service.close() }
    let identity = Data(repeating: 1, count: 32)
    let selector = "mlx:" + String(repeating: "é", count: 200)
    let session = HelperSession(transport: helper, backend: NamedProvider(name: selector),
        buildID: identity, schemaDigest: identity)
    let running = Task { await session.run() }
    defer { running.cancel() }
    var replies = service.frames.makeAsyncIterator()
    var hello = Asura_Model_V1_Hello()
    hello.buildID = identity; hello.schemaDigest = identity; hello.maxFrameBytes = 65_536
    hello.availability = .unknown; hello.capabilities = 0; hello.reason = .none
    hello.selectedModel = selector
    var frame = Envelope(); frame.body = .hello(hello)
    try await service.send(frame)
    let reply = try Wire.decode(#require(try await replies.next()))
    #expect(reply.hello.availability == .available)
    #expect(reply.hello.selectedModel == selector)
    #expect(!reply.hello.hasModelName)
    var begin = Asura_Model_V1_Begin()
    begin.model = selector; begin.inputBytes = 10; begin.deadlineRemainingMs = 2_000
    begin.maxResponseTokens = 512
    frame = Envelope(); frame.operationID = Data(repeating: 7, count: 16); frame.generation = 1
    frame.body = .begin(begin)
    try await service.send(frame)
    #expect(try Wire.decode(#require(try await replies.next())).credit.transferID == 1)
    service.close(); helper.close()
    await running.value
}

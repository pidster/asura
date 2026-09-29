import Darwin
import Foundation
import Testing
@testable import HelperCore

private func inventoryRoot() throws -> URL {
    // Use the real macOS directory. Foundation temporary URLs can retain /var,
    // whose symlink alias is intentionally rejected by the production scanner.
    let root = URL(fileURLWithPath: "/private/tmp", isDirectory: true)
        .appending(path: "asura-inventory-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    return root
}
private func metadataFile(_ root: URL, _ path: String, _ contents: String) throws {
    let file = root.appending(path: path)
    try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
    try Data(contents.utf8).write(to: file)
}
private let coreMetadata = #"{"kind":"llm","assets":{"main":"model.aimodel"},"language":{"max_context_length":4096}}"#
private let mlxMetadata = #"{"max_position_embeddings":4096}"#

@Test func inventoryLocalMetadataListsNestedModelsWithoutWeightsOrVendorLoading() throws {
    let root = try inventoryRoot(); defer { try? FileManager.default.removeItem(at: root) }
    try metadataFile(root, "coreai/vendor/model/metadata.json", coreMetadata)
    try metadataFile(root, "mlx/model/config.json", mlxMetadata)
    let core = ModelInventory.local(root: root.path, provider: "coreai")
    #expect(core.rows.map(\.selector) == ["coreai:vendor/model"])
    #expect(core.rows.first?.status == 2)
    #expect(core.issues.isEmpty)
    let mlx = ModelInventory.local(root: root.path, provider: "mlx")
    #expect(mlx.rows.map(\.selector) == ["mlx:model"])
    #expect(mlx.issues.isEmpty)
    // No weights or tokenizer exist: this proves metadata enumeration does not load a model.
    #expect(ModelInventory.local(root: root.appending(path: "missing").path, provider: "mlx").issues.isEmpty)
}

@Test func inventoryRejectsSymlinksMalformedMetadataAndSpecialFiles() throws {
    let root = try inventoryRoot(); defer { try? FileManager.default.removeItem(at: root) }
    try metadataFile(root, "mlx/bad/config.json", "not json")
    try metadataFile(root, "external/config.json", mlxMetadata)
    try FileManager.default.createSymbolicLink(at: root.appending(path: "mlx/link"), withDestinationURL: root.appending(path: "external"))
    try FileManager.default.createDirectory(at: root.appending(path: "mlx/fifo"), withIntermediateDirectories: true)
    #expect(mkfifo(root.appending(path: "mlx/fifo/config.json").path, 0o600) == 0)
    let result = ModelInventory.local(root: root.path, provider: "mlx")
    #expect(result.rows.map(\.selector) == ["mlx:bad"])
    #expect(result.rows.first?.status == 4)
    #expect(!result.issues.isEmpty)
    try FileManager.default.createSymbolicLink(at: root.appending(path: "coreai"), withDestinationURL: root.appending(path: "external"))
    #expect(ModelInventory.local(root: root.path, provider: "coreai").rows.isEmpty)
    #expect(ModelInventory.local(root: root.path, provider: "coreai").issues.first?.reason == "unsafe_asset")
}

@Test func inventoryEnforcesRowsMetadataDepthAndEntryBounds() throws {
    let root = try inventoryRoot(); defer { try? FileManager.default.removeItem(at: root) }
    for index in 0..<22 { try metadataFile(root, "mlx/model\(index)/config.json", mlxMetadata) }
    let rows = ModelInventory.local(root: root.path, provider: "mlx")
    #expect(rows.rows.count == 21)
    #expect(rows.issues.first?.reason == "inventory_limit")
    try metadataFile(root, "coreai/large/metadata.json", String(repeating: "x", count: 65_537))
    #expect(ModelInventory.local(root: root.path, provider: "coreai").issues.first?.reason == "inventory_limit")
    try FileManager.default.removeItem(at: root.appending(path: "coreai"))
    let fullMetadata = coreMetadata + String(repeating: " ", count: 65_536 - coreMetadata.utf8.count)
    for index in 0..<17 { try metadataFile(root, "coreai/model\(index)/metadata.json", fullMetadata) }
    let total = ModelInventory.local(root: root.path, provider: "coreai")
    #expect(total.rows.count == 16)
    #expect(total.issues.first?.reason == "inventory_limit")
    try FileManager.default.removeItem(at: root.appending(path: "coreai"))
    try metadataFile(root, "coreai/a/b/c/d/e/metadata.json", coreMetadata)
    #expect(ModelInventory.local(root: root.path, provider: "coreai").issues.first?.reason == "inventory_limit")
    try FileManager.default.removeItem(at: root.appending(path: "coreai"))
    for index in 0..<257 { try metadataFile(root, "coreai/file\(index)", "") }
    #expect(ModelInventory.local(root: root.path, provider: "coreai").issues.first?.reason == "inventory_limit")
}

@Test func inventoryCatalogValidatesNamesAndCapsRowsAndBytes() throws {
    let result = try ModelInventory.catalog(Data(#"{"models":[{"name":"model:b"},{"name":"model:a"},{"name":"model:a"}]}"#.utf8))
    #expect(result.rows.map(\.selector) == ["ollama:model:a", "ollama:model:b"])
    #expect(result.rows.allSatisfy { $0.status == 3 })
    for json in [#"{"models":[{"name":""}]}"#, #"{"models":[{"name":"bad\nname"}]}"#, "{}"] {
        #expect(throws: (any Error).self) { try ModelInventory.catalog(Data(json.utf8)) }
    }
    let large = try JSONSerialization.data(withJSONObject: ["models": (0..<22).map { ["name": "model\($0)"] }])
    #expect(try ModelInventory.catalog(large).rows.count == 21)
    #expect(try ModelInventory.catalog(large).issues.first?.reason == "inventory_limit")
    #expect(throws: (any Error).self) { try ModelInventory.catalog(Data(repeating: 32, count: 65_537)) }
}

/// Private one-shot HTTP fixture. Only the detached fixture task uses blocking sockets;
/// socket deadlines and cancellation bound cleanup independently of the tested client.
private struct InventoryHTTP {
    let endpoint: String
    let task: Task<Void, Never>
    init(status: Int = 200, body: String, delay: Duration = .zero) throws {
        let listener = socket(AF_INET, SOCK_STREAM, 0)
        guard listener >= 0 else { throw HelperError.unavailable }
        var address = sockaddr_in(); address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET); address.sin_addr.s_addr = inet_addr("127.0.0.1")
        var timeout = timeval(tv_sec: 4, tv_usec: 0)
        _ = setsockopt(listener, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(listener, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
        }
        guard bound == 0, listen(listener, 1) == 0 else { Darwin.close(listener); throw HelperError.unavailable }
        var size = socklen_t(MemoryLayout<sockaddr_in>.size)
        let named = withUnsafeMutablePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(listener, $0, &size) }
        }
        guard named == 0 else { Darwin.close(listener); throw HelperError.unavailable }
        endpoint = "http://127.0.0.1:\(UInt16(bigEndian: address.sin_port))"
        task = Task.detached {
            defer { Darwin.close(listener) }
            let client = accept(listener, nil, nil)
            guard client >= 0 else { return }
            defer { Darwin.close(client) }
            var noSignal: Int32 = 1
            _ = setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noSignal, socklen_t(MemoryLayout<Int32>.size))
            var timeout = timeval(tv_sec: 4, tv_usec: 0)
            _ = setsockopt(client, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
            _ = setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
            var buffer = [UInt8](repeating: 0, count: 4096)
            let read = recv(client, &buffer, buffer.count, 0)
            guard read > 0 else { return }
            #expect(String(decoding: buffer.prefix(read), as: UTF8.self).hasPrefix("GET /api/tags HTTP/"))
            do { try await Task.sleep(for: delay); try Task.checkCancellation() } catch { return }
            let reply = Data("HTTP/1.1 \(status) Test\r\nContent-Length: \(body.utf8.count)\r\nConnection: close\r\n\r\n\(body)".utf8)
            reply.withUnsafeBytes { bytes in
                var offset = 0
                while offset < bytes.count {
                    let count = send(client, bytes.baseAddress!.advanced(by: offset), bytes.count - offset, 0)
                    if count <= 0 { break }; offset += count
                }
            }
        }
    }
}

@Test func inventoryHTTPReturnsCatalogAndPreservesPartialResultsOnErrors() async throws {
    let server = try InventoryHTTP(body: #"{"models":[{"name":"test:one"}]}"#)
    let result = await ModelInventory.collect(root: nil, endpoint: server.endpoint,
        system: { BackendStatus(contextTokens: nil) })
    await server.task.value
    #expect(result.rows.map(\.selector) == ["system", "ollama:test:one"])
    #expect(result.rows.first?.status == 4)
    let unavailable = try InventoryHTTP(status: 500, body: "private error text")
    let error = await ModelInventory.ollama(endpoint: unavailable.endpoint)
    await unavailable.task.value
    #expect(error.issues.first?.reason == "provider_unavailable")
    let oversized = try InventoryHTTP(body: String(repeating: "x", count: 65_537))
    let limit = await ModelInventory.ollama(endpoint: oversized.endpoint)
    await oversized.task.value
    #expect(limit.issues.first?.reason == "inventory_limit")
}

@Test func inventoryHTTPStallAndCancellationSettleWithinBound() async throws {
    let stall = try InventoryHTTP(body: "{}", delay: .seconds(4))
    let clock = ContinuousClock(); let start = clock.now
    let timedOut = await ModelInventory.ollama(endpoint: stall.endpoint)
    #expect(clock.now - start < .seconds(3))
    #expect(timedOut.issues.first?.reason == "provider_timeout")
    stall.task.cancel(); await stall.task.value
    let server = try InventoryHTTP(body: "{}", delay: .seconds(4))
    let task = Task { await ModelInventory.ollama(endpoint: server.endpoint) }
    try await Task.sleep(for: .milliseconds(100)); task.cancel()
    let cancelled = await task.value
    #expect(!cancelled.issues.isEmpty)
    server.task.cancel(); await server.task.value
}

@Test func inventoryWireRejectsWrongDirectionFieldsStatusesAndUnboundedRows() throws {
    var hello = Asura_Model_V1_Hello()
    hello.buildID = Data(repeating: 1, count: 32); hello.schemaDigest = hello.buildID
    hello.maxFrameBytes = 65_536; hello.availability = .unknown; hello.capabilities = 0
    hello.reason = .none; hello.selectedModel = "system"; hello.inventoryOnly = true
    var message = Envelope(); message.body = .hello(hello)
    try Wire.validate(message, from: .service)
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .helper) }
    hello.models = [ModelInventory.row("mlx:model", "mlx", 2)]
    message.body = .hello(hello)
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .helper) }
    hello.models = [ModelInventory.row("system", "system", 4)]
    message.body = .hello(hello); try Wire.validate(message, from: .helper)
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .service) }
    hello.models[0].status = 6; message.body = .hello(hello)
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .helper) }
    hello.models = (0..<65).map { ModelInventory.row("mlx:model\($0)", "mlx", 2) }
    message.body = .hello(hello)
    #expect(throws: HelperError.self) { try Wire.validate(message, from: .helper) }
}

private actor InventoryBackend: ModelBackend {
    var generations = 0
    func status() async -> BackendStatus { BackendStatus(contextTokens: nil) }
    func generate(_ input: ModelInput, maximumTokens: UInt32,
                  snapshot: @escaping @Sendable (Snapshot) async throws -> Void) async throws {
        generations += 1; throw HelperError.unavailable
    }
}

@Test func inventorySessionReturnsHelloThenClosesWithoutStartingGeneration() async throws {
    var descriptors: [Int32] = [-1, -1]
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &descriptors) == 0)
    let helper = try Transport(fd: descriptors[0]); let client = try Transport(fd: descriptors[1])
    defer { helper.close(); client.close() }
    let backend = InventoryBackend(); let identity = Data(repeating: 1, count: 32)
    let session = HelperSession(transport: helper, backend: backend, buildID: identity, schemaDigest: identity)
    let runner = Task { await session.run() }
    let server = try InventoryHTTP(body: #"{"models":[]}"#)
    var hello = Asura_Model_V1_Hello()
    hello.buildID = identity; hello.schemaDigest = identity; hello.maxFrameBytes = 65_536
    hello.availability = .unknown; hello.capabilities = 0; hello.reason = .none
    hello.selectedModel = "system"; hello.inventoryOnly = true; hello.endpoint = server.endpoint
    var envelope = Envelope(); envelope.body = .hello(hello)
    try await client.send(envelope)
    var received = 0
    for try await bytes in client.frames {
        let message = try Wire.decode(bytes, from: .helper)
        guard case .hello(let result) = message.body else { Issue.record("Unexpected inventory response"); break }
        #expect(result.inventoryOnly)
        #expect(result.models.first?.selector == "system")
        #expect(result.models.first?.status == 4)
        #expect(result.availability == .unknown)
        received += 1
    }
    await runner.value; await server.task.value
    #expect(received == 1)
    #expect(await backend.generations == 0)
}

@Test func inventorySystemAvailabilityRequiresUsableContextCapacity() async throws {
    for capacity: UInt32? in [nil, 0, 512, 513] {
        let server = try InventoryHTTP(body: #"{"models":[]}"#)
        let result = await ModelInventory.collect(root: nil, endpoint: server.endpoint,
            system: { BackendStatus(contextTokens: capacity) })
        await server.task.value
        #expect(result.rows.first?.status == ((capacity ?? 0) > 512 ? 1 : 4))
    }
}

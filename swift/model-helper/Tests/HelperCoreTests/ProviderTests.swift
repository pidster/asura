import Foundation
import Testing
@testable import HelperCore

private func withAssetRoot(_ body: (URL) throws -> Void) throws {
    let root = FileManager.default.temporaryDirectory.appending(path: "asura-assets-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root.appending(path: "mlx/example"), withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    try body(root)
}

@Test func assetNamesCannotEscapeManagedProviderRoot() throws {
    try withAssetRoot { root in
        for name in ["", "/tmp/model", "../example", "example/..", "./example", "example//nested", "example\\other"] {
            #expect(throws: (any Error).self) {
                try AssetLocation.directory(root: root.path, provider: "mlx", name: name)
            }
        }
        #expect(throws: (any Error).self) {
            try AssetLocation.directory(root: nil, provider: "mlx", name: "example")
        }
        let path = try AssetLocation.directory(root: root.path, provider: "mlx", name: "example")
        #expect(path.lastPathComponent == "example")
        try FileManager.default.createSymbolicLink(at: root.appending(path: "mlx/link"),
            withDestinationURL: path)
        #expect(throws: (any Error).self) {
            try AssetLocation.directory(root: root.path, provider: "mlx", name: "link")
        }
    }
}

@Test func assetTreeRejectsLinksAndBoundedMetadataRejectsOversize() throws {
    try withAssetRoot { root in
        let directory = root.appending(path: "mlx/example")
        let data = directory.appending(path: "config.json")
        try Data("{}".utf8).write(to: data)
        try AssetLocation.inspect(directory)
        #expect(try AssetLocation.metadata(data) == Data("{}".utf8))
        try Data(repeating: 0, count: AssetLocation.metadataLimit + 1).write(to: data)
        #expect(throws: AssetLocation.Failure.limit) { try AssetLocation.metadata(data) }
        try FileManager.default.createSymbolicLink(at: directory.appending(path: "weights"),
            withDestinationURL: data)
        #expect(throws: AssetLocation.Failure.unsafeAsset) { try AssetLocation.inspect(directory) }
        #expect(throws: AssetLocation.Failure.unsafeAsset) {
            try AssetLocation.requireContained(root.appending(path: "elsewhere"), in: directory)
        }
    }
}

@Test func mlxCapacityUsesModelMetadataAndNeverTokenizerSentinels() throws {
    #expect(try MLXProvider.capacity(from: Data(#"{"max_position_embeddings":128000}"#.utf8)) == 128000)
    #expect(try MLXProvider.capacity(from: Data(#"{"text_config":{"max_position_embeddings":8192}}"#.utf8)) == 8192)
    for value in [#"{"model_max_length":1000000000000000000000000000000}"#,
                  #"{"max_position_embeddings":512}"#,
                  #"{"max_position_embeddings":-1}"#,
                  #"{"max_position_embeddings":4294967296}"#,
                  #"{"max_position_embeddings":1.5}"#] {
        #expect(throws: (any Error).self) { try MLXProvider.capacity(from: Data(value.utf8)) }
    }
}

@Test func providerSelectionDoesNotFallBackOrInferAssetRoot() async throws {
    for selection in ["unknown:name", "system:other", "", "mlx:", "mlx:../escape", "mlx:missing", "coreai:missing"] {
        await #expect(throws: (any Error).self) {
            try await ProviderFactory.make(selection)
        }
    }
}

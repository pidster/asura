import Foundation

/// Local asset inspection, called only in the supervised helper's loading task.
enum AssetLocation {
    enum Failure: Error, Equatable { case invalidPath, missingAsset, unsafeAsset, limit, invalidMetadata }
    static let metadataLimit = 1_048_576

    static func directory(root: String?, provider: String, name: String) throws -> URL {
        guard let root, root.hasPrefix("/"), ["coreai", "mlx"].contains(provider),
              !name.isEmpty, name.utf8.count <= 1024, !name.hasPrefix("/") else { throw Failure.invalidPath }
        let parts = name.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." && !$0.contains("\\") }) else {
            throw Failure.invalidPath
        }
        var url = URL(fileURLWithPath: root, isDirectory: true).standardizedFileURL.resolvingSymlinksInPath()
        for part in [provider] + parts.map(String.init) {
            url.append(path: part, directoryHint: .isDirectory)
            let values = try url.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey])
            guard values.isDirectory == true, values.isSymbolicLink != true else { throw Failure.unsafeAsset }
        }
        return url
    }

    static func inspect(_ directory: URL) throws {
        let keys: Set<URLResourceKey> = [.isRegularFileKey, .isDirectoryKey, .isSymbolicLinkKey, .fileSizeKey]
        var traversalFailed = false
        guard let entries = FileManager.default.enumerator(at: directory,
            includingPropertiesForKeys: Array(keys), options: [], errorHandler: { _, _ in
                traversalFailed = true; return false
            }) else { throw Failure.missingAsset }
        var count = 0
        var total: UInt64 = 0
        for case let entry as URL in entries {
            try Task.checkCancellation()
            count += 1
            guard count <= 8192, entry.pathComponents.count - directory.pathComponents.count <= 16 else { throw Failure.limit }
            let values = try entry.resourceValues(forKeys: keys)
            guard values.isSymbolicLink != true else { throw Failure.unsafeAsset }
            if values.isDirectory == true { continue }
            guard values.isRegularFile == true, let size = values.fileSize, size >= 0 else { throw Failure.unsafeAsset }
            total += UInt64(size)
            guard total <= 128 * 1024 * 1024 * 1024 else { throw Failure.limit }
        }
        guard !traversalFailed else { throw Failure.unsafeAsset }
    }

    static func metadata(_ url: URL) throws -> Data {
        let values = try url.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey])
        guard values.isRegularFile == true, values.isSymbolicLink != true else { throw Failure.unsafeAsset }
        guard let size = values.fileSize, size <= metadataLimit else { throw Failure.limit }
        let file = try FileHandle(forReadingFrom: url)
        defer { try? file.close() }
        let data = try file.read(upToCount: metadataLimit + 1) ?? Data()
        guard data.count <= metadataLimit else { throw Failure.limit }
        return data
    }

    static func requireContained(_ asset: URL, in directory: URL) throws {
        let resolved = asset.standardizedFileURL.resolvingSymlinksInPath().path
        guard resolved.hasPrefix(directory.path + "/") else { throw Failure.unsafeAsset }
    }

    static func capacity(_ value: Int) throws -> UInt32 {
        guard let capacity = UInt32(exactly: value), capacity > 512 else { throw Failure.invalidMetadata }
        return capacity
    }
}

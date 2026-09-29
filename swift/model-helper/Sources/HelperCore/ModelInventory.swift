import Darwin
import Foundation

/// Metadata only. All filesystem work runs in the disposable helper's detached task.
/// Descriptor-relative traversal rejects links without opening weights or vendor models.
enum ModelInventory {
    typealias Row = Asura_Model_V1_ModelInventoryEntry
    typealias Issue = Asura_Model_V1_ModelInventoryIssue
    struct Result: Sendable {
        var rows: [Row] = []
        var issues: [Issue] = []
    }
    enum Failure: Error { case unsafe, metadata, limit, unavailable }

    static func row(_ selector: String, _ provider: String, _ status: UInt32,
                    _ detail: String? = nil) -> Row {
        var row = Row(); row.selector = selector; row.provider = provider; row.status = status
        if let detail { row.detail = detail }
        return row
    }
    static func issue(_ provider: String, _ reason: String) -> Issue {
        var issue = Issue(); issue.provider = provider; issue.reason = reason; return issue
    }
    static func validSelector(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 256 && !value.unicodeScalars.contains {
            CharacterSet.whitespacesAndNewlines.union(.controlCharacters).contains($0)
        }
    }

    static func collect(root: String?, endpoint: String?,
                        system: @escaping @Sendable () async -> BackendStatus = { await SystemBackend().status() }) async -> Result {
        let task = Task.detached {
            async let catalog = ollama(endpoint: endpoint)
            let status = await system()
            let available = (status.contextTokens ?? 0) > 512
            var result = Result(rows: [row("system", "system", available ? 1 : 4,
                ProviderMetadata.displayName(status.modelName) ?? (available ? nil : "model_unavailable"))])
            for provider in ["coreai", "mlx"] {
                let local = local(root: root, provider: provider)
                result.rows += local.rows; result.issues += local.issues
            }
            let remote = await catalog
            result.rows += remote.rows; result.issues += remote.issues
            return result
        }
        return await withTaskCancellationHandler { await task.value } onCancel: { task.cancel() }
    }

    static func reason(_ error: any Error) -> String {
        if error is CancellationError { return "cancelled" }
        switch error {
        case Failure.limit, OllamaLanguageModel.Failure.limit: return "inventory_limit"
        case Failure.unsafe: return "unsafe_asset"
        case Failure.metadata: return "invalid_metadata"
        case let error as URLError where error.code == .timedOut: return "provider_timeout"
        case let error as URLError where error.code == .cancelled: return "cancelled"
        default: return "provider_unavailable"
        }
    }

    static func ollama(endpoint: String?) async -> Result {
        do {
            guard let url = URL(string: endpoint ?? "http://127.0.0.1:11434") else { throw Failure.unavailable }
            let settings = try OllamaLanguageModel.Settings(name: "inventory", endpoint: url)
            let session = OllamaLanguageModel.session(seconds: 2)
            defer { session.invalidateAndCancel() }
            return try await withTaskCancellationHandler {
                let (bytes, response) = try await session.bytes(from: settings.endpoint.appending(path: "api/tags"))
                guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw Failure.unavailable }
                var data = Data()
                for try await byte in bytes {
                    try Task.checkCancellation()
                    guard data.count < 65_536 else { throw Failure.limit }
                    data.append(byte)
                }
                return try catalog(data)
            } onCancel: { session.invalidateAndCancel() }
        } catch { return Result(issues: [issue("ollama", reason(error))]) }
    }

    static func catalog(_ data: Data) throws -> Result {
        struct Catalog: Decodable { let models: [Model]; struct Model: Decodable { let name: String } }
        guard data.count <= 65_536 else { throw Failure.limit }
        let catalog: Catalog
        do { catalog = try JSONDecoder().decode(Catalog.self, from: data) }
        catch { throw Failure.metadata }
        var result = Result()
        var seen = Set<String>()
        for model in catalog.models {
            let selector = "ollama:\(model.name)"
            guard !model.name.isEmpty, validSelector(selector) else { throw Failure.metadata }
            guard seen.insert(selector).inserted else { continue }
            if result.rows.count == 21 {
                result.issues = [issue("ollama", "inventory_limit")]; break
            }
            result.rows.append(row(selector, "ollama", 3, "catalog_only"))
        }
        result.rows.sort { $0.selector < $1.selector }
        return result
    }

    static func metadata(_ data: Data, provider: String) throws {
        guard data.count <= 65_536 else { throw Failure.limit }
        do {
            if provider == "mlx" { _ = try MLXProvider.capacity(from: data) }
            else {
                struct Metadata: Decodable {
                    let kind: String
                    let assets: [String: String]
                    let language: Language
                    struct Language: Decodable { let max_context_length: Int }
                }
                let value = try JSONDecoder().decode(Metadata.self, from: data)
                guard value.kind == "llm", !value.assets.isEmpty,
                      value.assets.values.allSatisfy({ path in
                          !path.isEmpty && !path.hasPrefix("/") && !path.contains("\\") &&
                          path.split(separator: "/", omittingEmptySubsequences: false).allSatisfy {
                              !$0.isEmpty && $0 != "." && $0 != ".."
                          }
                      }) else { throw Failure.metadata }
                _ = try AssetLocation.capacity(value.language.max_context_length)
            }
        } catch is Failure { throw Failure.metadata }
        catch { throw Failure.metadata }
    }

    static func local(root: String?, provider: String) -> Result {
        guard ["coreai", "mlx"].contains(provider), let root, root.hasPrefix("/") else {
            return Result(issues: [issue(provider, "provider_unavailable")])
        }
        var result = Result()
        var count = 0
        var total = 0
        func note(_ reason: String) {
            if result.issues.isEmpty || reason == "inventory_limit" { result.issues = [issue(provider, reason)] }
        }
        func scan(_ fd: Int32, _ name: String, _ depth: Int) throws {
            try Task.checkCancellation()
            let metadataName = provider == "coreai" ? "metadata.json" : "config.json"
            if !name.isEmpty {
                let file = openat(fd, metadataName, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
                if file >= 0 {
                    defer { Darwin.close(file) }
                    var info = stat()
                    guard fstat(file, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG else { throw Failure.unsafe }
                    guard info.st_size >= 0, info.st_size <= 65_536, total + Int(info.st_size) <= 1_048_576 else { throw Failure.limit }
                    var bytes = Data()
                    var buffer = [UInt8](repeating: 0, count: 4096)
                    while true {
                        try Task.checkCancellation()
                        let size = Darwin.read(file, &buffer, min(buffer.count, 65_537 - bytes.count))
                        if size < 0 && errno == EINTR { continue }
                        guard size >= 0 else { throw Failure.unavailable }
                        if size == 0 { break }
                        bytes.append(contentsOf: buffer.prefix(size))
                        guard bytes.count <= 65_536 else { throw Failure.limit }
                    }
                    total += bytes.count
                    guard total <= 1_048_576 else { throw Failure.limit }
                    let selector = "\(provider):\(name)"
                    guard validSelector(selector) else { throw Failure.metadata }
                    guard result.rows.count < 21 else { throw Failure.limit }
                    do {
                        try metadata(bytes, provider: provider)
                        result.rows.append(row(selector, provider, 2, "metadata_only"))
                    } catch {
                        result.rows.append(row(selector, provider, 4, reason(error))); note(reason(error))
                    }
                    return // Assets below a candidate are not inspected or loaded.
                } else if errno != ENOENT { throw errno == ELOOP ? Failure.unsafe : Failure.unavailable }
            }
            let duplicate = dup(fd)
            guard duplicate >= 0 else { throw Failure.unavailable }
            guard let stream = fdopendir(duplicate) else { Darwin.close(duplicate); throw Failure.unavailable }
            defer { closedir(stream) }
            while true {
                errno = 0
                guard let entry = readdir(stream) else {
                    if errno != 0 { throw Failure.unavailable }; break
                }
                let child = withUnsafePointer(to: &entry.pointee.d_name) {
                    $0.withMemoryRebound(to: CChar.self, capacity: Int(MAXNAMLEN) + 1) { String(cString: $0) }
                }
                if child == "." || child == ".." { continue }
                try Task.checkCancellation()
                count += 1
                guard count <= 256 else { throw Failure.limit }
                var info = stat()
                guard fstatat(fd, child, &info, AT_SYMLINK_NOFOLLOW) == 0 else { note("provider_unavailable"); continue }
                let kind = info.st_mode & S_IFMT
                if kind == S_IFLNK || (kind != S_IFREG && kind != S_IFDIR) { note("unsafe_asset"); continue }
                if kind != S_IFDIR { continue }
                guard depth < 4 else { note("inventory_limit"); continue }
                let directory = openat(fd, child, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
                guard directory >= 0 else { note("unsafe_asset"); continue }
                defer { Darwin.close(directory) }
                do { try scan(directory, name.isEmpty ? child : name + "/" + child, depth + 1) }
                catch Failure.limit { throw Failure.limit }
                catch { note(reason(error)) }
            }
        }
        // Open each absolute path component without following links, then stay descriptor-relative.
        var fd = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard fd >= 0 else { return Result(issues: [issue(provider, "provider_unavailable")]) }
        defer { Darwin.close(fd) }
        for component in root.split(separator: "/").map(String.init) + [provider] {
            guard component != ".", component != ".." else { return Result(issues: [issue(provider, "unsafe_asset")]) }
            let next = openat(fd, component, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
            guard next >= 0 else {
                if errno == ENOENT { return result }
                return Result(issues: [issue(provider, "unsafe_asset")])
            }
            Darwin.close(fd); fd = next
        }
        do { try scan(fd, "", 0) } catch { note(reason(error)) }
        result.rows.sort { $0.selector < $1.selector }
        return result
    }
}

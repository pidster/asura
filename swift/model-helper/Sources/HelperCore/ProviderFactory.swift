import Foundation

/// One selection per helper. The service supplies the admitted configuration.
public enum ProviderFactory {
    public enum Failure: Error, Equatable { case invalidSelection, unsupportedProvider }

    public static func make(_ selector: String, assetRoot: String? = nil,
        endpoint: String? = nil, capabilities: UInt32? = nil) async throws -> any ModelBackend {
        let task = Task.detached(priority: .userInitiated) {
            try await selected(selector, assetRoot: assetRoot, endpoint: endpoint, capabilities: capabilities)
        }
        return try await withTaskCancellationHandler {
            try await task.value
        } onCancel: { task.cancel() }
    }

    static func selected(_ selector: String, assetRoot: String?, endpoint: String?, capabilities: UInt32? = nil) async throws -> any ModelBackend {
        guard !selector.isEmpty, selector.utf8.count <= 1024,
            !selector.unicodeScalars.contains(where: {
                CharacterSet.whitespacesAndNewlines.contains($0) || CharacterSet.controlCharacters.contains($0)
            }) else { throw Failure.invalidSelection }
        if selector == "system" { return SystemBackend() }
        guard let separator = selector.firstIndex(of: ":") else { throw Failure.invalidSelection }
        let provider = selector[..<separator].lowercased()
        let name = String(selector[selector.index(after: separator)...])
        guard !name.isEmpty else { throw Failure.invalidSelection }
        try Task.checkCancellation()
        switch provider {
        case "coreai": return try await CoreAIProvider.make(root: assetRoot, name: name)
        case "mlx": return try await MLXProvider.make(root: assetRoot, name: name, capabilities: capabilities)
        case "ollama":
            let settings: OllamaLanguageModel.Settings
            if let endpoint {
                guard let url = URL(string: endpoint) else { throw Failure.invalidSelection }
                settings = try .init(name: name, endpoint: url)
            } else { settings = try .init(name: name) }
            let model = try await OllamaLanguageModel.discover(settings)
            return FoundationBackend(model: model, contextTokens: model.contextTokens,
                reportedContextTokens: model.reportedContextTokens, contextSource: .ollama,
                modelName: "ollama:\(name)", supportsTools: model.supportsTools,
                localToolDestination: model.localToolModel != nil,
                instrumentationFactory: { callback in
                    var instrumented = model
                    instrumented.inputObserver = OllamaInputObserver(snapshot: callback)
                    return instrumented
                })
        default: throw Failure.unsupportedProvider
        }
    }
}

import CoreAILanguageModels
import Foundation
import FoundationModels

/// The native CoreAI bridge supplies tool proposals to the common engine.
enum CoreAIProvider {
    static func make(root: String?, name: String) async throws -> any ModelBackend {
        let directory = try AssetLocation.directory(root: root, provider: "coreai", name: name)
        try AssetLocation.inspect(directory)
        _ = try AssetLocation.metadata(directory.appending(path: "metadata.json"))
        let bundle = try LanguageBundle(at: directory)
        guard bundle.hasEmbeddedTokenizer, let tokenizer = bundle.tokenizerPath else {
            throw AssetLocation.Failure.missingAsset
        }
        try AssetLocation.requireContained(tokenizer, in: directory)
        for key in bundle.componentKeys {
            try AssetLocation.requireContained(bundle.requireModelURL(for: key), in: directory)
        }
        try AssetLocation.requireContained(directory.appending(path: bundle.modelAssetPath), in: directory)
        let capacity = try AssetLocation.capacity(bundle.maxContextLength)
        try Task.checkCancellation()
        let model = try await CoreAILanguageModel(resourcesAt: directory)
        return FoundationBackend(model: model, contextTokens: capacity, contextSource: .coreai,
            modelName: "coreai:\(name)", supportsTools: model.capabilities.contains(.toolCalling))
    }
}

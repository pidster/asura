import Foundation
import FoundationModels
import MLXFoundationModels
import MLXHuggingFace
import MLXLLM
import MLXLMCommon
import Tokenizers

/// Local weights only. The native MLX parser owns model-specific tool syntax.
enum MLXProvider {
    static func capacity(from data: Data) throws -> UInt32 {
        struct Configuration: Decodable {
            let max_position_embeddings: Int?
            let text_config: Text?
            struct Text: Decodable { let max_position_embeddings: Int? }
        }
        let config = try JSONDecoder().decode(Configuration.self, from: data)
        guard let count = config.text_config?.max_position_embeddings ?? config.max_position_embeddings else {
            throw AssetLocation.Failure.invalidMetadata
        }
        return try AssetLocation.capacity(count)
    }

    /// Thinking forms of a declared reasoning model (thinking-level.md):
    /// a template flag toggles; always-on takes only on; no strategy has no
    /// control. The pinned bridge maps every level other than `no_think` to on.
    static func thinkingControls(mask: UInt32?, strategy: ReasoningPromptStrategy?) -> ThinkingControls {
        guard mask.map({ $0 & 4 != 0 }) == true else { return [] }
        switch strategy {
        case .templateFlag?: return [.off, .on]
        case .alwaysOn?: return [.on]
        case .none?, nil: return []
        }
    }

    static func make(root: String?, name: String, capabilities: UInt32?) async throws -> any ModelBackend {
        let directory = try AssetLocation.directory(root: root, provider: "mlx", name: name)
        try AssetLocation.inspect(directory)
        let capacity = try capacity(from: AssetLocation.metadata(directory.appending(path: "config.json")))
        // Local tokenizer files are mandatory; no pretrained-model fallback is admitted.
        _ = try AssetLocation.metadata(directory.appending(path: "tokenizer_config.json"))
        guard FileManager.default.fileExists(atPath: directory.appending(path: "tokenizer.json").path) else {
            throw AssetLocation.Failure.missingAsset
        }
        try Task.checkCancellation()
        let container = try await loadModelContainer(from: directory, using: #huggingFaceTokenizerLoader())
        let configuration = await container.configuration
        let profile = try CapabilityProfile(mask: capabilities, provenance: capabilities == nil ? .undeclared : .configuration)
        let supportsTools = profile.support(.toolCalling) == .supported
        guard !supportsTools || configuration.toolCallFormat != nil else { throw HelperError.unavailable }
        let model = MLXLanguageModel(configuration: configuration,
            capabilities: profile.native,
            weightsLocation: { _ in directory }, load: { _, _ in container })
        return FoundationBackend(model: model, contextTokens: capacity, contextSource: .mlx,
            modelName: "mlx:\(name)", supportsTools: supportsTools, capabilityProfile: profile,
            responseTokensFromWindow: true,
            thinkingControls: thinkingControls(mask: capabilities, strategy: configuration.reasoningConfig?.promptStrategy))
    }
}

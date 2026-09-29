import Foundation
import FoundationModels
import MLXLMCommon
import Testing
@testable import HelperCore

@Test func capabilitySupportAndActivationAreSeparateAndUnknownFailsRequirements() throws {
    let unknown = try CapabilityProfile(mask: nil, provenance: .undeclared)
    #expect(unknown.support(.toolCalling) == .unknown)
    #expect(throws: HelperError.self) { try unknown.require([.toolCalling]) }
    let profile = try CapabilityProfile(mask: 5, provenance: .configuration)
    try profile.require([.toolCalling, .reasoning])
    #expect(throws: HelperError.self) { try profile.require([.vision]) }
    #expect(profile.activation(.toolCalling, toolsEnabled: false) == .disabled)
    #expect(profile.activation(.toolCalling, toolsEnabled: true) == .enabled)
    #expect(profile.activation(.reasoning, toolsEnabled: true) == .providerControlled)
    #expect(profile.activation(.vision, toolsEnabled: true) == .disabled)
    #expect(throws: HelperError.self) { try CapabilityProfile(mask: 16, provenance: .configuration) }
}

@Test func ollamaLocalRoutingRejectsCloudUnknownVersionsAndRemoteMetadata() throws {
    #expect(OllamaLanguageModel.localReference("granite4.1:8b") == "granite4.1:8b:local")
    #expect(OllamaLanguageModel.localReference("model") == "model:latest:local")
    #expect(OllamaLanguageModel.localReference("model:8b:local") == "model:8b:local")
    for name in ["model:cloud", "model:8b-cloud", "model:cloud:local", "model:local:local"] {
        #expect(OllamaLanguageModel.localReference(name) == nil)
    }
    #expect(OllamaLanguageModel.qualifiedLocalRoutingVersion("0.34.4"))
    #expect(OllamaLanguageModel.qualifiedLocalRoutingVersion("0.34.5"))
    for version in ["0.34.3", "0.35.0", "1.0.0", "0.34.4-unknown", "garbage"] {
        #expect(!OllamaLanguageModel.qualifiedLocalRoutingVersion(version))
    }
    #expect(OllamaLanguageModel.localMetadata(Data("{}".utf8)))
    for json in [#"{"remote_host":"https://ollama.com"}"#, #"{"remote_model":"cloud"}"#, #"{"remote_host":false}"#] {
        #expect(!OllamaLanguageModel.localMetadata(Data(json.utf8)))
    }
}


@Test func optionalMLXReasoningCanBeDisabledWithoutChangingSupport() throws {
    #expect(MLXProvider.disablesReasoning(mask: 5, strategy: .templateFlag(key: "enable_thinking", defaultOn: true)))
    #expect(!MLXProvider.disablesReasoning(mask: 5, strategy: .alwaysOn))
    #expect(!MLXProvider.disablesReasoning(mask: 5, strategy: nil))
    #expect(!MLXProvider.disablesReasoning(mask: 1, strategy: .templateFlag(key: "enable_thinking", defaultOn: true)))
    let profile = try CapabilityProfile(mask: 5, provenance: .configuration, reasoningDisabled: true)
    #expect(profile.support(.reasoning) == .supported)
    #expect(profile.activation(.reasoning, toolsEnabled: true) == .disabled)
    #expect(throws: HelperError.self) { try CapabilityProfile(mask: 1, provenance: .configuration, reasoningDisabled: true) }
}

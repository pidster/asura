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

@Test func ollamaLocalRoutingRejectsCloudAndRemoteMetadata() throws {
    #expect(OllamaLanguageModel.localReference("granite4.1:8b") == "granite4.1:8b:local")
    #expect(OllamaLanguageModel.localReference("model") == "model:latest:local")
    #expect(OllamaLanguageModel.localReference("model:8b:local") == "model:8b:local")
    for name in ["model:cloud", "model:8b-cloud", "model:cloud:local", "model:local:local"] {
        #expect(OllamaLanguageModel.localReference(name) == nil)
    }
    #expect(OllamaLanguageModel.localMetadata(Data("{}".utf8)))
    for json in [#"{"remote_host":"https://ollama.com"}"#, #"{"remote_model":"cloud"}"#, #"{"remote_host":false}"#] {
        #expect(!OllamaLanguageModel.localMetadata(Data(json.utf8)))
    }
}


@Test func mlxThinkingControlsFollowTheReasoningStrategy() throws {
    let toggle: ThinkingControls = [.off, .on]
    #expect(MLXProvider.thinkingControls(mask: 5, strategy: .templateFlag(key: "enable_thinking", defaultOn: true)) == toggle)
    #expect(MLXProvider.thinkingControls(mask: 5, strategy: .alwaysOn) == [.on])
    #expect(MLXProvider.thinkingControls(mask: 5, strategy: ReasoningPromptStrategy.none) == [])
    #expect(MLXProvider.thinkingControls(mask: 5, strategy: nil) == [])
    // Undeclared reasoning has no controls, whatever the template says.
    #expect(MLXProvider.thinkingControls(mask: 1, strategy: .templateFlag(key: "enable_thinking", defaultOn: true)) == [])
    #expect(MLXProvider.thinkingControls(mask: nil, strategy: .alwaysOn) == [])
    // Off disables reasoning activation; support is unchanged.
    let profile = try CapabilityProfile(mask: 5, provenance: .configuration)
    #expect(profile.support(.reasoning) == .supported)
    #expect(profile.activation(.reasoning, toolsEnabled: true, thinking: .off) == .disabled)
    #expect(profile.activation(.reasoning, toolsEnabled: true, thinking: .on) == .providerControlled)
}

/// TL-04 to TL-06 and TL-15 (helper): accepted forms and the SDK option
/// each request becomes.
@Test func thinkingRequestsMapToReportedFormsAndSdkLevels() {
    let toggle: ThinkingControls = [.off, .on]
    #expect(toggle.accepts(.none) && toggle.accepts(.off) && toggle.accepts(.on))
    #expect(!toggle.accepts(.low) && !toggle.accepts(.high))
    #expect(!ThinkingControls.all.accepts(.UNRECOGNIZED(9)))
    #expect(ThinkingControls.reasoningLevel(.none) == nil)
    #expect(ThinkingControls.reasoningLevel(.off) == .custom("no_think"))
    #expect(ThinkingControls.reasoningLevel(.on) == .custom("think"))
    #expect(ThinkingControls.reasoningLevel(.low) == .light)
    #expect(ThinkingControls.reasoningLevel(.medium) == .moderate)
    #expect(ThinkingControls.reasoningLevel(.high) == .deep)
}

/// TL-09: thinking text comes from reasoning entries only, one blank line
/// between entries.
@Test func thinkingTextJoinsReasoningEntriesOnly() {
    let entries: [Transcript.Entry] = [
        .prompt(.init(segments: [.text(.init(content: "question"))])),
        .reasoning(.init(segments: [.text(.init(content: "first ")), .text(.init(content: "idea"))])),
        .response(.init(segments: [.text(.init(content: "answer"))])),
        .reasoning(.init(segments: [.text(.init(content: "second"))])),
        .reasoning(.init(segments: []))
    ]
    #expect(thinkingText(entries) == "first idea\n\nsecond")
    #expect(thinkingText([Transcript.Entry]()) == "")
}

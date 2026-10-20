import FoundationModels

public enum ModelCapability: UInt32, CaseIterable, Sendable {
    case toolCalling = 1, guidedGeneration = 2, reasoning = 4, vision = 8
    var native: LanguageModelCapabilities.Capability {
        switch self {
        case .toolCalling: .toolCalling
        case .guidedGeneration: .guidedGeneration
        case .reasoning: .reasoning
        case .vision: .vision
        }
    }
}
public enum CapabilitySupport: Sendable { case supported, unsupported, unknown }
public enum CapabilityProvenance: UInt32, Sendable { case framework = 1, runtime = 2, configuration = 3, undeclared = 4 }
public enum CapabilityActivation: Sendable { case enabled, disabled, providerControlled, unknown }
public struct CapabilityProfile: Sendable {
    public let mask: UInt32?
    public let provenance: CapabilityProvenance
    public init(mask: UInt32?, provenance: CapabilityProvenance) throws {
        guard mask.map({ $0 <= 15 }) ?? true, (mask == nil) == (provenance == .undeclared) else { throw HelperError.unavailable }
        self.mask = mask; self.provenance = provenance
    }
    public init(native: LanguageModelCapabilities, provenance: CapabilityProvenance) {
        self.mask = ModelCapability.allCases.reduce(0) { $0 | (native.contains($1.native) ? $1.rawValue : 0) }
        self.provenance = provenance
    }
    public func support(_ capability: ModelCapability) -> CapabilitySupport {
        guard let mask else { return .unknown }
        return mask & capability.rawValue != 0 ? .supported : .unsupported
    }
    public func require(_ required: [ModelCapability]) throws {
        guard required.allSatisfy({ support($0) == .supported }) else { throw HelperError.unavailable }
    }
    /// `thinking` is the form the service sent; off disables reasoning
    /// (thinking-level.md).
    public func activation(_ capability: ModelCapability, toolsEnabled: Bool,
        thinking: ThinkingRequest = .none) -> CapabilityActivation {
        switch capability {
        case .toolCalling: return toolsEnabled && support(.toolCalling) == .supported ? .enabled : .disabled
        case .guidedGeneration, .vision: return .disabled
        case .reasoning:
            if thinking == .off { return .disabled }
            switch support(.reasoning) {
            case .supported: return .providerControlled
            case .unsupported: return .disabled
            case .unknown: return .unknown
            }
        }
    }
    var native: [LanguageModelCapabilities.Capability] {
        ModelCapability.allCases.filter { support($0) == .supported }.map(\.native)
    }
}

public typealias ThinkingRequest = Asura_Model_V1_ThinkingRequest

/// Thinking forms a loaded model accepts, reported in Hello field 23
/// (thinking-level.md): off 1, on 2, low 4, medium 8, high 16.
public struct ThinkingControls: OptionSet, Sendable, Equatable {
    public let rawValue: UInt32
    public init(rawValue: UInt32) { self.rawValue = rawValue }
    public static let off = ThinkingControls(rawValue: 1)
    public static let on = ThinkingControls(rawValue: 2)
    public static let low = ThinkingControls(rawValue: 4)
    public static let medium = ThinkingControls(rawValue: 8)
    public static let high = ThinkingControls(rawValue: 16)
    public static let all: ThinkingControls = [.off, .on, .low, .medium, .high]

    /// Whether `request` is a form these controls accept. None always is.
    public func accepts(_ request: ThinkingRequest) -> Bool {
        switch request {
        case .none: return true
        case .off: return contains(.off)
        case .on: return contains(.on)
        case .low: return contains(.low)
        case .medium: return contains(.medium)
        case .high: return contains(.high)
        case .UNRECOGNIZED: return false
        }
    }

    /// The SDK option for `request` (thinking-level.md, helper translation).
    /// MLX reads `no_think` as off and any other custom value as on.
    public static func reasoningLevel(_ request: ThinkingRequest) -> ContextOptions.ReasoningLevel? {
        switch request {
        case .none, .UNRECOGNIZED: return nil
        case .off: return .custom("no_think")
        case .on: return .custom("think")
        case .low: return .light
        case .medium: return .moderate
        case .high: return .deep
        }
    }
}

/// Thinking text of one response snapshot: the text segments of every
/// reasoning entry, a blank line between entries.
func thinkingText(_ entries: some Collection<Transcript.Entry>) -> String {
    entries.compactMap { entry -> String? in
        guard case .reasoning(let reasoning) = entry else { return nil }
        let text = reasoning.segments.compactMap { segment -> String? in
            if case .text(let text) = segment { return text.content }
            return nil
        }.joined()
        return text.isEmpty ? nil : text
    }.joined(separator: "\n\n")
}

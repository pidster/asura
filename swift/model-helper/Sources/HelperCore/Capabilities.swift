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
    public let reasoningDisabled: Bool
    public init(mask: UInt32?, provenance: CapabilityProvenance, reasoningDisabled: Bool = false) throws {
        guard mask.map({ $0 <= 15 }) ?? true, (mask == nil) == (provenance == .undeclared) else { throw HelperError.unavailable }
        guard !reasoningDisabled || mask.map({ $0 & 4 != 0 }) == true else { throw HelperError.unavailable }
        self.mask = mask; self.provenance = provenance; self.reasoningDisabled = reasoningDisabled
    }
    public init(native: LanguageModelCapabilities, provenance: CapabilityProvenance) {
        self.mask = ModelCapability.allCases.reduce(0) { $0 | (native.contains($1.native) ? $1.rawValue : 0) }
        self.provenance = provenance
        self.reasoningDisabled = false
    }
    public func support(_ capability: ModelCapability) -> CapabilitySupport {
        guard let mask else { return .unknown }
        return mask & capability.rawValue != 0 ? .supported : .unsupported
    }
    public func require(_ required: [ModelCapability]) throws {
        guard required.allSatisfy({ support($0) == .supported }) else { throw HelperError.unavailable }
    }
    public func activation(_ capability: ModelCapability, toolsEnabled: Bool) -> CapabilityActivation {
        switch capability {
        case .toolCalling: return toolsEnabled && support(.toolCalling) == .supported ? .enabled : .disabled
        case .guidedGeneration, .vision: return .disabled
        case .reasoning:
            if reasoningDisabled { return .disabled }
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

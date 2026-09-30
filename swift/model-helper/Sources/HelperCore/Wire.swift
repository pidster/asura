import Foundation
import SwiftProtobuf

public typealias Envelope = Asura_Model_V1_Envelope
public typealias ModelInput = Asura_Model_V1_ModelInput
public typealias Reason = Asura_Model_V1_Reason

public enum HelperError: Error, Sendable, Equatable {
    case protocolFault, limit, closed, unavailable, contextLimit, timeout
}

public enum Limits {
    public static let frame = 65_536
    public static let input = 65_536
    public static let output = 61_440
    public static let chunk = 16_384
    public static let cumulativeOutput = 4 * 1024 * 1024
}

/// Canonical decoding does not grant admission. Unknown fields are explicitly rejected.
public enum Wire {
    public enum Peer { case service, helper }
    public static func decode(_ data: Data, from peer: Peer? = nil) throws -> Envelope {
        guard !data.isEmpty, data.count <= Limits.frame else { throw HelperError.limit }
        let message = try Envelope(serializedBytes: data)
        guard message.unknownFields.data.isEmpty, let body = message.body else { throw HelperError.protocolFault }
        let known: Bool
        switch body {
        case .contextMeasured(let v): known = v.unknownFields.data.isEmpty
        case .hello(let v): known = v.unknownFields.data.isEmpty
            && v.models.allSatisfy { $0.unknownFields.data.isEmpty }
            && v.issues.allSatisfy { $0.unknownFields.data.isEmpty }
        case .begin(let v): known = v.unknownFields.data.isEmpty
        case .chunk(let v): known = v.unknownFields.data.isEmpty
        case .credit(let v): known = v.unknownFields.data.isEmpty
        case .inputEnd(let v): known = v.unknownFields.data.isEmpty
        case .ready(let v): known = v.unknownFields.data.isEmpty
        case .start(let v): known = v.unknownFields.data.isEmpty
        case .cancel(let v): known = v.unknownFields.data.isEmpty
        case .snapshotEnd(let v): known = v.unknownFields.data.isEmpty
        case .terminal(let v): known = v.unknownFields.data.isEmpty
        case .toolCall(let v):
            let argumentsKnown: Bool
            switch v.arguments {
            case .observeStatus(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .memoryListNotes(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .memoryGetNote(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .memoryCreateNote(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .memoryNoteSources(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .shell(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .readAudit(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .listTools(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .readFile(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case .listDirectory(let args): argumentsKnown = args.unknownFields.data.isEmpty
            case nil: argumentsKnown = false
            }
            known = v.unknownFields.data.isEmpty && argumentsKnown
        case .toolResult(let v): known = v.unknownFields.data.isEmpty
        }
        guard known, try message.serializedData() == data else { throw HelperError.protocolFault }
        if let peer { try validate(message, from: peer) }
        else {
            do { try validate(message, from: .service) }
            catch { try validate(message, from: .helper) }
        }
        return message
    }

    public static func validate(_ value: Envelope, from peer: Peer) throws {
        guard let body = value.body else { throw HelperError.protocolFault }
        let service = peer == .service
        func check(_ valid: Bool) throws { if !valid { throw HelperError.protocolFault } }
        func selector(_ text: String) -> Bool {
            !text.isEmpty && text.utf8.count <= 1024 && !text.unicodeScalars.contains(where: { CharacterSet.whitespacesAndNewlines.union(.controlCharacters).contains($0) })
        }
        func counts(_ count: UInt64, _ bytes: UInt64, _ max: UInt64) -> Bool {
            bytes <= max && count <= bytes && (count == 0) == (bytes == 0)
                && (bytes == 0 || count >= (bytes + 16_383) / 16_384)
        }
        func transfer(_ id: UInt64, _ direction: Asura_Model_V1_TransferDirection) -> Bool {
            direction == .input ? id == 1 : direction == .output && (2...1025).contains(id)
        }
        if case .hello = body {
            try check(!value.hasOperationID && !value.hasGeneration)
        } else {
            try check(value.hasOperationID && value.operationID.count == 16
                && value.operationID.contains(where: { $0 != 0 }) && value.hasGeneration && value.generation > 0)
        }
        switch body {
        case .contextMeasured(let v):
            try check(!service && v.hasInputTokens && v.hasCapacityTokens && v.capacityTokens > 0 && v.inputTokens <= v.capacityTokens)
        case .hello(let v):
            try check(!v.hasSelectedModel || selector(v.selectedModel))
            let expectedContextSource: UInt32? = {
                if !v.hasSelectedModel || v.selectedModel == "system" { return ContextCapacitySource.system.rawValue }
                let selector = v.selectedModel.lowercased()
                if selector.hasPrefix("coreai:") { return ContextCapacitySource.coreai.rawValue }
                if selector.hasPrefix("mlx:") { return ContextCapacitySource.mlx.rawValue }
                if selector.hasPrefix("ollama:") { return ContextCapacitySource.ollama.rawValue }
                return nil
            }()
            try check(!v.hasReportedContextTokens || (!service && !v.inventoryOnly))
            try check(!v.hasContextSource || (!service && !v.inventoryOnly))
            try check(v.hasReportedContextTokens == v.hasContextSource)
            try check(!v.hasAssetRoot || (service && v.assetRoot.hasPrefix("/") && v.assetRoot.utf8.count <= 4096 && !v.assetRoot.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })))
            try check(!v.hasEndpoint || (service && !v.endpoint.isEmpty && v.endpoint.utf8.count <= 2048 && !v.endpoint.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })))
            try check(!v.hasModelName || (!service && !v.modelName.isEmpty && v.modelName.utf8.count <= 256 && !v.modelName.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })))
            try check(v.hasBuildID && v.buildID.count == 32 && v.hasSchemaDigest && v.schemaDigest.count == 32
                && v.hasMaxFrameBytes && v.maxFrameBytes == 65_536 && v.hasAvailability && v.hasCapabilities && v.hasReason)
            try check(!v.hasReasoningDisabled || (!service && !v.inventoryOnly && v.availability == .available
                && (!v.reasoningDisabled || (v.hasSupportedCapabilities && v.supportedCapabilities & 4 != 0 && v.selectedModel.hasPrefix("mlx:")))))
            if v.hasCapabilitySource || v.hasSupportedCapabilities {
                try check(!service && !v.inventoryOnly && v.availability == .available)
                if v.capabilitySource == 4 { try check(!v.hasSupportedCapabilities && v.capabilities == 1) }
                else {
                    try check(v.hasCapabilitySource && (1...3).contains(v.capabilitySource)
                        && v.hasSupportedCapabilities && v.supportedCapabilities <= 15
                        && ((v.supportedCapabilities & 1 != 0) == (v.capabilities == 3)))
                }
            }
            try check(!v.hasModelCapabilities || (service && !v.inventoryOnly && v.modelCapabilities <= 15 && v.selectedModel.hasPrefix("mlx:")))
            try check(!v.hasLocalToolDestination || (!service && !v.inventoryOnly && v.availability == .available && v.localToolDestination))
            try check(!v.hasInventoryOnly || v.inventoryOnly)
            try check(v.inventoryOnly || (v.models.isEmpty && v.issues.isEmpty))
            if v.inventoryOnly {
                try check(v.hasSelectedModel && v.selectedModel == "system" && !v.hasModelName
                    && v.availability == .unknown && v.capabilities == 0 && !v.hasContextTokens
                    && !v.hasReportedContextTokens && !v.hasContextSource && v.reason == .none)
                if service { try check(v.models.isEmpty && v.issues.isEmpty) }
                else {
                    try check(!v.models.isEmpty && v.models.count <= 64 && v.issues.count <= 4
                        && v.models.contains { $0.provider == "system" && $0.selector == "system" })
                    var seen = Set<String>()
                    var counts: [String: Int] = [:]
                    for row in v.models {
                        counts[row.provider, default: 0] += 1
                        try check(row.hasSelector && ModelInventory.validSelector(row.selector)
                            && row.hasProvider && ["system", "ollama", "coreai", "mlx"].contains(row.provider)
                            && row.hasStatus && (1...5).contains(row.status)
                            && (row.status != 1 || row.provider == "system")
                            && (row.status != 2 || ["coreai", "mlx"].contains(row.provider))
                            && (row.status != 3 || row.provider == "ollama")
                            && counts[row.provider, default: 0] <= (row.provider == "system" ? 1 : 21)
                            && seen.insert(row.selector).inserted
                            && (row.provider == "system" ? row.selector == "system" : row.selector.hasPrefix(row.provider + ":") && row.selector.utf8.count > row.provider.utf8.count + 1)
                            && (!row.hasDetail || ProviderMetadata.displayName(row.detail) != nil))
                    }
                    for issue in v.issues {
                        try check(issue.hasProvider && ["system", "ollama", "coreai", "mlx"].contains(issue.provider)
                            && issue.hasReason && !issue.reason.isEmpty && issue.reason.utf8.count <= 64
                            && issue.reason.utf8.allSatisfy { (97...122).contains($0) || $0 == 95 })
                    }
                }
            } else if service {
                try check(v.availability == .unknown && v.capabilities == 0 && !v.hasContextTokens
                    && !v.hasReportedContextTokens && !v.hasContextSource && v.reason == .none)
            } else if v.availability == .available {
                try check((v.capabilities == 1 || v.capabilities == 3) && v.hasContextTokens
                    && v.contextTokens > 512 && v.hasReportedContextTokens
                    && v.reportedContextTokens >= v.contextTokens
                    && v.hasContextSource && expectedContextSource == v.contextSource
                    && v.reason == .none)
            } else {
                try check((v.availability == .unknown || v.availability == .unavailable)
                    && v.capabilities == 0 && !v.hasContextTokens
                    && !v.hasReportedContextTokens && !v.hasContextSource
                    && v.reason == .modelUnavailable)
            }
        case .begin(let v):
            try check(service && v.hasModel && selector(v.model) && v.hasInputBytes && (1...65_536).contains(v.inputBytes)
                && (!v.hasDeadlineRemainingMs || (1...60_000).contains(v.deadlineRemainingMs))
                && v.hasMaxResponseTokens && (1...2048).contains(v.maxResponseTokens))
        case .chunk(let v):
            try check(v.hasTransferID && v.hasDirection && transfer(v.transferID, v.direction)
                && v.direction == (service ? .input : .output) && v.hasData && !v.data.isEmpty && v.data.count <= Limits.chunk
                && v.hasRevision && v.hasOrdinal)
            try check(service ? v.revision == 0 && v.ordinal < 65_536 : (1...1024).contains(v.revision) && v.ordinal < 61_440)
        case .credit(let v):
            try check(v.hasTransferID && v.hasDirection && transfer(v.transferID, v.direction)
                && v.direction == (service ? .output : .input) && v.hasAcceptedBytes && v.hasGrantedBytes
                && v.acceptedBytes <= (service ? 61_440 : 65_536) && v.grantedBytes >= v.acceptedBytes
                && v.grantedBytes - v.acceptedBytes <= 65_536)
        case .inputEnd(let v):
            try check(service && v.hasCount && v.count > 0 && v.hasTotalBytes && counts(v.count, v.totalBytes, 65_536))
        case .ready: try check(!service)
        case .start: try check(service)
        case .cancel(let v): try check(service && v.hasReason && (1...4).contains(v.reason.rawValue))
        case .snapshotEnd(let v):
            try check(!service && v.hasRevision && (1...1024).contains(v.revision) && v.hasCount && v.hasTotalBytes
                && counts(v.count, v.totalBytes, 61_440))
        case .toolCall(let v):
            try check(!service && v.hasOrdinal && (1...8).contains(v.ordinal))
            switch v.arguments {
            case .observeStatus: break
            case .shell(let args):
                try check(args.hasCommand && validShellArguments(command: args.command, cwd: args.hasCwd ? args.cwd : nil, timeoutSeconds: args.hasTimeoutSeconds ? args.timeoutSeconds : nil))
            case .listTools: break
            case .readAudit(let args): try check(args.hasLimit && (1...16).contains(args.limit))
            case .memoryListNotes(let args):
                try check((!args.hasAfter || args.after.utf8.count <= 1024) && args.hasLimit)
            case .memoryGetNote(let args):
                try check(args.hasVersion && args.version.utf8.count <= 1024 && args.hasOffset && args.hasLimit)
            case .memoryCreateNote(let args):
                try check(args.hasBody && args.body.utf8.count <= 16_384
                    && (!args.hasSourceVersion || args.sourceVersion.utf8.count <= 1024))
            case .memoryNoteSources(let args):
                try check(args.hasVersion && args.version.utf8.count <= 1024)
            case .readFile(let args):
                try check(args.hasPath && !args.path.isEmpty && args.path.utf8.count <= 1024
                    && args.hasOffset && args.hasLimit && (1...16_384).contains(args.limit))
            case .listDirectory(let args):
                try check(args.hasPath && !args.path.isEmpty && args.path.utf8.count <= 1024)
            case nil: throw HelperError.protocolFault
            }
        case .toolResult(let v):
            try check(service && v.hasOrdinal && (1...8).contains(v.ordinal)
                && v.hasStatus && (1...7).contains(v.status.rawValue)
                && v.hasText && v.text.utf8.count <= 16_384 && v.hasTruncated)
            if v.status != .success { try check(!v.hasNextOffset) }
        case .terminal(let v):
            try check(!service && v.hasUsageKnown && (v.usageKnown ? v.hasUsageTokens && v.usageTokens <= 2048 : !v.hasUsageTokens)
                && v.hasLastRevision && v.lastRevision <= 1024 && v.hasCount && v.hasTotalBytes
                && counts(v.count, v.totalBytes, 4 * 1024 * 1024) && v.hasOutcome && v.hasReason)
            if v.outcome == .complete { try check(v.lastRevision > 0 && v.reason == .none) }
            else { try check((v.outcome == .failed || v.outcome == .cancelled) && (1...12).contains(v.reason.rawValue)) }
        }
    }

    public static func input(_ data: Data) throws -> ModelInput {
        guard data.count <= Limits.input else { throw HelperError.limit }
        let value = try ModelInput(serializedBytes: data)
        guard value.unknownFields.data.isEmpty, value.hasInstructions, value.hasPrompt,
            !value.prompt.isEmpty, value.prompt.utf8.count <= 32_768, value.history.count <= 32,
            try value.serializedData() == data else { throw HelperError.protocolFault }
        var bytes = value.instructions.utf8.count + value.prompt.utf8.count
        for turn in value.history {
            guard turn.unknownFields.data.isEmpty, turn.hasRole, turn.hasText,
                turn.role == .user || turn.role == .assistant else { throw HelperError.protocolFault }
            bytes += turn.text.utf8.count
        }
        guard bytes <= Limits.input else { throw HelperError.limit }
        return value
    }

    public static func frame(_ message: Envelope) throws -> Data {
        let body = try message.serializedData()
        guard !body.isEmpty, body.count <= Limits.frame else { throw HelperError.limit }
        var size = UInt32(body.count).bigEndian
        var data = withUnsafeBytes(of: &size) { Data($0) }
        data.append(body)
        return data
    }
}

/// Incremental framing, with a cap checked before the body is buffered.
public struct FrameDecoder: Sendable {
    private var buffer = Data()
    public init() {}
    public var isEmpty: Bool { buffer.isEmpty }
    public mutating func feed(_ bytes: Data) throws -> [Data] {
        guard bytes.count <= Limits.chunk, buffer.count + bytes.count <= Limits.frame + 4 + Limits.chunk else {
            throw HelperError.limit
        }
        buffer.append(bytes)
        var result: [Data] = []
        while buffer.count >= 4 {
            let size = buffer.prefix(4).reduce(0) { ($0 << 8) | Int($1) }
            guard size > 0, size <= Limits.frame else { throw HelperError.protocolFault }
            guard buffer.count >= size + 4 else { break }
            guard result.count < 8 else { throw HelperError.limit }
            result.append(Data(buffer.dropFirst(4).prefix(size)))
            buffer.removeFirst(size + 4)
        }
        return result
    }
}

import Foundation

/// Pure protocol state is actor-isolated. SDK work and descriptor readiness progress independently.
public actor HelperSession {
    private enum Phase { case hello, inventory, begin, input, ready, running, terminal }
    private let transport: Transport
    private var backend: (any ModelBackend)?
    private let factory: BackendFactory?
    private var selectedModel = "system"
    private var supportsTools = false
    private var capabilities: CapabilityProfile?
    private var activation: [ModelCapability: CapabilityActivation] = [:]
    /// Hello field 23 and the form Begin carried (thinking-level.md).
    private var thinkingControls: ThinkingControls = []
    private var thinking: ThinkingRequest = .none
    private let buildID: Data
    private let schemaDigest: Data
    private var phase = Phase.hello
    private var operation = Data()
    private var generation: UInt64 = 0
    private var inputData = Data()
    private var expectedInput: UInt64 = 0
    /// Effective capacity reported in Hello; bounds the input allocation.
    private var contextTokens: UInt32 = 0
    private var inputChunks: UInt64 = 0
    private var decodedInput: ModelInput?
    private var projectToolsEnabled = false
    private var toolOrdinal: UInt32 = 0
    private var pendingTool: (UInt32, CheckedContinuation<ToolResult, any Error>)?
    private var modelTask: Task<Void, Never>?
    private var timer: Task<Void, Never>?
    private var timerRevision: UInt64 = 0
    /// The only timer is the kept 250 ms final drain after a terminal (owner,
    /// 2026-10-16). Hello, Begin, input transfer and the Ready-to-Start handoff
    /// have no time limit: each ends on the next message, channel close, a
    /// protocol fault or cancellation. Begin carries no time field (owner,
    /// 2026-10-17); nothing else arms a timer.
    private let timerSleep: @Sendable (UInt32) async throws -> Void
    private var measuredContext = false
    private var settledInferenceOrdinal: UInt32 = 0
    private var activeInferenceOrdinal: UInt32?
    /// The response text as the service holds it after the last SnapshotEnd.
    private var committed = Data()
    /// The newest SDK text not yet committed.
    private var target: Data?
    /// The thinking text as the service holds it, and the newest not yet
    /// committed (thinking-level.md).
    private var committedThinking = Data()
    private var targetThinking: Data?
    /// One revision in flight, at most `Limits.revisionBytes`: the thinking
    /// part first, then the response part from `transmitOffset`.
    private var transmitting: Data?
    private var transmitOffset = 0
    private var transmitThinking: (offset: Int, count: Int)?
    private var transfer: UInt64 = 2
    private var revision: UInt64 = 0
    private var transferChunks: UInt64 = 0
    private var totalChunks: UInt64 = 0
    private var totalBytes: UInt64 = 0
    private var usage: UInt64?
    private var backendFinished = false
    private(set) var pendingFailure: Reason?
    private var pumping = false
    private struct Ledger { var sent: UInt64 = 0; var accepted: UInt64 = 0; var granted: UInt64 = 0 }
    private var ledgers: [UInt64: Ledger] = [2: Ledger()]

    public init(transport: Transport, backend: any ModelBackend, buildID: Data, schemaDigest: Data) {
        self.init(transport: transport, backend: backend, buildID: buildID, schemaDigest: schemaDigest,
            timerSleep: Self.sleep)
    }

    // Inject only the timer boundary; production protocol phases and expiry decisions stay canonical.
    init(transport: Transport, backend: any ModelBackend, buildID: Data, schemaDigest: Data,
        timerSleep: @escaping @Sendable (UInt32) async throws -> Void) {
        self.transport = transport; self.backend = backend; self.factory = nil
        self.buildID = buildID; self.schemaDigest = schemaDigest; self.timerSleep = timerSleep
    }
    private static func sleep(milliseconds: UInt32) async throws {
        try await Task.sleep(for: .milliseconds(Int(milliseconds)))
    }

    public typealias BackendFactory = @Sendable (String, String?, String?, UInt32?) async throws -> any ModelBackend
    public init(transport: Transport, factory: @escaping BackendFactory, buildID: Data, schemaDigest: Data) {
        self.transport = transport; self.factory = factory; self.backend = nil
        self.buildID = buildID; self.schemaDigest = schemaDigest; self.timerSleep = Self.sleep
    }

    public func run() async {
        do {
            guard buildID.count == 32, schemaDigest.count == 32 else { throw HelperError.protocolFault }
            for try await bytes in transport.frames {
                if phase == .terminal { continue }
                try await receive(Wire.decode(bytes, from: .service))
            }
        } catch {
            reportModelFailure(error)
            transport.close()
        }
        finishTool(.failure(HelperError.closed))
        timer?.cancel()
        modelTask?.cancel()
        phase = .terminal
    }

    private func disarm() {
        timer?.cancel(); timer = nil; timerRevision += 1
    }
    private func arm(milliseconds: UInt32) {
        disarm()
        let current = timerRevision
        let sleep = timerSleep
        timer = Task { [weak self] in
            do { try await sleep(milliseconds) }
            catch { return }
            await self?.expired(current)
        }
    }
    /// The kept 250 ms final drain after a terminal elapsed: close the channel.
    private func expired(_ current: UInt64) async {
        guard current == timerRevision else { return }
        phase = .terminal; modelTask?.cancel(); transport.close()
    }
    private func envelope(_ body: Envelope.OneOf_Body) -> Envelope {
        var value = Envelope()
        if !operation.isEmpty { value.operationID = operation; value.generation = generation }
        value.body = body
        return value
    }

    private func inventoryFinished(_ inventory: ModelInventory.Result) async {
        guard phase == .inventory, !Task.isCancelled else { return }
        var response = Asura_Model_V1_Hello()
        response.buildID = buildID; response.schemaDigest = schemaDigest
        response.maxFrameBytes = UInt32(Limits.frame)
        response.selectedModel = "system"; response.inventoryOnly = true
        response.availability = .unknown; response.reason = .none
        response.models = inventory.rows; response.issues = inventory.issues
        do { try await transport.send(envelope(.hello(response))) }
        catch { transport.close() }
        phase = .terminal; timer?.cancel(); transport.finish()
    }

    private func receive(_ message: Envelope) async throws {
        guard let body = message.body, phase != .terminal else { throw HelperError.protocolFault }
        if phase == .hello {
            guard case .hello(let hello) = body, !message.hasOperationID, !message.hasGeneration,
                hello.hasBuildID, hello.buildID == buildID, hello.hasSchemaDigest, hello.schemaDigest == schemaDigest,
                hello.hasMaxFrameBytes, hello.maxFrameBytes == UInt32(Limits.frame),
                hello.hasAvailability, hello.availability == .unknown,
                hello.hasSelectedModel, !hello.hasContextTokens,
                !hello.hasReportedContextTokens, !hello.hasContextSource,
                hello.hasReason, hello.reason == .none else { throw HelperError.protocolFault }
            selectedModel = hello.selectedModel
            if hello.inventoryOnly {
                phase = .inventory
                let root = hello.hasAssetRoot ? hello.assetRoot : nil
                let endpoint = hello.hasEndpoint ? hello.endpoint : nil
                let injected = backend
                modelTask = Task {
                    let inventory = await ModelInventory.collect(root: root, endpoint: endpoint,
                        system: {
                            if let injected { return await injected.status() }
                            return await SystemBackend().status()
                        })
                    await self.inventoryFinished(inventory)
                }
                return
            }
            if let factory {
                backend = try? await factory(selectedModel, hello.hasAssetRoot ? hello.assetRoot : nil,
                    hello.hasEndpoint ? hello.endpoint : nil, hello.hasModelCapabilities ? hello.modelCapabilities : nil)
            }
            let status = await backend?.status() ?? BackendStatus(contextTokens: nil)
            supportsTools = status.supportsTools
            capabilities = status.capabilityProfile
            thinkingControls = status.thinkingControls
            // Timer may have closed discovery while the SDK availability observation was pending.
            guard phase == .hello else { throw HelperError.closed }
            var response = Asura_Model_V1_Hello()
            response.buildID = buildID; response.schemaDigest = schemaDigest
            response.maxFrameBytes = UInt32(Limits.frame)
            response.selectedModel = selectedModel
            if let context = status.contextTokens, context > 512,
                let reported = status.reportedContextTokens, reported >= context,
                let source = status.contextSource {
                response.availability = .available
                response.contextTokens = context; response.reason = .none
                response.reportedContextTokens = reported
                response.contextSource = source.rawValue
                if status.localToolDestination { response.localToolDestination = true }
                if status.supportsTools { response.toolSchemaBytes = projectToolSchemaBytes() }
                contextTokens = context
                if let profile = status.capabilityProfile {
                    response.capabilitySource = profile.provenance.rawValue
                    if let mask = profile.mask { response.supportedCapabilities = mask }
                    // Controls need a known source with reasoning support.
                    if !status.thinkingControls.isEmpty, profile.provenance != .undeclared,
                        profile.support(.reasoning) == .supported {
                        response.thinkingControls = status.thinkingControls.rawValue
                    }
                }
                if let name = ProviderMetadata.displayName(status.modelName) { response.modelName = name }
            } else {
                response.availability = .unavailable
                response.reason = .modelUnavailable
            }
            try await transport.send(envelope(.hello(response)))
            if response.availability != .available { phase = .terminal; transport.finish(); return }
            phase = .begin
            return
        }
        guard message.hasOperationID, message.operationID.count == 16,
            message.operationID.contains(where: { $0 != 0 }), message.hasGeneration, message.generation > 0 else {
            throw HelperError.protocolFault
        }
        if phase == .begin {
            guard case .begin(let begin) = body, begin.hasModel, begin.model == selectedModel,
                begin.hasInputBytes, begin.inputBytes > 0, begin.inputBytes <= Limits.input,
                // A sanity bound on allocation, not a content limit.
                begin.inputBytes <= UInt64(contextTokens) * 8 else { throw HelperError.protocolFault }
            // Only a form reported in Hello (thinking-level.md); absent is none.
            if begin.hasThinking {
                guard begin.thinking != .none, thinkingControls.accepts(begin.thinking) else {
                    throw HelperError.protocolFault
                }
                thinking = begin.thinking
            }
            operation = message.operationID; generation = message.generation
            expectedInput = begin.inputBytes
            projectToolsEnabled = begin.hasEnableProjectTools && begin.enableProjectTools
            if let capabilities {
                activation = Dictionary(uniqueKeysWithValues: ModelCapability.allCases.map {
                    ($0, capabilities.activation($0, toolsEnabled: projectToolsEnabled, thinking: thinking))
                })
            }
            if projectToolsEnabled && (!supportsTools || !(backend is any ToolModelBackend)) { throw HelperError.unavailable }
            phase = .input
            var credit = Asura_Model_V1_Credit()
            credit.transferID = 1; credit.direction = .input; credit.acceptedBytes = 0
            credit.grantedBytes = Limits.creditWindow
            try await transport.send(envelope(.credit(credit)))
            return
        }
        guard message.operationID == operation, message.generation == generation else { throw HelperError.protocolFault }
        if case .cancel(let cancel) = body {
            guard cancel.hasReason, (1...4).contains(cancel.reason.rawValue) else { throw HelperError.protocolFault }
            await fail(cancel.reason == .timeout ? .timeout : .cancelled, cancelled: true)
            return
        }
        switch body {
        case .chunk(let chunk):
            guard phase == .input, chunk.hasTransferID, chunk.transferID == 1,
                chunk.hasDirection, chunk.direction == .input, chunk.hasOrdinal, chunk.ordinal == inputChunks,
                chunk.hasRevision, chunk.revision == 0, chunk.hasData, !chunk.data.isEmpty,
                chunk.data.count <= Limits.chunk, UInt64(inputData.count + chunk.data.count) <= expectedInput else {
                throw HelperError.protocolFault
            }
            inputData.append(chunk.data); inputChunks += 1
            // Each accepted chunk advances the sliding window.
            if UInt64(inputData.count) < expectedInput {
                var credit = Asura_Model_V1_Credit()
                credit.transferID = 1; credit.direction = .input
                credit.acceptedBytes = UInt64(inputData.count)
                credit.grantedBytes = UInt64(inputData.count) + Limits.creditWindow
                try await transport.send(envelope(.credit(credit)))
            }
        case .inputEnd(let end):
            guard phase == .input, end.hasCount, end.count == inputChunks, end.hasTotalBytes,
                end.totalBytes == expectedInput, UInt64(inputData.count) == expectedInput else { throw HelperError.protocolFault }
            decodedInput = try Wire.input(inputData)
            inputData.removeAll()
            phase = .ready
            try await transport.send(envelope(.ready(.init())))
        case .start:
            guard phase == .ready, let input = decodedInput else { throw HelperError.protocolFault }
            decodedInput = nil; phase = .running
            guard let backend = self.backend else { throw HelperError.unavailable }
            let owner = self
            let enableTools = projectToolsEnabled
            let thinking = self.thinking
            modelTask = Task {
                do {
                    if enableTools, let toolBackend = backend as? any ToolModelBackend {
                        try await toolBackend.generateWithTools(input, thinking: thinking,
                            handler: { proposal in try await owner.requestTool(proposal) },
                            snapshot: { value in try await owner.snapshot(value) })
                    } else {
                        try await backend.generate(input, thinking: thinking) { value in
                            try await owner.snapshot(value)
                        }
                    }
                    await owner.completed()
                } catch is CancellationError {
                    await owner.backendCancelled(taskCancelled: Task.isCancelled)
                } catch let error as BackendFailure {
                    reportModelFailure(error)
                    await owner.backendFailed(error.reason)
                } catch {
                    reportModelFailure(error)
                    await owner.backendFailed(.internalError)
                }
            }
        case .toolResult(let result):
            guard phase == .running, projectToolsEnabled,
                let pending = pendingTool, result.ordinal == pending.0,
                // No turn result-byte total (owner, 2026-10-18); Wire bounds each result.
                result.ordinal < Limits.toolProposals || result.status == .limit else { throw HelperError.protocolFault }
            finishTool(.success(ToolResult(status: result.status, text: result.text,
                nextOffset: result.hasNextOffset ? result.nextOffset : nil, truncated: result.truncated)))
        case .credit(let credit):
            guard phase == .running, credit.hasTransferID,
                var ledger = ledgers[credit.transferID], credit.hasDirection, credit.direction == .output,
                credit.hasAcceptedBytes, credit.hasGrantedBytes, credit.acceptedBytes >= ledger.accepted,
                credit.acceptedBytes <= ledger.sent, credit.grantedBytes > ledger.granted,
                credit.grantedBytes >= ledger.sent, credit.grantedBytes <= credit.acceptedBytes + UInt64(Limits.frame) else {
                throw HelperError.protocolFault
            }
            ledger.accepted = credit.acceptedBytes; ledger.granted = credit.grantedBytes
            ledgers[credit.transferID] = ledger
            do { try await pump() }
            catch let error as BackendFailure { await fail(error.reason, cancelled: false) }
            catch { await fail(.protocolFault, cancelled: false) }
        default: throw HelperError.protocolFault
        }
    }

    private func finishTool(_ result: Result<ToolResult, any Error>, ordinal: UInt32? = nil) {
        guard let pending = pendingTool, ordinal == nil || pending.0 == ordinal else { return }
        pendingTool = nil
        pending.1.resume(with: result)
    }

    /// Forwards one proposal unread: name and argument text, or their sizes when
    /// over the wire limits (known-size exhaustion). Ordinals run 1 through
    /// Limits.toolProposals; after the result of the last, over-limit ordinal the
    /// turn fails with outputLimit (TCR-D6). The name
    /// and arguments are never logged.
    private func requestTool(_ proposal: ToolProposal) async throws -> ToolResult {
        guard phase == .running, pendingFailure == nil, projectToolsEnabled else { throw CancellationError() }
        // A concurrent callback gets the busy reply: no frame, no ordinal, no
        // record; the turn continues (model-tool-execution.md, EXQ-D1).
        guard pendingTool == nil else { return ToolResult.busy }
        guard toolOrdinal < Limits.toolProposals else { throw BackendFailure(.outputLimit) }
        toolOrdinal += 1
        var call = Asura_Model_V1_ToolCall()
        call.ordinal = toolOrdinal
        call.source = proposal.source
        let nameBytes = proposal.name.utf8.count
        if nameBytes <= Limits.toolName { call.name = proposal.name }
        else { call.nameBytes = UInt32(clamping: nameBytes) }
        let argumentBytes = proposal.arguments.utf8.count
        if argumentBytes <= Limits.toolArguments { call.arguments = proposal.arguments }
        else { call.argumentsBytes = UInt32(clamping: argumentBytes) }
        let requestOrdinal = toolOrdinal
        let message = envelope(.toolCall(call))
        try Wire.validate(message, from: .helper)
        let result = try await withCheckedThrowingContinuation { continuation in
            pendingTool = (toolOrdinal, continuation)
            Task {
                // A ToolCall can hold 64,000 bytes of argument text, beyond the
                // 4,096-byte control frame bound, so it travels on the ordered
                // data queue (bounded at one frame plus 1,024 bytes).
                do { try await transport.send(message, control: false) }
                catch { finishTool(.failure(error), ordinal: requestOrdinal) }
            }
        }
        if requestOrdinal == Limits.toolProposals {
            // The over-limit proposal is recorded; the turn ends here.
            await backendFailed(.outputLimit)
            throw BackendFailure(.outputLimit)
        }
        return result
    }

    private func snapshot(_ value: Snapshot) async throws {
        guard phase == .running, pendingFailure == nil else { throw CancellationError() }
        if let ordinal = value.inferenceStartedOrdinal {
            guard value.text.isEmpty, value.usageTokens == nil, value.inputContext == nil,
                value.inferenceUsage == nil, activeInferenceOrdinal == nil,
                // No request count per turn (owner, 2026-10-18); UInt32 exhaustion is a fault.
                settledInferenceOrdinal < UInt32.max, ordinal == settledInferenceOrdinal + 1 else { throw HelperError.protocolFault }
            activeInferenceOrdinal = ordinal
            return
        }
        if let report = value.inferenceUsage {
            guard value.text.isEmpty, value.usageTokens == nil, value.inputContext == nil,
                report.ordinal == activeInferenceOrdinal, report.ordinal == settledInferenceOrdinal + 1 else {
                throw HelperError.protocolFault
            }
            let message = envelope(.inferenceUsage(report.wire))
            try Wire.validate(message, from: .helper)
            settledInferenceOrdinal = report.ordinal; activeInferenceOrdinal = nil
            try await transport.send(message)
            return
        }
        if let context = value.inputContext {
            guard !measuredContext, value.text.isEmpty, value.usageTokens == nil,
                context.capacity > 0, context.tokens <= context.capacity else { throw HelperError.protocolFault }
            measuredContext = true
            var measured = Asura_Model_V1_ContextMeasured()
            measured.inputTokens = context.tokens; measured.capacityTokens = context.capacity
            try await transport.send(envelope(.contextMeasured(measured)))
            return
        }
        // No response size, revision or transmitted-byte total (owner, 2026-10-18).
        target = Data(value.text.utf8); usage = value.usageTokens
        if let thought = value.thinking { targetThinking = Data(thought.utf8) }
        try await pump()
    }
    private func backendCancelled(taskCancelled: Bool) async {
        // Control cancellation and teardown set terminal before cancelling the task.
        guard phase == .running else { return }
        reportModelFailure(UnexpectedBackendCancellation(taskCancelled: taskCancelled))
        await backendFailed(.internalError)
    }
    private func backendFailed(_ reason: Reason) async {
        guard phase == .running, pendingFailure == nil else { return }
        pendingFailure = reason
        backendFinished = true
        finishTool(.failure(CancellationError()))
        do { try await pump() }
        catch { await fail(reason, cancelled: false) }
    }
    private func completed() async {
        guard phase == .running else { return }
        backendFinished = true
        if revision == 0 && target == nil { target = Data() }
        do { try await pump() } catch { await fail(.outputLimit, cancelled: false) }
    }

    private func pump() async throws {
        guard !pumping, phase == .running else { return }
        pumping = true
        defer { pumping = false }
        while phase == .running {
            if transmitting == nil {
                if target == nil && targetThinking == nil {
                    if backendFinished {
                        try await terminal(pendingFailure == nil ? .complete : .failed,
                            reason: pendingFailure ?? .none)
                    }
                    return
                }
                guard let part = Self.revision(committed: committed, target: target,
                    committedThinking: committedThinking, targetThinking: targetThinking,
                    first: revision == 0) else {
                    // Both texts are committed: nothing to send.
                    target = nil; targetThinking = nil
                    continue
                }
                guard revision < UInt64.max - 1 else { throw HelperError.protocolFault }
                revision += 1; transferChunks = 0
                transmitting = part.bytes
                transmitOffset = part.offset
                transmitThinking = part.thinking
            }
            guard let bytes = transmitting, var ledger = ledgers[transfer] else { throw HelperError.protocolFault }
            while ledger.sent < UInt64(bytes.count) {
                guard ledger.granted > ledger.sent else { return }
                let count = min(Limits.chunk, Int(ledger.granted - ledger.sent), bytes.count - Int(ledger.sent))
                guard totalBytes <= UInt64.max - UInt64(count), totalChunks < UInt64.max else { throw HelperError.protocolFault }
                var chunk = Asura_Model_V1_Chunk()
                chunk.transferID = transfer; chunk.direction = .output
                chunk.ordinal = transferChunks; chunk.revision = revision
                chunk.data = bytes.subdata(in: Int(ledger.sent)..<(Int(ledger.sent) + count))
                ledger.sent += UInt64(count); transferChunks += 1; totalChunks += 1; totalBytes += UInt64(count)
                ledgers[transfer] = ledger
                try await transport.send(envelope(.chunk(chunk)), control: false)
                guard phase == .running else { return }
                ledger = ledgers[transfer]!
            }
            var end = Asura_Model_V1_SnapshotEnd()
            end.revision = revision; end.count = transferChunks; end.totalBytes = UInt64(bytes.count)
            end.offset = UInt64(transmitOffset)
            var responseBytes = bytes
            if let part = transmitThinking {
                end.thinkingOffset = UInt64(part.offset); end.thinkingBytes = UInt64(part.count)
                committedThinking.removeSubrange(part.offset..<committedThinking.count)
                committedThinking.append(bytes.prefix(part.count))
                responseBytes = bytes.subdata(in: part.count..<bytes.count)
            }
            let old = transfer
            transfer += 1
            ledgers = [old: ledger, transfer: Ledger()]
            committed.removeSubrange(transmitOffset..<committed.count)
            committed.append(responseBytes)
            // A target is done when the service holds it, unless a newer
            // snapshot replaced it.
            if target == committed { target = nil }
            if targetThinking == committedThinking { targetThinking = nil }
            transmitting = nil; transmitThinking = nil
            // Keep SnapshotEnd ordered after the data it terminates.
            try await transport.send(envelope(.snapshotEnd(end)), control: false)
        }
    }

    /// One revision from the committed and target texts (thinking-level.md):
    /// the thinking change first, then the response change in the remaining
    /// budget of `Limits.revisionBytes`. A part without a change, or without
    /// budget, keeps its committed length. Returns nil when nothing changes,
    /// except that the first revision is always sent.
    static func revision(committed: Data, target: Data?, committedThinking: Data, targetThinking: Data?,
        first: Bool) -> (bytes: Data, offset: Int, thinking: (offset: Int, count: Int)?)? {
        func boundary(_ data: Data, _ prefix: Int, _ budget: Int) -> Int {
            var end = min(data.count, prefix + budget)
            while end < data.count && end > prefix && data[end] & 0xC0 == 0x80 { end -= 1 }
            return end
        }
        var budget = Limits.revisionBytes
        var bytes = Data()
        var thinking: (offset: Int, count: Int)?
        if let next = targetThinking {
            let prefix = sharedPrefix(committedThinking, next)
            if !(prefix == committedThinking.count && prefix == next.count) {
                let end = boundary(next, prefix, budget)
                bytes.append(next.subdata(in: prefix..<end))
                thinking = (prefix, end - prefix)
                budget -= end - prefix
            }
        }
        var offset = committed.count
        if let next = target {
            let prefix = sharedPrefix(committed, next)
            if !(prefix == committed.count && prefix == next.count) {
                let end = boundary(next, prefix, budget)
                // A cut needs no budget; an append waits for budget.
                if end > prefix || prefix < committed.count {
                    bytes.append(next.subdata(in: prefix..<end))
                    offset = prefix
                }
            }
        }
        if !first && thinking == nil && offset == committed.count && bytes.isEmpty { return nil }
        return (bytes, offset, thinking)
    }

    /// Length of the common prefix, backed off to a UTF-8 character boundary
    /// in both texts.
    static func sharedPrefix(_ a: Data, _ b: Data) -> Int {
        var n = 0
        let limit = min(a.count, b.count)
        a.withUnsafeBytes { x in
            b.withUnsafeBytes { y in
                while n < limit && x[n] == y[n] { n += 1 }
            }
        }
        func inside(_ data: Data, _ index: Int) -> Bool { index < data.count && data[index] & 0xC0 == 0x80 }
        while n > 0 && (inside(a, n) || inside(b, n)) { n -= 1 }
        return n
    }

    private func terminal(_ outcome: Asura_Model_V1_Outcome, reason: Reason) async throws {
        guard phase != .terminal else { return }
        phase = .terminal
        finishTool(.failure(CancellationError()))
        timer?.cancel(); modelTask?.cancel()
        if let ordinal = activeInferenceOrdinal {
            activeInferenceOrdinal = nil; settledInferenceOrdinal = ordinal
            try await transport.send(envelope(.inferenceUsage(RequestUsage(ordinal: ordinal).wire)))
        }
        target = nil; targetThinking = nil; transmitting = nil; inputData.removeAll(); decodedInput = nil
        var end = Asura_Model_V1_Terminal()
        end.outcome = outcome; end.lastRevision = revision
        end.count = totalChunks; end.totalBytes = totalBytes; end.reason = reason
        end.usageKnown = outcome == .complete && usage != nil
        if end.usageKnown, let usage { end.usageTokens = usage }
        // Drain previously queued chunks first. The service independently owns cancellation.
        try await transport.send(envelope(.terminal(end)), control: false)
        transport.finish()
        arm(milliseconds: 250)
    }
    private func fail(_ reason: Reason, cancelled: Bool) async {
        guard phase != .terminal else { return }
        if operation.isEmpty { phase = .terminal; transport.close(); return }
        do { try await terminal(cancelled ? .cancelled : .failed, reason: reason) }
        catch { phase = .terminal; modelTask?.cancel(); transport.close() }
    }
}

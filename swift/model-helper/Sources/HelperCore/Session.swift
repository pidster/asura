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
    private let buildID: Data
    private let schemaDigest: Data
    private var phase = Phase.hello
    private var operation = Data()
    private var generation: UInt64 = 0
    private var inputData = Data()
    private var expectedInput: UInt64 = 0
    private var inputChunks: UInt64 = 0
    private var decodedInput: ModelInput?
    private var maximumTokens: UInt32 = 512
    private var projectToolsEnabled = false
    private var toolOrdinal: UInt32 = 0
    private var toolResultBytes = 0
    private var pendingTool: (UInt32, CheckedContinuation<ToolResult, any Error>)?
    private var modelTask: Task<Void, Never>?
    private var timer: Task<Void, Never>?
    private var timerRevision: UInt64 = 0
    private var measuredContext = false
    private var latest: Snapshot?
    private var transmitting: Data?
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
        self.transport = transport; self.backend = backend; self.factory = nil
        self.buildID = buildID; self.schemaDigest = schemaDigest
    }

    public typealias BackendFactory = @Sendable (String, String?, String?, UInt32?) async throws -> any ModelBackend
    public init(transport: Transport, factory: @escaping BackendFactory, buildID: Data, schemaDigest: Data) {
        self.transport = transport; self.factory = factory; self.backend = nil
        self.buildID = buildID; self.schemaDigest = schemaDigest
    }

    public func run() async {
        do {
            guard buildID.count == 32, schemaDigest.count == 32 else { throw HelperError.protocolFault }
            arm(milliseconds: 5_000)
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

    private func arm(milliseconds: UInt32) {
        timer?.cancel(); timerRevision += 1
        let current = timerRevision
        timer = Task { [weak self] in
            do { try await Task.sleep(for: .milliseconds(Int(milliseconds))) }
            catch { return }
            await self?.expired(current)
        }
    }
    private func expired(_ current: UInt64) async {
        guard current == timerRevision else { return }
        if operation.isEmpty || phase == .terminal {
            phase = .terminal; modelTask?.cancel(); transport.close(); return
        }
        await fail(.timeout, cancelled: false)
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
        response.availability = .unknown; response.capabilities = 0; response.reason = .none
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
                hello.hasCapabilities, hello.capabilities == 0, !hello.hasContextTokens,
                hello.hasReason, hello.reason == .none else { throw HelperError.protocolFault }
            selectedModel = hello.hasSelectedModel ? hello.selectedModel : "system"
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
            // Timer may have closed discovery while the SDK availability observation was pending.
            guard phase == .hello else { throw HelperError.closed }
            var response = Asura_Model_V1_Hello()
            response.buildID = buildID; response.schemaDigest = schemaDigest
            response.maxFrameBytes = UInt32(Limits.frame)
            response.selectedModel = selectedModel
            if let context = status.contextTokens, context > 512 {
                response.availability = .available; response.capabilities = status.supportsTools ? 3 : 1
                response.contextTokens = context; response.reason = .none
                if status.localToolDestination { response.localToolDestination = true }
                if let profile = status.capabilityProfile {
                    response.capabilitySource = profile.provenance.rawValue
                    if profile.reasoningDisabled { response.reasoningDisabled = true }
                    if let mask = profile.mask { response.supportedCapabilities = mask }
                }
                if let name = ProviderMetadata.displayName(status.modelName) { response.modelName = name }
            } else {
                response.availability = .unavailable; response.capabilities = 0
                response.reason = .modelUnavailable
            }
            try await transport.send(envelope(.hello(response)))
            if response.availability != .available { phase = .terminal; transport.finish(); return }
            phase = .begin
            arm(milliseconds: 5_000)
            return
        }
        guard message.hasOperationID, message.operationID.count == 16,
            message.operationID.contains(where: { $0 != 0 }), message.hasGeneration, message.generation > 0 else {
            throw HelperError.protocolFault
        }
        if phase == .begin {
            guard case .begin(let begin) = body, begin.hasModel, begin.model == selectedModel,
                begin.hasInputBytes, begin.inputBytes > 0, begin.inputBytes <= UInt64(Limits.input),
                begin.hasDeadlineRemainingMs, (1...60_000).contains(begin.deadlineRemainingMs),
                begin.hasMaxResponseTokens, (1...2048).contains(begin.maxResponseTokens) else { throw HelperError.protocolFault }
            operation = message.operationID; generation = message.generation
            expectedInput = begin.inputBytes
            maximumTokens = begin.maxResponseTokens
            projectToolsEnabled = begin.hasEnableProjectTools && begin.enableProjectTools
            if let capabilities {
                activation = Dictionary(uniqueKeysWithValues: ModelCapability.allCases.map {
                    ($0, capabilities.activation($0, toolsEnabled: projectToolsEnabled))
                })
            }
            if projectToolsEnabled && (!supportsTools || !(backend is any ToolModelBackend)) { throw HelperError.unavailable }
            arm(milliseconds: begin.deadlineRemainingMs)
            phase = .input
            var credit = Asura_Model_V1_Credit()
            credit.transferID = 1; credit.direction = .input; credit.acceptedBytes = 0
            credit.grantedBytes = UInt64(Limits.input)
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
            let maximumTokens = self.maximumTokens
            let owner = self
            let enableTools = projectToolsEnabled
            modelTask = Task {
                do {
                    if enableTools, let toolBackend = backend as? any ToolModelBackend {
                        try await toolBackend.generateWithTools(input, maximumTokens: maximumTokens,
                            handler: { arguments in try await owner.requestTool(arguments) },
                            snapshot: { value in try await owner.snapshot(value) })
                    } else {
                        try await backend.generate(input, maximumTokens: maximumTokens) { value in
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
                toolResultBytes + result.text.utf8.count <= 65_536 else { throw HelperError.protocolFault }
            toolResultBytes += result.text.utf8.count
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

    private func requestTool(_ arguments: ToolArguments) async throws -> ToolResult {
        guard phase == .running, pendingFailure == nil, projectToolsEnabled else { throw CancellationError() }
        guard pendingTool == nil, toolOrdinal < 8 else { throw BackendFailure(.outputLimit) }
        toolOrdinal += 1
        var call = Asura_Model_V1_ToolCall()
        call.ordinal = toolOrdinal
        switch arguments {
        case .shell(let command, let cwd, let timeout):
            var args = Asura_Model_V1_Shell()
            args.command = command; if let cwd {args.cwd = cwd}; args.timeoutSeconds = timeout
            call.arguments = .shell(args)
        case .readAudit(let limit):
            var args = Asura_Model_V1_AuditRead()
            args.limit = limit
            call.arguments = .readAudit(args)
        case .listTools:
            call.arguments = .listTools(Asura_Model_V1_ListTools())
        case .memoryListNotes(let after, let limit):
            var args = Asura_Model_V1_MemoryListNotes()
            if let after { args.after = after }
            args.limit = limit
            call.arguments = .memoryListNotes(args)
        case .memoryGetNote(let version, let offset, let limit):
            var args = Asura_Model_V1_MemoryGetNote()
            args.version = version; args.offset = offset; args.limit = limit
            call.arguments = .memoryGetNote(args)
        case .memoryCreateNote(let body, let source):
            var args = Asura_Model_V1_MemoryCreateNote()
            args.body = body; if let source { args.sourceVersion = source }
            call.arguments = .memoryCreateNote(args)
        case .memoryNoteSources(let version):
            var args = Asura_Model_V1_MemoryNoteSources()
            args.version = version
            call.arguments = .memoryNoteSources(args)
        case .observeStatus:
            call.arguments = .observeStatus(Asura_Model_V1_ObserveStatus())
        case .readFile(let path, let offset, let limit):
            var args = Asura_Model_V1_ProjectReadFile()
            args.path = path; args.offset = offset; args.limit = limit
            call.arguments = .readFile(args)
        case .listDirectory(let path):
            var args = Asura_Model_V1_ProjectListDirectory()
            args.path = path
            call.arguments = .listDirectory(args)
        }
        let requestOrdinal = toolOrdinal
        let message = envelope(.toolCall(call))
        try Wire.validate(message, from: .helper)
        return try await withCheckedThrowingContinuation { continuation in
            pendingTool = (toolOrdinal, continuation)
            Task {
                do { try await transport.send(message) }
                catch { finishTool(.failure(error), ordinal: requestOrdinal) }
            }
        }
    }

    private func snapshot(_ value: Snapshot) async throws {
        guard phase == .running, pendingFailure == nil else { throw CancellationError() }
        if let context = value.inputContext {
            guard !measuredContext, revision == 0, latest == nil, value.text.isEmpty,
                context.capacity > 0, context.tokens <= context.capacity else { throw HelperError.protocolFault }
            measuredContext = true
            var measured = Asura_Model_V1_ContextMeasured()
            measured.inputTokens = context.tokens; measured.capacityTokens = context.capacity
            try await transport.send(envelope(.contextMeasured(measured)))
            return
        }
        guard value.text.utf8.count <= Limits.output else { throw BackendFailure(.outputLimit) }
        latest = value
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
        if revision == 0 && latest == nil { latest = Snapshot(text: "") }
        do { try await pump() } catch { await fail(.outputLimit, cancelled: false) }
    }

    private func pump() async throws {
        guard !pumping, phase == .running else { return }
        pumping = true
        defer { pumping = false }
        while phase == .running {
            if transmitting == nil {
                guard let next = latest else {
                    if backendFinished {
                        try await terminal(pendingFailure == nil ? .complete : .failed,
                            reason: pendingFailure ?? .none)
                    }
                    return
                }
                guard revision < 1024 else { throw BackendFailure(.outputLimit) }
                latest = nil; revision += 1; transferChunks = 0
                transmitting = Data(next.text.utf8); usage = next.usageTokens
            }
            guard let bytes = transmitting, var ledger = ledgers[transfer] else { throw HelperError.protocolFault }
            while ledger.sent < UInt64(bytes.count) {
                guard ledger.granted > ledger.sent else { return }
                let count = min(Limits.chunk, Int(ledger.granted - ledger.sent), bytes.count - Int(ledger.sent))
                guard totalBytes + UInt64(count) <= UInt64(Limits.cumulativeOutput) else { throw BackendFailure(.outputLimit) }
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
            let old = transfer
            transfer += 1
            ledgers = [old: ledger, transfer: Ledger()]
            transmitting = nil
            // Keep SnapshotEnd ordered after the data it terminates.
            try await transport.send(envelope(.snapshotEnd(end)), control: false)
        }
    }

    private func terminal(_ outcome: Asura_Model_V1_Outcome, reason: Reason) async throws {
        guard phase != .terminal else { return }
        phase = .terminal
        finishTool(.failure(CancellationError()))
        timer?.cancel(); modelTask?.cancel()
        latest = nil; transmitting = nil; inputData.removeAll(); decodedInput = nil
        var end = Asura_Model_V1_Terminal()
        end.outcome = outcome; end.lastRevision = revision
        end.count = totalChunks; end.totalBytes = totalBytes; end.reason = reason
        end.usageKnown = outcome == .complete && usage != nil
        if end.usageKnown, let usage { end.usageTokens = usage }
        // Drain previously queued chunks first. The service independently enforces its deadline.
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

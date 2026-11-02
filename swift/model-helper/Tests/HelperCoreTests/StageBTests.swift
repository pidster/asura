import Foundation
import FoundationModels
import Testing
@testable import HelperCore

// CC4 helper cases (context-composer.md, CC4). `ScriptedToolModel` drives the
// real FoundationModels session machinery with a scripted executor, so these
// tests also fail if the SDK stops behaving as the stage B workaround needs
// (CC-N1): a throwing tool must leave its `toolCalls` entry in the preserved
// transcript, and the executor must receive the replaced transcript.

/// One scripted response: text, or tool calls (name, argument text).
enum ScriptedReply: Sendable {
    case text(String)
    case calls([(String, String)])
}

/// What each dispatch received.
actor DispatchLog {
    var transcripts: [Transcript] = []
    var tools: [[String]] = []
    func record(_ transcript: Transcript, _ tools: [String]) {
        transcripts.append(transcript); self.tools.append(tools)
    }
}

struct ScriptedToolModel: LanguageModel {
    let reply: ScriptedReply
    let log: DispatchLog
    var capabilities: LanguageModelCapabilities { .init([.toolCalling]) }
    var executorConfiguration: Int { 0 }
    struct Executor: LanguageModelExecutor {
        typealias Model = ScriptedToolModel
        typealias Configuration = Int
        init(configuration: Int) {}
        func respond(to request: LanguageModelExecutorGenerationRequest, model: Model,
            streamingInto channel: LanguageModelExecutorGenerationChannel) async throws {
            await model.log.record(request.transcript, request.enabledToolDefinitions.map(\.name))
            switch model.reply {
            case .text(let text):
                await channel.send(.response(action: .appendText(text, tokenCount: 1)))
            case .calls(let calls):
                for (index, call) in calls.enumerated() {
                    await channel.send(.toolCalls(action: .toolCall(id: "provider-\(index)", name: call.0,
                        action: .appendArguments(call.1, tokenCount: 4))))
                }
            }
        }
    }
}

func scriptedEngine(_ reply: ScriptedReply, log: DispatchLog = DispatchLog()) -> FoundationBackend<ScriptedToolModel> {
    FoundationBackend(model: ScriptedToolModel(reply: reply, log: log), contextTokens: 8192,
        modelName: "stage-b-fixture", supportsTools: true)
}

func jsonEqual(_ a: String, _ b: String) throws -> Bool {
    let left = try JSONSerialization.jsonObject(with: Data(a.utf8)) as? NSDictionary
    let right = try JSONSerialization.jsonObject(with: Data(b.utf8)) as? NSDictionary
    return left != nil && left == right
}

private func turn(_ role: Asura_Model_V1_Role, _ text: String) -> Asura_Model_V1_HistoryTurn {
    var value = Asura_Model_V1_HistoryTurn(); value.role = role; value.text = text; return value
}
/// One settled request of the continuation: its calls, then their outputs.
func continuation(_ calls: [(id: String, name: String, arguments: String, output: String)]) -> [Asura_Model_V1_HistoryTurn] {
    var assistant = turn(.assistant, "")
    assistant.toolCalls = calls.map { call in
        var value = Asura_Model_V1_HistoryToolCall()
        value.callID = call.id; value.name = call.name; value.arguments = call.arguments
        return value
    }
    var entries = [assistant]
    for call in calls {
        var output = turn(.tool, call.output)
        output.callID = call.id; output.toolName = call.name
        entries.append(output)
    }
    return entries
}

/// CC-N1, part 1: three calls in one response come back in response order,
/// from the preserved transcript; exactly one dispatch; nothing ran.
@Test func stopToolPreservesEveryCallOfTheResponseInOrder() async throws {
    let log = DispatchLog()
    let calls = [("exec_cmd", #"{"command":"ls"}"#), ("job", #"{"command":"list"}"#),
        ("project", #"{"command":"read_file","path":"README.md"}"#)]
    let engine = scriptedEngine(.calls(calls), log: log)
    var input = ModelInput(); input.instructions = "i"; input.prompt = "p"
    let proposals = try await engine.generateWithTools(input, thinking: .none, request: 1, snapshot: { _ in })
    #expect(proposals.map(\.name) == calls.map(\.0))
    for (proposal, call) in zip(proposals, calls) {
        #expect(try jsonEqual(proposal.arguments, call.1))
        #expect(proposal.source == .generatedContent)
    }
    #expect(await log.transcripts.count == 1)
    #expect(await log.tools.first == ["project", "memory", "service", "exec_cmd", "job"])
}

/// CC-N1, part 2: the executor receives the whole request (instructions with
/// the declared tools, history, prompt, then the continuation), and the
/// request continues from a transcript that ends in a tool output.
@Test func executorReceivesTheWholeRequestAsItsTranscript() async throws {
    let log = DispatchLog()
    let engine = scriptedEngine(.text("answer"), log: log)
    var input = ModelInput(); input.instructions = "instructions"; input.prompt = "prompt"
    input.history = [turn(.user, "earlier question"), turn(.assistant, "earlier answer")]
    input.continuation = continuation([
        (id: "call_1", name: "exec_cmd", arguments: #"{"command":"ls"}"#, output: "job: 7f3a"),
        (id: "call_2", name: "job", arguments: #"{"command":"list"}"#, output: "Tool failed: denied")
    ]) + continuation([(id: "call_3", name: "project", arguments: "{}", output: "private: x")])
    let texts = TextLog()
    let proposals = try await engine.generateWithTools(input, thinking: .none, request: 3,
        snapshot: { value in await texts.add(value.text) })
    #expect(proposals.isEmpty)
    #expect(await texts.last == "answer")
    let received = try #require(await log.transcripts.first)
    var kinds: [String] = []
    for entry in received {
        switch entry {
        case .instructions(let v):
            kinds.append("I")
            // Declared as the SDK's own session declares them.
            #expect(v.toolDefinitions.map(\.name) == ["project", "memory", "service", "exec_cmd", "job"])
        case .prompt: kinds.append("P")
        case .response: kinds.append("R")
        case .toolCalls(let calls):
            kinds.append("C\(calls.count)")
        case .toolOutput(let output):
            kinds.append("O:\(output.id)")
        default: kinds.append("?")
        }
    }
    #expect(kinds == ["I", "P", "R", "P", "C2", "O:call_1", "O:call_2", "C1", "O:call_3"])
    #expect(await log.transcripts.count == 1)
    #expect(await log.tools.first == ["project", "memory", "service", "exec_cmd", "job"])
}

actor TextLog {
    var texts: [String] = []
    func add(_ text: String) { if !text.isEmpty { texts.append(text) } }
    var last: String? { texts.last }
}

/// A continuation call whose arguments are not JSON is a service fault.
@Test func continuationArgumentsMustBeJSON() async throws {
    let engine = scriptedEngine(.text("unused"))
    var input = ModelInput(); input.instructions = "i"; input.prompt = "p"
    input.continuation = continuation([(id: "call_1", name: "exec_cmd", arguments: "not json", output: "x")])
    await #expect(throws: BackendFailure.self) {
        _ = try await engine.generateWithTools(input, thinking: .none, request: 2, snapshot: { _ in })
    }
}

/// The request ordinal names the usage report of the request's one inference.
@Test func usageReportCarriesTheServiceRequestOrdinal() async throws {
    let engine = scriptedEngine(.calls([("job", #"{"command":"list"}"#)]))
    var input = ModelInput(); input.instructions = "i"; input.prompt = "p"
    let ordinals = OrdinalLog()
    _ = try await engine.generateWithTools(input, thinking: .none, request: 7, snapshot: { value in
        if let started = value.inferenceStartedOrdinal { await ordinals.add(started) }
        if let usage = value.inferenceUsage { await ordinals.add(usage.ordinal) }
    })
    #expect(await ordinals.values == [7, 7])
}
actor OrdinalLog {
    var values: [UInt32] = []
    func add(_ value: UInt32) { values.append(value) }
}

/// Arguments reach the service unread, malformed or out of range ones too
/// (tool-call-records.md): the helper never validates them.
@Test func proposalsCarryArgumentsUnread() async throws {
    let handle = "7f3a09c2e4b15d68a0c3f1e2d4b5a697"
    let cases = [
        ("exec_cmd", #"{"command":""}"#), ("exec_cmd", #"{"command":"pwd","cwd":"/tmp"}"#),
        ("job", #"{"command":"read","job":"\#(handle)","stream":"stderr","offset":9223372036854775807,"limit":8193}"#),
        ("job", #"{"command":"note","job":"NOT-HEX","text":""}"#),
        ("memory", #"{"command":"create_note","body":"  note\nbody é "}"#)
    ]
    for (name, arguments) in cases {
        let proposals = try await scriptedEngine(.calls([(name, arguments)])).generateWithTools(
            { var i = ModelInput(); i.instructions = "i"; i.prompt = "p"; return i }(),
            thinking: .none, request: 1, snapshot: { _ in })
        #expect(proposals.count == 1 && proposals[0].name == name)
        #expect(try jsonEqual(proposals[0].arguments, arguments), "\(arguments)")
    }
}

/// Without tools the engine dispatches once and returns no calls.
@Test func plainTextDispatchesOnce() async throws {
    let log = DispatchLog()
    let engine = scriptedEngine(.text("plain"), log: log)
    var input = ModelInput(); input.instructions = "i"; input.prompt = "p"
    try await engine.generate(input, snapshot: { _ in })
    #expect(await log.transcripts.count == 1)
    #expect(await log.tools.first == [])
}

/// The one-inference gate refuses a second dispatch in one session.
@Test func requestGateAdmitsOneInference() async throws {
    let gate = RequestGate()
    try await gate.admit()
    await #expect(throws: BackendFailure.self) { try await gate.admit() }
    #expect(await gate.recordedFailure()?.reason == .protocolFault)
}

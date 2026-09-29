import Foundation
import FoundationModels
import Testing
@testable import HelperCore

private func shellEnvelope(_ args: Asura_Model_V1_Shell) -> Envelope {
    var call = Asura_Model_V1_ToolCall(); call.ordinal = 1; call.shell = args
    var message = Envelope(); message.operationID = Data(repeating: 7, count: 16)
    message.generation = 1; message.body = .toolCall(call)
    return message
}

@Test func shellNativeSchemaDefaultsAndFailureOutput() async throws {
    let tool = ShellTool { args in
        guard case .shell(let command, let cwd, let timeout) = args else { throw HelperError.protocolFault }
        #expect(command == "printf fixture" && cwd == "." && timeout == 30)
        return ToolResult(status: .timeout, text: "stdout: partial\nstderr: deadline", truncated: true)
    }
    let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
    let properties = try #require(schema["properties"] as? [String: Any])
    #expect(Set(properties.keys) == ["command", "cwd", "timeout_seconds"])
    #expect(Set(schema["required"] as? [String] ?? []) == ["command"])
    let output = try await tool.call(arguments: .init(command: "printf fixture", cwd: ".", timeout_seconds: nil))
    #expect(output.contains("stdout: partial") && output.contains("stderr: deadline") && output.contains("truncated: true"))
    #expect(!ToolResult(status: .timeout, text: "private failed body").rendered.contains("private failed body"))
}

@Test func shellInvalidNativeArgumentsNeverReachService() async throws {
    let tool = ShellTool { _ in Issue.record("invalid arguments reached service"); throw HelperError.protocolFault }
    for command in ["", "x\0y", String(repeating: "é", count: 4097)] {
        await #expect(throws: BackendFailure.self) { try await tool.call(arguments: .init(command: command, cwd: nil, timeout_seconds: nil)) }
    }
    for cwd in ["", "/tmp", "..", "src/../other", "x\0y", String(repeating: "x", count: 1025)] {
        await #expect(throws: BackendFailure.self) { try await tool.call(arguments: .init(command: "pwd", cwd: cwd, timeout_seconds: nil)) }
    }
    for timeout in [-1, 0, 61, Int.max] {
        await #expect(throws: BackendFailure.self) { try await tool.call(arguments: .init(command: "pwd", cwd: nil, timeout_seconds: timeout)) }
    }
}

@Test func shellWireBoundsPresenceUnknownFieldsAndDirection() throws {
    var args = Asura_Model_V1_Shell(); args.command = "printf fixture"
    let message = shellEnvelope(args)
    #expect(try Wire.decode(message.serializedData(), from: .helper) == message)
    #expect(throws: HelperError.self) { try Wire.decode(message.serializedData(), from: .service) }
    #expect(throws: HelperError.self) { try Wire.decode(shellEnvelope(.init()).serializedData(), from: .helper) }
    for command in ["", "x\0y", String(repeating: "é", count: 4097)] {
        var bad = args; bad.command = command
        #expect(throws: HelperError.self) { try Wire.decode(shellEnvelope(bad).serializedData(), from: .helper) }
    }
    for cwd in ["", "/tmp", "..", "a/../b", "x\0y", String(repeating: "x", count: 1025)] {
        var bad = args; bad.cwd = cwd
        #expect(throws: HelperError.self) { try Wire.decode(shellEnvelope(bad).serializedData(), from: .helper) }
    }
    for timeout in [UInt32(0), 61] {
        var bad = args; bad.timeoutSeconds = timeout
        #expect(throws: HelperError.self) { try Wire.decode(shellEnvelope(bad).serializedData(), from: .helper) }
    }
    for timeout in [UInt32(1), 60] {
        var good = args; good.command = String(repeating: "é", count: 4096); good.cwd = "."; good.timeoutSeconds = timeout
        let envelope = shellEnvelope(good)
        #expect(try Wire.decode(envelope.serializedData(), from: .helper) == envelope)
    }
    var raw = try args.serializedData(); raw.append(contentsOf: [0xa0, 6, 1])
    let unknown = try Asura_Model_V1_Shell(serializedBytes: raw)
    #expect(throws: HelperError.self) { try Wire.decode(shellEnvelope(unknown).serializedData(), from: .helper) }
}

@Test func shellFailedWireResultMayContainBoundedTruncatedTextButNoCursor() throws {
    var result = Asura_Model_V1_ToolResult(); result.ordinal = 1; result.status = .timeout
    result.text = "stdout: partial\nstderr: deadline"; result.truncated = true
    var message = Envelope(); message.operationID = Data(repeating: 7, count: 16); message.generation = 1
    message.body = .toolResult(result)
    #expect(try Wire.decode(message.serializedData(), from: .service) == message)
    result.nextOffset = 1; message.body = .toolResult(result)
    #expect(throws: HelperError.self) { try Wire.decode(message.serializedData(), from: .service) }
    result.clearNextOffset(); result.text = String(repeating: "x", count: 16385); message.body = .toolResult(result)
    #expect(throws: HelperError.self) { try Wire.decode(message.serializedData(), from: .service) }
}

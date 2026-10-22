import Foundation
import FoundationModels
import Testing
@testable import HelperCore

@Test func execCmdNativeSchemaForwardsCommandAndRendersFailureOutput() async throws {
    let tool = ExecCmdTool { proposal in
        #expect(proposal.name == "exec_cmd" && proposal.source == .generatedContent)
        // jsonString is the SDK's own serialization; member order is not stable
        // across instances, so compare the JSON values (TCR-Q1 finding).
        let object = try JSONSerialization.jsonObject(with: Data(proposal.arguments.utf8)) as? NSDictionary
        #expect(object == ["command": "printf fixture", "cwd": "."] as NSDictionary)
        return ToolResult(status: .timeout, text: "stdout: partial\nstderr: deadline", truncated: true)
    }
    let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
    let properties = try #require(schema["properties"] as? [String: Any])
    // No time bound: the native schema offers no timeout argument. The yield
    // window only ends the call's wait, never the job (J3).
    #expect(Set(properties.keys) == ["command", "cwd", "yield_seconds"])
    #expect(Set(schema["required"] as? [String] ?? []) == ["command"])
    let output = try await tool.call(arguments: GeneratedContent(json: #"{"command":"printf fixture","cwd":"."}"#))
    #expect(output.hasPrefix("Tool failed: timeout\n"))
    #expect(output.contains("stdout: partial") && output.contains("stderr: deadline") && output.contains("truncated: true"))
}

/// Arguments that the earlier helper rejected now reach the service unread;
/// the service records and rejects them (tool-call-records.md).
@Test func execCmdInvalidArgumentsReachTheServiceUnread() async throws {
    let tool = ExecCmdTool { proposal in
        ToolResult(status: .invalidArguments, text: "invalid_arguments: unsafe_path")
    }
    for json in [#"{"command":""}"#, #"{"command":"pwd","cwd":"/tmp"}"#, #"{"command":"pwd","cwd":"a/../b"}"#] {
        let output = try await tool.call(arguments: GeneratedContent(json: json))
        #expect(output == "Tool failed: invalidArguments\ninvalid_arguments: unsafe_path")
    }
}

@Test func execCmdFailedWireResultMayContainBoundedTruncatedTextButNoCursor() throws {
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

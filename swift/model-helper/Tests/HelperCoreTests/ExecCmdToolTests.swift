import Foundation
import FoundationModels
import Testing
@testable import HelperCore

@Test func execCmdNativeSchemaHasNoTimeBoundAndCallsComeBackUnread() async throws {
    let tool = ExecCmdTool()
    let schema = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(tool.parameters)) as? [String: Any])
    let properties = try #require(schema["properties"] as? [String: Any])
    // No time bound: the native schema offers no timeout argument. The yield
    // window only ends the call's wait, never the job (J3).
    #expect(Set(properties.keys) == ["command", "cwd", "yield_seconds"])
    #expect(Set(schema["required"] as? [String] ?? []) == ["command"])
    // jsonString is the SDK's own serialization; member order is not stable
    // across instances, so compare the JSON values (TCR-Q1 finding).
    let arguments = #"{"command":"printf fixture","cwd":"."}"#
    var input = ModelInput(); input.instructions = "i"; input.prompt = "p"
    let proposals = try await scriptedEngine(.calls([("exec_cmd", arguments)]))
        .generateWithTools(input, thinking: .none, request: 1, snapshot: { _ in })
    #expect(proposals.count == 1 && proposals[0].name == "exec_cmd" && proposals[0].source == .generatedContent)
    #expect(try jsonEqual(proposals[0].arguments, arguments))
}

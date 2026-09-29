import Darwin
import Foundation
import HelperCore

@main
struct Main {
    static func main() async {
        if CommandLine.arguments == [CommandLine.arguments[0], "--version"] {
            let build = BuildIdentity.buildID.map { String(format: "%02x", $0) }.joined()
            let schema = BuildIdentity.schemaDigest.map { String(format: "%02x", $0) }.joined()
            print("asura-model 0.1.0 build=\(build) schema=\(schema)")
            return
        }
        guard CommandLine.arguments.count == 1 else { exit(2) }
        do {
            let transport = try Transport(fd: 3)
            let session = HelperSession(
                transport: transport,
                factory: { selector, assets, endpoint, capabilities in
                    try await ProviderFactory.make(selector, assetRoot: assets, endpoint: endpoint, capabilities: capabilities)
                },
                buildID: Data(BuildIdentity.buildID), schemaDigest: Data(BuildIdentity.schemaDigest))
            await session.run()
            // A stalled SDK task must not keep the private helper process alive after channel closure.
            exit(0)
        } catch {
            exit(3)
        }
    }
}

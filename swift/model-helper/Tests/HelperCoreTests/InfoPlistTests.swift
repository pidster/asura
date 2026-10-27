import Foundation
import MachO
import Testing

// Release signing, HS-P1 (packet SG2a): every helper build embeds
// Support/Info.plist in `__TEXT,__info_plist`.
private let packageRoot = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()

/// Bytes of `segment,section` in a thin 64-bit Mach-O file, or nil when absent.
private func section(_ file: Data, _ segment: String, _ name: String) -> Data? {
    func field(_ offset: Int, _ length: Int) -> String {
        String(decoding: file[offset..<offset + length].prefix { $0 != 0 }, as: UTF8.self)
    }
    func u32(_ offset: Int) -> UInt32 { file[offset..<offset + 4].withUnsafeBytes { $0.loadUnaligned(as: UInt32.self) } }
    func u64(_ offset: Int) -> UInt64 { file[offset..<offset + 8].withUnsafeBytes { $0.loadUnaligned(as: UInt64.self) } }
    guard file.count >= MemoryLayout<mach_header_64>.size, u32(0) == MH_MAGIC_64 else { return nil }
    var command = MemoryLayout<mach_header_64>.size
    for _ in 0..<u32(16) {
        let kind = u32(command), size = Int(u32(command + 4))
        if kind == UInt32(LC_SEGMENT_64), field(command + 8, 16) == segment {
            let count = Int(u32(command + 64))
            for index in 0..<count {
                let header = command + MemoryLayout<segment_command_64>.size + index * MemoryLayout<section_64>.size
                guard field(header, 16) == name else { continue }
                let length = Int(u64(header + 40)), offset = Int(u32(header + 48))
                return file[offset..<offset + length]
            }
        }
        command += size
    }
    return nil
}

@Test func helperInfoPlistNamesTheModelIdentity() throws {
    let data = try Data(contentsOf: packageRoot.appending(path: "Support/Info.plist"))
    let plist = try #require(try PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any])
    #expect(plist["CFBundleIdentifier"] as? String == "org.pidster.asura.model")
    #expect(plist["CFBundleExecutable"] as? String == "asura-model")
    #expect(plist["CFBundleInfoDictionaryVersion"] as? String == "6.0")
}

@Test func helperBuildEmbedsTheInfoPlist() throws {
    // `swift test` builds the executable product beside the test bundle.
    let products = Bundle(for: Marker.self).bundleURL.deletingLastPathComponent()
    let helper = try Data(contentsOf: products.appending(path: "asura-model"), options: .alwaysMapped)
    let embedded = try #require(section(helper, "__TEXT", "__info_plist"))
    #expect(embedded == (try Data(contentsOf: packageRoot.appending(path: "Support/Info.plist"))))
}

private final class Marker {}

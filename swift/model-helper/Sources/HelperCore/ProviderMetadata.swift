import Foundation

/// Presentation metadata never changes the admitted routing selector.
enum ProviderMetadata {
    static func displayName(_ value: String?) -> String? {
        guard let value, !value.isEmpty, value.utf8.count <= 256,
              !value.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else {
            return nil
        }
        return value
    }
}

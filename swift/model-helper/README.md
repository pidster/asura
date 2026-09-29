# Private model helper

The service owns admission. This executable accepts an inherited Unix socket on
FD 3; it has no interactive inference mode. `--version` opens no model session.

Before a standard SwiftPM build, the root build owner must:

1. Verify protoc 36.2 and protoc-gen-swift 1.38.1 against the existing lock.
2. Generate `contracts/model/v1/model.proto` into
   `Sources/HelperCore/Generated/`, using `--swift_opt=Visibility=Public`.
3. Generate `Sources/HelperCore/Generated/BuildIdentity.swift` with public enum
   `BuildIdentity`, `public static let buildID: [UInt8]` and
   `public static let schemaDigest: [UInt8]`, each exactly 32 bytes.
4. Build with standard `swift build --package-path swift/model-helper`; run
   model-free tests with `swift test --package-path swift/model-helper`.
5. Assemble helper/binary/manifest identities according to the D2 contract.

Generated source and identities are ignored. Missing inputs fail compilation;
there is no unverified fallback or user-path helper lookup. The dependency is the
already locked SwiftProtobuf revision. Dependency acquisition/builds remain under
the existing toolchain's verification and outer network-denial rules.

No live-inference test runs implicitly. SDK availability and real cancellation
latency need separate supported-host qualification.

## Qualification status, 2026-09-27

Root ran nine model-free Swift tests successfully. Coverage includes incremental
framing, unknown/duplicate fields, structured roles, a scripted conversation over
real socketpairs, cancellation before Start, coalescing while credit is withheld,
and a stalled generation deadline (terminal timeout observed in 0.106 seconds).
The socketpair journeys use both endpoints in the test process.

The cross-language fixture test accepted 22 Rust-generated payloads with identical
Swift reserialization and rejected 14 malformed payloads. This proves those
fixtures' wire agreement, not a complete deployed Rust/Swift process exchange.

The integrated developer package also passed real local inference through the
Rust supervisor: availability, input preparation, response streaming, committed
history across a second turn, cancellation, and exact terminal replay after restart.
The native PTY journey passed explicit initialization, project registration/selection,
a real response, terminal restoration, and owned-backend shutdown.
Tests used synthetic prompts and private scratch state on macOS 27.0 (26A428),
Apple Swift 6.4 (swiftlang-6.4.0.34.1).

Still unverified:

- Physical power-loss durability and every injected disk failure.
- Exhaustive helper/service crash boundaries beyond the recorded mid-generation
  service kill, helper EOF exit and Interrupted recovery journey.
- SDK/process memory high-water under the qualification workload and live OS stalls.
- Signed distribution, production sandbox confinement and upgrade behavior.

These are separate integration and live-host gates. Scripted success does not
claim that inference is available or that a user conversation is complete.

# Swift model helper

Follow the root instructions and the scoped first-conversation contract in
[swift-rust-boundary.md](../docs/designs/swift-rust-boundary.md#first-conversation-helper-scoped-d2-contract-2026-09-27).

- The Rust service owns admission, policy, budgets, context generations and outcomes.
- This helper accepts only the inherited private channel and never runs host tools.
- Keep descriptor IO nonblocking, queues bounded and SDK work cancellable.
- Preserve structured history roles. Never treat provisional output as completed history.
- Use generated SwiftProtobuf types from the canonical model schema, not a second parser.
- Model-free tests use injected backends. Live inference is a separate qualification.
- Root owns serial builds and package identities. Do not run concurrent builds or inference.

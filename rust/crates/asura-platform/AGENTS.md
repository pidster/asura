# macOS platform boundary

Follow the root and Rust instructions and the foundation contract in
[system architecture](../../../docs/designs/system-architecture.md).

This crate owns account paths, validated runtime descriptors, local peer identity,
owner locks, fixed startup spawning, polling, signals and random identifiers.
Keep all unsafe calls narrow and explain their safety conditions. Do not add
service lifecycle, protocol validation or client policy here. Test-only scratch
constructors require the non-default `test-support` feature. Tests must never
open the user's real Asura runtime directory.

# Isolated Rust TUI experiment

Read the root instructions and the [implementation packet](../../docs/plans/tui-prototype-implementation.md)
before changing this package. The [prototype design](../../docs/designs/tui-interaction-prototype.md)
owns behavior; this experiment does not implement the production service.

- Keep runtime I/O limited to terminal input/output. Use synthetic fixture data.
- Preserve module ownership from the packet. Reuse the rat-text adapter for every draft.
- Forbid unsafe code. Propagate expected failures; input must not cause panics.
- Pin direct dependencies and retain Cargo.lock. Do not change global configuration.
- Run format, Clippy, unit/integration and PTY checks from the packet. Record native
  Ghostty and Terminal.app evidence separately from buffer tests.
- Keep target output and generated previews untracked. No dependency or UX claim
  is verified merely because the package compiles.

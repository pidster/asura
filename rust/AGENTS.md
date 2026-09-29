# Production Rust

Read the root instructions, [engineering standards](../docs/engineering.md) and
the governing implementation packet before changing code.

- The owner's latest instruction makes Asura implementation approval implicit.
  Use a sufficient governing design and proceed with service-first foundation
  stages 1–2 under the [foundation packet](../docs/plans/production-foundation-implementation.md)
  and read-only stage 3A under the [status packet](../docs/plans/production-status-implementation.md#selected-first-delivery-3a-inspection-only).
  Standard Cargo, installed tools, verified pinned protoc and locked prost
  generation are the immediate path. Full PB0 cache publication, Swift smoke,
  portable-cache proof and custom-driver extension are deferred qualification.
  Do not introduce another wrapper, cache or process supervisor. The owner permits
  the shell command cleanup helper within the existing platform process owner,
  under [the shell design](../docs/designs/shell-tool.md). This exception does not
  permit a second service scheduler.
- The standalone driver uses only the Rust standard library and required macOS
  process APIs. Do not add a crate dependency to that driver. PB0.2 introduces
  the bootstrap crate's exact designed dependencies. Review the full Cargo
  lockfile before building dependencies.
- Keep unsafe FFI calls in narrow wrappers. State each safety condition.
- Use typed errors for expected failures. Preserve the original failure when
  cleanup also fails. Unknown child state must retain the incomplete marker.
- Add unit, actual process integration and command-level checks for delivered
  behavior. Use isolated scratch roots, never the user's service runtime path.
- Run the governing packet's checks and Rust formatting. Foundation uses standard
  Cargo now; changing the existing driver still requires its own checks. Record exact results
  and proof limits. Do not claim later bootstrap or service qualification.
- Keep generated output under ignored build paths. The root workspace has one
  metadata owner; do not create another workspace under this directory.

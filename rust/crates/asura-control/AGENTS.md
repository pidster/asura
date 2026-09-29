# Local control

Follow the root and Rust instructions. The stage 1–2 control section in
[system architecture](../../../docs/designs/system-architecture.md#foundation-service-proposal-stages-12)
and the [foundation packet](../../../docs/plans/production-foundation-implementation.md)
govern the foundation contract. The selected
[stage 3A inspection contract](../../../docs/plans/production-status-implementation.md#selected-first-delivery-3a-inspection-only)
records the earlier inspection extension. The selected
[config command contract](../../../docs/designs/config-commands.md) adds get/set
and uses the owner-selected wire protocol 0.1. Only the version number is frozen. Schemas and operations may evolve for
authorized features without a version bump; follow
[protocol change authority](../../../docs/engineering.md#wire-protocol-change-authority).

- Keep runtime code free of filesystem and process access.
- Generate bindings and strict validation metadata from the canonical schema.
- Keep lifecycle, attachment state and request admission in their existing owners.
- Build with the explicit verified pinned `ASURA_PROTOC` executable. Do not acquire tools.
- Test malformed wire, semantic rejection and fragmented/coalesced frame boundaries.

# Service orchestration

Follow the root and Rust instructions. The stage 1–2 service section in
[system architecture](../../../docs/designs/system-architecture.md#foundation-service-proposal-stages-12)
and the [foundation packet](../../../docs/plans/production-foundation-implementation.md)
govern the foundation lifecycle. The selected read-only stage 3A contract in
[the status packet](../../../docs/plans/production-status-implementation.md#selected-first-delivery-3a-inspection-only)
also governs installation inspection.

- Keep one reactor and one lifecycle dispatcher. Reuse control validation and platform enforcement.
- Preserve Hello, Inspect, InspectInstallation, Stop and the config operations
  in `docs/designs/config-commands.md`. The owner has expanded this scope to
  reviewed conversation admission, recovery and model execution under the
  first-conversation packet in `docs/designs/platform-capabilities.md`.
  Resolve its durable journal and helper prerequisites before dispatching a model.
  This does not authorize unrelated tool execution or graph scheduling.
- Process inputs through the shared pipeline: decode, validate, normalize into
  typed commands, route to the canonical owner, admit, execute and publish events.
  Clients own syntax and presentation; service owners control admission and effects.
  Preserve bounded queues, deadlines, cancellation, identity and recovery at each
  applicable boundary. Follow ADR-0006; do not add a parallel lifecycle dispatcher.
- Keep test runtimes in private scratch homes, never the account runtime.
- Preserve bounded admission, deadlines, identity validation and lock-last drain.

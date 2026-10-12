# Plans

This directory orders design and implementation work. Plans name dependencies,
deliverables and validation gates; governing designs define the behavior.
Read the architecture and design plan before the product implementation plan.

The product delivery sequence remains proposed. A scoped implementation packet
must meet the [design process](../design-process.md), including owner review and
authorization. The TUI packet authorizes only its isolated experiment.

## Files

| Document | Purpose and status |
| --- | --- |
| [Architecture and design plan](architecture-and-design.md) | Proposed D0-D8 sequence for workflows, contracts, validation design and consistency review. |
| [Implementation plan](implementation.md) | Proposed I0-I9 delivery sequence and acceptance gates for the selected release scope and later capabilities. |
| [Production foundation packet](production-foundation-implementation.md) | Stages 1–2: root workspace, shared bootstrap handoff, Protobuf control and local service; implemented with a 2026-09-26 developer checkpoint, full qualification open. |
| [Protobuf bootstrap packet](protobuf-bootstrap-implementation.md) | PB0: shared pinned tools, smoke generation and clean online/offline qualification; PB0.0–PB0.2 implemented, PB0.3 incomplete, remaining work deferred under service-first delivery. |
| [Production status packet](production-status-implementation.md) | Stages 3–5: installation, registry, Git observations and real status TUI; 3A, embedded 3B, 3C registration, stage 4 and the thin TUI implemented; external graph, discovery and full stage 5 open. |
| [TUI prototype implementation packet](tui-prototype-implementation.md) | Authorized isolated experiment scope, file ownership, dependencies, limits and required checks. |
| [Command interaction trial packet](command-interaction-trial.md) | Validated offline command-discovery trial, file ownership, implementation order and recorded checks. |

Each plan contains its canonical dependency diagrams. Follow its links to the
[designs](../designs/README.md) and [decisions](../decisions/README.md) before
starting an implementation packet.

Return to the [documentation index](../README.md).

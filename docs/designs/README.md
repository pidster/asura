# Designs

This directory holds the workflows and contracts that make Asura's architecture
concrete. Briefs identify required behavior and unresolved mechanisms. Detailed
designs must resolve the choices needed for their implementation scope under the
[design process](../design-process.md).

Use the status in each document to distinguish required behavior, proposals,
selected mechanisms and recorded evidence. The isolated TUI experiment has its
own authorization and validation scope; it does not establish product readiness.

## Files

| Document | Purpose and status |
| --- | --- |
| [Context storage](context-storage-candidates.md) | Required embedded/external SurrealDB and hybrid file/database ownership, recovery and backup boundaries. |
| [Core harness](core-harness-brief.md) | Required and proposed lifecycle, admission, budget, context and model-session contracts for D3-D4. |
| [Domain model](domain-model.md) | Proposed D0 identities, relationships and ownership; concrete schemas remain later design work. |
| [Early production status slice](early-production-status-slice.md) | Proposed scoped production trial for real service-backed project and Git status; implementation contracts remain open. |
| [Interaction and extension boundaries](interaction-and-extension-boundaries.md) | Required interaction goals, three in-chat command categories and integration boundaries; proposed input-processing separation. |
| [Product workflows](product-workflows.md) | Selected first-release scope, required workflows and proposed acceptance targets. |
| [Platform capabilities](platform-capabilities.md) | D1 macOS 27 SDK evidence and proposed home, service and local-authentication mechanisms. |
| [Protobuf toolchain bootstrap](protobuf-toolchain-bootstrap.md) | Scoped D2/D7 design ready for owner review: test-schema packet, verified download, full offline rebuild and BT1-BT8 validation. Implementation remains gated. |
| [Persistence and recovery](persistence-recovery.md) | Proposed D3 local control-store, installation, graph-binding, registration and backup protocol for I1. |
| [Production bootstrap and status](production-bootstrap-status.md) | Proposed transition from the isolated TUI to per-user initialization, project registration and real scoped status. |
| [Repository agent configuration](repository-agent-configuration.md) | Selected development-agent configuration, directory-scoped instructions and recorded configuration checks. |
| [Release distribution](release-distribution.md) | Selected Homebrew tap direction and proposed artifact, formula, upgrade and I9 qualification contract. |
| [Requirements and validation](requirements-validation.md) | Proposed D0 traceability from requirements to design owners, delivery increments and required evidence. |
| [Runtime architecture](runtime-architecture.md) | Proposed process layout, asynchronous pipelines and coordination mechanisms. |
| [Security policy](security-policy-brief.md) | Required capabilities and proposed authorization contract; policy and OS mechanisms remain open. |
| [System architecture](system-architecture.md) | Proposed D2 standalone per-user service transport, owner arbitration and fencing for I1. |
| [Local-model boundary and Swift implementation](swift-rust-boundary.md) | Proposed D2 portable capability contract with selected supervised first macOS Swift helper; cancellation and release details remain open. |
| [TUI interaction prototype](tui-interaction-prototype.md) | Interaction behavior, ownership and validation for the authorized isolated experiment. |
| [TUI composer](tui-composer.md) | Implemented prototype status bar, message tray and direct actions; automated qualification and owner-reported native smoke evidence. |
| [TUI project status](tui-project-status.md) | Implemented shell-style project/Git and model/context status row, synthetic metadata, compact layouts and recorded automated/visual checks. |
| [TUI transcript styling](tui-transcript-style.md) | Implemented input-like bands for submitted user messages, typed provenance, scroll layout and recorded automated/visual checks. |
| [TUI command discovery](tui-command-discovery.md) | Validated isolated-trial discovery, completion and invocation for built-ins, extensions and Skill-based fixtures. |
| [Command system](command-system.md) | Proposed production catalogue, definition identity, admission and recovery contract; D3/D4/D6 decisions remain open. |
| [TUI key inspection](tui-key-inspection.md) | Implemented inert diagnostic, partial native results from both terminals and the identified decoder compatibility gap. |
| [Guided TUI tour](tui-guided-tour.md) | Selected synthetic tour and renderer previews that reduce repeated native test setup while preserving distinct proof requirements. |
| [TUI prototype preflight](tui-prototype-preflight.md) | Initial environment and dependency findings, proof limits and readiness follow-up. |
| [Threat model](threat-model.md) | Proposed D1 assets, adversaries, trust boundaries and required bootstrap/status defenses. |
| [User service and configuration](user-service-configuration.md) | Required user service, `$HOME/.asura/`, project-context and configuration behavior; concrete mechanisms remain open. |
| [Validation strategy](validation-strategy.md) | Proposed D7 evidence plan for the first Protobuf bootstrap packet; broader I0 and release gates remain open. |

The [architecture and design plan](../plans/architecture-and-design.md) orders
these artifacts. The [visual reading path](../decisions/README.md#visual-reading-path)
links the canonical ownership, lifecycle and data diagrams.

Return to the [documentation index](../README.md).

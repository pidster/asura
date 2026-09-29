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
| [Composer interactions](composer-interactions.md) | Selected production focus navigation, selectors, history and queue interaction. |
| [Composer editor navigation](composer-editor-navigation.md) | Selected visual row boundaries and bounded draft history recall. |
| [Service status pane](service-status-pane.md) | Selected connection ownership, service uptime and bounded stored-memory footprint observations. |
| [Context observations](context-observations.md) | Service-owned filesystem signals, bounded Git collection and scoped status observations; runtime qualification required. |
| [Sensors](sensors.md) | Passive and directed observations, durable evidence inspection, held proposals and selected System 1/System 2 direction. |
| [Model context telemetry](model-context-telemetry.md) | Native model identity and measured input context, with explicit basis and asynchronous event delivery. |
| [Model providers](model-provider-integration.md) | Local and remote provider abstraction, endpoint locality and staged Ollama, Core AI and MLX integration. |
| [Model tools](model-tool-execution.md) | Shared service tool execution, task grants, bounded native model callbacks and staged activation. |
| [Context storage](context-storage-candidates.md) | Required embedded/external SurrealDB and hybrid file/database ownership, recovery and backup boundaries. |
| [Core harness](core-harness-brief.md) | Required and proposed lifecycle, admission, budget, context and model-session contracts for D3-D4. |
| [Conversation admission](conversation-admission.md) | Format-1 authority, processing pipeline, durable conversation admission and queued input contracts. |
| [Project names and rename](project-names.md) | Selected capitalized project display names and durable selected-project rename contract; implementation and focused runtime checks recorded, full acceptance matrix open. |
| [Managed input queue](managed-input-queue.md) | Selected all-input queue, durable reorder, sequential dispatch, steering and compact composer projection; implementation and validation in progress. |
| [Conversation restoration](conversation-restoration.md) | Selected project-scoped latest conversation restoration, explicit `/new`, bounded control reads and recovery checks. |
| [Domain model](domain-model.md) | Proposed D0 identities, relationships and ownership; concrete schemas remain later design work. |
| [Hybrid memory ontology](hybrid-memory-ontology.md) | Proposed file, document and graph memory, plan/task tracking, dependencies, typed built-in tools, authority boundaries, selected home paths and recovery checks. |
| [Graph memory and instruction evolution](graph-memory-evolution.md) | Proposed versioned knowledge graph, instruction lineage, temporal authority, plan and progress links, typed queries and reconciliation. |
| [Early production status slice](early-production-status-slice.md) | Production TUI, lifecycle and presentation contracts, with staged delivery and recorded validation. |
| [Interaction and extension boundaries](interaction-and-extension-boundaries.md) | Required interaction goals, three in-chat command categories and integration boundaries; proposed input-processing separation. |
| [Product workflows](product-workflows.md) | Selected first-release scope, required workflows and proposed acceptance targets. |
| [Platform capabilities](platform-capabilities.md) | D1 macOS 27 SDK evidence and proposed home, service and local-authentication mechanisms. |
| [Protobuf toolchain bootstrap](protobuf-toolchain-bootstrap.md) | Shared tools for control and model schemas; verified download, test-only smoke exchange and offline qualification. Implementation remains gated. |
| [Protobuf cache preparation](protobuf-cache-preparation.md) | Approved PB0.3 archive, manifest and driver contracts; live preparation remains blocked on native process settlement. |
| [Protobuf process settlement](protobuf-process-settlement.md) | Authorized test-only API qualification packet; production cleanup correction and proof limits remain open. |
| [Persistence and recovery](persistence-recovery.md) | Proposed D3 local control-store, installation, graph-binding, registration and backup protocol for I1. |
| [Production bootstrap and status](production-bootstrap-status.md) | Original bootstrap proposal with selected automatic setup, project registration and scoped status amendments. |
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
| [Production command flows](command-flows.md) | Production CLI and TUI branch charts, canonical owners, regression mapping and explicit audit gaps. |
| [Command system](command-system.md) | Proposed production catalogue, definition identity, admission and recovery contract; D3/D4/D6 decisions remain open. |
| [TUI key inspection](tui-key-inspection.md) | Implemented inert diagnostic, partial native results from both terminals and the identified decoder compatibility gap. |
| [Guided TUI tour](tui-guided-tour.md) | Selected synthetic tour and renderer previews that reduce repeated native test setup while preserving distinct proof requirements. |
| [TUI prototype preflight](tui-prototype-preflight.md) | Initial environment and dependency findings, proof limits and readiness follow-up. |
| [Threat model](threat-model.md) | Proposed D1 assets, adversaries, trust boundaries and required bootstrap/status defenses. |
| [User service and configuration](user-service-configuration.md) | Required user service, `$HOME/.asura/`, project-context and configuration behavior; concrete mechanisms remain open. |
| [Validation strategy](validation-strategy.md) | Proposed D7 evidence plan for the first Protobuf bootstrap packet; broader I0 and release gates remain open. |
| [YAML configuration commands](config-commands.md) | Typed YAML configuration display, get/set and scope contracts. |
| [Bounded shell tool](shell-tool.md) | Selected noninteractive shell contract, separate execution grant, confined process group and truthful cleanup limits. |
| [Storage adapter ownership](storage-adapters.md) | File and SurrealDB persistence ownership and migration packets. |
| [Event routing and readiness](event-routing.md) | Bounded priority inboxes, service readiness and recorded integration evidence. |

The [architecture and design plan](../plans/architecture-and-design.md) orders
these artifacts. The [visual reading path](../decisions/README.md#visual-reading-path)
links the canonical ownership, lifecycle and data diagrams.

Return to the [documentation index](../README.md).

- [Response activity and navigation](response-activity.md): live tool activity and expandable response history.

- [Consolidated parameterised tools](consolidated-tools.md): typed grouped commands over canonical operation owners.

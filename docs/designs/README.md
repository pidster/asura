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
| [Composer editor navigation](composer-editor-navigation.md) | Selected visual row boundaries, row and word movement, the Shift+Up/Down focus chain and bounded draft history recall. |
| [Help mode](help-mode.md) | Client-local `?` help: search over the user guide, area help and a single help source compiled into the client; implemented 2026-10-14. |
| [Person command mode](person-command-mode.md) | Proposed `!` mode: person-typed commands run as tool jobs with a person execution profile, complete retained output and a person notice; PC1–PC12 resolved; threat-model person section approved 2026-10-15 with secret filtering (EX4); waits for job packets and the secret detection design. |
| [Service status pane](service-status-pane.md) | Selected connection ownership, service uptime and bounded stored-memory footprint observations; implemented, with no runtime evidence recorded. |
| [Context observations](context-observations.md) | Service-owned filesystem signals, bounded Git collection and scoped status observations; runtime qualification required. |
| [Sensors](sensors.md) | Passive and directed observations, durable evidence inspection, held proposals and selected System 1/System 2 direction. |
| [Turn usage helper](turn-usage-helper.md) | Reviewed request-correlated counters and conservative budget settlement through existing helper owners; implemented, with final qualification recorded in response activity. |
| [Result retention and graph packet](result-retention-graph-packet.md) | Proposed complete captured-result storage and typed thread/evidence graph; analysis and review only. |
| [Tool job controller](tool-job-controller.md) | Conversation-scoped tool jobs with retained output, no fixed lifetime, reference-only results and model/client reconnect; blocking decisions resolved 2026-10-14; threat-model review completed 2026-10-15; J1 waits for a secret detection design (EX4), J2 for escapee detection (EX5) and per-provider shell qualification (EX10); not implemented. |
| [Context composer](context-composer.md) | Selected per-request composition of model input from the model's window: calibrated token conversion, fidelity levels, outcome-based excerpts, manifests and streamed window-sized input; slice CC1 implemented 2026-10-15; later slices wait on job packets and loop-closure decision 6. |
| [Loop closure and endings](loop-closure.md) | Owner-confirmed stream model for loop closures, interrupts and permanent or temporary endings; operators and per-loop mapping proposed, not implemented. |
| [Tool output views and limits](tool-output-views.md) | Selected principles: one retained stream with person and model views, journal references, fixed transport units, evidence-derived turn backstop; classifies 49 tool limits. |
| [Asura state isolation](asura-state-isolation.md) | Implemented EX16 fix: project opens reject roots overlapping `~/.asura` by directory identity, including firmlink and mount-point spellings; overlapping projects are listed unavailable; the model shell profile denies `~/.asura` except its scratch. |
| [Secret detection](secret-detection.md) | Selected 2026-10-15: Wisp's secret rules ported to Rust on the capture worker, fail-closed loss markers, per-job `$ASURA_SECRET_<n>` placeholders, safe command rewrite to per-execution variables, digests over redacted bytes, no time bounds; slice SL1 (rules, scanner, tokenizer, rewrite planner) implemented 2026-10-15 with exact Wisp parity; SL2–SL3 unblock J1. |
| [Execution threat model](execution-threat-model.md) | Reviewed by the owner 2026-10-14 and 2026-10-15: threats, mitigations, gaps and probe results for shell, memory writes, model reads and endpoints; decisions EX1–EX16 with required follow-up designs; person-command section and PX1–PX6 approved; remote providers wait for EX13 and EX14. |
| [Conversation projections](conversation-projections.md) | Required separation of evidence, audit, model context and presentation; proposed richer activity, result retention and recomposition packets. |
| [Model context telemetry](model-context-telemetry.md) | Native model identity and measured input context, with explicit basis and asynchronous event delivery. |
| [Model providers](model-provider-integration.md) | Local and remote provider abstraction, endpoint locality and staged Ollama, Core AI and MLX integration. |
| [Model tools](model-tool-execution.md) | Shared service tool execution, task grants, bounded native model callbacks and staged activation. |
| [Context storage](context-storage-candidates.md) | Required embedded/external SurrealDB and hybrid file/database ownership, recovery and backup boundaries. |
| [Core harness](core-harness-brief.md) | Required and proposed lifecycle, admission, budget, context and model-session contracts for D3-D4. |
| [Conversation admission](conversation-admission.md) | Format-1 authority, processing pipeline, durable conversation admission and queued input contracts; CA-A through CA-D implemented, listed fault qualification open. |
| [Project names and rename](project-names.md) | Selected capitalized project display names and durable selected-project rename contract; implementation and focused runtime checks recorded, full acceptance matrix open. |
| [Managed input queue](managed-input-queue.md) | Selected all-input queue, durable reorder, sequential dispatch, steering and compact composer projection; implemented, focused checks recorded, full validation matrix open. |
| [Conversation restoration](conversation-restoration.md) | Selected project-scoped latest conversation restoration, explicit `/new`, bounded control reads and recovery checks. |
| [Domain model](domain-model.md) | Proposed D0 identities, relationships and ownership; concrete schemas remain later design work. |
| [Hybrid memory ontology](hybrid-memory-ontology.md) | Proposed file, document and graph memory, plan/task tracking, dependencies, typed built-in tools, authority boundaries, selected home paths and recovery checks. |
| [Graph memory and instruction evolution](graph-memory-evolution.md) | Proposed versioned knowledge graph, instruction lineage, temporal authority, plan and progress links, typed queries and reconciliation. |
| [Early production status slice](early-production-status-slice.md) | Production TUI, lifecycle and presentation contracts, with staged delivery and recorded validation. |
| [Interaction and extension boundaries](interaction-and-extension-boundaries.md) | Required interaction goals, three in-chat command categories and integration boundaries; proposed input-processing separation. |
| [Product workflows](product-workflows.md) | Selected first-release scope, required workflows and proposed acceptance targets. |
| [Platform capabilities](platform-capabilities.md) | D1 macOS 27 SDK evidence and proposed home, service and local-authentication mechanisms. |
| [Protobuf toolchain bootstrap](protobuf-toolchain-bootstrap.md) | Shared tools for control and model schemas; verified download, test-only smoke exchange and offline qualification. PB0.0–PB0.2 implemented and locally verified; PB0.3 incomplete; remaining work deferred under service-first delivery. |
| [Protobuf cache preparation](protobuf-cache-preparation.md) | Approved PB0.3 archive, manifest and driver contracts; live preparation is not yet qualified. |
| [Protobuf process settlement](protobuf-process-settlement.md) | Selected qualified-toolchain cleanup contract and recorded test-only qualification; implementation and native validation incomplete. |
| [Persistence and recovery](persistence-recovery.md) | Proposed D3 local control-store, installation, graph-binding, registration and backup protocol for I1. |
| [Production bootstrap and status](production-bootstrap-status.md) | Original bootstrap proposal with selected automatic setup, project registration and scoped status amendments. |
| [Repository agent configuration](repository-agent-configuration.md) | Selected development-agent configuration, directory-scoped instructions and recorded configuration checks. |
| [Release distribution](release-distribution.md) | Selected Homebrew tap direction and proposed artifact, formula, upgrade and I9 qualification contract. |
| [Requirements and validation](requirements-validation.md) | Proposed D0 traceability from requirements to design owners, delivery increments and required evidence. |
| [Runtime architecture](runtime-architecture.md) | Proposed process layout, asynchronous pipelines and coordination mechanisms. |
| [Security policy](security-policy-brief.md) | Required capabilities and proposed authorization contract; policy and OS mechanisms remain open. |
| [System architecture](system-architecture.md) | Proposed D2 standalone per-user service transport, owner arbitration and fencing for I1; foundation service and control codec implemented, host fencing proof open. |
| [Local-model boundary and Swift implementation](swift-rust-boundary.md) | Proposed D2 portable capability contract; the selected first-conversation Swift helper is implemented; production sandboxing, packaging and the second binding remain open. |
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
| [Bounded shell tool](shell-tool.md) | Implemented noninteractive shell contract, separate execution grant, confined process group and truthful cleanup limits; scoped native validation recorded 2026-09-28. |
| [Storage adapter ownership](storage-adapters.md) | File and SurrealDB persistence ownership and migration packets. |
| [Event routing and readiness](event-routing.md) | Bounded priority inboxes, service readiness and recorded integration evidence. |
| [Response activity and navigation](response-activity.md) | Implemented live tool activity and expandable response history, with recorded qualification on 2026-09-30. |
| [Consolidated parameterised tools](consolidated-tools.md) | Implemented typed grouped commands over canonical operation owners; validation recorded 2026-09-28. |

The [architecture and design plan](../plans/architecture-and-design.md) orders
these artifacts. The [visual reading path](../decisions/README.md#visual-reading-path)
links the canonical ownership, lifecycle and data diagrams.

Return to the [documentation index](../README.md).

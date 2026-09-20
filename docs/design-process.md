# Design before code

Never write code without a design. The architecture baseline establishes project
direction; a scoped design makes a change concrete enough to implement and test.
This rule covers production code, test code, scaffolding, build scripts, and spikes.

Current project phase: design and implementation planning only. Repo owners must
review both thoroughly, and explicitly authorize starting implementation, before
any product code, tests, scaffolding or build scripts are written. A ready design,
ADR or packet alone does not open that gate.

## Workflow

1. Read the architecture, engineering standards, relevant designs, and existing
   contracts. Identify conflicts and stop for the user if instructions disagree.
2. Identify the existing owner of each affected capability. For a new capability,
   assign its ownership and explain how it fits the dependency structure.
3. Write or update the scoped design. Resolve the decisions needed to implement
   that scope; record remaining unrelated questions explicitly.
4. Check the design against architecture, security, UX, observability, and testing
   requirements. Only then write the code it governs.
5. Validate the implementation, update documentation, and report evidence against
   the design's acceptance criteria. Revise the design first if implementation
   reveals a necessary behavioral or architectural change.

Design work can use read-only investigation of existing code, installed APIs,
documentation, and runtime capabilities. An executable experiment needs a design
stating its question, boundaries, and validation before its code is written.

## Required design contents

Keep designs proportionate to the change. A small correction may update an
existing design; a substantial feature needs its own document under `docs/designs/`.
Every governing design must address:

- **Status and scope:** proposed, ready for implementation, implemented, or
  superseded; objectives, non-goals, and dependencies.
- **User behavior:** workflows and observable acceptance criteria.
- **Ownership:** component responsibilities, existing functionality reused, and
  dependency direction.
- **Contracts:** inputs, outputs, errors, state transitions, compatibility, and
  concurrency semantics.
- **Failure and recovery:** relevant deadlines, cancellation, retries,
  idempotency, persistence, restart, and partial outcomes.
- **Security:** authority, data handling, trust boundaries, enforcement, and
  threat assumptions.
- **Operations:** configuration, resource bounds, telemetry, audit, and
  performance objectives.
- **Validation:** mapping from requirements and behaviors to unit, integration,
  and end-to-end cases, plus relevant model, security, performance, and usability
  evidence; name the required environments and acceptance criteria.
- **Decisions:** selected approach, consequential alternatives and tradeoffs,
  unresolved questions, and rollout or migration implications where relevant.

Mark an inapplicable concern explicitly with a reason. An unresolved decision
affecting the scoped behavior prevents that design being ready for implementation.

Record significant cross-cutting or hard-to-reverse choices under
`docs/decisions/`. Decision records explain rationale and link to the canonical
design; avoid copying specifications into multiple documents.

## Mermaid diagram requirements

Architecture, design documents, software plans, and specifications must contain
detailed Mermaid diagrams that implementation agents can follow. Store the
editable source in fenced `mermaid` blocks beside the governing prose. Diagrams
are part of the specification and must be updated with the behavior they describe.

| Document concern | Required diagram content |
| --- | --- |
| Architecture and ownership | Components, responsibility boundaries, dependencies, data/control flows, external systems, and trust/process boundaries where selected |
| API and cross-component interaction | Sequence diagrams naming callers and owners, request/result identity, authorization, persistence points, asynchronous events, and failure/timeout paths |
| Stateful behavior | State diagrams with triggering events, guards, terminal outcomes, interruption and recovery; distinguish a requested control action from its completed effect |
| Data and context | Entity/relationship or class diagrams, identities, cardinalities, versions and provenance; separate logical contracts from physical storage choices |
| Algorithms and decisions | Flowcharts showing inputs, decision conditions, bounded repetition, rejection, fallback and termination |
| Delivery plans | Dependency graphs, prerequisites, readiness gates, deliverables, and validation before completion |

Use the views relevant to the scope; record a reason when a view is inapplicable.
A high-level component picture alone is insufficient for an implementation-ready
stateful or distributed design. Link to an existing canonical diagram rather than
copying it into every document.

Each diagram needs a stable heading, a status (requirement, proposal, or selected
design), and a short explanation of its scope and arrow semantics. Use the same
component names, state names, IDs and stage IDs as the prose and tables. Label
requests, events, guards and dependency edges. Avoid relying on color for meaning.
Split large views at meaningful boundaries to keep labels readable.

Detailed designs must map important branches, transitions and invariants to
acceptance criteria and unit, integration, and end-to-end tests. Missing failure
branches are design gaps. Diagrams must not invent selected transports, database
engines, OS mechanisms, atomicity guarantees, or timing thresholds.

### Design readiness lifecycle

Required process. Arrows name the evidence or event that permits the transition;
the lifecycle applies to each scoped design and its implementation packet.

```mermaid
stateDiagram-v2
    direction LR
    [*] --> Proposed
    Proposed --> Investigating: Identify owners and unresolved contracts
    Investigating --> Proposed: Evidence requires revision
    Investigating --> Ready: Contracts consistent and scope blockers resolved
    Ready --> Implementing: Design and plan reviewed, implementation authorized, packet prerequisites available
    Implementing --> Investigating: Behavioral or architectural change required
    Implementing --> Validating: Implementation and required tests delivered
    Validating --> Implementing: Failure within the governing design
    Validating --> Investigating: Failure exposes a design defect
    Validating --> Implemented: Required evidence passes and docs agree
    Implemented --> Superseded: Replacement design identifies migration
    Superseded --> [*]
```

### Diagram validation

Before delivery, render every changed Mermaid block and check the output for
syntax errors, missing labels, clipped content, and unreadable layout. Check
state transitions and dependency edges against the governing prose as well as
rendering them. Report the renderer/version and any verification limits.

For documentation work, an available local Mermaid CLI may read Markdown files
and render all their diagrams into an isolated temporary directory. Inputs are
the documents; outputs are disposable SVG/PNG previews and CLI diagnostics.
Do not overwrite Markdown sources, install project dependencies, or upload project
documents to a remote rendering service for this check. Renderer failures must
be corrected or reported; rendering alone does not validate architectural semantics.

## Initial state

Swift and Rust are selected. No implementation-ready scoped design or validation
toolchain exists yet. Follow the [architecture and design plan](plans/architecture-and-design.md)
to establish the initial workflows and governing contracts, then the
[implementation plan](plans/implementation.md). The core harness brief is a
design input, not permission to begin its implementation.

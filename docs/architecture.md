# Architecture baseline

Status: required behavior and component responsibilities. Detailed mechanisms
remain open. Runtime behavior is not implemented or verified.

Read the [glossary](glossary.md) for project terms and the
[decision map](decisions/README.md) for selected decisions and detailed models.

## Purpose and platform

Asura is a coding AI harness for macOS 27 and later. It uses Apple's Foundation
Models API for on-device classification, decisions and tool management. The local
model helps the orchestrator determine when a task needs remote AI.

Swift and Rust are the selected implementation languages. Their proposed
responsibilities and repository layout are recorded in the
[architecture and design plan](plans/architecture-and-design.md). Allocation and
interoperation require a detailed design before scaffolding.

Asura inverts the delegation relationship of sibling project Daimon's MCP role:
the local system coordinates work and calls remote AI when appropriate. Daimon
is a reference, not an inherited architecture or dependency.

## Control and execution boundaries

The initial non-interactive CLI and interactive chat/TUI are control clients of
an orchestrator managing local agent instances. The same architecture must
accommodate a GUI and remote machines running Asura agents from day one.

| Component | Owns |
| --- | --- |
| Control clients | Input, presentation, interaction, and delivery of user decisions through the control contract |
| Orchestrator | User-service lifecycle, project context registry, task state, scheduling, delegation and shared budget reservations |
| Configuration resolver | Directory discovery, schema-based composition, source provenance and configuration snapshots |
| On-device decision subsystem | Model-assisted classification, routing, tool selection, and structured decision results |
| Agent runtime | Execution of assigned work and reporting progress and outcomes |
| Host services | Local process and workspace access, credentials, capability enforcement, and resource limits |
| Remote AI adapters | Provider-specific inference requests and response translation |
| Remote host adapters | Communication with and control of Asura execution on other machines |

On a user device, the backend runs once per OS user and manages multiple project
and repository contexts. All local clients connect to that service. Agent and
platform helpers may use separate processes. The service supervisor, helper
boundaries, transport and package structure remain detailed design decisions.
A local deployment must exercise the same semantic control contract that future
interfaces will use.

The backend discovers configuration in a command's directory and its parents.
One resolver combines applicable sources from root to leaf, with explicit schema
rules and inspectable provenance. Project settings cannot change service-owned
settings or expand security permissions. The
[service and configuration brief](designs/user-service-configuration.md) defines
context identity, composition, change handling and validation requirements.

Remote AI inference and execution on a remote Asura machine are distinct
capabilities. Each requires its own authorization, lifecycle, and failure model.

Context storage must support embedded SurrealDB and a configured external SurrealDB
connection through one storage contract. A new installation uses embedded storage
unless an external connection is configured. On restart, the orchestrator verifies
the saved association between the installation and its graph. Missing settings or
connection failure must not select another graph. A change of mode or database
requires an authorized migration or binding change.

External database access needs its own trust, data-transfer and outage rules.
These rules are separate from remote AI and remote agent control. The
[storage brief](designs/context-storage-candidates.md) defines the required behavior
and the remaining engine and connection decisions.

### Logical component and trust boundaries

Requirements view. Arrows name interactions, not compile-time dependencies. Boxes
inside a host are logical responsibilities, not selected process boundaries.
External content is untrusted input; the model's output is a proposal.

```mermaid
flowchart TB
    subgraph Clients["Control surfaces"]
        CLI["Non-interactive CLI"]
        TUI["Interactive chat / TUI"]
        GUI["Future GUI"]
    end
    subgraph Local["User device: one backend owner per OS user"]
        API["Control API: authenticate and authorize"]
        Orch["Orchestrator: lifecycle and scheduling"]
        Registry["Project context registry"]
        Config["Configuration resolver"]
        Agent["Agent runtime: bounded steps"]
        HostAdapter["Remote host adapter"]
        State[("Durable task and action state")]
    end
    Remote["Remote Asura host: independent grant enforcement"]
    CLI -->|Commands and subscriptions| API
    TUI -->|Commands and user decisions| API
    GUI -->|Same semantic contract| API
    API <-->|Authorized commands / task events| Orch
    Orch -->|Own context identities| Registry
    Orch -->|Resolve scoped snapshots| Config
    Orch -->|Assign bounded work| Agent
    Orch -->|Commit lifecycle changes| State
    Orch -->|Authorized placement and control| HostAdapter
    HostAdapter <-->|Authenticated control / outcome evidence| Remote
```

Every client receives the orchestrator's events through the API; return arrows
are condensed here to keep the ownership view legible. Remote results re-enter
the same validation and reconciliation path as local observations.

### Execution and data boundaries

Requirements view for the agent assigned work above. Arrows show requests,
validation and evidence flow. Logical components do not imply separate processes.
Workspace content and provider results cross untrusted-input boundaries.

```mermaid
flowchart TD
    Agent["Agent runtime: bounded steps"] -->|Retrieve scoped evidence| Context["Context subsystem"]
    Agent <-->|Decision request / typed proposal| Model["On-device decision subsystem"]
    Agent -->|Check action and egress scope| Policy["Canonical capability policy"]
    Agent -->|Action with grant and preconditions| Host["Host services"]
    Host -->|Revalidate current grant| Policy
    Host -->|Constrained operations| Workspace["Workspace and tool output"]
    Workspace -->|Untrusted observations| Context
    Agent -->|Authorized inference request| AIAdapter["Remote inference adapter"]
    AIAdapter <-->|Bounded disclosure / untrusted result| Provider["Remote AI provider"]
```

## Control API requirements

The orchestrator API must be modern, asynchronous, highly reliable, very high
performance, and secure. Its design must define:

- Typed, versioned commands and structured events with stable context, task and agent identities.
- Progress streaming, deadlines, cancellation semantics, and bounded backpressure.
- Durable state and reconnect behavior, including event ordering, replay, and retention.
- Idempotency and retry semantics, including how a caller resolves an ambiguous
  outcome after a connection or process failure.
- Authentication, per-operation authorization, and remote machine trust establishment.
- User decision and approval delivery across all supported control surfaces.
- Compatibility and error contracts shared by every client.

Reliability and performance objectives need measurable targets and benchmark
workloads before a transport is selected. Routine control operations must not
depend on waiting for model inference.

### Asynchronous submission and reconnect

Proposed interaction contract for D3. The durability acknowledgement and event
cursor rules need a concrete storage/transport design; the diagram establishes
their required ordering and distinguishes acceptance from completion.

```mermaid
sequenceDiagram
    autonumber
    actor Client as CLI / TUI / GUI
    participant API as Control API
    participant O as Orchestrator
    participant S as Durable state
    participant A as Agent runtime
    Client->>API: Submit task with request identity and caller credentials
    API->>API: Authenticate, authorize and validate version
    alt Invalid or unauthorized
        API-->>Client: Typed rejection, no dispatch
    else Authorized request
        API->>O: Submit scoped command
        O->>S: Resolve idempotency identity and persist acceptance if new
        S-->>O: Existing result or durable new task identity
        O-->>API: Accepted task identity and current revision
        API-->>Client: Acceptance, work may still be pending
        opt New task eligible for scheduling
            O->>A: Assign work with task revision and bounded authority
        end
        Client->>API: Subscribe from last observed event cursor
        API->>O: Authorized subscription
        O-->>API: Replay or snapshot/resync requirement
        API-->>Client: State followed by ordered task events
        A-->>O: Progress and outcome evidence
        O->>S: Persist authoritative outcome and replayable event position
        S-->>O: Durable acknowledgement
        O-->>API: Outcome event
        API-->>Client: Completed, failed, cancelled or reconciliation required
    end
    Note over Client,API: After disconnect, reuse request identity or query task state, do not assume failure
    Note over O,A: Status and cancellation processing must remain independent of model latency
```

## On-device decisions

Model-driven orchestration is a core part of Asura. Its design must specify the
inputs, structured outputs, context budgets, concurrency bounds, and evaluation
criteria for each decision type. Define behavior for unavailable models,
uncertainty, invalid output, timeouts, and failed remote calls.

Model decisions operate within independently enforced permissions and budgets.
Repository content, tool output, and remote responses cannot grant authority.
Task state must remain understandable without reconstructing it from model prose.

The orchestrator reserves aggregate budgets durably before dispatch, including
concurrent children and model operations. Unknown usage is not refunded on timeout
or restart. Every terminal failure trigger stops new work and accounts for effects
through the same settlement procedure. The [decision map](decisions/README.md)
links rationale, detailed lifecycle/admission diagrams and delivery evidence.

Model session history and application-managed caches are part of the effective
context whenever they can influence a later response. They must obey the same
scope, provenance, budget and invalidation rules as newly selected evidence.
The [model session contract](designs/core-harness-brief.md#model-session-ownership-and-effective-context)
assigns ownership and requires stateless calls or explicitly isolated sessions.

## Security boundaries

Protection is bidirectional: protect Asura from external interference and protect
the operating system from agent overreach or compromise. Use OS-local capabilities
to enforce the selected isolation and least-privilege design.

Before execution or remote control is implemented, define a threat model,
adversary capabilities, trust boundaries, credential lifecycle, and recovery
behavior. Select and verify the macOS mechanisms against those requirements.
State limitations explicitly; no protection against a particular adversary is
established merely by naming an OS facility.

Permission checks must be authoritative at the execution boundary. Distributed
hosts must enforce their own grants; a UI or model decision cannot bypass them.

Asura must support a documented, versioned policy format or standard for command
invocation, sandbox access control, and other security controls. One canonical
authorization contract must apply across all clients and agent hosts, with host
enforcement of the resulting limits. Policy evaluation and OS enforcement are
distinct responsibilities. The [security policy brief](designs/security-policy-brief.md)
defines the required scope and evaluation work; the format and engine remain open.

## User experience

The experience must be elegant, intuitive, configurable, and controllable.
Clients should explain what is happening, why, and what the user can do next.
Design workflows for:

- Useful defaults with advanced settings exposed when needed.
- Inspecting, pausing, cancelling, and redirecting work with explicit effect semantics.
- Understanding remote AI use, delegation, permission requests, and failures.
- Reconnecting and switching interfaces without losing task state or pending decisions.
- Consistent configuration precedence and an inspectable effective configuration.
- Keyboard interaction, accessibility, readable output, and machine-readable CLI results.

UX scenarios and usability evidence belong in feature designs and validation.

## Observability

Support OpenTelemetry metrics, logs, and spans. Correlate activity across control
clients, orchestration, local model decisions, agents, tools, and remote calls,
propagating trace context across process and machine boundaries.

Measure routing outcomes, inference latency, queue time, retries, cancellations,
and resource use. Define bounded attribute cardinality and export buffering.
Collector failure must not stall task execution. Sensitive payloads such as
prompts, source code, tool output, and credentials are excluded by default;
any additional content collection requires an explicit data-handling design.

Security audit durability and retention must be designed separately from trace
sampling. Telemetry is not the authoritative task state store.

### Telemetry, audit and task state

Requirements view. Solid arrows show data flow; overflow behavior is bounded and
must be selected in D6. Audit persistence failure policy remains a separate
security decision and cannot inherit telemetry's lossy behavior.

```mermaid
flowchart LR
    Work["Clients, orchestrator, model, tools and remote adapters"] -->|Correlated operational signals| Redact["Redaction and bounded attributes"]
    Redact --> Queue["Bounded OTel export buffer"]
    Queue -->|Metrics, logs and spans| Collector["Configured collector"]
    Collector -->|Unavailable| Limit["Bounded retry / overflow policy; control stays responsive"]
    Limit --> Queue
    Work -->|Security-relevant events| Audit["Audit writer and independent durability policy"]
    Audit --> AuditStore[("Audit records")]
    Work -->|Authoritative transitions through owning component| State[("Task and action state")]
    State -->|Replayable progress| Client["Control event stream"]
```

## Single ownership of functionality

Each capability and contract has one canonical owner. Control clients do not
reimplement scheduling or authorization. Adapters translate protocols and
provider formats while shared behavior stays with its owner. Model selection
and tool metadata must have canonical definitions rather than client-specific copies.

Logical ownership does not require a single process. Designs must describe how
shared contracts and host-local enforcement remain consistent across deployments.

## Decisions required before implementation

1. Initial workflows, task/agent lifecycle, and UX acceptance criteria.
2. Module ownership, user-service supervision, helper topology and remote deployment contracts.
3. Control protocol, compatibility rules, persistence, and recovery semantics.
4. Threat model, host isolation mechanisms, identity, and capability grants.
5. Decision subsystem responsibilities and model evaluation criteria.
6. Remote AI and remote host integration boundaries.
7. Configuration, credentials, audit, and telemetry schemas and lifecycles.
8. Swift/Rust allocation and toolchains, testing infrastructure, quality gates,
   and measurable objectives.

Resolve the decisions required by a scoped implementation before coding that
scope. Keep unrelated open decisions explicit and preserve the boundaries above.

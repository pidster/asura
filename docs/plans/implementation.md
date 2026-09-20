# Implementation delivery plan

Status: proposed sequence, gated by the
[architecture and design plan](architecture-and-design.md). No code is to be
written from this plan alone. Swift/Rust allocation and paths refer to that
plan's proposed layout until D2 resolves them.

## Entry gate

Implementation is not currently authorized. Begin I0 only after repo owners have
reviewed both the design and this plan, explicitly authorized implementation, and
D8 establishes coherent system boundaries with ready initial implementation
designs. Each later increment requires its own ready
design and completed dependencies. A later feature may retain detailed open
questions only if they do not undermine an earlier contract or security boundary.

Do not defer testing, observability, or security to a final hardening phase.
Every increment includes its unit, integration, and end-to-end coverage, relevant
model evaluations and operational documentation. Hardware/provider-dependent
checks require the real environment before claiming those behaviors work.

## Increment sequence

### Delivery dependencies

Proposed delivery graph. Solid arrows are prerequisite milestones; transitive
edges are omitted. Dashed arrows into release scope mean feature inclusion is
decided at D0, not that every release must contain the GUI and remote hosts.

```mermaid
flowchart TD
    D8["D8: architecture consistent and first designs ready"] --> Review["Owner review of design and implementation plan"]
    Review --> Authorized["Explicit implementation authorization"]
    Authorized --> I0["I0: repository, contracts and quality infrastructure"]
    I0 --> I1["I1: local control and durable lifecycle"]
    I1 --> I2["I2: context and bounded execution"]
    I2 --> I3["I3: on-device harness loop"]
    I3 --> I4["I4: interactive chat / TUI"]
    I4 --> I5["I5: remote inference assistance"]
    I5 --> I6["I6: validated coding changes"]
    I6 --> I7["I7: remote Asura hosts"]
    I4 --> I8["I8: native GUI"]
    GUIReq["User GUI requirements and ready design"] --> I8
    D0["D0: selected release capabilities"] --> Scope["Completion gate for all selected increments"]
    I4 -.->|Local terminal capability| Scope
    I6 -.->|Remote-assisted coding capability| Scope
    I7 -.->|Remote-host capability| Scope
    I8 -.->|GUI capability| Scope
    Scope --> I9["I9: release qualification and distribution evidence"]
```

Every milestone also depends on its own ready design and the required validation
environment. Those gates apply even where the graph only shows delivery ordering.

| Increment | Depends on | Deliver | Exit demonstration |
| --- | --- | --- | --- |
| I0: repository and contracts | D8; ready build/layout/contract designs | Minimal Cargo/Swift package structure, pinned toolchains, generated bindings, CI and test runners; initial telemetry setup | Clean build on supported macOS; cross-language contract round trip, compatibility rejection and bounded cancellation through a test composition |
| I1: local control and durable lifecycle | I0 | Host entry point, control API, client library, non-interactive CLI; task IDs, event stream, configuration, local caller authentication and authorization; selected policy format validation, evaluator and atomic activation; durable control intent and user-decision deadline arbitration | Submit a no-effect diagnostic task, inspect it, attach a second client, disconnect/reconnect, cancel and recover state after restart; preserve control intent received during reconciliation; reject unauthorized callers and invalid policy updates; replay response/deadline/cancel races with one durable outcome |
| I2: context and bounded host execution | I1 | Initial graph/storage, workspace observations, canonical tool registry, grants, policy-to-sandbox enforcement, constrained read/process tools and action recovery; explicit task-local scratch/output grants with read-only source access | CLI runs a permitted fixture inspection/build with graph-linked evidence; source-write attempts by commands and descendants are denied; unsupported restrictions, policy revocation/launch races, output bounds, timeout, crash ambiguity and stale files are handled; cancellation during reconciliation cannot resume work |
| I3: on-device harness loop | I2 | Swift model adapter/service, core loop, structured decisions, effective context manifests, model-state isolation and invalidation, progress/budget rules and evaluation suite | Real on-device decisions complete a bounded local task; invalid or stale proposals are rejected; stalled/unavailable model does not block control; session state cannot cross task/authority scope or bypass provenance and budgets; terminal decision timeout cannot start another model call or action |
| I4: interactive chat/TUI | I3 | Streaming chat, task/agent views, explainable decisions, pause/resume/cancel/redirect and pending user decisions | Reattach without state loss, resolve a decision from either client, steer active work, navigate by keyboard, and recover from errors clearly |
| I5: remote AI assistance | I3, I4 | One selected remote inference provider behind canonical contracts, egress context views, credential handling and budgets | Local decision requests bounded remote help; authorized context is sent; rate limits, failure, denial and ambiguous provider outcomes remain visible and bounded |
| I6: coding task completion | I2-I5 | Designed change/patch application and validation workflow, workspace concurrency controls and reviewable artifacts | Reproduce a failing fixture test, investigate, make a scoped change, run validation and present evidence; handle stale edits, failures and cancellation without claiming unverified success |
| I7: remote Asura hosts | I6 | Host enrollment and identity, task placement, remote execution control, grant expiry/revocation, reconciliation and compatibility handling | On two actual supported machines, disconnect during execution, revoke authority, reconnect and reconcile; prevent duplicate dispatch by stale owners |
| I8: GUI | I4; user's GUI requirements and ready GUI design | Native Swift client over the established control contract | GUI and terminal observe/control the same task without duplicated orchestration rules; pass accessibility and usability scenarios |
| I9: release qualification | Required release increments | Signed/distributable artifacts, install/upgrade/recovery guidance, compatibility and performance evidence | Fresh supported host installation, upgrade/migration recovery, security regressions, collector outage, sustained workload and usability acceptance all meet the design |

I2 uses deterministic fixture requests to verify execution before connecting the
model loop. Such fixtures are test drivers of the production interfaces, not an
alternative harness implementation. I2 may grant writes only to explicitly
identified task-local scratch and generated-output locations, including bounded
build artifacts and caches. The source workspace remains read-only for tools,
build scripts and all child processes. A build that requires source writes must
be rejected or redesigned to use authorized outputs; calling it a build does not
expand its authority. Source modification is first enabled when I6's edit design
and enforcement tests are ready. Generic command execution cannot bypass that gate.

### Early-increment acceptance gates

I2's context storage delivery includes both optional embedded SurrealDB and a
configured external connection, following the [storage brief](../designs/context-storage-candidates.md).
Verify embedded initialization when no external connection is configured and
external initialization when one is configured; reject partial settings without
falling back. Reopening must honor the persisted binding, reject missing or
mismatched identity/configuration, and recover interrupted migration/rebinding.
Include the storage brief's B1-B5 cases and ensure bootstrap metadata does not
require an embedded graph in external mode.
Require configuration/security unit cases, real embedded/server integration and
CLI workflows for both modes. External-mode validation must cover authenticated
connection setup, incompatible schemas, partitions and unknown commits, responsive
status/cancellation, and operation without an embedded graph store. Do not defer
this capability to remote inference (I5) or remote Asura hosts (I7); those are
different boundaries. The exact engine, transport and schema require ready designs.

These cases refine delivery evidence, not the canonical lifecycle or context
contracts in the [core harness brief](../designs/core-harness-brief.md). Detailed
designs must retain the cases when resolving implementation mechanisms. Under the
proposed allocation, the Rust orchestrator owns durable task/control arbitration,
the Rust context subsystem owns effective-context provenance, the Swift adapter
owns model-session mechanics, and host execution owns confinement of the full
process tree. D2 must select their process and Swift/Rust boundary contracts;
placing them in one process cannot remove the checks.

| Gate | Unit acceptance | Integration acceptance | End-to-end acceptance |
| --- | --- | --- | --- |
| I1-I2: common failure settlement | Exercise fatal error, task deadline and budget exhaustion in every nonterminal state, including paused/waiting, racing success/cancel; no new dispatch after winning failure intent | Crash before/after failure persistence and during stopping; recover unresolved effects and usage without deadline reset or refunded reservations; exercise authority-store outage and bounded recovery exhaustion | CLI reports failure or reconciliation during an active fixture action, restart preserves the cause, and terminal output distinguishes accounted effects from uncertainty |
| I2: aggregate admission and recovery | Competing children cannot exceed an ancestor cap; duplicate admission and settlement are idempotent; typed limits, context capacity and occupancy remain distinct | Race real durable admissions for the last allowance, crash around reservation/dispatch/settlement, inject unknown usage and store outage; no double spend or unearned refund | Concurrent fixture operations consume one shared task allowance; cancel/restart with uncertain usage leaves that allowance reserved and explains why further work cannot be admitted |
| I1-I2: control during reconciliation and decision expiry | Exercise pause/cancel received during reconciliation, cancellation precedence, and each response/deadline/cancel ordering; terminal decision expiry forbids new model/work dispatch while existing effects settle | Restart between durable control-intent receipt and effect settlement; replay the winning decision outcome; deliver late results and ensure they cannot resume work after cancellation or timeout | CLI disconnects during an uncertain action, accepts cancel while reconciling, then reconnects without new dispatch; pending decision expires with a visible terminal failure after required reconciliation, not another decision loop |
| I2: scratch/output writes with read-only source | Reject grants and path resolutions that exceed authorized output roots, including attempts to alias source files through links or traversal; distinguish generated-output authority from source-edit authority | On supported macOS, execute actual fixture builds and hostile build scripts/descendants that attempt direct and indirect source writes or output-root escape; verify allowed outputs succeed and source contents remain unchanged | Through the CLI, a permitted out-of-source fixture build produces evidence and bounded artifacts; a fixture requiring source mutation is denied with an explanation and no source change |
| I3: model-state isolation and invalidation | Verify scope/generation matching, retained-context budget accounting, and invalidation on revocation, deletion or cancellation; reject stale results and cross-task state reuse | Across the selected Rust/Swift boundary, retire/rebuild affected model state, inject late completions and adapter restart, and confirm no invalidated contribution or untracked history can enter accepted context | With the real on-device model, interleave tasks containing distinguishable private fixtures, invalidate one task's context during inference, and verify only current scoped results can drive actions; inspect provenance and state-use evidence in addition to generated text |

I2 cannot pass without real host-confinement evidence for every process capability
it exposes. If the selected macOS mechanism cannot enforce the required boundary,
the affected process tool remains unavailable; reducing the source protection is
not an acceptable substitute. I3 similarly cannot pass by checking model responses
alone: the evidence must establish what state was eligible for each invocation.
Two-host reconciliation evidence is added in I7; early local fault injection does
not establish remote-host guarantees.

I3 extends budget admission to real local inference and framework callbacks. I5
requires real provider evidence for billable bounds, ambiguous responses and late
usage settlement; mock billing alone cannot prove a hard monetary cap. I7 requires
two-host envelope/fencing tests through partition, restart and stale-owner handoff.
These extend the canonical I2 budget authority rather than introducing independent
provider or child-task accounts. Common failure settlement applies at each new
boundary. [ADRs 0002 and 0003](../decisions/README.md) record the rationale.

The first useful terminal milestone is I4: both non-interactive and interactive
local workflows. I6 adds the remote-assisted coding workflow. Remote-host and GUI
delivery are separate capabilities; I8 need not wait for I7 unless its requirements
depend on remote hosts. Define release scope at D0, and never call an incomplete
feature shipped because its interface was reserved.

## Cross-cutting delivery in every increment

- Add instrumentation at the component boundary where behavior first exists;
  propagate trace context, control cardinality, redact payloads and exercise
  collector unavailability. Verify Swift/Rust exporter capability before claiming
  equivalent support for all three OpenTelemetry signals.
- Add capability enforcement before exposing an effect, including local-only
  operations. Remote transports start with authenticated/authorized test requests.
- Apply the selected policy contract through its canonical owner. Include policy
  conformance and actual host-enforcement evidence for every new protected effect;
  follow the [security policy brief](../designs/security-policy-brief.md) and its
  successor detailed designs. Do not add client-specific permission engines.
- Deliver meaningful unit, integration and e2e assertions with the feature.
  Include failure injection and recovery tests as durable boundaries are introduced.
- Update the requirements/test matrix, public contract compatibility fixtures,
  design status and user-facing documentation.
- Measure latency, throughput and bounded resource usage against the budgets
  selected during design; include graph retrieval and concurrent model work.

## Implementation packet contract

Before an implementation agent starts, record:

1. Packet ID, objective, governing design revision and prerequisite evidence.
2. Exact owned files/modules and explicit non-goals.
3. Existing functionality to reuse and contracts consumed or produced.
4. Acceptance criteria and unit/integration/e2e cases, with the required real
   environments and canonical check commands.
5. Security, resource, failure and compatibility invariants that must remain true.
6. Integration order, documentation changes and completion evidence required.

Reject a packet with unresolved implementation-affecting design questions. If
implementation reveals a contract conflict, stop and resolve the design before
continuing. Do not expand a packet to absorb adjacent work or invent a duplicate
helper because another component is inconvenient to call.

### Packet execution and completion gate

Required workflow for implementation agents. Edges name checks and outcomes;
scope or instruction discrepancies require stopping work and raising the issue
with the user, as required by `AGENTS.md`.

```mermaid
flowchart TD
    Packet["Read packet, governing design revision and owned paths"] --> Conflict{"Instruction or ownership conflict?"}
    Conflict -->|Yes| Stop["Stop and bring discrepancy to user"]
    Conflict -->|No| Ready{"Design ready and prerequisites evidenced?"}
    Ready -->|No| Design["Resolve design or dependency gap before coding"]
    Design --> Packet
    Ready -->|Yes| Authority{"Design and plan reviewed and implementation authorized?"}
    Authority -->|No| Hold["Remain in design and planning"]
    Authority -->|Yes| Reuse["Locate canonical implementations and contracts"]
    Reuse --> Implement["Implement only the packet's owned scope"]
    Implement --> Changed{"Requires a contract or design change?"}
    Changed -->|Yes| Design
    Changed -->|No| Tests["Run required unit, integration and e2e checks"]
    Tests --> Extra["Run applicable model, security, performance and UX checks"]
    Extra --> Pass{"All required evidence available and passing?"}
    Pass -->|No: implementation failure| Implement
    Pass -->|No: environment missing| Gap["Record incomplete validation; do not claim completion"]
    Gap -->|Environment becomes available| Tests
    Pass -->|Yes| Docs["Reconcile diagrams, contracts, user docs and test matrix"]
    Docs --> Handoff["Report design revision, changes, commands and proof limits"]
```

### I6 coding workflow acceptance sequence

Proposed end-to-end scenario for I6. It composes prior milestones through their
canonical interfaces. The remote path is conditional, and edit execution has
independent authorization and stale-source checks.

```mermaid
sequenceDiagram
    autonumber
    actor User
    participant UI as CLI / TUI
    participant O as Orchestrator and agent core
    participant C as Context subsystem
    participant M as Swift model adapter
    participant R as Remote inference adapter
    participant H as Host execution
    User->>UI: Investigate and fix failing fixture test
    UI->>O: Submit scoped task
    O->>H: Authorized bounded test run
    H-->>O: Exit status and captured evidence
    O->>C: Record observations and assemble versioned context
    C-->>O: Context view and provenance
    O->>M: Request next decision with eligible tools and budget
    M-->>O: Typed proposal with evidence references
    opt Proposal requests remote help and egress is authorized
        O->>R: Bounded request with approved context manifest
        R-->>O: Untrusted response or typed failure
        O->>C: Record result and re-evaluate evidence
    end
    O->>O: Validate proposed change and current authority
    O->>H: Apply permitted change against expected source revision
    alt Source changed or grant denied
        H-->>O: Rejection with no edit applied
        O-->>UI: Refresh context or request user decision
    else Edit applied
        H-->>O: Actual changed artifacts
        O->>H: Run required validation under its own grant
        H-->>O: Validation results and artifacts
        O->>C: Link changed sources, action results and test evidence
        O-->>UI: Reviewable change and verified result or explicit failure
    end
    UI-->>User: Explain outcome, evidence and remaining work
```

## Verification and release evidence

Use progressively broader evidence: deterministic component tests; real storage,
protocol and process integration; supported-macOS end-to-end workflows; actual
Foundation Models evaluations; credentialed provider checks; and two-host tests.
Each increment runs all layers applicable to its claims, not just the cheapest one.

Maintain a capability matrix with designed, implemented, verified and released
states. Record environment and versions for every platform-dependent result.
Document unverified limitations instead of silently substituting mocks.

I9 consolidates already-running checks and tests distribution-specific behavior;
it is not the first security, UX, load, or recovery review. The final release gate
must include user workflows, hostile input, interrupted actions, upgrades, and
clean uninstall/data-retention behavior specified in D7.

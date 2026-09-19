# Core harness: design brief

Status: proposed semantics and open questions. This is an input to D3-D4 in the
[design plan](../plans/architecture-and-design.md), not an implementation-ready
specification. Detailed algorithms, schemas, thresholds, and test cases remain
to be resolved there.

## Ownership and execution model

Propose one durable orchestration state machine coordinating bounded agent steps.
The orchestrator owns scheduling and task/agent lifecycle; the agent core owns
step semantics; the context subsystem owns evidence retrieval and assembly;
host execution enforces action grants. Avoid a second scheduler or competing
task state machine inside a model adapter or UI.

Treat each model interaction as an operation within this outer loop. Determine
whether Foundation Models interactions return structured proposals, use bounded
tool callbacks, or both. Any framework-managed tool calls must pass the same
action lifecycle and enforcement boundary. A hidden nested loop cannot escape
step, tool-call, time, or spending budgets or make cancellation unknowable.

Candidate task states are queued, running, waiting for user, waiting for external
work, paused, reconciling, succeeded, failed, and cancelled. D3 must define which
are persisted states versus derived views. Pause and cancellation requests are
distinct from their eventual effect; define safe points and non-interruptible work.

### Proposed task lifecycle

Proposal for D3. Edge labels specify triggers and completion guards. D3 must
decide which states are durable and whether pause/cancel intent is a separate
field. Normal progression and interruption are split into complementary views.

```mermaid
stateDiagram-v2
    [*] --> Queued: Authorized acceptance persisted
    Queued --> Running: Valid assignment and grant
    Running --> WaitingUser: User decision required
    WaitingUser --> Running: Authorized current-revision response
    Running --> WaitingExternal: Bounded dispatch
    WaitingExternal --> Running: Outcome reconciled
    Running --> Succeeded: Acceptance criteria verified
    Running --> Failed: Fatal error or task budget exhausted
    WaitingUser --> Failed: Decision deadline expires
    Succeeded --> [*]
    Failed --> [*]
```

### Proposed interruption and recovery states

Companion proposal for D3. `ActiveWork` is a diagram entry representing the
current running or waiting state, not a new stored task state. Queued and paused
tasks may also be cancelled. Pending control intent survives reconciliation.

```mermaid
stateDiagram-v2
    state "Running or waiting work" as ActiveWork
    ActiveWork --> PausePending: Pause requested
    PausePending --> Paused: Safe point and effects accounted for
    Paused --> ActiveWork: Resume with fresh context and grant
    ActiveWork --> CancelPending: Cancel requested
    Queued --> CancelPending: Cancel requested
    Paused --> CancelPending: Cancel requested
    PausePending --> CancelPending: Cancel supersedes pause
    CancelPending --> Cancelled: Dispatch stopped and effects accounted for
    ActiveWork --> Reconciling: Unknown outcome or recovery required
    PausePending --> Reconciling: Effect uncertain
    CancelPending --> Reconciling: Effect uncertain
    Reconciling --> ActiveWork: Resolved and no pending control intent
    Reconciling --> Paused: Resolved and pause remains
    Reconciling --> Cancelled: Accounted for and cancel remains
    Reconciling --> Failed: Recovery contract cannot be satisfied
    Cancelled --> [*]
    Failed --> [*]
```

On startup, persisted nonterminal work with potentially outstanding effects enters
reconciliation before redispatch. Cancellation cannot undo an already-completed
effect; its terminal report must identify residual effects. Terminal states are
not reopened by late results, which are retained as evidence for investigation.

## Proposed agent step

| Step | Responsibility and resulting evidence |
| --- | --- |
| 1. Observe | Accept user input and completed action observations; check cancellation, grants, task revision and budgets |
| 2. Assemble | Select a versioned context view with provenance, freshness and bounded size; include current task constraints |
| 3. Decide | Ask the on-device subsystem for a typed proposal: gather context, perform a tool action, delegate, ask the user, wait, or finish |
| 4. Validate | Validate schema, task relevance, context revision, capabilities, data egress, resource limits and preconditions; reject or request clarification |
| 5. Record intent | Durably bind action identity, task revision, selected inputs and grant scope before dispatch; define expiry and recovery behavior |
| 6. Execute or wait | Dispatch to the local agent/tool, remote inference adapter, or remote host; stream bounded progress and honor deadlines |
| 7. Reconcile | Record actual outcome, artifacts, evidence, and unknown effects; update durable state and graph consistently |
| 8. Evaluate progress | Check acceptance criteria and independent validation results; continue, replan, ask, or terminate with an explicit reason |

### Proposed step control flow

Proposal for D4. Solid arrows show step ordering and guarded branches. The
execution box covers local tools, remote inference, and remote-host operations;
all go through the action lifecycle below.

```mermaid
flowchart TD
    Observe["Observe inputs, results and task revision"] --> Control{"Cancel or pause pending?"}
    Control -->|Yes| Settle["Stop new dispatch and reconcile outstanding effects"]
    Settle --> Controlled["Report paused, cancelled or reconciling"]
    Control -->|No| Budget{"Authority and remaining budgets valid?"}
    Budget -->|No| Stop["Explain blocked or failed outcome"]
    Budget -->|Yes| Context["Assemble scoped versioned context"]
    Context --> Decision["Request bounded on-device decision"]
    Decision --> Valid{"Schema, evidence and task revision valid?"}
    Valid -->|No| Retry{"Retry or refresh within budget?"}
    Retry -->|Yes| Observe
    Retry -->|No| Stop
    Valid -->|Yes| Kind{"Proposed next step"}
    Kind -->|Ask user| User["Persist decision request and wait"]
    User -->|Authorized response or deadline| Observe
    Kind -->|Finish| Verify{"Acceptance evidence sufficient?"}
    Verify -->|Yes| Success["Persist success with validation evidence"]
    Verify -->|No| Progress
    Kind -->|Gather, tool or delegate| Gate{"Action, egress and preconditions permitted?"}
    Gate -->|No| Rejection["Record denial or request authorized decision"]
    Rejection --> Progress
    Gate -->|Yes| Intent["Persist action identity, grant reference and input versions"]
    Intent --> Execute["Dispatch after execution-boundary revalidation"]
    Execute --> Result{"Outcome known?"}
    Result -->|No| Reconcile["Reconcile; do not blindly repeat an effect"]
    Reconcile -->|Resolved with recorded evidence| Progress
    Reconcile -->|Still unknown| Unknown["Remain reconciling or report terminal failure with uncertainty; no dependent dispatch"]
    Result -->|Yes| Record["Record result and graph update consistently"]
    Record --> Progress{"Progress and retry budgets permit continuation?"}
    Progress -->|Yes| Observe
    Progress -->|No| Stop
    Kind -->|Wait| Wait["Persist wake condition and deadline"]
    Wait -->|Event or deadline| Observe
```

An unavailable, timed-out, or invalid model response follows the bounded
retry/fallback policy; it cannot authorize a remote fallback on its own.

Not every step needs model inference. Deterministic events such as cancellation,
expired authorization, and a completed process update state without model approval.
No model call belongs inside a storage transaction or a lock needed for control.

The detailed loop design must address:

- Task revisions, concurrent user steering, stale proposals and late results.
- Agent ownership/leases and fencing so an old owner cannot continue dispatching.
- Step, time, cost, context and tool-call budgets, plus repeated-action and
  no-progress detection. Model-reported confidence alone is not a routing policy.
- Separating permission to contact a remote model from permission to execute its
  suggestions, and bounding any recursively delegated work.
- Behavior when the on-device model is unavailable, including status/cancel
  availability and whether a task waits, fails, or uses an explicitly enabled fallback.
- Completion criteria checked against evidence; a prose success claim is insufficient.
- Recovery after dispatch with no recorded outcome. Exactly-once external effects
  are not assumed: classify effects as safely retryable, reconcilable, or unknown.

### Proposed action durability and uncertain outcomes

Proposal for D3-D4. The storage participant represents the selected durability
contract, not a chosen database. The graph is either committed with the result
or updated through a recoverable projection; that choice remains open.

```mermaid
sequenceDiagram
    autonumber
    participant A as Agent core
    participant P as Capability policy
    participant S as Durable action ledger
    participant H as Host execution
    participant T as Tool or external operation
    A->>P: Validate action, egress, task revision and grant scope
    P-->>A: Denial or bounded authorization
    alt Denied
        A->>S: Record rejected proposal
    else Authorized
        A->>S: Persist action ID, input versions and grant reference
        S-->>A: Durable intent acknowledgement
        A->>H: Dispatch action ID and preconditions
        H->>P: Revalidate current grant, lease and limits
        alt Revoked, expired or stale
            H-->>A: Rejected before effect
            A->>S: Persist rejection
        else Execution allowed
            H->>T: Execute bounded operation
            alt Result available
                T-->>H: Actual result and artifacts
                H-->>A: Outcome for action ID
                A->>S: Commit outcome and graph update or projection checkpoint
                S-->>A: Durable result acknowledgement
            else Crash or connection loss after dispatch
                Note over S,T: Persisted intent does not prove whether the effect occurred
                A->>S: On recovery, find action with unknown outcome
                A->>H: Reconcile original action ID and external evidence
                H-->>A: Completed, proven not executed, or still unknown
                A->>S: Persist reconciliation evidence
                Note over A,H: Retry only under the designed effect semantics and renewed authorization
            end
        end
    end
```

## Context graph purpose

The graph represents the evidence and relationships needed to choose and justify
the next action. It must support focused context assembly, dependency tracking,
staleness detection, and inspection. It is distinct from a chat transcript, the
durable action/event ledger, and OpenTelemetry trace context. Decide their links
and transaction boundaries without creating competing task-state authorities.

Start with typed relationships and concrete queries. A graph data model does not
require a graph database, embeddings, or a general autonomous memory system.
Choose storage and indexes from workload evidence.

Embedded SurrealDB is a candidate for evaluation; see the
[context storage assessment](context-storage-candidates.md). Keep graph semantics
owned by the context subsystem and database-specific persistence in the storage
adapter. Database selection must not dictate the agent loop or expose raw query
access to models and interface clients.

Candidate node types:

- Task goals, constraints, acceptance criteria, and user decisions.
- Versioned source artifacts: files, code locations, test/build results, tool outputs.
- Observations, hypotheses, model decisions, action proposals, and action outcomes.
- Context views, derived summaries, delegated requests, and validation evidence.

Candidate edge types include derived-from, supports, contradicts, depends-on,
supersedes, produced-by, and selected-for. Provenance/dependency relationships
must preserve causality; other relationships may be cyclic. D4 must select the
actual types, cardinalities, direction and cycle rules rather than treating the
entire graph as a DAG by assumption.

### Proposed logical graph schema

Proposal for D4. Crow's-foot cardinalities describe logical relationships; fields
are illustrative contract candidates, not a physical database schema. Node
versions are immutable evidence; task state and current grants remain authoritative
outside this graph. A context view contains explicit selection records so order
and transformations can be inspected.

```mermaid
erDiagram
    TASK_REF ||--o{ NODE_VERSION : scopes
    TASK_REF ||--o{ CONTEXT_VIEW : requests
    NODE ||--|{ NODE_VERSION : versions
    NODE_VERSION ||--o{ EDGE : source
    NODE_VERSION ||--o{ EDGE : target
    NODE_VERSION ||--o{ SELECTION : selected_by
    CONTEXT_VIEW ||--o{ SELECTION : contains
    ACTION_REF o|--o{ NODE_VERSION : produces
    TASK_REF {
        string task_id PK
        string workspace_id
    }
    NODE {
        string node_id PK
        string node_kind
    }
    NODE_VERSION {
        string node_version_id PK
        string node_id FK
        string task_id FK
        string content_identity
        string source_locator
        string trust_origin
        string sensitivity
    }
    EDGE {
        string edge_id PK
        string source_version_id FK
        string target_version_id FK
        string relationship_kind
        string provenance_ref
    }
    CONTEXT_VIEW {
        string view_id PK
        string task_id FK
        string graph_revision
        string selection_policy_version
        string destination
        int token_budget
    }
    SELECTION {
        string selection_id PK
        string view_id FK
        string node_version_id FK
        int ordinal
        string source_range
        string transformation_ref
    }
    ACTION_REF {
        string action_id PK
        string ledger_locator
    }
```

This initial view scopes each evidence version to a task. Cross-task reuse,
permission inheritance, payload storage and deletion need explicit D4 decisions;
they must not be inferred from shared identifiers. The optional producing action
allows user-provided or externally captured evidence without inventing an action.

### Example provenance and invalidation relationships

Illustrative graph instance. Arrows follow the named relationship from subject to
object; `derived-from` therefore points toward the source, while invalidation
walks its reverse. Each item refers to a version, not just a filesystem path.

```mermaid
flowchart LR
    File1["Source file version 1"]
    File2["Source file version 2"] -->|supersedes| File1
    Summary["Summary version 1"] -->|derived-from| File1
    View["Context view 17"] -->|depends-on| Summary
    Proposal["Patch proposal 9"] -->|derived-from| View
    Test["Test observation 23"] -->|produced-by| Action["Test action 22"]
    Test -->|supports| Claim["Validation claim for version 1"]
    Claim -->|depends-on| File1
    File2 -.->|Triggers reverse-dependency invalidation| Stale["Summary, view, proposal and claim require revalidation"]
```

## Required graph semantics

1. **Identity and provenance:** stable IDs; source locator plus content/version
   identity; producing task/action/model where applicable; capture time and trust
   origin. Separate observations from inference and verified claims.
2. **Freshness:** define file/workspace revision tracking, derived-node invalidation,
   deletion, competing observations, and checks immediately before a side effect.
   Path equality alone is not content identity.
3. **Access:** scope retrieval by workspace, task and principal. Propagate
   sensitivity through derivation and summaries. A permission reference in the
   graph is evidence, never the authoritative current grant.
4. **Consistency:** define concurrent updates, snapshot versions and action-ledger
   linkage. Choose transactional updates or rebuildable projections with explicit
   checkpoints and lag; never silently combine incompatible revisions.
5. **Retrieval:** specify candidate discovery, relevance ranking, dependency closure,
   deduplication, diversity and deterministic tie-breaking. Evaluate a lexical and
   structural baseline before optional semantic retrieval.
6. **Context assembly:** reserve budgets for instructions, tool schemas and output;
   select source excerpts by budget and relevance; record what was omitted.
   Never assume one fixed context size across all models.
7. **Summarization:** retain source links, uncertainty and supersession semantics;
   do not promote lossy summaries to authority or erase unresolved contradictions.
8. **Lifetime:** define retention, task isolation, optional cross-task reuse,
   garbage collection, quotas, encryption needs and user deletion semantics,
   including indexes, summaries and retained artifacts.
9. **Egress:** build a separately authorized remote context view, record its
   manifest and destination, and enforce limits before transmission. A model's
   request for more context cannot override the policy.

Each assembled context view should identify the selected node versions, source
ranges, transformations, order, selection policy version, budget accounting,
destination, and omissions. Keep sensitive payloads out of telemetry. The design
must settle how to retain enough evidence to inspect a decision while respecting
deletion and minimization requirements; hashes alone cannot reconstruct deleted data.

### Proposed context assembly and remote disclosure

Proposal for D4-D5. Arrows show bounded data transformations and explicit branch
conditions. Insufficient context is an observable result, not permission to exceed
budgets or include inaccessible evidence.

```mermaid
flowchart TD
    Input["Task revision, principal, destination and model limits"] --> Scope["Filter authorized workspace/task evidence"]
    Scope --> Snapshot["Select consistent graph revision and fresh source versions"]
    Snapshot --> Retrieve["Retrieve candidates with bounded dependency expansion"]
    Retrieve --> Rank["Rank, deduplicate and preserve relevant contradictions"]
    Rank --> Budget["Reserve instructions, schemas and output allowance"]
    Budget --> Fit{"Required evidence fits?"}
    Fit -->|No| Transform["Select excerpts or provenance-preserving summaries"]
    Transform --> Enough{"Sufficient within fixed budget?"}
    Enough -->|No| Gap["Report context gap; request narrower scope or more evidence"]
    Enough -->|Yes| Manifest
    Fit -->|Yes| Manifest["Record ordered selections, versions, transformations and omissions"]
    Manifest --> Destination{"Remote destination?"}
    Destination -->|No| Local["Supply view to on-device model"]
    Destination -->|Yes| Egress{"Current egress policy permits exact view and destination?"}
    Egress -->|No| Denied["Record denial; no transmission"]
    Egress -->|Yes| Remote["Send bounded request with manifest reference"]
    Local --> Proposal["Decision references task and context revisions"]
    Remote --> Proposal
    Proposal --> Fresh{"Sources and authorization still valid before action?"}
    Fresh -->|Yes| Validate["Continue action validation"]
    Fresh -->|No| Rebuild["Reject stale proposal and re-enter bounded agent loop"]
```

## Model decision contracts

Define separate request/result schemas and evaluations for task classification,
context selection assistance, tool selection, delegation, and progress assessment.
Each result includes the proposed action, referenced evidence, and a concise
user-facing rationale. Do not require hidden reasoning transcripts.

Tool metadata has one canonical registry: stable identity/version, input/output
schema, capabilities, side effects, retry class, limits and provenance. Produce
model-facing descriptions and UI views from that registry. Model-guided selection
can narrow the eligible set but cannot enlarge authorized capabilities.

Evaluate decisions against labelled representative tasks and failure cases.
Measure routing quality, inappropriate delegation, missed delegation, unnecessary
context, invalid tool selections, and latency/cost tradeoffs. Calibrate any
confidence or uncertainty threshold on evidence rather than trusting a generated score.

## Validation required for the detailed designs

| Concern | Unit | Integration | End-to-end |
| --- | --- | --- | --- |
| Loop | Transition/invariant and budget tests with generated event sequences | Durable transitions, late results, crashes at dispatch boundaries | Coding workflow with restart, user steering and cancellation |
| Graph | Retrieval/invalidation/authorization properties and budget limits | Concurrent storage, projection recovery, migrations and deletion | Changed source invalidates a proposal; retrieved evidence explains the final result |
| Decisions | Schema and deterministic policy tests with controlled outputs | Real Swift model adapter with cancellation and unavailable-model cases | Actual on-device decisions in local and remote-assisted workflows |
| Tools | Registry contracts and capability decisions | Actual constrained processes and denied resource access | Malicious content cannot induce unauthorized execution or egress |

Also define model evaluation datasets, protocol fuzzing, multi-client race tests,
context poisoning cases, token-boundary cases, graph growth/retrieval benchmarks,
and control latency during model load. Each test needs an observable oracle;
no production safety claim is established by fixtures alone.

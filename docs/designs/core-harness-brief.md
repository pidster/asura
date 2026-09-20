# Core harness: design brief

Status: proposed semantics and open questions. This is an input to D3-D4 in the
[design plan](../plans/architecture-and-design.md), not an implementation-ready
specification. Detailed algorithms, schemas, thresholds, and test cases remain
to be resolved there.

Selected constraints within this proposal are recorded in the
[decision index](../decisions/README.md). They bind later designs but do not
authorize implementation.

## Ownership and execution model

Propose one durable orchestration state machine coordinating bounded agent steps.
The orchestrator owns scheduling, task/agent lifecycle and aggregate budget
accounting; the agent core owns
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
    direction LR
    [*] --> Queued: Authorized acceptance persisted
    Queued --> Running: Valid assignment and grant
    Running --> WaitingUser: Decision required
    WaitingUser --> Running: Timely authorized response
    Running --> WaitingExternal: Bounded dispatch
    WaitingExternal --> Running: Outcome reconciled
    Running --> Succeeded: Acceptance criteria verified
    Running --> FailurePending: Fatal error or exhausted task budget
    WaitingUser --> FailurePending: Decision or task deadline expires
    WaitingExternal --> FailurePending: Fatal error or task deadline expires
    Queued --> FailurePending: Task deadline expires
    FailurePending --> Failed: Effects accounted for and no cancel intent
    FailurePending --> Reconciling: Unresolved effects
    Succeeded --> [*]
    Failed --> [*]
```

### Proposed interruption and recovery states

Companion proposal for D3. `ActiveWork` is a diagram entry representing the
current running or waiting state, not a new stored task state. Queued and paused
tasks may also be cancelled. Pending control intent survives reconciliation.
The orchestrator accepts and durably records new pause/cancel intent while
reconciliation continues, acknowledging acceptance only after persistence. Cancel
supersedes pause and any pending failure outcome; pause cannot clear cancellation
or a pending failure. Updating intent does not imply an outstanding effect stopped.
No new task work is dispatched while reconciling. Reconciliation observations and
bounded cleanup retain their own authorization and cannot restart task work.

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
    Cancelled --> [*]
```

### Reconciliation control and exit

Companion proposal for D3. Arrows name durable intent updates or guarded exits
from reconciliation. `ActiveWork` again denotes running or waiting work. The
terminal failure contract below distinguishes accounted failure from irrecoverable
uncertainty; neither exit permits new task dispatch.

```mermaid
flowchart TD
    Control["New authorized pause or cancel"] --> Persist["Persist intent with cancel precedence, retain reconciling state"]
    Persist --> Reconciling["Reconciling: no new task dispatch"]
    Reconciling -->|Effects accounted for| Intent{"Pending intent in current revision?"}
    Reconciling -->|Recovery impossible| Uncertain["Failed with uncertainty and recorded control intent"]
    Intent -->|Cancel| Cancelled["Cancelled"]
    Intent -->|Failure without cancel| Failed["Failed with accounted effects"]
    Intent -->|Pause only| Paused["Paused"]
    Intent -->|None| ActiveWork["Running or waiting work"]
```

On startup, persisted nonterminal work with potentially outstanding effects enters
reconciliation before redispatch. Cancellation cannot undo an already-completed
effect; its terminal report must identify residual effects. Terminal states are
not reopened by late results, which are retained as evidence for investigation.
Serialize intent updates, recovery completion and dispatch eligibility against the
same authoritative task revision. A cancellation accepted during reconciliation
must prevent the recovery path from resuming or dispatching task work, including
after restart. With accounted effects, pending failure and no cancellation,
persist `Failed`. If recovery fails with unknown effects, persist `Failed` with
their uncertainty and any cancellation intent in the terminal report rather than
claiming they stopped.

Throughout these lifecycle views, **accounted** means operation effects are known
and each usage obligation is either settled from evidence or conservatively held
by an explicit durable reservation. Exact provider billing need not delay a
terminal outcome once effects are known. Such an outcome must disclose unresolved
usage and retained allowance. Unknown operation effects still require bounded
reconciliation and cannot produce a successful cancellation claim.

### Common failure and deadline contract

Selected constraint for D3-D4; rationale in
[ADR-0002](../decisions/0002-failure-settlement.md). `FailurePending` is the
logical combination of failure intent and stopped dispatch, not a commitment to
a separate stored enum. Fatal errors, exhausted task budgets, task deadline expiry
and required user-decision expiry all enter this procedure from **every
nonterminal state**, including queued, paused, waiting and reconciling states.
Recoverable per-operation failures may retry only while this procedure has not
started and current authorization and budgets permit it.

1. Serialize the trigger against the authoritative task revision. Persist its
   cause and pending failure intent, fence further task dispatch, and invalidate
   planning/model generations. A previously accepted cancel retains precedence.
2. Request bounded stopping of outstanding work and reconcile every admitted
   operation, including model calls and delegated descendants. Admission records
   must distinguish proven non-dispatch from an unknown outcome. Do not interpret
   timeout, process death or a disconnected provider as proof of no effect or cost.
3. Once effects are accounted for, persist `Failed`, or `Cancelled` if cancellation
   won. If reconciliation cannot establish the outcome within its designed bound,
   persist `Failed` with explicit uncertain effects, unresolved usage reservations
   and recorded control intent. Never claim cancellation completed in that case.

Failure intent forbids resume and fresh task/model work. Reconciliation and cleanup
use separately authorized, bounded recovery resources, reserved outside the task's
spendable allowance; they cannot borrow permission for new task work. D3/D4 must
specify recovery bounds, allowed operations and exhaustion reporting before code.
An authority-store outage cannot produce a false durable acknowledgement: locally
stop new dispatch and use only existing bounded stop/cleanup authority until the
selected persistence contract can record the transition. Remote owners remain
subject to their lease and host limits. D3/D5 must define that outage protocol.

The task deadline covers queueing, waiting, pause and reconciliation; pausing does
not extend it. Deadline expiry during reconciliation records failure intent but
does not skip settlement or reset the recovery bound. Terminalization, failure,
cancel and dispatch admission are serialized: if success was already durably
committed, a later timer cannot reopen it. Success requires no outstanding task
operations and reconciled budget accounting. D3 defines clock representation,
restart/clock-jump handling and the atomic mechanism. Clock uncertainty must not
extend work authority. Late observations may append evidence and settle usage
without reopening terminal state or triggering new work.

#### Failure settlement flow

Selected logical ordering. Edges are triggers or guarded outcomes; persistence
technology and numerical recovery limits remain D3/D4 design work.

```mermaid
flowchart TD
    Trigger["Fatal error, exhausted budget, task or decision expiry"] --> Fence["Serialize failure intent and dispatch fence against task revision"]
    Fence --> Durable{"Authority store acknowledged?"}
    Durable -->|No| Outage["Stop local dispatch, no durable acknowledgement, bounded outage recovery"]
    Outage -->|Authority restored| Fence
    Durable -->|Yes| Stop["Stop outstanding operations and descendants under recovery authority"]
    Stop --> Known{"Effects and usage accounted for?"}
    Known -->|Effects known and usage settled or reserved| Intent{"Cancel intent accepted?"}
    Intent -->|Yes| Cancelled["Persist Cancelled with residual effects"]
    Intent -->|No| Failed["Persist Failed with cause and effects"]
    Known -->|No| Recover["Reconciling with bounded cleanup and retained reservations"]
    Recover -->|New evidence| Known
    Recover -->|Recovery bound exhausted| Unknown["Persist Failed with uncertainty and retained obligations"]
```

### User-decision expiry contract

Resolved brief-level behavior for D3-D4: expiry of a required user decision fails
the task, rather than asking again or returning to model inference. Apply the
[common failure procedure](#common-failure-and-deadline-contract): persist the
expired decision and failure intent, disable further task dispatch, and account
for outstanding effects before reporting failure. With unresolved effects, remain
in reconciliation until they are accounted for or the recovery contract produces
an explicit terminal failure with uncertainty. A subsequently accepted cancellation
supersedes the pending failure outcome as described above.

The orchestrator serializes response acceptance, deadline expiry and control intent
against the pending decision/task revision. Accept a response only while that
decision is pending and before its authoritative deadline; late or duplicate
responses cannot resume work. An already accepted cancellation cannot be replaced
by timeout failure. D3 must specify the authoritative clock, restart handling and
atomic update mechanism. A timer wakeup alone is not permission to infer or dispatch.

## Proposed agent step

| Step | Responsibility and resulting evidence |
| --- | --- |
| 1. Observe | Accept user input and completed action observations; check cancellation, grants, task revision and budgets |
| 2. Assemble | Select a versioned context view with provenance, freshness and bounded size; include current task constraints |
| 3. Decide | Ask the on-device subsystem for a typed proposal: gather context, perform a tool action, delegate, ask the user, wait, or finish |
| 4. Validate | Validate schema, task relevance, context revision, capabilities, data egress, resource limits and preconditions; reject or request clarification |
| 5. Admit and record intent | Atomically reserve aggregate budgets and durably bind action identity, task revision, selected inputs and grant scope before dispatch |
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
    Budget -->|No| Stop["Enter common failure procedure and stop task dispatch"]
    Stop --> FailureRecovery["Settle effects and usage, then report accounted or uncertain terminal outcome"]
    Budget -->|Yes| Context["Assemble scoped versioned context"]
    Context --> Decision["Request bounded on-device decision"]
    Decision --> Valid{"Schema, evidence and task revision valid?"}
    Valid -->|No| Retry{"Retry or refresh within budget?"}
    Retry -->|Yes| Observe
    Retry -->|No| Stop
    Valid -->|Yes| Kind{"Proposed next step"}
    Kind -->|Ask user| User["Persist decision request and wait"]
    User -->|Authorized response accepted before deadline| Observe
    User -->|Decision expired| Stop
    Kind -->|Finish| Verify{"Acceptance evidence sufficient?"}
    Verify -->|Yes| Success["Persist success with validation evidence"]
    Verify -->|No| Progress
    Kind -->|Gather, tool or delegate| Action["Action lifecycle below: authorize, reserve, execute and reconcile"]
    Action -->|Accounted result or definitive admission rejection| Progress{"Progress and retry budgets permit continuation?"}
    Action -->|Unknown admission or effect| Unknown["Reconcile original ID without fresh dispatch"]
    Unknown -->|Accounted| Progress
    Unknown -->|Recovery bound exhausted| Stop
    Progress -->|Yes| Observe
    Progress -->|No| Stop
    Kind -->|Wait| Wait["Persist wake condition and deadline"]
    Wait -->|Event or deadline| Observe
```

The on-device decision request itself also passes budget admission and the action
lifecycle; the diagram delegates those details to the action sequence below. Admission
rejections must yield a bounded wait, replan or failure decision; returning to
Observe is not permission for a busy retry loop. Task deadline and common failure
checks apply while waiting and before success.

An unavailable, timed-out, or invalid model response follows the bounded
retry/fallback policy; it cannot authorize a remote fallback on its own.

Not every step needs model inference. Deterministic events such as cancellation,
expired authorization, and a completed process update state without model approval.
No model call belongs inside a storage transaction or a lock needed for control.

The detailed loop design must address:

- Task revisions, concurrent user steering, stale proposals and late results.
- Agent ownership/leases and fencing so an old owner cannot continue dispatching.
- The aggregate budget contract below, plus repeated-action and
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
    participant O as Orchestrator
    participant P as Capability policy
    participant S as Durable action ledger
    participant H as Host execution
    participant T as Tool or external operation
    A->>P: Validate action, egress, task revision and grant scope
    P-->>A: Denial or bounded authorization
    alt Denied
        A->>S: Record rejected proposal
    else Authorized
        A->>O: Admit action ID, task revision, input versions, grant and upper bounds
        O->>S: Atomically reserve ancestor budgets and persist action intent
        S-->>O: Durable admission or rejection
        O-->>A: Admission reference or no-dispatch result
        alt Rejected or ambiguous admission
            Note over A,O: No dispatch, reconcile ambiguous admission by original ID before retry
        else Durable admission
        A->>H: Dispatch action ID, reservation and preconditions
        H->>P: Revalidate current grant, lease, reservation fence and limits
        alt Revoked, expired or stale
            H-->>A: Rejected before effect
            A->>O: Record proven non-execution and release unused reservation
        else Execution allowed
            H->>T: Execute bounded operation
            alt Result available
                T-->>H: Actual result and artifacts
                H-->>A: Outcome for action ID
                A->>S: Commit outcome and graph update or projection checkpoint
                S-->>A: Durable result acknowledgement
                A->>O: Settle reservation from durable usage evidence exactly once
            else Crash or connection loss after dispatch
                Note over S,T: Persisted intent does not prove whether the effect occurred
                A->>S: On recovery, find action with unknown outcome
                A->>H: Reconcile original action ID and external evidence
                H-->>A: Completed, proven not executed, or still unknown
                A->>S: Persist reconciliation evidence
                A->>O: Settle proven usage or retain uncertain reservation
                Note over A,H: Retry only under the designed effect semantics and renewed authorization
            end
        end
        end
    end
```

### Aggregate budget ownership and admission

Selected constraints for D3-D5, with alternatives and consequences in
[ADR-0003](../decisions/0003-aggregate-budget-admission.md). The orchestrator owns
one durable budget authority. Agent core proposes bounds and consumes admission
results; model/provider adapters report usage; hosts enforce operation limits.
Neither a context graph value nor telemetry is spend authority.

| Dimension | Accounting rule |
| --- | --- |
| Consumable steps, tool calls, model tokens and monetary cost | Typed units, cumulative charged usage plus outstanding reservations may not exceed the configured cap at admission. Money records currency and pricing revision; unlike units are never added together. |
| Concurrent operations or resource occupancy | Reserve capacity while occupied; release only on evidence of completion/non-execution. A lost connection does not free the slot. |
| Context capacity | Check the entire effective context per invocation, including retained history, with its model-specific limit. This is distinct from cumulative inference-token allowance. |
| Elapsed task time | Check the authoritative task deadline; it is not replenished by retries, restart, waiting or pausing. |

Budgets form a hierarchy: configured installation/workspace scopes where applicable,
root task, child tasks and operations. Every operation is charged once against
every applicable ancestor cap. A child cap can only narrow its inherited allowance.
Creating children cannot multiply the root budget. The atomic admission decision
checks all applicable dimensions and ancestors, current task revision and dispatch
eligibility, then durably binds an operation ID to its reservation and action intent.
Competing admissions cannot both consume the same remaining allowance. Duplicate
admission requests return the same reservation/outcome, never another allocation.

Each model call, retry, framework tool callback and delegated operation requires
admission. Reserve a defensible upper bound on consumption before dispatch and
enforce it at the execution/provider boundary. An operation without an enforceable
bound cannot be admitted under a hard cap. Estimates alone do not establish hard
cost guarantees. A provider-specific design must address billable input, maximum
output, pricing and retries before claiming bounded monetary cost.

Settle actual usage exactly once using durable operation evidence; release only
the proven unused portion. Proven non-execution may release the full reservation.
Unknown execution or billing retains the conservative reservation through timeout,
cancel, terminal failure and restart. An expired reservation lease prevents new
dispatch but does not prove old work stopped or refund consumption. Late usage
evidence may settle it without reviving the task. If observed usage exceeds its
bound, record all usage and the breach, stop further affected admissions, and enter
failure handling; never truncate accounting to preserve the nominal invariant.

For remote delegation, the root reserves a bounded child allocation before handoff;
the child subdivides that allocation under a fenced owner epoch. Parent accounting
counts the allocated envelope once, while child accounting counts its operations
within it. Parent cannot spend or reassign the outstanding envelope during a
partition. Release requires evidence that the old owner cannot continue consuming
it and final usage is accounted for. Lost authority permits only already admitted
bounded work, never new unallocated spending. D5 must specify the delegation
protocol, expiry enforcement, epoch fencing and settlement evidence.

D3/D4 must select reservation schemas, transaction/serialization primitives,
durability location, configuration scopes, numeric precision/overflow behavior and
recovery checkpoints. No network/model work runs inside the admission transaction.
Admission unavailable or ambiguous means no fresh dispatch until the original
operation ID is reconciled. Retrying a provider request is a new attempt with its
own reservation unless the provider contract proves it is the same operation.

#### Reservation lifecycle

Selected logical states; arrows name evidence or admission outcomes. Unknown
usage remains committed against the cap, including after a task becomes terminal.

```mermaid
stateDiagram-v2
    direction LR
    [*] --> Requested
    Requested --> Rejected: Invalid authority or insufficient allowance
    Requested --> Reserved: Atomic durable intent and ancestor reservation
    Reserved --> InFlight: Current dispatch fence accepted
    Reserved --> Settled: Proven non-dispatch releases allowance
    InFlight --> Settled: Durable actual usage and unused portion recorded
    Reserved --> Uncertain: Crash with dispatch status unknown
    InFlight --> Uncertain: Effect or billing outcome unknown
    note right of Uncertain: Timeout and task termination retain allowance
    Uncertain --> Settled: Authoritative non-execution or usage evidence
    Rejected --> [*]
    Settled --> [*]
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

SurrealDB support must allow optional embedded operation or a configured external
connection; see the [context storage assessment](context-storage-candidates.md).
Embedded is the initialization default when no external connection is configured;
a configured external connection selects external initialization. On reopen,
verify the persisted binding before opening either mode; missing configuration
cannot silently select a different graph. The storage brief owns binding and
migration semantics, including external failure behavior.
Concrete engine, SDK/server versions and failure contracts remain under evaluation.
Keep graph semantics owned by the context subsystem and database-specific
persistence in the storage adapter. Database selection must not dictate the agent
loop or expose raw query access to models and interface clients.

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
    Destination -->|No| Local["Model session admission with full effective context"]
    Destination -->|Yes| Egress{"Current egress policy permits exact view and destination?"}
    Egress -->|No| Denied["Record denial; no transmission"]
    Egress -->|Yes| Remote["Model session admission with full effective context"]
    Local --> Proposal["Decision references task and context revisions"]
    Remote --> Proposal
    Proposal --> Fresh{"Sources and authorization still valid before action?"}
    Fresh -->|Yes| Validate["Continue action validation"]
    Fresh -->|No| Rebuild["Reject stale proposal and re-enter bounded agent loop"]
```

The local and remote admission nodes use the session contract below. They cannot
dispatch inference with unaccounted history. For a retained remote conversation,
the exact view checked for egress includes the retained inputs as well as new
content; admission must reject any mismatch and rebuild/re-authorize the view.

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

### Model session ownership and effective context

Required contract for D4-D5. Select either stateless requests built from each
authorized context view or explicitly managed stateful sessions. Do not infer
permission to reuse a session from its presence in the Foundation Models API or
a provider adapter. The requirements apply to transcripts, retained tool results,
summaries, application-managed prompt caches and remote conversation references
that can affect a later model response.

- The orchestrator supplies task/principal/workspace identity, current authority
  scope and a session generation tied to the task revision. The model adapter
  owns session resources; the context subsystem owns the effective input manifest.
  The adapter must account for every retained input before inference. Session
  identity or a cache hit cannot act as a grant or a second context authority.
- Never reuse conversation state across tasks or principals. Any authorized
  cross-task evidence reuse must re-enter through the context subsystem and a new
  session. Within a task, account for retained history, instructions, tool schemas,
  tool results and new selections in provenance, sensitivity and total input/output
  budget calculations. The recorded manifest must describe the effective input,
  not just the latest appended message. No hidden reasoning transcript is required.
- Revalidate retained inputs against current authority, deletion and source
  invalidation before reuse. If the adapter cannot enumerate and bound their
  contribution, retire the session and rebuild from authorized evidence. If it
  cannot establish an isolated fresh session, report a bounded failure without
  inference or remote disclosure.
- Revocation, relevant deletion, cancellation, task completion or a scope change
  invalidates affected session generations. The adapter stops reuse and attempts
  cancellation/cleanup under the designed lifecycle. Reject late results and tool
  callbacks from invalidated generations. Continuing permitted work requires a
  fresh generation and a rebuilt, authorized manifest; cancellation never restarts
  work. Provider-side deletion guarantees and cleanup failures must be explicit,
  not inferred from retiring a local reference.

Proposed admission flow. Arrows identify checks before either local or remote
inference. Admission and result acceptance use current authority and generation
fencing; D4 must specify their concurrency protocol rather than assuming that a
check alone prevents invalidation races.

```mermaid
flowchart TD
    Request["Task identity, authority and candidate context view"] --> Mode{"Retain session state?"}
    Mode -->|No| Fresh["Create isolated session from authorized view"]
    Mode -->|Yes| Inspect["Inspect identity, generation and retained input inventory"]
    Inspect --> Valid{"All retained inputs current and accounted for?"}
    Valid -->|No| Retire["Retire generation and rebuild authorized view"]
    Retire --> Fresh
    Valid -->|Yes| Manifest["Bind full effective manifest and budget"]
    Fresh -->|Isolation established| Manifest
    Fresh -->|Unavailable| Fail["Bounded failure with no inference"]
    Manifest --> Admit{"Current authority, budget and destination permit?"}
    Admit -->|No| Reject["Reject without inference or disclosure"]
    Admit -->|Yes| Infer["Dispatch inference bound to manifest and generation"]
    Infer --> Current{"Generation and task still current at result acceptance?"}
    Current -->|No| Discard["Reject stale result or callback"]
    Current -->|Yes| Result["Return proposal with full context provenance"]
```

## Validation required for the detailed designs

| Concern | Unit | Integration | End-to-end |
| --- | --- | --- | --- |
| Loop | Transition/invariant and budget tests with generated event sequences | Durable transitions, late results, crashes at dispatch boundaries | Coding workflow with restart, user steering and cancellation |
| Graph | Retrieval/invalidation/authorization properties and budget limits | Concurrent storage, projection recovery, migrations and deletion | Changed source invalidates a proposal; retrieved evidence explains the final result |
| Decisions | Schema and deterministic policy tests with controlled outputs | Real Swift model adapter with cancellation and unavailable-model cases | Actual on-device decisions in local and remote-assisted workflows |
| Tools | Registry contracts and capability decisions | Actual constrained processes and denied resource access | Malicious content cannot induce unauthorized execution or egress |

The following regression scenarios are required design acceptance cases, not
implemented tests. D3-D5 must assign concrete fixtures, commands and environments
before the corresponding implementation packets become ready.

| Case | Unit | Integration | End-to-end |
| --- | --- | --- | --- |
| Common failure settlement | Generate fatal/deadline/budget triggers in every nonterminal state and success/cancel orderings; assert dispatch fencing, pause does not extend deadlines, and bounded recovery is separate from task work | Crash at intent, stopping and terminal commits; expire deadline during external work, inject authority-store outage and unknown effects/usage; recover without dispatch or budget reset | Expire a CLI task during an actual operation, restart, and show accounted failure or explicit uncertainty with its original cause and residual effects |
| Aggregate budgets | Concurrent child and parent admissions share ancestor caps; duplicate operation IDs and settlement cannot allocate/refund twice; exercise typed units, overflow and bound violations | Real persistence races at the last allowance, crash before/after admission and settlement, unknown provider usage, partitioned child envelope and stale epochs | Parallel delegated work reaches a shared cap without overspend; cancel/restart retains unknown usage, late evidence settles it without reopening task; I5/I7 add actual provider and two-host evidence |
| Control during reconciliation | Generate pause/cancel/recovery orderings and prove cancel precedence with no resume | Persist cancel after entering reconciliation, restart, then deliver a late outcome and assert no new task dispatch | Disconnect an executing host, enter reconciliation, cancel from another client, reconnect and report accounted or explicitly uncertain effects |
| User-decision expiry | Race response, timeout and cancellation against one revision and assert a single winner with no inference after expiry | Restart with an expired pending decision and unresolved action, reject late/duplicate responses and preserve failure/cancel intent | Let a required decision expire through CLI/TUI, observe failure or reconciliation and no re-prompt or new task work |
| Model session isolation | Reject identity/generation mismatch and account for full retained-input budgets and provenance | Exercise the real adapter with task A then B, revocation/deletion during inference, and stale tool callbacks; prove retirement or isolated reconstruction | Run tasks with distinct access scopes and revoke access within a task; verify excluded evidence never enters a later effective input, and invalidated outputs are not accepted |

Session tests must inspect admitted effective inputs, manifests and generation
acceptance at the adapter boundary. Absence of a secret in one stochastic model
answer alone does not prove isolation. Exercise both stateless and retained-state
paths if both are shipped, including remote conversations when I5 is delivered.

Also define model evaluation datasets, protocol fuzzing, multi-client race tests,
context poisoning cases, token-boundary cases, graph growth/retrieval benchmarks,
and control latency during model load. Each test needs an observable oracle;
no production safety claim is established by fixtures alone.

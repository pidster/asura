# First conversation: durable admission and recovery

Response-budget update, 2026-09-28: the [response activity contract](response-activity.md#response-limits-and-truthful-completion)
supersedes this packet's earlier 256/128/128 allocations. Every admission
reserves exactly 2,048 output tokens; replay rejects any other reservation
(owner, 2026-10-17). Tool requests are capped at 1,024 tokens first
and 512 thereafter, at most 64 per turn, under the
[turn budget](turn-usage-helper.md#turn-budget-and-request-admission). Earlier
validation evidence below describes the earlier packet.

Status: CA-A through CA-D implemented. The storage validation subset below
passed on 2026-09-27. The owner authorized conversation execution and the shared
input pipeline. The implementation extends the canonical ordinary-file authority
owner and embedded graph adapter. Journal format remains 1. Fault and durability
qualification remains incomplete where explicitly listed below.
Journal step J1-0 (owner, 2026-10-15) removes the journal append, observation,
replay and queue-worker time bounds. It is implemented; see
[J1-0 implementation](#j1-0-implementation).

The [recovery design](persistence-recovery.md) owns the journal and installation
binding. This amendment defines format 1 and the first conversation writer.
The [model packet](platform-capabilities.md#remaining-authority-prerequisite-and-execution-order)
owns helper execution and conversation transport. The [storage ownership contract](storage-adapters.md)
places persistence in `asura-storage`; platform supplies safe OS primitives.
[ADR-0006](../decisions/0006-async-event-pipelines.md) governs both pipelines.

## Scope and canonical owners

Deliver one active text generation per service and one project per conversation.
Tools were outside this packet; [model tools](model-tool-execution.md) defines
current tool behavior. The initial provider is the admitted `system` model. One submitted
turn creates one task and one model operation. Both have durable identities.
The task has one operation and a 2,048-output-token budget. This packet does not
implement multi-step scheduling, remote providers, conversation branching, history
editing, retention deletion or journal compaction.

Required product direction: conversation models may run locally or remotely.
Classifiers run locally only, including custom classifiers. Classifier failure
must never trigger a remote-classifier fallback. Provider selection and classifier
selection are separate responsibilities. Remote conversation adapters and explicit
classifier stages are not implemented by this first-conversation packet; they
require scoped stage contracts and validation before implementation.

| Owner | Responsibility |
| --- | --- |
| Control codec | Decode version 0.1 frames and validate directions, sizes and identities |
| Shared service pipeline | Normalize commands, route to existing owners, reject unsupported capabilities |
| Orchestrator/admission owner | Project scope, conversation generation, task lifecycle, reservations, start/cancel order and terminal decisions |
| Context owner | Construct bounded history from exact committed turns, validate its generation and input digest |
| Storage authority adapter | Canonical encoding, serialized append, durable flush, replay and request-result lookup |
| Platform | Validated directory/file descriptors, local APFS checks, identity fencing and durability primitive |
| Model owner/helper | Availability, token count, actual generation, cancellation and child settlement |
| Client | Draft, request ID retention, provisional display, reconnect and observed outcomes |

A typed command passes through decode, validate, normalize, route, admit, execute
and event publication. Local presentation commands stop before backend admission.
Helper and OS signals pass through validation and normalization before reaching
the same lifecycle owner. Neither the TUI nor helper writes authority records.

### Pipeline and authority

Selected ownership proposal. Arrows carry typed requests, effects or observations.
The two input paths converge on one serialized admission owner.

```mermaid
flowchart TD
    Client[Client input] --> Decode[Decode and validate]
    Decode --> Normalize[Normalize typed command]
    Normalize --> Route[Route to canonical owner]
    Signals[Helper and OS signals] --> Validate[Validate identity and normalize]
    Validate --> Route
    Route --> Admit[Admit and fence lifecycle]
    Admit --> Journal[Storage journal writer]
    Journal --> Durable[Durable result]
    Durable --> Execute[Model owner executes admitted operation]
    Execute --> Signals
    Durable --> Events[Publish correlated events]
    Events --> Client
```

## Installation and project prerequisites

Use `state/control/slot-0.log` under the validated account root. Directory mode is
0700; journal mode is 0600, current-user owned, regular and singly linked. Reuse
existing descriptor/ACL checks. Keep the service owner lock until all writers and
owned helpers settle. Log files, YAML settings and graph receipts never replace it.

Normal service startup initializes only a verified fresh installation. Initialization
creates the installation and graph identity through PendingInit, verified graph
marker and ActiveBinding. Reuse the [HM0 binding contract](hybrid-memory-ontology.md#files-binding-and-initialization-boundary).
This packet supplies format-1 request outcomes for that sequence; it does not
turn the scratch database adapter into a production initialization bypass.
A missing or mismatched embedded graph remains repair-required.

Before the first conversation, the project must have a committed registration.
The registration owner validates a directory with retained device/inode identity,
records its location and closed visibility, and rechecks identity after commit.
This is an explicit project selection/registration flow. A working directory is
a hint, not implicit registration or authority to use a replacement path.
New registration follows the existing GraphReady prerequisite. A registered text
conversation may use only journal-backed prompt/history when an external graph
is unavailable; it cannot retrieve graph evidence or register a new location.
Missing embedded data or binding mismatch never qualifies for this allowance.

Installation, graph, project, conversation, task, operation and transition IDs use
nonzero 16-byte values generated by the existing platform random-ID owner.
Request IDs come from the authenticated client. They are not paths or PID values.
Conversation generation starts at 1 and advances exactly once per accepted turn.
Only one nonterminal turn is permitted per conversation and per service.
No integer, cursor, revision or generation may wrap.

## Automatic initialization and explicit registration workflow

Normal startup must make a new installation ready without an `/init` command.
The service admission owner waits for initial inspection and the serialized
writer's Open result. Only a successful Open with no replay, verified as RuntimeOnly,
permits one internal Initialize request with a new request ID. The existing writer
rechecks the fresh-state condition and owns all initialization writes. No client
performs initialization policy, and no project is registered automatically.

Required behavior (owner, 2026-10-13, reconciling threat-model TB9): after an
automatic initialization, the client shows a notice once. It states that Asura
created a new installation at `~/.asura` and that any earlier data was not found.
The service cannot tell first use from prior loss, so every fresh initialization
produces the notice. The owner selected a per-run notice on 2026-10-13; nothing
durable records it.

- The control schema adds `optional bool new_installation` as field 8 of
  `InspectInstallationReply` and field 5 of `InspectReply`. Protocol stays 0.1.
- The service sets it to true only after automatic initialization completes in
  the current service run, and only with installation `GraphReady` and reason
  `GraphVerified`. Otherwise the field is absent. Validation rejects `false` and
  any other combination. An explicit `/init` does not set it.
- A service restart clears it, so the notice never returns for that installation.
- The TUI shows the notice once per client session when an inspect reply carries
  the field: "New installation created at ~/.asura. Earlier data, if any, was
  not found." It uses the existing notice line and never clears the draft. The
  next key press dismisses it, so the header returns to service status.

Validation: control unit tests for the field rules; service unit tests that the
flag follows `Complete` only; the service conversation-flow test checks it after
fresh startup and its absence after restart; a TUI unit test checks the notice
appears once.

The root entry `.DS_Store` is Finder metadata, not installation authority. The
platform scanner accepts only a regular file owned by the account, with one
hard link and no group or world write permission. Read permission for other
users is permitted. Existing descriptor, ACL and change validation still apply.
The scanner does not read, remove or modify its contents. It counts the entry
against the existing 64-entry limit but excludes it from installation evidence
and unknown content. Symlinks, directories and unsafe metadata remain rejected.
All other unknown names retain the existing rejection behavior.

Validation requires isolated scanner cases for safe metadata and unsafe types,
permissions and links. Real service and terminal journeys must initialize with
metadata present, reopen the same installation after restart, and complete a
repeated `/init` without replacing data. Preserve metadata bytes throughout.

An existing binding is reopened and verified. Pending initialization retains its
original intent and requires explicit recovery; automatic startup must not create
a replacement intent or graph. Unknown, damaged or missing associated data remains
unavailable. Failed automatic initialization publishes its failure and does not
retry with new IDs. Stop prevents scheduling a new initialization job; an already
submitted job settles under the writer's settlement and shutdown contract; the
writer has no time bound ([J1-0](#journal-step-j1-0-journal-waits-without-a-time-bound)).
Inspection and terminal input remain responsive while initialization runs. Publish
GraphReady only after the committed binding and graph have been verified.

Selected startup flow. Arrows show storage observations and the service's next
action; initialization and verification run on the existing bounded writer.

```mermaid
flowchart TD
    Open["Service owner: inspect and open"] --> State{"Writer result"}
    State -->|"Verified fresh; no replay"| Stop{"Shutdown requested?"}
    Stop -->|Yes| Close["Close without starting setup"]
    Stop -->|No| Init["One internal Initialize request"]
    Init --> Progress["Publish Recovering; keep input responsive"]
    Progress --> Result{"Writer outcome"}
    Result -->|"Binding committed and verified"| Ready["Publish GraphReady"]
    Result -->|"Failure or uncertain outcome"| Failed["Publish unavailable; preserve evidence"]
    State -->|"Existing active binding"| Verify["Verify recorded graph; never replace"]
    Verify -->|Valid| Ready
    Verify -->|"Missing or damaged"| Failed
    State -->|"Existing pending intent"| Pending["Keep original intent; report recovery state"]
    State -->|"Unknown or damaged state"| Repair["Preserve evidence; report repair state"]
```

`/init` and `asura init` remain explicit setup/readiness commands. On an active
binding, their no-change response refers the user to the current installation
status. The normal path is startup, then `/project add PATH`, then a prompt.
The equivalent CLI project command uses the same service operation.
A missing project displays a project-registration instruction and preserves the draft. The TUI
must not invent a project ID or register its launch directory without a command.
For `PATH` equal to `.`, the client resolves its working directory to an absolute
UTF-8 locator; the platform owner verifies the actual directory independently.
Reject non-UTF-8 locations in this slice rather than performing lossy conversion.

Extend version-0.1 control operations through the canonical schema owner:

| Command | Typed inputs | Durable result |
| --- | --- | --- |
| InitializeInstallation | request ID, mode `embedded`, expected authority revision 0 | Installation ID, graph ID, pending/completed phase and authority revision |
| ResolveInitialization | original request ID and request digest | Original pending or committed result; unknown request never creates state |
| ProjectRegister | request ID, absolute location string | Project ID, registry revision 1, canonical location and current/stale association status |
| ProjectList | optional after-project ID, limit 1 through 8 | Ordered page of at most eight project IDs/locations/states and next cursor; complete response at most 40 KiB |

The initialization request digest covers command kind, embedded mode and expected
revision. The service records configuration revision 1 plus its validated digest
for the new installation. Same-ID retries use that original snapshot. A different
ID during PendingInit returns busy; after ActiveBinding it returns already-initialized.
The storage owner returns a distinct `AlreadyInitialized` result for a new request
ID against an active binding. It does not append records or replace the graph.
An ID already used by another command still conflicts. The service exposes
`installation_already_initialized`; clients show this as a successful no-change
setup result with guidance to list or add a project. Definite service rejections
must not be described as unconfirmed outcomes. Validation must cover a fresh-ID
repeat, unchanged journal bytes, a real service reply, and visible TUI feedback.
Root integrates these fields with HM0's initialization/resolve definitions rather
than creating duplicate operations. Unsupported external mode returns unavailable;
it never selects an embedded fallback for existing external settings/bindings.

Initialization runs through one serialized storage worker while holding the existing
service lock. It creates the protected journal directories and writes format-1
PendingInit. Then it creates or verifies only the intended embedded graph marker
using the canonical adapter. Finally it appends format-1 ActiveBinding. These are
three durable boundaries, not one cross-store transaction. Missing acknowledgement
at any boundary resolves from the original request/graph IDs. Existing partial or
unknown state is preserved for repair; no `init --force` exists in this slice.

The production graph connection must use validated account-root capabilities;
it cannot call HM0-A's scratch-only path constructor. Extract its typed query/marker
implementation behind the canonical adapter while retaining scratch-only test
constructors. Qualify no-create reopen, owner retention and identity mismatch before
service initialization is enabled. This is an explicit implementation dependency
within stage CA-B below, not permission to weaken HM0 path checks.

ProjectRegister first checks the initialized binding and GraphReady state, validates
the location through the platform owner, and looks for an existing same-object
association. A repeated original request returns its result. A new request naming
an already registered device/inode returns that project through a durable request
alias record, kind 9 below. It does not create another project. A changed path/object
cannot silently replace an association. Current/stale status is revalidated before
selection and before each admission. Required behavior (owner, 2026-10-14,
[PX3](execution-threat-model.md#person-command-decisions)): registration rejects
a location whose retained directory is the Asura home `~/.asura`, lies inside
it, or contains it, such as the account home. The platform owner compares
directory identities, not path text. Registration identity and location are
immutable; the later [project-name contract](project-names.md) adds a durable
display-name update without changing either. Removal remains outside this slice.

The client retains request IDs/digests while connected and offers resolution, not
a fresh-ID retry. CLI initialization accepts an explicit `--request-id` and prints
the selected ID before submission, so an operator can retain and reuse it after
client loss. ProjectList and installation inspection provide recovery views when
that receipt was not retained. A restarted TUI first reconciles service state;
it never automatically resubmits a previous draft under a new request ID.
Durable automated client-session recovery is outside this first packet. Clients
write no control or recovery files; the existing journal remains the only authority.

## Format 1 and record kinds

Journal format remains **1**. Format, API and protocol numbers change only when
the owner explicitly authorizes the change. Authorized feature work can add or
change record kinds without changing the format number. Asura is not released, so
no compatibility with older journals is required (owner, 2026-10-17). Schemas change
in place. A removed kind number stays unassigned; replay rejects it as unsupported.

Every stream starts with kind 10 PendingInit; kind 11 ActiveBinding binds the
graph. The earlier diagnostic initialization kinds 1 and 2 are removed, and replay
rejects a stream that starts with any other kind. Kind 3 keeps its OwnerGeneration
meaning; kinds 4 through 9 add the conversation transitions below. Later amendments
add kinds 13–23 for queue decisions, tools, promotion, Memory create, the managed
queue, project rename, composed turns and exec_cmd. Kinds 1, 2, 5, 12, 17 and
22 are unassigned. Never reinterpret an existing kind based on payload length.

Tool record kinds after the [tool call records](tool-call-records.md) change
(2026-10-18). Replay returns `UnsupportedFormat` for a retired kind and
`Corrupt` for a kind 14 record in the old `ToolIntent` layout.

| Kind | Record | State |
| --- | --- | --- |
| 14 | `ToolCall` | New layout in place: one record per model tool call, including malformed calls; the redacted name and argument text, operation code, decision and fault. See [record kind 14](tool-call-records.md#record-kind-14-toolcall). |
| 15 | `ToolResult` | Layout kept. Written only after a kind 14 with decision `run`. |
| 17 | `MemoryCreateIntent` | Retired. Create identities are derived on replay. |
| 22 | `ExecCmdIntent` | Retired. Kind 14 fields 20–23 hold the exec_cmd facts; the rest are derived. |
| 23 | `CaptureSettled` | Changed: rule-set fields 9–10 removed; follows a `run` exec_cmd kind 14. See [kind 23](tool-job-controller.md#kind-23-capturesettled). |

Public inspection validates all transitions before returning the installation
summary. It must not skip unsupported kinds or return a valid prefix after an
invalid transition.

Retain the 144-byte header, SHA-256 chain, 40-byte trailer, byte order and size
limits from the [format-1 contract](persistence-recovery.md#format-1-byte-contract).
The file magic remains `ASURAJ01`; every frame has header version 1. Other versions
reject. Transition operation IDs remain unique per frame. User request IDs are
payload fields and may link several transitions. The header command digest remains
SHA-256 of kind-u16 plus exact payload bytes. A separate request digest identifies
the normalized user command across retries.

Payload encoding is canonical, bounded binary data. Integers are unsigned
big-endian. IDs are 16 bytes; digests are 32 bytes. Strings are u32 byte length
followed by strict UTF-8. Lists are u16 count followed by encoded elements.
Optional values use u8 0/1 followed by the value only for 1. Reject other flags,
unknown enums, trailing bytes, duplicate IDs and allocation lengths above bounds.
Request digest input is command-kind-u16 plus the normalized request fields in
this encoding, excluding request ID and generated result IDs. Preserve prompt
bytes exactly; normalization does not trim or rewrite user text.

### Normalized request digest fields

Use SHA-256 over the following exact encodings. Numeric command kinds are separate
from the transition frame kind when a transition is a result or alias. Client
request ID, generated result IDs and current filesystem state are excluded.

| User command | Digest input, in exact order |
| --- | --- |
| InitializeInstallation | u16 1, mode u8, expected authority revision u64 (must be 0) |
| ProjectRegister | u16 4, normalized absolute location string |
| Turn dispatch (internal, from a queued input) | u16 5, project ID, optional original conversation ID, expected generation u64, exact prompt string |
| ConversationCancel | u16 7, model-operation ID, conversation generation u64 |

Command kind 5 identifies the service's internal dispatch of one queued input.
There is no public submit request; clients queue input with `ConversationQueueSubmit`.
For a create dispatch, the original conversation option is 0 and expected generation
is 0. For an existing conversation, the option is 1 followed by its nonzero ID,
and expected generation equals its current positive generation. TurnAccepted
retains both those original fields and the resulting conversation/generation.
A create result has generation 1 and a previously unused generated conversation ID.
An existing result keeps the original conversation ID and increments generation
once. Replay recomputes the request digest from the stored original fields before
indexing acceptance. The generated task and operation IDs must also be unused.

ProjectRegister normalization accepts an absolute UTF-8 locator with no `.` or `..`
components and no trailing separator except the filesystem root. It does not follow
symlinks or replace the locator with current filesystem metadata while hashing.
The platform independently resolves and pins the directory for admission. Alias
transitions retain the same original locator so replay recomputes command kind 4.
PendingInit has implicit expected authority revision 0 and recomputes command kind 1.
Cancellation has no independent client request ID in the selected wire contract;
its operation/generation key is idempotent, and only one cancel intent is committed.
Internal deadline/shutdown cancellation retains its distinct recorded cause.

### Transition payloads

Selected minimum schema. Fields occur in the listed order. Revision/sequence and
installation/owner identity remain in the common header. All transition writes
include the complete state/result/event obligation described below.

| Kind | Payload fields in byte order | Preconditions and effect |
| --- | --- | --- |
| 10 PendingInit | request ID, request digest, mode u8, configuration revision u64, configuration digest, intended graph ID | First frame only, owner generation 1; mode 1 embedded or 2 external; creates durable pending initialization lookup |
| 11 ActiveBinding | original request ID, binding generation u64, graph ID, configuration digest | Pending intent matches; generation 1; atomically records successful initialization result |
| 3 OwnerGeneration | no payload | Previous owner generation plus 1; durable before any new dispatch |
| 4 ProjectRegistered | request ID, request digest, project ID, location string, device u64, inode u64, registry revision u64, visibility u8 | First project revision 1, visibility 1 closed; records original result and pinned association |
| 9 ProjectRequestAlias | request ID, request digest, original location string, existing project ID, registry revision u64 | Existing same-object association; records idempotent result for a new request without another project |
| 21 TurnAccepted | request ID, request digest, project ID, original conversation ID optional, expected generation u64, conversation ID, generation u64, task ID, model-operation ID, model string, configuration digest, instructions digest, input digest, prior-operation ID list, prompt string, reserved-output-tokens u32, event cursor u64, then the required composition (see [graph memory evolution](graph-memory-evolution.md)) | Registry and expected generation verified; atomically creates conversation when needed, accepts task/operation, reserves exactly 2,048 tokens and records acceptance cursor 1. Kind 5, the earlier form without composition, is removed |
| 6 StartAuthorized | model-operation ID, conversation generation u64, helper-instance ID, input digest | Accepted operation, no cancel/terminal and same owner generation; one dispatch permit only |
| 7 CancelRequested | model-operation ID, conversation generation u64, cause u8 | Cause 1 user, 2 deadline, 3 output limit, 4 service shutdown; one intent per operation, existing intent wins |
| 8 TurnTerminal | model-operation ID, conversation generation u64, terminal kind u8, cause u8, final cursor u64, usage-known u8, output-tokens u32, charged-tokens u32, text string | Kind 1 complete, 2 failed, 3 cancelled, 4 interrupted; only once, helper settled where applicable; terminal task/operation, accounting and final event commit atomically |

Location is at most 4096 UTF-8 bytes. Model identity is at most 1024 bytes. Prompt
is nonempty and at most 32 KiB. The prior-operation list names the complete turns
in the same conversation that the composition shows; the composition accounts for
the omitted ones. Terminal text is at most 60 KiB.
Maximum frame remains 64 KiB including overhead; encode and check the entire
frame before append. At most 64 registered projects and 256 conversations are
admitted in this slice. Journal bounds remain 8 MiB and 32,768 frames. At most
one operation is nonterminal.

Required follow-up design (owner, 2026-10-15): the single 8 MiB file must not
remain the journal's limit. The owner chose to keep a separate file journal,
not SurrealDB, and to move to known-size segment files with checkpoint
snapshots and compaction. Total size comes from a configurable quota and
available disk space. A separate journal design must define it; it is not
designed here. Until it lands, the limits above apply. No overwrite or deletion makes space at a limit.

Terminal cause values are closed: 0 none, 1 user cancel, 2 deadline, 3 output limit,
4 service shutdown, 5 provider failure, 6 protocol failure, 7 restart, 8 authority
failure, 9 input/context limit. Complete requires cause 0, valid terminal metadata and no committed cancel intent.
A complete response can have unknown usage; it retains the full 2,048-token charge.
Unknown usage requires output-tokens 0 plus usage-known 0; the flag prevents that
placeholder from being reported as measured zero. Before-start terminal usage is
known zero. A restart terminal requires interrupted kind and cause 7. Terminal
text may be empty; complete text must be nonempty. Generation, model-operation ID
and task mapping must match TurnAccepted. Cancel and start records are forbidden
after terminal. A cancellation arriving during terminal append waits for that
append's known outcome, then returns already-terminal or follows recovery; it
cannot append an intent behind a completed operation.

The serialized writer reserves capacity for terminal closure before accepting a
turn: maximum terminal frame plus one start frame, one cancel frame and one owner
frame. New normal records cannot consume that reserved byte/frame capacity.
The replay-memory bound reserves those future frame bytes and the conservative
per-transition index allocation charge as well. Normal records cannot consume
that memory reserve. A terminal commit releases unused capacity, not records.
The finite store reserves one recovery owner frame, not unlimited crashes. If a
further OwnerGeneration would consume the remaining mandatory terminal reserve,
return capacity exhausted and remain repair-required with all evidence preserved.
No successful recovery or automatic space reclamation is claimed in that case.

## Admission and dispatch ordering

1. Validate the attachment, project, conversation generation and request digest.
   Check committed request IDs before allocating new task or operation identities.
2. Resolve the selected model/configuration snapshot and availability. Construct
   instructions, exact complete history references and prompt within 64 KiB.
   This preparation checks bytes, references and scope only. It creates no model
   session and requests no token count. Token counting follows the durable start
   permit under the selected D2 helper contract.
3. Recheck scope, generation, configuration identity, owner generation and the
   one-active-operation slot. Reserve journal capacity and append TurnAccepted.
4. Wait for its durable result asynchronously. Only then publish the Running
   queue projection. Publication uses the accepted operation and conversation
   identities from that frame.
5. Serialize cancellation against start authorization. Append StartAuthorized and
   wait for durability before delivering its one-use permit to the identified helper.
   Recheck a committed cancel intent before sending the permit. If the selected
   adapter has a native input counter, the helper validates its measured window
   with 2,048 output tokens reserved. Otherwise, input usage stays unknown and the
   provider must enforce context overflow under its provider contract.
   A context/counting failure becomes a durable failed terminal with known zero
   output usage when the helper confirms no generation started.
6. Process helper snapshots as provisional events. Validate operation, helper and
   conversation generations. A stale helper cannot publish or settle another turn.
7. After helper settlement, append TurnTerminal. Publish its durable terminal event
   and release the active-operation slot only after the append is known committed.

A committed StartAuthorized record does not prove the helper received its permit.
That uncertainty is intentional: a crash in this interval cannot cause replay of
an operation that might already have started. A cancel arriving after the permit
fences output immediately and starts the helper's bounded cancellation/termination
sequence. A durable cancellation acknowledgement means intent persisted; it does
not mean the SDK stopped. If cancellation arrives while an append is in flight,
queue it in reserved control capacity and prevent subsequent permit dispatch.

### Admission and cancellation race

Selected sequence. Dashed replies follow durable commit. The start permit is a
separate effect that requires both a durable start record and a current cancel fence.

```mermaid
sequenceDiagram
    participant C as Client
    participant A as Admission owner
    participant J as Journal worker
    participant H as Model helper
    C->>A: Queue input with original request ID and scoped prompt
    A->>J: TurnAccepted with 2,048-token reservation for the dispatch ID
    J-->>A: Durable acceptance
    A-->>C: Running queue projection
    alt Cancel wins before dispatch
        C->>A: Cancel operation and generation
        A->>J: CancelRequested
        J-->>A: Durable cancel intent
        A-->>C: CancelAccepted
    else Start remains authorized
        A->>J: StartAuthorized for exact helper
        J-->>A: Durable permit record
        A->>H: One-use start permit after fence check
        H-->>A: Provisional snapshots and terminal observation
    end
    A->>J: TurnTerminal with settled accounting
    J-->>A: Durable terminal event
    A-->>C: Complete or incomplete terminal outcome
```

## Idempotency, history and event recovery

The journal index maps `(installation ID, request ID)` to kind, request digest,
accepted identities and original result. Same ID and digest returns the existing
result; changed kind/digest returns `idempotency_conflict`. Revalidate disclosure
scope before returning historical content. An unresolved append reports
`outcome_unconfirmed`; do not automatically retry it or allocate another request.
A disconnect does not cancel accepted work.

Store the accepted prompt directly in TurnAccepted and complete response directly
in TurnTerminal. They are bounded conversation authority, not a second graph
memory store. Graph references and exported transcripts are optional later
projections. Private journal mode protects retained text; diagnostic/audit logs
must not duplicate it. UI drafts remain client-owned until admission commits.

Only `complete` terminal text becomes assistant history. Failed/cancelled/interrupted
prefixes remain incomplete UI evidence and are excluded from later model context.
No streaming snapshot is separately journalled. It may be lost on service restart.
A complete response lost before terminal commit is interrupted, never fabricated
from an incomplete journal tail or helper transcript.

Acceptance cursor is 1. Live snapshot cursors increase within the owner generation.
Reserve `u64::MAX` for the terminal cursor, which exceeds every provisional cursor.
The owner rejects snapshot-counter exhaustion before reaching that value. Terminal
cursor and text are durable and unchanged across restart. A restarted nonterminal
operation is committed as interrupted with this terminal cursor. Thus a client
holding any old provisional cursor can observe its terminal result without replaying
volatile revisions. `after_cursor == u64::MAX` returns the same terminal result.

## Budget and terminal outcomes

The 2,048-token reservation bounds output generation for this one-operation task.
Input-window accounting is separate and is measured after the durable start permit,
before generation, as selected by D2. The
helper limit and reservation use the same value. This packet creates no money
budget or fabricated provider billing estimate.

| Terminal condition | Charged output tokens | History and recovery |
| --- | --- | --- |
| Complete with validated known count 0 through 2,048 | Actual count; release unused reservation | Complete text becomes history after durable terminal commit |
| Complete with unknown count | 2,048, usage unknown | Complete text becomes history after durable terminal commit; no measured usage is invented |
| Cancel/failure before any StartAuthorized record | 0, usage known | No helper generation was permitted |
| Started operation with validated known terminal count | Actual count 0 through 2,048 | Incomplete text excluded from history |
| Started operation with unknown count, crash or uncertain permit delivery | 2,048, usage unknown | Full reservation remains charged; never infer zero usage |
| Count above 2,048 or invalid terminal metadata | 2,048, usage unknown and failed result | Protocol failure; no complete-history publication |

Retained unknown charges are conservative accounting, not a claim of measured
usage. Restart cannot restore them to available task budget. A new user turn is
a new task with its own explicit budget; no interrupted task is silently retried.
Task status reflects terminal failure/cancellation/interruption, not an apparently
successful empty response. Parent task scheduling remains outside this packet.

## Writer, recovery and bounded execution

Storage serializes one append worker. Queue capacity is eight: at most six normal
commands plus two slots reserved for cancel/terminal or recovery controls. New
admission returns busy when its capacity or the active-operation slot is occupied.
Frames and worker replies have fixed maximum lengths. A second worker cannot race
journal offsets or expected revisions. The service polls completions without joining
unfinished workers or holding its event loop on filesystem calls.

Open existing journal files without truncation. Retain descriptors and expected
end offset, revision and digest. Before writing, validate the chain identities,
size and stamp, then write the complete pre-encoded frame with a bounded pwrite
loop. Short writes advance the same offset; EINTR retries the same write. The
append has no time bound (J1-0). Zero write, unexpected offset/identity or failed flush fences all new
admission. Do not issue another append to guess what committed.

On supported local APFS, use the platform's reviewed `F_FULLFSYNC` wrapper for
journal durability. Creation also synchronizes each new parent directory entry
using the selected platform directory-sync primitive before acknowledgement.
No silent downgrade from a failed full flush is allowed. Unsupported volume or
sync behavior returns unavailable and blocks writes. Ordinary process-kill tests
do not qualify physical power-loss durability.

Deadlines: journal replay, append and append observation have no time bound
([journal step J1-0](#journal-step-j1-0-journal-waits-without-a-time-bound), owner,
2026-10-15). Helper preparation and handshakes have no time bound either; each
step ends on the next message, helper exit, a protocol fault, cancel or shutdown
(owner, 2026-10-16; see [provider integration](model-provider-integration.md#cancellation-driven-turn-lifetime--selected-2026-09-30)).
Generation and client observation are cancellation-driven.
Cancellation does not stop a blocked syscall. Keep its writer slot, file handle
and owner claim until settlement. Inspect and signal processing remain responsive.
Stop waits for the writer to settle. Only a worker that reports an unknown outcome
leaves the owner in RepairOnly ([stop and drain](system-architecture.md#reactor-stop-and-drain),
owner, 2026-10-16).
A full journal or uncertain writer leaves independently safe read-only status usable.

The replay result exposes one canonical internal state: sequence, end offset,
last frame digest, authority revision, owner generation, initialization intent and
binding, project/location index, request-result index, conversation generations,
accepted task/operation records, reservation accounting and terminal event offsets.
Do not build a second parser in the service. Keep prompt/response bodies on disk
by validated frame offsets; decode at most one bounded response buffer per read.
Replay format 1 into bounded indexes using the existing 16 MiB allocation budget.
Unknown kind/version, interior damage or inconsistent transitions reject the full
public replay. A short trailing frame remains preserved as `incomplete_tail` and
repair-required, matching the selected inspection contract. This packet adds no
automatic truncation, quarantine copy, checkpoint or slot switch.

Retire the initial inspection witness after its result is published and before
any append. Writer-owned descriptor identities and the current committed revision
then govern inspection snapshots. Legitimate journal growth must not invalidate
an old witness and leave permanent `authority_changed` status.

After successful replay, append and flush OwnerGeneration. Before accepting new
work, settle every previously nonterminal operation as Interrupted. An operation
with no durable StartAuthorized record charges zero. A durable start record charges
2,048 with unknown usage unless an already committed terminal record supplies usage.
Recovery never starts a model, reuses an old permit or appends a second terminal
for a completed operation. The helper owner must settle any process it can identify
as still owned before permitting replacement; uncertain child lifetime blocks
new generation. Request lookup and cancel intents remain available during recovery.

### Journal step J1-0: journal waits without a time bound

Required behavior (owner, 2026-10-15). No journal wait has a time bound:

- **Append.** An append waits for its write and its `F_FULLFSYNC` to return.
  Only an actual I/O or validation error makes the outcome unconfirmed and
  fences the journal; slowness never fences.
- **Observation.** The service waits for the writer's real reply, with no
  timer. A cancelled turn stops waiting; the late reply is applied when it
  arrives. Queued writer jobs run in order and are never dropped for age.
- **Replay.** Startup replay runs until it finishes, fails on content, or is
  cancelled by shutdown. While it runs, the service answers Inspect and refuses
  other work with a loading state.
- **Queue worker.** The one-slot queue worker waits for the real reply. Cancel
  stops the wait. A second queue request while one is in flight gets `busy`.
- **Overload.** Beyond the writer's 8-slot queue, a request gets `busy`. The
  reactor stays responsive throughout.

Tool job packet J1 depends on this step for its kind 14, 23 and 15 records
([J1 prerequisite](tool-job-controller.md#j1-prerequisite-journal-step-j1-0)).

Code changes (commit `41eebfb`):

| Site | Today | J1-0 |
| --- | --- | --- |
| `asura-storage/src/authority/writer/state.rs:97`, `:362` | Caps the append and the initialization append at 2 s | Removed |
| `asura-platform/src/journal_io.rs:165-193` | `JournalFile::append` returns `Deadline` between writes and after the sync, and leaves the file fenced | No deadline argument; errors come only from `write_at`, `F_FULLFSYNC`, `stat` or validation |
| `writer/state.rs:143-153` | Any append error fences the writer | Unchanged: an error now means an actual I/O or validation failure |
| `writer/mod.rs:273-279` `Ticket::poll` | Reports `OutcomeUnconfirmed` or `Deadline` when its deadline passes | No time check; only a reply or a disconnected worker settles it. The caller may drop its interest on cancel. |
| `writer/mod.rs:486-492` | Default ticket deadlines: 10 s initialize, 5 s open, 2 s other | All removed. Initialization waits for the engine and shows progress; cancel and shutdown still stop it (owner, 2026-10-15) |
| `writer/mod.rs:322-323` | Drops a queued job whose deadline passed | Removed: queued jobs run in order |
| `conversation/state.rs:1276-1362`, `authority.rs:44-52` | Replay stops with `Timeout` at its deadline | No deadline; the cancel flag still stops it |
| Queue worker observation, 5 s (`asura-cli/src/tui/queue.rs:112`, `:146`) | Reports uncertainty after 5 s | Waits for the reply; cancel stops the wait |
| `try_submit_before` callers | Pass caller deadlines into the writer | The caller's own deadline applies only to the caller's wait, never to the writer's work. The L10 tool deadline was later removed (owner, 2026-10-17) |

Unchanged: the 500 ms queue metadata refresh, a polling cadence that ends
nothing. The 10 s initialization budget is removed (owner, 2026-10-15).

#### J1-0 journal wait states

Selected design. Arrows name the event that moves one writer request.

```mermaid
stateDiagram-v2
    [*] --> Queued: Request admitted to the 8-slot queue
    [*] --> Busy: Queue full
    Busy --> [*]
    Queued --> Writing: Dequeued in order, never dropped for age
    Writing --> Syncing: All frame bytes written
    Writing --> Fenced: write error
    Syncing --> Committed: F_FULLFSYNC and validation succeed
    Syncing --> Fenced: F_FULLFSYNC, stat or validation error
    Committed --> Replied: Replay state advanced
    Fenced --> Replied: OutcomeUnconfirmed, repair required
    Replied --> [*]: Caller applies the reply, also after a turn cancel
    note right of Queued: The caller waits with no timer, and a cancel stops only its wait
```

| ID | Case | Unit | Integration | End-to-end |
| --- | --- | --- | --- | --- |
| J1-0-01 | Stalled append holds | A fake file whose write and sync block past 2 s returns success after release; the writer is not fenced | A test-support gate holds a real append for 5 s; the record commits and replay shows it | — |
| J1-0-02 | Stalled observation | A ticket with no reply for 10 s never reports `OutcomeUnconfirmed`; a reply after a turn cancel is applied | Gated append: the conversation owner neither fails the turn nor becomes unavailable; the late reply updates state | — |
| J1-0-03 | Queued jobs never dropped for age | A job queued behind a gated job runs after the gate opens, in order | Real writer: queued appends commit in order after a long stall | — |
| J1-0-04 | Replay holds | Replay with a gated reader returns only on completion, content failure or cancel | Large journal with a gated read: Inspect returns a loading state; work is refused; shutdown cancels replay | TUI shows loading and accepts input within 100 ms |
| J1-0-05 | Queue worker holds | A second queue request while one is in flight gets `busy`; cancel stops the wait | Gated queue mutation: no uncertainty report; the late reply is applied | — |
| J1-0-06 | Busy beyond capacity | Ninth writer request returns `busy` | Gated append: requests beyond 8 slots return `busy` | — |
| J1-0-07 | Responsiveness | — | Inspect and cancel respond within 100 ms during each gate above | TUI typing within 100 ms during a gated append |
| J1-0-08 | Real I/O error still fences | Injected `write` and `F_FULLFSYNC` errors fence and return `OutcomeUnconfirmed` | Injected error on a real file; later appends return `RepairRequired` | — |

#### J1-0 implementation

Implemented on 2026-10-15. Journal format 1 and protocol 0.1 are unchanged.
These routine refinements apply the owner rule to sites the table did not list:

- **Writer API.** `try_submit_before`, `Ticket::deadline`, the job deadline and
  `writer::Error::Deadline` are removed. `ReplayError::Timeout` and
  `AuthorityError::Timeout` are removed because nothing produces them. The
  `storage_timeout` wire string is no longer accepted (owner, 2026-10-17).
- **Shutdown cancels replay.** The writer passes its close flag to the
  authority scan, `JournalFile::read_all` and replay. A replay stopped by
  shutdown replies `Cancelled`, not `RepairRequired`.
- **Writer sub-steps.** Configuration reads and embedded database calls on the
  writer inherited the removed job deadline. They now have none
  (`conversation_snapshot`, which has no time bound).
- **Embedded database (owner, 2026-10-16).** The SurrealDB 2 s query and
  transaction timeouts and the adapter's per-command deadline checks are
  removed everywhere, including the scratch memory owner's command and startup
  deadlines. Queries end on completion, an engine error or the caller's
  cancellation (turn cancel through the create's cancel flag, shutdown through
  owner close). The scratch owner's 2 s slow-close report is unchanged.
- **Foreground FIFO.** Commands retained behind the serialized writer ticket no
  longer expire with `storage_timeout` after 2 s. They wait in order; the 33rd
  gets `conversation_busy`; shutdown replies `service_draining`.
- **Installation inspection.** The 5 s inspection scan bound is removed. The
  scan ends on completion, content failure, an epoch change or shutdown. The 2 s
  wait for the audit bootstrap before the scan starts is not a journal wait and
  was unchanged here. A later owner decision (2026-10-16) removes it too; see
  [audit bootstrap ordering](config-commands.md#audit-bootstrap-and-initial-authority-inspection-ordering);
  implemented 2026-10-16.
- **Loading state (owner, 2026-10-16).** While the inspection replay or the
  writer's own replay runs, Inspect reports `recovering`/`inspection_pending`
  and conversation work gets `conversation_busy`. A first run with no journal
  shows no loading state. When the writer's replay ends, the conversation
  owner's snapshot applies, or the inspected snapshot returns if it has none
  (`installation::ReplayLoading`). No state is added.
- **Memory create (HM3).** The caller's wait has no deadline; the earlier
  2 s tool deadline (L10) is removed (owner, 2026-10-17). A turn cancel,
  steering or shutdown sets the create's cancel flag, which stops the
  cancellable put. The receipt resolution read has no deadline.
- **Journal-backed client mutations (owner, 2026-10-16).** Initialize, project
  register and rename, queue input, reorder and decision, and
  conversation cancel have no client response budget. The client checks the
  person's cancel callback every 50 ms; this cadence ends nothing. Cancel or a
  lost connection returns `OutcomeUnconfirmed`, and the attachment then refuses
  further requests. `asura init` and `asura project add` print the request ID
  and progress, and Ctrl-C stops the wait. In the TUI, `/init` and
  `/project add` show progress and `/cancel` stops the wait, as it does for a
  rename or queue request. Read, config, models, lifecycle and TUI job
  budgets were unchanged in J1-0. A later owner decision (2026-10-16) removes
  them as well: each wait ends on the real reply, the person's cancel or a lost
  connection. Implemented 2026-10-16; see
  [command flows](command-flows.md#client-waits-without-time-bounds).
- **Queue worker.** `/cancel` while a queue request is in flight stops the wait.
  The slot stays busy until the worker settles. The service-owned queue
  projection shows the request if it committed.
- **Turn cancel display (owner, 2026-10-16).** The 100 ms cancel requirement
  measures admission. After `/cancel` the TUI shows "Cancelling" until the
  service acknowledges the durable `CancelRequested` record, then "Cancelled"
  until the turn settles.
- **Test hook.** `asura_platform::journal_gate` (feature `test-support`) holds or
  fails appends and reads for one scratch home. A file-based form serves child
  service processes. Without the feature the hook points compile to no-ops.

Validation, 2026-10-15:

| ID | Unit | Integration | End-to-end |
| --- | --- | --- | --- |
| J1-0-01 | `journal_io::tests::j1_0_01_*` (fake target and gated real append past 2 s) | `journal_writer::j1_0_01_*` (5 s hold, commit, replay) | — |
| J1-0-02 | `writer::tests::j1_0_02_*` (10.1 s without reply); `foreground_tests::j1_0_02_*` (reply after turn cancel applied) | `conversation_flow --journal-waits` (held queue append; late reply applied; owner usable) | — |
| J1-0-03 | `writer::tests::j1_0_03_*` | `journal_writer::j1_0_03_*` (5.2 s stall) | — |
| J1-0-04 | `journal_io::tests::j1_0_04_*`; `installation::tests` held scan never expires and `writer_replay_shows_loading_only_when_a_journal_exists` | `journal_writer::j1_0_04_*` (shutdown cancels); `conversation_flow --journal-waits` (loading during inspection and writer replay, refused work, shutdown) | `tui_pty.py --journal-waits` |
| J1-0-05 | `queue::tests::j1_0_05_*`; client `initialization_wait_has_no_time_bound_and_only_cancel_ends_it` | `conversation_flow --journal-waits` (queue mutation held 5.2 s, past the removed 3 s client budget; no uncertainty) | — |
| J1-0-06 | `writer::tests::j1_0_06_*` | `journal_writer::j1_0_06_*` | — |
| J1-0-07 | `ui::tests::j1_0_*` (cancel paths); `ui::tests::turn_cancel_shows_cancelling_until_the_durable_record_then_cancelled` | `conversation_flow --journal-waits` (Inspect, cancel and init cancel under 100 ms) | `tui_pty.py --journal-waits` (typing under 100 ms) |
| J1-0-08 | `journal_io::tests::j1_0_08_*` | `journal_writer::j1_0_08_*` | — |

The 100 ms cancel requirement measures admission (owner, 2026-10-16). The
integration check times cancel requests for an unknown operation. A cancel for a
live turn is admitted without waiting for the writer (unit
`foreground_tests::j1_0_02_*`). Its durable acknowledgement follows the
`CancelRequested` record; the TUI shows "Cancelling" until then.

### Turn and restart states

Selected state transitions. Journal commit establishes each durable state.
Uncertain writes route to replay/repair, not directly back to admission.

```mermaid
stateDiagram-v2
    [*] --> Preparing
    Preparing --> Accepted: TurnAccepted durable
    Preparing --> Rejected: Validation or capacity failure
    Accepted --> Started: StartAuthorized durable and fence valid
    Accepted --> CancelPending: CancelRequested durable
    Started --> CancelPending: CancelRequested durable
    Started --> Settling: Terminal helper observation
    CancelPending --> Settling: Helper settled or never started
    Settling --> Terminal: TurnTerminal durable
    Accepted --> Recovering: Restart
    Started --> Recovering: Restart
    CancelPending --> Recovering: Restart
    Settling --> Recovering: Uncertain append or restart
    Recovering --> Terminal: Interrupted terminal durable
    Recovering --> RepairRequired: Invalid or incomplete journal
    Terminal --> [*]
    Rejected --> [*]
```

## Implementation order and acceptance

Root must integrate this amendment into the canonical recovery design before
changing journal code. Public inspection validates the kind-10 initialization
path and rejects malformed records and removed kinds.
No model permit may be enabled before the writer and recovery checks below pass.

1. **CA-A, pure codec/replay:** own canonical storage authority modules and tests.
   Encode/decode the exact format-1 records, validate transitions, reconstruct
   indexes within format 1. No filesystem writes or model calls.
2. **CA-B, platform IO and storage writer:** implement descriptor-held create/open,
   full flush/directory durability, expected-offset append and uncertain settlement.
   Integrate the production graph binding adapter and explicit initialization.
3. **CA-C, service init/register/admit:** deliver request outcomes and registration,
   preserving PendingInit/ActiveBinding and bound-graph checks. The scratch HM0
   adapter alone does not complete this prerequisite.
   Implement conversation routing/admission with a scripted helper. Prove request
   replay, reservation, start/cancel ordering, terminal durability and restart.
4. **CA-D, UI and native execution:** connect the reviewed native model helper and
   client setup/selection/event flow. Then run the
   real on-device conversation and the existing status/TUI responsiveness journeys.

| Case | Unit | Integration | End-to-end |
| --- | --- | --- | --- |
| CA01 format | Header stays 1; first record must be kind 10; reject unknown versions, removed kinds 1, 2, 5 and 12, and other unknown kinds | Kind-10/11 frames from the canonical encoder pass every-byte damage, every-boundary truncation and writer-death checks; conversation file survives reopen and public inspection | A journal that starts with another kind is reported unsupported, never converted |
| CA02 initialization/project | Original request mapping, identity, revision and visibility | PendingInit/ActiveBinding fault boundaries; pinned directory replacement | Explicit initialize/register/restart resolves original IDs |
| CA03 admission | Duplicate ID, changed digest, generation mismatch, limits and 2,048 reservation | Real journal commit before scripted helper permit | Lost acceptance reply and reconnect produce one operation |
| CA04 cancel race | Cancel before/after permit, stale helper, duplicate intent | Gate each append/flush and helper start boundary | Cancel acknowledgement remains distinct from terminal settlement |
| CA05 completion | Usage limits, provisional/complete distinction, terminal cursor | Kill before/after terminal flush; preserve complete response only after commit | Reconnect displays original complete or interrupted result |
| CA06 restart | Started versus never-started charge and no re-execution | Real child/service death with durable or incomplete frames | Restart reports Interrupted without another model call |
| CA07 journal failure | Reserved closure capacity, short writes, zero write and stalled write | Real scratch disk/flush failures, identity swaps, stalled worker and owner retention | Inspect/Stop and TUI input remain responsive, unknown outcomes explicit |
| CA08 bounds | 64 projects, 256 conversations, 8 MiB/32768 frame limits, eight-slot queue | Saturation and 16 MiB replay allocation measurement | Capacity exhaustion preserves draft and existing outcomes |
| CA09 live model | Model selection and result validation | Real helper transport and cancellation, no tools | Supported macOS model completes a scoped turn with durable history |

Target TUI/control response under a stalled storage/helper dependency is 100 ms
in the supported test environment. Record actual latencies and host/filesystem/SDK
versions. Mock success does not prove APFS durability, helper cleanup or live model
availability. Missing physical power-loss evidence remains a stated limit.

## Implementation status and remaining qualification

CA-A supplies the canonical format-1 conversation codec and replay. CA-B supplies
the descriptor-held writer, explicit embedded initialization, graph binding,
project registration, preparation and restart settlement. The service and clients
use these owners; root records their native and command-level validation separately.

Verified storage behavior on 2026-09-27: 15 unit tests, 12 conversation authority
tests, six diagnostic authority tests and five journal writer integration tests
passed. The writer tests use private scratch directories and actual APFS files.
They cover:

- Full-sync append, reopen and unchanged bytes after rejected replacement.
- Exclusive journal locking and project directory replacement/symlink refusal.
- Preservation of an incomplete journal tail.
- Initialization request replay, registration and guarded turn acceptance.
- Restart settlement with known zero usage before start and unknown full-reservation
  charge after a durable start permit.
- Missing database refusal without creating a replacement database.
- Snapshot retention backpressure before writes, followed by progress after release.

Unit checks verified that expired mutation observations and disconnected completion
channels reported uncertain outcomes. J1-0 later removed observation expiry; a
disconnected channel still reports an uncertain outcome. The timeout poll completed
within 100 ms in that test; this does not qualify responsiveness under an actual stalled syscall.
The 64 MiB journal allocation ceiling has static conservative accounting and a
retained-snapshot backpressure test. Peak resident-memory measurement remains open.

Remaining qualification includes injected short/zero writes and flush failures,
actual stalled storage calls and queue overload, process death at each durability
boundary, and physical power loss. Successful APFS full-sync calls and normal
reopen tests do not establish physical power-loss durability. These limits prevent
a claim that every CA01–CA09 acceptance case is complete.

External database deployment remains outside this slice. Preserve existing evidence and report unavailable capabilities.
Root owns integration, the design index and delivery-plan updates.

## Packet validation

On 2026-09-27, Mermaid CLI 12.0.0 rendered all three diagrams. The rendered
images were visually inspected for readable labels and consistent transitions.
The initial packet passed whitespace checks. Those renders validate the diagrams,
not runtime behavior. The implementation checks are listed separately above.
The CA-B refinement keeps the same diagram transitions and component ownership.

## CA-B descriptor and worker implementation contract

Selected refinement, 2026-09-27. The storage worker retains one platform journal
capability. This capability pins the runtime root, state directory, control
directory and slot file. A nonblocking exclusive file lock prevents another writer
from opening the same slot. This supplements the service owner lock. The capability
checks identities and private permissions before and after each append. It rejects nonlocal or non-APFS volumes. Newly created
entries use `fsync` on their parent directories; journal contents use
`F_FULLFSYNC`. Either failure prevents acknowledgement. The writer never truncates.

The worker owns all replay buffers and mutable state. Callers enqueue at most six
ordinary requests or eight total requests, including closure controls. Each reply
has a one-item channel. Polling never waits. A caller waits for the real reply,
with no observation deadline; startup replay and journal appends have no time
bound either ([journal step J1-0](#journal-step-j1-0-journal-waits-without-a-time-bound)).
Initialization has no time bound (owner, 2026-10-15, replacing HM0's 10-second operation budget); it waits for the engine, the client shows progress, and cancel or shutdown stop it. The worker keeps its claim until it settles. The embedded engine shares the existing process-wide engine claim.
Shutdown drops its router and retains the executor until all engine tasks settle.
Dropping a caller never opens a replacement writer. Shutdown joins only
a finished worker. Initialization and graph verification run on this same isolated
owner; service control remains independent. Project identities retain a directory
file descriptor and revalidate every path component without following symlinks.

The existing admission and recovery diagrams govern this implementation without
new lifecycle states. The embedded graph marker uses the original initialization
request ID as `init_operation_id`; frame transition IDs remain distinct in the
journal. CA02 and CA07 require actual scratch APFS append/reopen,
replacement rejection, incomplete-tail preservation and expired observation tests.

### CA-B aggregate journal memory limit

The per-replay accounting limit remains 16 MiB. The worker bounds the aggregate
journal allocation at 64 MiB: three replay generations at most 16 MiB each,
one 8 MiB journal buffer, eight commands/replies of at most 64 KiB each, and
bounded frame, preparation and project-page buffers. This bound excludes the
separately bounded embedded engine and model owners.

The reactor replaces its replay snapshot when it consumes a reply. The writer
retains weak references to retired snapshots. Before another append, it permits
at most one externally held retired generation. If an additional generation would
remain held, admission returns busy before allocating replay state or writing.
Queued replies may share the same immutable generation; they do not clone indexes.
Tests retain old generations to prove this backpressure and release them to resume.


## Durable input queue (IQ1)

Status: selected input contract, revised 2026-10-17. The service owns its durable
records and replay. The [managed input queue](managed-input-queue.md) defines
submission, order, reorder and scheduling. Every queued input is one kind-18
`InputQueuedV2` record (owner choice “One record”, 2026-10-17), including when
the service is idle and when the person steers with Ctrl+S. The earlier
target-bearing kind-12 `InputQueued` record, its digest command kind 12 and the
`ConversationEnqueue` operation are removed. No compatibility with older journals
is required (owner, 2026-10-17); replay rejects kind 12 as unsupported. The
[composer interaction](composer-interactions.md) covers the client. Wire stays
0.1 and journal format stays 1.

### Ownership and bounded admission

The conversation service owner accepts, orders, dispatches and recovers inputs.
The existing journal writer alone persists them. Clients retain an unsent draft,
one unacknowledged request and a service projection. Disconnect never deletes an
accepted input. Retries reuse the original request ID and exact payload.

A queue input names its project and either an existing conversation with its
observed generation or an explicit new conversation. Queue acceptance validates
these identities and records the prompt before acknowledgement. A stale or
foreign identity rejects without effects. At most 16 unresolved inputs exist
across the service, each at most 32 KiB UTF-8. Busy storage rejects before
acceptance; clients retain text. Existing cancellation capacity remains
reserved. Each dispatch uses the
[cancellation-driven turn lifetime](model-provider-integration.md#cancellation-driven-turn-lifetime--selected-2026-09-30).
It keeps the current output reservation and one active operation limit.

InputQueuedV2 stores its original request/digest, a service-generated input ID,
a fresh internal dispatch request ID, project, conversation, expected generation,
new-conversation flag, prompt and initial order revision. Internal dispatch IDs
cannot collide with any request or another queued dispatch ID. Acceptance creates
no model operation or token reservation. TurnAccepted for the internal dispatch
ID establishes dispatch atomically and remains subject to canonical admission.
History and configuration are resolved at dispatch, not queue acceptance.

Replay retains bounded metadata and journal frame references. It must not clone
all queued prompt bodies into each replay snapshot. Input bodies are read through
the existing bounded ReadRecord writer command. Queue listing returns at most 16
metadata entries with 256-byte UTF-8-safe excerpts; one-input inspection returns
its bounded full text. A frame remains limited by the existing 64 KiB contract.
Queue projection reads cannot wait on filesystem or inference in the reactor.

### Order, steering and recovery

Within a conversation lane, the durable order revision and order positions
define dispatch order; the [managed queue](managed-input-queue.md#scheduling-dependency-and-steering)
owns that rule. A committed failed or cancelled turn settles its operation; queued
successors stay Queued. Interruption after owner restart, preparation failure or
an explicit Hold holds an input. Held inputs do not run until an explicit Resume
commits. Drop commits a terminal queue decision and never deletes the retained
prompt or rewrites history.

Steer uses an explicit cancel-and-replace boundary in this first model adapter.
An input becomes Steer only through IQ2 promotion of an accepted kind-18 input.
Its presentation says that active generation will stop and restart with the
instruction. It does not claim to modify running inference. Only one unresolved
Steer may target an operation. Durable promotion precedes cancellation. The owner
fences output and uses existing cancellation/settlement before replacement.
The replacement prompt includes the original user prompt and the new instruction;
it excludes incomplete assistant output. Their combined UTF-8 size must fit 32 KiB,
or promotion rejects before cancellation. A Steer is eligible only after its exact
target is durably Cancelled by that steering request. Restart, unrelated user
cancellation, service shutdown or failed settlement holds it instead. Queue
successors in the lane keep their durable order behind the promoted replacement.

The journal records InputDecision with Hold for preparation failure, or Resume/Drop
for explicit recovery. Decisions have unique request IDs and canonical digests; exact retries
return the original result and changed payloads conflict. Replaying OwnerGeneration
holds undispatched inputs whose scheduling outcome is uncertain. Recovery never
restarts inference automatically from a previous owner generation. It preserves
accepted text and projects Held until explicit Resume. A queue input already
associated with TurnAccepted resolves to that operation, never a second dispatch.
Uncertain append fences new scheduling under the existing writer contract.

### Asynchronous boundaries

Queue mutations use the existing eight-slot isolated journal worker. Append,
append observation and replay have no time bound
([journal step J1-0](#journal-step-j1-0-journal-waits-without-a-time-bound)). Project checks, prompt reads,
configuration and history remain worker-side. One in-flight queue mutation uses
the conversation owner's serialized ticket; the reactor continues Inspect, helper
signals and cancellation. Scheduler work per reactor cycle is one bounded queue
transition. Queue projections are immutable bounded results; client workers send
RPCs and publish results through bounded mailboxes. A separate one-slot queue
worker permits queue input while the conversation worker observes execution. It
waits for the real reply with no observation deadline; cancel stops the wait,
and a second queue request while one is in flight gets `busy` (J1-0). Queue
metadata refresh uses a 500 ms interval, a polling cadence that ends nothing, with no RPC
in rendering and no claim of server-push delivery. Rendering performs no RPC,
filesystem call, database call, blocking receive or producer-lock wait.

The [composer design](composer-interactions.md) and [managed queue](managed-input-queue.md)
define all-input staging, queue presentation and promotion. /queue lists service
state; /queue resume ID and /queue drop ID operate on exact retained identities.
/retry retains request identity; /cancel retains the current operation's
existing semantics.

### Command and scheduler outcomes

Selected flow. Arrows are decisions or durable transitions; no arrow bypasses
canonical service admission. Ctrl+S adds a promotion request after this flow
acknowledges the input (see [IQ2](#existing-input-promotion-iq2)).

```mermaid
flowchart TD
    Input["Queue input: project, conversation, generation, text"] --> Validate{"Identity and bounds valid?"}
    Validate -->|No| Reject["Reject and retain client text"]
    Validate -->|Yes| Prior{"Request already recorded?"}
    Prior -->|"Same digest"| Ack["Return original durable result"]
    Prior -->|"Changed digest"| Reject
    Prior -->|No| Capacity{"Known lane and capacity available?"}
    Capacity -->|No| Reject
    Capacity -->|Yes| Persist["Append InputQueuedV2 (kind 18) and flush"]
    Persist -->|"Uncertain or failed"| Fence["Fence scheduling and retain request identity"]
    Persist -->|Durable| Ack
```

Selected scheduler flow. Durable queue records supply work; clients do not dispatch.

```mermaid
flowchart TD
    Ack["Durable queued input"] --> Kind{"Promoted to Steer?"}
    Kind -->|Yes| Cancel["Fence and settle exact active generation"]
    Kind -->|No| Head{"Lane head and service idle?"}
    Cancel --> Settled{"Steering cancellation settled?"}
    Settled -->|Yes| Prepare["Resolve current history and admit replacement"]
    Settled -->|"No or restart"| Held["Held until explicit review"]
    Head -->|No| WaitHead["Wait for earlier lane inputs or active turn to settle"]
    WaitHead --> Head
    Head -->|"Yes, not Held"| Prepare
    Prepare -->|Failure| Held
    Prepare -->|"TurnAccepted durable"| Run["Run once under existing model lifecycle"]
```

```mermaid
flowchart TD
    Command["Queue list / inspect / resume / drop"] --> Read{"Read operation?"}
    Read -->|Yes| Projection["Return bounded service projection"]
    Read -->|No| Prior{"Decision request already recorded?"}
    Prior -->|"Exact retry"| Reply["Return recorded outcome"]
    Prior -->|"Changed payload"| Conflict["Reject conflict"]
    Prior -->|No| State{"Exact input is Held?"}
    State -->|No| Reject["Reject stale decision"]
    State -->|Yes| Commit["Append Resume or Drop and flush"]
    Commit -->|Uncertain| Unknown["Preserve request ID and fence scheduling"]
    Commit -->|Durable| Reply
    Reply --> Action{"Committed action"}
    Action -->|Resume| Ready["Eligible for canonical admission"]
    Action -->|Drop| Dropped["Retained terminal record"]
```

### Required validation

IQ01 codec/replay unit tests cover record bounds, lane identities, duplicate
requests, changed digests, dispatch association, capacity and invalid transitions.
They also prove that kind 12 is rejected as an unsupported kind.
IQ02 real journal integration tests cover queued acceptance/reopen, owner restart,
held text, exact decision retries and one dispatch across lost acknowledgement.
IQ03 service process tests cover in-order execution, concurrent clients,
stale target rejection, overload, steering commit-before-cancel and helper settlement.
IQ04 terminal tests cover default queue, Ctrl+T and Ctrl+S staging, selection-based promotion, pending acknowledgement, retained
new draft, queue projection, resume/drop and reconnect. No synthetic fixture alone
proves production scheduling. IQ05 stalled storage/helper tests retain responsive
Inspect/cancel/input with a 100 ms target; uncertain work must not lose its owner.
Existing native-model completion and cleanup checks remain required. Physical
power-loss durability remains outside the existing evidence, not newly claimed.

### Service terminal precedence during helper settlement

A service-detected failure fixes its terminal cause before it sends a cancellation
frame to settle the helper. The helper's resulting cancelled terminal acknowledges
settlement; it must not replace the original failure, accounting or snapshot.
An explicit user cancellation likewise keeps its selected cause. A later failure
while settling an already failed or cancelled operation cannot replace that cause.
The service emits a fixed diagnostic class at the initial failure boundary, with
no prompt, tool arguments, file content or model output. Unit regressions cover
protocol failure, deadline failure and explicit cancellation followed by helper
cancel acknowledgement; native replay must still establish the original runtime
cause of an unexpected first-turn cancellation.

```mermaid
flowchart TD
    Failure[Service detects failure] --> Record[Retain first failure cause and accounting]
    Record --> Log[Emit fixed failure diagnostic class]
    Log --> Cancel[Send settlement Cancel to helper]
    Cancel --> Reply[Helper terminal arrives]
    Reply --> Existing{Service terminal already selected?}
    Existing -->|Yes| Preserve[Preserve existing terminal unchanged]
    Existing -->|No| Classify[Classify helper terminal using current cancellation authority]
    Preserve --> Settle[Wait for helper and tool worker settlement]
    Classify --> Settle
    Settle --> Commit[Commit canonical terminal record]
```

## Existing input promotion (IQ2)

Status: implemented and extended by the [managed queue](managed-input-queue.md).
The composer queues new text
without a choice dialog. Selecting an accepted queued input can send it now.
The service changes that same input to Steer; clients must not copy its prompt or
combine Drop and a second queue input. IQ1 bounds, transport, ownership and settlement apply.
Protocol remains 0.1 and the journal remains format 1.

`ConversationQueueDecision.action = 3` requests promotion. It includes the input
ID, a fresh stable request ID, and the observed active operation ID and generation
(fields 4 and 5). Resume and Drop omit these target fields. Exact retries return
the original result; a changed target or input under the same request ID conflicts.

The writer requires an undispatched kind-18 Queue input in Queued or Held state, the exact active
operation in that input's conversation and project, and no existing Steer for that
operation. A completed, cancelled, changed or dispatched target rejects without
cancellation. The writer reads the retained prompt through its existing bounded
record reader and verifies the combined steering prompt fits 32 KiB. It validates
the project identity, then commits one InputPromoted record (kind 16). That record
retains the original input and dispatch identities, prompt frame and sequence. It
changes the effective kind and target. The original queue request remains
retryable with its original digest. An idle Send now instead moves the input to
the front by durable reorder.

Ctrl+S uses the same promotion. The TUI first queues the draft as a kind-18 input.
When the durable receipt arrives and the captured target operation is still
active, it sends action 3 for the accepted input and that exact target. If the
target is no longer active, it sends no promotion; the input stays Queued in lane
order and the notice says so. A stale-target rejection also leaves it Queued.

Correction (2026-10-17): the writer previously read the input's record as kind 12
and returned `repair_required` for a kind-18 input, so Send now failed during an
active turn. The writer reads the kind-18 record. Regression case IQ2-R1 covers
it at the writer unit, service integration and PTY journey layers.

The service detects the committed Steer through the existing scheduling path.
Only then may it cancel the active generation. Existing output fencing and helper
settlement precede replacement dispatch. Other queued inputs keep their lane
order behind the replacement. An explicit promotion clears the
selected input's Held state. Restart holds undispatched promotion,
and an uncertain append prevents scheduling. No new reactor I/O, worker, retry loop
or queue is introduced: the eight-slot writer and 16-input limit remain
authoritative. Append, observation and replay have no time bound
([journal step J1-0](#journal-step-j1-0-journal-waits-without-a-time-bound)).

### IQ2 promotion flow

Selected design. Arrows describe request validation and durable state transitions.

```mermaid
flowchart TD
    Select["Select queued input: send now"] --> Send["Decision 3: input, request, exact target and generation"]
    Send --> Retry{"Existing request ID?"}
    Retry -->|Same digest| Prior["Return original input result"]
    Retry -->|Different digest| Reject["Reject without cancellation"]
    Retry -->|No| Check{"Undispatched Queue or Held input and exact active target?"}
    Check -->|No| Reject
    Check -->|Yes| Bounds{"No competing Steer; valid project; combined prompt <= 32 KiB?"}
    Bounds -->|No| Reject
    Bounds -->|Yes| Commit["Commit InputPromoted for same input and dispatch ID"]
    Commit -->|Uncertain append| Fence["Fence scheduling; retain request for exact retry"]
    Commit -->|Durable| Cancel["Existing owner cancels exact generation"]
    Cancel -->|Steering cancellation and settled helper| Dispatch["Dispatch same input once as replacement"]
    Cancel -->|Restart or other failure| Held["Hold input for explicit recovery"]
```

### IQ2 validation

- Unit: strict action/target fields; unchanged version numbers; promotion codec,
  digest and replay; unchanged input identity and prompt frame; stale target,
  dispatched input, competing Steer and invalid generation rejection.
- Integration: journal reopen preserves promotion; owner restart holds it; exact
  retries resolve to the same input and changed payloads conflict. Existing writer
  failure tests cover append uncertainty and mutation settlement.
- End-to-end: real control client queues text, promotes that input, observes target
  cancellation and replacement completion, then verifies one input and one dispatch.
  TUI tests cover default queue, selection, pending acknowledgement, draft retention,
  project fencing and responsive navigation while the mutation is pending.
- IQ2-R1 regression: with a turn active, a queued kind-18 input is promoted by
  Send now (action 3). Required: one InputPromoted record, target cancellation and
  one replacement dispatch; never `repair_required`. Evidence: writer unit test,
  service integration test and a PTY journey.
- IQ2-S1 Ctrl+S: with a turn active, Ctrl+S stages the draft, the kind-18 receipt
  arrives, then action 3 names that input and target. Required: one kind-18 record
  and one kind-16 record, then replacement. If the target settled first, the input
  stays Queued and no promotion is sent. Evidence: TUI unit tests and the PTY journey.

### IQ2 history-baseline correction

Queue inspection includes historical terminal entries. Their dispatched generation
is historical evidence, not the current conversation generation. Clients must not
derive the active cursor from the first full queue snapshot or lower an active
same-conversation generation. The separate [conversation restoration read](conversation-restoration.md)
selects the latest accepted conversation from the journal. The
[composer history baseline](composer-interactions.md#queue-history-and-the-active-conversation-cursor)
defines bounded eligibility tracking for new/live completions across reconnects.
The [managed queue](managed-input-queue.md) permits a lagging observed generation
in a known conversation lane while using the current generation at dispatch.
This historical baseline correction itself did not change journal records or
stored user data.

```mermaid
flowchart LR
    History[Queued turn completed at generation 3] --> Later[Later turn advances conversation to 4]
    Later --> Launch[Fresh client receives historical queue entry]
    Launch --> Baseline[Mark terminal entry as baseline history]
    Launch --> Read[Read latest accepted conversation from journal]
    Baseline --> Cursor[Queue does not change cursor]
    Read --> Cursor
    Cursor --> Submit[New queue input names restored conversation]
    Submit --> Admit[Canonical service admission validates dispatch generation]
```

The historical validation fixture includes the generation-2 target,
generation-3 queued completion and generation-4 later turn. Revised
queue-first validation must cover live transitions and provisional lanes.

Historical verification on 2026-09-28: the normal CLI lifecycle suite passed the
old fresh-conversation requirement. Strict replay verified each new input used no
original conversation and expected generation zero. Conversation restoration
supersedes that behavior and requires new evidence. The 124-test CLI unit suite
also passed under the old requirement. See the
[composer verification record](composer-interactions.md#history-baseline-verification--2026-09-28)
for scope, cleanup evidence and the separate native generation limitation.

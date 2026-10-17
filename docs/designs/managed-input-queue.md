# Managed conversation input queue

Status: selected design for the owner's 2026-09-29 queue instruction. Codec,
authority, service and TUI implementation are integrated. Focused unit,
service and isolated lifecycle/PTY checks passed; the full validation matrix
below remains open. Protocol number remains
0.1. Journal format remains 1. The [IQ1 and IQ2 contracts](conversation-admission.md#durable-input-queue-iq1)
govern durable input records, promotion and steering settlement. Every queued
input is one kind-18 record (owner choice “One record”, 2026-10-17).

## Required behavior and scope

Every conversational message enters a managed input queue before model admission.
The TUI accepts Enter without waiting for a backend response. A sender consumes
its pending local outbox sequentially. The service durably accepts each message,
then its existing conversation owner selects one eligible message for dispatch.
The user can move an undispatched message within its conversation queue. The user
can select one to send now while an operation is active. The queue appears in a
compact panel above the composer.

Built-in commands, configuration changes and project selection remain control
operations. Slash text recognized as a command does not enter the conversation
queue. A literal slash message follows the existing escape or quoting rule.
This packet does not add a generic job scheduler, another process, multiple
simultaneous model operations or automatic execution after uncertain recovery.

The owner replaced the earlier direct idle submission and the target-bearing
kind-12 queue record. Both are removed, with the `ConversationSubmit` (envelope
field 22) and `ConversationEnqueue` (field 35) operations (owner, 2026-10-17).
Asura is not released, so no compatibility with older journals is required;
replay rejects kind 12. Ctrl+T queues the draft exactly as Enter does. Ctrl+S
queues the draft as a kind-18 input, then requests IQ2 promotion of that input
(see [steering keys](#steering-keys-ctrls-and-ctrlt)). IQ2's atomic promotion,
exact target validation, cancel-and-replace behavior and prompt size limit remain
in force. Queue successors keep their durable order behind the promoted replacement.

## Ownership and process boundaries

| Owner | Responsibility |
| --- | --- |
| Rust TUI | Draft editing, volatile pending outbox, focus, reorder interaction and service projection |
| Rust control client worker | Serial RPC delivery, exact retry identity and bounded result delivery |
| Rust conversation service | Project/conversation checks, queue ordering, fair selection, admission, steering and events |
| Rust storage authority writer | One format-1 journal, flushed queue mutations, replay and request lookup |
| Swift model helper | One admitted model operation at a time; no queue mutation or scheduling |

The TUI outbox is a staging area, not an authority record. Its `Pending local`
rows are visibly different from service-acknowledged `Queued` rows. A crash can
lose local rows, including text removed from the editor when Enter stages it.
The TUI labels this state provisional and never claims durable acceptance.
At local outbox acceptance, Enter copies the exact text and request ID into a
row, then clears the editor immediately. If the outbox is full, the editor
retains its text. A definite service rejection keeps the exact text in a
`Rejected local` row. Enter on that row restores it into an empty editor;
when the editor contains a newer draft, the row stays retrievable until the
user clears or submits that draft. The client never overwrites newer text.
Once the service acknowledges an input, only service state determines its position and
outcome. A client disconnect cannot drop it. The render path only reads immutable
projections. It performs no socket, file, model or blocking lock operation.

### Ownership view — selected design

Arrows show requests and event delivery. The service alone starts model work.

```mermaid
flowchart TB
    Key["TUI Enter"] --> Outbox["Volatile outbox: max 16"]
    Outbox --> Sender["One serial client sender"]
    Sender -->|"Queue RPC, stable request ID"| API["Control API 0.1"]
    API --> Owner["Conversation owner: order and admission"]
    Owner -->|"Append and flush"| Journal[("Format-1 authority journal")]
    Journal -->|"Accepted result"| Owner
    Owner -->|"One eligible input"| Model["Swift model helper"]
    Owner -->|"Queue projection and receipt"| API
    API -->|"Worker mailbox"| UI["TUI event loop"]
    UI -->|"Render only"| Panel["Compact queue panel"]
```

## Identity, order and mutation contracts

An `input_id` is a service-generated nonzero 16-byte ID. The client supplies a
stable nonzero 16-byte `request_id`. Its digest covers project ID, optional
conversation ID, expected conversation generation, exact UTF-8 prompt bytes and
the explicit new-conversation intent. The service returns the same input ID for
an exact retry. Reusing an ID with different fields is a conflict. Prompt bytes
are never normalized. One input has one internal dispatch request ID and at most
one accepted model operation. The existing 32 KiB prompt limit applies.

The first queued message for a new conversation allocates its conversation ID
in the same durable `InputQueuedV2` record. Its generation is zero until first
`TurnAccepted`. Later queued messages name that conversation. The service rejects
an unknown, cross-project or stale conversation. The first enqueue requires an
explicit empty restoration result or `/new` intent, not a guessed absence of
history. A selection change fences unsent local requests. A service-epoch change
discards stale projections and resolves an in-flight request by its exact ID.
Replay tracks a generation-zero provisional conversation separately from
accepted-turn history.
Later messages may name that provisional conversation with generation zero and
`new_conversation = false`; the service verifies its durable project and lane
identity. They do not allocate another conversation. After a turn is accepted,
the service permits another queue input for the same conversation when the
client's observed generation lags that accepted turn. This queues future work;
dispatch still uses the current generation and complete history.
Only one undispatched new-conversation lane per project is allowed in this
slice. `/new` rejects while that project has unresolved queued inputs. On
relaunch, a queue snapshot identifies that lane before a new submission; the
TUI selects it if it is newer than the latest accepted turn. The service still
validates the generation. This extends the restoration contract without
mistaking a historical terminal queue row for a current cursor.

The queue has one ordered lane per conversation. Reorder moves one `Queued` or
`Held` input after a named pending input in that same lane; an absent predecessor
means the front. The predecessor cannot be the moved input. Running, terminal,
promoted steering and in-flight local entries cannot move. Unsent local rows
may change position before the sender claims them. A successful reorder
does not modify prompt, input ID, dispatch ID or conversation generation. It
changes only future selection order. It cannot preempt a dispatch already
committed as `TurnAccepted`.

The client sends `input_id`, optional `after_input_id`, stable mutation request
ID and `expected_order_revision`. The storage writer checks all identities and
revision under its serialized owner, then appends and flushes one `InputReordered`
record. The queue-specific order revision increases by one for each committed
enqueue, reorder, drop, resume, promotion or dispatch. It never wraps. A stale
revision returns the current bounded projection; the client keeps selection and
can offer a fresh user-directed move. An uncertain append keeps the same request
ID and digest for resolution; no local optimistic order is presented as durable.
The reorder digest covers command kind 19, input ID, optional predecessor ID
and expected order revision in that order. It excludes request ID and resulting
revision. A position-preserving move still records its request result and
advances the revision, so exact retries remain stable.

Kind 18 is `InputQueuedV2` and kind 19 is `InputReordered`. Kind 12, the earlier
target-bearing record, is removed and unassigned. Kind 18 contains,
in order, request ID, request digest, input ID, dispatch request ID, project ID,
conversation ID, expected generation, new-conversation flag, exact prompt and
initial order revision. The new-conversation flag requires an unused generated
conversation ID and expected generation zero. Existing-conversation records
require a registered conversation and matching expected generation. Kind 19's
canonical binary payload is
request ID, request digest, input ID, optional predecessor input ID, expected
order revision and resulting order revision. Replay rejects a missing input,
wrong lane, non-pending predecessor, changed request digest, revision gap,
duplicate ID or impossible order. The version-0.1 Protobuf envelope carries
`ConversationQueueSubmit` (field 53) and `ConversationQueueReorder` (field 54);
queue projections carry `order_revision`. Do not reuse the journal `revision` as
an order revision. Submit returns the common queue reply with input ID,
conversation ID and order revision. Reorder returns the common queue reply with
the committed order revision. A stale reorder returns `stale_order = true` and the
current bounded queue projection with its current order revision. A successful
reorder returns `stale_order = false`. Reject malformed optional fields before
admission.
Submit and Reorder replies carry the full bounded same-project projection.
Submit also returns `accepted_input_id` so the client can correlate its local
request with one projected entry; an absent or mismatched identity rejects the
reply. Decision replies (Resume, Drop, promotion, Hold) carry the one decided entry.
Each entry's `sequence` is its immutable journal acceptance sequence for
history-baseline detection. Its `order_position` gives the committed lane order.
The service returns pending entries in that order; the TUI uses `order_position`
for presentation and move anchors.

The queue keeps at most 16 unresolved service inputs across all projects, each
at most 32 KiB. The client outbox keeps at most 16 local pending inputs and one
RPC in flight. Full capacity rejects a new Enter visibly and retains the draft.
The service never acknowledges before its journal flush. The existing 64 KiB
frame and 8 MiB journal caps remain; capacity or storage exhaustion rejects
without losing an earlier accepted input. The client removes only the outbox
row whose identity received a durable acknowledgement. It does not clear the
editor again. Subsequent editing creates another draft identity and is never
cleared by a late reply.

## Scheduling, dependency and steering

The service retains its single active model operation. When that operation
settles, the conversation owner selects one eligible lane head. It rotates among
project/conversation lanes that have eligible heads, starting after the lane last
dispatched. Within a lane, order is the last durable order. A held head blocks
later items in that lane until the user resumes or drops it; other lanes can run.
No operation from a lane runs concurrently with another operation from that
conversation. Model reservation, current configuration and complete-turn history
are checked only at dispatch. A queued input has no reserved model budget.

For Queue inputs, a committed terminal `Failed` or `Cancelled` outcome
settles the preceding operation. Queued successors and inputs submitted after
that terminal remain `Queued`; the service dispatches them in durable lane order.
The next prompt uses the normal history policy. A failed response is not reported
as a successful response or retried by this transition. An interrupted operation after owner
restart does not grant dispatch authority. Owner-generation replay, explicit Hold,
and uncertain journal settlement keep undispatched inputs `Held` until Resume or
Drop. Reordering does not erase an explicit or recovery hold. A held head blocks
later inputs in its lane; another lane can run.
A preparation failure may also hold an input before model admission. The service
publishes a bounded reason code with a Hold that it caused during the current
owner lifetime. The queue entry carries optional `hold_reason`; the client shows
it beside `Held`. The reason is diagnostic, not authority. It clears when Resume or Drop moves the input out of Held, or dispatch begins.
An exact decision replay preserves a newer Hold reason. An older Held entry after restart has no recorded cause and is
shown as `Held (reason unavailable)`; the client must not guess that a prior
model failure caused it. This adds no journal record or format change. The
service keeps at most 16 reason entries in memory and never logs prompt text.
While the service prepares the lane head before model admission, that Queued
entry also carries the live `waiting_step` (preparing or starting the model).
`/cancel` then sends decision action 4, an explicit Hold that the service
accepts only for the input it is preparing; the Hold cancels the helper and
records the current-owner reason `cancelled`. See
[waiting step display](model-provider-integration.md#waiting-step-display--owner-2026-10-16).
`Input accepted by service` means only that the queue write committed. The UI
must not present it as model admission. A Held row explains that the worker
will not dispatch it until the user resumes or drops it.
The storage authority derives this status from existing format-1 terminal and
input records. The service scheduler consumes that status; the TUI does not
infer or override it. The terminal-eligibility change adds no blocking call,
worker, queue, or journal record.
The existing 16-input capacity, eight-slot journal worker and cancellation-driven
model lifetime still apply. The writer has no deadline ([journal step J1-0](conversation-admission.md#journal-step-j1-0-journal-waits-without-a-time-bound)). An uncertain append fences
scheduling until the canonical writer resolves it. Replay and dispatch remain
idempotent under the existing request and owner-generation checks.

The existing IQ2 `Send now` action may select a `Queued` or `Held` input only
while an exact operation in the same conversation is active. The service
atomically promotes that input to Steer, checks the combined 32 KiB prompt,
then requests cancel-and-replace. The TUI says this stops and restarts the
response. If no active operation exists, `Send now` moves the item to the front
with a reorder request; it does not create a steering cancellation. A stale
target, conflicting steer or already dispatched input rejects and preserves
the queue. The TUI never implements steering as Drop plus a second queue input.

### Steering keys: Ctrl+S and Ctrl+T

Selected design (owner choice “One record”, 2026-10-17). Ctrl+T stages the draft
in the local outbox exactly as Enter does; it becomes one kind-18 queued input.
Ctrl+S requires an acknowledged active operation in the current conversation.
It stages the draft in the local outbox and marks the row with that steer target
(operation ID and generation). The serial sender submits it like any other row.
When the durable kind-18 receipt arrives, the TUI checks the target. If the same
operation is still active, it sends `ConversationQueueDecision` action 3 for the
accepted input and that exact target. The service commits one kind-16
`InputPromoted` record, then requests cancel-and-replace under IQ2. If the target
is no longer active, the TUI sends no promotion. The input stays Queued in lane
order, and the notice says that the turn ended before steering. A stale-target
promotion rejection also leaves the input Queued. A crash between the receipt and
the promotion leaves a Queued input; it never loses the text.

Selected flow. Arrows show key handling, requests and durable outcomes.

```mermaid
flowchart TD
    Key{"Key"} -->|"Enter or Ctrl+T"| Stage["Stage draft in local outbox"]
    Key -->|"Ctrl+S"| Target{"Acknowledged active operation in this conversation?"}
    Target -->|No| Keep["Keep draft; notice: no active target"]
    Target -->|Yes| StageS["Stage draft with steer target"]
    Stage --> Send["Serial sender: ConversationQueueSubmit"]
    StageS --> Send
    Send -->|"Rejected"| Rejected["Rejected local row keeps text"]
    Send -->|"Unconfirmed"| Unconfirmed["Keep request ID for exact retry"]
    Send -->|"Durable kind-18 receipt"| Marked{"Row has steer target?"}
    Marked -->|No| Queued["Queued in lane order"]
    Marked -->|Yes| Active{"Same operation still active?"}
    Active -->|No| QueuedNote["Queued; notice: turn ended before steering"]
    Active -->|Yes| Promote["Decision action 3: input, target, generation"]
    Promote -->|"InputPromoted durable"| Steer["Cancel and replace under IQ2"]
    Promote -->|"Stale target"| QueuedNote
```

### Admission and delivery — selected design

Arrows identify the durable acknowledgement and dispatch boundary.

```mermaid
sequenceDiagram
    participant U as TUI event loop
    participant C as Serial client sender
    participant S as Conversation service
    participant J as Journal writer
    participant M as Model helper
    U->>C: Enter: stage exact text and stable request ID
    U->>U: Clear editor and render provisional Pending local
    C->>C: Claim first row and lock its position while in flight
    C->>S: QueueSubmit(project, conversation, generation, request, text)
    S->>J: Validate and append InputQueuedV2
    alt Flush succeeds
        J-->>S: Input ID and order revision
        S-->>C: Durable queue receipt
        C-->>U: Replace Pending local with Queued
    else Rejection
        J-->>S: Definite rejection
        S-->>C: Error without acceptance
        C-->>U: Keep exact Rejected local row for later restoration
    else Reply lost or append uncertain
        S--xC: No definitive receipt
        C-->>U: Keep request ID and show Unconfirmed
    end
    opt Eligible head and no active model operation
        S->>J: Commit TurnAccepted for dispatch ID
        J-->>S: Durable operation identity
        S->>M: Start one admitted operation
        S-->>U: Running projection through subscription
    end
```

### Input state — selected design

Arrows name accepted actions or observed outcomes. `PendingLocal`, `Sending`
and `RejectedLocal` are client-only.

```mermaid
stateDiagram-v2
    [*] --> PendingLocal: Enter stages exact text and clears editor
    PendingLocal --> PendingLocal: Move unsent row locally
    PendingLocal --> Sending: Serial sender claims row and locks position
    Sending --> Unconfirmed: RPC or append outcome unknown
    Sending --> Queued: Durable service acknowledgement
    Sending --> RejectedLocal: Definite rejection with retrievable text
    Unconfirmed --> Queued: Exact request resolves accepted
    Unconfirmed --> RejectedLocal: Exact request resolves rejected
    RejectedLocal --> EditorRestored: Enter restores exact text to empty editor
    Queued --> Queued: Durable reorder
    Queued --> Queued: Earlier turn fails or is cancelled with committed terminal
    Queued --> Held: Owner restart or explicit Hold
    Queued --> Held: /cancel while preparing (action 4, reason cancelled)
    Queued --> Held: Preparation failure and publish current owner reason
    Held --> Queued: Durable Resume
    Queued --> Steering: Send now or Ctrl+S promotion with exact active target
    Held --> Steering: Send now with exact active target
    Steering --> Held: Unrelated cancellation or restart
    Steering --> Running: Steering settlement and dispatch
    Queued --> Running: TurnAccepted durable
    Queued --> Dropped: Durable Drop
    Held --> Dropped: Durable Drop
    Running --> Complete: Terminal success committed
    Running --> Failed: Terminal failure or cancellation committed
```

## Failure, restart and async limits

The client sender uses the existing one-slot queue transport worker. It holds
one in-flight request until a definitive reply or exact resolution. It does not
send later local inputs while an earlier outcome is uncertain; this preserves
the user's observed ordering. The request has no client budget and the worker
has no observation bound ([journal step J1-0](conversation-admission.md#journal-step-j1-0-journal-waits-without-a-time-bound)). The wait ends on the reply,
cancellation by the person or disconnect. Only cancellation or a lost connection
reports Unconfirmed; the worker slot is retained until settlement. Idle rendering, editing, project
navigation and quit remain responsive under a stalled service. Exit leaves
durably accepted inputs in the service. Exit with pending local inputs warns
that they were not accepted; it does not silently label them queued.

The service uses the existing eight-slot journal worker and one bounded
scheduler transition per reactor cycle. Filesystem replay, append and prompt
read remain on the worker. The service never performs them in its reactor.
The scheduler pauses on uncertain journal state; cancellation/status retain
reserved capacity. A crashed owner replays accepted inputs and order mutations.
It holds undispatched inputs from a prior owner generation until explicit
Resume, as IQ1 requires. A recorded `TurnAccepted` resolves to the same operation
and is never dispatched twice. A missing journal or graph binding does not
create a replacement queue. A queue subscription gives a bounded projection;
the TUI fences it by project, service epoch and order revision.

The PTY journey must verify that its executable is the isolated lifecycle
fixture before it launches a TUI or issues any service command. The fixture
must return a fixed probe response without accessing a runtime. If the probe
fails, the journey stops before fixture creation. This prevents a mistaken
production-binary argument from reaching the account's live service. The
positive probe and a production-binary rejection are required checks.

The restored-history admission journey uses a deterministic numeric-loopback
Ollama fixture through the actual packaged helper. It must not depend on System
model availability or model prose for its admission oracle. The fixture advertises
completion and an 8,192-token capacity, streams provisional text, then a recognized
final stop record with measured counts. A single retained test server allows at
most eight requests across two launches, 64 KiB per request, two-second socket IO,
60 seconds of server work and a three-second cleanup join. It serves no external
endpoint and emits no tool actions. Configuration is written only in the private
fixture home before launch. No helper identity check is bypassed.

The existing exact format-1 replay verification remains the oracle: both restored
launches must advance the same conversation once, with the expected generations
and original conversation identity. Streamed text provides deterministic UI
activity for history/draft interaction checks; it does not replace durable
admission proof. Each turn reaches its final state before its owned backend is
closed; all test services and the HTTP worker settle under the cleanup contract.
This refines MQ10 validation without changing production queue behavior.

```mermaid
flowchart LR
    Seed[Private canonical journal seed] --> Launch[Restore TUI and configure local fixture]
    Launch --> Queue[Submit exact next input]
    Queue --> Service[Canonical service admission and dispatch]
    Service --> Helper[Actual verified packaged helper]
    Helper --> Fixture[Bounded local provider streams then finishes]
    Fixture --> UI[Exercise history while draft is retained]
    UI --> Close[Wait terminal and close owned backend]
    Close --> Replay[Verify exact journal identity and generation]
    Replay --> Again[Repeat restored launch once]
```

For fairness, the scheduler selects each continuously ready lane once before
selecting any ready lane a second time. A lane leaves this set when its project
becomes unavailable or its head becomes Held. This is a scheduling invariant,
not a latency promise:
one model call may occupy the service until it completes or is cancelled.
Controls and cancellation must remain responsive during that work.
The queue panel shows at most four unresolved rows and an overflow count. It
uses the existing padded grey-blue panel style, bounded 256-byte excerpts and
explicit local, queued, held, steering, rejected and unconfirmed labels. Up
from the first editor row enters queue focus; arrows select a row. Enter sends
an accepted row now. While queue focus is active, `[` moves the selected
movable row one position up and `]` moves it one position down. These plain
characters work in terminals that do not report modified arrow keys. Hints
show `[ move up · ] move down`; `/help` spells out the same keys. An in-flight
row shows `Sending` and rejects either move. Enter on a rejected local row
restores its exact text into an empty editor. The panel closes when empty.

### Reorder and recovery — selected design

Arrows show exact-request and revision checks. The committed order is authoritative.

```mermaid
flowchart TD
    Move["[ or ] on selected queue row"] --> State{"Row state?"}
    State -->|"In-flight or terminal"| Reject["Reject move and keep position"]
    State -->|"Unsent local"| Local["Reorder local outbox without durable claim"]
    State -->|"Accepted input"| Retry{"Request ID already recorded?"}
    Retry -->|Same digest| Receipt["Return original result"]
    Retry -->|Different digest| Reject["Reject and keep projection"]
    Retry -->|No| Check{"Pending same-lane IDs and order revision match?"}
    Check -->|No| Reject
    Check -->|Yes| Flush["Append kind 19 and flush"]
    Flush -->|Durable| Project["Publish new order revision"]
    Flush -->|Uncertain| Fence["Fence scheduling and resolve exact request"]
    Project --> Ready{"Eligible head and idle?"}
    Ready -->|Yes| Dispatch["Commit TurnAccepted once"]
    Ready -->|No| Wait["Wait for state change"]
```

## Security and proof limits

The control API authenticates the attaching OS user and verifies project scope
before enqueue, reorder, inspect, resume, drop or promotion. IDs and excerpts
do not authorize cross-project access. An invalid foreign ID returns the same
non-disclosing error as an unknown ID. The service stores prompt text only in
the existing protected authority journal. The TUI does not write another queue
file. The Swift helper receives only an admitted prompt under the current
model and tool authority. New local inputs cannot silently route to a remote
model; provider selection remains governed by its own policy.

Runtime checks recorded in this document cover only their listed cases; the full
MQ matrix, fairness and visual quality remain unqualified. Power-loss durability
remains limited by the existing journal qualification evidence.

## Acceptance cases and delivery order

| ID | Initial state and trigger | Required result | Evidence |
| --- | --- | --- | --- |
| MQ01 | Idle service; Enter once, then rapidly enter two more messages | Each Enter stages exact text and clears editor immediately; all three get distinct durable queue receipts; one model operation at a time | Unit outbox/IDs; real service integration; PTY end to end |
| MQ02 | Backend stalls before first queue reply; user edits and quits | Renderer and keys remain responsive; in-flight ID stays stable; provisional crash-loss risk is visible; no false durable claim | Unit worker cancellation with no timer; stalled service integration; PTY end to end |
| MQ03 | Two accepted pending inputs; move second to front | One kind-19 mutation; new revision and dispatch order persist after restart | Replay unit; real journal integration; service and PTY end to end |
| MQ04 | Concurrent reorder clients use same order revision | Exactly one wins; loser gets stale revision without losing text | Unit CAS; service process integration; two-client end to end |
| MQ05 | Same reorder request retried after lost reply | Original position/result returned; no second move or dispatch | Codec/replay unit; fault-injected journal integration; control-client end to end |
| MQ06 | Operation active; select same-conversation queued item to send now | IQ2 promotion commits before cancellation; one replacement; successors stay ordered behind it unless explicitly held; never `repair_required` (regression IQ2-R1) | Writer unit target/bounds; real service/helper integration; PTY end to end |
| MQ07 | A turn fails or is cancelled with a committed terminal; one successor is already queued; then one new input arrives | Existing and new inputs stay Queued and dispatch in durable lane order, without duplicating the failed turn | Replay and scheduler unit; real journal/service integration; PTY end to end |
| MQ07R | Owner restarts with undispatched inputs or journal settlement is uncertain | Inputs remain Held; no automatic retry; unrelated lanes progress; explicit Resume or Drop is needed | Replay unit; restart journal integration; PTY end to end |
| MQ07H | Provider preparation fails before admission | Input becomes Held with a reason code; queue pane explains required Resume; no model turn is fabricated | Service unit and queue reply integration; PTY held-row rendering |
| MQ08 | 16 unresolved inputs, local outbox full, or 8 MiB journal full | Definite rejection preserves draft and existing queue; controls remain responsive | Bounds unit; fault-injected service integration; PTY end to end |
| MQ09 | Two ready lanes across projects, one busy model | Scheduler rotates after each terminal; no lane starves under the stated invariant | Scheduler unit; two-project service integration; control-client end to end |
| MQ10 | Reconnect after dispatch and owner crash | Exact input maps to one operation; historical queue rows do not lower restored cursor | Replay unit; restart integration; relaunch PTY end to end |
| MQ12 | First outbox row is in flight; later input is edited; service rejects first | In-flight row cannot move; exact rejected text remains retrievable without overwriting newer draft; unsent rows can move | Unit state/focus; stalled service integration; PTY end to end |
| MQ13 | Operation active; Ctrl+S with a draft | One kind-18 input, then one kind-16 promotion for that input and target; target cancels and the replacement runs once (IQ2-S1) | TUI unit staging and promotion request; service integration; PTY end to end |
| MQ14 | Ctrl+S row receipt arrives after its target already settled | No promotion is sent; the input stays Queued in lane order; notice explains | TUI unit; service integration |
| MQ15 | Ctrl+T with a draft, idle or busy | Same result as Enter: one kind-18 input in lane order | TUI unit; PTY end to end |
| MQ16 | Journal contains a kind-12 record | Replay rejects the journal as unsupported; no conversion | Codec/replay unit |

First implement codec, replay and typed wire additions. Then implement service
admission, order mutation and scheduler under the existing journal writer. Last,
add client outbox, serial sender, compact panel, move interaction and terminal
tests. Run the mandatory CLI lifecycle gate and the native service process suite.
Use a real macOS terminal for final layout and stalled-backend checks. Record
which cases ran; mock success does not prove service durability or model results.


### Recorded queue and navigation checks — 2026-09-29

The isolated process journey verified the preparation Hold reason, a second Hold
following Resume, and retention of that reason on exact Resume replay. Restart
retained the input and omitted the transient reason. The full CLI lifecycle and
PTY suite passed with a matching client/helper package. It covered input recall,
Shift+Up/Down history navigation, two restored-history admissions, queue reorder,
durable replay and owned-process cleanup. The account's live backend was unchanged.

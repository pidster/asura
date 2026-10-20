# Composer interactions

Status: selected design for the owner's composer changes, 2026-09-28, as
revised by the [managed input queue](managed-input-queue.md) on 2026-09-29.
Earlier scoped validation is recorded below; it does not validate the revised
all-input queue interaction.

## Ownership and scope

The TUI owns focus, selection, draft history and presentation. Existing project,
configuration, model inventory and conversation workers own transport. The service
owns queued inputs and steering. See [IQ2](conversation-admission.md#existing-input-promotion-iq2)
for atomic queue promotion and [editor navigation](composer-editor-navigation.md)
for bounded local recall. No rendering or key handler performs network or file I/O.
Protocol numbering remains 0.1 and journal format remains 1.

## Layout and interaction

Add one bottom row below the status row. Use mid-grey text on the terminal
background. This row describes interaction state and the keys available at the
current focus. Keep input blue exclusive to input, and use the existing grey-blue
panel colour for lists and queue display. All backgrounds include internal padding.
Focused status items and selected list or queue rows use a lighter grey-blue
background (RGB 70, 82, 94) and brighter text (RGB 245, 248, 250).
Do not invert foreground and background colours for selection.

Down from the final visual input row enters status focus when input recall is
not active. Ctrl+Down remains a shortcut from any input row.
Project is the initial status item. Left, Right and Tab move between project and
model. Up or Escape returns to the editor without changing the draft. Enter opens
the selected list. Project displays its name alone, without a Project prefix.

Lists appear below the status row; the composer moves upward to reserve list space.
The bottom hint row remains visible. Lists have a bounded viewport, scroll only
when their rows exceed it, and keep the highlighted row visible. Up/Down select,
Enter applies, and Escape closes without applying. Very small terminals keep the
editor and hint visible and may clip the list to remaining space. No subtraction
may overflow when the terminal shrinks.

The project list uses the client's project window of at most 64 entries and
fetches the next or previous page at its edges
([CF06b](command-flows.md#cf06b-project-list-and-local-selection)); the number
of projects has no total (owner, 2026-10-19). It shows stale entries
as unavailable. Both `/project select` and list selection use the same transition.
An active conversation blocks switching projects under the existing contract.
A successful change resets conversation and telemetry scope and preserves the draft.

The model list uses the existing bounded `/models` inventory worker. Opening the
list starts one discovery request; pending, empty, partial and failed discovery
remain visible and cancellable. Available, installed and listed rows can be selected.
Unavailable or unchecked rows explain their status without applying. Enter writes
the selector through the canonical `model` config setter. The UI adopts it only
after success. Failure preserves selection and draft. This affects future admissions;
it does not change a running model. The picker never loads weights or starts inference.
Picker-origin requests must not clear drafts or open command-result overlays.

Key routing between the editor, queue, log and status bar follows the
[focus chain](composer-editor-navigation.md#focus-chain-and-draft-movement--selected-2026-10-13).
In summary, plain Up/Down move between draft rows and recall input at the
edges. Shift+Up/Down move focus up to the queue and log, or down to the status
bar. Ctrl+Up/Down remain queue and status shortcuts, and Ctrl+P/N remain recall
aliases. With the log focused, Up, Down, Page Up, Page Down, Home and End scroll
it, and Escape returns to the editor. The log retains its scroll position while
focused and follows the latest turn again when focus returns to the editor.
These local operations perform no I/O.
Hints expose these interactions. Rendering and key handling perform no I/O.
The hint row uses `^` for Control chords and `↵` for Return. On macOS it
uses `⌥` for Option; on other systems it writes `Alt+`. It shows the newline
chord only when the editor can accept it. These labels describe existing key
events and do not change their handling. If a terminal is narrow, the existing
hint clipping applies; the full key list remains available through `/help`.

## Input styling and command highlighting — selected 2026-10-13

Required behavior (owner, 2026-10-13): adopt four details of Wisp's prompt box
and highlight active slash commands. Revised the same day: a marker colour change
was too subtle, so slash commands show only the text highlight. Command amber
`#E8B577` as a box tint is reserved for the person-run `!` command mode.

Required behavior (owner, 2026-10-13): the `!` and `?` modes tint the whole input
box, including its half-block strips. The
[person command mode](person-command-mode.md) and [help mode](help-mode.md)
designs own each mode's behavior; this section owns their presentation.

| Element | Selected presentation |
| --- | --- |
| Prompt marker | `›` in glow `#CFF1FF`, bold, on the input tint `#253B4E` |
| Typed text | White `#E6EDF0`, bold; selection keeps the existing reversed style inside the editor |
| Marker placement | Only on the first visual row of the draft. When the draft has scrolled, no row shows the marker. |
| Slash commands | The box and marker keep their normal colours while the draft starts with `/` |
| Active command | When the first word exactly names a TUI command, that word uses command amber `#E8B577`, bold. Other words and unknown names keep the typed-text style. |
| `!` command mode | Box and strips in command amber `#E8B577`; marker `!` and text in bold black `#000000`. A sent command's log line is a stripe in `#745A3C` with light text. |
| `!!` outside mode | Proposed with [person command mode](person-command-mode.md#outside-mode-in-the-composer) PCR26 (owner, 2026-10-19): box and strips in outside red `#D27068`; marker `!!` and text in bold black `#000000`; hidden marks in bold black. A sent command's log line is a stripe in `#693834` with light text, followed by a warning line in `#FF6B6B`, bold. Values from Wisp's palette. |
| `?` help mode | Box and strips in help indigo `#252D4E`; marker `?` in glow `#CFF1FF`, bold; text in the typed-text style |
| Context gauge | `N% input` stays muted below 50%, uses glow `#CFF1FF` from 50% and amber `#F2B950` from 80%. The model name stays muted. |

The values come from Wisp's palette (`tools/wisp-tui/src/palette.rs`), except help
indigo, which the owner chose from rendered candidates on 2026-10-13. The TUI
defines them once as named composer roles instead of repeating literals. Moving
the remaining hard-coded colours in `ui.rs` into those roles is separate work.

Validation: buffer tests check marker style and placement after scrolling, the
unchanged marker for a slash draft, highlighting of a known and an unknown command, and each
gauge threshold. The existing terminal journeys must still pass.

## Hidden characters — selected 2026-10-14

Required behavior (owner, 2026-10-14,
[PX5](execution-threat-model.md#person-command-decisions)): in both message and
`!` command mode, the input box shows each hidden character as a full block `█`
in hidden-mark red `#B3261E`. The hint row adds `N hidden characters`, so colour
is not the only signal.

Hidden characters are C0 and C1 controls other than newline and tab, U+00AD,
U+180E, U+200B–U+200F, U+2028–U+202E, U+2060–U+2064, U+2066–U+2069 and U+FEFF.

The editor keeps the real characters. A message is sent, and a command runs,
with the bytes as typed. Only the view changes: each hidden character takes one
cell, and cursor movement steps over it as one character. `HIDDEN_MARK` joins
the named composer roles.

Validation: buffer tests render each listed character as a red `█` in both
modes and confirm the draft bytes are unchanged; a hint test counts them; a
PTY journey pastes text with U+202E and U+200B.

## Queue interaction

Enter stages every conversational message in a bounded local outbox, whether a
model operation is active or idle. It clears the editor as soon as the exact
text and a stable request ID have a local row. A serial worker sends rows to
the service queue one at a time. The service's durable receipt replaces the
provisional row with its accepted projection. A rejected or uncertain row keeps
its exact text and request identity without overwriting a newer draft. Slash
commands remain immediate control operations. The [managed queue contract](managed-input-queue.md)
defines the limits, receipts, retry and crash-loss disclosure.

Show a padded queue panel above the composer with `• Queued messages:` and rows
`>> input excerpt`. Display accepted rows from the bounded service projection
and provisional rows from the local outbox with distinct labels. Label held,
sending, unconfirmed and rejected rows explicitly. Display at most four rows
and an overflow count; scroll the selected item into view. Running and terminal
rows leave this panel.

Ctrl+Up from the editor enters queue focus at the last unresolved input.
Up/Down traverse the queue; Down past its last item returns to the editor. Escape
also returns. `[` and `]` move a selected unsent local row one position in the
local outbox. For an accepted row, they request a same-lane durable move
with the observed order revision. In-flight, running and terminal rows cannot
move. A stale order result refreshes the projection without claiming
the move succeeded.

Enter on a queued or held accepted input sends it now. With an exact active
operation in the same conversation, the canonical service atomically promotes
the same input to Steer. The hint says this stops and restarts the response.
Without an active operation, Enter requests a move of a queued input to the
front; a held input does not move, and the notice asks for `/queue resume ID`.
Never combine client Drop and a second queue input. If the
target is stale or the input was dispatched, show rejection and keep the
projection.

Ctrl+T stages the draft in the local outbox exactly as Enter does. Ctrl+S
requires an acknowledged active operation in the current conversation; it
stages the draft with that steer target. After the durable kind-18 receipt, the
TUI promotes the accepted input with decision action 3 if the target is still
active. Otherwise the input stays Queued and the notice explains that the turn
ended before steering. Without an active target, Ctrl+S keeps the draft and
shows a notice. The [steering keys contract](managed-input-queue.md#steering-keys-ctrls-and-ctrlt)
owns this rule. Enter on a rejected local row restores its exact text only when
the editor is empty. The UI keeps uncertain decisions for `/retry`; success
does not clear an unrelated draft. Changed project/epoch invalidates stale
focus and projections.

## Focus flow

Selected design. Arrows describe key events and completed worker outcomes.

```mermaid
stateDiagram-v2
  [*] --> Editor
  Editor --> Status: Down on final input row after recall ends, Shift+Down or Ctrl+Down
  Status --> Editor: Up or Escape
  Status --> ProjectList: Enter on project
  Status --> ModelList: Enter on model
  ProjectList --> Status: Escape or successful selection
  ModelList --> Saving: Enter on selectable model
  Saving --> Status: Configuration saved
  Saving --> ModelList: Rejection, failure or cancellation
  ProjectList --> ProjectList: Stale or busy selection rejected
  ModelList --> Status: Escape cancels discovery
  Editor --> Editor: Up/Down move rows or recall, Ctrl+P/N recall
  Editor --> Queue: Shift+Up or Ctrl+Up with queued inputs
  Editor --> History: Shift+Up with no queue and visible turns
  Queue --> History: Shift+Up, or Up on first row, with visible turns
  History --> History: Up/Down, Page keys, Home or End scroll
  History --> Queue: Shift+Down with queued inputs
  History --> Editor: Escape, or Shift+Down with no queue
  Queue --> Editor: Escape, Shift+Down or Down past last row
  Queue --> Queue: Move, send now, acknowledge or reject
```

## Mutation flow

Selected design. Arrows show dispatch, failure and acknowledgement boundaries.

```mermaid
flowchart TD
  Key[Enter, Ctrl+T or Ctrl+S] --> Focus{Focused element}
  Focus -->|Editor, conversational input| Stage[Stage exact text in bounded local outbox; Ctrl+S marks steer target]
  Stage -->|Serial sender claims row| Enqueue[Typed queue submit with stable request ID]
  Enqueue -->|Receipt for Ctrl+S row and target still active| Promote
  Focus -->|Queued item and active same-conversation operation| Promote[Promote same input with exact target]
  Focus -->|Queued item and idle service| Reorder[Move input to front with order revision]
  Focus -->|Model row| Config[Existing config worker]
  Focus -->|Project row| Project[Existing guarded project selection]
  Enqueue --> Result{Durable result}
  Promote --> Result
  Reorder --> Result
  Config --> Result
  Result -->|Success| Apply[Update projection, preserve unrelated draft]
  Result -->|Rejected| Retain[Keep exact rejected local text or service row]
  Result -->|Uncertain| Retry[Retain exact request for retry]
  Project -->|Current and idle| Apply
  Project -->|Stale or busy| Retain
```

## Limits, recovery and tests

Reuse the model inventory worker, configuration's existing bounded worker and the
queue worker. Model discovery and configuration saves have no client time budget
(owner, 2026-10-16). Implemented 2026-10-16: the inventory and configuration
workers carry no job deadline; `/cancel` reports at once and the slot stays busy
until the worker settles (`tui/models.rs` and `tui/config.rs` unit tests). The queue worker has no
observation bound ([journal step J1-0](conversation-admission.md#journal-step-j1-0-journal-waits-without-a-time-bound)). Each wait ends on reply, cancellation by the
person or disconnect. Each worker retains its slot until settlement. Escape suppresses late picker results. Quit uses existing bounded
worker cancellation and owned-service cleanup. No new polling or timer is added.
History is volatile, at most 100 entries and 1 MiB. List projections remain bounded
by existing 64-project, 65-model and 16-unresolved-input service limits. The local
outbox has its separate 16-input cap. Exit warns when local rows lack a durable
receipt; an accepted service input survives client exit.

Unit checks cover modifier routing, history boundaries, focus transitions, narrow layouts, hint
states, list scrolling, stale selection, draft preservation, model save rejection,
history pane scrolling, exact unsent draft restoration, command recall and default queue admission. Backend checks cover durable promotion,
exact retry, stale targets, duplicate steering, size rejection and restart hold.
Integration and real terminal checks exercise project and model selectors, local
staging, serial delivery, durable reorder, promotion and history while requests
are pending. Verify editor/quit responsiveness with stalled discovery and queue
delivery. [MQ01–MQ16](managed-input-queue.md#acceptance-cases-and-delivery-order)
define the new queue unit, integration and end-to-end proof. Root runs serial
Cargo/Swift checks and builds the executable.
Render and inspect both diagrams and terminal previews before reporting completion.

## Recorded validation

Historical checks before the all-input queue change: on 2026-09-28, all 115 CLI unit tests passed, including seven composer interaction
tests. The real composer PTY journey passed project and model selection, confirmed
configuration saves, draft preservation, history and owned-service cleanup.
Mermaid diagrams and terminal previews were rendered and visually inspected.
The revised non-inverted selection colours were inspected in status, project-list
and queue previews. This visual check does not claim identical font rendering in
every terminal application.

## Queue history and the active conversation cursor

The first full queue snapshot is a history baseline, not a request to resume all
past work. A terminal entry in that baseline must not select its conversation or
replace the current generation. A running entry remains eligible for observation.
Queued and held entries are remembered for later transitions. A subsequent entry
with a sequence above the baseline, or a previously observed unresolved entry that
finishes between polls, remains eligible. Explicit mutation acknowledgements also
retain eligibility, including when completion precedes the first full snapshot.

The client retains a sequence watermark and at most 16 eligible input identities
for each project/service-epoch scope. Subscription retries and missed-heartbeat
display staleness do not reset this history baseline; a changed project or service epoch does.
Existing 32 observed-operation identities still suppress duplicate transcript rows.
No helper or storage call is added to the render/input path. Scope, source and
revision fences remain owned by the existing queue projection. Explicit queue jobs
capture the project and last known service epoch at dispatch. Connection loss alone
does not invalidate an acknowledgement or an unconfirmed request identity. A reply from another scope must
not change the projection, baseline, editor or cursor. A terminal stale reply only
releases its worker state; a staged local row retains its exact text and request
identity without overwriting the editor.

Before observing an eligible entry, compare its conversation and generation with
the active client cursor. Never replace a cursor with an older generation of the
same conversation. Historical queue entries do not select the current cursor.
The separate [conversation restoration read](conversation-restoration.md) selects
the latest accepted conversation for the project, even when the queue is empty.
Service generation validation stays mandatory.

```mermaid
flowchart TD
    Snapshot[Validated queue snapshot or mutation acknowledgement] --> Scope{Same project and service epoch?}
    Scope -->|No| Reject[Discard stale reply without changing history]
    Scope -->|Yes| Baseline{First full snapshot?}
    Context[Selected project or service epoch changes] --> Reset[Reset queue baseline and eligible identities]
    Baseline -->|Yes| History[Remember sequence watermark; terminal history is not eligible]
    Baseline -->|No| New[Track new sequence or known unresolved input]
    History --> Running[Running or explicitly acknowledged work remains eligible]
    New --> Running
    Running --> Ready{Eligible dispatched operation and client idle?}
    Ready -->|No| Retain[Retain bounded projection]
    Ready -->|Yes| Older{Same conversation and older generation?}
    Older -->|Yes| Ignore[Keep active cursor unchanged]
    Older -->|No| Observe[Observe exact operation once through existing worker]
```

Historical regression fixture: queued work targeting generation
2 completes as generation 3, then a later turn advances that conversation to 4.
A fresh TUI restores generation 4 from the journal read. It must not adopt
generation 3 from the queue. Also cover running
work on the first snapshot, queued-to-complete between polls, new terminal work
after an empty baseline, reconnect/display staleness, mutation acknowledgement before
the baseline, duplicate snapshots and a same-conversation generation regression.

The prior native PTY regression created generations 1 and 2, queued generation
3, then submitted direct generation 4. The queue-first version must submit
both later inputs through the durable queue. It restarts the owned service and
fresh TUI against the same isolated journal, then requires another real answer
without conflict.
The fixture has a 240-second work deadline and its existing separate cleanup budget.

The earlier deterministic lifecycle fixture seeded queued generation 3 and later
direct generation 4. It expected two fresh TUI launches to start new conversations
at generation 1. That expectation is superseded by conversation restoration.
The updated fixture must verify both launches advance the restored conversation.
The actual packaged helper may run, but model completion is not this test's oracle.
Each TUI shuts down its owned service and helper before verification. This coverage
has a separate 75-second selector budget and the existing independent cleanup budget.

### History-baseline verification — 2026-09-28

The serial integration runner verified:

- CLI unit suite: 124 tests passed, including historical terminal exclusion,
  active queue observation, fast completion, delayed snapshots, generation fencing,
  project/epoch changes, and acknowledgement retention across real unavailable views.
- `cargo test --locked -p asura-cli --test lifecycle` passed. The canonical
  generation-3 queue/generation-4 direct fixture completed two fresh TUI admissions.
  Strict replay verified absent original conversation and expected generation zero
  for both inputs. Each owned service and helper stopped before replay verification.
- CLI Clippy for all targets with warnings denied passed.
- Both history diagrams rendered with Mermaid CLI 12.0.0 and were visually inspected.

The optional native answer journey failed on its first prompt with `outputLimit`,
before the queue scenario. It does not provide native answer success evidence for
this correction. The passing lifecycle case proves durable admission and cleanup;
it does not claim successful model generation. No user journal or database was changed.

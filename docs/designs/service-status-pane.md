# Service status pane

Status: selected implementation packet, 2026-09-28; implemented. Source tests
for each validation layer are listed under [Validation](#validation). This
document records no test run or runtime evidence. Protocol remains 0.1.

Time-bound update (owner, 2026-10-16): the stored-size sample has no time limit.
The pane shows the size as measuring until the sample returns. The code still
applies a two-second limit; a follow-up implementation will remove it.

The top pane reports `Asura · Connected (process|local|remote) · uptime · memory:
status size · graph: state reason`. Process means this TUI launched the current
service with its lifetime lease; local means it attached to an existing Unix-socket
service. The canonical `start_owned` result decides ownership, including startup
races. No remote service transport exists yet; do not label remote model inference
as a remote service. Retain ownership only for the same service epoch.

## Owners and measurement

The existing service observer owns status publication and one retained metadata
worker. The storage adapter owns the stored-memory size operation; platform owns
its descriptor-relative traversal. This does not open another database engine or
read records, prompts, logs, configuration values or model assets for measurement.

Stored size means the sum of regular-file logical lengths below the validated
embedded database directory. It includes engine indexes, manifests and write-ahead
logs, and excludes config, logs, sessions, scratch and model assets. It is neither
RAM usage nor allocated disk blocks. Files can change during an engine update, so
this is a bounded sampled footprint, not a transactional database snapshot.

Open the database directory without creation. Traverse no more than 20,000 entries,
64 directories and eight nested directory levels. Check cancellation between
entries. Never follow symlinks, hard-linked files or special files. Validate
owner, non-writable-by-others mode, ACL and held/named directory identity. Read
metadata only, sum checked u64 lengths, and discard the whole measurement on error
or overflow. Pin and revalidate ancestor descriptors; memory remains bounded by the
traversal stack and counters. Database directories/files can have engine-created
owner-readable modes; do not rewrite their permissions.

The retained observer worker samples on startup, configuration refresh and no more
than once every 30 seconds otherwise; this cadence is pacing. A sample has no time
limit (owner, 2026-10-16). While it runs, the pane shows the size as measuring
(`size_reason` 2, pending) with the last valid byte count and sample time marked
stale. The sample ends on completion, failure, cancellation or shutdown, and an
unfinished worker slot is never released. A refresh due while a sample runs
coalesces into the next sample. Never replace an unknown value with zero. A failed worker may be replaced only after actual thread
settlement and the next bounded refresh. Cached graph readiness comes from the
existing verified authority installation observation; a successful footprint scan
cannot establish database availability.

Uptime is monotonic elapsed time since the service observer was constructed during
reactor startup. Each response carries server-sampled milliseconds. It is not TUI
age or wall-clock subtraction. The client can display the latest lower bound, or
advance it locally only while the same live service epoch remains current.

## Observation fields

Extend ServiceObservation with optional uint64 uptime_ms field 5 and
StoredMemoryStatus stored_memory field 6. New fields are required in current
service replies; no version increment is made. StoredMemoryStatus has:

| Field | Type | Meaning |
| --- | --- | --- |
| available 1 | bool | Existing GraphReady and GraphVerified pair |
| size_bytes 2 | optional uint64 | Last valid sampled byte sum |
| sampled_uptime_ms 3 | optional uint64 | Server uptime when that sample finished |
| stale 4 | bool | Size is absent or refresh failed/expired |
| size_reason 5 | uint32 | 1 ready, 2 pending (measuring), 3 timeout (retired for new samples, owner 2026-10-16; schema unchanged), 4 unsafe, 5 unavailable/IO, 6 limit, 7 cancelled |

Size and sample time are present together; sample time cannot exceed reply uptime.
Ready requires a present size and stale=false. Other reasons require stale=true.
Availability must match the canonical installation pair in the same observation.
Graph text uses that same pair directly, including unavailable/repair outcomes.
Client ServiceUpdate exposes uptime_ms and stored_memory; clients perform no file
scan. Ordinary one-second retained observation heartbeats carry current uptime
without adding a status revision or creating new workers each second.

```mermaid
flowchart TD
    Timer[Startup / config event / 30-second refresh] --> Free{Metadata worker actually settled?}
    Free -->|No| Retain[Retain worker; serve cached observations]
    Free -->|Yes| Sample[One retained worker reads config and bounded DB metadata]
    Sample --> Result{Sample returned?}
    Result -->|Not yet| Measuring[Publish measuring with stale last size]
    Measuring --> Status
    Result -->|Yes| Valid{Validated complete footprint?}
    Valid -->|Yes| Cache[Cache bytes and monotonic sample time]
    Valid -->|No| Stale
    Graph[Canonical verified installation state] --> Status[Combine availability, size and uptime]
    Cache --> Status
    Stale --> Status
    Status --> Observe[Existing retained service observation]
    Observe --> Client[Epoch-fenced client pipeline]
    Client --> Pane[Pure top-pane rendering]
```

## Client notice placement

The owner's 2026-09-30 correction places the existing transient client notice at
the top right of this pane. It shares the first row with the backend header; it
does not reserve a row above the composer. Keep the existing one-cell horizontal
pane padding, washed blue-grey background and bottom interaction hints. Paste
capture instructions use this same notice location when no other notice exists.

Rendering remains a pure projection of `App.notice` and the delivered service
view. Sanitize notices as terminal literals and clip by grapheme display width.
When a notice is present, reserve no more than half the usable row for it and a
two-cell separation; clip the left backend header to the remaining cells. Align
the notice to the right padding. An empty notice leaves the full width available
to the header. Resizing cannot overwrite the left header or create another row.
There is no new IO, worker, queue, timer or keyboard route. Draft editing, status
focus, selectors and the lower hints retain their current behavior. Overlays and
history use the space released by the removed notice row.

```mermaid
flowchart LR
    State[Client notice or paste capture hint] --> Literal[Sanitize bounded literal]
    View[Delivered backend status] --> Header[Format existing header]
    Literal --> Layout[Partition one padded row by display width]
    Header --> Layout
    Layout --> Left[Clipped backend header at left]
    Layout --> Right[Clipped notice aligned right]
    Editor[Editor and interaction state] --> Composer[Composer and bottom hints unchanged]
```

SS-U4 renders empty, ordinary, long and Unicode/control-bearing notices at wide,
compact and minimum supported dimensions. Assert top-row alignment and palette,
left-header preservation, no composer notice row, unchanged draft and bottom
hints. SS-E2 uses the isolated real PTY composer journey: trigger a notice, assert
it appears only on the first row, then open selectors and verify draft retention.
Rendered buffers verify geometry and colors; PTY evidence verifies the actual
event route and terminal cleanup, without claiming native terminal appearance.

## Validation

SS-U1 checks footprint byte sums, nested limits, symlink/hardlink/FIFO rejection,
replacement and cancellation using private scratch roots. SS-U2 checks
strict wire presence, state/reason combinations, paired size/time, future timestamps
and graph-availability agreement. SS-U3 holds a worker for a long sample and checks
the measuring state, nonblocking cached replies, no replacement until settlement, retained stale bytes,
30-second cadence and uptime continuity across client reconnects. SS-I1 uses a real
embedded database under scratch, obtains service observations and verifies stored
size against bounded fixture files while a separate client attaches. SS-E1 checks
TUI owned/local labels, epoch reset, uptime and graph status without render IO.
Root owns integration, serial builds and runtime qualification; source tests alone
are not end-to-end evidence.

Implemented test owners. SS-U1 is in `asura-platform/src/journal_io/footprint.rs`.
SS-U2 is `stored_memory_status_is_bounded_truthful_and_paired_with_service_uptime`
in `asura-control/tests/protocol.rs`. SS-U3 is in
`asura-service/src/service_observer.rs`. SS-I1 is `asura-storage/tests/stored_memory.rs`.
SS-U4 and header rendering are `asura-cli/src/tui/ui.rs` unit tests. SS-E1 and
SS-E2 assertions are in `asura-cli/tests/tui_pty.py`. This list identifies
source tests only; it does not claim that they passed.

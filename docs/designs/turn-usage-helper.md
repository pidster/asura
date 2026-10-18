# Per-request inference usage in the Swift helper

Status: reviewed scoped design and integrated implementation. The Swift helper
suite and Rust/Swift fixture exchange have passed. Parent-owned final service,
terminal and native qualification are recorded in the response activity design.
The parent owns canonical schema, generated bindings, Rust collection, client
display, documentation indexes and serial validation.

Owner requirement, 2026-10-13: token limits must not be hard-coded. The fixed
token figures in this design describe the current implementation and will be
replaced by [model-sized limits](loop-closure.md#model-sized-limits).

Owner decision, 2026-10-18: “We found these types of tool and turn limits to be
profoundly limiting in Wisp.” The helper no longer limits the number of requests
in a turn and no longer keeps a turn output-token budget. The 64-request cap, the
2,048-token turn total, its refunds and the durable turn reservation are removed.
Each request keeps its own output cap. See [request caps and admission](#request-caps-and-admission).

## Scope and ownership

The Swift helper observes actual SDK executor requests in `BoundedToolModel`.
One turn-local observer assigns consecutive request ordinals and settles reports.
Provider adapters supply measured counters from the request they execute.
`HelperSession` publishes each report through the existing private transport.
It does not coalesce reports with replacement response text.

This packet adds no scheduler, model invocation, tokenization call, provider probe,
filesystem read, dependency or public tool. Context occupancy, request token
caps and differences between session totals cannot supply usage counters.
The protocol remains 0.1; the journal remains format 1.

## Installed API evidence

The installed macOS 27 FoundationModels Swift interface exposes executor request
identity and asynchronous dispatch. `LanguageModelExecutorGenerationChannel.Event`
is opaque: its public interface provides factories without readable action fields.
The helper cannot inspect generic executor token usage through that channel.
`LanguageModelSession.Usage` supplies session-level data; this packet does not use
that data to infer individual request costs.

The pinned MLX adapter's readable generation observer is internal and test-only.
The pinned CoreAI adapter emits usage through the opaque SDK channel.
Neither adapter exposes a supported per-request usage callback in this packet.
System, CoreAI and MLX therefore report unknown counters. Changing dependencies
or using private APIs is outside this packet.

The Ollama adapter owns each HTTP request and its final decoded record.
`prompt_eval_count` measures input; `eval_count` measures output.
Each counter must come from the final recognized record for that request.
Ollama does not report cached input tokens here. The adapter must preserve unknown
cached usage rather than use its existing synthetic SDK cached count of zero.

## Request and report contract

Each executor invocation admitted under the request caps receives one ordinal,
starting at 1. The observer holds at most one active UUID, ordinal and counter tuple.
Overlapping invocations fail with the existing protocol failure; no second lane
or unbounded map is created. No request count or turn token total limits admission
(owner, 2026-10-18). The 200-call tool limit of
[tool call records](tool-call-records.md#size-limits) still applies.

`Snapshot` gains an optional usage report and an optional request-start ordinal.
These callbacks carry empty text and no text usage or context measurement.
`HelperSession` routes them before the replacement-text path. A start callback
records the active request but emits no wire message. This marker lets cancellation
settle an active request before the terminal frame.

The parent adds private envelope field 23, `InferenceUsage`, with:

| Field | Meaning |
| --- | --- |
| request_ordinal | Consecutive request identity within the operation, starting at 1; any `uint32` value |
| input_tokens | Optional actual request input count, at most 1,000,000,000 |
| output_tokens | Optional actual request output count, at most 1,000,000,000 |
| cached_input_tokens | Optional measured cached input count; requires input and cannot exceed it |
| complete | True only when successful request settlement has both input and output counts |

Zero is a measured value. Absence means unknown. Cached input may remain unknown
on an otherwise complete report. Missing counts or failed requests require
`complete = false`. Failed requests retain available valid counts and never invent
missing counts. Invalid provider counts cause provider protocol failure and an
incomplete report with those invalid counts omitted.

Reports carry the existing operation and generation fence. The session enforces
consecutive ordinals and at most one unsettled request. The ordinal is a `uint32`.
A request after ordinal 4,294,967,295 fails with the protocol failure; this is a
known-size exhaustion that a turn does not reach in practice. The session rejects
duplicates, missing start markers, invalid bounds and reports after terminal.
A normally completed request emits exactly one report before the next request
starts. No additional inference runs to collect missing statistics.

## Request caps and admission

Selected design (owner, 2026-10-18). The request wrapper admits any number of
requests in a turn. It keeps no turn output-token total, refunds nothing and has
no remaining-budget check. The 64-request cap and the 2,048-token turn budget are
removed. A turn ends by [loop closure](loop-closure.md#current-loops): the model
completes, the 201st tool proposal is recorded, the person cancels or steers, the
context window fills, the provider fails, the helper exits or the service stops.

Each request keeps its own output cap. The caller's cap is the `Begin`
`max_response_tokens` value. The service sends its per-request ceiling,
`MAX_RESPONSE_TOKENS` (2,048); the wire accepts 1–2,048.

- **Tool-enabled generation:** the first request has a cap of at most 1,024 tokens.
  Each later request has a cap of at most 512 tokens. The actual cap is the minimum
  of that value, the caller's cap and the cap that the SDK requests.
- **Plain text:** one request has the minimum of the caller's cap and the SDK
  request cap. A second plain-text dispatch fails with the output-limit result
  before provider I/O.

The usage observer still checks each measured output count against that request's
cap. A count above the cap is a provider protocol failure. A measured count no
longer releases tokens, because no turn total exists. Per-request usage reports
remain; the service sums them for the turn statistics display.

### Request admission — selected design

Arrows show admission decisions within the existing executor. No path counts
requests or tokens across the turn.

```mermaid
flowchart TD
    Request[SDK request] --> Failed{Earlier failure recorded or cancelled?}
    Failed -->|Yes| Reject[Reject before provider dispatch]
    Failed -->|No| Mode{Tools enabled?}
    Mode -->|No| Plain{First plain-text request?}
    Plain -->|No| Reject
    Plain -->|Yes| PlainCap[Cap = min of caller cap and SDK cap]
    Mode -->|Yes| First{First request of the turn?}
    First -->|Yes| FirstCap[Cap = min of 1,024, caller cap and SDK cap]
    First -->|No| LaterCap[Cap = min of 512, caller cap and SDK cap]
    PlainCap --> Provider[Existing provider executor]
    FirstCap --> Provider
    LaterCap --> Provider
    Provider --> Check{Measured output within the request cap?}
    Check -->|Yes or unknown| Report[Report request usage then release waiters]
    Check -->|No| Fault[Record provider protocol failure]
    Fault --> Report
    Provider -->|Context window full| Full[Record context limit; turn fails]
    Report --> Next[SDK may request the next pass]
```

## Settlement and cancellation

The existing executor wrapper marks request start before calling the provider.
After provider return or failure, it settles usage before releasing inference
waiters. This prevents a host tool or later inference overtaking that report.
Ollama only updates the observer's current counter tuple; it does not emit a
second report from its completion callback.

On user cancellation, helper failure or shutdown with an active request,
`HelperSession` emits one incomplete report before Terminal and stops admission.
This fallback contains unknown counters because the provider has not settled.
The session then cancels the existing model task. Late callbacks cannot emit
another report after terminal. Channel loss can prevent delivery; the service
must retain unknown usage instead of manufacturing a settled report.

### Usage lifecycle — selected design

Arrows name asynchronous callbacks and state transitions. The session marker is
internal; only settled reports cross the private channel.

```mermaid
sequenceDiagram
    participant E as Existing executor wrapper
    participant U as Turn usage observer
    participant P as Provider executor
    participant S as HelperSession
    participant R as Rust model owner
    E->>U: Begin request UUID
    U->>S: Empty Snapshot with start ordinal
    E->>P: Existing bounded request
    opt Ollama recognized final counters
        P->>U: Update active request counts
    end
    alt Provider returns or fails
        E->>U: Settle request outcome
        U->>S: Empty Snapshot with one usage report
        S->>R: Fenced InferenceUsage
        E->>E: Release inference waiters
    else User cancels while request pending
        R->>S: Cancel
        S->>R: Incomplete unknown InferenceUsage
        S->>S: Close admission and cancel model task
        S->>R: Terminal
        E-->>S: Late settlement ignored after terminal
    end
```

## Async and resource obligations

All added work uses existing actors and awaited callbacks. One executor request,
one usage observer and one session usage tuple are active per admitted turn.
There are no detached reporting tasks or new callback waiter collections.
Each callback performs constant-size validation and one bounded transport enqueue.
The transport retains its eight control frames, 4096-byte control-frame maximum
and nonblocking socket owner. Queue saturation fails explicitly; it does not drop
usage silently or block the service reactor.

Inference retains cancellation-driven lifetime. Preparation, handoff and
transport waits have no time limit (owner, 2026-10-16); see
[helper and provider waits](model-provider-integration.md#helper-and-provider-waits-without-time-limits--owner-2026-10-16).
Cleanup keeps its wind-down bounds. Counter reporting adds no external call and
no time bound.
Cancellation and terminal delivery retain existing ordering and cleanup ownership.
A pending native model cannot prevent the client or service processing controls.

## Security and recovery

Reports contain integers and existing operation identities only. They cannot
include prompts, tool arguments, output text, model metadata or provider errors.
Counter observations grant no tool authority and release no tokens. The service
keeps no durable token reservation (owner, 2026-10-18).
The service owns persistence and reconnect semantics. This helper packet has no
cross-session usage cache. A helper crash leaves unfinished usage unknown.

## Required validation

| ID | Initial state and trigger | Required observable result |
| --- | --- | --- |
| UH01 | One successful Ollama request reports zero input/output | Ordinal 1 retains both zeros and complete true; cached input remains absent |
| UH02 | SDK performs several admitted tool passes | One ordered report per executor request; counts are request-local |
| UH03 | Provider lacks supported request usage API | One report with absent counters and complete false; no counting call |
| UH04 | Recognized Ollama completion omits one count | Available count retained, missing count absent, complete false |
| UH05 | Pending request receives user cancellation | Exactly one incomplete report before cancelled Terminal; late settlement emits nothing |
| UH06 | Provider fails after measured counts | One incomplete report, original failure retained and no new request |
| UH07 | Duplicate report, overlapping start or invalid counter arrives | Existing protocol failure; no duplicated or invented statistics |
| UH08 | Text streams before usage settlement | Full replacement response retained; report is delivered without replacing text |
| UH09 | Slow reader saturates existing transport | Explicit bounded failure; no additional task, queue or hidden drop |
| UH10 | Tool-enabled turn dispatches 100 requests with the caller cap 2,048 | Every request dispatches; caps are 1,024 then 512; ordinals 1–100 are reported in order; no request-count or turn-total failure |
| UH11 | Output count exceeds the request cap or belongs to another request | Provider protocol failure; no later request reaches the provider |
| UH12 | Cancellation occurs while a request is admitted | No later request reaches the provider |
| UH13 | Caller cap is smaller than the pass cap | Every request uses the caller cap; the sum across requests can exceed the caller cap |
| UH14 | Plain-text turn requests a second dispatch | Second dispatch fails with the output-limit result before provider I/O |
| UH15 | A later tool-turn request finds the context window full | The executor records `context_limit`; the turn fails with the context limit and no further request dispatches |

Unit checks exercise observer request identity, zero, partial counts, invalid
bounds, multiple passes and failure. Real-socket helper integration checks UH05,
UH07 and UH08 through actual protobuf frames and terminal ordering.
Existing transport tests cover queue limits; root must run those checks as part
of the serial helper suite. The parent owns service/process and PTY integration
for event collection, persistence, user display and cancellation responsiveness.

Native qualification must observe an actual Ollama request and tool continuation
through the packaged service, verifying independently reported input/output.
System/CoreAI/MLX checks can establish unknown-report behavior; they cannot claim
measured per-request usage. Mock success does not establish provider statistics.
All model runs and builds remain serial under the parent owner.

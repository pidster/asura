# Tool output views and limits

Status: selected principles, 2026-10-13. The owner selected the principles in
[Required behavior](#required-behavior). The [limit review](#limit-review)
classifies every tool-handling limit found on 2026-10-13 against them. Nothing
here is implemented. Changes to each limit belong to the design that owns it,
listed in [Required design changes](#required-design-changes). Protocol
numbering remains `0.1`; journal format remains `1`.

## Purpose

Asura's tool limits were set when tool output was copied whole into model
input and into the journal. Under the [tool job controller](tool-job-controller.md),
output is retained as a stream and read through a handle. Under
[loop closure](loop-closure.md), loops end through closures over their own
streams, and token capacity comes from the selected model. Most fixed limits
were standing in for those mechanisms. This document records which limits stay,
which move to another owner, and which are removed.

## Required behavior

Required behavior (owner, 2026-10-13):

1. **One retained stream.** A tool's complete output is held once, as a retained
   stream in the payload store. It is the only complete copy.
2. **Two views.** The person view and the model view of a stream differ and are
   chosen independently.
   - The **person view** is a display rule: the log folds output, and the job
     viewer pages through all of it.
   - The **model view** is what the context composer places in one model
     request. The composer chooses it per request from the model's capacity
     record and the other content competing for context.
   - No tool defines a fixed preview size. A tool result gives the model a
     handle and a short disclosure; the model pages the stream for more.
3. **Journal references, not text.** The journal stops holding tool result text.
   A result record keeps a payload reference, a digest and a short summary.
4. **Fixed transport units.** Wire and journal frame sizes stay fixed. They are
   transport units, not content limits; large content streams across frames.
5. **Evidence before a turn backstop.** The closure that guarantees a turn's
   tool loop ends is not chosen yet. Asura first logs real tool-loop traces,
   builds a classifier over them, and derives stall and timing closures from
   that evidence. Until then the existing 8-call exhaustion closure stays in
   force unchanged.
6. **Ollama context from configuration.** The helper's hard-coded
   `min(capacity, 8192)` context clamp is removed. Context comes from the
   model's capacity record under [model-sized limits](loop-closure.md#model-sized-limits).

A fixed numeric limit remains valid only as a
[known-size exhaustion closure](loop-closure.md#model-sized-limits): a quantity
whose size Asura knows and fixes, such as a buffer, a frame or a process bound.

## Views of one stream

Selected principle. Arrows show data flow from the producing process to each
reader. The composer and the job viewer read the same retained stream.

```mermaid
flowchart LR
    Proc[Tool process] -->|stdout and stderr chunks| Ctl[Job controller]
    Ctl -->|append| Store[(Retained stream)]
    Ctl -->|reference, digest, summary| Journal[(Journal record)]
    Store -->|pages| Viewer[Person view: log fold and job viewer]
    Store -->|pages on request| Composer[Context composer]
    Cap[Model capacity record] -->|budget per request| Composer
    Composer -->|model view in one request| Model[Model request]
    Model -->|job read with handle| Ctl
```

## Limit review

The 2026-10-13 inventory found 49 limits. Each row names its current value and
its classification under the principles above.

| Class | Meaning |
| --- | --- |
| Keep | A known-size bound; unchanged |
| Replace | Superseded by a job or composer mechanism, named in the row |
| Remove | Not a limit under the principles; its role moves to the named owner |
| Model-sized | Derived from the model capacity record |
| Evidence | Waits for the trace classifier in rule 5; current value stays meanwhile |

### Execution

| ID | Limit | Current value | Class | Owner after review |
| --- | --- | --- | --- | --- |
| L1 | Shell timeout | 1–60 s, default 30 | Remove | Job yield window only; jobs have no fixed lifetime (owner, 2026-10-14, J2a-1). Job closures are completion, cancel, revocation, restart and capture fault |
| L2 | Setup budget | 2 s | Keep | Shell tool |
| L3 | Cleanup until unconfirmed | 2.5 s | Keep | Shell tool |
| L4 | TERM to KILL grace | 500 ms | Keep | Shell tool; the person profile sets its own |
| L5 | Guardian lease poll and clock step | 100 ms each | Keep | Platform guardian |
| L6 | Group-membership scan | 256 process IDs | Keep | Platform guardian |
| L7 | Scratch cleanup | 4096 entries, 32 levels | Keep | Shell tool |
| L8 | Live shell jobs | 1 per active turn | Replace | Job slots per service and per conversation |
| L9 | Pending tool calls and executors | 1 each | Keep | Model tools |
| L10 | Ordinary tool deadline | 2 s | Keep | Model tools; `job wait` needs an exemption |
| L11 | Command and cwd | 8192 B and 1024 B | Keep | Shell tool |
| L12 | Private argv payload | 8704 B | Keep | Shell tool |

### Capture and output

| ID | Limit | Current value | Class | Owner after review |
| --- | --- | --- | --- | --- |
| L13 | Tail ring | 8 KiB per stream | Replace | Retained stream |
| L14 | Drain quantum | 8 KiB per stream per dispatch | Keep | Reactor fairness |
| L15 | Read buffer | 4 KiB | Keep | Capture |
| L16 | Combined output cap | 64 MiB | Replace | One retention quota with pause and expiry in the job controller |
| L17 | Rendered stream | 6144 B, most recent end | Remove | Model view by the composer; person view by display rules |
| L18 | Metadata envelope | 512 B | Keep | Shell tool |
| L19 | Status channel | 128 B buffer, 32 B notices, 3 notices | Keep | Platform guardian |

### Result delivery to the model

| ID | Limit | Current value | Class | Owner after review |
| --- | --- | --- | --- | --- |
| L20 | Calls per turn | 8 | Evidence | Trace classifier; turn closure mapping |
| L21 | Per-result bytes | 16,384 | Remove | Composer for the model view; journal references for records |
| L22 | Result bytes per turn | 65,536 | Remove | Composer |
| L23 | Memory-create headroom | 256 B | Keep until L21 changes | Memory tools |
| L24 | Output reservation and request caps | 2048; 1024 then 512 | Model-sized | Capacity record |
| L25 | Inference requests per turn | 64 | Evidence | Trace classifier; turn closure mapping |
| L26 | Ollama context clamp | min(capacity, 8192) | Model-sized | Capacity record (rule 6) |
| L27 | Model input, history, prompt | 64 KiB; 32 entries; 32 KiB | Remove | Composer; transfer chunking stays fixed |
| L28 | Ollama parser bounds | 16 KiB arguments; 128 B name; 64 KiB line; 4 MiB stream | Keep | Helper parser |

### Journal and records

| ID | Limit | Current value | Class | Owner after review |
| --- | --- | --- | --- | --- |
| L29 | Frame | 64 KiB | Keep | Journal transport unit |
| L30 | Journal capacity | 8 MiB, 32,768 frames | Keep; revisit after rule 3 | Journal owner |
| L31 | Tool result text record | 16,384 B | Replace | Reference, digest and summary record |
| L32 | Shell intent payload | 9217 B | Keep | Shell tool |
| L33 | Shell intent timeout | 1–60,000 ms | Remove | Job intent records no timeout (owner, 2026-10-14, J2a-1) |
| L34 | Record ordinal | 1–8 | Evidence | Follows L20 |
| L35 | Journal owner memory | 16 MiB; writer 64 MiB | Keep | Journal owner |

### Protocol, file, memory, audit and inventory tools

| ID | Limit | Current value | Class | Owner after review |
| --- | --- | --- | --- | --- |
| L36 | Control and model frame, chunk, credit | 64 KiB; 16 KiB; 64 KiB | Keep | Transport units (rule 4) |
| L37 | `read_file` page | 1–16,384, default 4096 | Keep ceiling; default becomes a composer choice | Model tools |
| L38 | Path | 1024 B | Keep | Model tools |
| L39 | `list_directory` | 128 entries, 16 KiB | Keep | Model tools |
| L40 | `list_notes` | 1–8, default 8 | Keep | Memory tools |
| L41 | Note preview | 256 B | Keep | Memory tools |
| L42 | `get_note` page | 1–16,384, default 4096 | Keep ceiling; default becomes a composer choice | Memory tools |
| L43 | Note sources | 32 | Keep | Memory tools |
| L44 | Note body | 1–16,384 | Keep | Memory tools |
| L45 | Store response and deadline | 32 records or 64 KiB; 2 s | Keep | Memory store |
| L46 | `read_audit` | 1–16 records | Keep | Audit tools |
| L47 | Audit reply bytes | 16,384 in design; 15,000 in code | Keep | Audit tools |
| L48 | Audit window, record, hydrate | 256; 4096 B; 256 KiB | Keep | Audit |
| L49 | `list_tools` | 32 entries, 16 KiB | Keep | Model tools |

### Design and code mismatches

| Mismatch | Design | Code |
| --- | --- | --- |
| Shell job scope (L8) | One service-wide job (`shell-tool.md`) | One job per active turn (`conversation.rs:487`); the owner holds one active turn (`conversation.rs:584`), so both statements hold |
| Truncation fields (L18) | `stdout_truncated` and `stderr_truncated` | One `truncated` field (`shell.rs:51`) |
| Audit reply bytes (L47) | 16,384 | Stops at 15,000 (`audit_tools.rs:60`) |
| Ollama context (L26) | No design | `min(capacity, 8192)` (`OllamaBackend.swift:130`) |

The job designs also disagreed among themselves. Rules 1–3 resolve the three
preview sizes (job head and tail, rendered stream, `!` notice tail) and the three
meanings of 64 MiB (emitted-output stop, per-call retention, per-job quota).

## Tool-loop trace evidence

Required behavior (rule 5): the turn backstop is derived from evidence.

Proposed mechanism: the service records a bounded trace for each turn's tool
loop. Each entry holds the call kind, a digest of the arguments, a digest of the
result, new retained bytes, timing and the turn's eventual ending. The trace
holds no argument or output text. A classifier over these traces separates
productive loops from stalls and cycles, and proposes the stall fingerprint,
cycle window and timing bounds for the [closure mapping](loop-closure.md#decisions).
The trace design, its retention and the classifier belong to a later design.
Until it selects closures, L20, L25 and L34 keep their current values.

## Required design changes

Each owner design must adopt these principles before implementing its part.

| Design | Change |
| --- | --- |
| [Tool job controller](tool-job-controller.md) | Replace its fixed preview with a disclosure the composer sizes; until the composer exists, results carry no output text and the model pages with `job read` (owner, 2026-10-14, with PC12); hold one retention quota; result records carry references |
| [Result retention packet](result-retention-graph-packet.md) | Align its 64 MiB per-call limit and capture concurrency with the single job quota |
| [Model tools](model-tool-execution.md) | Move per-result and per-turn byte budgets to the composer; keep page ceilings |
| [Shell tool](shell-tool.md) | Replace the tail ring, rendered size and 60 s timeout under jobs; fix the truncation-field mismatch |
| [Loop closure](loop-closure.md) | Record the trace-classifier direction for the tool-loop backstop |
| [Person command mode](person-command-mode.md) | Drop its fixed notice and record sizes; use the two views |
| [Execution threat model](execution-threat-model.md) | Update statements about journaled output size once records hold references |
| Model provider and helper | Remove the Ollama clamp once the capacity record exists |

## Validation

Each owner design defines the tests for its change. This review requires two
cross-cutting checks when the changes land:

| ID | Check |
| --- | --- |
| TV1 | A tool result larger than any former fixed limit is retained whole, shown in full to the person, and reaches the model only as the composer's chosen view plus a handle that pages the rest. |
| TV2 | No journal record contains tool output text; each result record's reference and digest match the retained stream. |

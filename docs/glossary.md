# Asura glossary

Status: definitions for terms used in the current design documents. Detailed
schemas and runtime mechanisms remain with their governing designs.

## Components and work

| Term | Meaning |
| --- | --- |
| Orchestrator | Component that controls task state, schedules work and manages shared budgets |
| User service | One active backend owner for an OS user on a device; it can supervise separate agent and helper processes |
| Installation | Persistent backend state scope; on a user device it belongs to the user service, not to an individual project |
| Agent core | Component that determines the steps needed to perform assigned work |
| Agent runtime | Component that executes assigned work and reports its progress and results |
| Host | Machine that runs an Asura instance or performs an operation |
| Host services | Components that start processes, access local resources and enforce permissions and limits |
| Control client | CLI, TUI or GUI that sends commands and presents task events |
| Composer | Chat editor together with its adjacent destination, information, status and control areas; distinct from the context composer |
| Context composer | Service component that chooses, for each model request, the instructions, history, notices and model view of tool output sent to the model, sized from the model's capacity, and records a composition manifest; see the [context composer design](designs/context-composer.md). It is not the TUI input box |
| Status bar | Compact row below the input showing project/Git context, draft count, running activity, thinking level and model/context status |
| Thinking level | The person's model reasoning setting: model default, off, low, medium or high; see [thinking level](designs/thinking-level.md) |
| Thinking text | A model's reasoning output for a turn; journaled and shown folded, never sent back to the model |
| Message tray | Bounded area above the input showing submitted messages and their observed states, with access to full text |
| Task | A unit of user-requested work with goals, limits and completion criteria |
| Plan | A versioned graph of intended work, dependencies and completion criteria |
| Plan work item, proposed | An identified piece of planned work; it may link to an accepted task after admission |
| Work dependency, proposed | A prerequisite and explicit satisfaction condition that controls readiness of planned work |
| Partial progress report | A task-linked account of completed work, remaining work, blockers and evidence; it does not establish task completion |
| Conversation | User-visible grouping of messages and task references; distinct from a retained model session |
| Agent instance | Runtime participant assigned bounded work under orchestrator control |
| Action | A proposed or admitted operation, tracked by a stable identity through execution and recovery |
| Operation | An individual activity, such as a model call, tool call or remote request |
| Dispatch | Sending admitted work to the component that will perform it |
| Effect | A change or external consequence of an operation, such as a file write or data disclosure; usage is also tracked in budget records |
| Input pipeline | Processing that validates and translates user input or control requests into typed commands for their owner |
| Signal | Notification whose meaning belongs to a specific component, such as a deadline, process exit or operation completion |
| Signal pipeline | Processing that normalizes, validates and routes signals to their responsible component |
| Managed input queue | Durable per-conversation queue that every conversational message enters before model admission; see the [queue design](designs/managed-input-queue.md) |
| Steer | User input that durably cancels the active operation and replaces it with the original prompt plus a new instruction; it does not modify running inference |
| Conversation restoration | TUI recovery of the selected project's most recent conversation and queue scope; see the [restoration design](designs/conversation-restoration.md) |
| Sensor | Scoped source of observations; an observation is evidence, not an instruction, permission or accepted task; see the [sensor design](designs/sensors.md) |
| `exec_cmd` | Model tool that runs one noninteractive `/bin/sh -c` command line in the selected project under the model execution profile; see the [exec_cmd tool design](designs/exec-cmd-tool.md) |
| exec_cmd qualification | The per-model EX10 campaign: cases EQ-1 to EQ-4 (with diagnostic EQ-3b and EQ-4b) run 5 times on one recorded model identity, giving the verdict qualified, unreliable, failed, unavailable or not run. A verdict does not gate whether exec_cmd is offered; see [per-model qualification](designs/exec-cmd-tool.md#per-model-exec_cmd-qualification-ex10) |
| Tool call record | Journal kind 14 `ToolCall`: one record for each tool call the model proposes in an admitted turn, including malformed calls; it holds the redacted tool name and argument text (the only copy), the operation code, the decision and the fault, and never authorizes execution; see [tool call records](designs/tool-call-records.md) |
| Tool call decision | What the service did with a proposed tool call: `run`, `invalid_arguments`, `unknown_tool`, `refused`, `busy`, `denied`, `limit` or `cancelled`; only `run` executes, and only `run` is followed by a result record; see [decisions](designs/tool-call-records.md#decisions) |
| Argument form | How a tool call record keeps the argument text: 1 verbatim, 2 canonical JSON, 3 unparsed text, or 4 not retained; see [tool call records](designs/tool-call-records.md#redaction-algorithm) |
| Not-retained reason | Why a tool call record keeps no argument text: `oversized`, `detection_unavailable` or `unverifiable_escape`; the record keeps the received size; see [tool call records](designs/tool-call-records.md#record-kind-14-toolcall) |
| Tool job | Proposed conversation-scoped long-running tool effect whose output the service retains; see the [tool job controller](designs/tool-job-controller.md) |
| Agent job queue | The job controller's FIFO of job starts for one agent (today a conversation's main agent) that wait for a slot, quota or another start condition; each entry shows a brief blocking reason; see [agents, queues and slots](designs/tool-job-controller.md#agents-queues-and-slots) |
| Job slot | The resources of one running tool job: a slot worker, a capture worker and its quota reservation; counted per agent and per service |
| Job reservation ledger | The journal capacity that each unsettled tool job holds for its cancel and settlement records, independent of any turn; see [J2 records](designs/tool-job-controller.md#replay-order-and-the-job-reservation-ledger) |
| Wind-down bound | A fixed bound on one of Asura's own operations that acts only after a closure has fired, such as the TERM-to-KILL grace or the cleanup bound; it never ends the work it serves |
| Job handle | Opaque service-derived job ID used to reconnect to a tool job; it is not an access token |
| Escapee | A command descendant that left the guardian's owned process group, by `setsid`/new session or by double-fork reparenting to `launchd`; see [escapee control](designs/escapee-control.md) |
| Job marker | A per-job test, evaluated with `sandbox_check`, that attributes a sandboxed process to one job: it may write the job's own scratch and may not write the job's decoy path; see [escapee control](designs/escapee-control.md#attribution-the-per-job-sandbox-marker-model-and--jobs) |
| Settlement gate | The condition the guardian requires before it reports `owned_group_settled`: leader reaped, owned group empty, capture/status pipes at EOF, and for sandboxed jobs zero attributed descendants; see [escapee control](designs/escapee-control.md#the-settlement-gate-and-cleanup_confirmed) |
| Secret placeholder | The text `$ASURA_SECRET_<n>` that replaces a detected secret in stored job output, in a rewritten command and in a tool call record; within one job or call the same value has the same number; see [secret detection](designs/secret-detection.md#placeholders-and-numbering) |
| Unnumbered placeholder | The reserved placeholder `$ASURA_SECRET_0`; it replaces a value after the numbering limit and any placeholder token the model wrote itself, so every numbered placeholder in a record was inserted by Asura; see [tool call records](designs/tool-call-records.md#argument-redaction) |
| Secret variable | The environment variable `ASURA_SECRET_<n>` that carries a command secret into one execution only, or `ASURA_SECRET_COMMAND`, which carries a whole raw command the rewrite cannot make safe; records keep its name, never its value; see [secret detection](designs/secret-detection.md#environment-path) and the [whole-command variable](designs/secret-detection.md#whole-command-variable) |
| Loss marker | Text in a stored stream that replaces bytes the secret scanner could not check, stating their size and reason; see [secret detection](designs/secret-detection.md#loss-markers) |
| Secret rule set | Asura's port of Wisp's secret-detection rules, identified by a revision and digest that each capture descriptor records; see [secret detection](designs/secret-detection.md#rule-set) |
| Loop closure | Ending condition a loop evaluates over its own streams, such as exhaustion, stall or an owner-approved deadline; distinct from an interrupt, which comes from outside the loop; see [loop closure](designs/loop-closure.md) |
| Journal segment file | One authority journal file of complete format-1 frames, at most 8 MiB and 32,768 frames; never deleted; see the [segmented journal](designs/segmented-journal.md) |
| Journal checkpoint | Derived file holding the live replay state at one frame sequence and the list of index runs; replay starts from it; see the [segmented journal](designs/segmented-journal.md#checkpoints) |
| Index run | Derived, immutable file of sorted index entries that address settled history by frame locator; it holds no text; see the [segmented journal](designs/segmented-journal.md#index-runs-and-compaction) |
| Journal quota | Total byte limit for journal files, from `journal.quotaBytes` and free disk space; at the limit new work is held, nothing is deleted; see the [segmented journal](designs/segmented-journal.md#quota-and-disk-space) |
| Waiting step | Live, unjournaled label for what a pending turn waits on, such as preparing or starting the model; display only, it never ends work; see [waiting step display](designs/model-provider-integration.md#waiting-step-display--owner-2026-10-16) |
| Permanent or temporary ending | A permanent ending is terminal; a temporary ending suspends a loop with a named resume condition, resolver and expiry |

These are reading definitions. The proposed [D0 domain model](designs/domain-model.md)
refines their relationships, including actions and individual execution attempts.
Those refinements await review; concrete schemas remain D3-D4 work.
The [memory ontology](designs/hybrid-memory-ontology.md) defines proposed plan
graphs and task tracking records, including their relationship to control authority.
The selected [asynchronous architecture](decisions/0006-async-event-pipelines.md)
defines input/signal processing boundaries. A signal does not itself grant authority
or prove an operation completed.

## Identity and permission

| Term | Meaning |
| --- | --- |
| Principal | User or service identity whose permissions apply to a request |
| Authority | Permission to perform specified work; the document must name who grants or enforces it |
| Authoritative state | Recorded state that the owning component uses to decide what is currently valid |
| Authority store | Durable store for the state that controls task transitions or permissions; its location is a D3 decision |
| Grant | Authorization tied to an action, principal, host, policy revision, limits and expiry |
| Admission | The checks and durable records required before an operation may start; admission does not mean execution completed |
| Revision | Version of a task, policy or other record used to detect concurrent changes |
| Generation or owner epoch | Version that identifies the current binding, session or work owner; requests from an obsolete version must be rejected where the contract requires it |
| Fencing | Enforcement that prevents a previous owner or obsolete generation from starting work after it loses authority |
| Lease | Time-limited authority to own or perform specified work |
| Egress | Data sent across a defined trust boundary, for example to a model provider or external database |

The [policy brief](designs/security-policy-brief.md) defines authorization duties.
The [harness brief](designs/core-harness-brief.md) defines task and session checks.

## Persistence, recovery and budgets

| Term | Meaning |
| --- | --- |
| Durable | Recorded so that the selected storage contract preserves it across the specified failures |
| Ledger | Record of task or action decisions and outcomes used for recovery and inspection |
| Intent | A recorded decision to request an operation or state change; it does not prove the effect occurred |
| Reconciliation | Using durable records and external evidence to determine what happened to an operation |
| Accounted effects and usage | Effects are known, and usage is either settled from evidence or retained in a durable budget reservation |
| Reservation | Budget allowance set aside before an operation starts and unavailable to competing work |
| Settlement | Recording actual resource usage and releasing only allowance proven to be unused |
| Ancestor budget | A parent task, workspace or installation budget that also limits a child operation |
| Remote budget envelope | A reserved share of the parent budget allocated to a remote child; the parent cannot also spend that share |
| Terminal task | A task in Succeeded, Failed or Cancelled state; late evidence cannot restart it |
| Binding | Saved association between an installation and a specific context graph |
| Cutover | The point at which a new graph binding becomes active during an authorized change |
| Projection | Derived data that can be rebuilt from an authoritative record, with progress tracked for recovery |

Use the [budget contract](designs/core-harness-brief.md#aggregate-budget-ownership-and-admission)
for reservations and settlement. Use the [failure contract](designs/core-harness-brief.md#common-failure-and-deadline-contract)
for terminal outcomes. The [storage brief](designs/context-storage-candidates.md)
defines binding changes.

## Context and design work

| Term | Meaning |
| --- | --- |
| Provenance | Records of an item's source, version and transformations |
| Memory version, proposed | Immutable record of content and provenance under the hybrid memory ontology |
| Artifact set, proposed | Scoped storage grouping for retained payloads; distinct from a conversation or model session |
| Project context | Registered project or repository scope with stable identity and working locations; distinct from model input |
| Project parent | Explicit installation-local discovery container for candidate child projects; it is not a project context, working location or permission grant |
| Project group | Installation-local grouping that can make member contexts eligible to reuse linked evidence after per-use authorization; a context belongs to at most one group at a time, while membership authority remains D3 work |
| Project visibility | Closed, group or open eligibility for linked-evidence reuse within one installation; closed is the default and no label grants access by itself |
| Working location | Validated filesystem location associated with a project context, including an individual repository worktree |
| Repository | Version-control history associated with local checkouts; a remote URL does not uniquely identify a working location |
| Worktree | Individual checkout with its own working location, even when it shares repository history |
| Registration | Association between a project context and a validated working location; it grants no access |
| Configuration snapshot | Immutable effective settings and source provenance for a context and working directory at a revision |
| Context view | Selected evidence and instructions prepared for a particular task and destination |
| Input context, proposed | Scoped evidence and instructions available to interpret a particular user submission; distinct from its project context or an execution model session |
| Input processor, proposed | Component that transforms a submitted input and its context into a proposed intent or clarification; it does not grant authority |
| Effective context | All input that can affect a model response, including retained session history and caches |
| Manifest | Record of selected inputs, their versions, order, transformations and omissions |
| Canonical owner | The one component or document responsible for a behavior or rule |
| Design stage | A D0–D8 activity in the architecture and design plan |
| Delivery increment | An I0–I9 milestone in the implementation plan |
| Implementation packet | A scoped assignment that names the ready design, owned files, dependencies and required checks |
| Readiness gate | Conditions that must be met before the next stage starts; name those conditions when using this term |

The [visual reading map](decisions/README.md#visual-reading-path) connects the
architecture overview to the detailed contracts. This glossary explains terms;
it does not replace those contracts.

The [user-service brief](designs/user-service-configuration.md) defines project
contexts and directory configuration. Existing workspace-scoped contracts must
carry that project context and validated location; “workspace” is not a global
current directory for the service. The [D0 model](designs/domain-model.md) proposes
their semantic relationships; D3 must define concrete identity schemas.

The [interaction brief](designs/interaction-and-extension-boundaries.md) explores
input context and processor boundaries. These terms do not select an additional
process, model or lifecycle owner.

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
| Task | A unit of user-requested work with goals, limits and completion criteria |
| Conversation | User-visible grouping of messages and task references; distinct from a retained model session |
| Agent instance | Runtime participant assigned bounded work under orchestrator control |
| Action | A proposed or admitted operation, tracked by a stable identity through execution and recovery |
| Operation | An individual activity, such as a model call, tool call or remote request |
| Dispatch | Sending admitted work to the component that will perform it |
| Effect | A change or external consequence of an operation, such as a file write or data disclosure; usage is also tracked in budget records |

These are reading definitions. The proposed [D0 domain model](designs/domain-model.md)
refines their relationships, including actions and individual execution attempts.
Those refinements await review; concrete schemas remain D3-D4 work.

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
| Project context | Registered project or repository scope with stable identity and working locations; distinct from model input |
| Working location | Validated filesystem location associated with a project context, including an individual repository worktree |
| Repository | Version-control history associated with local checkouts; a remote URL does not uniquely identify a working location |
| Worktree | Individual checkout with its own working location, even when it shares repository history |
| Registration | Proposed association between a project context and a validated working location; it grants no access |
| Configuration snapshot | Immutable effective settings and source provenance for a context and working directory at a revision |
| Context view | Selected evidence and instructions prepared for a particular task and destination |
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

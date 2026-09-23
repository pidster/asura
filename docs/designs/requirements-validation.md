# D0 requirements and validation matrix

Status: initial traceability proposal for the selected local CLI/TUI release with
on-device assistance. Existing requirements remain required; D0 refinements and
objective values await review. All product implementation,
runtime verification and release states are **not started**. No row is a passing
test or an implementation authorization.

## Ownership and use

This matrix owns coverage tracking, not duplicate behavioral contracts. The
[workflows](product-workflows.md) define W0-W5 and proposed P0-P9 objectives.
The [domain model](domain-model.md) defines proposed relationships. Existing
briefs retain their lifecycle, storage, configuration and security contracts.

The primary design coordinator maintains this index. D1-D6 resolve the named
contracts. D7 assigns distinct executable test IDs to each fault/race combination,
pins environments and commands, and records artifacts. D8 checks completeness.
Unit, integration and end-to-end evidence are cumulative; mocks cannot substitute
for real platform, storage, model or remote-boundary evidence.

### Requirement to delivery

Required traceability with proposed D0 artifacts. Arrows show dependencies and
evidence flow. Review approval and implementation authority remain separate.

```mermaid
flowchart TD
    Required["Existing requirements"] --> D0["D0 workflows, domain and target proposals"]
    D0 --> Review["Resolve scope and review D0 choices"]
    Review --> Designs["D1-D7: contracts and detailed validation"]
    Designs --> D8["D8: consistency and ready initial packets"]
    D8 --> Authority["Owner review and explicit implementation authorization"]
    Authority --> Delivery["Implement within assigned packet"]
    Delivery --> Unit["Unit evidence"]
    Delivery --> Integration["Real integration evidence"]
    Delivery --> E2E["End-to-end user evidence"]
    Unit --> Verify["Check all required layers and environments"]
    Integration --> Verify
    E2E --> Verify
    Verify -->|Pass| Record["Record verified scope, versions and limitations"]
    Verify -->|Missing or failed| Gap["Retain incomplete status and repair"]
```

## Preserved architecture scenarios

R01-R13 preserve the numbered
[architecture review scenarios](../plans/architecture-and-design.md#scenarios-that-must-survive-the-design-review).
An increment is the first delivery boundary for the stated behavior, not a reason
to postpone its architectural design. Later increments repeat relevant earlier
cases when a new boundary changes their proof obligations.

| ID | Behavior and workflow | Governing contract / design stage | Delivery |
| --- | --- | --- | --- |
| R01 | Non-interactive investigation returns evidence and deterministic status; W2 | Harness; D3-D4, D6 | I1 control; I2-I3 investigation |
| R02 | Interactive investigation, remote help, permitted edits and validation | Harness and provider/edit designs; D4-D6 | I4 interaction, I5 inference, I6 edits |
| R03 | Second client sees and controls the same task; W3-W4 | Service C1, harness control; D3, D6 | I1, repeated I4 |
| R04 | Stalled model does not stall status/cancel; W3-W4 | Architecture control API; D2-D4 | I3 |
| R05 | Crash after effect does not cause blind replay; W4-W5 | Harness failure, storage B4; D3-D4 | I1-I2, repeat I5-I7 |
| R06 | Changed source invalidates evidence and proposal; W2 | Harness context/model contract; D4 | I2-I3 |
| R07 | Denied/unavailable inference explains options without disclosure | Policy and provider designs; D1, D5-D6 | I5; no remote fallback in I3 |
| R08 | Remote disconnect preserves ownership, expiry and recovery | Harness remote budget contract; D5 | I7 |
| R09 | Hostile input cannot expand authority; collector outage preserves control | Policy brief, architecture telemetry; D1-D4, D6 | I1-I3, repeat I5-I7 |
| R10 | Pause/cancel during reconciliation survives restart; W4 | Harness H3, implementation A3; D3-D4 | I1-I2 |
| R11 | Response/expiry/cancel race has one durable winner; W3 | Harness H4, implementation A3; D3-D4 | I1-I2 |
| R12 | Revoked/deleted retained model input cannot affect later work; W2-W3 | Model-session contract, implementation A5; D4 | I3 |
| R13 | Build descendants and aliases cannot write protected source; W2 | Implementation A4 and I2 boundary; D1, D4 | I2; extend for I6 |

For R01, D3/D6 must distinguish accepted/running, succeeded, failed with known
effects, failed with uncertainty and cancelled outcomes in structured output.
They select exact exit codes and whether a request waits or detaches. Transport
failure must not be reported as proof that a task failed or never started.

R02, R07 and R08 remain required architecture scenarios beyond the selected I4
first-release functionality. That release cannot claim remote-assisted editing
or remote-host completion. Their later designs still constrain earlier interfaces.

## Required evidence by scenario

U, I and E describe unit, integration and end-to-end evidence. Each cell describes
a validation obligation, not an existing test suite. Canonical detailed cases are
linked below; D7 must split combined conditions into independently identifiable tests.

| ID | U: isolated rules | I: real boundaries | E: user-visible result |
| --- | --- | --- | --- |
| R01 | Typed status and scope validation | Durable submission, tool evidence and result records | CLI investigation with status and cited evidence |
| R02 | Edit preconditions, budgets and completion criteria | Provider, edit transaction and validation tools | TUI investigation through permitted validated change |
| R03 | Revision conflicts and decision ownership | Two clients, shared service and restart | Attach, respond and cancel without duplicate task |
| R04 | Deadlines and independent control scheduling | Stall real adapter boundary while control remains live | Status/cancel under P0 stalled-inference profile |
| R05 | Intent/outcome distinction and retry decisions | Crash before/after dispatch and durable result | Reopen task and disclose reconciled or unknown effects |
| R06 | Dependency invalidation and proposal versions | Change source between retrieval and operation start | Explain stale evidence and re-evaluate before work |
| R07 | Disclosure manifests and rejection reasons | Actual provider authentication/outage plus deny path | Explain denied help; verify no unauthorized egress |
| R08 | Owner epoch, expiry and reserved envelope rules | Two real hosts, partition and late results | Disconnect, cancel, reconnect and inspect residual effects |
| R09 | Untrusted attributes and bounded telemetry buffers | Host enforcement, unauthorized client, collector outage | Hostile input denied while authorized control remains usable |
| R10 | Cancel precedence and no resume after accepted intent | Persist intent during reconciliation, then crash | Reconnect and observe preserved cancellation and effects |
| R11 | Response/deadline/cancel arbitration | Race and restart at authoritative update boundary | Expired decision cannot restart inference or task work |
| R12 | Effective-context manifest and generation checks | Real model sessions, caches and in-flight invalidation | No reuse across tasks/principals; stale result rejected |
| R13 | Structured request and output-scope rules | Real scripts, subprocesses, aliases and file replacement | Build output permitted; protected source unchanged |

## Cross-cutting coverage

These cases are required in addition to R01-R13. Their canonical definitions
include the initial state, trigger, result and required layers.

| ID | Existing acceptance source | Workflows and delivery | Required environment |
| --- | --- | --- | --- |
| R14 | [Service C1-C4](user-service-configuration.md#validation-required-before-delivery) | W1, W4-W5; I1-I2 | macOS service, separate OS users, filesystem, both graph modes |
| R15 | [Storage B1-B5](context-storage-candidates.md#binding-acceptance-cases) | W5; I2 | Actual embedded store and external SurrealDB, interrupted rebinding |
| R16 | [Failure A1](../plans/implementation.md#a1-common-failure-settlement) | W3-W5; I1-I2 | Real durable state and process failures |
| R17 | [Budget A2](../plans/implementation.md#a2-shared-budget-reservations-and-recovery) | W2-W5; I2, extended I3/I5/I7 | Concurrent admission, real store, provider/host usage where claimed |
| R18 | [Security policy deliverables](security-policy-brief.md#required-design-and-validation-deliverables) | W1-W5; I1 onward | Selected evaluator and actual macOS enforcement |
| R19 | [P0-P9 objectives](product-workflows.md#proposed-measurable-objectives) | W1-W5; I1-I4 | Frozen workload, supported hardware, real clients/models/stores |
| R20 | [Default Ratatui chat launch W0](product-workflows.md#w0-launch-chat-by-default) | W0; I4 | Actual Rust backend, concurrent clients, real terminals and redirected input/output |
| R21 | [Async input/signal pipeline validation](../decisions/0006-async-event-pipelines.md#design-handoff-and-validation) | W0-W5; I1 onward, terminal input in I4 | Actual queues, stores, adapters, OS signals and slow consumers |

R18 preserves the distinction between required security capabilities and proposed
policy mechanisms. D1/D3 must resolve policy composition, approval semantics,
activation and audit failure behavior; this matrix does not select an engine.

R19 unit evidence covers timing accounting, metric definitions and bounded
resource policies. Integration evidence runs the profile through real boundaries.
End-to-end evidence measures client-observed results and completes usability and
accessibility review. P6/P7 are invariants across fault tests, not percentile goals.

The [runtime sketch](runtime-architecture.md#validation-and-open-decisions) refines
R20-R21 with proposed RT1-RT5 cases for scheduling, publication, cancellation,
slow consumers and recovery. Its process/queue mechanisms remain proposals.

## D0 refinement acceptance cases

These proposed cases make the new domain relationships reviewable. They extend
existing coverage and must not replace C1-C4 or the harness regression cases.

### N1: Alias and overlap selection

- **Initial state:** A parent project, nested repository and two worktrees are
  registered. Two contexts explicitly reference one location; an alias reaches it.
- **Trigger:** Resolve by path with and without explicit context/location IDs;
  include overlapping locations within one context. Repeat using the alias, a
  matching remote URL, invalid supplied IDs and an unauthorized registration.
- **Required result:** The proposed domain model yields the same location for
  aliases, distinct identities for worktrees/clones, and explicit ambiguity where
  appropriate. No hidden context is disclosed and no new allowance is created.
- **Unit:** Candidate selection, idempotent association and scope rejection.
- **Integration:** Actual aliases, nested repositories, worktrees and context
  subscriptions; race relocation or replacement against resolution.
- **End-to-end:** Two clients select different contexts without changing each
  other's task scope or exposing private evidence. Report a replaced location.
- **Environment:** Supported macOS filesystem and actual version-control fixtures.

### N2: Conversation is not execution authority

- **Initial state:** One conversation references two tasks. Each task has its own
  model state and budget; a second principal lacks access to the conversation.
- **Trigger:** Attach a second authorized client, send a follow-up, request a task
  revision, then attempt cross-task model-state reuse and unauthorized attachment.
- **Required result:** Explicit commands determine new work versus task revision.
  A transcript cannot mutate goals, extend budgets, grant access or carry retained
  model state into another task. Authorized evidence reuse records provenance.
- **Unit:** Membership, revision conflict and message-kind validation.
- **Integration:** Durable conversation/task references, session isolation and
  concurrent updates through the shared control contract.
- **End-to-end:** Reconnect in another client, inspect both tasks and revise one;
  verify the other task's goals and session remain unchanged.
- **Environment:** Real macOS service, clients, persistence and Foundation Models.

### N3: Attempt identity and uncertain retry

- **Initial state:** An admitted operation has reserved usage; its outcome is unknown.
- **Trigger:** Repeat the control request, restart, and later request another attempt.
- **Required result:** Recover the original identity before dispatching anything
  new. Duplicate delivery cannot allocate again. A new executable attempt requires
  fresh admission and a reservation under all applicable ancestor budgets.
- **Unit:** Action/operation relationships and replay classification.
- **Integration:** Crash around admission/dispatch/settlement with actual persistence;
  race duplicates against recovery and verify retained reservations.
- **End-to-end:** Repeat a timed-out command and inspect one original attempt;
  show any separately admitted retry and its usage without hiding uncertainty.
- **Environment:** Real control clients and stores; actual provider or remote-host
  evidence is additionally required when those boundaries become available.

## Evidence records and D0 readiness

Each future evidence record must name requirement/case IDs, design and code
revisions, test command, environment versions, fixture identity, observed outcome
and retained artifacts. Record designed, implemented, verified and released states
separately. An unavailable environment leaves its evidence pending.

| D0 exit item | Current state | Resolution needed |
| --- | --- | --- |
| First-release scope | Selected by owner on 2026-09-22: local CLI/TUI with on-device assistance, through I4 plus I9 qualification | Resolved; both storage modes remain required |
| Chat technology and default mode | Ratatui, Rust chat backend and default chat selected on 2026-09-23 | Resolve W0 terminal/startup details in D3/D6 |
| Processing architecture | Fully asynchronous and event-driven, with input/signal pipelines, selected on 2026-09-23 | Resolve ADR-0006 mechanisms and limits in D2-D6 |
| Workflows | W0 selected launch behavior; draft W1-W5 | Review workflow refinements and unsupported-capability behavior |
| Domain and identity semantics | Draft relationships and N1-N3 | Review conversation, overlap, relocation and attempt semantics |
| Measurable objectives | Proposed P0-P9; no measured baseline | Accept targets or revise from D1-D2 feasibility evidence |
| Traceability | R01-R21 and N1-N3 specified | Review completeness; D7 expands concrete tests and environments |

D0 is not marked complete while these decisions await review. D1 may gather
read-only platform evidence, but must not treat an unreviewed target as a proven
capability. The remaining D1-D8 mechanisms and implementation gate still apply.

## Documentation verification on 2026-09-22

An independent read-only review found three draft issues: observation cardinality,
ambiguity between overlapping locations within one context, and omission of I9
from the release-scope explanation. All three were corrected and rechecked.

Mermaid CLI 11.16.0 rendered all six diagrams in the three new D0 documents.
Each output was visually inspected; the workflow overview was simplified for
readability. All 184 local Markdown links and anchors passed. Whitespace checks
passed for the tracked changes and new documents. Generated previews remained
outside the repository. These are documentation checks only; no product code,
runtime tests, benchmarks or model evaluations were run.

# Implementation agent instructions

## Conduct and authority

- Be helpful and candid. Do not agree without examining the evidence.
- Act as an expert software engineer and apply architecture and testing best practices.
- If user instructions conflict with existing project guidance, rules, or
  instructions, stop work immediately and bring the discrepancy to the user.
  Do not silently choose an interpretation or change guidance to remove the conflict.
- Distinguish requirements, proposed designs, implemented behavior, and verified
  behavior. Do not present a proposal or passing mock test as runtime proof.

## Read before working

Read [architecture.md](docs/architecture.md),
[engineering.md](docs/engineering.md), and
[design-process.md](docs/design-process.md), plus the design governing the task.
These documents are the canonical locations for their respective subjects.

For documentation changes, follow [writing-standard.md](docs/writing-standard.md).
Use [glossary.md](docs/glossary.md) for project terms. Preserve the technical
conditions and required test coverage when simplifying text.

## Mandatory design gate

- Never write code without a design. This includes implementation, tests,
  scaffolding, build scripts, and implementation spikes.
- Before coding, identify the governing design and confirm it defines the
  relevant behavior, component ownership, interfaces, failure handling, security
  boundaries, and unit, integration, and end-to-end validation.
- If the design is missing or insufficient, do design work first. Resolve
  decisions needed for the scoped implementation before writing its code.
- The architecture baseline alone is not a detailed implementation design.
- When a change requires a design change, update the design before changing code.
- Architecture, design, plans, and specifications must include detailed Mermaid
  diagrams appropriate to their scope. Follow the diagram requirements in
  [design-process.md](docs/design-process.md#mermaid-diagram-requirements).
  Keep diagrams and prose consistent; diagrams do not resolve open decisions.

## Mandatory implementation practices

- Search for an existing owner of a capability before adding one. Extend the
  canonical implementation rather than creating another copy in an interface,
  agent, or transport adapter.
- Keep interface clients, orchestration, agent execution, and host enforcement
  within their documented responsibilities.
- Deliver the unit, integration, and end-to-end coverage required by the design.
  Untested behavior and missing test layers are incomplete work.
- Run the applicable checks. Report exactly what ran, what passed, and any
  unverified behavior. Do not disable checks or weaken assertions to obtain a pass.
- Keep designs, contracts, user documentation, and tests consistent with the
  delivered implementation.

Swift and Rust are the selected implementation languages. Component allocation,
transport, interoperation, build integration, and test runners must follow their
governing designs. Do not select them implicitly through scaffolding.

Use the [architecture and design plan](docs/plans/architecture-and-design.md)
before the [implementation plan](docs/plans/implementation.md). Planning proposals
are not implementation-ready designs.

## Use the available harness capabilities

- Start by inspecting repository state, scoped instructions, design status and
  ownership. Maintain a short dependency-aware plan for substantial work.
- Use native subagents for concrete independent research, adversarial review, or
  disjoint implementation packets when parallel work improves the result. Keep
  serial or small tasks local. Available roles and boundaries are documented in
  [repository-agent-configuration.md](docs/designs/repository-agent-configuration.md).
- Give each delegate the governing design, exact owned paths, non-goals, expected
  evidence and stopping conditions. Record ownership in the task plan or handoff;
  never let two writers own the same file concurrently. The primary agent owns
  integration and must inspect delegated results before accepting them.
- Batch independent searches and checks. Keep dependent operations, mutations,
  integration and publishing sequential. Reuse existing tool sessions for
  long-running work and keep the user informed while checks run.
- Search with `rg`; use targeted file/symbol reads and installed SDK evidence.
  Use current primary documentation for version-sensitive APIs and dependencies.
  Record versions and distinguish documentation claims from live verification.
- Discover applicable skills, APIs, connectors and local tools before inventing
  an integration. Read applicable skills and use purpose-built tools where they
  help. Availability is not authorization to access unrelated data or services.
- Use image viewing and local Mermaid rendering to inspect diagrams. Use browser
  or native UI tooling when a designed user workflow requires interactive proof.
  Do not claim visual correctness from syntax checks alone.
- Ask concise questions early when a missing decision changes the result; keep
  independent authorized work moving while awaiting an answer. Stop immediately
  for instruction conflicts as required above.
- Prefer narrow, evidence-backed review findings with file references, failure
  scenarios and affected contracts. Check authority, races, recovery, context
  provenance, data egress and missing test layers before cosmetic concerns.
- Use task-local notes and artifacts to preserve decisions during long work.
  Keep credentials, private prompts, machine-specific paths and generated previews
  out of tracked files. Do not modify personal memory or global configuration
  without explicit user instruction.

## Integration and publishing

- Preserve unrelated user changes. Review the complete staged diff and ensure
  only intended files, designs, configuration and validation evidence are included.
- Commit and push when requested. Check the branch, upstream and current remote
  tip first; use a normal fast-forward push. Do not force-push, rewrite history,
  bypass hooks, or change signing policy to work around a failure.
- Report the commit, destination, checks performed and remaining proof limits.
  A task is not complete while its authorized commit/push or required validation
  remains outstanding; explain concrete blockers accurately if encountered.

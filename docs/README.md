# Documentation

This directory explains Asura's intended architecture, engineering requirements
and development process. Its subdirectories contain detailed designs, decision
records and delivery plans. Start with the architecture, then read the design
process before planning a change.

Each document states its own status. A requirement, proposal or selected decision
does not establish implemented or verified behavior. The
[design process](design-process.md) governs implementation readiness and authority.

## Files

| Document | Purpose |
| --- | --- |
| [Architecture](architecture.md) | Required behavior, component ownership and trust boundaries; detailed mechanisms remain open. |
| [Coding standards](coding-standards.md) | Draft coding rules and enforcement mapping for owner review. |
| [Design process](design-process.md) | Required design contents, review and implementation gates, and diagram validation. |
| [Engineering standards](engineering.md) | Required testing layers, evidence and completion criteria. |
| [Glossary](glossary.md) | Shared project terms and links to their governing contracts. |
| [Writing standard](writing-standard.md) | Required practice for clear, precise technical documentation. |

## Subdirectories

| Directory | Contents |
| --- | --- |
| [Decisions](decisions/README.md) | Selected architecture decisions, rationale and recorded contract reviews. |
| [Designs](designs/README.md) | Workflows, component contracts, proposals and the isolated TUI experiment. |
| [Plans](plans/README.md) | Design stages, delivery dependencies and scoped implementation packets. |

The [visual reading path](decisions/README.md#visual-reading-path) connects the
architecture to detailed contracts and review gates. These directory indexes
provide navigation; the linked documents own the requirements and diagrams.
Maintain these indexes under the [directory README rules](writing-standard.md#directory-readmes).

Return to the [project overview](../README.md).

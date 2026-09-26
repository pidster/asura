# Asura

Asura is a coding AI harness for macOS 27 and later. It uses Apple's Foundation
Models API and on-device AI to guide orchestration and decide when to call remote
AI capabilities.

The default launch mode will be Ratatui chat with a Rust backend. Explicit
non-interactive CLI commands will also be available. Asura will use a fully
asynchronous, event-driven architecture with input and signal processing pipelines.
The architecture must also accommodate a GUI and control of remote machines
running Asura agent instances.

One interactive interface will navigate multiple projects, conversations, tasks
and agents. Its launch directory will not bind it to a single project. Background
work stays with the orchestrator while the user switches activities.

On a user device, one backend service runs per OS user and manages multiple
project and repository contexts. It discovers configuration in each working
directory and its parents, then combines applicable settings with explicit rules.

The production system is in design, using Swift and Rust for its main components.
An explicitly authorized [isolated TUI experiment](experiments/tui-chat/README.md)
now supports editor, multi-project and recovery trials with synthetic work.
Production implementation remains subject to reviewed designs and authorization.

- [Documentation directory guide](docs/README.md)
- [Agent instructions](AGENTS.md)
- [Architecture baseline](docs/architecture.md)
- [Engineering and testing standards](docs/engineering.md)
- [Draft coding standards and enforcement](docs/coding-standards.md)
- [Design process](docs/design-process.md)
- [Technical writing standard](docs/writing-standard.md)
- [Project glossary](docs/glossary.md)
- [Architecture decisions and visual review map](docs/decisions/README.md)
- [Architecture and design plan](docs/plans/architecture-and-design.md)
- [Runtime and asynchronous processing sketch](docs/designs/runtime-architecture.md)
- [Interaction and extension boundaries, proposed](docs/designs/interaction-and-extension-boundaries.md)
- [TUI prototype design and experiment plan](docs/designs/tui-interaction-prototype.md)
- [D0 product workflows and objectives](docs/designs/product-workflows.md)
- [D0 domain model](docs/designs/domain-model.md)
- [Requirements and validation matrix](docs/designs/requirements-validation.md)
- [Core harness design brief](docs/designs/core-harness-brief.md)
- [User service and hierarchical configuration](docs/designs/user-service-configuration.md)
- [Security policy design brief](docs/designs/security-policy-brief.md)
- [Implementation plan](docs/plans/implementation.md)
- [Repository agent configuration](docs/designs/repository-agent-configuration.md)


## Semantic development tools

The project configures `asura_lsp`, a pinned local Serena MCP bridge to installed
Rust Analyzer and Xcode SourceKit-LSP. It exposes read-only semantic queries.
Reconnect MCP or start a new Codex session after configuration changes. Verify
Asura is active before querying; explicit project activation handles app sessions
whose server starts elsewhere. Setup and smoke evidence are in the
[development tooling design](docs/designs/repository-agent-configuration.md#rust-and-swift-semantic-tooling).

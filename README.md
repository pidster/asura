# Asura

Asura is a coding AI harness for macOS 27 and later. It uses Apple's Foundation
Models API and on-device AI to guide orchestration and decide when to call remote
AI capabilities.

The default launch mode will be Ratatui chat with a Rust backend. Explicit
non-interactive CLI commands will also be available. Asura will use a fully
asynchronous, event-driven architecture with input and signal processing pipelines.
The architecture must also accommodate a GUI and control of remote machines
running Asura agent instances.

On a user device, one backend service runs per OS user and manages multiple
project and repository contexts. It discovers configuration in each working
directory and its parents, then combines applicable settings with explicit rules.

The project is in design, using Swift and Rust for its main components.
Implementation has not started. Design and the implementation plan will both be
reviewed before implementation is authorized.

- [Agent instructions](AGENTS.md)
- [Architecture baseline](docs/architecture.md)
- [Engineering and testing standards](docs/engineering.md)
- [Design process](docs/design-process.md)
- [Technical writing standard](docs/writing-standard.md)
- [Project glossary](docs/glossary.md)
- [Architecture decisions and visual review map](docs/decisions/README.md)
- [Architecture and design plan](docs/plans/architecture-and-design.md)
- [Runtime and asynchronous processing sketch](docs/designs/runtime-architecture.md)
- [D0 product workflows and objectives](docs/designs/product-workflows.md)
- [D0 domain model](docs/designs/domain-model.md)
- [Requirements and validation matrix](docs/designs/requirements-validation.md)
- [Core harness design brief](docs/designs/core-harness-brief.md)
- [User service and hierarchical configuration](docs/designs/user-service-configuration.md)
- [Security policy design brief](docs/designs/security-policy-brief.md)
- [Implementation plan](docs/plans/implementation.md)
- [Repository agent configuration](docs/designs/repository-agent-configuration.md)

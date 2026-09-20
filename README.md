# Asura

Asura is a coding AI harness for macOS 27 and later. It uses Apple's Foundation
Models API and on-device AI to guide orchestration and decide when to call remote
AI capabilities.

The first interfaces will be a non-interactive CLI and interactive chat/TUI.
The architecture must also accommodate a GUI and control of remote machines
running Asura agent instances.

The project is in design, using Swift and Rust for its main components.
Implementation has not started. Design and the implementation plan will both be
reviewed before implementation is authorized.

- [Agent instructions](AGENTS.md)
- [Architecture baseline](docs/architecture.md)
- [Engineering and testing standards](docs/engineering.md)
- [Design process](docs/design-process.md)
- [Architecture decisions and visual review map](docs/decisions/README.md)
- [Architecture and design plan](docs/plans/architecture-and-design.md)
- [Core harness design brief](docs/designs/core-harness-brief.md)
- [Security policy design brief](docs/designs/security-policy-brief.md)
- [Implementation plan](docs/plans/implementation.md)
- [Repository agent configuration](docs/designs/repository-agent-configuration.md)

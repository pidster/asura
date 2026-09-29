# Asura

Asura is a coding AI harness for macOS 27 and later. It uses Apple's Foundation
Models API and on-device AI to guide orchestration and decide when to call remote
AI capabilities.

The default launch mode is Ratatui chat with a Rust backend. Explicit
non-interactive CLI commands are also available. Asura requires a fully
asynchronous, event-driven architecture with input and signal processing pipelines.
The architecture must also accommodate a GUI and control of remote machines
running Asura agent instances.

One interactive interface will navigate multiple projects, conversations, tasks
and agents. Its launch directory will not bind it to a single project. Background
work stays with the orchestrator while the user switches activities.

On a user device, one backend service runs per OS user and manages multiple
project and repository contexts. Current configuration uses the account's
`$HOME/.asura/config.yaml`. Hierarchical discovery and composition of directory
settings remain intended behavior.

The production system uses Swift and Rust. The first real Rust service and CLI
are implemented for a developer trial. They support start, status, stop and
foreground operation through a local Protobuf control socket.
An explicitly authorized [isolated TUI experiment](experiments/tui-chat/README.md)
now supports editor, multi-project and recovery trials with synthetic work.
The service supports automatic fresh embedded database initialization, project registration,
and one active local-model conversation. Accepted turns and final responses use
the durable format-1 journal. Restart recovers recorded outcomes without repeating
model calls. The composer shows the context working directory and service-observed
Git branch, changed-file count and added/deleted line counts. Filesystem signals update Git status without
terminal input. System-model project and status tools are available; additional
tool classes and multi-agent scheduling remain in development.

## Try the production TUI

```sh
./target/debug/asura
```

The TUI uses the experiment's editor and layout with real service status. It
attaches to an existing service. If the backend is absent, it starts one as a
child through the standard service launcher. A backend started by a TUI stops
when that launching TUI exits; its logs go to `$HOME/.asura/logs/asura.log`.
A backend started separately (for example, at OS startup) remains running when
attached TUIs exit. Each TUI launch makes at most one automatic start attempt. Startup failures leave editing and
local commands available. An existing service keeps its current log destination.

On launch, the TUI displays registered project names and selects the project
containing the launch directory. For nested projects, the closest match wins.
If no directory matches and only one current project exists, it selects that one.
The selected name has a `*` marker. After selection, Enter submits a conversation turn.
The draft remains until durable acceptance; model output streams into the log pane.
Without a selected project, Enter shows setup instructions and retains the draft.
Alt+Enter adds a newline. F1 or `/help` shows help. `/quit` and `/exit` close
the client and release its owned backend, if any. A pre-existing backend stays
running. These three commands take no arguments. Unknown
commands retain the draft and show an error. Tab completes `/he`, `/qui`, `/ex` or `/con`;
Tab after `/` opens the local command list. Arrows select and Tab or Enter
completes; press Enter again to run the command. Elsewhere Tab inserts two spaces.
Ctrl+Q exits; with a draft, select
**Discard and exit** to confirm. Drafts are local to this TUI session and are not
saved on exit. Completed conversation history is stored by the service.

The current client uses protocol 0.1. Stop an older service with its previous CLI
before starting this build; a protocol mismatch leaves the old service running.

### First conversation

The current developer package includes `asura-model` beside the `asura` binary.
A fresh installation initializes automatically. If no projects exist, the TUI
asks whether to use the launch directory as a project. Choose Yes to register and
select it, or No to leave it unchanged. To add a project later, run:

```text
/project add .
```

Press Escape to close each result panel. Project registration selects that project.
Then type a message and press Enter. The unset model defaults to `system`.
To try the system model explicitly, run `/config set model system` first.
`/cancel` requests cancellation. `/project list` shows registrations;
`/project select ID` selects an existing one. `/observe OPERATION_ID` recovers a
recorded operation. `/retry` retains the original request ID after an uncertain
request outcome; it does not allocate another turn.

During active work, Enter queues input without a choice dialog. Queue waits for
successful completion. Select a queued input and press Enter to send it now;
this stops the active generation before starting its replacement.
Ctrl+T queues and Ctrl+S steers. `/queue` lists queued inputs; `/queue ID` shows
one entry. Use `/queue resume ID` or `/queue drop ID` for held work. The service
stores accepted inputs durably; restart holds queued work for an explicit decision.

At the bottom of the input, press Down again to enter the status row. Left, Right
or Tab selects the project or model; Enter opens its selector. Escape returns
without applying a selection. Selected items use a lighter background and brighter
text. The bottom row shows the available keys. Ctrl+P and Ctrl+N browse input history.

System Foundation Models, Ollama and MLX have passed scoped native conversation
checks. CoreAI has passed the native Memory create/read/source/restart journey.
See the [provider design](docs/designs/model-provider-integration.md) for exact limits.
One operation may run at a time, with a 2,048-token output reservation.
Tool turns share that reservation across at most three inference passes.
Supported local models can read project files, list project directories and observe
service status through bounded built-in tools. Read-only Memory tools list stored
notes, read note pages and inspect source links within the admitted project.
The system, verified-local Ollama and MLX Memory journeys have passed; provider-specific
limits are recorded in the [Memory design](docs/designs/hybrid-memory-ontology.md#hm2-implementation-evidence).
Ask the model to list the project's stored notes. An empty store returns an empty
list. The `memory` tool also implements `create_note` for foreground local-model
turns. It stores an immutable note and can link it to an existing source version
in the same project. Write recovery uses durable intents and database receipts;
native write qualification is recorded separately in the Memory design.
The `shell` tool runs one noninteractive `/bin/sh` command in the selected project.
It returns bounded stdout, stderr and exit status. `cwd` is project-relative;
`timeout_seconds` defaults to 30 and accepts 1–60, within the remaining turn budget.
The sandbox permits project and private scratch writes and denies network access.
For example, ask: “Use shell to run pwd and git status --short.”
The system-model shell journey is verified; other local adapters have shared
contract tests but no shell-specific native qualification yet.
The service records tool results before continuing model generation.
`/init` checks setup explicitly; it is not required for normal first use.
Startup preserves existing unknown, incomplete or damaged state.

### Configuration

The TUI supports `/config`, `/config get name` and `/config set name value`.
With no arguments, `/config` displays the effective YAML configuration, including
defaults. An unset model is omitted. It does not change the file. Dotted names
select nested settings in `$HOME/.asura/config.yaml`:

```text
/config set model system
/config get model
/config set audit.enabled true
/config set audit.keepFiles 5
/config set audit.maxFileBytes 10485760
/config get audit
```

Provider selectors are `system`, `ollama:MODEL`, `mlx:NAME` and `coreai:NAME`.
MLX and CoreAI require installed assets below `$HOME/.asura/data/models/mlx/NAME`
and `$HOME/.asura/data/models/coreai/NAME`; Asura does not download missing assets.
Ollama defaults to `http://127.0.0.1:11434`. Set `providers.ollama.endpoint` to
select another endpoint. Remote endpoints require HTTPS; authenticated endpoints
need credential support that is not yet implemented.

Values use YAML syntax. Quote strings containing spaces or YAML punctuation.
The audit defaults are the values above; `model` starts unset. Get/set validates
keys and types. Successful writes normalize the YAML and do not preserve comments.
Malformed or unsafe files are rejected without overwriting them. Config files
must be private regular files (mode 0600). Results appear in a padded bottom panel;
scrolling is enabled only when the content does not fit.
Escape closes it. Failure retains the command draft. If the outcome is unconfirmed,
use `get` before deciding whether to retry.

These commands persist settings. Model changes apply to the next admitted turn.
Audit changes apply after service restart. The recorder rotates files using the active
settings. See the [config contract](docs/designs/config-commands.md).

## Try the real service

From a checkout with the developer binary built:

```sh
./target/debug/asura service start
./target/debug/asura service status --json
./target/debug/asura service stop
```

Run the status command from another terminal to inspect the same service.
Repeating start attaches to the existing owner. Restarting after stop creates a
new service epoch. For foreground operation, use `asura service run` through the
same binary path, then press Ctrl+C to stop it.

Service events use Rust `tracing`, with timestamps and levels on stderr.
To append logs to a file, add `--logs DIR` to any service command:

```sh
./target/debug/asura service start --logs ./logs
```

This writes to `./logs/asura.log`. It preserves existing contents and does not
rotate the file. `service status --json` keeps JSON on stdout. A repeated start
does not change the running service's log destination. A service started without
`--logs` keeps the terminal's stderr open until it stops.

The service resolves the account home and uses `~/.asura/run/`. It creates only
runtime state when no installation exists. The service then initializes verified
fresh state through the durable writer and verifies the embedded graph. An existing conversation installation is reopened and recovered;
inspection reports `graph_ready` after binding verification. Unknown or damaged
state remains unavailable and is never silently repaired.
Bare `asura` opens the production TUI in a terminal.

Inspect installation state after starting the service:

```sh
./target/debug/asura installation status
./target/debug/asura installation status --json
```

The command attaches to an existing service. It does not start one or create
installation state. `--logs DIR` works with this command too. A successful
inspection means the state was read. Only `graph_ready` confirms the current
verified binding.

Control protocol 0.1 requires matching client and service binaries. Stop a running
1.0 service with its previous CLI before starting the new binary. A version
mismatch reports `incompatible_protocol` and leaves that service running.

Build with installed Rust 1.98.0 and an explicit protoc 36.2 executable verified
against the pinned archive in `tools/protobuf/lock.json`:

```sh
ASURA_PROTOC=/absolute/path/to/verified/protoc cargo build --locked -p asura-cli
```

The current workspace has a verified compiler at `.build/dev-tools/protoc`.
Use `ASURA_PROTOC="$PWD/.build/dev-tools/protoc"` for that local build.
Cargo generates control bindings directly. This developer trial does not require
portable dependency snapshots or the custom build driver. Native conversations
also require the assembled [Swift helper](swift/model-helper/README.md) and its
compiled package identity. A plain Rust build without that identity still supports
service/configuration operations and reports the model package as unavailable.

The foundation tests, lint check and real CLI lifecycle trial have passed.
Full host and release qualification remains incomplete. See the
[foundation packet](docs/plans/production-foundation-implementation.md) for exact
evidence and remaining checks.

Run the standard CLI tests, including real terminal command regressions:

```sh
ASURA_PROTOC="$PWD/.build/dev-tools/protoc" cargo test --locked -p asura-cli
```

The lifecycle test automatically runs the existing terminal journeys and setup
checks for repeated initialization, project commands and restart persistence.
These checks require macOS, Python 3, PTYs and local socket access. They use
private temporary homes and stop their test services. They do not call a model.
Missing prerequisites fail the test. Native inference has its separate journey.

## First build-driver check

The approved [PB0.0–PB0.3 packet](docs/plans/protobuf-bootstrap-implementation.md)
provides the first build check on macOS 27 with Apple silicon and an installed
Rust 1.98.0 compiler:

```sh
scripts/check-i0-toolchain --check driver
```

The check compiles the standalone driver and runs its lock, process and command
tests. It includes a real 120-second launcher-timeout test. It uses no Cargo
dependencies and must not download a missing compiler.

The bootstrap library also has a scoped check:

```sh
scripts/check-i0-toolchain --check bootstrap
```

This runs a locked, offline build, Clippy and bootstrap tests. It requires
the complete installed Rust 1.98.0 toolchain and reviewed Cargo inputs at
`.build/asura-deps/cargo/work`. It does not acquire missing dependencies.

### Scoped cache preparation

PB0.3 provides these preparation commands under the same driver:

```sh
scripts/check-i0-toolchain --check preparation
scripts/check-i0-toolchain --check preparation-offline
```

The first command permits network access to acquire locked Cargo dependencies
and pinned Protobuf tool archives. It verifies archives, builds the Swift
generator and prepares the current bootstrap Cargo snapshot. It requires the
installed Rust and Apple toolchains specified by the
[preparation design](docs/designs/protobuf-cache-preparation.md).

The offline command requires existing tool inputs and a prepared Cargo snapshot
at `.build/asura-deps/cargo/snapshot`. Its preparation children run with
OS-enforced network denial; it cannot populate an empty checkout from the network.
Both commands validate their inputs and use the worktree's managed cache paths.

A successful preparation reports `bootstrap-only`. This qualifies the current
bootstrap target, not the final Rust–Swift smoke exchange or portable snapshot.
The commands are available; this README does not claim a successful live
preparation run. The [implementation packet](docs/plans/protobuf-bootstrap-implementation.md)
records validation evidence and remaining checks.

PB0.4 and later work remains unavailable. The final normal invocation, `--offline`
and `--prepare-only` interfaces await the complete smoke-target contract.
Production control generation and service commands use the separate developer
path above. Full bootstrap qualification remains deferred.

One check can run in a worktree at a time. If a previous run left uncertain child
state, the next check reports `cleanup_required`. Inspect and stop that run's
remaining processes before removing its reported incomplete marker. Do not
delete the marker merely because its parent process has exited.

## Project documentation

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

### Audit inspection

Run `/audit [limit]` to inspect recent audit metadata for the selected project.
The default limit is 16 (maximum 16). The panel reports active audit settings,
health, recorded gaps and the bounded recent window; it does not browse archives.
Audit setting changes activate after service restart.

### Tool inventory

Run `/tools` to show registered tool names and descriptions in a padded table.
Registration does not mean a tool is permitted for the current model or project.
Escape closes the panel; arrows scroll when the content exceeds its height.

### Model inventory

Run `/models` in the chat client to list the service's model inventory. The table
marks the configured selection with `*` and reports provider failures below it.
`available` means the system SDK reported availability. `installed` means local
metadata was found; `listed` means Ollama returned the model in its catalog.
Neither state verifies inference. Missing selections remain visible as unchecked
or unavailable rows. The command does not select, download or load a model.
Use `/config set model SELECTOR` to select a model for subsequent work.

### Response activity and grouped tools

The conversation view shows service-reported tool activity while a response runs.
Model responses render common Markdown formatting in the terminal, including bold,
italic, headings, lists, code and links. The input and activity log remain literal.
Press **F6** to browse retained responses, **Up/Down** to select, **Enter** to expand
activity, **PageUp/PageDown** to scroll, and **Escape** to return to your draft.
This view retains the current TUI session's last eight responses. Explicit `/observe`
can retrieve an operation from the persistent journal; a past-session browser is
not yet available.

Models use four typed tools: `project` (file reads/listing), `memory` (note reads/creation),
`service` (status/tools/audit), and `shell`. `/tools` displays their commands.
Each command keeps its existing permissions and durable operation records.

The top bar reports the connection type, service uptime, sampled stored-memory
footprint and verified graph status. `process` means this TUI owns the backend's
lifetime; `local` means it attached to an independently started local service.
Stored size includes embedded database files, not RAM, models or diagnostic logs.
Unavailable or stale observations remain labelled.

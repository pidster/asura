# Asura CLI

Follow the root and Rust instructions and the foundation service contract in
`docs/designs/system-architecture.md`. Own arguments, presentation and exit codes.
Delegate service lifecycle to the client and service crates. No release argument
or environment variable may override the account runtime path.

The selected [thin TUI contract](../../../docs/designs/early-production-status-slice.md#selected-thin-production-tui)
governs bare interactive launch. Reuse the bounded editor and terminal guard;
consume real service observations through the client. Keep synthetic experiment
models and task actions out of this production client. Restore the terminal before
waiting for the observation worker, and preserve local drafts on unavailable actions.

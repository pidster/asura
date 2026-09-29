# Local control client

Follow the root and Rust instructions and the foundation service contract in
`docs/designs/system-architecture.md`. This crate owns attachment, correlation,
bounded startup and stop reconciliation. Use the control codec and platform
handles. Keep CLI presentation and service dispatch outside this crate.

# Policy admission

Follow the root and Rust instructions and the governing
[security policy design](../../../docs/designs/security-policy.md), in
particular its PL3 implementation contract ("PL3 admission", "PL3 compile
step", "PL3 solver adapter").

This crate owns the admission of one candidate scope text: bounds, parse,
strict validation, lints, the SymCC proofs SP-AQ1 to SP-AQ9, the PL3
compile checks and the change difference.

- Keep it synchronous with no I/O, no clock reads for decisions, no threads
  and no unsafe code. The solver is reached only through the injected
  `SolverPipe`; path literals are checked only through `LiteralCheck`.
- Depend on `asura-policy` and the pinned Cedar crates only.
  `asura-platform` is a dev-dependency for the real-solver tests and the
  bench example; production code never uses it.
- Never add a time limit to a query. A solver failure, `unknown` or a cancel
  refuses the candidate; nothing is retried.
- Counterexamples and solver output reach the person only, never a model,
  a journal record or an audit event.
- Real-solver tests fail when the development package is absent: an absent
  solver is a gap, never a pass.

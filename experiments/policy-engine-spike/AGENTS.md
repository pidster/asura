# Policy engine spike

Read the root instructions and the [spike design](../../docs/designs/policy-engine-spike.md)
before changing this package. The design owns the corpus, the wrapper contract,
the budgets and the results; this package does not implement a product policy
component and selects no engine.

- Keep the spike out of the product workspace. Do not edit the root `Cargo.toml`.
- Use synthetic corpus data only. Do not read `~/.asura`, the Keychain or any
  real configuration. The binaries open no network connection.
- Keep the Cedar and Rego encodings equivalent. Change both, and the design's
  side-by-side text, in the same change.
- Do not change a budget after measurement. Record a failure as a failure.
- Bound benchmark loops by iteration counts, never by wall-clock deadlines.
- Forbid unsafe code. Pin direct dependencies and keep `Cargo.lock`.
- cvc5 lives in a scratch directory outside the repository and is named by the
  `CVC5` environment variable. Never commit its path or binary.

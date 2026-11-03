# Policy engine spike: Cedar and Rego

An isolated measurement spike for the [policy engine spike design](../../docs/designs/policy-engine-spike.md).
It runs one synthetic Asura policy corpus through Cedar (`cedar-policy` 4.14.1,
with `cedar-policy-symcc` 0.8.1 and cvc5 1.3.1 for admission analysis) and
through Rego (`regorus` 0.14.1). It is not product code and selects no engine.
The design holds the budgets and the [results](../../docs/designs/policy-engine-spike.md#results).

## Layout

| Path | Contents |
| --- | --- |
| `corpus/asura.cedarschema` | The Cedar schema |
| `corpus/cedar/`, `corpus/rego/` | The 38 core policies in each language, by scope; the Rego helper and combiner |
| `corpus/conformance.json` | 64 conformance cases with expected decisions and rule IDs |
| `src/model.rs` | Typed request model; its JSON form is the Rego input |
| `src/store.rs` | Entity store with Cedar and Rego encoders |
| `src/corpus.rs` | Core entities and the deterministic scale generator (220 policies per language, 10,325 entities, 1,000 requests) |
| `src/cedar_engine.rs`, `src/rego_engine.rs` | The fail-closed wrappers |
| `src/admission.rs` | SymCC queries AQ1 to AQ9 (feature `admission`) |
| `src/bin/` | `bench`, `startup`, `fuzz`, `decisions`, `admission`, size probes, and the `breakdown` diagnostic |
| `tests/` | Conformance, differential, fail-closed, schema defects, determinism, admission |
| `results/` | Raw outputs of the recorded measurement run |

## Checks

Run from this directory with Rust 1.98.0:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features admission -- -D warnings
cargo test --locked --release
CVC5=<cvc5 1.3.1 binary> cargo test --locked --release --features admission --test admission
```

The admission test fails when `CVC5` is not set. A missing solver is a
validation gap, never a pass.

## Measurements

```sh
cargo run --locked --release --bin bench -- cedar-b    # also cedar-a, rego
cargo run --locked --release --bin startup -- cedar    # also rego; wrap in /usr/bin/time -l
cargo run --locked --release --bin fuzz -- cedar 1000000
cargo run --locked --release --bin decisions
CVC5=<cvc5 1.3.1 binary> cargo run --locked --release --features admission --bin admission
cargo build --profile size --bin size_baseline --bin size_cedar --bin size_rego
```

Every loop ends by an iteration count; nothing ends by a wall-clock deadline.
cvc5 comes from the pinned, digest-checked scratch download that the design
approves. It is never installed system-wide or committed.

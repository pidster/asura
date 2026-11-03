# Effect policy

Follow the root and Rust instructions and the governing
[security policy design](../../../docs/designs/security-policy.md), in
particular its PL1 implementation contract.

This crate owns the Cedar schema `asura-policy/1`, the built-in installation
and default user sets, the deny-on-any-error wrapper, pattern A scope
composition, reason codes and the `model-1` region compiler.

- Keep it pure and synchronous: no I/O, no clock, no threads, no unsafe code.
  The service owner `asura-service::policy` supplies every trusted attribute
  and the time, and runs each engine on its own worker.
- Depend on no Asura crate. Pin `cedar-policy` exactly; a version change
  changes the revision digest and needs the design's upgrade rule.
- Every failure denies with a reason. Never weaken W1 to W3, strict
  validation or the warnings-refuse rule to obtain a pass.
- The built-in texts change only with the design. A change to the `model-1`
  regions changes confinement: the golden profile tests in the service and
  platform must still prove the rendered profile.
- The test-only denying set exists only under the `test-support` feature.
- The bench (`policy-bench`) bounds its loops by counts, never by time.
  Record budget failures as failures.

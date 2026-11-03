# ADR-0013: Cedar effect policy, decided per scope, proved by a bundled cvc5

Date: 2026-11-03. Status: selected by the owner on 2026-11-03 (AD-13, AD-14,
AD-17 and AD-27 in the [owner decisions table](../designs/assurance-and-differentiation.md#owner-decisions)).
The [security policy design](../designs/security-policy.md) owns the detailed
contract and its validation. Nothing in this record is implemented.

## Context

The [security policy brief](../designs/security-policy-brief.md) requires one
documented, versioned policy format, a canonical evaluator and host
enforcement. The [policy engine spike](../designs/policy-engine-spike.md)
compared Cedar and Rego on one corpus. Cedar passed the safety and analysis
budgets and failed latency, startup and size as first designed; Rego lost
forbids silently on typos and missing data and has no analyser. The spike
wrote its corpus over commands. A shell reinterprets command text, so a rule
over a command line cannot be the authority for what a job does.

## Decision

1. **Cedar decides; host services enforce** (AD-13). Asura pins
   `cedar-policy` exactly and wraps it so that any evaluation error denies.
2. **Pattern A** (AD-14). Each scope (installation, user, workspace,
   repository; later remote host) is evaluated separately. A request is
   allowed only if every scope allows it. A refusal names the denying scope.
3. **Effects are the authority** (AD-27). The schema is written over
   resources and operations: file reads, writes, creations and deletions on
   canonical paths; program execution by canonical path and code-signing
   identity; network connections; secret use; Mach lookups if needed.
   Commands, MCP tools and skills are intent: context that may only tighten.
   Envelopes (AD-24) are effect sets.
4. **Today's enforcement is a per-job Seatbelt profile** compiled from the
   effect policy, under the rule that the backend allows a subset of what
   the policy permits. Endpoint Security and Network Extension are proposed
   for later and need research.
5. **Admission proofs run on the person's Mac** with a cvc5 that Asura builds
   from source with a dynamic or replaceable GMP, signs and notarises inside
   `Asura.app` (AD-17). The solver has no default time limit; it ends when its
   input closes or on cancel.
6. **Revised budgets**: decision p50 at most 100 µs; end-to-end p99 at most
   1 ms at 64 concurrent requests; at most 8 MiB added size; startup at most
   200 ms; with one policy set per action group and one engine copy per
   worker.

## Follow-up owner decisions (2026-11-03)

The owner answered the design's five open questions on 2026-11-03 (AD-28 to
AD-32 in the [owner decisions table](../designs/assurance-and-differentiation.md#owner-decisions)):

1. **Task envelopes wait for a task design** (AD-28). Packet PL8 depends on a
   new design that defines the task entity.
2. **The default "ask first" intent list** (AD-29): `git push` with its force
   flags, publish commands, privilege tools, hard-to-undo Git commands and
   unparsed text. Packet PL7 ships it as tighten-only rules.
3. **An LGPL-free cvc5 build is probed first** (AD-30). The solver packaging
   in decision item 5 waits on that probe and may change after it.
4. **The 8 MiB size budget covers the `asura` executable only** (AD-31). The
   solver's size is reported for each release, with no limit.
5. **No replacement solver for now** (AD-32). Only the bundled, signed solver
   runs.

## Consequences

- A repository policy can only narrow, by construction of the AND.
- A command's text never grants authority. An approval shows the person the
  intent and the effects it allows.
- Enforcement is static for the life of a job. A tightening during a job
  revokes or pauses it; nothing widens a running job.
- The bundle gains cvc5 and its GMP libraries, an LGPL-3.0 notice and a
  source offer. The release checks must cover them.
- Asura owns canonicalisation, code-signing identity, the command parser (for
  intent) and the entity loader. They are trusted code outside every proof.

## Alternatives

Rego (`regorus`) was not selected: 2 unenforced conventions, no analyser.
Forbid-only lower scopes in one merged set (pattern B) was not selected; the
owner chose per-scope evaluation so that a refusal names its scope. A
command-level policy, as in the spike corpus, was rejected by AD-27. A
developer-only or remote solver was rejected: it removes proofs on the
person's Mac.

# ADR-0011: Decouple tool output from model input

Date: 2026-10-12. Status: direction selected by the owner. Mechanisms, limits and
record encodings remain proposals in the [tool job controller](../designs/tool-job-controller.md)
design. Nothing is implemented.

## Context and decision

The [exec_cmd tool](../designs/exec-cmd-tool.md) runs one command for at most 60 seconds
within its turn. It returns bounded output tails directly into the model result.
Long builds, tests and servers either fail the deadline or flood model context.

Asura runs long-running tools as service-owned **tool jobs**. A job controller in
the Rust service owns job lifecycle and retains complete output. The model receives
a bounded preview and a job handle. It reconnects later to read pages, wait,
inspect status or cancel. The person can attach to the same job from the TUI.

The owner selected three behaviors on 2026-10-12:

1. Jobs are conversation-scoped. A job outlives the turn that started it.
   Service restart interrupts running jobs; they are never restarted.
2. Completion while idle is recorded and surfaced only. It never starts a turn.
   The next turn receives a short job notice without output.
3. A starting call waits for a short yield window. It returns either the outcome
   with a bounded preview, or the handle with progress counts.

## Consequences

- Model context contains only output the model requested, within existing
  per-turn tool-call and result limits.
- Effects can continue after their turn ends. The original execution grant,
  sandbox and descendant limits still bound them. The person can see and cancel
  every live job.
- Result retention must support reads while output is still arriving.
- The exec_cmd contract that stops a command at turn end or steering is superseded
  for jobs when this design is implemented.

## Alternatives

Turn-scoped jobs, automatic follow-up turns, handle-only results and interactive
sessions were considered. The [design](../designs/tool-job-controller.md#alternatives-considered)
records why each was not selected.

Return to the [decision index](README.md).

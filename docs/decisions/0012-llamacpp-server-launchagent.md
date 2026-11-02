# ADR-0012: A llama.cpp server Asura starts through a LaunchAgent

Date: 2026-11-02. Status: selected under the owner decision of 2026-11-02 ("Yes,
with Asura's own server"). The [llama.cpp backend design](../designs/llamacpp-backend.md)
owns the detailed contract and its validation.

## Context

The owner asked for a `llamacpp:` backend and for Asura to run and supervise its
own `llama-server` router, like Wisp's ADR 0082: a LaunchAgent, a key and a model
folder. Asura already has one per-user service. That service is standalone and may
be owned by a TUI, so it can stop when its TUI exits. The Rust instructions forbid
another process supervisor in the service.

## Decision

1. Asura's server is a LaunchAgent, `org.pidster.asura.llamacpp`, that runs
   `asura llamacpp serve`. The serve step checks the mode, the approved binary,
   the model folder and the key, then replaces itself with `llama-server` in
   router mode on `127.0.0.1`. launchd restarts it after a crash or a kill.
2. The person registers and removes the agent with `asura llamacpp start` and
   `stop`. Tests never register an agent; they run the same server plan as a
   child of the test in a scratch home.
3. The key lives in one team Keychain item. It reaches `llama-server` through a
   pipe and reaches requests through a relay in the service. The model helper,
   which runs inference (class B), never holds it.

## Consequences

- A model load survives service and TUI restarts.
- Asura gains a long-lived process outside every sandbox. Only an approved binary
  with its recorded digest starts.
- Asura's own server needs a signed build, because the key needs the team
  Keychain store.
- The service never starts, stops or drains `llama-server`.

## Alternatives

A child process of the per-user service was rejected: it adds a second process
supervisor, couples model loads to the service lifetime and adds the server to
the service's shutdown drain. `SMAppService` registration from `Asura.app` is an
open owner question (LCQ2).

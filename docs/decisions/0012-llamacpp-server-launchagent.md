# ADR-0012: A llama.cpp server Asura starts through a LaunchAgent

Date: 2026-11-02. Status: selected under the owner decision of 2026-11-02 ("Yes,
with Asura's own server"); revised 2026-11-02 with the owner decisions LCQ1 and
LCQ2 and the CORS fix; revised 2026-11-03 with the owner decision LCQ3 (a key
for every start). The [llama.cpp backend design](../designs/llamacpp-backend.md)
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
   `stop`. These commands write the agent file in `~/Library/LaunchAgents` and
   run `launchctl`. Asura does not use `SMAppService` (owner decision LCQ2,
   2026-11-02: "keep the LaunchAgents file plus `launchctl`"). Tests never
   register an agent; they run the same server plan as a child of the test in a
   scratch home.
3. The serve step makes a fresh key at every start and stores it in one team
   Keychain item, replacing the last one. The key reaches `llama-server`
   through a pipe and in its environment as `LLAMA_API_KEY`, which the router
   passes to its model processes, so they need it too (owner decision LCQ3,
   2026-11-03). It reaches requests through a relay in the service. The model
   helper, which runs inference (class B), never holds it.
4. The server plan closes CORS on the router. The router passes the CORS
   arguments to its model processes, so no web page can read a reply from them
   (2026-11-02 fix; the design's LCD16).
5. The router stays on loopback TCP. `llama-server` 0.7.0 can put the router on
   a Unix socket, but its model processes always listen on TCP (probes P19 to
   P21, 2026-11-03), so a socket would not close their ports.

## Consequences

- A model load survives service and TUI restarts.
- Asura gains a long-lived process outside every sandbox. Only an approved binary
  with its recorded digest starts.
- Asura's own server needs a signed build, because the key needs the team
  Keychain store.
- The service never starts, stops or drains `llama-server`.
- The agent does not show in Login Items as an app-registered item. The person
  removes it with `asura llamacpp stop`.
- The model processes behind the router need the key, because it is in their
  environment. A web page can no longer make them run inference blind.
  Same-user processes can read the key from that environment while the server
  runs; before LCQ3 they could use the model processes with no key, so this
  exposes nothing new. The key changes at every start.

## Alternatives

A child process of the per-user service was rejected: it adds a second process
supervisor, couples model loads to the service lifetime and adds the server to
the service's shutdown drain. `SMAppService` registration from `Asura.app` was
rejected by the owner on 2026-11-02 (LCQ2). It needs a signed app and Swift or
Objective-C code in the app, and the file plus `launchctl` already meets the
need.

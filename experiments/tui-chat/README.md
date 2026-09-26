# Asura chat interaction experiment

An offline, synthetic TUI for exploring the [prototype design](../../docs/designs/tui-interaction-prototype.md).
Explore two synthetic projects, explicit Steer/Queue choices, changing notices,
decisions and recovery without losing the draft you are writing.
It does not connect to a model, run tools or read project content. Drafts exist
only in memory; exiting or resetting discards them after explicit confirmation.

## Intended interaction and test machinery

The editor, contextual status bar, message tray, full-text inspection and styled
user-message history are candidate product interactions. Draft preservation,
explicit Steer/Queue and project navigation are intended behavior. Their final
presentation remains subject to usability review; Ctrl+S/Ctrl+T are interim
submission bindings while Return combinations are investigated.

Studio/Observatory, canned replies, progress and F2–F8/F10 controls are synthetic test
machinery. `--manual`, `--tour`, `--keys`, fault injection and prototype/fixture
instructions are diagnostic surfaces. They are not part of the intended everyday
composer. No real orchestrator, agent execution or durable storage is connected.

The [command interaction trial](../../docs/plans/command-interaction-trial.md) adds
an offline catalogue with client-local `/quit` (`/exit` is its alias) and `/help`
commands, plus synthetic extension and Skill entries. `/ext:core/check` is an inert
Asura-supplied Extension; `/ext:checks/test` is integration-supplied. Both use
the same fixture request path. It tests discovery and submission
without installing an extension, loading a Skill, or running real work.

## Run the prototype

Build and run from this directory with Rust 1.98.0:

```sh
cargo run --locked
```

The dark palette uses Wisp's blue tones. The light palette retains Asura's
earlier blue values. Both use the terminal's default main background.
Input and submitted-message bands remain shaded. Use `-- --light` for a light terminal.
Enter sends when idle; during work it opens
an explicit Steer/Queue choice. **Option+Return inserts a newline**.
During work, **Ctrl+S steers** and **Ctrl+T queues** the draft directly.
The status bar below the editor shows project/Git context on the left and
model/context status on the right. A nonempty draft adds its character count;
delivered running work adds a spinner. The message tray
above it shows pending, queued or held text and steering for the current task.
An idle composer reserves no information rows above its input. Notices and unread
or collapsed-message cues appear only when needed; project identity stays below.
**Ctrl+B** opens all retained messages, including completed ones, in full text.
Accepted user messages in the main transcript now use the input pane's spacing
and prompt marker with a slightly darker band. New turns, Steer and Queue share
that treatment; fixture replies keep the base background. Both palettes and
compact layouts follow the [transcript styling design](../../docs/designs/tui-transcript-style.md).
New turns show only their text; Steer and Queue entries retain those action labels
without a `You` prefix.
Half-cell edges provide the spacing around full-layout user messages; no extra
blank row is added beside those edges. Blank lines inside a message are preserved.
F1 shows all controls. Ctrl+Q requests exit. Use the terminal's native selection
and Command+C to copy visible output; mouse capture is disabled.

Bracketed paste inserts the whole payload without sending. For input paths without
bracketed paste, **Ctrl+V starts plain-text capture**. Paste, then press Ctrl+V
again to review. Tab selects Insert or Cancel; Enter confirms that selection.
Escape from the review cancels capture. During capture Enter adds a newline;
other control input invalidates the candidate without changing the draft.
Unbracketed control sequences cannot reliably be distinguished from deliberate
keys. Use bracketed paste for arbitrary payloads; fallback capture is for plain text.
Pasted line endings are normalized to LF, including Unicode line/paragraph
separators. Tabs become two spaces. Other control characters reject the whole paste.

Validation commands and remaining evidence are recorded in the
[implementation packet](../../docs/plans/tui-prototype-implementation.md).

## Project and model status

At full width, the status bar follows this shape:

```text
Studio · ~/src/studio · main · +24-8 · 12 chars ⠋       18% used · demo v1 fast
```

Fields use Wisp-style muted dots, without pipe separators or bracket wrappers. The project
name uses Wisp's status blue in dark mode; added lines are green and deleted
lines red.

Paths, branches, line counts, Git operations, model identity and context percentages
are **synthetic fixture data**. The trial does not inspect a repository or contact
a model. Studio and Observatory have different metadata so switching is visible.
`% used` means conversation context-window usage. Disconnecting makes Git/context
observations unknown instead of showing zero; configured identity remains visible.

Counts use user-perceived characters: combined accents and joined emoji count
once; spaces and newlines count too. Empty drafts omit the count. Work animates
independently of drafting, pending follow-ups and focused overlays; a decision or
unavailable state stops the spinner. The editor remains independent of Git state.

Narrow layouts shorten the metadata. Below 60 columns they omit path/Git detail,
use `Nc` for characters and `F` for fast mode. Expanding restores full fields.
F1 lists controls; Enter retains its send-or-choose behavior. The
[status design](../../docs/designs/tui-project-status.md) defines fitting and validation.

## Everyday controls

| Key | Action |
| --- | --- |
| Enter | Send while idle; choose Steer or Queue during work |
| Ctrl+S / Ctrl+T | Explicitly Steer / Queue for the task shown in the interface |
| Ctrl+B | Inspect retained submitted messages and their full text |
| Option+Return | Newline |
| Ctrl+P | Switch Studio / Observatory, retaining each editor and reading position |
| Ctrl+O | Review this project's decisions, connection or recoverable messages |
| Ctrl+X | Stop the selected project's simulated work |
| PageUp / PageDown | Read earlier / later transcript output |
| Ctrl+E | Follow latest output |
| Ctrl+L | Explicitly clear the current draft |
| Ctrl+Q / Ctrl+C | Exit, with explicit discard when text is retained |
| F9 | Browse commands; arrows select and Tab or Enter completes a selected name |
| Tab in a slash command name | Complete a unique name or extend a common prefix; otherwise open the browser |
| F1 | Full controls; PageUp / PageDown scroll help |

Choices start without a selected action. Tab or arrows select; Enter confirms.
Escape dismisses. Typing or pasting into an open choice cannot send or answer it.
An expired choice stays visible and unavailable until you dismiss it.
Direct shortcuts retain the draft if the displayed task has changed; they never
start an idle turn. Inspection is read-only. Use Ctrl+O for explicit text recovery.

## Fixture controls

| Key | Simulated event |
| --- | --- |
| F2 | Disconnect / reconnect this project |
| F3 | Raise / expire an active task's decision |
| F4 | Reject the next message at acceptance |
| F5 | Acknowledge the pending message, otherwise complete active work |
| F6 | Confirm resetting both projects and discarding their in-memory state |
| F7 | Lose the connection after acceptance, before acknowledgement |
| F8 | Fail active work and retain queued messages for recovery |
| F10 | Revoke this project's synthetic command sources; press again to restore new revisions |

### Command interaction trial

A slash at the very start of a draft activates command scanning. Type `/qu` and
press Tab to complete `/quit`, then Enter to request exit. The alias `/exit`
uses the same action. `/help` opens the existing F1 controls view and consumes
only that command draft; it takes no arguments. A leading quote makes the draft
ordinary chat and preserves the quote and path exactly: `'/tmp/file` sends
literally. A bare `/tmp/file` or
`//tmp/file` is an unknown command and stays in the editor. In ordinary text,
arguments, and selected text, Tab retains its two-space editing behavior.
Extension commands use `/ext:source/name`. The older `/extension:` spelling is
unknown; it is not an alias and never falls through to chat.
The browser labels each entry's origin and source. Asura origin does not grant
`/ext:core/check` any extra authority; it is a synthetic work request.
The `asura`, `core` and `internal` extension source handles are reserved for
Asura-supplied commands. This fixture defines only `/ext:core/check`.
The earlier trial name `/ext:asura/check` is unknown and retains its draft.

F9 opens a browser without selecting an action. Select a command with arrows,
then Tab or Enter to complete its name; completion never invokes it. Extension
and Skill entries are synthetic, source-qualified fixtures. While work is active,
Enter on a completed command requires an explicit Steer or Queue choice. The
Skill fixture permits Queue only. Ctrl+S and Ctrl+T continue to send the draft
as literal text, even when it starts with a slash. F10 changes only the selected
project's synthetic sources; it leaves the other project, `/quit` and `/help`
available.
The command hint appears near the input only when relevant. The project and model
status bar does not carry command discovery controls.

For a focused native check, run `cargo run --locked -- --manual` in Ghostty and
Terminal.app. Try F9, arrows, Tab and Escape; complete `/qu` and `/ex`; then type
`/ext:checks/test note`, press F10, and confirm the command remains in the
editor with an unavailable explanation. Press F10 again and review the renewed
entry. The PTY suite verifies decoded behavior and terminal cleanup, but only
these native runs can establish that each terminal delivers F9 and F10 and that
the compact browser is readable there. Record those observations separately.
On 2026-09-24, the owner reported that this short F9/F10, Tab-completion and
compact-browser check passed in both Ghostty and Terminal.app. This is native
owner-observed evidence, separate from PTY automation; individual palette and
size combinations were not recorded.
The later `/ext:` naming change does not change the key bindings. Its completion,
submission and old-prefix rejection were checked by Rust and PTY tests; the new
display spelling has not been separately inspected in native terminals.

Normal mode acknowledges after 250 ms and finishes work after 12 seconds unless
a decision is pending. `--manual` advances work only through fixture controls.
Queued messages run after success; cancellation or failure holds them for an
explicit Restore/Append or Discard choice. Reconnect resolves an existing pending
request without sending it again.
While a message is pending, its notice and Ctrl+O inspector retain the submitted
text separately from transcript scrolling. The inspector stays focused when the
request resolves; Escape returns to the current draft.
The message inspector distinguishes Pending, Unknown, Acknowledged, Queued,
Running and held or settled outcomes. Acknowledged Steer means receipt. Disconnected rows show their
previously observed state as stale. Eight recent settled messages are retained;
pending, queued and held text is protected separately. The inspector indicates
expired history and retains its captured text until dismissed. Inline headings
appear only when relevant messages overflow the available rows.

## First native trial

Run the following in **each** terminal, keeping its existing font/settings:

```sh
cargo run --locked -- --manual
```

1. Type a line, use Option+Return, then type another. Paste multiline text and emoji.
   Confirm that none of those actions sends. Try selection, Ctrl+Z and Ctrl+Y.
2. Press Enter, then type a new draft while the first message waits. F5 acknowledges
   the first message; the new draft must remain unchanged.
3. Select and copy a multiline transcript excerpt using native selection and
   Command+C. Paste it into the editor and verify the copied text.
4. Resize through 120×40, 80×24 and roughly 40×12 cells. Confirm alignment,
   visible cursor, usable controls and retained text. Repeat with `--light`.
5. Press Ctrl+Q with a draft present. Enter alone must not exit. Escape resumes
   editing; Ctrl+Q, Tab, Enter explicitly discards and exits. Verify normal shell
   typing and Return after exit.

Record the terminal, palette, size and any failed step. Automated buffer and PTY
checks do not substitute for this trial. The owner reported both terminals passed
this first editor trial on 2026-09-24. The available UI tool denied agent access
to both terminal applications; the report is owner-observed evidence.

## Interaction trial

Use the guided tour in **Ghostty and Terminal.app**:

```sh
cargo run --locked -- --tour
```

The tour prepares ten checked scenes: multiline editing, Steer/Queue, an expired
decision, project switching, text recovery, lost acknowledgement, reconnection and
explicit reset, the message tray and full-text inspection. **N / P** move forward/back, **L** changes palette, **H** explains
the current scene, **R** restarts and **Q** exits. Arrow keys also navigate.
The tour pauses at every scene. Typing and paste are locked so observations remain
repeatable; advancing is not a recorded approval.
Tour controls occupy the top row so the actual status bar remains visible below
the editor. The tour needs at least 30×9 cells; ordinary editing needs 30×8.

You only need to inspect the presentation and note confusing behavior. Resize
through 120×40, 80×24 and roughly 40×12, and compare the two palettes. The tour
checks synthetic transitions; it cannot check native font rendering, physical
keyboard behavior, copying or whether the interaction feels intuitive. The first
editor trial above already passed in both terminals. Repeat affected physical
input checks when input behavior changes; the
[tour design](../../docs/designs/tui-guided-tour.md) defines that boundary.

Studio and Observatory are synthetic fixtures, not discovered project directories.
Quit the tour and run normally to explore or type your own drafts.

On 2026-09-24 the owner reported that the guided tour worked in both Ghostty and
Terminal.app, with no issues reported. This is owner-observed evidence; palette
and size combinations were not individually recorded. It is separate from the
first editor trial and automated checks.

### Composer shortcut smoke

Use this short check to reproduce the composer trial in each native terminal. Run
`cargo run --locked -- --manual`, send a message and press F5. Type a refinement,
use Ctrl+S and acknowledge with F5. Type a follow-up, use Ctrl+T and acknowledge
with F5. Confirm the tray states, then use Ctrl+B to inspect the full text.
Dismiss with Escape, type a newer draft and resize; confirm that the draft remains.
Scenes 9 and 10 of the tour provide the corresponding presentation checkpoints.
This checks the new keys without repeating the earlier detailed journeys.
The owner reported on 2026-09-24 that the shortcut and layout smoke passed in both
Ghostty and Terminal.app. Individual palette/size combinations and latency were
not recorded; this is owner-observed evidence, separate from the automated checks.

### Return-modifier inspection

The owner prefers submission controls based on Return. Before changing bindings,
inspect what each terminal sends through the current decoder:

```sh
cargo run --locked -- --keys
```

Press plain Return, Control+Return, Shift+Return, Option+Return and Command+Return,
one at a time. Record the displayed key code, modifiers and event kind for each.
Escape exits. This mode never creates a draft or sends anything; paste is ignored.
It uses the same modes as ordinary chat and does not change terminal preferences.

Command+Return is currently reserved for Ghostty fullscreen; Terminal.app documents
it as marking a line and sending Return. A key may be consumed by the terminal or
discarded by the decoder. No new event therefore does not establish which cause
applies. A decoded `Enter` without modifiers cannot support a distinct action.
The [diagnostic design](../../docs/designs/tui-key-inspection.md) records proof limits.
The owner subsequently supplied Alt+Enter observations from both terminals.
Source inspection found that Ghostty's legacy Shift/Control+Return encodings are
discarded by the current decoder. The
[native-results record](../../docs/designs/tui-key-inspection.md#native-observations-and-decoder-investigation-on-2026-09-24)
separates those findings from the unresolved Terminal.app behavior. Bindings remain
unchanged; no additional full interaction trial is needed for this investigation.
The owner also reported Control+Return opens Terminal.app's context menu, ruling
it out as a shared default under the tested settings. Shift+Return remains unverified
as a shared shortcut.

### Optional detailed journeys

These manual reproductions remain useful for investigating a problem or exploring
the interaction. They no longer need full repetition for every iteration. Start
with `cargo run --locked -- --manual`.

1. **Start and refine.** Send a message, then use F5 to acknowledge it. While work
   is active, type another draft and press Enter twice. Nothing sends until you
   deliberately select Steer or Queue with Tab/arrows. Acknowledge with F5 and
   confirm that newer typing remains. Queue a follow-up and complete work with
   F5 to see that follow-up start. Use PageUp while work changes; the reading
   position stays anchored and new output is indicated.
2. **Decide while editing.** Use F3 during work to raise a decision. Keep typing;
   the notice must not take focus. Open it with Ctrl+O. Enter without a selected
   action does nothing. Use F3 again to expire it. Enter on an unavailable choice
   must not send the draft. Escape returns to editing. Resize with the choice open.
3. **Switch projects.** Send in Studio and switch with Ctrl+P before F5. Write in
   Observatory, then return to Studio and acknowledge. Check both drafts, cursor,
   selection, undo and transcript positions. In normal mode, repeat while both
   projects work automatically and watch the background activity indicator.
4. **Recover text.** Use F4, send a message and type a newer draft before F5 rejects
   it. Ctrl+O opens recovery. Explicit Append keeps both versions; one Undo removes
   the append. Repeat after stopping queued work with Ctrl+X and failing it with F8.
5. **Resolve uncertainty.** Send a message, then use F2 before F5. Reconnect with
   F2 and recover the unaccepted text. Repeat using F7 to lose acknowledgement
   after acceptance; F2 must reconcile it once without another message or task.
6. **Reset deliberately.** Leave text in both projects, press F6, then Enter alone.
   Both drafts remain until you select Reset and confirm. Verify clean editing and
   normal shell input after Ctrl+Q exit.

Record hesitations, mistaken expectations, hidden controls or loss of context.
Native tour observations and overall usability remain separate from the first
editor pass and automated checks.

### Automated renderer previews

Generate HTML sheets from the actual Ratatui cell buffers:

```sh
cargo test --locked --test tour_previews export_tour_previews -- --ignored
```

The ignored `target/tour-previews/` directory contains ten scenes at all three
sizes in both palettes. These previews help inspect spacing and clipping, but
browser font rendering is not evidence of native terminal appearance.

After an uncatchable kill, run `reset` in the shell if modes need recovery.
The `--fault=setup`, `--fault=draw` and `--fault=panic` arguments deliberately fail
for cleanup qualification; do not use them for an ordinary trial.

# Asura user guide

This guide explains how to use the Asura terminal client. It describes the
behavior of the current developer build. A section marked **Planned** describes
designed behavior that the current build does not have yet. The build has no
Planned section at present.

For exact command syntax, see the [commands reference](commands.md). For every
key, see the [keys reference](keys.md).

## Start Asura

Run `asura` without arguments in a terminal. The client connects to your Asura
service. If no service is running, the client starts one. A service that the
client started stops when that client exits. A service that you started
separately keeps running.

On the first start, Asura creates its data folder at `~/.asura`. The top bar then
shows this notice once: "New installation created at ~/.asura. Earlier data, if
any, was not found." The notice clears when you press the next key.

If Asura cannot start or reach the service, you can still edit text and use
local commands. The top bar shows the problem. Service logs are in
`~/.asura/logs/asura.log`.

The terminal must be at least 30 columns wide and 8 rows high. A smaller
terminal shows a resize message; Ctrl+Q still exits.

## The screen

The screen has these areas, from top to bottom:

| Area | What it shows |
| --- | --- |
| Top bar | Service connection, uptime, stored data size and graph status, or a notice |
| Conversation log | Your messages, model responses and tool activity |
| Queue | Messages that wait for the model; shown only while messages wait |
| Input box | The text you are writing; the `›` marker starts its first row |
| Status bar | Project name, folder, Git state, character count, input gauge and model |
| Hint row | The keys that work in the focused area |

### Top bar

The top bar shows `process` when this client owns the service and `local` when
it attached to a service that was already running. Notices, such as command
errors, appear at its right end.

### Conversation log

The conversation log shows the current conversation. Your messages appear in
shaded bands. Model responses use Markdown formatting and show tool
activity as it happens. See [Browse responses and tool activity](#browse-responses-and-tool-activity).

### Queue panel

The queue panel appears above the input box while messages wait for the model.
It disappears when the queue is empty. See [Use the queue](#use-the-queue).

### Input box

The input box holds the text you are writing. A `›` marker starts its first row.
When the first word of a command is a known command name, that word turns amber.
See [Write and edit text](#write-and-edit-text).

### Status bar

The status bar shows the selected project, its folder and Git state, the draft's
character count, the input gauge and the model. Narrow terminals omit the folder
and Git state. You can select the project or the model here and open a list to
change it. See [Projects](#projects) and [Models](#models).

The input gauge shows how much of the model's input context the last request
used. It is muted below 50%, bright from 50% and amber from 80%.

## Move between areas

The input box has focus when Asura starts. Shift+Up moves focus up to the queue,
or to the conversation log when the queue is empty. Shift+Down moves focus down
again; from the input box it moves to the status bar. Escape returns to the input
box from the log, the queue or the status bar.

The focused area shows its keys on the hint row. A selected item has a lighter
background and brighter text. Moving focus never changes your draft.

## Send a message

Type a message in the input box and press Enter. Asura first shows the message
as a `Pending local` row in the queue. When the service accepts it, the row
becomes a queued message. The model then answers in the conversation log.

A message must be at most 32 KiB. You need a selected project; without one,
Asura keeps your draft and asks you to add a project.

Enter works the same way while the model is answering. The new message waits
in the queue until the current answer finishes. The hint row then shows `↵ queue`
instead of `↵ send`.

Text that starts with `/` is a command, not a message. See the
[commands reference](commands.md).

## Write and edit text

Press Alt+Enter (Option+Enter on macOS) to start a new line. The input box grows
to six rows, or three rows in a narrow terminal, and then scrolls.

Use these editing keys:

- Left and Right move the cursor; Shift+Left and Shift+Right select text.
- Option+Left and Option+Right, or Ctrl+Left and Ctrl+Right, move one word.
  Alt+B and Alt+F also work. Add Shift to select.
- Up and Down move one row in a multi-line message.
- Home and End go to the start or end of the row. Ctrl+Home and Ctrl+End go to
  the start or end of the whole draft.
- Option+Backspace deletes the previous word.
- Ctrl+A selects all text. Ctrl+Z undoes and Ctrl+Y redoes.
- Tab inserts two spaces, except when it completes a command.

A draft can hold up to 64 KiB. Drafts stay in this client only. Asura does not
save them when you exit.

## Recall earlier input

Press Up on the first row of the input box to recall the previous message or
command. Keep pressing Up for older entries and Down for newer ones. Down after
the newest entry restores the draft you were writing. Ctrl+P and Ctrl+N do the
same from any row.

When you change recalled text, recall ends. Up and Down then move between rows
again. Recall keeps the last 100 entries of this session. Empty entries and
repeated identical entries are not kept. Recall never sends anything.

## Paste text

Paste text in the usual way for your terminal. Pasted text goes into the input
box at the cursor.

If your terminal does not support bracketed paste, use paste capture:

1. Press Ctrl+V, then paste or type the text. Enter and Ctrl+J add a new line;
   Tab adds a tab.
2. Press Ctrl+V again to open the finish dialog.
3. Press Tab or an arrow key to choose **Insert**, then press Enter.

To leave your draft unchanged, choose **Cancel capture** or press Escape in the
finish dialog. Other keys, such as arrows or Escape, during capture reject the
captured text. **Insert** then has no effect; choose **Cancel capture**.

## Use the queue

The queue panel sits above the input box. Its title is `• Queued messages:`.
It shows up to four rows and a `+N` count for more. Each row starts with `>>`.

| Row label | Meaning |
| --- | --- |
| No label | The service accepted the message; it waits for the model |
| `Preparing model` or `Starting model` | The service is starting the model for this message; Escape or `/cancel` holds it |
| `Held (reason)` | The service holds the message until you resume or drop it |
| `Pending local` | The message is in this client only; the service has not accepted it |
| `Sending` | The client is sending the message to the service |
| `Unconfirmed` | The result of sending is not known: you cancelled the wait or the connection was lost |
| `Rejected local` | The service rejected the message; its text is kept |

A `Pending local` row is not safe yet. If the client stops, the message can be
lost. When you exit with such rows, Asura asks you to confirm.

To work with the queue, press Shift+Up or Ctrl+Up from the input box. Then:

- Up and Down select a message.
- `[` moves the selected message up and `]` moves it down. A running or
  sending message cannot move.
- Enter sends the selected message now. If the model is answering, Asura stops
  that answer and starts the selected message instead. If the model is idle, the
  message moves to the front.
- Enter on a `Rejected local` row puts its text back in an empty input box.
- Escape returns to the input box.

A held message does not move to the front. Use `/queue resume ID` or
`/queue drop ID` for it. The service keeps accepted messages when the client or
the service restarts. After a service restart, it holds waiting messages until
you decide.

## Steer or queue the active answer

While the model is answering, type text in the input box and press a key:

- Ctrl+S steers: Asura stops the current answer and restarts it with your text
  as a new instruction.
- Ctrl+T queues the text behind the current answer.

These keys need an active answer that the service has confirmed. Otherwise
Asura keeps your draft and shows why.

## Cancel, retry and observe work

`/cancel` asks the service to stop the active answer. It also cancels a pending
`/models` or `/audit` request. Asura waits for the service to confirm the
cancellation. Escape in the input box does the same as `/cancel` and keeps your
draft. If a list, panel or selection is open, the first Escape closes it and a
second Escape cancels. With nothing to cancel, Escape does nothing.

A slow or silent model does not end by itself; Asura shows what it waits for.
While the model starts, the queue row shows `Preparing model` or
`Starting model`. Then Escape or `/cancel` holds the message as `Held (cancelled)`; use
`/queue resume ID` or `/queue drop ID` for it. After the model starts, the
activity row shows `Waiting: sending input` or `Waiting: model output`, and
Escape or `/cancel` stops the answer.

`/retry` repeats a request whose result is unknown. It uses the original request
ID, so the service does not do the work twice. It also retries restoring the
conversation after a failure.

`/observe ID` shows the recorded result of an earlier operation.

## Browse responses and tool activity

The conversation log shows each response as it arrives, including tool
activity. Responses use Markdown formatting such as bold text, lists and code.

To scroll the log, press Shift+Up from the input box until the log has focus.
Use Up, Down, Page Up, Page Down, Home and End. The log follows the latest
response again when you return to the input box.

To look at one response, press F6. Up and Down choose a response. Enter or Space
expands or collapses its tool activity. Page Up and Page Down scroll it. Escape
or F6 returns to the input box. This view keeps the last eight responses of the
current session.

## Projects

A project is a folder that Asura works in. Each conversation belongs to one
project. The status bar shows the selected project's name.

### Select a project at launch

Asura selects the project that contains the folder where you started it. When
projects are nested, the deepest one wins. If other projects also contain the
folder, the top bar names them with their IDs. If no project contains the folder,
no project is selected.

If you have no projects, Asura asks: "Use this directory as a project?" Choose
Yes to add the folder and select it, or No to leave things unchanged.

### Add, list and select projects

- `/project add PATH` adds a folder and selects it. `/project add .` adds the
  current folder.
- `/project list` shows each project's 32-digit ID, name and folder.
- `/project select ID` selects a project by its 32-digit ID.

You can also select a project from the status bar. Press Down on the last row of
the input box, or Shift+Down. With the project name selected, press Enter. Choose
a project with Up and Down and press Enter. Escape closes the list.

You cannot change projects while a request is active or a message is still
pending in this client. Changing projects keeps your draft. `[stale]` in the
list, or `unavailable` in the selector, marks a project whose folder Asura
cannot use now.

A project cannot be Asura's own folder, `~/.asura`, a folder inside it, or a
folder that contains it, such as your home folder. Adding one fails with
`project_overlaps_asura_state`, and nothing is added. A project like this that
was added by an earlier build shows `[unavailable: contains Asura state]` in the
list and `unavailable: contains Asura state` in the selector. It cannot be
selected or used. Add a folder inside your home folder instead, such as a
folder for one piece of work.

### Rename a project

`/project rename NAME` renames the selected project. Asura capitalizes the
first letter. A name can be up to 128 bytes. It cannot start or end with a space
or contain control characters or line breaks. Renaming does not change the
project's ID, folder or conversations.

## Conversations

When you select a project, Asura restores that project's most recent
conversation. If restoring fails, use `/retry`, or `/new` to start again.

`/new` starts a new conversation in the selected project. It works only when no
request is active and no messages wait in the queue.

The service stores completed conversation history. Drafts and recall entries
belong to this client session only.

## Models

The model answers your messages. Asura runs one model operation at a time.

### List models

`/models` shows the models that the service can find. A `*` marks the
configured model. The status column means:

| Status | Meaning |
| --- | --- |
| `available` | The system reported the model as available |
| `installed` | Asura found the model's files on this computer |
| `listed` | Ollama listed the model |
| `unchecked` or `unavailable` | Asura could not check the model, or it cannot be used |

None of these proves that the model can answer. `/models` does not select,
download or load a model.

### Choose a model

Use the status bar: press Shift+Down from the input box, then Right or Tab to
select the model, then Enter. Choose a model and press Enter. Asura saves the
choice for the next message. A running answer keeps its model. Rows that are
unavailable explain why when you press Enter on them.

You can also type `/config set model SELECTOR`. The selectors are:

| Selector | Model |
| --- | --- |
| `system` | The macOS system model; used when no model is set |
| `ollama:MODEL` | A model served by a local Ollama |
| `mlx:NAME` | An MLX model in `~/.asura/data/models/mlx/NAME` |
| `coreai:NAME` | A Core AI model in `~/.asura/data/models/coreai/NAME` |

Asura does not download missing model files. Ollama must run on this computer;
by default Asura uses `http://127.0.0.1:11434`.

### Model tools

Models can use four tools: `project` reads and lists project files, `memory`
reads and creates notes, `service` reports status, tools and audit data, and
`shell` runs one command in the project. The shell has no network access. A
command runs until it finishes or you stop the answer with `/cancel`; it has no
time limit. `/tools` lists the tools. A listed tool is not
always permitted for the current model or project.

## Configuration

Settings are in `~/.asura/config.yaml`. Use these commands:

- `/config` shows all settings, including defaults.
- `/config get NAME` shows one setting.
- `/config set NAME VALUE` changes one setting. Dotted names select nested
  settings, such as `audit.keepFiles`.

| Setting | Values | Default |
| --- | --- | --- |
| `model` | A model selector | Not set; Asura uses `system` |
| `audit.enabled` | `true` or `false` | `true` |
| `audit.keepFiles` | 1 to 10000 | `5` |
| `audit.maxFileBytes` | 1 to 1099511627776 | `10485760` |
| `providers.ollama.endpoint` | An `http` or `https` URL on `127.0.0.1` or `::1` | `http://127.0.0.1:11434` |

Values use YAML. Quote text that contains spaces or punctuation. A model change
applies to the next message. Audit changes apply after the service restarts.
If Asura cannot confirm a change, use `/config get` before you try again.

## Audit and tools

`/audit` shows recent audit records for the selected project. Add a number from
1 to 16 to limit the list; the default is 16. `/tools` lists the model tools.
Results open in a panel above the input box. Escape closes it. Arrow keys scroll
when the content is longer than the panel.

## Get help

Type `/help`, or press F1 in the input box, to see commands and keys. Press
Escape or F1 to close the panel. Arrow keys scroll it. To search instead, use
help mode.

### Help mode

Type `?` at the start of the input box to enter help mode. The marker
changes to `?` and the box turns indigo. Type words to search commands, keys and
help topics; a panel above the box lists the matches. Up and Down choose a
result, Page Up and Page Down move by a page, and Enter opens it. Escape closes
an opened result and returns to the list.

Enter on an empty line lists the screen areas: the input box, queue, log and
status bar. Choose one to read about it and its keys. The area you used last is
first. Help mode searches the same text as this guide.

Backspace at the start of the line, or Escape, leaves help mode and keeps your
text. Typing `?` anywhere else is ordinary text. Tab and Alt+Enter do nothing
in help mode, and Ctrl+S, Ctrl+T and Ctrl+D ask you to leave help mode first.
Help mode works offline and never sends text to a model or the service. Your
help searches are not added to recall.

To send a message that starts with `?`, type a space first.

## Exit Asura

Type `/quit` or `/exit`, or press Ctrl+Q or Ctrl+C. If a message has not
reached the service yet, Asura warns you that it is not saved. Ctrl+Q and Ctrl+C
also ask you to confirm when the input box contains a draft. To exit, choose
**Discard and exit** and press Enter.

Ctrl+D exits when the input box is empty. If a message has not reached the
service yet, it shows the same warning first.

Exiting stops a service that this client started. A service that was already
running keeps running, and it keeps your accepted messages.

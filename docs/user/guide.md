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
character count, the thinking level, the input gauge and the model. Narrow
terminals omit the folder and Git state; below 40 columns they also omit the
thinking level. You can select the project, the model or the thinking level here
and open a list to change it. See [Projects](#projects), [Models](#models) and
[Thinking level](#thinking-level).

The input gauge shows how much of the model's input context the last request
used. It is muted below 50%, bright from 50% and amber from 80%.

When the terminal is at least 40 columns wide, the status bar can also show
`jobs N` and `exec off`. `jobs N` means that N command jobs of the selected
project are running or waiting. `exec off` means that the model cannot run
commands in the selected project. See [Command jobs](#command-jobs).

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

- Ctrl+S steers: Asura queues your text, then stops the current answer and
  restarts it with your text as a new instruction. If the answer ends before
  the service accepts your text, the text stays queued and Asura says so.
- Ctrl+T queues the text behind the current answer, the same as Enter.

Ctrl+S needs an active answer that the service has confirmed. Otherwise Asura
keeps your draft and shows why.

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
Escape or `/cancel` stops the answer. If the service journal is full, the
activity row shows `Paused: journal space is full.` until space is available;
see [Journal space](commands.md#journal-space).

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

When the model thought before it answered, a folded `▸ Thinking` line shows
above the answer with its length. In the response browser, press t to open or
fold it. The model never sees its earlier thinking.

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
- `/project list` shows each project's 32-digit ID, name and folder, for at
  most 64 projects. If more follow, the last line shows
  `More: /project list after ID`; type it to see the next 64.
- `/project select ID` selects a project by its 32-digit ID. The project does
  not need to be in the list that Asura shows.

You can also select a project from the status bar. Press Down on the last row of
the input box, or Shift+Down. With the project name selected, press Enter. Choose
a project with Up and Down and press Enter. Escape closes the list. The list
holds at most 64 projects. When you move to its last or first row, Asura reads
the next or the previous projects.

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

The first time a Core AI model starts, macOS compiles it and keeps the result
in `~/Library/Caches/coreai-cache/<macOS build>/com.pidster.asura.model`. This
cache can use about 2 GB or more for each macOS build. Later starts reuse it.
macOS owns the cache, and Asura does not delete it; you can delete the folder
to free space, and the next start compiles the model again.

### Thinking level

Some models can think before they answer. The thinking level sets how much:
`default` lets the model choose, and `off`, `low`, `medium` and `high` ask for a
level. Type `/think high`, or select the thinking level on the status bar and
press Enter. The level applies to the next messages and stays after a restart;
a running answer keeps its level. `/config set thinking LEVEL` does the same.

Asura sends each level in the form the model takes. A model with only on and
off gets on for low, medium and high, and the status bar shows `(on)`. A level
the model cannot take shows `(n/a)` and nothing is sent. A model that cannot
think shows `no thinking`. The status bar learns this from the model's first
answer.

### Model tools

Models can use five tools: `project` reads and lists project files, `memory`
reads and creates notes, `service` reports status, tools and audit data,
`exec_cmd` runs one command in the project as a job, and `job` reads and
manages those jobs. The exec_cmd tool has no network access. A command has no
time limit; see [Command jobs](#command-jobs). `/tools` lists the tools. A
listed tool is not always permitted for the current model or project.

## Command jobs

A job is one command that runs in the service. The model starts a job each
time it uses `exec_cmd`. The service runs the job in the background and
records its output.

### How the model uses jobs

When the model uses `exec_cmd`, the service starts the job and waits a short
time, 10 seconds unless the model asks for 0 to 30 seconds. The model then
gets the job's ID, its state and the size of its output in bytes. If the job
ended in that time, the model also gets its result, such as `exit 0`. The
model never gets output text from `exec_cmd`.

To read the output, the model uses its `job` tool. The tool has these
commands: `list`, `status`, `read`, `wait`, `cancel` and `note`. The model can
see and use only the jobs of its own conversation. A note from the model is
marked as the model's note.

### Jobs after a turn

A job keeps running after its turn ends when the model has received its ID.
`/cancel`, steering, `/new` and closing the client do not stop such a job. If
the turn ends before the model receives the ID, the service stops the job.

When jobs keep running after a turn, the log shows a line under that response,
for example `↳ 2 jobs kept running after this turn · /jobs to view or cancel`.
While jobs of the selected project run or wait, the status bar shows `jobs N`.

A service that the client started stops its jobs when the service stops. See
[Exit Asura](#exit-asura). A service that you started separately keeps its
jobs running when the client exits.

### List jobs

`/jobs` lists the jobs of all conversations in the selected project, newest
first. `/jobs all` lists the jobs of every project; each row names its project.
The list shows 8 jobs at a time. Down past the last row shows the next jobs.

Each row shows the first 8 digits of the job ID, the command, the state or
result, the age, and the output size as `out` and `err`. `pinned` marks a
pinned job, and `✎2` shows that the job has 2 notes. In a terminal narrower
than 60 columns, the row omits the output size.

| State | Meaning |
| --- | --- |
| `queued` | The job waits for a free slot; the reason follows in brackets |
| `starting` or `running` | The command is starting or running |
| `paused` | The service stopped saving output, for example because the disk is full |
| `stopping`, `draining` or `finalizing` | The job is ending |
| `exit N` or `signal N` | The command ended with exit status N, or a signal stopped it |
| `cancelled` | Someone or something stopped the job; the reason follows |
| `not started` or `interrupted` | The job never started, or a service restart ended it |

`cancel requested` follows the state of a job that is stopping on request. See
[Job limits](commands.md#job-limits) for the number of jobs that run at the
same time.

### View job output

`/job ID` opens the job viewer, and so does Enter in the job list. The viewer
shows one stream at a time: standard output (`stdout`) or standard error
(`stderr`). It opens at the start of `stdout`. Tab switches the stream.

The top line shows the job ID, its state, the stream, the bytes shown and the
total bytes. It also shows `pinned`, and the project when the job belongs to
another project. Press f to follow new output as it arrives; the top line
then shows `following`. Following stops when the job ends.

Press i to open the detail panel. It shows the state, the output size in bytes
and lines, the folder, the whole command and the notes. Each note shows
whether you or the model wrote it.

To open a job from a response, press F6, select the response and press j.
Escape closes the viewer. Closing the viewer never stops the job. For every
key, see [Job viewer keys](keys.md#job-viewer-keys).

### Cancel, pin, delete and note jobs

Use these commands, or the keys in the job list:

- `/job cancel ID`, or c, asks the service to stop the job. Asura then shows
  `Stopping job ID; /jobs shows the settlement`.
- `/job pin ID`, or p, keeps the job's output from expiring. `/job unpin ID`
  lets it expire again.
- `/job delete ID`, or d, deletes the job's output after you answer y. The
  summary and the notes stay. A running job's output cannot be deleted.
- `/job note ID TEXT`, or n, adds your note to the job.

For a job of another project, cancel asks you to confirm first. The viewer
shows `Output deleted` or `Output expired` when the output is gone; i still
shows the summary and notes. See [Job commands](commands.md#job-commands).

### Execution setting

The execution setting decides whether the model may run commands. It is on by
default. `/execution off` turns it off for every project without its own
value. `/execution project off` turns it off for the selected project only.

When execution turns off for a project, the service refuses new model commands
there and stops the model's jobs in that project. The model is told not to try
again. The status bar shows `exec off`. Other model tools still work, and the
model can still read its jobs. See [Execution setting](commands.md#execution-setting).

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
| `journal.quotaBytes` | 67108864 (64 MiB) to 1099511627776 (1 TiB) | `4294967296` (4 GiB) |
| `jobs.liveService` | 1 to 16 | `4` |
| `jobs.liveAgent` | 1 to 8 | `2` |
| `jobs.queuedAgent` | 1 to 16 | `8` |
| `jobs.clientViewers` | 1 to 32 | `8` |
| `jobs.modelReservedPageReads` | 0 or 1 | `1` |
| `execution.enabled` | `true` or `false` | `true` |
| `execution.projects.ID` | `on`, `off` or `inherit` for the project with this ID | `inherit` |

Values use YAML. Quote text that contains spaces or punctuation. A model change
applies to the next message. Audit changes apply after the service restarts.
A `journal.quotaBytes` change applies at once. It limits the service journal,
which holds conversations and projects. It is separate from
`retention.quotaBytes`. See [Journal space](commands.md#journal-space).
The `jobs` settings limit command jobs. See [Job limits](commands.md#job-limits).
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

When this client started the service and jobs are running, every way to exit
asks you to confirm, even with an empty input box. The dialog then says, for
example, `2 jobs are still running in all projects; they stop when the service
stops.` Choose **Discard and exit** to exit and stop those jobs. Exiting from a
service that was already running never stops jobs.

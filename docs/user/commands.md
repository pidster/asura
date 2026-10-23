# Commands reference

Type a command in the input box and press Enter. A command starts with `/`.
Commands run in the client or ask the service directly; they never go to the
model or the queue. Tab completes a command name; see the
[keys reference](keys.md#input-box-keys).

In the syntax, `ID` is a 32-digit hexadecimal ID. `/project list` shows project
IDs and `/queue` shows input IDs. Words in capitals are values that you supply.

If a command fails, Asura keeps your text in the input box and shows the reason
on the top bar. An unknown command shows "Unknown command; use /help."

## General commands

| Command | Action |
| --- | --- |
| `/help` | Show commands and keys; F1 does the same; `?` searches them |
| `/quit` | Close this client; a service it started stops too |
| `/exit` | Same as `/quit` |
| `/init` | Check the installation setup; normal first use does not need it |

## Conversation commands

| Command | Action |
| --- | --- |
| `/new` | Start a new conversation in the selected project when nothing is active or queued |
| `/cancel` | Stop the active answer, hold a message while its model starts, or cancel a pending `/models` or `/audit` request. Escape in the input box does the same and keeps your draft |
| `/retry` | Repeat a request whose result is unknown, with its original request ID |
| `/observe ID` | Show the recorded result of an operation |

## Queue commands

| Command | Action |
| --- | --- |
| `/queue` | List the messages in the queue |
| `/queue ID` | Show one queued message |
| `/queue resume ID` | Let a held message continue |
| `/queue drop ID` | Remove a held message |

## Project commands

| Command | Action |
| --- | --- |
| `/project add PATH` | Add a folder as a project and select it |
| `/project list` | List the first 64 projects with their IDs, names and folders |
| `/project list after ID` | List the next 64 projects after the project with this ID |
| `/project select ID` | Select a project, also one that the list does not show |
| `/project rename NAME` | Rename the selected project |

There is no limit on the number of projects. Asura reads the list in pages of
8 projects and shows at most 64 at a time. When more projects follow, the last
line is `More: /project list after ID`. Type that command to see the next 64.
In the project list on the status bar, Asura reads the next or the previous
page when you move past the last or the first row.

`/project select ID` first asks the service for that project when the list
does not show it. If the project is not found, Asura keeps your text and
shows "Project not found". Escape stops the wait.

In a terminal, `asura project list` prints every project, one page at a time,
as each page arrives. Press Ctrl-C to stop it.

## Model and configuration commands

| Command | Action |
| --- | --- |
| `/models` | List models; `*` marks the configured model |
| `/think LEVEL` | Set the model thinking level for the next messages: `off`, `low`, `medium`, `high` or `default` |
| `/config` | Show all settings, including defaults |
| `/config get NAME` | Show one setting |
| `/config set NAME VALUE` | Change one setting; dotted names select nested settings |

## Inspection commands

| Command | Action |
| --- | --- |
| `/tools` | List the model tools and their descriptions |
| `/audit [LIMIT]` | Show recent audit records for this project; LIMIT is 1 to 16, default 16 |
| `/status` | Show the service status and its Keychain notices |

`/help`, `/quit`, `/exit`, `/init`, `/new`, `/cancel`, `/retry`, `/models`,
`/tools` and `/status` take no arguments. Results open in a panel above the
input box; Escape closes it.

## Job commands

| Command | Action |
| --- | --- |
| `/jobs` | List the command jobs of the selected project, newest first; `/jobs all` lists the jobs of every project |
| `/job ID` | Open the job viewer; `/job cancel`, `pin`, `unpin`, `delete` and `note` act on the job |

The `/job` command has these forms:

| Form | Action |
| --- | --- |
| `/job ID` | Open the job viewer at the start of the job's standard output |
| `/job cancel ID` | Ask the service to stop the job |
| `/job pin ID` | Keep the job's output from expiring |
| `/job unpin ID` | Let the job's output expire again |
| `/job delete ID` | Delete the job's output after you confirm; its summary and notes stay |
| `/job note ID TEXT` | Add your note to the job; TEXT is the rest of the line |

For a job command, `ID` is the full 32-digit job ID or the start of it. A
shorter ID must have at least 6 hexadecimal digits and match exactly one job
that Asura listed in this session. Use `/jobs` first if Asura says
"Use /jobs to find the job, or give the full handle". If several jobs match,
Asura lists them and keeps your text.

`/jobs` needs a selected project; `/jobs all` does not. `/job cancel ID` for a
job of another project asks `Cancel job ID in project NAME? (y/n)` first.
`/job delete ID` always asks first. The service refuses to delete the output of
a job that is still running. A note is 1 to 2048 bytes without control
characters; a job holds at most 16 notes.

If you stop the wait for `/job cancel` with Escape or `/cancel`, the result is
unknown. Repeating `/job cancel ID` is safe. Check `/job ID` before you repeat
a note, because the first note may have been added. When the service does not
support jobs, these commands show "This service does not support jobs". See
[Command jobs](guide.md#command-jobs).

## Execution setting

| Command | Action |
| --- | --- |
| `/execution` | Show whether the model may run commands; `/execution on` or `off` sets the default, `/execution project on`, `off` or `inherit` sets the selected project |

The execution setting controls the model's `exec_cmd` tool. It does not affect
the other model tools.

| Form | Action |
| --- | --- |
| `/execution` | Show the default and the selected project's value with its source |
| `/execution on` | Let the model run commands in projects without their own value |
| `/execution off` | Stop the model from running commands in projects without their own value |
| `/execution project on` | Let the model run commands in the selected project |
| `/execution project off` | Stop the model from running commands in the selected project |
| `/execution project inherit` | Remove the selected project's own value; it uses the default again |

`/execution` shows, for example,
`execution: on (default) · this project: off (project override)`. The default
is on. A project value wins over the default.

When a change turns execution off for a project, the service refuses new model
commands there. It also stops the model's queued and running jobs in that
project. The reply then ends with `/jobs`, where you can see the jobs stop.
Turning execution on again does not restart stopped jobs. While execution is
off for the selected project, the status bar shows `exec off`.

The setting changes `execution.enabled` and `execution.projects.ID` in
`~/.asura/config.yaml`. If Asura cannot confirm the change, it shows
"Execution setting outcome unconfirmed; read it again with /execution".

## Job limits

The `jobs` settings limit how many command jobs the service runs and queues.
The service reads them when it starts, so a change applies after the service
restarts. `/config get jobs` shows all five; `/config set jobs.liveAgent 3`
changes one.

| Setting | Meaning |
| --- | --- |
| `jobs.liveService` | Jobs that run at the same time in the service, 1 to 16 |
| `jobs.liveAgent` | Jobs that run at the same time for one conversation, 1 to 8. The service uses the smaller of this and `jobs.liveService` |
| `jobs.queuedAgent` | Jobs that wait for one conversation, 1 to 16. The service refuses a job above this limit |
| `jobs.clientViewers` | Job output reads that clients may have waiting, 1 to 32 |
| `jobs.modelReservedPageReads` | Output readers kept for the model, 0 or 1 |

A job that cannot start yet waits and shows why, for example
`waiting: 2 of 2 jobs running for this agent`. It starts when a running job
ends. A value outside its range gives `config_invalid_value`.

## Journal space

The service records conversations and projects in its journal. The journal has
a size limit, the `journal.quotaBytes` setting, and it keeps some free disk
space. See [Configuration](guide.md#configuration). When the journal is full,
the service refuses new messages, new projects and renames, and Asura keeps
your text:

| Message | Meaning |
| --- | --- |
| Journal space is full. Raise journal.quotaBytes or free disk space. | The journal reached `journal.quotaBytes`. Use `/config set journal.quotaBytes VALUE` with a larger value, or remove files. |
| Disk space is low for the journal. | The disk has too little free space. Remove files from the disk. |

An answer that is running when the journal fills pauses. Its activity row
shows `Paused: journal space is full.` The answer continues when space is
available again. Escape or `/cancel` stops it.

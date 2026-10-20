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

`/help`, `/quit`, `/exit`, `/init`, `/new`, `/cancel`, `/retry`, `/models` and
`/tools` take no arguments. Results open in a panel above the input box; Escape
closes it.

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

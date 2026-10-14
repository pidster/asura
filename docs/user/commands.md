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
| `/cancel` | Stop the active answer, or a pending `/models` or `/audit` request |
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
| `/project list` | List projects with their IDs, names and folders |
| `/project select ID` | Select a project |
| `/project rename NAME` | Rename the selected project |

## Model and configuration commands

| Command | Action |
| --- | --- |
| `/models` | List models; `*` marks the configured model |
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

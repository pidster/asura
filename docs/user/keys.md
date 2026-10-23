# Keys reference

Keys depend on the focused area. The hint row at the bottom of the screen shows
the main keys for the focused area. It uses `↵` for Enter, `⇧` for Shift and,
on macOS, `⌥` for Option. On macOS, Option keys arrive as Alt keys, so this
reference writes Option or Alt for the same key.

## Keys anywhere

These keys work in the input box, queue, log and status bar. An open panel or
dialog takes all keys. An open job list or job viewer takes all keys except
Ctrl and Alt keys. In the project and model lists, only the Ctrl+Q, Ctrl+C,
Ctrl+D, Ctrl+S, Ctrl+T and Ctrl+V keys in this table work.

| Key | Action |
| --- | --- |
| Ctrl+Q or Ctrl+C | Exit; asks first when a draft or an unsent message exists |
| Ctrl+D | Exit when the input box is empty; asks first when an unsent message exists |
| Ctrl+S | Queue the text in the input box, then steer the active answer with it |
| Ctrl+T | Queue the text in the input box, the same as Enter |
| Ctrl+V | Start paste capture |
| Ctrl+P or Ctrl+N | Recall the previous or next input in the input box |
| F6 | Browse responses, or return from the response browser |

## Input box keys

| Key | Action |
| --- | --- |
| Enter | Send the message or run the command; during an answer, the message waits in the queue |
| Option+Enter or Alt+Enter | Start a new line |
| Up | Move up one row; on the first row, recall the previous input |
| Down | Move down one row; on the last row, focus the status bar |
| Up or Down while recalling | Step to an older or newer input; Down after the newest restores your draft |
| Shift+Up | Focus the queue, or the log when the queue is empty |
| Shift+Down | Focus the status bar |
| Ctrl+Up | Focus the queue at its last message |
| Ctrl+Down | Focus the status bar |
| Left or Right | Move the cursor |
| Shift+Left or Shift+Right | Select text |
| Option+Left or Option+Right | Move one word; add Shift to select |
| Ctrl+Left or Ctrl+Right | Move one word; add Shift to select |
| Alt+B or Alt+F | Move one word back or forward |
| Home or End | Go to the start or end of the row |
| Ctrl+Home or Ctrl+End | Go to the start or end of the draft |
| Backspace or Delete | Delete the character before or after the cursor |
| Option+Backspace or Alt+Backspace | Delete the previous word |
| Ctrl+A | Select all text |
| Ctrl+Z or Ctrl+Y | Undo or redo |
| Tab | Complete a command name, or insert two spaces |
| F1 | Show commands and keys |
| ? | At the start of the draft, enter help mode; elsewhere it is text |

Tab completes a command when the draft starts with `/` and the cursor is in the
first word. One match completes at once. Several matches open the command list.

## Command mode keys

Type `!` at the start of the input box to run a shell command, `!COMMAND`, in
the sandbox. Type `!` again at the start to run it outside the sandbox,
`!!COMMAND`. Asura queues the command like a message and shows its output in
the conversation log. Command mode works only with a service that supports
commands; otherwise Enter shows "Commands are not enabled in this service"
and keeps the draft.

| Key | Action |
| --- | --- |
| ! | At the start of the draft, switch to command mode; in command mode, switch to outside mode |
| Backspace | At the start of the draft, step back one mode and keep the text |
| Enter | Queue the command; it runs when its turn comes |
| Ctrl+J | Start a new line in the command |
| Ctrl+C | Stop the running command; press again to kill it |
| Enter or Space | In the conversation log, open the job viewer at the end of the command's output |

## Help mode keys

Type `?` at the start of the input box to enter help mode. Editing keys work
as in the input box. These keys differ:

| Key | Action |
| --- | --- |
| Up or Down | Select the previous or next result |
| Page Up or Page Down | Move the selection by a page |
| Enter | Open the selected result; on an empty line, open the area help |
| Backspace at the start | Leave help mode and keep the text |
| Escape | Close an opened result; otherwise leave help mode and keep the text |
| Ctrl+S, Ctrl+T or Ctrl+D | Ignored with a notice to leave help mode first |
| F1 | Show commands and keys; help mode resumes when it closes |

In help mode, Tab, Option+Enter, Shift+Up, Shift+Down, Ctrl+Up, Ctrl+Down, F6,
Ctrl+P and Ctrl+N do nothing. Help mode keeps focus in the input box and never
recalls earlier input.

## Queue keys

| Key | Action |
| --- | --- |
| Up or Down | Select a message |
| Home or End | Select the first or last message |
| Up on the first message | Focus the log |
| Down on the last message | Return to the input box |
| `[` or `]` | Move the selected message up or down |
| Enter | Send the selected message now; an active answer stops and restarts |
| Enter on a `Rejected local` message | Put its text back in an empty input box |
| Shift+Up | Focus the log |
| Shift+Down or Escape | Return to the input box |

## Conversation log keys

| Key | Action |
| --- | --- |
| Up or Down | Scroll one row |
| Page Up or Page Down | Scroll one page |
| Home or End | Go to the start or end of the conversation |
| Shift+Down | Focus the queue, or the input box when the queue is empty |
| Escape | Return to the input box |

Ctrl+P, Ctrl+N, Ctrl+Up, Ctrl+Down, Shift+Left and Shift+Right leave the log
and act as they do in the input box.

## Response browser keys

Press F6 to open the response browser.

| Key | Action |
| --- | --- |
| Up or Down | Choose an earlier or later response |
| Home or End | Choose the first or last response |
| Enter or Space | Expand or collapse the response's tool activity |
| t | Open or fold the response's thinking |
| j | Open the job viewer for the response's command job, or choose one of its jobs |
| Page Up or Page Down | Scroll the response |
| Escape or F6 | Return to the input box |

When a response started several jobs, j opens a list of up to 9 of them. Up
and Down select a job, Enter opens it and Escape returns to the response.

## Status bar keys

| Key | Action |
| --- | --- |
| Left, Right or Tab | Select the project, the model or the thinking level |
| Enter | Open the project list, the model list or the thinking list |
| Up, Shift+Up or Escape | Return to the input box |

## Project and model list keys

| Key | Action |
| --- | --- |
| Up or Down | Select a row |
| Home or End | Select the first or last row |
| Enter | Use the selected project, model or thinking level |
| Escape | Close the list without a change; stops a model search |

The project list shows at most 64 projects. On its last or first row, Up,
Down, Home and End read the next or the previous projects.

## Panel and dialog keys

Result panels show `/help`, `/config`, `/models`, `/tools` and `/audit` output.

| Key | Action |
| --- | --- |
| Escape | Close the panel or dialog |
| F1 | Close the help panel |
| Up, Down, Left or Right | Scroll a result panel that is longer than its space |
| Tab, Left, Right, Up or Down | Change the choice in a dialog |
| Enter | Confirm the choice in a dialog |
| y | Answer yes to a job question, such as `Cancel job ID in project NAME? (y/n)` |
| n | Answer no to a job question; Escape does the same and nothing is sent |

## Job list keys

Type `/jobs` or `/jobs all` to open the job list.

| Key | Action |
| --- | --- |
| Up or Down | Select a job; past the first or last row, show the previous or next page |
| Page Up or Page Down | Select the first or last row; on that row, show the previous or next page |
| Enter | Open the job viewer for the selected job |
| c | Cancel the selected job; a job of another project asks first |
| p | Pin the selected job, or unpin a pinned job |
| d | Delete the selected job's output; Asura asks first |
| n | Write a note for the selected job; Enter adds it and Escape discards it |
| r | Read the list again after an error |
| Escape | Close the list |

## Job viewer keys

Type `/job ID`, press Enter in the job list or press j in the response browser
to open the job viewer.

| Key | Action |
| --- | --- |
| Tab | Switch between standard output and standard error; each keeps its position |
| Up or Down | Scroll one line |
| Page Up or Page Down | Scroll one page |
| Home or End | Go to the first byte or to the current end of the output |
| f | Start or stop following new output |
| i | Open the job detail panel |
| r | Read the output again after an error |
| Escape | Close the viewer; the job keeps running |

While the viewer follows, Down, Page Down and End do nothing. Up, Page Up, Tab
and Home stop following. While a page loads, only Escape works.

## Job detail keys

Press i in the job viewer to open the job detail panel.

| Key | Action |
| --- | --- |
| Up or Down | Scroll the detail panel |
| Escape | Return to the job viewer |

## Command list keys

The command list opens when Tab matches several commands.

| Key | Action |
| --- | --- |
| Down or Right | Select the next command |
| Up or Left | Select the previous command |
| Tab | Select the first command, or complete the selected command |
| Enter | Complete the selected command; press Enter again to run it |
| Escape | Close the list |

## Paste capture keys

Press Ctrl+V to start paste capture.

| Key | Action |
| --- | --- |
| Enter or Ctrl+J | Add a new line to the captured text |
| Tab | Add a tab to the captured text |
| Ctrl+V | Open the finish dialog |

Other keys reject the captured text. In the finish dialog, choose **Insert** or
**Cancel capture** with Tab or the arrows and press Enter.

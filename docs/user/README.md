# User documentation

This directory holds the user guide for the Asura terminal client. It is
written for the person using Asura, not for implementers. Each file describes
the behavior of the current developer build. A section marked **Planned**
describes designed behavior that the build does not have yet.

These files are also the single source for in-client help. The
[help mode design](../designs/help-mode.md) compiles them into the
`asura` binary for `/help`, `?` search and area help. Keep headings stable and
keep the command and key tables in the format that design defines.

## Files

| Document | Contents |
| --- | --- |
| [User guide](guide.md) | Task-oriented guide: start Asura, send messages, use the queue, projects, conversations, models, command jobs, the execution setting and configuration. |
| [Commands reference](commands.md) | Every slash command with its exact syntax, grouped by purpose, including the job and execution commands, and the journal space messages. |
| [Keys reference](keys.md) | Every key, organised by area: input box, queue, log, response browser, status bar, lists, panels, the job list, job viewer and job detail, and paste capture. |

Return to the [documentation index](../README.md).

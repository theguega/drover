---
name: drover
description: "Drive drover, the herdr desk runtime, from the terminal: start agents in worktrees, read agents and their terminals, send keys, write journal entries, search the journal, check usage and setup. Use when the user mentions drover, a Discord task thread, /new, /done, the journal, or asks to start, read, or close an agent through drover."
---

# drover

drover runs the same herdr desk as Discord. Every Discord command has a terminal form, and both act on the same tasks.

The full reference, with every command, its Discord equivalent, and the setup, is printed by:

```bash
drover help
```

Read it before the first command in a session. Use `drover config` for this service's live setup: host, remotes, channel, paths.

## Rules

- A task is its Discord thread. Find thread ids with `drover agents`, then pass them as `--thread`.
- `drover new` starts a real agent in a worktree and opens a Discord thread for it. Pass `--scout` when the user wants a report with no changes.
- `done` asks the agent for a journal entry, then closes the workspace. Use `--no-journal` only when the user asks for it.
- `done --remove` deletes a checkout only when drover opened it, and drover-wt keeps a dirty checkout or an unmerged branch. Never remove by hand what drover did not open.
- `keys` sends real keystrokes to a running agent. Read `screen` first.
- drover must be running. If a command says it is not running here, say so and stop.

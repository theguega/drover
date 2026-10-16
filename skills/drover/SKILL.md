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

- Terminal commands stay in the terminal by default and do not open a Discord thread. Only pass `--discord` when the user asks for Discord, or run `drover attach --pane P --discord` when the user wants to continue a running task on Discord.
- A task is addressed by its pane id (`--pane P`, printed by `drover new` and `drover agents`), or by its Discord thread (`--thread T`) when bound to one.
- `drover new` starts a real agent in a worktree in herdr. Pass `--scout` when the user wants a report with no changes.
- `done` asks the agent for a journal entry, then closes the workspace. Use `--no-journal` only when the user asks for it.
- `done --remove` deletes a checkout only when drover opened it, and drover-wt keeps a dirty checkout or an unmerged branch. Never remove by hand what drover did not open.
- `keys` sends real keystrokes to a running agent. Read `screen` first.
- `send` sends a prompt to a task's agent. It is queued when the agent is busy.
- A mate is an agent in the `drover` workspace with its own folder. `drover mate NAME` starts one, and `drover new --mate NAME` dispatches a crewmate to it. Mates have at most three at a time.
- A scout is a short look with no repo. `drover scout PROMPT` starts one, and `drover scout close --pane P` ends it and deletes its folder. Do not use `done` on a scout.
- `take` gives a pane back to the desk user. `release` lets its mate drive it again. A mate checks its owner list before every send.
- `link` attaches a running agent to a mate. `brief` adds a note to a mate's log. `escalate` logs a question for the desk (and posts it in the mate's thread when on Discord).
- drover must be running. If a command says it is not running here, say so and stop.

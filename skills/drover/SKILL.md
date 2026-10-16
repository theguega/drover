---
name: drover
description: "Drive drover, the herdr desk runtime, from the terminal: start agents in worktrees, read agents and their terminals, send keys, write journal entries, search the journal, check usage and setup. Use when the user mentions drover, a Discord task thread, /new, /done, the journal, or asks to start, read, or close an agent through drover."
---

# drover

drover is the agent runtime on top of Herdr. It opens git worktrees (`$REPOS_ROOT/<repo>.<branch>`), starts real coding agents (`claude`, `cursor`) inside Herdr workspaces across the host and SSH remotes, tracks their lifecycle events, maintains a searchable SQLite FTS5 journal, and bridges tasks to Discord threads on demand.

Before issuing any mutating command, verify that the drover service is running and discover the live machines and active panes in one call:

```bash
drover config && drover agents
```

If `drover` reports that the service is not running, say so and stop.

## Understand tasks, scouts, and mates

Choose the primitive that matches the job:

- **Task (`drover new`)**: a repo-backed worktree and Herdr workspace running a coding agent (`claude` by default, or `cursor`). Pass `--scout` when the task should investigate a repo and report back without editing files, committing, or pushing.
- **Attached task (`drover attach`)**: an agent that was already started at the desk in Herdr and is now tracked by drover.
- **Repo-less scout (`drover scout`)**: a short investigation with no repository, opened in its own scratch folder (`mates/state/scouts/scout-XXXXXX`) and pane on the host.
- **Mate (`drover mate`)**: a dispatcher agent in the `mates` workspace (up to 3 at a time) that spawns and reviews crewmate tasks (`drover new --mate NAME`) without writing code itself.

`drover agents` and `drover worktrees` prefix each entry with its live Herdr status icon:

- `●` `working` — agent is actively running a turn.
- `◆` `blocked` — agent is waiting at an approval or question dialog.
- `✓` `done` — agent finished a turn and is ready for input.
- `○` `idle` — agent is ready for input (or in `drover worktrees`, the worktree is open in Herdr with no active state).
- `·` `closed` — in `drover worktrees`, a checkout on disk with no open Herdr workspace.

## Use IDs, machines, and Discord binding

Discover the host name, configured SSH remotes, `REPOS_ROOT`, and active panes from `drover config` and `drover agents`:

```bash
drover config
drover agents
drover worktrees
```

- Every task is addressed by its Herdr pane ID (`--pane w1:p1`, printed by `drover new`, `drover scout`, `drover mate`, and `drover agents`) or, when bound to Discord, by its thread ID (`--thread T`).
- Terminal commands stay in the terminal by default and print only the pane ID (`w1:p1`). They do **not** open a Discord thread unless `--discord` is passed.
- Only pass `--discord` when the user asks for a Discord thread, or attach an already-running terminal task to Discord later:

```bash
drover attach --pane w1:p1 --discord
```

- `--machine <name>` must match the `host` or one of the `remotes` returned by `drover config`.

## Start, drive, and close a task

Open a worktree and start an agent on a machine:

```bash
drover new \
  --machine <machine> \
  --repo <repo> \
  --branch <new-name|existing-branch|#pr> \
  --prompt "<instructions>" \
  [--agent claude|cursor] \
  [--model <model>] \
  [--scout] \
  [--mate <mate-name>] \
  [--discord]
```

`drover new` creates or opens the checkout via `drover-wt`, appends relevant context recalled from the repo's journal (up to 2 KB), starts the agent in Herdr, and delivers the prompt automatically once the agent becomes ready. Read the returned pane ID (or `<#thread>`) from stdout.

Follow an agent that was started directly in Herdr:

```bash
drover attach [--machine <machine>] --pane <pane-id> [--discord]
```

Send a follow-up prompt to a task:

```bash
drover send --pane <pane-id> --text "<prompt>"
```

If the agent is currently `working`, `drover send` queues the prompt (at most one queued prompt per task) and submits it automatically on the next idle transition.

Inspect the terminal viewport or answer a `◆` blocked dialog:

```bash
drover screen --pane <pane-id>
drover keys --pane <pane-id> --keys "esc shift+tab enter"
```

Always read `drover screen` before sending `drover keys`. `keys` forwards space-separated logical key names to Herdr, waits 700 ms, and prints the updated screen.

Close a finished task:

```bash
drover done --pane <pane-id> [--remove] [--no-journal]
```

By default, `done` prompts the agent for a five-line journal entry (`what`, `why`, `outcome`, `decisions`, `next`), records it in `journal.db`, and closes the Herdr workspace. Pass `--no-journal` only when the user asks to skip journaling. Pass `--remove` only when the user wants the worktree deleted; `drover` only removes checkouts it opened (`Origin::Opened`), and `drover-wt` refuses to delete dirty checkouts or unmerged branches.

## Coordinate mates and scouts

Start a repo-less scout and close it when finished:

```bash
drover scout "<prompt>" [--discord]
drover scout close --pane <pane-id>
```

Always close a repo-less scout with `drover scout close`, which tears down the pane and removes its scratch directory. Do not run `drover done` on a repo-less scout ( note: a repo task started with `drover new --scout` is a regular worktree task and closes with `drover done`).

Start or coordinate a mate (`[a-z][a-z0-9_-]{0,25}`, max 3 running):

```bash
drover mate <name> [--discord]
drover link --machine <machine> --pane <pane-id> --mate <name>
drover brief --mate <name> --text "<note>"
drover escalate --mate <name> --text "<question for the desk>"
drover take --pane <pane-id>
drover release --pane <pane-id>
```

- `drover new ... --mate <name>` or `drover link ... --mate <name>` assigns a task pane to a mate.
- `take` hands control of a pane back to the desk user (`Owner::User`); `release` returns control to the mate (`Owner::Mate`). A mate checks `mates/state/<name>/owners` before every `send` and never writes to a pane owned by `user`.

## Journal, recall, and usage

Search or inspect the persistent task journal and per-machine quota usage:

```bash
drover recall <words...>
drover journal [--pane <pane-id> | --thread <thread-id>]
drover journal rm <id>
drover usage
```

- `drover journal` lists the 5 newest entries globally, or for a specific task's repo when `--pane` / `--thread` is given.
- `drover recall` runs an FTS5 search across past journal entries.
- `drover usage` queries Claude and Cursor usage across the host and all remotes.

## Safety and coordination rules

- Check `drover config && drover agents` before starting work so you use valid machine names and do not duplicate an existing branch or pane.
- Keep terminal commands in the terminal; never pass `--discord` unless the user explicitly asked for Discord.
- Read `drover screen --pane <pane-id>` before sending `drover keys` or retrying a prompt.
- Do not close tasks (`drover done`, `drover scout close`) or delete journal entries (`drover journal rm`) that you did not create unless the user explicitly asks.
- Never delete worktrees by hand when `drover done --remove` is appropriate.
- Run `drover <command> --help` or `drover help` if you need flag syntax or setup reference.

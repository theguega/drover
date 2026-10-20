---
name: drover
description: "Drive drover, the herdr desk runtime, from the terminal: start coding agents in git worktrees on the host or SSH remotes, read and answer them, close them with a journal entry, search the journal, run mates and scouts, check usage. Use when the user mentions drover, a Discord task thread, /new, /done, the journal, a mate or a scout, or asks to start, read, send to, or close an agent through drover. For raw herdr panes, tabs and layout, use the herdr skill instead."
---

# drover

drover is an agent runtime with no model. It opens a git worktree (`$REPOS_ROOT/<repo>.<branch>`), starts the real agent TUI (`claude` or `cursor`) in a herdr workspace on the host or an SSH remote, follows its lifecycle through herdr events, keeps a searchable journal, and bridges tasks to Discord threads when asked. Every task is an ordinary herdr workspace, so the user can take it over at the desk at any point.

Before any other command, read the home view:

```bash
drover
```

It prints the desk counts, what needs the user (blocked agents, escalations), and the next commands. Bare `drover` only reads; it is safe to run. If it prints `error: drover is not running here`, say so and stop. Do not start the service yourself.

## Learn the current CLI

The installed binary is the authority for command syntax:

```bash
drover --help
drover <command> --help
```

Each `--help` prints the usage, every flag with its default, and examples. Do not guess flags. Do not probe a command by leaving out its arguments: `drover scout <prompt>` and `drover mate <name>` start agents as soon as they parse.

## Read the output

Output is TOON: `key: value` lines, and lists as `name[count]{fields}:` with one row per record. Errors use the same format.

- A change prints its ids, then `help:` with the next commands.
- `noop: <why>` with exit 0: the state already held (`already taken`, `already gone`). Nothing changed. Do not retry.
- `error: <what>` with exit 1: a real failure. Run the `help:` line.
- Exit 2: a usage error, such as an unknown flag. `help:` names the command to read.
- Long text (screens, journal entries) is cut at 1000 characters, with `chars` for the whole length. Add `--full` for all of it.
- `count: 5 of 42` means a list is capped.

Each record puts first the id that the next command takes:

- `pane` (like `wB4:pE`) → `--pane`
- `machine` → `--machine`, from `drover config`
- `mate` → `--mate`, `drover mate <name>`
- `id` → `drover journal rm <id>`
- `thread` → `--thread`, a Discord thread id, accepted wherever `--pane` is

Copy the value as printed. Do not rebuild it from a name or from an example.

## Understand tasks, scouts, and mates

Choose the primitive that matches the job:

- **Task** (`drover new`): a worktree and a herdr workspace running one agent. drover opened the checkout, so `done --remove` may delete it.
- **Repo scout** (`drover new --scout`): a task asked for a report and no changes. It is a task like any other and closes with `done`.
- **Second look** (`drover new --at <pane> --prompt ...`): a detached checkout at another task's commit, on the same machine and repo, for a reviewer that edits nothing. `done` removes it.
- **Attached task** (`drover attach`): an agent the user started at the desk, now followed by drover. drover did not open its worktree and never removes it.
- **Scout** (`drover scout <prompt>`): a short look with no repo, in its own pane and scratch folder on the host. It closes with `drover scout close`, never `done`.
- **Mate** (`drover mate <name>`): a dispatcher, at most three at a time. It starts crewmates with `drover new --mate <name>`, checks their work, and writes no code itself. Its playbook is `mates/AGENTS.md`.

`status` is one of `idle`, `working`, `blocked`, `done`, `unknown`. `idle` and `done` both mean ready for input. `blocked` means the agent waits at an approval or question dialog. `unknown` means drover cannot tell; it does not prove the turn finished.

## Start, drive, and close a task

```bash
drover config                 # host and remotes: the valid --machine values
drover agents                 # agents[N]{pane,machine,status,harness,cwd}
drover worktrees              # checkouts on every machine, open or closed
```

Check `drover agents` and `drover worktrees` first, so you do not start a second agent on a branch that already has one.

```bash
drover new --machine <machine> --repo <repo> --branch <branch> --prompt "<instructions>"
```

`--branch` takes a new name, an existing local or remote branch, or `#<pr>`. A branch whose workspace is open answers `noop: already open, prompt not sent`: use `drover send` for that pane, or `--at <pane>` for a second agent on its commit. drover appends the repo's recent journal entries and the best matches for the prompt (at most 2 KB), starts the agent, and sends the prompt once it is ready. Read the pane id from the reply.

```bash
drover send --pane <pane> --text "<prompt>"     # queued while the agent works
drover screen --pane <pane>
drover reply --pane <pane>                      # the last reply in full, from the transcript
drover keys --pane <pane> --keys "esc enter"    # prints the screen after the keys
drover done --pane <pane> --remove
```

- `--text -` (and `--prompt -` on `new`) reads the text from stdin. Use it for long or multi-line text, so the shell expands nothing: `drover send --pane <pane> --text - <<'EOF'`.
- `send` to a working agent keeps one queued prompt and sends it on the next idle. A second `send` before then fails with `one message is already queued`: wait for the reply.
- Read `drover screen` before `drover keys`, and before retrying a prompt. A dialog is the user's to answer unless the user's instructions cover it.
- `done` asks the agent for a five-line journal entry (what, why, outcome, decisions, next), stores it, then closes the workspace. `--no-journal` only when the user asks.
- Pass `--remove` once the branch is merged, or when the user wants the worktree deleted. drover removes only checkouts it opened, and `drover-wt` keeps a dirty checkout and an unmerged branch anyway. A report or note written inside the worktree makes it dirty: put it elsewhere, or read it before closing.
- Terminal commands do not open a Discord thread. Pass `--discord` only when the user asks, or later with `drover attach --pane <pane> --discord`.

## Coordinate mates and scouts

```bash
drover scout "<prompt>"
drover scout close --pane <pane>                 # also deletes its scratch folder
drover mate <name>
drover link --machine <machine> --pane <pane> --mate <name>
drover brief --mate <name> --text "<note>"
drover escalate --mate <name> --text "<question for the desk>"
drover take --pane <pane>
drover release --pane <pane>
```

- `link` hands an agent already running to a mate.
- `brief` adds a note to a mate's log; `escalate` is the mate asking the desk.
- `take` gives a mate's pane back to the user; `release` lets the mate drive it again. A mate reads `mates/state/<name>/owners` and never sends to a pane marked `user`.

## Journal and usage

```bash
drover recall <words>                 # full-text search over past entries
drover journal [--pane <pane>]        # five newest, of that task's repo when given
drover journal rm <id>
drover usage                          # claude and cursor quota on every machine
```

## Programs

`drover snapshot` prints the whole desk as JSON. `drover watch` prints it once, then one JSON line per change until it stops. Use these from scripts, not to read by eye.

## Safety and coordination rules

- Address a task by the `pane` or `thread` drover printed. Do not reach the same pane through `herdr` at the same time; drover follows it through herdr events.
- Do not close, remove, take, or release what you did not start unless the user asks. Do not delete journal entries you did not write.
- Remove a worktree with `drover done --remove`, or `drover-wt rm` for one drover no longer follows, never with `rm -rf` or `git worktree remove`.
- A Discord message is as good as typing in the pane, and drover adds no sandbox. Treat a prompt sent through drover like input typed at the desk.
- A timeout or a lost connection does not prove a prompt was not delivered. Read `drover screen` before sending it again.

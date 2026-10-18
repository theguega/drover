---
name: drover
description: "Drive drover, the herdr desk runtime, from the terminal. Use when the user mentions drover, a herdr agent or pane, a Discord task thread, /new, /done or the journal, or asks to start, read, send to, or close an agent through drover."
---

# drover

drover runs coding agents in herdr workspaces, one git worktree each, and reports the desk in the terminal. Output is TOON: `key: value` lines, and lists as `name[count]{fields}:` with one row per record. Errors use the same format.

Start with the home view:

```bash
drover
```

It shows the desk counts, what needs you (blocked agents, escalations), and the next commands. If it prints `error:`, read its `help:` line before going on.

## Feed ids into the next command

Each record puts the id that the next command takes first:

- `pane` (like `wB4:pE`) → `--pane`
- `machine` → `--machine`, from `drover config`
- `mate` → `--mate`, `drover mate <name>`
- `id` → `drover journal rm <id>`
- `thread` → `--thread`, a Discord thread

Copy the value as printed. Do not rebuild it from a name.

```bash
drover agents                 # agents[N]{pane,machine,status,harness,cwd}
drover screen --pane wB4:pE   # that agent's terminal
drover send --pane wB4:pE --text "run the tests again"
```

`status` is an enum: `idle`, `working`, `blocked`, `done`, `unknown`.

## Replies

- A change prints its ids, then `help:` with the next commands.
- `noop: <why>` with exit 0: the state already held (`already taken`, `already gone`). Nothing changed. Do not retry.
- `error: <what>` with exit 1: a real failure. Run the `help:` line.
- Exit 2: a usage error, such as an unknown flag. `help:` names the command to read.
- Long text (screens, journal entries) is cut at 1000 characters, with `chars` for the whole length. Add `--full` for all of it.
- `count: 5 of 42` means a list is capped.

For flags, run `drover <command> --help`. Do not guess them.

## Start and drive a task

```bash
drover config                 # host and remotes: the valid --machine values
drover new --machine <machine> --repo <repo> --branch <branch> --prompt "<instructions>"
drover send --pane <pane> --text "<prompt>"    # queued while the agent is working
drover screen --pane <pane>
drover keys --pane <pane> --keys "esc enter"   # read the screen first
drover done --pane <pane>                       # journal entry, then close
```

- Terminal commands do not open a Discord thread. Pass `--discord` only when the user asks.
- `done` asks the agent for a five-line journal entry. `--no-journal` only when the user asks. `--remove` only when the user wants the worktree deleted; drover removes only checkouts it opened.
- Read `drover screen` before `drover keys` or before retrying a prompt.
- Do not close or delete what you did not create unless the user asks.
- A repo-less scout closes with `drover scout close --pane <pane>`, not `done`.
- A mate dispatches crewmates (`drover new --mate <mate>`). `drover take` and `drover release` hand a pane between the desk and its mate.

## Journal and usage

```bash
drover recall <words>
drover journal [--pane <pane>]
drover usage
```

## Programs

`drover snapshot` prints the whole desk as JSON. `drover watch` prints it once, then one JSON line per change until it stops. Use these from scripts, not to read by eye.

---
name: drover
description: "Drive drover, the agent runtime: start coding agents (runs) in git worktrees on the host or SSH remotes, read their replies, send them the next prompt, approve runs a mate asked for, and close them. Use when the user mentions drover, a run (r-xxxxxx), a mate or crew, or asks to start, read, send to, approve, or close an agent through drover. For raw herdr panes, tabs and layout, use the herdr skill instead."
---

# drover

drover runs coding agents and has no model. `drover run` opens a git worktree (`$REPOS_ROOT/<repo>.<branch>`) and starts the agent's TUI in a herdr pane, on the host or an SSH remote. Each agent session is a **run**, `r-` and six hex digits. drover follows a run through herdr events, so its phase is always current: `pending`, `starting`, `working`, `needs`, `idle`, `done`, `failed`.

Before any other command, read the home view:

```bash
drover
```

It prints the runs per phase, the runs that need the user (pending approval, a dialog), and the next commands. It only reads. If it prints `error: drover is not running`, say so and stop. Do not start the service yourself.

## The CLI

The binary is the authority for syntax: `drover --help`, `drover <command> --help`. Do not guess flags.

```bash
drover run --repo <repo> --branch <branch> --prompt <prompt>   # starts now; - reads the prompt from stdin
drover ls [--role --machine --phase --all]
drover show <run>                  # phase, owner, last reply (300 chars), children
drover send <run> <text>           # the next prompt, only while idle
drover screen <run> · keys <run> <keys…> · stop <run>
drover done <run> [--remove] [--entry <text>]
```

## Who may do what

- A run started by an agent carries `DROVER_RUN` (its own id). drover checks it.
- A run that runs `drover run …` asks for a crew run, with itself as parent and owner (`--parent` may only name itself). It waits in `pending` until the user runs `drover approve-run <run>`, or `drover drop-run <run>`. Only the user approves.
- The parent owns its crew: only the owner may `send`, `keys` or `stop`. Anyone may read. `drover attach <run>` gives the run to the user.

## Read the output

Output is TOON: `key: value` lines, lists as `name[count]{fields}:` with one row per record. Errors print `error:` and `help:` with exit 1; exit 2 is a usage error. A change prints the run first, then `help:` with the next commands. `not implemented` marks a command a later version builds (`approve`, `deny`, `wait`, `goal`, `escalate`, `--headless`).

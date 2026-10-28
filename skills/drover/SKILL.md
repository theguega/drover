---
name: drover
description: "Drive drover, the agent runtime: start coding agents (runs) in git worktrees on the host or SSH remotes, read their replies, send them the next prompt, approve runs a mate asked for, start a mate, answer its escalations, and close runs. Use when the user mentions drover, a run (r-xxxxxx), a mate or crew, or asks to start, read, send to, approve, or close an agent through drover. For raw herdr panes, tabs and layout, use the herdr skill instead."
---

# drover

drover runs coding agents and has no model. `drover run` opens a git worktree (`$REPOS_ROOT/<repo>.<branch>`) and starts the agent's TUI in a herdr pane, or headless with `--headless`, on the host or an SSH remote. Each agent session is a **run**, `r-` and six hex digits. drover follows a run through the agent's hooks and stream, so its phase is always current: `pending`, `starting`, `working`, `needs`, `idle`, `done`, `failed`.

Before any other command, read the home view:

```bash
drover
```

It prints the runs per phase, the runs that need the user (pending approval, a dialog), and the next commands. It only reads. If it prints `error: drover is not running`, say so and stop. Do not start the service yourself.

## The CLI

The binary is the authority for syntax: `drover --help`, `drover <command> --help`. Do not guess flags.

```bash
drover run --repo <repo> --branch <branch> --prompt <prompt>   # starts now; - reads the prompt from stdin
drover run --role mate --repo <repo> --branch <branch> --headless --prompt <goal>   # a mate, see below
drover ls [--role --machine --phase --all]
drover show <run>                  # phase, owner, last reply (300 chars), children, a mate's goal graph and question
drover send <run> <text>           # the next prompt, only while idle
drover approve <run> <perm> · deny <run> <perm> --reason <why>   # a tool call in `needs`; `show` lists perms
drover watch [--run --role --since]   # events as JSON lines: started, text, tool_start, tool_end, needs, reply, done, failed
drover approve-run <run> · drop-run <run>   # a run a mate asked for, pending
drover screen <run> · keys <run> <keys…> · stop <run> · attach <run>
drover done <run> [--remove] [--entry <text>]
drover wait [--any] [--parent <run>] [--timeout <s>]   # blocks for the next reply, needs, done or failed
drover goal set --file goal.json · goal node <id> [--status --round --crew --review]   # a mate's graph
drover escalate --text <question>      # a run asks you; - reads it from stdin
```

## Who may do what

- A run started by an agent carries `DROVER_RUN` (its own id). drover checks it.
- A run that runs `drover run …` asks for a crew run, with itself as parent and owner (`--parent` may only name itself). It waits in `pending` until the user runs `drover approve-run <run>`, or `drover drop-run <run>`. Only the user approves.
- A run that runs `drover escalate --text …` asks you something: it shows in `drover` and in `drover ls --phase needs` with its `ask`, and stays in `needs` after its turn ends. Answer with `drover send <run> <text>`.
- The parent owns its crew: only the owner may `send`, `keys`, `stop`, `approve` or `deny`. Anyone may read. `drover attach <run>` gives the run to the user.

## Read the output

Output is TOON: `key: value` lines, lists as `name[count]{fields}:` with one row per record. Errors print `error:` and `help:` with exit 1; exit 2 is a usage error. A change prints the run first, then `help:` with the next commands. `not implemented` marks a command a later version builds (`--scratch`, harnesses other than claude).

## Mates

A mate is a run with `--role mate` that follows the `mates` skill (`skills/mates`): it plans a goal graph, requests crew with `drover run --parent $DROVER_RUN`, and drives each with `drover wait --any --parent $DROVER_RUN`. Only you start a mate. It must be headless or a pane in a worktree like any actor run, and the agent must have the `mates` skill installed and allow `drover` in its Bash permissions, or each call asks you. Its crew wait in `pending` for your `approve-run`; their tool calls go to the mate, not you. Look at `drover show <mate>` for its goal graph. The mate's `escalate` is its only voice to you.

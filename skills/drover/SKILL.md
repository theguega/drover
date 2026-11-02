---
name: drover
description: "Drive drover, the agent runtime: start coding agents (runs) in git worktrees, plain folders or scratch dirs on the host or SSH remotes, read their replies, send them the next prompt, approve runs a mate asked for, start a mate, answer its escalations, and close runs. Use when the user mentions drover, a run (r-xxxxxx), a mate or crew, or asks to start, read, send to, approve, or close an agent through drover. For raw herdr panes, tabs and layout, use the herdr skill instead."
---

# drover

drover runs coding agents and has no model. `drover run` opens a place (see below), by default a git worktree (`$REPOS_ROOT/<repo>.<branch>`), and starts the agent's TUI in a herdr pane, or headless with `--headless`, on the host or an SSH remote. Each agent session is a **run**, `r-` and six hex digits. drover follows a run through the agent's hooks and stream, so its phase is always current: `pending`, `starting`, `working`, `needs`, `idle`, `done`, `failed`.

Before any other command, read the home view:

```bash
drover
```

It prints the runs per phase, the runs that need the user (pending approval, a dialog), and the next commands. It only reads. If it prints `error: drover is not running`, say so and stop. Do not start the service yourself.

## Where a run works

Pick the place from the job. Each run's `place` (in `show` and `ls`) is `worktree`, `dir` or `scratch`.

| Job | Place | Command | What `done` does to the folder |
|---|---|---|---|
| a code change you will review, merge or throw away as a branch | worktree | `drover run --repo myapp --branch fix-retry --prompt …` | keeps it; `done --remove` removes the checkout (kept while dirty, branch only when merged) |
| work in a folder that is not a branch: notes, a data dir, a repo you want edited in place | `--dir` | `drover run --dir ~/notes --prompt "Tidy the headings in todo.md"` | never deletes it; `done --remove` is refused |
| a throwaway experiment, no repo | `--scratch` | `drover run --scratch --prompt "Try the regex crate on this sample"` | always removes the temp dir, with or without `--remove` |
| a lookup or a question about files, changing nothing | reader scout | `drover run --profile reader --dir ~/papers --prompt "Which of these mention JEPA?"` or `drover run --profile reader --repo myapp --prompt "Where is the retry policy set?"` | never deletes it |

- `--dir` takes an existing folder on the machine (absolute or `~/…`, git or not; over ssh for `--machine`). It cannot be combined with `--repo`, `--branch` or `--scratch`.
- `--key <k>` works with every place: a retry with the same key returns the first run, before any folder check, so a retried `--dir` run is not refused as a second actor.
- One actor per folder: a second actor in the same `--dir` on the same machine is refused with the run that has it open. Readers change nothing, so any number may read one folder.
- A reader scout is headless and has Read and the config's `reader_allow` commands only. `--repo` without `--branch` reads that repo's main checkout, with no worktree. An actor still needs `--branch`. A reader with neither `--dir` nor `--repo` runs on the host only, in the state dir's `reader/`. A pi reader takes neither.

## The CLI

The binary is the authority for syntax: `drover --help`, `drover <command> --help`. Do not guess flags.

```bash
drover run --repo <repo> --branch <branch> --prompt <prompt> [--key <k>]   # starts now; - reads the prompt from stdin; a retry with the same key returns the first run
drover run … --permission-mode <mode>   # Claude only; default auto: harmless calls run unasked, only what auto mode escalates is a `needs`. `default` asks for every call outside the allowlist
drover run --role mate --repo <repo> --branch <branch> --headless --prompt <goal>   # a mate, see below
drover run --harness cursor|pi …   # over ACP, always headless; `attach` opens the agent's own TUI on the session
drover ls [--role --machine --phase --all]
drover show <run>                  # phase, owner, last reply (first and last 300 chars), children, a mate's goal graph and question
drover reply <run>                 # the whole last reply
drover doctor [--json]             # each machine: tools, herdr's server, drover-wt at this build, cursor's login; a fix per failing check
drover machine ls · add <ssh> · rm <ssh>   # add: herdr machine add, drover-wt and the client copied over ssh (no clone on the remote), remotes updated, service restarted. Rerun to update. Not from a run. Then DROVER_HOST on the remote
drover send <run> <text> [--wait] [--key <k>]   # the next prompt, only while idle; or the answer to a cursor run's question (`needs` with tool AskQuestion). --wait blocks until the turn ends; --key: a retry with the same key sends nothing new and returns the first result
drover approve <run> <perm> · deny <run> <perm> --reason <why>   # a tool call in `needs`; `show` lists perms
drover approve <run> <perm> --answer Blue   # a question (AskUserQuestion): one --answer per question, a label or your own text; `send <run> Blue` answers a single one
drover watch [--run --role --since]   # events as JSON lines: started, text, tool_start, tool_end, needs, reply, done, failed
drover approve-run <run> · drop-run <run>   # a run a mate asked for, pending
drover screen <run> · keys <run> <keys…> · stop <run> · attach <run>
drover done <run> [--remove]   # closes the run and its pane; --remove also its worktree if clean and merged
drover wait [--any] [--parent <run>] [--timeout <s>]   # blocks for the next reply, needs, done or failed
drover goal set --file goal.json · goal node <id> [--status --round --crew --review]   # a mate's graph
drover escalate --text <question>      # a run asks you; - reads it from stdin
```

## Undo

A pane run can rewind with Claude's own `/rewind`: `drover send <run> /rewind` opens the picker, then `drover keys <run> Up Enter` picks the prompt, and `drover keys <run> Down Enter` picks an option: 1 restores code and conversation, 2 the conversation, 3 the code. Check the result with `drover screen <run>`. A headless run cannot rewind (`/rewind` is refused), so undo its changes with git in its worktree. A restore does not undo Bash changes, and it puts files back whole, losing later edits by anyone.

## Who may do what

- A run started by an agent carries `DROVER_RUN` (its own id). drover checks it.
- A run that runs `drover run …` asks for a crew run, with itself as parent and owner (`--parent` may only name itself). It waits in `pending` until the user runs `drover approve-run <run>`, or `drover drop-run <run>`. Only the user approves.
- A run that runs `drover escalate --text …` asks you something: it shows in `drover` and in `drover ls --phase needs` with its `ask`, and stays in `needs` after its turn ends. Answer with `drover send <run> <text>`.
- The parent owns its crew: only the owner may `send`, `keys`, `stop`, `approve` or `deny`. Anyone may read. `drover attach <run>` gives the run to the user.

## Read the output

Output is TOON: `key: value` lines, lists as `name[count]{fields}:` with one row per record. Errors print `error:` and `help:` with exit 1; exit 2 is a usage error. `send --wait` exits 0 on a reply, 1 when the run failed or ended, 3 when it needs you (`outcome:` says which). A change prints the run first, then `help:` with the next commands. `not implemented` marks a command a later version builds (harnesses other than claude, cursor and pi). Each run's `place` is `worktree`, `dir` or `scratch`.

## Mates

A mate is a run with `--role mate` that follows the `mates` skill (`skills/mates`): it plans a goal graph, requests crew with `drover run --parent $DROVER_RUN`, and drives each with `drover wait --any --parent $DROVER_RUN`. Only you start a mate. It must be headless or a pane in a worktree like any actor run, and the agent must have the `mates` skill installed and allow `drover` in its Bash permissions, or each call asks you. Its crew wait in `pending` for your `approve-run`; their tool calls go to the mate, not you. Look at `drover show <mate>` for its goal graph. The mate's `escalate` is its only voice to you.

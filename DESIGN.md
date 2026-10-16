# Design

drover is a model-free desk runtime. It relays Discord and terminal input to herdr panes, tracks which thread owns which pane, and keeps the journal. Agents are the only thing that thinks.

Status: **built** is what is in the code today. **designed** is agreed but not yet built.

## Layers

| Layer | Owns | Status |
|---|---|---|
| herdr | workspaces, tabs, panes, agent status, named agents | built |
| herdr-wt | worktrees: new branch, existing branch, `#pr`, remove | built |
| drover | Discord threads, thread → pane map, journal, usage, event log | built (log designed) |
| `mates/` | mate instructions and playbooks (`AGENTS.md`, short playbooks), `state/` | designed |
| skills | one copy in `~/.dotfiles/agents/.agents/skills`, symlinked from `~/.claude/skills` | built (dotfiles) |

## Lanes

Two lanes share the same herdr panes and never interfere.

- **Direct:** you route. A Discord thread or a terminal pane is one task on one worktree.
- **Mate:** a mate routes. It dispatches crewmates and reports back.

Every agent pane has one owner, `user` or `mate`. Owner changes only by an explicit command.

- A mate sends input only to panes it owns. It may read any pane.
- Takeover: `herdr agent attach` plus an owner flip. The mate checks the owner before every `prompt` or `send-keys`.
- herdr has no owner lock, so the check is drover's and the mate's job.
- Two agents may share a worktree. drover warns at dispatch and does not block.

## Harnesses

A harness is data: how to start it, submit a prompt, find its transcript, read its usage, and what it can do (resume, usage, visible subagents). claude and cursor are the two today.

Adding a harness adds one entry. A trait waits for a third harness that actually differs.

Skills location is per harness data too. Claude reads `~/.claude/skills` (symlink). Cursor reads `.agents` directly.

## Context

- A new task's context travels in the prompt drover sends, the same way `/new` appends journal recall. herdr-wt and herdr do not inject anything.
- Claude reads `AGENTS.md` and `CLAUDE.md` from the working directory on its own.
- Long tasks rely on the agent's own compaction. drover holds no conversation.

## Pane awareness

drover already receives `pane.agent_detected` and `agent.list`. A new pane is related to a task by metadata first, with no model call:

- same repo and branch as a task the mate knows: linked automatically
- otherwise: unrelated, ignored
- ambiguous: the mate asks in its own thread

Manual context: `/link <pane>` attaches a pane to a task, `/brief` adds a note. drover sends nothing to a pane unless asked.

## Names

herdr agent names mark what a pane is, and they survive a drover restart through `herdr agent list`.

- `mate-<name>` for a mate
- `scout-<id>` for a scout
- a task's agent keeps its own name

Names must match `[a-z][a-z0-9_-]{0,31}`.

## Mates

- Started from Discord (`/mate <name>`) or the terminal. Both call one code path.
- The pane goes into the `drover` workspace, tab `mates`, with cwd `drover/mates/`, the directory that holds its `AGENTS.md`.
- Up to three mates. drover refuses a fourth.
- A mate creates crewmate worktrees with `herdr-wt`. Repos resolve under `REPOS_ROOT`, so `mates/` needs no worktree of its own.
- A mate's SOP is a few short playbooks in `mates/`: dispatch, review, handoff, close. They cover spec before code, small diffs, tests as the check, a second look before merge, bounded context, and asking when ambiguous. Public references: [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills) and the open [Agent Skills spec](https://agentskills.io). Borrow checkpoints, do not copy whole packs.
- A mate's crewmates report only bounded output: the same five-line entry `/done` writes, plus status changes.

## Scouts

A scout is a short, unrelated look. No task, no repo, no journal.

- `/scout <prompt>` creates `mates/state/scouts/<id>/`, a plain directory with no git.
- It gets its own pane in a `scouts` tab, owned by the scout.
- It ends on `/scout close` only. drover closes the pane and deletes that one directory.
- The delete is restricted to the scouts root, and that path check is tested first.
- If a scout turns into real work, promote it to a task and use the normal `herdr-wt` path.

## CLI (built)

drover is also a terminal tool. `drover help` is the full reference, for humans and agents. Every Discord command has a terminal form, and the terminal form works the same whether or not Discord is involved.

- The service holds the state, so the CLI does not write state files. `drover <cmd>` sends one JSON line to the service over a unix socket in the state directory and prints the reply.
- Discord and the socket call one action layer: a `Command` enum and one `run` function that returns text. Discord handlers only translate interactions into commands and post the text.
- A task started from the CLI still gets a Discord thread, so the phone sees it too.
- Argument parsing is hand-written. No new dependency.
- `drover serve` runs the service. Any other first word is a CLI command, so `drover new …` and `drover mate …` need no flags.
- The `drover` skill documents every command for agents, and lives in the single skills folder described under Layers.

## Events

drover writes an append-only log per mate under `mates/state/<name>/log`. Lines are dispatched, linked, reported, escalated, closed. The log is what you read to understand what happened. A UI, if built, is only a view over it.

## Cleanup

`/done` (direct tasks) and a mate's close step do the same thing:

1. journal entry (unless `journal:false`)
2. `herdr-wt rm` only on a checkout drover opened, and only if clean and merged
3. pane closed
4. state marked closed

## Storage

| File | Holds | In git |
|---|---|---|
| `.env` | Discord and host config | no |
| `state.json` | thread → task map, pane owners | no |
| `journal.db` | journal entries (SQLite FTS5) | no |
| `mates/state/` | mate logs, scout directories | no |

## Non-goals

- drover calls no model.
- No sandbox or container. Panes run as your account, and agents run with your permissions.
- No plugin system until a second real use exists.
- No dashboard until the log proves insufficient.
- No orchestration across two hosts at once. One host runs drover.

## Open

- Escalation: always to the mate's Discord thread, or to a chosen channel?
- Mate log format: plain text or one JSON object per line?
- Scout id format: short random, or a slug from the prompt?

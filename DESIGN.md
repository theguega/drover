# Design

drover is an agent runtime for herdr. It starts agents in worktrees, follows them, relays Discord and terminal input to their panes, tracks which thread owns which pane, and keeps the journal. It ships its own worktree tool (`drover-wt`) and its own skill, and it installs with one curl command. drover is model-free: the agents are the CLIs, and they are the only thing that thinks.

Status: everything here is **built** on the `mates` branch. The known gaps at the end are not.

## Layers

| Layer | Owns | Status |
|---|---|---|
| herdr | workspaces, tabs, panes, agent status, named agents | built |
| drover-wt | worktrees: new branch, existing branch, `#pr`, remove | built |
| drover | Discord threads, thread → pane map, journal, usage, mate event log | built |
| `mates/` | mate instructions and playbook (`AGENTS.md`), `state/` (logs, owner lists, scouts) | built |
| skills | `skills/drover` in the repo, an Agent Skills folder that people copy into their own skills directory | built |

## Lanes

Two lanes share the same herdr panes and never interfere.

- **Direct:** you route. A Discord thread or a terminal pane is one task on one worktree.
- **Mate:** a mate routes. It dispatches crewmates and reports back.

Every agent pane has one owner, `user` or `mate`. Owner changes only by an explicit command.

- A mate sends input only to panes it owns. It may read any pane.
- Takeover: `herdr agent attach` plus `drover take`. The mate checks its owner list before every `prompt` or `send-keys`.
- herdr has no owner lock, so the check is drover's and the mate's job.
- Two agents may share a worktree. drover does not block it.

## Harnesses

A harness is data: how to start it, submit a prompt, find its transcript, read its usage, and what it can do (resume, usage, visible subagents). Two harnesses are built today.

Adding a harness adds one entry. A trait waits for a third harness that actually differs.

Skills are not drover's business. drover ships the `skills/drover` folder, and each person copies it where their agents read skills.

## Context

- A new task's context travels in the prompt drover sends, the same way `/new` appends journal recall. drover-wt and herdr do not inject anything.
- An agent reads the project instructions in its working directory on its own, so every worktree gets `AGENTS.md`.
- Long tasks rely on the agent's own compaction. drover holds no conversation.

## Pane awareness

drover sees every agent on the host. A pane it did not start is checked by its folder's repo and branch:

- same repo and branch as a task linked to a mate: linked to that mate, owner `user`, with its own thread
- same repo, another branch: the mate is asked in its thread, with the `drover link` line to run
- anything else: ignored

Nothing is sent to a pane unless asked. `drover link` attaches a running agent to a mate by hand. `drover brief` adds a note to a mate's log.

## Names

herdr agent names mark what a pane is, and they survive a drover restart through `herdr agent list`.

- `mate-<name>` for a mate
- `scout-<id>` for a scout
- a task's agent keeps its own name

Names must match `[a-z][a-z0-9_-]{0,31}`.

## Mates

- Started from Discord (`/mate <name>`) or the terminal (`drover mate <name>`). Both call one code path.
- The pane goes into the `drover` workspace, tab `mates`, with cwd `mates/` in the drover checkout: the folder that holds its `AGENTS.md`.
- Up to three mates. drover refuses a fourth.
- A mate dispatches crewmates with `drover new --mate <name>`. The crewmate's pane is owned by that mate, and it is listed in the mate's owner list.
- A mate's playbook (`mates/AGENTS.md`) covers the goal graph, model routing, dispatch, the loop, handoff and close, with the practices it works by: spec before code, small diffs, the check decides, a second look before merge, bounded context, and asking when ambiguous.
- A goal is a small graph in `state/<name>/goal.md`: nodes are crewmates, each with one check the mate runs itself, and edges are `after <node>`. One node is the default, since a crewmate runs its own subagents. Each node loops on crew news for at most three rounds without progress, then escalates. drover does not read the graph: it only carries the news. Public references: [addyosmani/agent-skills](https://github.com/addyosmani/agent-skills) and the open [Agent Skills spec](https://agentskills.io). Borrow checkpoints, do not copy whole packs.
- A mate's reports are bounded: what changed, what was checked, what is left.
- `drover escalate --mate <name> --text ...` posts in the mate's thread and logs it. A mate escalates a decision that is the desk's, a node stuck three rounds, and the finished graph, nothing else.
- A crewmate the mate owns has no thread of its own, so drover sends its replies, dialogs and exit to the mate's pane, or holds them in the mate's inbox until its turn ends.
- The mate's thread carries only answers to the desk's own messages, the mate's dialogs, and `drover escalate`. A turn started by crew news posts nothing.

## Scouts

A scout is a short, unrelated look. No repo, no journal.

- `drover scout <prompt>` and `/scout <prompt>` create `mates/state/scouts/scout-<id>/`, a plain folder with no git.
- The scout gets its own pane in a `scouts` tab of the drover workspace. The desk user owns it.
- It ends only on `drover scout close --thread <id>` or `/scout-close`. drover closes that pane and deletes that one folder.
- The delete only takes a direct child of the scouts folder named `scout-*`, and that guard is tested.
- `done` refuses a scout. If a scout becomes real work, start a task with `drover-wt` and `drover new`.

## CLI (built)

drover is also a terminal tool. `drover help` is the full reference, for humans and agents. Every Discord command has a terminal form, and the terminal form works the same whether or not Discord is involved.

- The service holds the state, so the CLI does not write state files. `drover <cmd>` sends one JSON line to the service over a unix socket in the state directory and prints the reply.
- Clients and the host. Only the host runs the service, and its socket is local, so another machine cannot reach it. A client's `drover` is `bin/drover-ssh`: it runs the same command on the host in `DROVER_HOST` over ssh. It refuses `serve`, which would start a second Discord bot.
- Discord and the socket call one action layer: an `Action` enum and one `act` function that returns a `Reply`. Discord handlers only translate interactions into actions and post `Reply::discord()`. The socket sends the reply as JSON, and the terminal prints it as TOON (`src/toon.rs`). `Reply::Text` carries a command not converted yet, printed as it is on both sides.
- A task started from the CLI stays in the terminal by default; `--discord` (or `drover attach --pane <id> --discord` later) opens a Discord thread so the phone sees it too.
- Argument parsing is hand-written. No new dependency.
- `drover serve` runs the service. Any other first word is a CLI command, so `drover new …` and `drover mate …` need no flags.
- The `drover` skill documents every command for agents, and lives in the single skills folder described under Layers.

## Terminal output (AXI)

The terminal is an [AXI](https://github.com/kunchenguid/axi): agents and other tools read it and turn it into the next command.

- stdout is [TOON](https://toonformat.dev). `snapshot` and `watch` stay JSON, for programs. Fields come out in the order the reply builds them (serde_json's `preserve_order`), so the order is part of each record's design.
- Errors are TOON on stdout too: `error: <what>` and `help: <command that fixes it>`. Exit 0 ok (a no-op too), 1 error, 2 usage error. A service error raised as `cli::Failure` carries its own `help`; any other falls back to `drover <cmd> --help`.
- One `Spec` per command in `cli.rs` is the only list of its flags: parsing, the unknown-flag error, `--help`, and `drover help` read it. An unknown flag fails before anything else, naming the flag and the valid ones.
- `drover` alone is the home view, built from the snapshot: `bin`, `description`, `desk` counts, `needs` (blocked agents, escalations), `help`. The counts are the live desk: a gone task is history and is not counted.
- `-v`, `-V` and `--version` print the bare version.
- Long text is cut to 1000 characters, with its whole length in `chars` and a `help` line for `--full`: a screen keeps its end, a journal entry its start. `--full` is declared only on commands that cut (`screen`, `journal`, `recall`). `reply` prints a task's last reply whole, from the transcript. Discord asks for everything, so its text does not change.
- A list capped at its limit says `count: 5 of 42`. An empty one says so in a line, such as `journal: 0 entries for drover`.
- A command that changes the desk answers with the fields that feed the next command (`pane`, `machine`, `thread`, ...), then `help`. When the desired state already held it exits 0 with `noop: <why>` (`already open, prompt not sent`, `already taken`, `already released`, `already ending`, `already gone`, `already closed`, `already running`) and changes nothing. Only a real failure exits 1. `keys` answers with the pane's screen, as `screen` does.
- Converted: every command but `snapshot`, `watch` and `complete`, which stay JSON or bare names.

### Field names

One name per thing, the same in TOON, the snapshot JSON and help placeholders (`<pane>` is the value of a `pane` field). A list is one record per row.

Field order, in every record: the id that feeds the next command first, then its status, then the rest, then long text last. A document ends with `help`. For example `agents{pane,machine,status,harness,cwd}`, `machines{name,reach}`, `flags{flag,default,about}`, `blocked{pane,machine,repo,branch}`, `escalations{mate,pane,at,text}`. Count maps keep a fixed key order with zeros shown.

| Field | Is | Feeds |
|---|---|---|
| `machine` | a machine name, as `drover config` lists it | `--machine <machine>` |
| `pane` | a herdr pane id, like `wAX:p1` | `--pane <pane>` |
| `thread` | a Discord thread id, present when one is bound (in the snapshot, only whether) | `--thread <thread>` |
| `mate` | a mate name | `--mate <mate>`, `drover mate <name>` |
| `id` | a journal entry id | `drover journal rm <id>` |
| `repo`, `branch` | the task's repo and branch | `--repo <repo>`, `--branch <branch>` |
| `harness` | the agent CLI: `claude` or `cursor` | `--agent <agent>` |
| `status` | the agent: `idle`, `working`, `blocked`, `done`, `unknown`. A worktree's: `working`, `blocked`, `done`, `open` (a workspace whose agent is idle or unknown), `closed` | |
| `phase` | the task: `starting`, `idle`, `waiting`, `ending`, `gone` | |
| `role` | `task`, `crew`, `mate`, `scout` | |
| `owner` | who drives a crew pane: `user`, `mate` | `drover take`, `drover release` |
| `reach` | a machine: `reachable`, `unreachable`, `unknown` | |
| `name` | the record's own name, in a list of machines or mates | |
| `at`, `resets_at` | unix seconds | |
| `changes` | a worktree's changed files, 0 when clean, null when the checkout is gone | |
| `window`, `percent` | a usage limit and how much of it is used | |
| `pool`, `used_usd`, `limit_usd` | a spend limit (`extra`, `plan`, `on-demand`, `on-demand-team`) and its dollars | |
| `day`, `text` | a journal entry's date and its text | |
| `workspace` | a herdr workspace id | |
| `scout` | a scout's id, like `scout-1a2b3c` | `drover scout close --pane <scout>` |
| `noop` | why nothing changed: the state already held | |
| `delivery` | a prompt sent: `sent` or `queued` | |
| `dropped` | a queued prompt `new --fresh` threw away with the old conversation | |
| `entry` | `done`'s journal entry: `asked` (the agent is writing it) or `none` | |
| `closed` | whether `done` has closed the task yet | |
| `teardown` | what `done` was asked to end with: `close` or `remove` | |
| `checkout` | after `done --remove`: `removed` or `kept` | |
| `event` | the mate log event a command wrote: `brief`, `escalated` | |
| `chars` | the whole length of a field that was cut | `--full` |
| `count` | `5 of 42`, on a capped list | |
| `cwd`, `bin` | a path, with the home directory as `~` | |
| `help` | next commands, complete, with `<placeholders>` | |

Status words are enums, never prose. Text meant for a person, such as herdr's own error, stays on Discord and out of TOON.

## Events

drover appends one line per event to a mate's log at `mates/state/<name>/log`: `<unix-time> <event> <detail>`. Events are `started`, `linked`, `owner`, `brief`, `escalated`, `crew` and `closed`. A `crew` line is news from a crewmate the mate owns (a reply, a dialog, an exit), which drover also sends to the mate's pane, or queues behind its turn. The log is what you read to see what happened. `drover snapshot` is a view over it.

## Snapshot

The desk as read-only JSON, for tools outside drover. It is a client of the action layer, like Discord and the terminal, and it holds no state of its own.

- `Action::Snapshot` builds the desk from the tasks in memory and from each mate's `goal.md` and log. `drover snapshot` prints it. It writes nothing and runs no shell.
- `drover watch` prints the snapshot as one JSON line, then one line per change, until the client exits. The service builds each line when `App.changed` fires, which every save notifies; changes that land during a build coalesce into the next line. `bin/drover-ssh` execs over `ssh -T`, so it streams the same way.
- A task's `reply` is the first 300 characters of its last turn and the whole length in `chars`, kept on the task when the turn ends, so a snapshot reads no transcript. It is null until a turn ends under drover.
- A harness is shown by name. A field a harness cannot supply, such as model, context fill or tokens, is absent, not guessed. `usage` is the same `windows`, `spend` and `unavailable` rows as `drover usage`, for every machine. It is read from a cache that a background task refreshes every 5 minutes and notifies `watch` on, so a snapshot never waits on the endpoints; it is null until the first read ends, and a failed read shows as `unavailable`.
- The mate log has no node, round or check, so those would become `key=value` fields on the same line, for example `1781600000 crew node=docs round=2 check=fail pane=crew:drover-docs`. Still one line per event, still grep-friendly.

## Cleanup

`/done` (direct tasks) and a mate's close step do the same thing:

1. journal entry (unless `journal:false`)
2. `drover-wt rm` only on a checkout drover opened, and only if clean and merged
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
- No orchestration across two hosts at once. One host runs drover.

## Decided

- **Escalation** goes to the mate's own Discord thread. There is no separate channel.
- **Mate log** is plain text, one line per event: `<unix-time> <event> <detail>`. It is grep-friendly, and it needs no parser.
- **Scout id** is `scout-` plus 6 random hex characters. It fits the agent name rule.

## Build order

All eleven steps are built, one commit each on the `mates` branch. Nothing is pushed yet.

1. Event log: append lines to `mates/state/<name>/log`.
2. Owner per pane: `user` or `mate`, default `user`. `take` and `release` switch it, and the owner list is written for the mate to read.
3. Harness as data: one entry per harness, replacing the string checks. Two harnesses today.
4. `drover mate <name>` and `/mate`: `drover` workspace, `mates` tab, cwd `mates/`, agent `mate-<name>`, a Discord thread, cap of three.
5. Mate playbooks in `mates/AGENTS.md`: dispatch, review, handoff, close. Short checklists, plus a herdr section that points to `herdr --skill`.
6. Mate close: `done` on a mate task logs `closed` and clears its owner entries.
7. Scouts: `drover scout <prompt>` and `/scout`, a `scouts` tab, `mates/state/scouts/<id>/`. `scout close` deletes that one directory. The path guard is tested first.
8. Pane awareness: `link` and `brief`. Same repo and branch as a known task means linked. Anything else is ignored. Ambiguous cases are asked in the mate's thread.
9. Escalation: `escalated` lines are posted to the mate's thread.
10. Docs: README, help text, the `drover` skill, and the status markers in this file match what is built.
11. Publish: each step is pushed to `theguega/drover` as it lands.

## Known gaps

- herdr has no owner lock. The check is the mate's, against its owner list, so a pane driven by hand can still be sent to by a mate that has not looked.
- A crewmate a mate starts with plain `drover-wt` and `herdr` is not linked. Only `drover new --mate` and `drover link` link.
- A scout is owned by the desk user, not by a scout. Scouts have no owner of their own.
- Mates run on the host only. Remote machines have no mates.

# drover

The user-facing guide is `README.md`. This file is how the program is put together. The design, including what is built and what is only designed, is in [`DESIGN.md`](DESIGN.md).

## Layout

```
src/main.rs     error type, err!/bail!, module list
src/cli.rs      clap: argv to one Cmd, which is also the socket request; prints TOON
src/control.rs  unix socket the service listens on, and the client side; `watch` holds it open
src/http.rs     127.0.0.1 HTTP: hook endpoints, GET /events (SSE), POST /runs/:id/permissions/:perm
src/serve.rs    the service: commands, headless processes, hooks and approvals, a herdr watcher per machine
src/goal.rs     a mate's goal graph (C16): its shape, checked once when set
src/harness/    the Harness trait (C12) and its adapters: claude.rs, cursor.rs, pi.rs
harness/pi.ts   pi extension: posts events, holds tool calls for approval; linked into ~/.pi/agent/extensions
src/run.rs      Run and its types: RunId, Machine, Mode, Role, Profile, Owner, Phase, Usage
src/store.rs    drover.db: runs, events, goals, and each run's `wait` cursor
src/migrate.rs  v1 state.json into runs, once
src/config.rs   state dir, config.toml (a flat TOML subset)
src/socket.rs   herdr socket: requests and `events.subscribe` (direct locally, one ssh relay per remote)
src/herdr.rs    herdr calls per machine, drover-wt over sh / ssh
src/toon.rs     JSON to TOON, what the terminal prints
fixtures/       v1-state.json, the live v1 state with reply text scrubbed, for the migration test
bin/drover-wt   worktrees for herdr: branch, new name, #pr, ls, rm (shipped, not from dotfiles)
bin/drover-ssh  the client on a non-host machine: runs drover on DROVER_HOST
skills/drover/  the drover skill, copied by hand into ~/.agents/skills
skills/mates/   the mates skill: a mate's dispatch playbook, copied the same way
install.sh      host: config.toml, build, links, user service. --remote: links only
bootstrap.sh    curl entry point: clones into ~/.local/share/drover, runs install.sh
scripts/live-test.sh  headless, approvals, pane and attach against a real claude (haiku) and herdr
scripts/live-test-mates.sh  a mate's crew, wait, goal and escalate against a real claude (haiku)
scripts/live-test-pi.sh  the same for pi; PI_PROVIDER and PI_MODEL pick the model
scripts/live-test-cursor.sh  headless, approvals and the hooks file against a real cursor-agent
```

State lives in `~/.local/state/drover/` (`drover.db`, `drover.sock`), config in `~/.config/drover/config.toml`.

drover is an agent runtime, and it has no model. It opens a worktree and starts the real agent CLI: its TUI in a herdr pane, or headless over its stream. Every pane run is an ordinary herdr workspace, so the desk can take it over at any point. The model lives in the agent CLI, never in drover.

```
drover <cmd> ─ drover.sock ─▶ drover serve ─ drover.db
dashboard ─ 127.0.0.1:7341 ─▶      │  ▲ hooks (http)
                                   ├─ claude -p stream-json     headless, warm between turns
                                   ├─ herdr.sock + drover-wt    $REPOS_ROOT/<repo>.<branch>
                                   └─ ssh relay, ssh -R tunnel  the same on each remote
```

## Dotfiles

drover ships with its own worktree tool (`bin/drover-wt`), linked to `~/.local/bin` by `install.sh`, and its own skill. Your desk is personal and lives in your dotfiles: the herdr keys in `herdr/.config/herdr/config.toml`, the `wt` alias, and the env (`PATH`, `REPOS_ROOT`, `HERDR_WORKTREE_PREFIX`) in `.zshenv`. Those point at `drover-wt`. drover reads none of them itself, except `repos_root` from its config, given to drover-wt on the host. The service starts through `zsh -c`, and ssh runs the login shell, so every machine's own dotfiles decide.

Remotes need `drover-wt` on PATH, so they run `install.sh --remote` too.

## Worktrees

`drover-wt` is the one entry point for the desk popup, the `wt` shell alias, and `drover run`. Do not switch those to native `herdr worktree create`.

Native herdr worktree (0.9.3):

- no optional prefix of its own (`HERDR_WORKTREE_PREFIX` is what adds one)
- a name that exists only on origin becomes a new branch from HEAD, which is an empty copy of someone else's work
- no `#pr` fetch
- checkouts go under `~/.herdr/worktrees/<repo>/<branch>`, not `$REPOS_ROOT/<repo>.<branch>`
- remove deletes the checkout and never the merged branch

The dotfiles blank native `new_worktree` so `prefix+shift+g` can run `drover-wt prompt`. `prefix+o` focuses the first blocked agent, else the first done one — the native toast target is gone with `ui.toast.delivery = "system"`.

## Harness adapters

`src/harness/mod.rs` holds the `Harness` trait: an adapter is its harness's protocol, as pure functions. `start` is the command line (pane or headless, profile, model, `--resume`), `send` a prompt as a stdin line, `events` one stdout line as `Out`s, `hook_settings` and `hook` the hooks it posts (`hook_file`: in a file in the cwd, not on the command line), `respond` the answer to a permission (back to the hook, or onto stdin), `abort`, and `caps` (`warm`: one process serves every turn, or each turn is a process). Spawning the process, here or over ssh, its stdin and stdout, and what the events do to a run are drover's own and the same for every harness. A new harness is one file in `src/harness/`.

Claude (`claude.rs`, see SPIKE.md):

- Headless: `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages`. `system/init` gives the session, `text_delta`s are text, `tool_use` and `tool_result` blocks are tool_start and tool_end, `result` is the reply with the turn's `usage`; `total_cost_usd` is the process's running total, so a turn's cost is the difference.
- Both modes get `--settings` with `http` hooks to `127.0.0.1:<http_port>/hooks/<run>`: `PermissionRequest` always, and for a pane `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop` (`last_assistant_message` is the reply) and `SessionEnd`. An actor gets `--permission-mode default`, since your default may be auto.
- A reader gets `--restricted --tools Bash,Read --permission-mode dontAsk` and allow rules for `brain find|get|pages|code` and `Read`: your own settings files (and allowlists) are not read, and anything else is denied without asking.

Pi (`pi.rs`, `harness/pi.ts`):

- Headless: `pi --mode rpc`. `message_update` `text_delta`s are text, `tool_execution_start`/`end` are tool_start and tool_end, and `agent_end` (unless `willRetry`) is the reply, its usage summed over the turn's assistant messages. A refused `prompt` response is a failed reply.
- pi has no hooks or permissions of its own. `--drover-hook <url>` makes `harness/pi.ts` (a global extension, linked by `install.sh`, inert without the flag) post to the run's hook URL: `session_start` (the session id), and for each tool that is not read-only (`read`, `grep`, `find`, `ls`) a `permission` that blocks until drover answers `{allow, reason}`; a deny returns `{block, reason}` to the model, and an unreachable drover blocks. A pane also gets `agent_start`, tool events, `agent_settled` (the reply) and `session_shutdown`.
- Resume is `--session <id>`. A reader gets `--drover-reader`: read-only tools and `brain find|get|pages|code` only.

Cursor (`cursor.rs`, see SPIKE.md q6):

- Headless: `cursor-agent --trust -p --output-format stream-json --stream-partial-output`. `cursor-agent` takes one prompt per process (no stream input; it reads stdin to EOF), so `warm` is false: each turn is a process, the prompt is its stdin, and the next `send` starts it again with `--resume <session>`, which keeps the id. `system/init` gives the session; an `assistant` line with a timestamp and no `model_call_id` is a text delta, the one with neither is the turn's last message and the reply (`result` joins all of the turn's text); `tool_call` `started`/`completed` are tool_start and tool_end, the tool being the key (`shellToolCall` is Shell), ok when its result has `success`. `usage` has tokens and no cost, so `cost_usd` stays 0.
- Approvals: cursor has no flag or env for hooks, so an actor run gets `.cursor/hooks.json` in its cwd, with `beforeShellExecution` and `beforeMCPExecution` commands running `drover hook <host:port/path>`. That posts to the run's hook endpoint, blocks on the answer, and prints `{"permission", "user_message", "agent_message"}`. cursor lets a call run when a hook times out, exits other than 2, or is missing, so a permission hook is `drover hook --closed <at> <secs> || exit 2`: `drover hook` gives up after `hook_timeout - 1` s (before cursor's timeout) and exits 2 on no answer, any error or a non-200, and `|| exit 2` covers a missing binary. drover's own deny deadline counts from the hook's arrival, not from the turns lock. Report hooks (pane) fail open with `{}`. cursor skips a hook command with `://` in it, so the address has no scheme. A hook outranks `--force`, and a shell call needs no `--force` once a hook allows it. Edits and reads are not held.
- The file is merged with the repo's own, whose hooks keep running. The repo's file waits beside it as `hooks.json.drover` and comes back when the run ends (or none, if it had none); a crash leaves both. An untracked `.cursor/hooks.json` in the worktree can be committed by the agent in the meantime. Remote machines are refused: the file is written here.
- A pane also gets `sessionStart`, `beforeSubmitPrompt`, `afterAgentResponse` (the reply: once a turn, with its whole text) and `stop` (only when not completed), `sessionEnd`. `--trust` skips the trust dialog, which would take the first prompt.
- A reader is `--mode ask` and has no hooks.

## Event loop

Events are C9: `started`, `text`, `tool_start`, `tool_end`, `needs`, `goal`, `reply`, `done`, `failed`. Each is stored and sent on a broadcast channel under one lock, so ids reach readers in order. `drover watch` and `GET /events` read through a `Feed`: stored events after `since`, then live ones; a reader that lags reads the store again. `requested` is stored and never streamed.

```
pending ──approve-run──▶ starting ──first prompt──▶ working ──reply──▶ idle ──send──▶ working
   └──drop-run (row deleted)        needs ◀──permission──▶ working      any ──done / pane gone──▶ done
                                    open fails, a process dies mid-turn ──▶ failed
```

- Headless: the process stays warm after a turn and is ended after `idle_kill` (a harness that is not `warm` ends it itself). `send` to a cold run spawns it again with `--resume`. A process that dies mid-turn fails its run; drover restarting leaves its headless runs idle, to resume on the next send.
- Ending a headless process (stop, done, attach, idle_kill, a run's end) is SIGTERM, 5 s, then SIGKILL: claude ends the tools it started on SIGTERM, and those run in sessions of their own, so a bare SIGKILL leaves them running. Each process prints `drover-pid <pid> <lstart>` first on stderr (a `sh`/`ssh` wrapper, then `exec`), kept in the `procs` table while it runs; a signal goes out only while pid and start time still match. A restarted serve ends the processes the table lists, and a resume ends its run's own first, so one session never has two processes.
- Pane: `agent.start` with the adapter's args, then `agent.wait` for idle or blocked. Blocked is a startup dialog (folder trust): a `needs` with `dialog`, answered with `keys`, after which the first prompt goes out. Every later change comes from hooks. herdr's events report only a pane or workspace that closed; a run's watcher subscribes while its machine has live pane runs.
- Turn handling, hooks and commands take one lock (`turns`). `send` refuses while a turn runs (Claude would queue the prompt).
- Approvals: a `PermissionRequest` hook is a `needs` event with a perm id (`p1`, `p2`, …) and blocks until the owner's `approve` or `deny`. With no answer `min(10, hook_timeout/2)` s before `hook_timeout`, drover denies it with "awaiting approval" and the run goes back to working; the call stays approvable for a day (at most 16 per run, in memory). A late approve follows `send`'s rule: refused while a turn runs, else it is the run's next prompt, and that turn may make the same call (same tool and input, Bash description aside) once without asking, since session allows do not survive `--resume`; the allowance ends with the turn. An ended run takes no hooks and no approvals: its waiting calls hear "the run has ended" and nothing of it stays approvable.
- Sessions: a hook must carry the run's session. A new one is taken when the run has none yet (a pane's first hook), and once after a resume drover started in a pane (attach: claude may resume under a copy's id) or after `SessionEnd` with reason `clear`. Any other hook is ignored and its permission request denied. A headless run's session comes from its own stream (`system/init`), which is drover's child, so it is always taken.
- Remote: a headless run is `ssh <machine> 'cd … && exec claude …'`, and each remote has one `ssh -N -R` tunnel carrying its hooks back to the host's port.

## Mates

A mate is a run with `role: mate`, started by you with `drover run --role mate` (a run that asks for a run is always crew, so a mate cannot start one). Its first prompt gets a line naming the `mates` skill, as a crew run's gets the crew rules; drover loads nothing else, and has no mate logic. A mate is an ordinary actor run with `DROVER_RUN` set, and its crew and approvals follow from `parent` and `owner`.

- Goal: `goal set --file` reads the file in the client (the service may be remote) and the service checks its shape (`Graph::parse`, deny unknown fields, ids unique, edges name nodes) and stores it in `goals`, one per run. `goal node` changes `status`, `round`, `crew` or `review` of one node. Each is a `goal` event carrying the whole graph. drover does not read edges or statuses. `show` prints the graph.
- `wait [--any] [--parent R]`: a run's own crew (a run may only name itself), or any run's for you. It reads `reply`, `needs`, `done` and `failed` through a `Feed` and returns what came after the cursor kept for the calling run in `waits`, advanced only on a return, so a timeout or a killed client loses nothing. From you it starts at now. `--timeout` (90 s) returns no events, under a shell tool's own timeout.
- `escalate --text`: a `needs` event with `escalation`, from any run, and the run goes to `needs`. A reply keeps `needs` while no tool call waits, so the question outlives the turn that asked it; `send` answers it, and is refused only while the turn still runs. `home` and `show` carry the question as `ask`.
- Crew approvals follow the owner: `approve` and `deny` need the run's owner, which is the mate that asked for it. You cannot, unless you `attach` it.

## Memory

Sessions are throwaway. drover holds no knowledge: `done --entry` hands the entry to `brain log` on stdin, before anything closes, and refuses when `brain` is not on PATH. drover stores runs, their events, and later mates' goal graphs. Long tasks rely on the agent's own compaction.

## Machines

The service runs on one host, named by `host` in the config. Each name in `remotes` is an ssh name whose herdr server is already running; drover never starts one. Match it to the saved herdr machine label so the desk and drover agree. One service at a time: it owns `drover.db`.

Every other machine is a client: `install.sh --remote` links `drover-wt` and `bin/drover-ssh` as `drover`, and `DROVER_HOST` names the host. `drover-ssh` passes `DROVER_RUN` along, so a remote run speaks as itself.

## Rules the code follows

Enforced by the lints in `Cargo.toml`:

- `unsafe` is forbidden. `unwrap`, `expect`, `panic!`, and slice indexing (serde_json `v["k"]` included) are denied.
- Enums instead of flag combinations (`Phase`, `Mode`, `Owner`, `Role`, `Reach`, `Status`).
- Newtypes for ids (`RunId`, `Machine`, `PaneId`, `SessionId`, `Name`), validated once at the edge. herdr output is parsed straight into typed structs.
- A typed `herdr::Error` (codes like `agent_not_ready` are matched). Elsewhere a boxed `std::error::Error` with `bail!` and `err!`; a `control::Failure` carries the `help:` line to the terminal.
- Five dependencies: tokio, serde, serde_json, rusqlite, clap. Prefer a few lines over a crate: config.toml is read by a 30-line parser.
- The owner check lives in the service: `send`, `keys`, `stop`, `approve` and `deny` need the run's owner (a crew run's approvals go to its mate), `approve-run`, `drop-run` and `attach` need you (no `DROVER_RUN`).
- HTTP listens on 127.0.0.1 only, closes a connection whose request has not arrived within 5 s, and refuses a request with an `Origin` header or a `Host` that is not its address, so a browser page cannot reach it.
- `state.json` from v1, and from the TypeScript version before it, still migrates. There are tests for both.

Repo and branch names are checked (letters, digits, `# . _ / -`), machine names likewise, and session ids must be uuids before they reach a shell. drover adds no sandbox.

## Development

```sh
cargo run -- serve --state-dir /tmp/drover-test --config /tmp/drover-test/config.toml
DROVER_STATE_DIR=/tmp/drover-test cargo run -- ls
cargo test
cargo clippy --all-targets -- -D warnings
```

A test service needs its own state dir and `http_port`: the defaults are the live service's. `scripts/live-test.sh` sets both up. Keep the path short, a unix socket path is capped at 108 bytes. Startup refuses to launch if `herdr`, `drover-wt`, `git`, `jq`, or `ssh` is missing from `PATH`.

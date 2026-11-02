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
src/machine.rs  `drover machine`: ls, add (herdr, drover over ssh, remotes, restart), rm; runs here, not through serve
src/harness/    the Harness trait (C12) and its adapters: claude.rs, acp.rs (cursor and pi)
src/run.rs      Run and its types: RunId, Machine, Mode, Role, Profile, Owner, Phase, Usage
src/store.rs    drover.db: runs, events, goals, and each run's `wait` cursor
src/config.rs   state dir, config.toml (a flat TOML subset)
src/socket.rs   herdr socket: requests and `events.subscribe` (direct locally, one ssh relay per remote)
src/herdr.rs    herdr calls per machine, drover-wt over sh / ssh
src/toon.rs     JSON to TOON, what the terminal prints
bin/drover-wt   worktrees for herdr: branch, new name, #pr, ls, rm (shipped, not from dotfiles)
bin/drover-ssh  the client on a non-host machine: runs drover on DROVER_HOST
skills/drover/  the drover skill, copied by hand into ~/.agents/skills
skills/mates/   the mates skill: a mate's dispatch playbook, copied the same way
install.sh      host: config.toml, build, links, user service. --remote: links only
bootstrap.sh    curl entry point: clones into ~/.local/share/drover, runs install.sh
scripts/live-test.sh  headless, approvals, pane and attach against a real claude (haiku) and herdr
scripts/live-test-mates.sh  a mate's crew, wait, goal and escalate against a real claude (haiku)
scripts/live-test-tunnel.sh  REMOTE=<ssh name>: serve killed with SIGKILL and SIGTERM, the hooks tunnel binds again within 60 s
scripts/live-test-cursor.sh  cursor over ACP: warm turns, approve, deny with a reason, stop, resume after kill -9; REMOTE=<ssh name> runs it there
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

drover ships with its own worktree tool (`bin/drover-wt`), linked to `~/.local/bin` by `install.sh`, and its own skill. Your desk is personal and lives in your dotfiles: herdr's keybindings, a `wt` alias, and the env (`PATH`, `REPOS_ROOT`, `HERDR_WORKTREE_PREFIX`) in your shell's env file. Those point at `drover-wt`. drover reads none of them itself, except `repos_root` from its config, given to drover-wt on the host. The service starts through `zsh -lc`, and ssh runs the login shell, so every machine's own dotfiles decide.

Remotes need `drover-wt` on PATH, so they run `install.sh --remote` too.

## Worktrees

`drover-wt` is the one entry point for the desk popup, the `wt` shell alias, and `drover run`. Do not switch those to native `herdr worktree create`.

Native herdr worktree (0.9.3):

- no optional prefix of its own (`HERDR_WORKTREE_PREFIX` is what adds one)
- a name that exists only on origin becomes a new branch from HEAD, which is an empty copy of someone else's work
- no `#pr` fetch
- checkouts go under `~/.herdr/worktrees/<repo>/<branch>`, not `$REPOS_ROOT/<repo>.<branch>`
- remove deletes the checkout and never the merged branch

A desk can blank herdr's native `new_worktree` key and bind one to `drover-wt prompt` instead.

## Places

A run's `place` (`Worktree`, `Dir`, `Scratch`) says where its cwd came from. `cmd_run` resolves it once on the run's machine (`Serve::place`, over `herdr::dir`, `main_checkout`; a scratch dir is made at open by `herdr::scratch`) and the folder is kept in the `requested` event. A pane run in a `Dir` or `Scratch` opens a plain `workspace.create`, not drover-wt. `done` never deletes a `Dir` (`--remove` is refused) and always deletes a `Scratch`, which `end` also sweeps for runs that end another way. `Serve::free` keeps two actors out of one folder.

## Harness adapters

`src/harness/mod.rs` holds the `Harness` trait: an adapter is its harness's protocol, as pure functions over the state it keeps per process (`Parse`). `start` is the command line (headless, or the TUI a pane runs, profile, model, resume), `open` the lines written first, `send` a prompt as stdin lines, `events` one stdout line as `Out`s (`Out::Write` is a protocol line back on stdin, `Out::Fail` a process that cannot serve the run), `answer` a permission asked on stdout, answered on stdin, `hook` and `respond` a hook and its answer, `abort`, `forward` the variables a remote process reads from its stdin, and `caps` (`pane`: runs in a pane, observed by hooks). Spawning the process, here or over ssh, its stdin and stdout, and what the events do to a run are drover's own and the same for every harness. A new harness is one file in `src/harness/`, or a line in `acp.rs` when it speaks ACP.

Claude (`claude.rs`):

- Headless: `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages`. `system/init` gives the session, `text_delta`s are text, `tool_use` and `tool_result` blocks are tool_start and tool_end, `result` is the reply. Its `modelUsage` and `total_cost_usd` are the session's totals (subagents, and a resumed session's earlier processes, included), so a turn's usage is what they grew by; a resumed process starts from the run's own usage. A turn begins at its first main-thread output, which also covers the turn Claude starts itself when a background task ends; an empty `result` for a notification already delivered (`num_turns: 0`) is dropped. `system/background_tasks_changed` counts the process's background tasks: while any run it is idle-killed after `background_kill` (default 10 × `idle_kill`) instead, and the run gets a `text` event saying why.
- An actor headless gets `--permission-prompt-tool stdio`: without a permission host `-p` offers no AskUserQuestion or ExitPlanMode. The hook answers first and Claude cancels its stdout `control_request`; a `can_use_tool` request still open `hook_timeout` + 5 s after it came is denied on stdin (`Out::Backstop`, `Parse.held`), since a hook with no decision leaves the turn waiting there. Every way out of `App::hook` answers a permission with a deny, never `{}`, and the deadline deny does not wait for the turns lock. An allow of those two carries the call's input as `updatedInput` (they need it), with `approve --answer`s as AskUserQuestion's `answers`; any other allow is plain, since an updated input meets the `ask` rules again and a pane then shows its own dialog; `approve` needs exactly one `--answer` per question (`Harness::questions`), and `drover send <text>` answers a call with one question, as it does cursor's `AskQuestion`. A late answer is the next prompt and records no allowance.
- A prompt starting with `/` sent to a pane leaves the phase alone: a command the TUI runs itself (`/context`) posts no hook, and one that is a turn posts `UserPromptSubmit` (`/init`, skills, custom commands) or `PreCompact`/`PostCompact` with trigger `manual` (`/compact`: working, then an empty reply).
- Both modes get `--settings` with `http` hooks to `127.0.0.1:<http_port>/hooks/<run>`: `PermissionRequest` always, and for a pane `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop` (`last_assistant_message` is the reply) and `SessionEnd`. An actor gets `--permission-mode auto` unless `drover run --permission-mode <mode>` asked for another (kept in the run's request, so a resume passes it again): auto mode runs harmless calls itself and the hook gets only what it escalates, such as a call an `ask` rule matches (an `ask` rule in the worktree's `.claude/settings.local.json` is how a test gets a `needs`). Without auto, `PermissionRequest` fires only for a call that needs a permission: a read-only or allowlisted call never reaches drover. A hook that times out is a deny with Claude's own text, so drover answers before the timeout.
- A reader gets `--restricted --tools Bash,Read --permission-mode dontAsk` and allow rules for `Read` and each `reader_allow` command in the config (`--tools Read` alone when there are none): your own settings files (and allowlists) are not read, and anything else is denied without asking.

Cursor and pi (`acp.rs`): the Agent Client Protocol, JSON-RPC 2.0 a line over the agent's stdio. Cursor is `cursor-agent acp`, pi is `pi-acp` (npm, which runs `pi --mode rpc`; its session id is pi's). Headless only: `drover run` runs them headless, and `attach` stops the process and opens the agent's own TUI on the session in a pane (`cursor-agent --resume <id>`, `pi --session <id>`), unobserved and yours.

- One warm process per run: `initialize` (no fs or terminal for the agent: it runs its own tools), then `session/new` with the run's cwd, or `session/load` to resume (its replayed history is not a turn), then `session/set_mode` for a cursor reader (`ask`) and `session/set_model` when `--model` names one of the session's models (by id, id before its `[…]`, or name: `auto` is cursor's Auto). Prompts sent before that wait. No `authenticate`: cursor's `cursor_login` opens a browser login; its CLI login is the login; without one `session/new` answers -32000 `Authentication required` and the run fails naming the login command.
- A turn is `session/prompt`. `agent_message_chunk` is text, `tool_call` and a `tool_call_update` that is `completed` or `failed` are tool_start and tool_end (the tool is the ACP `kind`, `execute` being Shell), and the prompt's response is the reply: the turn's text since its last tool call. Usage only when reported: the response's (unstable) `usage`, and `usage_update`'s cumulative USD `cost`, a turn's being the difference. cursor reports neither.
- `session/request_permission` is a `needs` with a perm id; it carries no `rawInput`, so its input is the one the call's earlier `tool_call` update had. It waits with no deadline: the agent waits. Approve is the option of kind `allow_once`, deny `reject_once`; drover never picks `allow_always`, which writes cursor's allowlist. cursor tells the model only "User rejected", so a deny with `--reason` also cancels the turn and sends `drover: the user denied <call>: <reason>` as the next prompt; that turn's reply is the run's. `cursor/ask_question` is a `needs` with tool `AskQuestion`, answered by `drover send`: text naming an option's id or label picks it, other text is the answer as written (the questions skipped with it as the reason). `cursor/create_plan` is a `needs` with tool `CreatePlan`: approve accepts, deny rejects with the reason. Whatever cancels a turn (that deny, `stop`) or ends it (cursor drops calls asked together when one is rejected) answers every call still waiting `{"outcome": "cancelled"}`, as ACP asks, and its `needs` is over (`Out::Cancel`); a reason is kept for the next prompt only while a prompt runs. Notifications other than `session/update` (`cursor/update_todos`, `cursor/task`) are dropped; other requests get method-not-found.
- `stop` is `session/cancel`: waiting calls are cancelled, the turn ends with its reply, and the process stays warm. Before the session is set up, `stop` ends the process and its queued first prompt. Retiring a process (attach, stop, idle_kill) answers its waiting calls first. A stdout line over 16 MiB is skipped. The process stays warm between turns and is ended after `idle_kill` like Claude's; a resume is a new process with `session/load`.
- Approvals follow cursor's own config: its `approvalMode` (`unrestricted` runs every call without asking, `allowlist` asks for shell calls outside `permissions.allow`, edits outside the workspace or under `.cursor/`). pi has no permissions: its tools run. A pi extension that calls `ctx.ui.confirm` reaches drover as a `needs` through pi-acp.
- A cursor reader is mode `ask`. pi has no read-only mode: the reader dir's `.pi/settings.json` sets `defaultTools` to `read, grep, find, ls`.
- Remote: `ssh <machine> 'cd … && exec cursor-agent acp'`. cursor runs with `AGENT_CLI_CREDENTIAL_STORE=file` (a plain `env` prefix), since a mac's login keychain is locked to an ssh session: its OAuth login is `~/.cursor/auth.json` instead, made once per macOS remote with `ssh -t <machine> 'AGENT_CLI_CREDENTIAL_STORE=file NO_OPEN_BROWSER=1 ~/.local/bin/cursor-agent login'` (`install.sh --remote` prints it). The remote agent runs under `env -u CURSOR_API_KEY`, so a key exported by the remote's login shell cannot override the login. `DROVER_REMOTE_CURSOR_CONFIG_DIR`, from drover's environment, is the remote's `CURSOR_CONFIG_DIR` (a path there), sent first on the process's stdin and read by the remote shell (`IFS= read -r`) before it execs.

## Event loop

Events are C9: `started`, `text`, `tool_start`, `tool_end`, `needs`, `goal`, `reply`, `done`, `failed`. Each is stored and sent on a broadcast channel under one lock, so ids reach readers in order. `drover watch` and `GET /events` read through a `Feed`: stored events after `since`, then live ones; a reader that lags reads the store again. `requested` is stored and never streamed.

```
pending ──approve-run──▶ starting ──first prompt──▶ working ──reply──▶ idle ──send──▶ working
   └──drop-run (row deleted)        needs ◀──permission──▶ working      any ──done / pane gone──▶ done
                                    open fails, a process dies mid-turn ──▶ failed
```

- Headless: the process stays warm after a turn and is ended after `idle_kill`. `send` to a cold run spawns it again with `--resume` (ACP: `session/load`). A process that dies mid-turn fails its run; drover restarting leaves its headless runs idle, to resume on the next send.
- Ending a headless process (stop, done, attach, idle_kill, a run's end) is SIGTERM, 5 s, then SIGKILL, to the process, its process group (each is spawned in one) and everything below it by parent pid: tools run in sessions of their own (cursor-agent does not end them), so a bare SIGKILL leaves them running. A tool that detached and was orphaned while the process lived is not found. Each process prints `drover-pid <pid> <lstart>` first on stderr (a `sh`/`ssh` wrapper, then `exec`), kept in the `procs` table while it runs; a signal goes out only while pid and start time still match (macOS pads `lstart` with spaces: both sides are trimmed), and the row goes only once the process is gone. A restarted serve ends the processes the table lists, and a resume ends its run's own first, so one session never has two processes.
- Pane: `agent.start` with the adapter's args, then `agent.wait` for idle or blocked. Blocked is a startup dialog (folder trust): a `needs` with `dialog`, answered with `keys`, after which the first prompt goes out. Every later change comes from hooks. herdr's events report only a pane or workspace that closed; a run's watcher subscribes while its machine has live pane runs.
- Turn handling, hooks and commands take one lock (`turns`). `send` refuses while a turn runs (Claude would queue the prompt).
- Approvals: a call an ACP agent asks about on its stdout is a `needs` that waits for the owner, with no deadline. A `PermissionRequest` hook is a `needs` event with a perm id (`p1`, `p2`, …) and blocks until the owner's `approve` or `deny`. With no answer `min(10, hook_timeout/2)` s before `hook_timeout`, drover denies it with "awaiting approval" and the run goes back to working; the call stays approvable for a day (at most 16 per run, in memory). A late approve follows `send`'s rule: refused while a turn runs, else it is the run's next prompt, and that turn may make the same call (same tool and input, Bash description aside) once without asking, since session allows do not survive `--resume`; the allowance ends with the turn. An ended run takes no hooks and no approvals: its waiting calls hear "the run has ended" and nothing of it stays approvable.
- Sessions: a hook must carry the run's session. A new one is taken when the run has none yet (a pane's first hook), and once after a resume drover started in a pane (attach: claude may resume under a copy's id) or after `SessionEnd` with reason `clear`. Any other hook is ignored and its permission request denied. A headless run's session comes from its own stream (`system/init`), which is drover's child, so it is always taken.
- Remote: a headless run is `ssh <machine> 'cd … && exec claude …'`, and each remote has one `ssh -N -R` tunnel carrying its hooks back to the host's port. The tunnel's ssh runs under a `sh` that kills it when its stdin pipe, held by serve, closes: serve dying by SIGKILL or SIGTERM must not leave an ssh holding the remote's port over a live connection. ServerAliveInterval 10 s x 3 ends it on a dead peer; the remote's sshd notices a silent dead client only with its own `ClientAliveInterval` (macOS sshd has none), so after a network drop the bind can fail until then: retried with backoff (max 10 s), logged once per change. A remote's worktrees go under its own `REPOS_ROOT`, or `remote_repos_root` from the config (absolute: a path on the remotes).

## Mates

A mate is a run with `role: mate`, started by you with `drover run --role mate` (a run that asks for a run is always crew, so a mate cannot start one). Its first prompt gets a line naming the `mates` skill, as a crew run's gets the crew rules; drover loads nothing else, and has no mate logic. A mate is an ordinary actor run with `DROVER_RUN` set, and its crew and approvals follow from `parent` and `owner`.

- Goal: `goal set --file` reads the file in the client (the service may be remote) and the service checks its shape (`Graph::parse`, deny unknown fields, ids unique, edges name nodes) and stores it in `goals`, one per run. `goal node` changes `status`, `round`, `crew` or `review` of one node. Each is a `goal` event carrying the whole graph. drover does not read edges or statuses. `show` prints the graph.
- `wait [--any] [--parent R]`: a run's own crew (a run may only name itself), or any run's for you. It reads `reply`, `needs`, `done` and `failed` through a `Feed` and returns what came after the cursor kept for the calling run in `waits`, advanced only on a return, so a timeout or a killed client loses nothing. From you it starts at now. `--timeout` (90 s) returns no events, under a shell tool's own timeout.
- `escalate --text`: a `needs` event with `escalation`, from any run, and the run goes to `needs`. A reply keeps `needs` while no tool call waits, so the question outlives the turn that asked it; `send` answers it, and is refused only while the turn still runs. `home` and `show` carry the question as `ask`.
- Crew approvals follow the owner: `approve` and `deny` need the run's owner, which is the mate that asked for it. You cannot, unless you `attach` it.

## Memory

Sessions are throwaway. drover holds no knowledge: the agents write their own notes, through their own skills. drover stores runs, their events, and later mates' goal graphs. Long tasks rely on the agent's own compaction.

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

Repo and branch names are checked (letters, digits, `# . _ / -`), machine names likewise, and session ids must be uuids before they reach a shell. drover adds no sandbox.

## Development

```sh
cargo run -- serve --state-dir /tmp/drover-test --config /tmp/drover-test/config.toml
DROVER_STATE_DIR=/tmp/drover-test cargo run -- ls
cargo test
cargo clippy --all-targets -- -D warnings
```

A test service needs its own state dir and `http_port`: the defaults are the live service's. `scripts/live-test.sh` sets both up. Keep the path short, a unix socket path is capped at 108 bytes. Startup refuses to launch if `herdr`, `drover-wt`, `git`, `jq`, or `ssh` is missing from `PATH`.

# drover

The user-facing guide is `README.md`. This file is how the program is put together. The design, including what is built and what is only designed, is in [`DESIGN.md`](DESIGN.md).

## Layout

```
src/main.rs     error type, err!/bail!, module list
src/cli.rs      clap: argv to one Cmd, which is also the socket request; prints TOON
src/control.rs  unix socket the service listens on, and the client side; `watch` holds it open
src/serve.rs    the service: what each command does to the runs, one herdr event watcher per machine
src/run.rs      Run and its types: RunId, Machine, Mode, Role, Profile, Owner, Phase, Usage
src/store.rs    drover.db: runs, events, goals
src/migrate.rs  v1 state.json into runs, once
src/config.rs   state dir, config.toml (a flat TOML subset)
src/socket.rs   herdr socket: requests and `events.subscribe` (direct locally, one ssh relay per remote)
src/herdr.rs    herdr calls per machine, drover-wt and transcripts over sh / ssh
src/toon.rs     JSON to TOON, what the terminal prints
fixtures/       v1-state.json, the live v1 state with reply text scrubbed, for the migration test
bin/drover-wt   worktrees for herdr: branch, new name, #pr, ls, rm (shipped, not from dotfiles)
bin/drover-ssh  the client on a non-host machine: runs drover on DROVER_HOST
skills/drover/  the drover skill, copied by hand into ~/.agents/skills
install.sh      host: config.toml, build, links, user service. --remote: links only
bootstrap.sh    curl entry point: clones into ~/.local/share/drover, runs install.sh
mates/          v1 mate instructions (v2 mates are a later node, see DESIGN.md)
```

State lives in `~/.local/state/drover/` (`drover.db`, `drover.sock`), config in `~/.config/drover/config.toml`.

drover is an agent runtime, and it has no model. It opens a worktree, starts the real agent TUI inside herdr, and records the end of each turn. Every pane run is an ordinary herdr workspace, so the desk can take it over at any point. The model lives in the agent CLI, never in drover.

```
drover <cmd> ─ drover.sock ─▶ drover serve ─ drover.db
                                  │
                                  ├─ herdr.sock + drover-wt    $REPOS_ROOT/<repo>.<branch>
                                  └─ ssh relay + ssh          the same on each remote
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

## Event loop

herdr's socket takes one request per connection and closes it after the reply. Locally drover connects to `~/.config/herdr/herdr.sock` per request (~0.1 ms). Each remote keeps one ssh open to a short Python relay that opens a socket connection per request line and streams every reply line back, matched by `id` (~1 RTT, about 100 ms to a remote, against ~2 s for `herdr --machine`). The relay needs `python3` on the remote. Only git (`drover-wt`) and remote transcript reads still spawn a shell.

Each machine gets a watcher holding one `events.subscribe` — `pane.agent_status_changed` per live pane run, plus `pane.agent_detected`, `pane.closed`, `pane.exited`, and `workspace.closed` anywhere. `agent.list` runs only after (re)subscribing and when an agent starts, leaves, or a workspace closes. A change in the set of live panes resubscribes, subscribing before that reconcile so no event falls between them.

```
pending ──approve-run──▶ starting ──ready: first prompt──▶ working ──turn read──▶ idle ──send──▶ working
   └──drop-run (row deleted)        blocked: needs ◀──▶ working          any ──done / pane gone──▶ done
                                    open fails, first prompt fails ──▶ failed
```

- The prompt a run was asked for is a `requested` event; it goes out on the agent's first ready push, then a `started` event.
- A ready push reads the transcript's newest turn. A turn not written yet leaves the run `working`, so an early push never loses a reply. Each reply is a `reply` event: the first 300 characters, `chars`, and the transcript line's id.
- Turn handling and `send` take one lock. `send` first reads a turn that ended before its push came, refuses while a turn runs (Claude would queue the prompt), and after typing waits until herdr sees the turn start.
- herdr may send no status change for a turn typed within a few ms of the previous turn's end. That turn is read at the next push, the next `send`, or a restart.

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
- The owner check lives in the service: `send`, `keys` and `stop` need the run's owner, `approve-run`, `drop-run` and `attach` need you (no `DROVER_RUN`).
- `state.json` from v1, and from the TypeScript version before it, still migrates. There are tests for both.

Repo and branch names are checked (letters, digits, `# . _ / -`), machine names likewise, and session ids must be uuids before they reach a shell. drover adds no sandbox.

## Development

```sh
cargo run -- serve --state-dir /tmp/drover-test --config /tmp/drover-test/config.toml
DROVER_STATE_DIR=/tmp/drover-test cargo run -- ls
cargo test
cargo clippy --all-targets -- -D warnings
```

A test service needs its own state dir: the default one is the live service's. Keep the path short, a unix socket path is capped at 108 bytes. Startup refuses to launch if `herdr`, `drover-wt`, `git`, `jq`, or `ssh` is missing from `PATH`.

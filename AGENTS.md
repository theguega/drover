# drover

Claude reads this when there is no `CLAUDE.md`. The user-facing guide is `README.md`. This file is how the program is put together.

## Layout

```
src/main.rs     discord: commands, threads, reactions, event watchers, prompt queue
src/events.rs   herdr `events.subscribe` (local socket; ssh+python relay on remotes)
src/herdr.rs    herdr and ssh per machine, transcript replies
src/journal.rs  journal store, search, context for new tasks
bin/herdr-wt    worktree open/list/remove. The desk `wt` execs this file
desk/herdr.toml sample desk keys (tuicr, lazygit, herdr-wt)
state.json      thread → task map (gitignored)
journal.db      journal (gitignored)
```

drover has no model. It opens a worktree, starts the real `claude` (or `codex`, `cursor`, `opencode`, `pi`) TUI inside herdr, and posts the final answer. Every task is an ordinary herdr workspace, so the desk can take it over at any point.

```
phone ─ discord ─▶ drover  (Rust, a user service on the host)
                     │
                     ├─ herdr-wt + herdr     $REPOS_ROOT/<repo>.<branch>
                     └─ ssh / herdr --machine    the same on each remote
```

## Worktrees

`bin/herdr-wt` is the one entry point for the desk popup, the `wt` shell alias, and `/new`. Do not switch those to native `herdr worktree create`.

Put it on `PATH` (`ln -sf …/drover/bin/herdr-wt ~/.local/bin/herdr-wt`). drover resolves it via `HERDR_WT`, then `PATH`, then `bin/herdr-wt` next to the binary’s checkout, then `~/.local/bin`. New branch names are used as-is. `HERDR_WORKTREE_PREFIX` adds a prefix when set. `REPOS_ROOT` (default `~/Developer`) is where *your* clones and `repo.branch` checkouts live — not the drover install (that is `~/.local/share/drover` from `install.sh`).

Native herdr worktree (0.9.3):

- no optional prefix of its own (`HERDR_WORKTREE_PREFIX` is what adds one)
- a name that exists only on origin becomes a new branch from HEAD, which is an empty copy of someone else's work
- no `#pr` fetch
- checkouts go under `~/.herdr/worktrees/<repo>/<branch>`, not `$REPOS_ROOT/<repo>.<branch>`
- remove deletes the checkout and never the merged branch

Blank native `new_worktree` so `prefix+shift+g` can run `herdr-wt prompt` (see `desk/herdr.toml`). `prefix+o` focuses the first blocked agent, else the first done one — the native toast target is gone with `ui.toast.delivery = "system"`.

## Event loop

Each machine gets a watcher. It opens herdr's Unix socket (`events.subscribe`) for the panes drover is following — `pane.agent_status_changed` per pane, plus `pane.closed`, `pane.exited`, and `workspace.closed`. Local connects to `~/.config/herdr/herdr.sock` (or `HERDR_SOCKET_PATH`). Remotes ssh to the machine and run a short Python relay onto that host's socket (needs `python3` there).

A push runs the same reconcile as before: `agent.list`, then reactions / dialog / transcript reply. `events_lost` or a pane-set change resubscribes. While a task is `Starting` / `Waiting` / `Ending`, `POLL_MS` (default 1s) re-checks the Claude transcript so a late flush is not missed.

```
Starting { prompt } ──ready──▶ Waiting ──reply──▶ Idle ──message──▶ Waiting
                                                    └──/done──▶ Ending ──entry──▶ (closed)
any ──pane gone──▶ Gone
```

If Discord sends a prompt while the agent is busy (`agent_not_ready` / `agent_not_idle`), drover keeps one message in `queued` and sends it on the next idle tick.

`Origin::Opened` means drover created the worktree, so `/done remove` may delete the checkout. `Attached` is an agent that was started at the desk.

## Memory

Sessions are throwaway. What carries over is small and curated.

| | |
|---|---|
| you | `~/.claude/CLAUDE.md` from the dotfiles, on every machine |
| project | `AGENTS.md` and Claude's auto memory, shared by every worktree of a repo. drover adds nothing |
| journal | `/done` asks for one five-line entry: what, why, outcome, decisions, next. Stored in `journal.db` (SQLite FTS5) |
| recall | `/new` appends the repo's three latest entries plus the best matches for the prompt, capped at 2 KB |

drover holds no conversation. It stores a map from thread to pane. Long tasks rely on the agent's own compaction.

## Machines

The host in `HOST_NAME` runs drover. Each name in `REMOTES` is a saved herdr machine reached with `herdr --machine <name>`, which talks to that machine's server and never starts it. `herdr --remote` attaches one session and leaves out the client's saved machines, so it is the wrong way to see both.

`HOST_NAME` is driven directly. `REMOTES` are driven over ssh, and the ssh name matches the herdr machine label. One host at a time. Two would both answer every message.

## Rules the code follows

Enforced by the lints in `Cargo.toml`:

- `unsafe` is forbidden. `unwrap`, `expect`, `panic!`, and slice indexing are denied. The one surviving `expect` carries an `// INVARIANT:` comment.
- Enums instead of flag combinations (`Phase`, `Origin`, `Teardown`, `Reach`, `Status`).
- Newtypes for ids (`PaneId`, `WorkspaceId`, `SessionId`, `Name`), validated once at the edge. herdr output is parsed straight into typed structs.
- `thiserror` in `herdr.rs` and `journal.rs`. `anyhow` only in `main.rs`.
- `state.json` from the TypeScript version still loads. There is a test for it.

Repo and branch names are checked (letters, digits, `# . _ / -`) and session ids must be uuids before they reach a shell. drover adds no sandbox. A Discord message is as good as typing in the pane.

## Development

```sh
cargo run
cargo test
cargo clippy --all-targets
```

`cargo run` needs a `.env`. Use a test server and channel. Startup refuses to launch if `herdr`, `git`, `jq`, or `ssh` is missing, or if `herdr-wt` cannot be found.

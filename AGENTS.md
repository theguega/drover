# drover

The user-facing guide is `README.md`. This file is how the program is put together.

## Layout

```
src/main.rs     discord: commands, threads, reactions, event watchers, prompt queue
src/socket.rs   herdr socket: requests and `events.subscribe` (direct locally, one ssh relay per remote)
src/herdr.rs    herdr calls per machine, herdr-wt and transcripts over sh / ssh
src/format.rs   agent markdown to discord: tables, rules, fences cut across messages
src/journal.rs  journal store, search, context for new tasks
install.sh      build + user service (systemd or launchd) from this checkout
state.json      thread → task map (gitignored)
journal.db      journal (gitignored)
```

drover has no model. It opens a worktree, starts the real `claude` (or `codex`, `cursor`, `opencode`, `pi`) TUI inside herdr, and posts the final answer. Every task is an ordinary herdr workspace, so the desk can take it over at any point.

```
phone ─ discord ─▶ drover  (Rust, a user service on the host)
                     │
                     ├─ herdr.sock + herdr-wt    $REPOS_ROOT/<repo>.<branch>
                     └─ ssh relay + ssh          the same on each remote
```

## Dotfiles

Personal tool, not a product. Everything about the desk lives in `~/.dotfiles` on every machine: `herdr-wt` (stowed to `~/.local/bin`), the herdr keys in `herdr/.config/herdr/config.toml`, the `wt` alias, and the env (`PATH`, `REPOS_ROOT`, `HERDR_WORKTREE_PREFIX`) in `.zshenv`. drover reads none of those itself. The service starts through `zsh -c`, and ssh runs the login shell, so every machine's own dotfiles decide.

## Worktrees

`herdr-wt` is the one entry point for the desk popup, the `wt` shell alias, and `/new`. Do not switch those to native `herdr worktree create`.

Native herdr worktree (0.9.3):

- no optional prefix of its own (`HERDR_WORKTREE_PREFIX` is what adds one)
- a name that exists only on origin becomes a new branch from HEAD, which is an empty copy of someone else's work
- no `#pr` fetch
- checkouts go under `~/.herdr/worktrees/<repo>/<branch>`, not `$REPOS_ROOT/<repo>.<branch>`
- remove deletes the checkout and never the merged branch

The dotfiles blank native `new_worktree` so `prefix+shift+g` can run `herdr-wt prompt`. `prefix+o` focuses the first blocked agent, else the first done one — the native toast target is gone with `ui.toast.delivery = "system"`.

## Event loop

herdr's socket takes one request per connection and closes it after the reply. Locally drover connects to `~/.config/herdr/herdr.sock` per request (~0.1 ms). Each remote keeps one ssh open to a short Python relay that opens a socket connection per request line and streams every reply line back, matched by `id` (~1 RTT, about 100 ms to chicken, against ~2 s for `herdr --machine`). The relay needs `python3` on the remote. Only git (`herdr-wt`) and remote transcript reads still spawn a shell.

Each machine gets a watcher holding one `events.subscribe` — `pane.agent_status_changed` per followed pane, plus `pane.agent_detected`, `pane.closed`, `pane.exited`, and `workspace.closed` anywhere.

A status push goes straight to the one task on that pane: reactions / dialog / transcript reply. Nothing polls. `agent.list` runs only after (re)subscribing, when an agent starts or leaves a followed pane, and when a workspace closes. `events_lost` or a pane-set change resubscribes, subscribing before that reconcile so no event falls between them. herdr 0.9.3 sends `pane.agent_status_changed` with dots and the other events with underscores (`pane_closed`), so names are matched both ways.

```
Starting { prompt } ──ready──▶ Waiting ──reply──▶ Idle ──message──▶ Waiting
                                                    └──/done──▶ Ending ──entry──▶ (closed)
any ──pane gone──▶ Gone
```

If Discord sends a prompt while the agent is busy (`agent_not_ready` / `agent_not_idle`), drover keeps one message in `queued` and sends it on the next idle push.

`Origin::Opened` means drover created the worktree, so `/done remove` may run `herdr-wt rm` on it, which keeps a dirty checkout and an unmerged branch. `Attached` is an agent that was started at the desk.

## Memory

Sessions are throwaway. What carries over is small and curated.

| | |
|---|---|
| you | `~/.claude/CLAUDE.md` or `~/.agents/AGENTS.md` from the dotfiles, on every machine |
| project | `AGENTS.md` shared by every worktree of a repo. drover adds nothing |
| journal | `/done` (unless `journal:false`) asks for one five-line entry: what, why, outcome, decisions, next. Stored in `journal.db` (SQLite FTS5) |
| recall | `/new` appends the repo's three latest entries plus the best matches for the prompt, capped at 2 KB |

drover holds no conversation. It stores a map from thread to pane. Long tasks rely on the agent's own compaction.

## Machines

The host in `HOST_NAME` runs drover. Each name in `REMOTES` is an ssh name whose herdr server is already running; drover never starts one. Match it to the saved herdr machine label so the desk and drover agree. One host at a time. Two would both answer every message.

## Rules the code follows

Enforced by the lints in `Cargo.toml`:

- `unsafe` is forbidden. `unwrap`, `expect`, `panic!`, and slice indexing are denied. The one surviving `expect` carries an `// INVARIANT:` comment.
- Enums instead of flag combinations (`Phase`, `Origin`, `Teardown`, `Reach`, `Status`).
- Newtypes for ids (`PaneId`, `WorkspaceId`, `SessionId`, `Name`), validated once at the edge. herdr output is parsed straight into typed structs.
- A typed `herdr::Error` (codes like `agent_blocked` are matched). `main.rs` uses a boxed `std::error::Error` with `bail!`, `err!` and `.context()`.
- Five dependencies: serenity, tokio, serde, serde_json, rusqlite. Prefer a few lines over a crate. serde stays: herdr, Discord and Claude transcripts all speak JSON.
- `state.json` from the TypeScript version still loads. There is a test for it.

`/usage` runs three short scripts on each machine and leaves out any agent that isn't logged in there (exit 3):

- claude: the login token (`~/.claude/.credentials.json`, or the macOS keychain) goes to curl on stdin for `api.anthropic.com/api/oauth/usage`, the endpoint behind Claude Code's own `/usage`
- cursor: the cursor-agent token (`~/.config/cursor/auth.json`, or the keychain) goes the same way to `DashboardService/GetCurrentPeriodUsage` on `api2.cursor.sh`, what the editor's usage view reads
- pi: no plan and no token. jq sums the tokens and cost pi logs per message in `~/.pi/agent/sessions`, over 24 h, 7 d and 30 d

The two endpoints are undocumented and may change without notice.

Repo and branch names are checked (letters, digits, `# . _ / -`) and session ids must be uuids before they reach a shell. drover adds no sandbox. A Discord message is as good as typing in the pane.

## Development

```sh
cargo run
cargo test
cargo clippy --all-targets
```

`cargo run` reads its config from the environment: `set -a; . ./.env; set +a` first, with a test server and channel. Startup refuses to launch if `herdr`, `herdr-wt`, `git`, `jq`, or `ssh` is missing from `PATH`.

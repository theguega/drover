# drover

Claude reads this when there is no `CLAUDE.md`. The user-facing guide is `README.md`. This file is how the program is put together.

## Layout

```
src/main.rs     discord: commands, threads, reactions, the poll loop
src/herdr.rs    herdr and ssh per machine, transcript replies
src/journal.rs  journal store, search, context for new tasks
bin/herdr-wt    worktree open/list/remove. The desk `wt` execs this file
state.json      thread → task map (gitignored)
journal.db      journal (gitignored)
```

drover has no model. It opens a worktree, starts the real `claude` (or `codex`, `cursor`, `opencode`, `pi`) TUI inside herdr, and posts the final answer. Every task is an ordinary herdr workspace, so the desk can take it over at any point.

```
phone ─ discord ─▶ drover  (Rust, a user service on the host)
                     │
                     ├─ bin/herdr-wt + herdr     ~/Developer/<repo>.<branch>
                     └─ ssh / herdr --machine    the same on each remote
```

## Worktrees

`bin/herdr-wt` is the one entry point for the <kbd>⌃b</kbd> <kbd>⇧g</kbd> popup, the `wt` shell alias, and `/new`. Do not switch those to native `herdr worktree create`.

Keep `bin/herdr-wt` identical to the copy stowed at `~/.local/bin/herdr-wt`. drover prefers `~/Developer/drover/bin/herdr-wt` and falls back to the stowed copy. New branch names are used as-is. `HERDR_WORKTREE_PREFIX` adds a prefix when it is set.

Native herdr worktree (0.9.3):

- no optional prefix of its own (`HERDR_WORKTREE_PREFIX` is what adds one)
- a name that exists only on origin becomes a new branch from HEAD, which is an empty copy of someone else's work
- no `#pr` fetch
- checkouts go under `~/.herdr/worktrees/<repo>/<branch>`, not `~/Developer/<repo>.<branch>`
- remove deletes the checkout and never the merged branch

The settings screen shows native "new worktree" as unset because `config.toml` sets `new_worktree = ""`. That frees `prefix+shift+g` for the popup that runs `bin/herdr-wt prompt`. The binding that works is the custom command, listed in <kbd>⌃b</kbd> <kbd>?</kbd>.

`prefix+o` is the same story. The native action only focuses the pane behind the current toast, and with `ui.toast.delivery = "system"` that target is gone by the time you are back at the keyboard. The custom command focuses the first blocked agent, otherwise the first done one, via `herdr agent focus`.

`prefix+v` and `prefix+l` are pane commands (`type = "pane"`, a temporary zoomed pane that closes when the program exits). The shift versions stay popups. Those chords used to be split-right and focus-right, which are now `prefix+percent` and `prefix+right`. A custom command on a default chord is disabled unless that default is moved or blanked.

## Poll loop

Every `POLL_MS`, drover lists the agents on each machine once and walks its tasks. A status change becomes a reaction. `blocked` posts the dialog. `idle` after a prompt reads the reply from the Claude transcript (`~/.claude/projects/*/<session>.jsonl`) and posts it. Other agents use the last 80 lines of the terminal. Each task has its own lock, so the poller and a command on the same thread take turns.

A task's phase is one enum in `state.json`:

```
Starting { prompt } ──ready──▶ Waiting ──reply──▶ Idle ──message──▶ Waiting
                                                    └──/done──▶ Ending ──entry──▶ (closed)
any ──pane gone──▶ Gone
```

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

`cargo run` needs a `.env`. Use a test server and channel. Startup refuses to launch if `herdr`, `git`, `jq`, or `ssh` is missing, or if `bin/herdr-wt` is not at `~/Developer/drover/bin/herdr-wt` or `~/.local/bin/herdr-wt`.

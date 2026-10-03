# drover

**The same herdr workspace, from the desk or from Discord.**

One branch → one worktree → one herdr workspace → one agent. At the desk you type into that agent. Away from it, a Discord thread types into the same pane and brings the answer back. drover has no model.

## Install

The dotfiles first, on every machine (`herdr-wt`, herdr keys, `.zshenv`). Then on the one host that runs drover:

```sh
git clone git@github.com:theguega/drover.git ~/Developer/drover
cd ~/Developer/drover && ./install.sh   # seeds .env and stops; fill it, rerun
```

`install.sh` builds a release and (re)starts a user service from this checkout: systemd on Linux, launchd on macOS. Update with `git pull && ./install.sh`.

Remotes need only the dotfiles, a running herdr, `python3`, and passwordless `ssh <name>`.

`.env`: `DISCORD_TOKEN`, `DISCORD_GUILD_ID`, `DISCORD_CHANNEL_ID`, `ALLOWED_USER_IDS`, `HOST_NAME`, `REMOTES` (comma-separated ssh names).

## Discord

| | |
|---|---|
| `/new machine repo branch prompt [agent] [scout]` | Worktree + agent. `branch` is a new name, an existing branch, or `#pr`. `scout` asks for a report and no changes. A new checkout gets the clone's untracked files matching `.worktreeinclude` |
| a message in the thread | Next prompt (queued if the agent is busy) |
| `/attach machine agent` | Follow an agent you started at the desk |
| `/screen` · `/keys` | Terminal view and key buttons |
| `/done [remove] [journal]` | Journal entry, close workspace; `journal:false` skips the entry; `remove` runs `herdr-wt rm` on a checkout drover opened: a dirty checkout and an unmerged branch are kept |
| `/recall` · `/agents` · `/worktrees` · `/usage` | Journal search, status, and Claude plan limits |

Default agent is `claude` (`codex`, `cursor`, `opencode`, `pi` also). State is one reaction: 👀 working · ⏸️ dialog · ✅ done.

## Develop

```sh
set -a; . ./.env; set +a; cargo run
cargo test && cargo clippy --all-targets
```

Internals are in [`AGENTS.md`](AGENTS.md).

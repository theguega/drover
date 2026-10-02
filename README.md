# drover

**The same herdr workspace, from the desk or from Discord.**

One branch → one worktree → one herdr workspace → one agent. At the desk you type into that agent. Away from it, a Discord thread types into the same pane and brings the answer back. Sit down later: the session is still there.

drover has no model. It opens the worktree, starts the agent you already pay for, and posts the final reply.

```
 Discord thread                         herdr workspace
 ┌──────────────────────┐              ┌─────────────────────────────┐
 │ /new  app  cache     │── drover ──▶ │ ~/Developer/app.cache       │
 │ "add a loader cache" │              │                             │
 │                      │◀──────────── │  claude                     │
 │ ✅  the answer       │              └─────────────────────────────┘
 └──────────────────────┘
```

## Why / non-goals

Work **with** agents, close to them — not a background farm.

| | |
|---|---|
| Need | herdr, so you see and organise panes on real machines |
| Look | review and git at the desk (`tuicr`, `lazygit`) |
| Steer | type into the same pane from Discord when you're away |
| Not | its own model, fleets, web UI, MCP server, or hosted runners |

Review and git stay at the desk. Discord is the remote keyboard for the same pane.

## Discord

| | |
|---|---|
| `/new machine repo branch prompt [agent]` | Worktree + agent. `branch` is a new name, an existing branch, or `#pr` |
| a message in the thread | Next prompt (queued if the agent is busy) |
| `/attach machine agent` | Follow an agent you started at the desk |
| `/screen` · `/keys` | Terminal view and key buttons |
| `/done [remove]` | Journal entry, close workspace; `remove` deletes a checkout drover opened |
| `/recall` · `/agents` · `/worktrees` | Journal search and status |

Default agent is `claude` (`codex`, `cursor`, `opencode`, `pi` also). State is one reaction: 👀 working · ⏸️ dialog · ✅ done.

## Desk

Put `bin/herdr-wt` on `PATH`, then merge [`desk/herdr.toml`](desk/herdr.toml) into your herdr config. Prefix `⌃b`:

| | |
|---|---|
| <kbd>⇧g</kbd> | New worktree (name, branch, or `#pr`) |
| <kbd>⇧o</kbd> / <kbd>⇧k</kbd> | Open / remove worktree |
| <kbd>o</kbd> | Jump to blocked, else done |
| <kbd>v</kbd> / <kbd>⇧v</kbd> | tuicr (pane / popup) |
| <kbd>l</kbd> / <kbd>⇧l</kbd> | lazygit (pane / popup) |

`wt` is the same helper the popup and `/new` use. Checkouts land next to the clone as `<REPOS_ROOT>/<repo>.<branch-slug>` (default `~/Developer`).

## Machines

`HOST_NAME` is where drover runs. `REMOTES` are other machines, each a saved herdr machine reached over ssh under the same name. Run one drover host. On the phone, use a Discord thread.

<details>
<summary><strong>Setup</strong></summary>

Needs Rust, plus `herdr` (≥ 0.9.3), `git`, `jq`, and `ssh` on `PATH`. Remotes also need `python3` (event stream relay). Prefer Homebrew herdr (`brew install herdr`); `herdr update` can shadow it in `~/.local/bin`.

```sh
git clone https://github.com/theguega/drover.git
cd drover
cp .env.example .env   # fill token, guild, channel, allowed user ids
cargo build --release
ln -sf "$PWD/bin/herdr-wt" ~/.local/bin/herdr-wt
```

Create the bot at [discord.com/developers](https://discord.com/developers/applications), enable **Message Content Intent**, invite with `bot` + `applications.commands`. Developer mode → copy server, channel, and user ids.

| Variable | Default |
|---|---|
| `DISCORD_TOKEN` · `DISCORD_GUILD_ID` · `DISCORD_CHANNEL_ID` · `ALLOWED_USER_IDS` | required |
| `HOST_NAME` | `host` |
| `REMOTES` | empty |
| `REPOS_ROOT` | `~/Developer` |
| `POLL_MS` | `1000` (transcript retry while waiting) |
| `HERDR_WT` | auto (PATH, then this repo’s `bin/`, then `~/.local/bin`) |

For each remote: `herdr machine add <name>`, passwordless `ssh <name>`, and `herdr-wt` on that machine’s `PATH`.

**Run** from the repo so `.env`, `state.json`, and `journal.db` sit next to the binary. One host only.

```sh
# Linux (linger keeps it after logout)
loginctl enable-linger
cp drover.service ~/.config/systemd/user/   # edit WorkingDirectory if the clone is not ~/Developer/drover
systemctl --user enable --now drover

# macOS — replace /Users/YOU in drover.plist first
cp drover.plist ~/Library/LaunchAgents/dev.drover.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.drover.plist
```

</details>

## Trust

Only `ALLOWED_USER_IDS` on a private server. Repo and branch names are checked before they reach a shell. A Discord message is a keystroke into the pane.

## Develop

```sh
cargo test && cargo clippy --all-targets
```

Internals (event loop, phases, journal) are in [`AGENTS.md`](AGENTS.md).

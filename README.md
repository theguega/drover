# drover

**The same herdr workspace, from the desk or from Discord.**

One branch is one worktree, one herdr workspace, and one fresh agent. At the desk you type into that agent. Away from it, a Discord thread types into the same pane and brings the answer back.

drover has no model of its own. It opens the worktree, starts the agent you already pay for, and posts the final reply.

```
 Discord thread                         herdr workspace
 ┌──────────────────────┐              ┌─────────────────────────────┐
 │ /new  app  cache     │── drover ──▶ │ ~/Developer/app.cache       │
 │ "add a loader cache" │              │                             │
 │                      │◀──────────── │  claude                     │
 │ ✅  the answer       │              └─────────────────────────────┘
 └──────────────────────┘
```

Sit down later, open that workspace, and the session is still there.

## Install

You need current stable Rust, plus `herdr`, `git`, `jq`, and `ssh` on `PATH`. Install herdr with Homebrew (`brew install herdr`). The `herdr update` command drops a second binary in `~/.local/bin` that will shadow the Homebrew one.

```sh
git clone https://github.com/theguega/drover.git ~/Developer/drover
cd ~/Developer/drover
cp .env.example .env
cargo build --release
```

drover checks for those four tools, and for `bin/herdr-wt`, before it connects.

Put the worktree helper on `PATH` if you want the `wt` command at the desk:

```sh
ln -sf ~/Developer/drover/bin/herdr-wt ~/.local/bin/herdr-wt
```

`tuicr` and `lazygit` are only for the desk review keys.

## Configure

`cp .env.example .env`. drover reads `.env` from its working directory. Real environment variables win.

| Variable | | Default |
|---|---|---|
| `DISCORD_TOKEN` | Bot token | required |
| `DISCORD_GUILD_ID` | Server where the slash commands are registered | required |
| `DISCORD_CHANNEL_ID` | Channel that holds the task threads | required |
| `ALLOWED_USER_IDS` | Comma-separated user ids. Everyone else is ignored | required |
| `HOST_NAME` | This machine, driven directly | `host` |
| `REMOTES` | Other machines, driven over ssh. Empty for a single machine | none |
| `POLL_MS` | How often herdr is polled | `4000` |

Create the bot at [discord.com/developers](https://discord.com/developers/applications), enable **Message Content Intent**, and invite it to a private server with the `bot` and `applications.commands` scopes. With developer mode on, copy the server id, the channel id, and your user id.

Repos live in `~/Developer`. Each remote in `REMOTES` must be a saved herdr machine, and ssh must reach it under that same name with no password prompt:

```sh
herdr machine add laptop --label laptop
ssh -o BatchMode=yes laptop true
herdr --machine laptop agent list
```

A saved machine never starts its server. If that server is stopped, tasks for it wait until `herdr` is opened there again.

## Run

The units run `target/release/drover` from the repo, so `.env`, `state.json`, and `journal.db` sit next to the code. Run one host. A second copy would answer the same messages.

Linux, as a user service. Linger keeps it up after logout.

```sh
loginctl enable-linger
cp drover.service ~/.config/systemd/user/
systemctl --user enable --now drover
journalctl --user -u drover -f
```

macOS, with launchd:

```sh
# Replace /Users/YOU in drover.plist, then:
cp drover.plist ~/Library/LaunchAgents/dev.drover.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.drover.plist
tail -f logs/drover.log
```

Update with:

```sh
git pull && cargo build --release
systemctl --user restart drover    # macOS: launchctl kickstart -k gui/$(id -u)/dev.drover
```

Open threads survive a restart. Their state is in `state.json`.

To move the host, stop the old one, copy `.env`, `state.json`, and `journal.db`, set `HOST_NAME` to the new machine and `REMOTES` to the others, then start. Keep it on the machine that stays awake. A sleeping remote only takes its own tasks offline.

## Discord

`/new` opens a worktree, starts the agent, and creates a thread named `repo · branch · machine`. The first prompt goes out once the agent is ready.

| Command | |
|---|---|
| `/new machine repo branch prompt [agent]` | Start a task. `branch` is a new name, an existing branch, or `#pr` |
| a message in the thread | The next prompt. Attachments are passed as URLs |
| `/attach machine agent` | Follow an agent you started at the desk |
| `/screen` | The last 30 lines of the terminal, with key buttons |
| `/keys keys` | Send keys, for example `esc` or `shift+tab enter`, then show the screen |
| `/done [remove]` | Write a journal entry, close the workspace, archive the thread. `remove` also deletes the checkout, only when drover opened it |
| `/recall query` | Search the journal |
| `/agents` | Every agent, its state, and its thread if it has one |
| `/worktrees` | Worktrees on every machine |

`/new` starts `claude` by default, or `codex`, `cursor`, `opencode`, or `pi`. Anything herdr can see can be followed with `/attach`.

Every terminal view has two rows of buttons:

```
[ 1 ] [ 2 ] [ 3 ] [ up ] [ down ]
[ enter ] [ esc ] [ tab ] [ ctrl+c ] [ refresh ]
```

When the agent stops on a dialog (folder trust, a permission prompt, a choice), drover posts the screen with these buttons. Send a message while that dialog is open and drover asks you to answer it first, then resend.

drover stays quiet. State is one reaction on your latest prompt, and the only text it posts is the answer.

| | |
|---|---|
| 👀 | Working |
| ⏸️ | A dialog is waiting |
| ✅ | Done. The answer is below |

Answers longer than 6000 characters arrive as `reply.md` with a preview. If the agent exits, the thread says so. `/attach` a running one, or `/new`.

Claude replies are read from the session transcript. Other agents reply with the last 80 lines of the terminal once they go idle. `/done` writes a journal entry for Claude. Other agents do not have a transcript reader yet.

## Desk

The popup, the shell, and `/new` all call `bin/herdr-wt`, so names and paths agree.

| | |
|---|---|
| `wt <name>` | New branch `<name>` from origin's default branch |
| `wt <branch>` | Existing local or remote branch |
| `wt #123` | That pull request |
| `wt ls` | Worktrees and their herdr state |
| `wt rm` | Remove this worktree, its workspace, and its branch if the branch is merged. `-f` if the tree is dirty |

The same input reopens the existing checkout. The directory is `~/Developer/<repo>.<branch-slug>`, next to the main clone. Set `HERDR_WORKTREE_PREFIX` when new names should sit under a prefix: with `dev`, `wt cache` creates `dev/cache`.

`wt ls` marks each worktree: ● working · ◆ blocked · ✓ done · ○ open · closed.

Herdr's built-in new-worktree action creates a branch from `HEAD` when that name exists only on origin, and it has no pull-request checkout. Bind `prefix+shift+g` to the script instead, and leave the native action unset:

```toml
[keys]
new_worktree = ""

[[keys.command]]
key = "prefix+shift+g"
type = "popup"
command = "exec \"$HOME/Developer/drover/bin/herdr-wt\" prompt"
width = 70
height = 8
description = "Worktree: new, branch or #pr"
```

A matching desk layout, prefix `⌃b`:

| | |
|---|---|
| <kbd>⇧g</kbd> | New worktree |
| <kbd>⇧o</kbd> | Open an existing worktree |
| <kbd>⇧k</kbd> | Remove the current worktree |
| <kbd>o</kbd> | Jump to the agent that is blocked, or the one that just finished |
| <kbd>a</kbd> / <kbd>⇧a</kbd> | Next / previous agent |
| <kbd>w</kbd> | Workspace picker |
| <kbd>v</kbd> / <kbd>⇧v</kbd> | tuicr, in a pane or a popup |
| <kbd>l</kbd> / <kbd>⇧l</kbd> | lazygit, in a pane or a popup |
| <kbd>%</kbd> | Split right |
| <kbd>→</kbd> | Focus the pane to the right |
| <kbd>-</kbd> | Split down |
| <kbd>q</kbd> | Detach. Everything keeps running |

## Machines

`HOST_NAME` is the machine drover runs on. `REMOTES` are the others, listed by the ssh name you also saved in herdr.

| Where you are | What you run |
|---|---|
| On the host | `herdr`, with each remote saved as a machine |
| On another machine | `herdr`, with the host saved as a machine |
| On the phone | A Discord thread |

`/new laptop …` is for work that needs that machine: UI, a simulator, its screen. When it sleeps, only those tasks go offline, and they resume when it wakes.

`herdr --remote <host>` attaches to one session and does not show your saved machines. Open `herdr` on the machine whose machine list you want to see.

## Journal

Sessions are throwaway. `/done` asks the agent for one five-line entry: what, why, outcome, decisions, next. drover stores it in `journal.db`. The next `/new` on that repo appends the three latest entries and the best matches for the prompt, capped at 2 KB.

Project instructions stay in `AGENTS.md` and in Claude's own memory. drover only writes the journal.

## Trust

Only `ALLOWED_USER_IDS` can use the bot, on a private server. Repo and branch names are checked before they reach a shell. Agents run with the permissions they already have at the desk. A Discord message is as good as typing in the pane.

## Development

```sh
cargo run      # needs a .env; use a test server and channel
cargo test
cargo clippy --all-targets
```

How the poll loop, task phases, and transcript reader fit together is in [`AGENTS.md`](AGENTS.md).

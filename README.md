# drover

**Your herdr workflow on every machine, from the desk or the phone.**

One branch is one worktree, one herdr workspace and one fresh agent session. At the desk you
drive it with herdr keys. Away from it, a Discord thread drives the same workspace.

drover has no model of its own. It opens worktrees, types into the real `claude` TUI running
inside herdr, and posts back the final answer. Every task stays an ordinary herdr workspace,
so you can take it over at the desk at any point and hand it back.

```
phone ─ discord ─▶ drover  (Rust, systemd on meerkat, no model)
                     │
                     ├─ herdr-wt + herdr          ▶ meerkat  ~/Developer/<repo>.<branch>   claude
                     └─ ssh / herdr --machine     ▶ chicken  ~/Developer/<repo>.<branch>   claude

thread  ⇄  worktree  ⇄  herdr pane  ⇄  interactive agent (your subscription)
```

- [The workflow](#the-workflow): keys, shell, recipes, a day
- [Discord](#discord): commands, buttons, reactions, agents
- [Memory](#memory): what carries over between tasks, and why context never overflows
- [Setup](#setup), [Run](#run), [Update](#update), [Move host](#move-host)
- [Development](#development)

---

## The workflow

### Keys

| | |
|---|---|
| **worktrees** | |
| <kbd>⌃b</kbd> <kbd>⇧g</kbd> | new name, existing branch, or `#pr` |
| <kbd>⌃b</kbd> <kbd>⇧o</kbd> | open an existing worktree |
| <kbd>⌃b</kbd> <kbd>⇧k</kbd> | remove the current worktree |
| **agents** | |
| <kbd>⌃b</kbd> <kbd>o</kbd> | jump to the agent that needs you |
| <kbd>⌃b</kbd> <kbd>a</kbd> / <kbd>⇧a</kbd> | next / previous agent |
| <kbd>⌃b</kbd> <kbd>w</kbd> | workspace picker |
| **review** | |
| <kbd>⌃b</kbd> <kbd>⇧v</kbd> | tuicr: review the diff, export comments |
| <kbd>⌃b</kbd> <kbd>⇧l</kbd> | lazygit |
| **layout** (herdr defaults) | |
| <kbd>⌃b</kbd> <kbd>v</kbd> / <kbd>-</kbd> | split right / down |
| <kbd>⌃b</kbd> <kbd>h</kbd><kbd>j</kbd><kbd>k</kbd><kbd>l</kbd> | move between panes |
| <kbd>⌃b</kbd> <kbd>z</kbd> / <kbd>b</kbd> | zoom pane / toggle sidebar |
| <kbd>⌃b</kbd> <kbd>q</kbd> | detach, everything keeps running |

### Shell

| | |
|---|---|
| `wt <x>` | same as <kbd>⌃b</kbd> <kbd>⇧g</kbd>: a new name, an existing branch, or `#pr` |
| `wt ls` | every worktree with its herdr state: ● working · ◆ blocked · ✓ done · ○ open · closed |
| `wt rm` | remove this worktree, its workspace, and its branch if merged. `-f` if dirty |

One script, `herdr-wt` (alias `wt`, from the dotfiles), sits behind the popup, the shell and
drover, so all three agree on names and paths. It replaces worktrunk and herdr's built-in
new-worktree key, which creates a fresh branch from main even when the branch exists on
origin, so you silently get an empty copy of someone's work.

### Recipes

1. **Start a task.** <kbd>⌃b</kbd> <kbd>⇧g</kbd> `loader-cache` creates `theo/loader-cache` from
   `origin/main` in its own workspace. Run `claude` there.
2. **Test someone's branch.** <kbd>⌃b</kbd> <kbd>⇧g</kbd> `fred/action_frequency_loss` or `#123`
   fetches it, tracks origin, and opens a workspace with nothing carried over. Run the tests or
   ask `claude` to review it against main, read the diff with <kbd>⌃b</kbd> <kbd>⇧v</kbd>, then
   <kbd>⌃b</kbd> <kbd>⇧k</kbd>.
3. **Again later.** The same input reopens the existing workspace instead of making a second one.
4. **Clean up.** `wt ls` shows what's lying around; `wt rm` inside a worktree removes it. It
   refuses uncommitted work and keeps unmerged branches.
5. **Phone, hands on.** SSH to meerkat over Tailscale and run `herdr`, which switches to its
   single-column mobile layout.
6. **Phone, async.** `/new` in Discord, then talk to the agent in its thread. See [Discord](#discord).

### A day

| | |
|---|---|
| **desk** | ghostty → herdr, one workspace per branch. Agents in panes, tuicr for reviews, zed only for reading. Desktop toasts when an agent finishes or needs you |
| **phone** | drover for async, SSH into herdr for hands-on. Start a task, answer a dialog, read the result. Every thread is also a live workspace at the desk |
| **meerkat** | the default target: always on, GPU, auto mode. Experiments and long runs go here. Also runs drover |
| **mac** | local and UI work, anything that needs this machine or its screen |
| **research** | the paperdb skill on every agent. Findings become an artifact or doc, and a journal line |

### Where things run

meerkat is home. Its herdr session holds the workspaces and agents, and drover runs next to
it. chicken, the laptop, is a window into that session, plus a machine of its own for Mac-only
work.

| where you are | what you run |
|---|---|
| at meerkat | `herdr`, the local session |
| on chicken | `herdr --remote meerkat`, the same session over SSH (or `ssh -t meerkat herdr`) |
| on the phone | drover threads, or SSH + `herdr` for hands-on |

- **Why not the laptop.** A closed lid stops every agent on it, and drover too if it lived
  there. meerkat stays up, has the GPU, and a task started from the laptop is still there at
  the desk.
- **chicken as a target.** `/new chicken …` is for what needs the Mac: UI, simulators, its
  screen. When chicken sleeps only those tasks go offline, and they resume when it wakes.
- **Offline.** On a plane you lose meerkat's agents; local work on chicken still runs on chicken.

---

## Discord

### Commands

| | |
|---|---|
| `/new machine repo branch prompt [agent]` | Open a worktree, start the agent, open a thread named `repo · branch · machine`. `branch` works like <kbd>⌃b</kbd> <kbd>⇧g</kbd>: a new name (becomes `theo/<name>`), an existing branch, or `#pr`. `repo` autocompletes from the main clones in `~/Developer`. The first prompt goes out once the agent is ready, with the journal context appended |
| *a message* | A plain message in a task thread is the next prompt. Attachments are passed along as URLs |
| `/attach machine agent` | Follow an agent you started at the desk. `agent` autocompletes from what herdr sees running |
| `/screen` | This thread's terminal, the last 30 lines, with key buttons |
| `/keys keys` | Send keys, space separated, e.g. `esc` or `shift+tab enter`, then show the screen |
| `/done [remove]` | The agent writes a journal entry, then the workspace closes and the thread archives. `remove` also deletes the checkout, only for worktrees drover opened |
| `/recall query` | Search the journal |
| `/agents` | Every agent on every machine, its state, and its thread if it has one |
| `/worktrees` | `wt ls` on every machine |

### Buttons

Every terminal view comes with two rows of keys:

```
[ 1 ] [ 2 ] [ 3 ] [ up ] [ down ]
[ enter ] [ esc ] [ tab ] [ ctrl+c ] [ refresh ]
```

When an agent stops on a dialog (folder trust, a permission prompt, a choice), drover posts the
screen with these buttons. Once the dialog is answered, that message disappears. A message sent
while a dialog is open is not delivered: drover asks you to answer the dialog first.

### Reactions

drover is quiet. State is one reaction on your latest prompt, and the only text it posts is
the agent's final answer.

| | |
|---|---|
| 👀 | working |
| ⏸️ | needs you: a dialog is waiting |
| ✅ | done, the answer is below |

Answers longer than 6000 characters arrive as `reply.md` with a preview. If the agent exits,
the thread says `agent exited`; `/attach` a running one or `/new`.

### Agents

`/new` starts `claude` (default), `codex`, `cursor`, `opencode` or `pi`, through herdr's own
`agent start --kind`. Anything herdr detects can be followed with `/attach`.

| | claude | others |
|---|---|---|
| reply | the final answer, read from the session transcript | the last 80 lines of the terminal, once the agent goes idle |
| `/done` journal | ✓ | not yet: needs a transcript reader per agent |

---

## Memory

Sessions are throwaway. What carries over is small and curated.

| | |
|---|---|
| **you** | `~/.claude/CLAUDE.md` from the dotfiles, on every machine. Kept under 2 KB |
| **project** | `AGENTS.md` and Claude's auto memory, shared by every worktree of a repo. Claude owns it; drover adds nothing |
| **journal** | `/done` asks for one five-line entry: what, why, outcome, decisions, next. drover stores it in `journal.db` (SQLite FTS5), so it covers every machine |
| **recall** | `/new` appends the repo's three latest entries plus the best matches for the prompt, capped at 2 KB. Read once at session start. `/recall` searches it from the phone |

**Why context never overflows**

- drover holds no conversation. It stores a map from thread to pane and nothing else.
- Every task starts a fresh session and is discarded when it ends. Long tasks rely on
  Claude's own compaction.
- What carries over is curated text with a hard cap, never raw history.

---

## Setup

**1. Discord.** Create an application at [discord.com/developers](https://discord.com/developers/applications),
add a bot, and enable **Message Content Intent**. Invite it to a private server with the `bot`
and `applications.commands` scopes (send messages, create threads, add reactions). With
developer mode on, copy the server id, the channel id and your user id.

**2. Config.** `cp .env.example .env` and fill it in. drover reads `.env` from its working
directory; real environment variables win.

| variable | | default |
|---|---|---|
| `DISCORD_TOKEN` | bot token | required |
| `DISCORD_GUILD_ID` | server, where the slash commands are registered | required |
| `DISCORD_CHANNEL_ID` | text channel that holds the task threads | required |
| `ALLOWED_USER_IDS` | comma-separated user ids. Everyone else is ignored | required |
| `HOST_NAME` | this machine, driven directly | `mac` |
| `REMOTES` | comma-separated other machines, driven over ssh | `meerkat` |
| `POLL_MS` | how often herdr is polled | `4000` |

**3. Machines.** Each machine needs herdr, the agent CLIs, and `herdr-wt` from the dotfiles
(`stow herdr`). Repos live in `~/Developer`. The host needs every remote saved as a herdr
machine and reachable by ssh under the same name, without a password prompt:

```sh
# on meerkat, the host: drover drives chicken through these
herdr machine add chicken --label chicken
ssh -o BatchMode=yes chicken true

# on chicken: herdr --machine meerkat … from the laptop
herdr machine add meerkat --label meerkat
```

**4. Build.** Current stable Rust (edition 2024).

```sh
cargo build --release      # → target/release/drover
```

## Run

Linux, systemd user unit (linger keeps it up after logout):

```sh
loginctl enable-linger
cp drover.service ~/.config/systemd/user/ && systemctl --user enable --now drover
journalctl --user -u drover -f
```

macOS, launchd:

```sh
cp drover.plist ~/Library/LaunchAgents/dev.theo.drover.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.theo.drover.plist
tail -f logs/drover.log
```

Both units run `target/release/drover` from the repo directory, so `.env`, `state.json` and
`journal.db` sit next to the code. `PATH` in the unit must reach `herdr`, `ssh` and
`~/.local/bin/herdr-wt`.

## Update

```sh
git pull && cargo build --release
systemctl --user restart drover    # macOS: launchctl kickstart -k gui/$(id -u)/dev.theo.drover
```

Open threads survive a restart: their state is in `state.json`, and the poller picks them up
where it left off. If a unit file changed, copy it again and `systemctl --user daemon-reload`
first.

## Move host

Run one host at a time; two would both answer every message. Stop the old one, copy `.env`,
`state.json` and `journal.db`, set `HOST_NAME` to the new host and `REMOTES` to the others,
then start. Run it on the machine that stays awake: a sleeping remote only takes its own tasks
offline, and they resume when it wakes.

## Trust

- Only `ALLOWED_USER_IDS` can use the bot, on a private server. Anyone else gets `not allowed`
  or is ignored.
- Repo and branch names are checked (letters, digits, `# . _ / -`) and session ids must be
  uuids before they reach a shell.
- Agents run with whatever permissions they have at the desk. drover adds no sandbox; a
  Discord message is as good as typing in the pane.

---

## Development

```sh
cargo run                   # needs a .env; use a test server and channel
cargo test
cargo clippy --all-targets
```

```
src/main.rs     discord: commands, threads, reactions, the poll loop
src/herdr.rs    herdr and ssh per machine, transcript replies
src/journal.rs  journal store, search, context for new tasks
state.json      thread → task map (gitignored)
journal.db      journal (gitignored)
```

**How it works.** Every `POLL_MS`, drover lists the agents on each machine once and walks its
tasks. A status change becomes a reaction; `blocked` posts the dialog; `idle` after a prompt
reads the reply from the transcript (`~/.claude/projects/*/<session>.jsonl`) and posts it. Each
task has its own lock, so the poller and a command on the same thread take turns while other
threads go on.

**A task's phase** is one enum, stored in `state.json`:

```
Starting { prompt } ──ready──▶ Waiting ──reply──▶ Idle ──message──▶ Waiting
                                                    └──/done──▶ Ending ──entry──▶ (closed)
any ──pane gone──▶ Gone
```

**Rules** the code follows, enforced by lints in `Cargo.toml`:

- `unsafe` is forbidden; `unwrap`, `expect`, `panic!` and slice indexing are denied. The one
  surviving `expect` carries an `// INVARIANT:` comment.
- Enums instead of flag combinations (`Phase`, `Origin`, `Teardown`, `Reach`, `Status`).
- Newtypes for ids (`PaneId`, `WorkspaceId`, `SessionId`, `Name`), validated once at the edge.
  herdr output is parsed straight into typed structs.
- `thiserror` in `herdr.rs` and `journal.rs`; `anyhow` only in `main.rs`.
- `state.json` from the TypeScript version still loads; there is a test for it.

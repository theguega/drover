# drover

Drive [herdr](https://herdr.dev) agents from Discord. One thread is one worktree, one herdr
pane and one fresh agent session, on any of your machines.

drover has no model of its own. It opens worktrees, types into the real `claude` TUI running
in herdr, and posts back the final answer. Every task is also a normal herdr workspace, so
you can take it over at the desk at any point.

```
phone ─ discord ─▶ drover (meerkat, systemd)
                     ├─ herdr-wt + herdr          ▶ meerkat  ~/Developer/<repo>.<branch>  claude
                     └─ ssh / herdr --machine     ▶ chicken  ~/Developer/<repo>.<branch>  claude
```

## commands

| | |
|---|---|
| `/new machine repo branch prompt` | open a worktree, start the agent, open a thread. `branch` is a new name (becomes `theo/<name>`), an existing branch, or `#pr` |
| message | a plain message in a task thread is the next prompt |
| `/attach machine agent` | follow an agent you started at the desk |
| `/screen`, `/keys` | see the terminal, send keys. Dialogs show up on their own, with key buttons |
| `/done [remove]` | the agent writes a journal entry, then the workspace closes. `remove` also deletes the checkout |
| `/recall query` | search the journal |
| `/agents`, `/worktrees` | everything running, every worktree, on every machine |

State is one reaction on your latest prompt: 👀 working, ⏸️ needs you, ✅ done. The only text
drover posts is the agent's final answer. Long answers come as a file.

## memory

Sessions are throwaway; what carries over is small and curated.

- **you**: `~/.claude/CLAUDE.md` from the dotfiles, on every machine.
- **project**: `AGENTS.md` and Claude's auto memory, shared by all worktrees of a repo.
- **journal**: `/done` asks for a five-line entry (what, why, outcome, decisions, next),
  stored in `journal.db` (SQLite FTS5). It covers every machine because drover stores it.
- **recall**: `/new` adds the repo's latest entries and matches for the prompt to the first
  prompt, capped at 2 KB. Read once at session start, so context never grows from it.

## setup

1. Discord: create an application at discord.com/developers, add a bot, enable
   **Message Content Intent**, invite it to a private server with `bot` and
   `applications.commands` (send messages, threads, reactions). With developer mode on,
   copy the server, channel and your user id.
2. `cp .env.example .env` and fill it in. Only `ALLOWED_USER_IDS` can use the bot.
3. `cargo build --release`. Reads `.env` from the working directory.

Each machine needs herdr, `claude`, and `herdr-wt` from the dotfiles (`stow herdr`).
The host needs every remote saved as a herdr machine and reachable by ssh under the same
name (`herdr machine add chicken --label chicken`).

## run

Linux, systemd user unit (linger keeps it up after logout):

    loginctl enable-linger
    cp drover.service ~/.config/systemd/user/ && systemctl --user enable --now drover
    journalctl --user -u drover -f

macOS, launchd:

    cp drover.plist ~/Library/LaunchAgents/dev.theo.drover.plist
    launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.theo.drover.plist
    tail -f logs/drover.log

## move host

Run one host at a time; two would both answer every message. Stop the old one, copy `.env`,
`state.json` and `journal.db`, set `HOST_NAME` to the new host and `REMOTES` to the others,
then start. Run it on the machine that stays awake: a sleeping remote only takes its own
tasks offline.

## files

```
src/main.rs     discord: commands, threads, reactions, the poll loop
src/herdr.rs    herdr and ssh per machine, transcript replies
src/journal.rs  journal store, search, context for new tasks
state.json      thread → pane map (gitignored)
journal.db      journal (gitignored)
```

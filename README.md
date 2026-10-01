# drover

Discord bridge for herdr. One thread is one worktree, one pane, one fresh agent session.

    /new      machine repo branch prompt   branch: new name, existing branch, or #pr
    message   next prompt for the thread's agent
    /attach   follow an agent started at the desk
    /screen   terminal with key buttons; dialogs show up on their own
    /keys     send keys
    /done     journal entry, then close (remove: delete the checkout)
    /recall   search the journal
    /agents   everything running
    /worktrees  every worktree, same as `wt ls`

State is a reaction on your latest prompt: 👀 working, ⏸️ needs you, ✅ done.
New tasks get up to 2 KB of related journal entries with their first prompt.

## run

    cp .env.example .env    # token, guild, channel, your user id, HOST_NAME, REMOTES
    bun install

macOS (launchd):

    cp drover.plist ~/Library/LaunchAgents/dev.theo.drover.plist
    launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.theo.drover.plist

Linux (systemd user unit, survives logout with linger):

    loginctl enable-linger
    cp drover.service ~/.config/systemd/user/ && systemctl --user enable --now drover

Logs: `logs/drover.log` or `journalctl --user -u drover`.

## move host

Run one host at a time, since two would answer every message. Stop the old one, copy
`.env`, `state.json` and `journal.db`, set `HOST_NAME` to the new host and `REMOTES` to the
others, then start. Every remote must be a saved herdr machine and an ssh host of the same name.

Worktrees go through `herdr-wt` from the dotfiles, the same script as `⌃b ⇧g`.

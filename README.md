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

    cp .env.example .env    # token, guild, channel, your user id
    bun install
    cp drover.plist ~/Library/LaunchAgents/dev.theo.drover.plist
    launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.theo.drover.plist

Logs: `logs/drover.log`. Restart: `launchctl kickstart -k gui/$(id -u)/dev.theo.drover`.

Worktrees go through `herdr-wt` from the dotfiles, the same script as `⌃b ⇧g`.
Remotes are saved herdr machines reachable over ssh under the same name.

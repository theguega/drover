# drover

**An agent runtime for herdr.** drover starts coding agents in git worktrees, follows them in herdr, and carries their answers to Discord or your terminal. Every task is an ordinary herdr workspace, so you can take over any agent at any time.

drover calls no model itself. The agents are the CLIs you already use.

## What you get

- **Tasks:** `drover new` opens a worktree and starts an agent in herdr. Pass `--discord` (or `drover attach --pane ID --discord` later) to open a Discord thread where messages become the next prompts.
- **Terminal and Discord are the same thing.** Every Discord command has a terminal form, and both act on the same tasks.
- **`drover-wt`:** worktrees for herdr, from a branch, a new name, or a GitHub pull request (`#123`).
- **A journal:** `drover done` asks the agent for a five-line entry, and `drover recall` searches them later.
- **Remotes:** drive herdr servers on other machines over ssh from the same desk.
- **A skill:** `skills/drover` teaches agents to use all of this. See [Skill](#skill).

## Install

You need herdr, git, jq, ssh, zsh and Rust (for `cargo`). On the machine that runs drover:

```sh
curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh
```

It clones drover to `~/.local/share/drover`, asks for the Discord settings (below) and where your repositories live, builds it, puts `drover` and `drover-wt` on your PATH, and starts the service (systemd on Linux, launchd on macOS). Run the same command again to update.

On each remote machine, install only the worktree tool and the client:

```sh
curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh -s -- --remote
```

The `drover` on a remote machine is a client: it runs the command on the host in `DROVER_HOST` over ssh. Set it in your shell, for example `export DROVER_HOST=meerkat`.

## Discord setup

1. Create an application in the Discord developer portal, add a bot, and copy its token.
2. Invite the bot to your server with permission to read and send messages, create threads, and add reactions.
3. Copy the server (guild) id, the text channel where drover posts, and your own user id. Discord's developer mode shows these under "Copy ID".

The installer asks for the token, the guild id, the channel id, the allowed user ids (comma-separated; only these can drive agents), this machine's name, and the remotes (ssh names, comma-separated). It writes `.env` in the install directory. `drover config` shows the live values.

## Use

```sh
drover help                  # every command, with its Discord equivalent
drover new --machine host --repo myrepo --branch feature-x --prompt "add retries to the client"
drover agents                # agents on every machine, and the thread if bound to Discord
drover screen --pane ID      # an agent's terminal
drover keys --pane ID --keys "esc enter"
drover attach --pane ID --discord  # open a Discord thread for a running task to continue there
drover done --pane ID        # journal entry, then close the workspace
drover recall retries        # search the journal
drover journal rm 12         # delete an entry; /journal-rm in Discord picks one from a list
```

`drover-wt` also works on its own, at the desk or in a shell:

```sh
drover-wt my-branch          # new or existing branch
drover-wt '#123'             # a pull request's head
drover-wt ls                 # worktrees of this repo, or of every repo under REPOS_ROOT
drover-wt rm                 # remove this worktree, and its branch if merged
drover-wt --help             # full usage
```

A new checkout is ready to work in. Untracked files of the main clone that match `.worktreeinclude` (gitignore syntax, e.g. `.env`, `target/`, `node_modules/`) are copied over, whole directories as copy-on-write clones where the filesystem supports it (APFS, Btrfs, XFS). Then an executable `.worktree-setup` in the repo runs in the new checkout, with the main clone's path as `$1`, for steps like `uv sync` or `direnv allow`.

## Mates and scouts

A **mate** is a dispatcher: an agent in its own pane in the `drover` workspace, with its playbook in `mates/AGENTS.md`. It plans the goal as a small graph of crewmates, starts them with `drover new --mate`, hears each crewmate's turn as it ends, and loops until every check passes. It writes to you only when it needs a decision or the goal is done. At most three run at once.

```sh
drover mate reviewer         # start a mate; /mate in Discord does the same
drover new --machine host --repo myrepo --branch feature-x --prompt "..." --mate reviewer
drover take --thread ID      # the desk user takes a pane back from its mate
```

A **scout** is a short look at something that belongs to no repo. It has a folder of its own, no journal entry, and it is deleted when you close it:

```sh
drover scout "what does the cache layer do?"
drover scout close --thread ID
```

`mates/state/` holds each mate's log and owner list, and scouts' folders. It is git-ignored.

## Skill

`skills/drover` is an [Agent Skills](https://agentskills.io) folder: a `SKILL.md` with a name and a description, and nothing else. It documents this CLI for agents, and it points at `drover help` for the full reference, so it doesn't go stale.

To use it, copy the folder into the skills directory of your agent, the same way you'd install any skill:

```sh
cp -r skills/drover ~/.agents/skills/
```

Keep your copy in your own dotfiles if you want it on every machine. The installer doesn't touch the skills directory.

## Completion

Completion is opt-in: the installer doesn't set it up. Values such as machines, mates, threads and panes come from `drover complete`, so the scripts work with a running service.

Bash, with the `bash-completion` package:

```sh
mkdir -p ~/.local/share/bash-completion/completions
cp completions/drover.bash ~/.local/share/bash-completion/completions/drover
```

zsh, in any directory on `fpath`:

```sh
mkdir -p ~/.zsh/completions
cp completions/_drover ~/.zsh/completions/
```

Then add these lines to `.zshrc` before `compinit`:

```sh
fpath=(~/.zsh/completions $fpath)
```

## Configuration

drover reads only its own `.env`. Your desk is yours: keybindings, shell aliases, `REPOS_ROOT` and `HERDR_WORKTREE_PREFIX` belong in your dotfiles, and they point at `drover-wt`.

The installer asks where repositories and worktrees live. The default is `~/.drover-workspace`, and the answer goes into `.env` as `REPOS_ROOT`. Clone your repositories under it. Worktrees are created next to their main clone as `<repo>.<branch>`.

If your repositories already live somewhere else, such as `~/Developer`, answer that at the prompt, or set `REPOS_ROOT` in your shell environment for the desk and remotes.

## Develop

```sh
cargo test && cargo clippy --all-targets
```

The design, with what is built and what is planned, is in [`DESIGN.md`](DESIGN.md). The code's rules and layout are in [`AGENTS.md`](AGENTS.md).

## License

MIT. See [`LICENSE`](LICENSE).

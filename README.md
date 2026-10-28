# drover

**An agent runtime for herdr.** drover starts coding agents in git worktrees, on this machine or over ssh, and follows each one as a **run**. A pane run is an ordinary herdr workspace, so you can take over any agent at any time.

drover calls no model itself. The agents are the CLIs you already use.

## What you get

- **Runs:** `drover run` opens a worktree and starts the agent in a herdr pane, or `--headless` over its stream. drover follows it through the agent's own hooks and stream, and keeps its phase, owner, usage and last reply in `~/.local/state/drover/drover.db`.
- **Approvals:** a tool call that needs a permission waits for `drover approve` or `drover deny --reason`, in both modes.
- **Nothing runs without you:** a run another agent asks for (`--parent`) waits as `pending` until you `drover approve-run` it.
- **Mates:** a mate is a run that follows the `mates` skill: it plans a goal graph, requests crew, hears them through `drover wait`, approves their tool calls, and asks you only through `drover escalate`.
- **`drover-wt`:** worktrees for herdr, from a branch, a new name, or a GitHub pull request (`#123`).
- **Remotes:** drive herdr servers on other machines over ssh from the same desk.
- **Skills:** `skills/drover` teaches agents to use all of this, `skills/mates` is the dispatch playbook for a mate. See [Skill](#skill).

The design, with what is built and what is still planned, is in [`DESIGN.md`](DESIGN.md).

## Install

You need herdr, git, jq, ssh, zsh and Rust (for `cargo`). On the machine that runs drover:

```sh
curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh
```

It clones drover to `~/.local/share/drover`, writes `~/.config/drover/config.toml` from a few questions, builds it, puts `drover` and `drover-wt` on your PATH, and starts the service (systemd on Linux, launchd on macOS). Run the same command again to update.

On each remote machine, install only the worktree tool and the client:

```sh
curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh -s -- --remote
```

The `drover` on a remote machine is a client: it runs the command on the host in `DROVER_HOST` over ssh. Set it in your shell, for example `export DROVER_HOST=amazing_vps`.

## Use

```sh
drover                       # runs per phase, and what needs you
drover run --repo myrepo --branch feature-x --prompt "add retries to the client"
drover ls                    # live runs; --all for done and failed too
drover show r-1a2b3c         # phase, owner, last reply, children
drover send r-1a2b3c "now the tests"
drover screen r-1a2b3c       # the pane's terminal
drover keys r-1a2b3c esc enter
drover run --repo myrepo --branch fix-y --headless --model haiku --prompt "…"   # no pane; warm for the next send
drover approve r-1a2b3c p1   # allow the tool call it waits on; deny r-1a2b3c p1 --reason "…"
drover attach r-1a2b3c       # a headless run reopens in a pane, as yours
drover approve-run r-4d5e6f  # start a run an agent asked for; drop-run removes it
drover done r-1a2b3c --remove --entry "what, why, outcome"   # brain log, close, remove the worktree
drover run --role mate --repo myrepo --branch mate-x --headless --prompt "the goal"   # a mate, see Mates
drover send r-7a8b9c "the answer"   # to a mate that escalated: `drover` and `ls --phase needs` show its question
drover snapshot              # open runs as JSON (--all for every run)
drover watch --run r-1a2b3c  # its events as JSON lines; the same stream is GET http://127.0.0.1:7341/events
```

`--help` on any command documents its flags.

`drover-wt` also works on its own, at the desk or in a shell:

```sh
drover-wt my-branch          # new or existing branch
drover-wt '#123'             # a pull request's head
drover-wt ls                 # worktrees of this repo, or of every repo under REPOS_ROOT
drover-wt rm                 # remove this worktree, and its branch if merged
drover-wt --help             # full usage
```

A new checkout is ready to work in. Untracked files of the main clone that match `.worktreeinclude` (gitignore syntax, e.g. `.env`, `target/`, `node_modules/`) are copied over, whole directories as copy-on-write clones where the filesystem supports it (APFS, Btrfs, XFS). Then an executable `.worktree-setup` in the repo runs in the new checkout, with the main clone's path as `$1`, for steps like `uv sync` or `direnv allow`.

## Mates

A mate dispatches crew instead of writing code. You start it as a run with its role and a goal:

```sh
drover run --role mate --repo myrepo --branch mate-x --headless --model sonnet --prompt "ship retries in the client, docs included"
```

The mate follows `skills/mates`, which must be installed for its agent (see [Skill](#skill)), and runs `drover` commands from its Bash tool with `DROVER_RUN` set to its id: allow `Bash(drover:*)` in that agent's permissions, or each call asks you. It writes a goal graph (`drover goal set --file goal.json`: nodes with a check each, edges, and per-node status, round, crew and review), and starts crew with `drover run --parent $DROVER_RUN …`. Those wait in `pending` until you `drover approve-run` them. It hears them with `drover wait --any --parent $DROVER_RUN`, and answers their tool calls itself: a crew run's approvals go to its mate, not to you. The graph is stored with the mate's run (`drover show <mate>`) and streamed as `goal` events.

The mate speaks to you only with `drover escalate`. The question shows in `drover` and in `drover ls --phase needs`, and the mate stays in `needs` until you answer with `drover send <mate> <text>`. `drover attach <run>` takes any run over.

## Skill

`skills/drover` and `skills/mates` are [Agent Skills](https://agentskills.io) folders: a `SKILL.md` with a name and a description, and nothing else. `drover` documents this CLI for agents, and points at `drover help` for the full reference, so it doesn't go stale. `mates` is the mate's dispatch playbook.

To use them, copy the folders into the skills directory of your agent, the same way you'd install any skill:

```sh
cp -r skills/drover skills/mates ~/.agents/skills/
```

Keep your copy in your own dotfiles if you want it on every machine. The installer doesn't touch the skills directory.

## Session context

Claude Code can run `drover` at the start of every session, so the desk's home view is in the context from the first message. It is opt-in. Add this to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "drover" }] }
    ]
  }
}
```

Its output is added to every session's context, so each session pays for those tokens. The [skill](#skill) is the lighter choice: it loads only when a task mentions drover.

## Configuration

`~/.config/drover/config.toml`, read once when the service starts. Every key is optional:

```toml
host = "meerkat"                        # this machine's name, as herdr's machine label (default "host")
remotes = ["chicken"]                   # ssh names whose herdr server already runs
repos_root = "~/.drover-workspace"      # REPOS_ROOT for drover-wt on this machine
v1_state = "~/Developer/drover/state.json"   # a v1 state.json, migrated into a new drover.db once
```

The installer writes it. drover v1's `.env` is not read: copy `HOST_NAME`, `REMOTES` and `REPOS_ROOT` into this file by hand, and drop the Discord keys. `--config` or `DROVER_CONFIG` names another file, and `--state-dir` or `DROVER_STATE_DIR` another state directory, for a test service next to the real one.

Your desk is yours: keybindings, shell aliases, `REPOS_ROOT` and `HERDR_WORKTREE_PREFIX` belong in your dotfiles, and they point at `drover-wt`. Clone your repositories under the repos root. Worktrees are created next to their main clone as `<repo>.<branch>`.

## Develop

```sh
cargo test && cargo clippy --all-targets
scripts/live-test.sh         # headless, approvals, pane and attach, against a real claude (haiku) and herdr
scripts/live-test-mates.sh   # a mate's crew, wait, goal and escalate, the same way
```

The design, with what is built and what is planned, is in [`DESIGN.md`](DESIGN.md). The code's rules and layout are in [`AGENTS.md`](AGENTS.md).

## License

MIT. See [`LICENSE`](LICENSE).

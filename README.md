# drover

drover is an agent runtime with no model of its own. It starts the coding-agent CLIs you already use (Claude Code, Cursor and pi) in git worktrees, either in [herdr](https://herdr.dev) panes you can take over at any time or headless over their streams, on this machine or on other machines over ssh. It follows each agent session as a **run**, sends the tool calls that need a permission to you, and lets a **mate** (an agent that dispatches other agents) plan a goal and request crew, which start only after you approve them. It is for people who already work with several coding agents at once in the terminal and want one place to start, watch, approve and close them.

## A first session

```console
$ drover run --repo myapp --branch add-notes --headless --model haiku \
    --prompt "Create NOTES.md with the Bash tool: touch NOTES.md. Then reply in one short sentence."
run: r-75fedc
machine: devbox
phase: working
mode: headless
cwd: /home/you/.drover-workspace/myapp.add-notes
owner: user

$ drover                       # the home view: runs per phase, and what needs you
runs:
  pending: 0
  starting: 0
  working: 0
  needs: 1
  idle: 0
needs[1]{run,machine,role,phase,harness,mode,cwd}:
  r-75fedc,devbox,task,needs,claude,headless,/home/you/.drover-workspace/myapp.add-notes

$ drover show r-75fedc         # trimmed
phase: needs
harness: claude
session: bcc4791f-8856-483d-ad0b-f83603c10d32
perms[1]{perm,tool,input,state}:
  p1,Bash,"{\"command\":\"touch NOTES.md\"}",waiting

$ drover approve r-75fedc p1   # or: drover deny r-75fedc p1 --reason "not now"
run: r-75fedc
perm: p1
allowed: true

$ drover show r-75fedc         # trimmed
phase: idle
warm: true
usage:
  cost_usd: 0.00474064
reply:
  text: Created an empty `NOTES.md` in the worktree.

$ drover send r-75fedc "Now add a line about the release process"
$ drover attach r-75fedc       # take it over: the headless run reopens in a herdr pane, as yours
$ drover done r-75fedc --remove
run: r-75fedc
phase: done
removed: false
```

Without `--headless`, a run starts in a herdr pane, which is the default. Output is [TOON](https://github.com/toon-format/toon): `key: value` lines, and lists as `name[count]{fields}:` with one row per record. Each command also prints `help:` lines with the commands that usually come next (trimmed above). `done --remove` removes the worktree only when it is clean and merged.

## Requirements

- Linux or macOS. The service runs as a systemd user unit or a launchd agent.
- [herdr](https://herdr.dev), installed and with its server running, on every machine that runs agents.
- git, jq, ssh and zsh on the host. The service starts through zsh.
- Rust 1.91 or newer (`cargo`) on the host, to build drover.
- On each remote: git, jq, herdr and python3. The host's ssh key must reach the remote without a prompt.
- The agent CLIs you want, on each machine that runs them:
  - Claude Code: `claude`, logged in.
  - Cursor: `cursor-agent`, logged in.
  - pi: `pi` and [`pi-acp`](https://www.npmjs.com/package/pi-acp) (`npm i -g pi-acp`).

## Install

On the machine that runs the service (the host):

```sh
curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh
```

This clones drover into `~/.local/share/drover` (or `$DROVER_HOME`) and runs its `install.sh`. You can also clone the repository yourself and run `./install.sh`. On the host, `install.sh`:

1. asks for this machine's name, its remotes and the repos root, and writes `~/.config/drover/config.toml` when that file does not exist yet;
2. builds drover with `cargo build --release`;
3. links `drover` and `drover-wt` into `~/.local/bin`;
4. installs and starts the service: `~/.config/systemd/user/drover.service` on Linux (with `loginctl enable-linger`), or `~/Library/LaunchAgents/dev.drover.plist` on macOS.

Run it again to update. On each remote, set up only the worktree tool and the client:

```sh
curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh -s -- --remote
```

`--remote` links `drover-wt`, and links `bin/drover-ssh` as `drover`: a client that runs each command on the host over ssh. Set `DROVER_HOST` to the host's ssh name in your shell (`export DROVER_HOST=devbox`). On macOS it also prints the one-time Cursor login command (see [Harnesses](#harnesses)).

The service runs `zsh -lc`, and ssh runs commands through your remote shell without a terminal. So the `PATH` that finds `herdr`, `jq`, `drover-wt` and the agent CLIs must be set in a file a non-interactive shell reads (`~/.zshenv` or `~/.zprofile`), not only in `~/.zshrc`. The same goes for `~/.local/bin` on the host, where the client runs `drover`. If a tool is missing, the service does not start.

To check that it runs:

```sh
drover                                   # the home view; "drover is not running" means the service is down
systemctl --user status drover           # Linux; logs: journalctl --user -u drover -f
tail -f ~/.local/share/drover/drover.log # macOS: drover.log in the checkout
```

Clone your repositories under the repos root (`~/.drover-workspace` by default), on each machine. A run's worktree goes next to its main clone, as `<repo>.<branch>`.

## Configuration

The service reads `~/.config/drover/config.toml` once when it starts. Every key is optional, and a missing file means this machine alone. The file is a flat subset of TOML: strings, lists of strings and integers only, with `#` comments (so a string value cannot contain `#`). A key that drover does not know is ignored with a warning.

```toml
host = "devbox"                       # this machine's name, as herdr's machine label (default "host")
remotes = ["gpu", "mac"]              # ssh names whose herdr server already runs (default none)
repos_root = "~/.drover-workspace"    # where repositories and worktrees live here (default: $REPOS_ROOT, else ~/.drover-workspace)
remote_repos_root = "/srv/repos"      # the same on every remote, an absolute path there (default: each remote's own REPOS_ROOT)
http_port = 7341                      # hooks, events and approvals, on 127.0.0.1 only
hook_timeout = 600                    # seconds a Claude tool call waits for your approve or deny
idle_kill = 900                       # seconds a headless process stays warm after its last turn
background_kill = 9000                # the same while it runs background tasks (default 10 × idle_kill)
journal_cmd = "brain log"             # done --entry runs this through sh, the entry on stdin (default none: --entry is refused)
reader_allow = ["brain find *"]       # Bash commands a reader run may run, as Claude permission rules (default none)
```

`journal_cmd` and `reader_allow` connect drover to a notes tool of your own. In this example, `brain` is a personal knowledge CLI: `drover done <run> --entry "what, why, outcome"` appends the entry with `brain log`, and a read-only run may search with `brain find`. drover stores no knowledge itself.

`--config` or `DROVER_CONFIG` selects another config file. `--state-dir` or `DROVER_STATE_DIR` selects another state directory (default `~/.local/state/drover`, which holds `drover.db` and `drover.sock`). Use both to run a test service next to the real one, with its own `http_port`.

## Concepts

### Runs

A run is one agent session in one directory, with an id such as `r-75fedc`.

- **Mode.** A *pane* run is the agent's TUI in a herdr workspace. drover follows it through the agent's hooks and never reads the screen. You can type into it, and `screen` and `keys` reach it from the CLI. A *headless* run is the agent's streaming interface. The process stays warm between turns, is stopped after `idle_kill`, and resumes its session on the next `send`.
- **Role.** `task` (the default) is a run you start. `mate` is a dispatcher (see [Mates and crew](#mates-and-crew)). `crew` is a run that a mate requested. `chat` is for read-only conversations.
- **Profile.** `actor` (the default) works in a worktree with your settings and allowlists. `reader` is headless in `~/.local/state/drover/reader/`, and can use only Read and the `reader_allow` commands.
- **Owner.** Only the owner of a run may `send`, `keys`, `stop`, `approve` or `deny`. Anyone may read. You own the runs you start, and a mate owns the crew it requests. `attach` makes you the owner.
- **Phase.**

```
pending ──approve-run──▶ starting ──first prompt──▶ working ──reply──▶ idle ──send──▶ working
   └──drop-run                          needs ◀──permission──▶ working      any ──done──▶ done
                                         a process that dies mid-turn ──▶ failed
```

A run that another run asks for waits in `pending` until you `approve-run` or `drop-run` it. Nothing starts without you.

### Approvals

When a tool call needs a permission, the run goes to `needs`. `drover` and `drover show` list the call with a perm id (`p1`, `p2`, …), and the owner answers with `approve` or `deny --reason`. drover approves nothing on its own: what asks and what does not is set in each agent's own settings.

- **Claude Code** actors run in permission mode `auto`. Calls that auto mode thinks are harmless run without asking, and only what it escalates reaches you, such as a call that an `ask` rule in your Claude settings matches. With `drover run --permission-mode default`, every call outside your allowlist asks. Questions (`AskUserQuestion`) are answered with `approve <run> <perm> --answer <label or text>`, one `--answer` per question. Leaving plan mode (`ExitPlanMode`) is an approve or a deny. A Claude call can wait only as long as its hook. When you do not answer within `hook_timeout`, drover denies it with "awaiting approval", and the run continues. You can still approve the call for a day: the approval then becomes the run's next prompt.
- **Cursor** asks what its own `approvalMode` asks: `allowlist` asks for shell calls outside `permissions.allow` and for edits outside the workspace, and `unrestricted` asks for nothing. A call waits for you with no deadline. drover never picks "allow always". A Cursor question (`AskQuestion`) is answered with `drover send <run> <text>`.
- **pi** has no permissions: its tools run. A pi extension that asks for confirmation reaches you as a `needs`.

### Mates and crew

A mate is a run that dispatches crew instead of writing code. You start it yourself:

```sh
drover run --role mate --repo myapp --branch mate-retries --headless --prompt "ship retries in the client, docs included"
```

Its first prompt tells it to follow the `mates` skill ([`skills/mates`](skills/mates/SKILL.md)), which must be installed for its agent. Allow `Bash(drover:*)` in that agent's permissions, or each `drover` call that the mate makes asks you. The mate:

- writes a goal graph with `drover goal set --file goal.json` (nodes with a check each, and edges), and updates it with `drover goal node <id> --status … --round … --crew …`. drover checks the shape, stores the graph with the run and streams it, but does not act on it. `drover show <mate>` prints it.
- requests crew with `drover run --parent $DROVER_RUN …`. Each crew run waits in `pending` until you `approve-run` it.
- follows its crew with `drover wait --any`, and answers their tool calls itself: a crew run's approvals go to its mate, not to you.
- speaks to you only with `drover escalate --text "…"`. The question shows in `drover` and in `drover ls --phase needs`, and the mate stays in `needs` until you answer with `drover send <mate> <text>`.

`drover attach <run>` takes any run over, crew included.

### Remotes

Each name in `remotes` is an ssh name whose herdr server is already running. drover never starts one. Use the same name as that machine's herdr label. `drover run --machine gpu …` opens the worktree under that machine's repos root, and starts the agent there: in its herdr, or headless as `ssh gpu 'cd … && exec claude …'`. Hooks come back to the host through one `ssh -R` tunnel per remote. One host runs the service; on every other machine, `drover` is the ssh client that `install.sh --remote` links.

## Harnesses

| | Claude Code | Cursor | pi |
|---|---|---|---|
| `--harness` | `claude` (default) | `cursor` | `pi` |
| Transport | `claude -p` stream-json, and `http` hooks | ACP over `cursor-agent acp` | ACP over `pi-acp` (`pi --mode rpc`) |
| Modes | pane or headless | headless; `attach` opens `cursor-agent --resume` in a pane | headless; `attach` opens `pi --session` in a pane |
| Approvals | `PermissionRequest` hook; `--permission-mode` | Cursor's `approvalMode` | none |
| Auth | the unmodified `claude` binary with your own login | Cursor's own login | pi's own providers |
| Usage | per turn, from the session's totals | not reported | when pi-acp reports it |

- **Claude Code** runs the real `claude` binary, so your subscription login, settings, hooks, MCP servers, skills and plugins all apply. drover adds only its own hooks with `--settings`. drover does not use Claude's ACP adapter, because that adapter is built on the Agent SDK, which needs an API key.
- **Cursor** uses `cursor-agent login`. Without a login, the run fails and names the login command. On a macOS remote, the login keychain is locked to an ssh session, so drover starts Cursor with `AGENT_CLI_CREDENTIAL_STORE=file`. Log in once per macOS remote from the host:

  ```sh
  ssh -t <remote> 'AGENT_CLI_CREDENTIAL_STORE=file NO_OPEN_BROWSER=1 ~/.local/bin/cursor-agent login'
  ```

  A remote Cursor runs with `CURSOR_API_KEY` unset, so only that login counts. `DROVER_REMOTE_CURSOR_CONFIG_DIR`, in the service's environment, sets the remote's `CURSOR_CONFIG_DIR`.
- **pi** uses the providers and models configured in pi itself. `--model` picks one of the session's models.

`--harness` also accepts `codex` and `opencode`, which are not implemented yet.

## Security model

- drover has no sandbox. Every run acts with your uid, on your files, with your ssh keys and your agent logins.
- Approvals guard against mistakes, not against an agent set on getting around them. drover identifies the caller only by `$DROVER_RUN`: a run that unsets it speaks as you, and can approve its own calls or start runs. What an agent may run without asking is decided by the agent's own permission settings, not by drover.
- A `reader` run is the one restricted profile: Claude gets `--restricted`, Read and the `reader_allow` commands only. Cursor runs it in its `ask` mode, and pi with read-only tools.
- The CLI talks to the service over a unix socket (`drover.sock`, mode 600). The HTTP server, for hooks and events, binds 127.0.0.1 only. It refuses requests that carry an `Origin` header or another `Host`, so a web page cannot reach it. It has no authentication: anyone with a local account on the host can reach it.
- Repo, branch and machine names are checked before they reach a shell.

## Commands

| Command | Does |
|---|---|
| `drover` | home view: runs per phase, and what needs you |
| `drover run --repo <r> --branch <b> --prompt <p>` | start a run; `--headless`, `--machine`, `--harness`, `--model`, `--permission-mode`, `--role`, `--profile`, `--parent` |
| `drover ls` | live runs; `--all`, `--phase`, `--role`, `--machine` |
| `drover show <run>` | one run: phase, owner, usage, waiting perms, last reply, children, goal graph |
| `drover reply <run>` | the whole last reply |
| `drover send <run> <text>` | the next prompt, or the answer to an escalation or a question |
| `drover approve <run> <perm>` | allow a waiting tool call; `--answer` for a question |
| `drover deny <run> <perm> [--reason <why>]` | deny it; the agent reads the reason |
| `drover approve-run <run>` · `drop-run <run>` | start or remove a pending run |
| `drover attach <run>` | take a run over in a herdr pane |
| `drover screen <run>` · `keys <run> <keys…>` | read a pane run's terminal, send it keys |
| `drover stop <run>` | interrupt the current turn |
| `drover done <run>` | close a run; `--remove` its worktree if clean and merged, `--entry` to `journal_cmd` |
| `drover wait --any` | block for the next reply, needs, done or failed of a crew; `--parent`, `--timeout` |
| `drover goal set --file <f>` · `goal node <id>` | a mate's goal graph |
| `drover escalate --text <q>` | a run asks you a question |
| `drover watch` | events as JSON lines; `--run`, `--role`, `--since` |
| `drover snapshot` | open runs as JSON; `--all` |
| `drover serve` | the service (`install.sh` runs it for you) |

`drover <command> --help` is the reference for every flag. `send`, `escalate --text`, `run --prompt`, `done --entry` and `goal set --file` read stdin when given `-`. The event stream that `drover watch` prints is also served as SSE at `http://127.0.0.1:7341/events?run=&role=&since=`.

### drover-wt

`drover-wt` is the worktree tool that `drover run` uses. It also works on its own:

```sh
drover-wt my-branch       # a new or existing branch, as $REPOS_ROOT/<repo>.my-branch
drover-wt '#123'          # a GitHub pull request's head
drover-wt ls              # worktrees of this repo, or of every repo under REPOS_ROOT
drover-wt rm              # remove this worktree, and its branch if merged
drover-wt --help
```

A new checkout gets the main clone's untracked files that match `.worktreeinclude` (gitignore syntax, for example `.env` or `node_modules/`), as copy-on-write clones where the filesystem supports it. Then an executable `.worktree-setup` in the repo runs in the new checkout, with the main clone's path as `$1`. `HERDR_WORKTREE_PREFIX`, if set, puts new branch names under a prefix.

## Skills

[`skills/drover`](skills/drover/SKILL.md) teaches an agent the drover CLI, and [`skills/mates`](skills/mates/SKILL.md) is a mate's dispatch playbook. Both are [Agent Skills](https://agentskills.io) folders. The installer does not install them: copy them into your agent's skills directory, for example:

```sh
cp -r skills/drover skills/mates ~/.claude/skills/     # Claude Code
cp -r skills/drover skills/mates ~/.agents/skills/     # agents that read ~/.agents/skills
```

A mate needs `skills/mates` on the machine where it runs.

Claude Code can also print the home view at the start of every session. Add a `SessionStart` hook to `~/.claude/settings.json`. Each session then pays for that output in tokens, so the skill, which loads only when needed, is the lighter choice.

```json
{ "hooks": { "SessionStart": [ { "hooks": [ { "type": "command", "command": "drover" } ] } ] } }
```

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI runs these three on every push and pull request. The live tests start their own service, with a temporary state dir, their own port and a throwaway repo, and clean up after themselves. They use real agents, so they cost a few cents:

| Script | Tests | Needs |
|---|---|---|
| `scripts/live-test.sh` | headless, approvals, questions, plan mode, a pane run and attach | `claude` (haiku), herdr, jq, curl, git |
| `scripts/live-test-mates.sh` | a mate's crew, wait, goal and escalate | `claude` (haiku), herdr, jq, git |
| `scripts/live-test-cursor.sh` | Cursor over ACP: turns, approve, deny, stop, resume; `REMOTE=<ssh name>` runs it there | `cursor-agent`, herdr, jq, git, python3 |
| `scripts/live-test-tunnel.sh` | the hooks tunnel binds again after the service is killed | `REMOTE=<ssh name>` |

[`AGENTS.md`](AGENTS.md) describes how the code is put together and the rules it follows: no `unsafe`, `unwrap` or slice indexing, five dependencies, and enums and newtypes instead of flags and strings. [`DESIGN.md`](DESIGN.md) explains why, and what is designed but not built. Keep changes small, one concern each, and run the checks above before you open a pull request.

## Status

drover is early (0.1) and changes without notice. The live tests cover Claude Code (pane and headless, mates) and Cursor (on this machine and over ssh). The pi adapter has unit tests but has not yet run against a live pi. These are not built yet: `--scratch` runs, the `codex` and `opencode` harnesses, per-run usage for pane runs, and the web dashboard that the HTTP endpoints are made for. Approvals that are past their deadline live in memory, so a restart of the service forgets them.

## License

MIT. See [`LICENSE`](LICENSE).

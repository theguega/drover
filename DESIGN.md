# drover v2: design

drover runs coding agents. It starts them in git worktrees, on this machine or over ssh. It runs them either in a herdr pane, for you to pair with, or headless, for the dashboard and for other agents. It carries their approvals and streams their events. It calls no model and holds no knowledge: when a task ends, its journal entry goes to `brain log`.

Status: design, round 3. Replaces the `mates` branch design. Requirements D1–D12. Built: runs, pending runs, the Harness trait with the Claude adapter (pane and headless) and one ACP adapter for Cursor and pi (headless; pi's not yet run live), events and SSE, approvals, profiles, per-run usage. Mates: `--role mate`, `goal`, `wait`, `escalate`. Not yet: plan-window usage, the dashboard.

## What changes from v1

| v1 | v2 | Why |
|---|---|---|
| herdr panes are the bus; screens are scraped | Panes are one run mode. Events come from harness hooks or a headless stream | Typed events, approvals as data (D4) |
| Discord relay, serenity, `format.rs` | Removed once the dashboard PWA and Web Push exist | ~800 lines, and the action layer was coupled to it |
| `journal.db` | `brain log` | drover holds no knowledge (D8) |
| Resident mates (inbox injection, 3-mate cap, mate tabs) | A mate is any session following the mates skill, `role: mate` | The harness already does dispatch; drover supplies worktrees, hosts and the graph (D11) |
| Scouts as a special case | A run with `--scratch` (temp dir, no repo) | One concept fewer |
| Usage from shell scripts | Per-run usage from `turn_end`; plan windows in one isolated module | D12 |
| Hand-written `Spec` tables (`cli.rs`, 1112 lines) | clap derive | About 800 lines and the completion scripts go away |
| State in the checkout | `~/.local/state/drover/drover.db` | S1 |

Kept: `drover-wt`, the AXI output rules, the owner concept (now enforced by drover inside `send`), the typed newtypes, the clippy lint set, and remote hosts over ssh.

## Layers

| Layer | Owns |
|---|---|
| herdr | panes, workspaces, human takeover |
| drover-wt | worktrees: new branch, existing branch, `#pr`, safe rm |
| drover core | the run registry, adapters, approvals, events, goal graphs |
| serve | a unix socket for the CLI, plus HTTP and SSE on localhost for the dashboard BFF |
| skills | `skills/drover` (the CLI) and `skills/mates` (the dispatch playbook) |

## Runs

A run is one agent session.

```rust
struct Run {
    id: RunId,            // r-<6 hex>
    machine: Machine,
    cwd: PathBuf,         // a worktree, or a scratch dir
    harness: HarnessName, // claude | codex | opencode | pi
    mode: Mode,           // Pane { pane } | Headless
    role: Role,           // Task | Mate | Crew | Chat
    parent: Option<RunId>,
    profile: Profile,     // Reader | Actor
    owner: Owner,         // User | Run(RunId)
    session: Option<SessionId>,  // the harness's own id, used for resume
    phase: Phase,         // starting | working | needs | idle | done | failed
    usage: Usage,         // input, output, cache_read, cache_write tokens; cost_usd
}
```

- **Pane mode** opens a herdr pane running the harness's interactive TUI, the way `drover new` does today. drover gets its events through hooks the harness posts to `serve` (Claude: `http` hooks injected with `--settings`). The screen is never scraped. `attach`, `screen` and `keys` are kept for you.
- **Headless mode** spawns the harness with its streaming interface and reads stdout. The process stays warm and takes the next prompt on stdin. It's killed after it has been idle for `idle_kill` (15 min for `role: chat`).
- **Takeover:** `drover attach <run>` on a headless run opens a pane with `--resume <session>` and sets the owner to `User`. The headless process is stopped first, so two processes never share a session.
- **Owner** is checked inside `send` and `respond`. A run that isn't the owner can read, but can't type.

## Harness adapter (C12)

There is one adapter per protocol behind one trait (Q13): Claude's own, and ACP (the Agent Client Protocol) for every harness that speaks it. Each maps its protocol to the shared event type.

```rust
trait Harness {
    fn caps(&self) -> Caps;                          // headless, pane, resume, approvals, usage, hooks: declared, never guessed
    fn start(&self, spec: &StartSpec) -> Vec<String>; // the command line: headless, or the pane's TUI; profile, model, resume
    fn open(&self, spec: &StartSpec, p: &mut Parse) -> Vec<String>;  // lines written first (ACP: initialize)
    fn send(&self, text: &str, p: &mut Parse) -> Vec<String>;        // a prompt, as stdin lines
    fn abort(&self, p: &mut Parse) -> Abort;                         // kill, or an interrupt line
    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out>;         // one stdout line, normalized
    fn answer(&self, d: &Decision, call: &Value, p: &mut Parse) -> Vec<Out>; // a permission asked on stdout
    fn hook(&self, body: &Value) -> Vec<Out>;        // one hook it posted
    fn respond(&self, d: &Decision) -> Value;        // to the waiting hook
    fn forward(&self) -> &'static [(&'static str, &'static str)]; // env a remote process reads from its stdin
}
```

The adapter is the protocol only, as pure functions over a per-process state. Spawning the process (here or over ssh), its pipes, idle kill, and what events do to a run are the same for every harness and live in drover once. Claude asks through an `http` hook; an ACP agent asks on its stdout and hears the answer on its stdin.

| Harness | Headless | Approvals | Pane observation | Ships |
|---|---|---|---|---|
| Claude Code | `claude -p --input-format stream-json --output-format stream-json --verbose` | `PermissionRequest` `http` hook to drover. Same path as pane mode | `http` hooks via `--settings` | day one |
| Cursor | `cursor-agent acp` (ACP; mode `ask` for the reader profile) | `session/request_permission`, `cursor/ask_question`, `cursor/create_plan` on stdout, answered on stdin, no deadline | none: headless only; `attach` opens `cursor-agent --resume` in a pane | built |
| Codex | `codex app-server` (JSON-RPC over stdio) | `requestApproval` server→client requests | hooks.json | when used |
| opencode | `opencode serve` + SSE `/event` | `POST /session/:id/permissions/:pid` | subscribe to the TUI's own server | when used |
| pi | `pi-acp` (ACP over `pi --mode rpc`) | none of its own; an extension's `ctx.ui.confirm` arrives as `session/request_permission` | none: headless only; `attach` opens `pi --session` | built, not run live |

Login: Claude runs the unmodified `claude` binary with your own login (allowed, D6). Other harnesses use API keys or their own providers, never Claude OAuth.

## Events (C9)

```json
{"run":"r-1a2b3c","mode":"headless","role":"crew","parent":"r-9f8e7d","at":"2026-10-19T09:12:04-07:00",
 "kind":"tool_end","data":{"tool":"Bash","ok":true},"usage":null}
```

`kind` is one of:
- `started`
- `text` (deltas are batched every 500 ms)
- `tool_start`
- `tool_end`
- `needs` (an approval with its `perm` id, or a question)
- `reply` (end of a turn: `text`, its first 300 characters; `tail`, its last 300, where `DONE:` and `QUESTION:` are; `chars`). The whole text is stored and read with `drover reply <run>`, never streamed
- `done`
- `failed`

`usage` comes with each `reply` and with `done`.

Events go to the `events` table and fan out over SSE (`GET /events?run=&role=&since=`). `drover watch` reads the same stream. `drover wait --any [--parent R]` blocks until the next `reply`, `needs`, `done` or `failed` and prints it as C4. This is what a mate loops on.

## Nothing runs without your approval

- An `actor` run starts only when you start it yourself, or when you approve it.
- A run requested by anyone else becomes a run in `phase: pending`, which you approve or drop:
  - a mate dispatching crew
  - a chat promoted to a task
  - a planner suggestion
- You answer it with `drover approve-run <run>`, or from the dashboard Inbox.
- The setting is `autostart = "never"` in config. It's the only value today, and the rest of the system is built so it can be relaxed later, for example to allow small automated proofs of concept, without redesign.
- `reader` runs (the planner, chat) can't act, so a timer may start them.
- The caller is whatever `$DROVER_RUN` says, so these approval and owner checks guard against mistakes, not against an agent set on getting around them: every run is your uid, and an agent that unsets `$DROVER_RUN` speaks as you. Enforcing more would take a sandbox and a credential per run, which the non-goals rule out.
- Tool calls inside a run go through the approval flow below.

## Approvals

How it works for Claude, decided: a `PermissionRequest` `http` hook in both modes.
- In headless mode (`claude -p`) there's no terminal for the permission dialog. A tool call outside the allowlist would just be denied, unless you pass `--dangerously-skip-permissions`.
- So drover injects a `PermissionRequest` hook with `--settings`. Claude POSTs `{session_id, tool_name, tool_input}` to `serve`, and the request blocks until drover returns `allow` or `deny`, with an optional reason.
- Pane runs use the same hook, so approving works the same in both modes, and one code path covers them.
- Fallback, if the spike shows a gap: `--permission-prompt-tool` pointed at a small MCP tool drover provides.

The approval flow:

1. `permission_request` from the harness becomes a `needs` event, and the phase changes to `needs`. An ACP agent waits for the answer as long as it takes; a hook is answered before its timeout (5).
2. The owner answers with `drover approve|deny <run> <perm>`, or the dashboard calls `POST /runs/:id/permissions/:perm`.
3. A crew run's approvals go to its mate when the mate owns it. Otherwise they go to you.
4. Nothing is approved automatically. Approval rules live in the harness's own settings (its allowlists), not in drover.
   - Claude actors run in permission mode `auto` (the desk's choice), pane and headless; `drover run --permission-mode <mode>` picks another, and a non-Claude harness refuses it. Auto mode runs harmless calls without asking, so the hook fires only for what it escalates: an `ask` rule from any settings file (tested), and perhaps a classifier denial (not tested, SPIKE.md §8). Without auto (`--permission-mode default`), every call outside the allowlist is a `needs`.
5. Long waits: if you answer after the hook's timeout, drover returns `deny` with the reason "awaiting approval". When you approve later, drover resumes the session (`--resume`) with the approval in the prompt, so an approval given an hour later from the phone isn't lost.

## Claude features

What of Claude Code (2.1.316) a run reaches through drover's Claude path: the unmodified `claude` with your login, `claude -p` stream-json headless, the TUI in a pane, `--settings` adding drover's hooks. Sources are `claude --help` and code.claude.com/docs: [headless](https://code.claude.com/docs/en/headless.md), [hooks](https://code.claude.com/docs/en/hooks.md), [settings](https://code.claude.com/docs/en/settings.md). Tested with haiku in `/tmp` on 2026-11-01. "live" names a check in `scripts/live-test.sh`; "probe" is a run of `claude -p` by hand, its output quoted.

| Feature | Headless | Pane | How drover exposes it | Evidence |
|---|---|---|---|---|
| Model | yes | yes | `run --model`; `send "/model sonnet"` mid-run | unit test; probe: `/model sonnet` replies "Set model to `Sonnet 5.5` for this session only" |
| Effort | yes | yes | `send "/effort low"`; your `effortLevel` setting. No `run --effort` | probe: "Set effort level to low (this session only)" |
| Permission modes, plan included | yes | yes | `run --permission-mode` (default `auto`) | live: the plan run, the pane in auto mode |
| Leaving plan mode (ExitPlanMode) | yes | yes | a `needs` with tool `ExitPlanMode` and the plan as input; `approve` leaves plan mode, `deny --reason` keeps planning | live: `planned.txt` exists after approve |
| AskUserQuestion | yes | yes | a `needs` with tool `AskUserQuestion`; `approve --answer <label or text>`, one per question in order, or `send <text>` for one question; plain `approve` is refused | live, headless and pane |
| Resume | yes | yes | drover resumes its own session: a cold `send`, a restart, `attach` | live: after kill -9, attach |
| Continue, fork, a session started outside drover | no | in the TUI (`/resume`, `/branch`) | not exposed: `run` always starts a new session | — |
| Built-in commands (`/compact`, `/context`, `/cost`, `/clear`, `/model`, `/effort`) | yes | yes | `send "/context"`: headless, the output is the reply (`/compact` and `/clear` reply empty; `/clear` moves the run to the new session). In a pane a command the TUI runs itself posts no hook, so the run stays idle | live: `/context` in the pane; probe: each one headless |
| Custom commands, skills | yes | yes | `send "/name args"`. Claude reads commands when it starts: one added later is "Unknown command" until a new session | live: `/livepong` in both modes |
| Subagents (Agent tool) | yes | yes | their tool calls are `tool_start`/`tool_end` of the run, not marked as theirs; their text is not streamed (no `--forward-subagent-text`). A background subagent's result comes as a turn Claude starts itself: a `reply` with no `send` | probe: a background subagent's report as a second reply |
| Background shells and tasks | yes | yes | a headless process with background tasks (`system/background_tasks_changed`) is not idle-killed; the turn Claude starts when one ends is a `reply`. An empty turn for a notification already delivered (`num_turns: 0`) is dropped | live: a 45 s background shell outlives `idle_kill = 30` and replies |
| Stop mid-tool | yes | yes | `stop`: SIGTERM to the process and its tools; the turn has no result, the next `send` resumes. Pane: `keys esc`. Claude's stream-json `interrupt`, which keeps the process warm, is not used | live: stop takes the tool with it |
| Compaction | yes | yes | `send "/compact"`, and auto-compaction, run. No event: `compact_boundary` and the `PreCompact`/`PostCompact` hooks are not read | probe: `compact_boundary`, `pre_tokens` 29939 to `post_tokens` 11122 |
| Usage and cost, subagents included | yes | no | each `reply`'s usage is what the session's `modelUsage` and `total_cost_usd` grew by. A resumed process reports from the session's start, so drover counts from the run's own usage | unit test; live: the turn after the resume costs only itself |
| MCP servers, plugins, skills, agents, CLAUDE.md from your config | yes | yes | actors read your settings and `~/.claude.json` as they are; a reader has `--restricted` | probe `init`: 22 MCP servers, 7 plugins, 52 skills, 6 agents |
| Your own hooks (`~/.claude/settings.json`, plugins) | yes | yes | `--settings` adds drover's hooks next to yours: hooks merge across settings levels, and so do `permissions` lists | probe: your `SessionStart` command hook and a plugin's `UserPromptSubmit` ran in the run where drover's `http` hooks fired; SPIKE.md §8 for an `ask` rule from the worktree |
| Images and files in a prompt | yes | yes | by path in the text (`send "Look at /path/x.png"`): the Read tool reads images. A base64 image block cannot be sent | probe: a red PNG by path, reply `RED` |
| `--add-dir` | no | `/add-dir` | not exposed; `permissions.additionalDirectories` in a settings file works | — |
| Output styles | yes | yes | `send "/output-style <name>"`, or your `outputStyle` setting | docs (headless.md) |
| Workspace trust dialog | n/a: `-p` skips it | yes | a `needs` with `dialog`, answered with `keys` | live |
| Status line | n/a | yes | yours, on the pane | `drover screen` shows it |
| Needs a TTY (`/login`, `/config`, pickers such as `/resume` or `/model` without an argument) | no | yes | a pane, or `attach` | headless.md: "Built-in commands that only run in the terminal interface … aren't available" |

How headless Claude gets AskUserQuestion and ExitPlanMode: `-p` offers them only when the run has a permission host, so an actor gets `--permission-prompt-tool stdio`. The `PermissionRequest` hook still answers every call first; Claude then cancels the `control_request` it wrote to stdout, which drover ignores. Both tools run only on an `allow` that carries `updatedInput` (a plain allow falls through to the host), so drover's allow returns their input, with `answers` added for AskUserQuestion. Any other allow is plain: an updated input is checked against the `ask` rules again, and a pane then asks in its own dialog (seen with `Bash(touch *)`).

## Profiles

| Profile | Tools | cwd | Used by |
|---|---|---|---|
| `reader` | `Bash(brain find\|get\|pages\|code *)`, `Read`; no brain write command, no shell beyond that, no network tools | `~/.local/state/drover/reader/`, which holds only the brain skill and a minimal AGENTS.md | dashboard chat, planner |
| `actor` | the harness's defaults plus your allowlists | a worktree, or a scratch dir | tasks, mates, crew |

Each profile is a settings file per harness (Claude: `--settings`, `--allowedTools`), generated by the adapter. A reader run can't run `ssh` or a brain write command (`add`, `label`, `log`, `wiki merge`), and a test checks both. Those writes are made by the caller: the tick or the dashboard BFF.

## Mates (C16)

A mate is a run with `role: mate`, started with the mates skill loaded (`skills/mates/SKILL.md`: goal graph, one check per node, at most 3 rounds without progress before escalating, bounded reports). It dispatches crew with:

```sh
drover run --parent $DROVER_RUN --repo opalin --branch fix-x --prompt "…"   # role: crew
drover goal set --file goal.json      # C16: nodes, checks, edges
drover goal node docs --status pass --round 2 --crew r-1a2b3c
drover wait --any --parent $DROVER_RUN
drover escalate --text "need a decision on X"   # becomes a needs event to you
```

The goal graph is stored with the mate's run and streamed as `goal` events. That's what the dashboard's drover monitor draws (U6). drover doesn't interpret the graph. It stores it and carries it. Shape: `{goal, done_when?, nodes: [{id, check, task?, model?, review: none|code|adversarial, status: todo|working|review|pass|blocked, round, crew}], edges: [[before, after]]}`, checked once by `goal set`. A mate is started by you with `drover run --role mate`; its first prompt names the skill. `wait` keeps a cursor per calling run; `escalate` puts the run in `needs` with the question until you `send` the answer.

## Usage (D12)

- **Per run:** token counts and cost from the harness's `turn_end` or result events, summed on the run.
- **Plan windows:** one module, `usage.rs`, reads each machine's and account's plan-window usage every 5 minutes into a cache. When its upstream endpoint breaks, the field reads `unavailable` and nothing else is affected.

## CLI

| Command | Does |
|---|---|
| `drover` | home view: runs by phase, items that need you, help |
| `drover run [--machine --repo --branch \| --scratch] [--pane \| --headless] [--harness --profile --parent] --prompt` | starts a run |
| `drover ls [--role --machine --phase]` | `runs[n/total]{run,machine,role,phase,harness,mode,cwd}` |
| `drover show <run>` | run detail: usage, last reply, children, goal graph |
| `drover send <run> <text>` | the next prompt (owner checked) |
| `drover approve\|deny <run> <perm>` | answers an approval |
| `drover wait [--any] [--parent]` | blocks until the next significant event |
| `drover attach <run>` | opens it in a herdr pane and takes ownership |
| `drover screen <run>` · `keys <run> <keys>` | pane runs only, kept for pair-coding |
| `drover stop <run>` | aborts it |
| `drover done <run> [--remove]` | asks for the journal entry, `brain log`, closes the run, removes the worktree if it's clean and merged |
| `drover goal …` · `escalate` | C16, for mates |
| `drover watch` · `snapshot` | JSON for programs |
| `drover serve` | the service |

Output follows C4 (shared with brain). `--help` is the only place flags are documented.

## Serve

- **CLI path:** a unix socket at `~/.local/state/drover/drover.sock`, mode 600, with one JSON line per request (kept from v1).
- **Dashboard path:** HTTP and SSE on `127.0.0.1:7341`, behind the BFF and oauth2-proxy. It never listens on a public interface.
- **Remote machines:** a client on another host is `drover-ssh`, which runs the CLI on the host in `DROVER_HOST`. A headless run on a remote machine is `ssh <machine> <harness headless cmd>`, and its stdio is the stream. A key the harness needs there (`CURSOR_API_KEY`) goes first on that stdin, never on a command line. A pane run on a remote machine uses that machine's herdr, as today.

## Storage

```sql
runs(id PK, machine, cwd, harness, mode, pane, role, parent, profile, owner, session, phase, usage JSON, created_at, ended_at)
events(id PK, run, at, kind, data JSON)          -- capped at 30 days, older rows archived to JSONL
goals(run PK, graph JSON, updated_at)
```

The archive runs at start and daily: events older than 30 days move to `events.jsonl` in the state dir, except those of open runs, whose `requested`, `started` and last `reply` still drive them. `ls`, `watch`, `snapshot` and the watchers read open runs only; `--all` shows the rest.

Two event kinds beyond C9 are stored, never streamed: `requested` (repo, branch and the first prompt, kept so a pending run can start after a restart) and `started` (the first prompt went out). A pending pane run has no pane yet: `Mode::Pane { pane: Option<PaneId> }`, the `pane` column NULL.

Everything lives in `~/.local/state/drover/`. Config is in `~/.config/drover/config.toml` (machines, repos root, harness paths), with credentials alongside at mode 600.

## Migration from v1

1. `state.json` tasks become `runs` rows with `mode: pane` and `role: task` (or `mate` or `crew`).
2. The 13 `journal.db` entries are exported once with `brain log --at`. After that `journal.db` is deleted.
3. `mates/state/*/goal.md` is converted by hand into C16 for any mate still running. Otherwise it's archived.
4. Discord keeps running until the PWA and Web Push exist (D5), then serenity is removed in one commit.

## Non-goals

- drover calls no model and stores no knowledge.
- No sandbox or containers. Runs act with your permissions.
- No orchestration across two drover hosts at once.
- No automatic approval policy beyond what each harness's settings already do.
- No plugin system: an adapter is a file in `src/harness/`.

## Decided

- Pane and headless are both first-class (D4). The terminal workflow stays.
- One adapter per protocol: Claude's own (stream-json and hooks; the Claude ACP adapter is built on the Agent SDK, which needs an API key), and ACP for Cursor and pi (Q13, SPIKE.md §9).
- Nothing runs without your approval: `autostart = "never"`, and actor runs requested by mates, chat or the planner wait in `pending`.
- Claude approvals go through a `PermissionRequest` `http` hook in both modes. The fallback is an MCP permission tool.
- Mates are a skill plus `role: mate`. drover stores and streams the graph (D11, C16).
- The S7 restricted agent ssh key was cut. Agents use your key, and the reader/actor split is what keeps untrusted text away from the fleet.

## Open (design level)

- The spike's four questions are answered in SPIKE.md: `-p` fires `PermissionRequest`; no timeout cap was found, and drover answers before it; a deny's reason reaches the turn; session allows do not survive `--resume`, so drover allows a late-approved call itself.
- Pane runs stream `reply` and tool events, not text deltas: the hooks carry no deltas.
- Pane runs have no per-run usage yet: hooks carry none (the transcript does).
- Claude events drover does not read yet: compaction (`compact_boundary`, `PreCompact`), subagent text, `Notification`. Each would be a new event kind.
- Expired approvals live in memory: a restart of `serve` forgets them.

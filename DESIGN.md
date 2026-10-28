# drover v2: design

drover runs coding agents. It starts them in git worktrees, on this machine or over ssh. It runs them either in a herdr pane, for you to pair with, or headless, for the dashboard and for other agents. It carries their approvals and streams their events. It calls no model and holds no knowledge: when a task ends, its journal entry goes to `brain log`.

Status: design, round 3. Replaces the `mates` branch design. Requirements D1–D12. Built: runs, pending runs, the Harness trait with the Claude, Cursor and pi adapters (pane and headless; pi's not yet run live), events and SSE, approvals, profiles, per-run usage. Mates: `--role mate`, `goal`, `wait`, `escalate`. Not yet: plan-window usage, the dashboard.

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

There is one native adapter per harness behind one trait (Q13). Each adapter maps its harness's own protocol to the shared event type.

```rust
trait Harness {
    fn caps(&self) -> Caps;                          // headless, resume, approvals, usage, hooks: declared, never guessed
    fn start(&self, spec: &StartSpec) -> Vec<String>; // the command line: pane or headless, profile, model, resume, hooks
    fn send(&self, text: &str) -> String;            // a prompt, as one stdin line
    fn respond(&self, d: &Decision) -> Answer;       // to the waiting hook, or onto stdin
    fn abort(&self) -> Abort;                        // kill, or an interrupt line
    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out>;  // one stdout line, normalized
    fn hook_settings(&self, spec: &StartSpec) -> Value;       // what makes the harness post to drover
    fn hook(&self, body: &Value) -> Vec<Out>;                 // one hook it posted
}
```

The adapter is the protocol only, as pure functions. Spawning the process (here or over ssh), its pipes, idle kill, and what events do to a run are the same for every harness and live in drover once. Cursor's command hook reaches the same endpoint through `drover hook`; pi's extension posts to the same endpoint, so it answers through the hook as Claude does (`Answer::Stdin` is for a harness with no hook).

| Harness | Headless | Approvals | Pane observation | Ships |
|---|---|---|---|---|
| Claude Code | `claude -p --input-format stream-json --output-format stream-json --verbose` | `PermissionRequest` `http` hook to drover. Same path as pane mode | `http` hooks via `--settings` | day one |
| Cursor | `cursor-agent -p --output-format stream-json --stream-partial-output` (`--mode ask` for the reader profile) | `-p` gets every tool with no prompt, so a `beforeShellExecution` / `beforeMCPExecution` command hook calls `drover hook` and blocks on drover's answer (built) | the same hooks.json, `sessionStart`/`stop` | day one |
| Codex | `codex app-server` (JSON-RPC over stdio) | `requestApproval` server→client requests | hooks.json | when used |
| opencode | `opencode serve` + SSE `/event` | `POST /session/:id/permissions/:pid` | subscribe to the TUI's own server | when used |
| pi | `pi --mode rpc` | `tool_call` extension (`harness/pi.ts`) posting to drover | the same extension in the TUI | built |

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
- `reply` (end of a turn: the first 300 characters, plus `chars`)
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

1. `permission_request` from the harness becomes a `needs` event, and the phase changes to `needs`.
2. The owner answers with `drover approve|deny <run> <perm>`, or the dashboard calls `POST /runs/:id/permissions/:perm`.
3. A crew run's approvals go to its mate when the mate owns it. Otherwise they go to you.
4. Nothing is approved automatically. Approval rules live in the harness's own settings (its allowlists), not in drover.
5. Long waits: if you answer after the hook's timeout, drover returns `deny` with the reason "awaiting approval". When you approve later, drover resumes the session (`--resume`) with the approval in the prompt, so an approval given an hour later from the phone isn't lost.

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
- **Remote machines:** a client on another host is `drover-ssh`, which runs the CLI on the host in `DROVER_HOST`. A headless run on a remote machine is `ssh <machine> <harness headless cmd>`, and its stdio is the stream. A pane run on a remote machine uses that machine's herdr, as today.

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
- One native adapter per harness. Claude and Cursor on day one (Q13).
- Nothing runs without your approval: `autostart = "never"`, and actor runs requested by mates, chat or the planner wait in `pending`.
- Claude approvals go through a `PermissionRequest` `http` hook in both modes. The fallback is an MCP permission tool.
- Mates are a skill plus `role: mate`. drover stores and streams the graph (D11, C16).
- The S7 restricted agent ssh key was cut. Agents use your key, and the reader/actor split is what keeps untrusted text away from the fleet.

## Open (design level)

- The spike's four questions are answered in SPIKE.md: `-p` fires `PermissionRequest`; no timeout cap was found, and drover answers before it; a deny's reason reaches the turn; session allows do not survive `--resume`, so drover allows a late-approved call itself.
- Pane runs stream `reply` and tool events, not text deltas: the hooks carry no deltas.
- Pane runs have no per-run usage yet: hooks carry none (the transcript does).
- Expired approvals live in memory: a restart of `serve` forgets them.

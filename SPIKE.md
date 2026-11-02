# drover v2 spike: approvals through harness hooks

The scripts were removed after v2 shipped; they are in git history at commit 3dfece9.

Run on 2026-10-27, Linux, `claude` 2.1.312 (`--model haiku`), `cursor-agent` 2026.10.22-27dcf5f.

## Setup shared by §1–§5

**Hook server.** A 38-line Python HTTP server on `127.0.0.1:$PORT` logged every hook POST (path, time, body) as a JSON line, and answered `PermissionRequest` by mode:

- `allow`: `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}`
- `deny`: the same with `"decision":{"behavior":"deny","message":"REASON-XYZZY: blocked by drover spike"}`
- `allow-session`: `"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","rules":[{"toolName":"<tool_name>","ruleContent":"<tool_input.command>"}],"behavior":"allow","destination":"session"}]}`
- `sleep:<s>`: waits `<s>` seconds, then answers `allow`.

**Settings** (`$OUT/settings.json`), with `PORT` and `TIMEOUT` filled in:

```json
{"hooks":{
 "PermissionRequest":[{"hooks":[{"type":"http","url":"http://127.0.0.1:PORT/perm","timeout":TIMEOUT}]}],
 "PreToolUse":[{"hooks":[{"type":"http","url":"http://127.0.0.1:PORT/pre","timeout":TIMEOUT}]}]}}
```

**Run.** The prompt goes in as one stream-json line on stdin, from `$OUT/work` (a fresh directory, with `spike-file.txt` removed first):

```sh
jq -cn --arg p "$PROMPT" '{type:"user",message:{role:"user",content:$p}}' |
  claude -p --input-format stream-json --output-format stream-json --verbose --model haiku \
  --settings $OUT/settings.json --permission-mode default > $OUT/stream.jsonl 2> $OUT/stderr
```

The default prompt was `Run this exact bash command with the Bash tool, then report what happened: touch spike-file.txt`.

**Printed output.** The output lines below are the hook log (`path`, `event`, `tool`, `input`, and the `reply` the server sent) and the stream's `assistant`, `user` and `result` lines. Timing (`t`, `reply_after`) and `sid` are left out.

The answer body used (Claude): `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"|"deny","message":"..."}}}`.

| # | Answer |
|---|---|
| 1 | Yes. `PermissionRequest` fires in `-p` stream-json, right after `PreToolUse`, but **only when the call needs a permission** (a read-only `echo` fires `PreToolUse` alone) |
| 2 | No cap found: 99999 accepted, a 650 s hold honored (3700 s: pending). On timeout the hook is skipped and the normal flow continues: headless has no prompt, so the call is **denied** with Claude's own "needs approval" text, not "awaiting approval" |
| 3 | Yes. The turn continues and the model quotes the reason verbatim (`is_error: true` tool_result) |
| 4 | `allow` + `updatedPermissions` (`destination: session`) persists for identical calls **within one process**. A plain `allow` does not. **Not across `--resume`**: the hook fires again |
| 5 | Yes. An interactive pane session started with `--settings` fires the same hook, and a deny shows `Denied by PermissionRequest hook` |
| 6 | Yes. `beforeShellExecution` runs in `-p stream-json`, blocks the call until the script returns (20 s hold = 20 s), and `permission: deny` blocks it with the message to the model |

## 1. Does `-p` fire `PermissionRequest`?

Run the shared setup with `PORT=8791`, `TIMEOUT=30`, hook server mode `allow`, and the default prompt.

```
{"path":"/pre","event":"PreToolUse","tool":"Bash","input":{"command":"touch spike-file.txt",...}}
{"path":"/perm","event":"PermissionRequest","tool":"Bash","input":{"command":"touch spike-file.txt",...}}
{"reply":{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}}
{"type":"user","c":[{"type":"tool_result","content":"(Bash completed with no output)","is_error":false,...}]}
```
Payload: `session_id, cwd, prompt_id, permission_mode, effort, hook_event_name, tool_name, tool_input, permission_suggestions` (`transcript_path` too). `permission_suggestions` holds ready-made `updatedPermissions` entries (`addDirectories`, `setMode`).

First attempt used `echo spike-$((6*7))`: only `PreToolUse` fired, because a read-only command needs no permission. drover must expect no `PermissionRequest` for allowlisted or read-only calls.

## 2. Timeout

Each row is the shared setup with its own `PORT`, hook server mode and `TIMEOUT`:

```
PORT=8801 TIMEOUT=5      MODE=sleep:20   # hook times out
PORT=8802 TIMEOUT=100    MODE=sleep:75
PORT=8803 TIMEOUT=99999  MODE=sleep:75
PORT=8811 TIMEOUT=100000 MODE=sleep:650  # 650 s hold
PORT=8812 TIMEOUT=100000 MODE=sleep:3700 # 3700 s hold
```
The elapsed line is `elapsed = end − start` (seconds, taken before and after the `claude` run), and `file` is whether `spike-file.txt` exists in the work directory.
```
timeout=5     elapsed=10s  file=   tool_result: "touch in '.../spike-file.txt' needs approval. The path is inside the working directories ..." (is_error)
timeout=100   elapsed=80s  file=spike-file.txt   reply_after 75.0  -> allow honored
timeout=99999 elapsed=79s  file=spike-file.txt   reply_after 75.0  -> allow honored
hold=650 timeout=100000 elapsed=654s file=spike-file.txt   reply_after 650.0 -> allow honored
```
- Timeout 5 s: the run moved on after about 5 s (total 10 s), the hook's answer is ignored, the tool call is denied by the default no-prompt flow.
- Units are seconds. Values from 5 to 99999 were accepted without a schema error. The longest hold honored is 650 s; the 3700 s run is still going and its result is not yet in this file.
- Consequence for the design: a hook that times out is a deny, but with Claude's generic message. If drover wants "awaiting approval", it has to answer before the timeout, not after.

## 3. Deny with a reason

Shared setup with `PORT=8791`, `TIMEOUT=30`, hook server mode `deny`, and the prompt:

```
Run this exact bash command with the Bash tool: touch spike-file.txt. If it is denied, quote the denial reason verbatim, then say the word CONTINUED.
```
```
{"type":"user","c":[{"type":"tool_result","content":"REASON-XYZZY: blocked by drover spike","is_error":true,...}]}
{"type":"assistant","c":[{"type":"text","text":"The command was denied. The denial reason, verbatim: `REASON-XYZZY: blocked by drover spike`\n\nCONTINUED"}]}
```
The file was not created. The `message` is the tool_result text, and the turn ends normally with a `result`.

## 4. "Allow for this session"

Two runs of the same Bash call, with the prompt `Run this exact bash command with the Bash tool and say done: touch spike-file.txt`:

1. Hook server mode `allow-session`, `PORT=8795`, `TIMEOUT=30`. Same process: send the prompt, wait 20 s, send it again, wait 15 s, all into one `claude -p` stream. Then `SID` is the `session_id` of the `system` line, `spike-file.txt` is removed, and the prompt goes into a new `claude -p ... --resume $SID` process.
2. Control: the same with hook server mode `allow` and `PORT=8796`.

```
allow-session: same process, 2 identical calls: PermissionRequest count = 1
               after --resume <sid>, 3rd identical call: PermissionRequest count = 2
plain allow:   same process, 2 identical calls: PermissionRequest count = 2
               after --resume <sid>, 3rd identical call: PermissionRequest count = 3
```
The rule sent: `"updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch spike-file.txt"}],"behavior":"allow","destination":"session"}]`. Session rules live in the process, so after `--resume` drover must answer again (or use `destination: "localSettings"`, not tested).

## 5. Interactive pane session

Hook server mode `deny`, `PORT=8797`, `TIMEOUT=30`, settings as above. Claude runs in a tmux pane, with no `-p`:

```sh
tmux new-session -d -s spike5 -x 150 -y 50 -c $OUT/work \
  "claude --model haiku --permission-mode default --settings $OUT/settings.json"
tmux send-keys -t spike5 Down Enter   # trust prompt defaults to "No, exit"
tmux send-keys -t spike5 'Run this exact bash command with the Bash tool: touch spike-file.txt. If denied, quote the reason.'
tmux send-keys -t spike5 Enter
tmux capture-pane -t spike5 -p > $OUT/pane.txt
```
```
{"path":"/pre","event":"PreToolUse","tool":"Bash"}
{"path":"/perm","event":"PermissionRequest","tool":"Bash"}
{"reply":{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"REASON-XYZZY: blocked by drover spike"}}}}
  ⎿  Denied by PermissionRequest hook
● The touch spike-file.txt command was blocked. The Bash tool returned this reason:
  ▎ REASON-XYZZY: blocked by drover spike
```
Needed `--permission-mode default`: this machine's default is auto mode, which is the user's setting, not tested here. The trust-folder dialog defaults to "No, exit". The hook fired with no dialog shown to the pane.

## 6. cursor-agent `beforeShellExecution`

Run from a fresh git repo in `$OUT/work`, with the hook config written there as `.cursor/hooks.json`:

```json
{"version":1,"hooks":{"beforeShellExecution":[{"command":"env LOG=$OUT/hook.log HOLD=3 $HOOK"}]}}
```

`$HOOK` is a shell script that appends its stdin to `$LOG` with a timestamp, sleeps `HOLD` seconds, appends its end time, and prints the deny answer:

```sh
echo "start $(date +%s.%N) $(cat)" >> $LOG
sleep ${HOLD:-3}
echo "end   $(date +%s.%N)" >> $LOG
echo '{"permission":"deny","user_message":"blocked by drover spike","agent_message":"REASON-XYZZY: blocked by drover spike"}'
```

The agent run:

```sh
date +%s.%N > $OUT/start
cursor-agent -p --output-format stream-json --force --trust \
  "Run this exact shell command and quote any denial reason verbatim: touch spike-file.txt" > $OUT/stream.jsonl
date +%s.%N > $OUT/end
```

The hook gets JSON on stdin and prints `{"permission":"deny","user_message":...,"agent_message":...}`. Output with `HOLD=3`:

```
start 1793165691.01 {"conversation_id":...,"model":...,"command":"touch spike-file.txt","cwd":"","sandbox":false,"tool_use_id":...,"session_id":...}
end   1793165694.01
file:                                   # not created
{"type":"assistant",...,"text":"The command was denied with the following message:\n\n```text\nRejected: Command execution was blocked by a hook: blocked by drover spike ..."}
```
With `HOLD=20`: `start ...707.64`, `end ...727.64`, total run 30.1 s, against 15.7 s with the 3 s hold, so the call waits for the hook. Run with `--force --trust`: the hook still denies, so a hook outranks `--force`. The `agent_message` text was not what the model quoted; it quoted the `user_message`.

## 7. cursor-agent, found building the adapter

- No warm process: `--input-format` is unknown, a prompt is argv or stdin to EOF. `--resume <chatId>` keeps the session id, and a hook's `session_id` is the stream's.
- A hook command with `://` in it is skipped without a word (no error, the call just runs), so the hook command drover had then took `host:port/path`. Hooks come from `.cursor/hooks.json` in the project, `~/.cursor/hooks.json`, or `/etc/cursor/hooks.json`; no flag or env.
- A 75 s hold with `"timeout": 120` was honored. A denied call is a `tool_call` `completed` with `rejected.reason`, and the model quotes the `user_message`.
- The TUI fires `sessionStart`, `beforeSubmitPrompt`, `afterAgentResponse` (the turn's whole text, once) and `stop`; `-p` fires `sessionStart` and the before hooks, not `stop`. `{}` answers them. Its trust dialog takes the first typed prompt: `--trust`.

## Not tested

- `PermissionRequest` for tools other than Bash.
- `allow` with `destination: "localSettings"` (would persist across `--resume`).
- Interactive claude in auto mode (the user's default).

## 8. Auto mode and the `PermissionRequest` hook

Run on 2026-11-01, `claude` 2.1.316 (`--model haiku`); `~/.claude/settings.json` has `"permissions": {"defaultMode": "auto"}`. Everything ran in `/tmp/drover-s8/work` (a fresh git repo), with the §1 hook server (port 7377, answers `allow`, logs `PreToolUse` and `PermissionRequest` with the payload's `permission_mode`) and the same `--settings` with both hooks. Headless as in the shared setup, with `--permission-mode auto`; the prompt was `Run exactly this with the Bash tool: <cmd>`.

| # | Answer |
|---|---|
| 1 | In auto mode the hook does **not** fire for `ls`, `cat a.txt`, `touch new.txt`, `curl -s -m 5 https://example.com`, `rm -rf <named /tmp dir>`, `touch` outside the cwd, `git push` (no remote), `pip install`, `sudo -n true`, `chmod -R`, a `Write` into `.claude/`, `touch`/`rm -rf` in `.git/`: each ran, `PreToolUse` only. The payload has `permission_mode: "auto"` |
| 2 | It **does** fire when something forces a prompt: an `ask` rule (`"permissions":{"ask":["Bash(touch *)"]}`, passed through `--settings` here; a project `.claude/settings.local.json` is the same kind of source). `ls` still ran with no hook; `touch asked.txt` fired `PermissionRequest`, and the `allow` ran it |
| 3 | Same in a pane (tmux, `claude --permission-mode auto --settings …`, after the trust dialog): `ls` fires `PreToolUse` only; `touch pane-asked.txt` fires `PermissionRequest`, and the pane shows `Allowed by PermissionRequest hook` |
| 4 | **Not tested**: whether the hook fires when the classifier denies. The command the classifier blocks (`curl … \| sh`, reason `[Code from External]`) is not one to run, even in `/tmp`. Nothing here shows the hook firing for a classifier denial |

```
### ls                                  -> {"tool_result":"a.txt\ndoomed","is_error":false}
{"path": "/pre", "event": "PreToolUse", "tool": "Bash", "mode": "auto", "input": "ls"}
### touch asked.txt   (settings: permissions.ask = ["Bash(touch *)"])
{"tool_result":"(Bash completed with no output)","is_error":false}
{"path": "/pre", "event": "PreToolUse", "tool": "Bash", "mode": "auto", "input": "touch asked.txt"}
{"path": "/perm", "event": "PermissionRequest", "tool": "Bash", "mode": "auto", "input": "touch asked.txt"}
```

Consequences:

- Auto mode and the hook coexist: `--permission-mode auto --settings <hook>` loads both, and the hook sees what would otherwise go to the user.
- With auto, drover's `needs` events are the rare calls: ask rules, and (untested) whatever the classifier hands back. A harmless worktree command never reaches drover.
- The deterministic way to get an escalation in a test is an `ask` rule in the worktree's `.claude/settings.local.json`.

## 9. Cursor and pi over ACP

Run on 2026-11-01, Linux (meerkat) and macOS (chicken, over ssh), `cursor-agent` 2026.10.22-27dcf5f; `pi-acp` 0.0.34 and pi 0.85.1 read, not run (no working provider here). The ACP types are those of `@agentclientprotocol/sdk` 0.26.0 (`dist/schema/types.gen.d.ts`).

**Driver.** A 40-line Python client in a scratch dir spawned `cursor-agent acp` in `/tmp/drover-acp/probe` (a fresh git repo), wrote one JSON-RPC request a line (`initialize` with `clientCapabilities` `fs` and `terminal` off, `authenticate`, `session/new {cwd, mcpServers: []}`, then a `session/prompt` per prompt read from its stdin), logged every line both ways, and answered `session/request_permission` with `allow-once`, or with `reject-once` and then `session/cancel`. A second process sent `session/load` on the first's session id. Cursor's own config dir was `CURSOR_CONFIG_DIR=/tmp/drover-acp/cfg`, holding only `{"version":1,"permissions":{"allow":[],"deny":[]},"approvalMode":"allowlist"}`. The bundle quoted below is `~/.local/share/cursor-agent/versions/2026.10.22-27dcf5f/*.index.js`.

| # | Finding |
|---|---|
| 1 | `authenticate` with `cursor_login` starts a browser login even when the CLI is logged in, and fails headless: `{"code":-32602,"message":"Invalid params","data":{"message":"Failed to open browser for login. Please visit: https://cursor.com/loginDeepControl?…"}}`. `session/new` works without it, on the CLI's own login or an API key. drover does not send it |
| 2 | Whether a call asks is cursor's own `approvalMode` (`cli-config.json` in `CURSOR_CONFIG_DIR`, default `~/.cursor`). With `unrestricted` (this desk's), no `session/request_permission` ever came: `touch`, `curl`, `git add`, an edit all ran. With `allowlist`, a shell call asks (the request's content says `Shell allowlist is empty`); an edit in the workspace does not; an edit under `.cursor/` does (`checkCursorFileProtection`, a team setting); an edit outside the workspace does unless it is under `/tmp/`, `/var/tmp/` or `/var/folders/` |
| 3 | The answer has no reason: `RequestPermissionOutcome` is `cancelled` or `selected {optionId}`, and cursor maps a reject to `{approved: false, reason: "User rejected"}` (`requestPermission` in `6136.index.js`); `_meta` is not read. So drover rejects, sends `session/cancel`, and when the prompt returns (`stopReason: cancelled`) sends `drover: the user denied <title>: <reason>` as the next prompt. The model quotes it (§9 live test: `REASON-XYZZY no files today`) |
| 4 | `allow-always` writes the call to the allowlist in `cli-config.json` (`persistAllowlistEntry`). drover answers `allow_once` only |
| 5 | Request ids from the agent start at 0. The request carries `toolCall {toolCallId, title, kind, status, content}`, no `rawInput`; the `tool_call` update before it has `rawInput` (`{"command": "touch x"}`), and an edit's request a `diff` block with its `path` |
| 6 | `session/load` replays the history as `session/update`s (`user_message_chunk`, agent chunks, `tool_call` with ids `replay-0-2`, …) before it answers; afterwards the model knows the conversation (asked for its first reply, it said `PONG-1`) |
| 7 | No `usage` in the `session/prompt` response and no `usage_update`: cursor reports no tokens or cost over ACP |
| 8 | `session/cancel` during an approved `sleep 2717` ended the sleep, the prompt returned `stopReason: cancelled`, and the process took the next prompt |
| 9 | If the client errors on `cursor/ask_question`, cursor asks each single-choice question as a `session/request_permission` whose options are the answers (`ask-question` handler); drover serves `cursor/ask_question` itself |
| 10 | On a mac over ssh, `cursor-agent` exits at start with `Error: Your macOS login keychain is locked.` (`--version` too). The check is skipped for `AGENT_CLI_CREDENTIAL_STORE=memory` (`credential-store.ts`); with that and an API key, `cursor-agent acp` on chicken answered `PONG` (`cursor-agent status` still says `Not logged in`). drover no longer uses a key: `AGENT_CLI_CREDENTIAL_STORE=file` keeps the OAuth login in `~/.cursor/auth.json` (`getAuthFilePath`, by home dir, not `CURSOR_CONFIG_DIR`), `cursor-agent login` writes it, and without a login `session/new` answers `-32000 Authentication required` at once. An API key in the remote's login environment (chicken's `.zshenv` had one) wins over the file and hides a missing login |
| 10b | Found in review: cursor asks calls of one step together (requests 0 and 1 within 1 ms). Rejecting one and sending `session/cancel` ends the turn (`stopReason: cancelled`) and cursor drops the other request unanswered. ACP asks the client to answer every pending request `cancelled` after `session/cancel`, so drover does, and that call's `needs` ends |
| 10c | On macOS, `ps -o lstart= -p <pid>` ends with four spaces (`[Sun Nov  1 20:28:20 2026    ]`), Linux's with none: a start time compared as text is trimmed on both sides |
| 11 | pi-acp spawns `pi --mode rpc --no-themes` (only `--session <file>` added; `PI_ACP_PI_COMMAND` replaces the binary, not the arguments). Its ACP session id is pi's own session id, so `pi --session <id>` opens it. Its session modes are pi's thinking levels. It asks `session/request_permission` only for an extension's UI (`confirm`, `select`): pi itself has no permissions. pi reads `defaultTools` from `<cwd>/.pi/settings.json` (`settingsManager.getDefaultTools`), the reader's closest read-only mode |

```
>> {"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"ac8cef79-…","prompt":[{"type":"text","text":"Run this exact shell command: touch spike.txt. Then say DONE."}]}}
<< {"jsonrpc":"2.0","method":"session/update","params":{…,"update":{"sessionUpdate":"tool_call","toolCallId":"call-4e19…","title":"`touch spike.txt`","kind":"execute","status":"pending","rawInput":{"command":"touch spike.txt"}}}}
<< {"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{…,"toolCall":{"toolCallId":"call-4e19…","title":"`touch spike.txt`","kind":"execute","status":"pending","content":[{"type":"content","content":{"type":"text","text":"Shell allowlist is empty"}}]},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"allow-always","name":"Allow always","kind":"allow_always"},{"optionId":"reject-once","name":"Reject","kind":"reject_once"}]}}
>> {"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}
<< {"jsonrpc":"2.0","method":"session/update","params":{…,"update":{"sessionUpdate":"tool_call_update","toolCallId":"call-4e19…","status":"completed","rawOutput":{"exitCode":0,"stdout":"","stderr":""}}}}
<< {"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}
```

The live test is `scripts/live-test-cursor.sh` (and `REMOTE=chicken`): a reply, a second turn in the same process, a shell call approved, an edit under `.cursor/` denied with a reason the reply quotes, `stop` during a running tool, and a resume by `session/load` after `kill -9` of the agent. Both pass.

Not tested: pi over ACP live; `cursor/ask_question` and `cursor/create_plan` from a real cursor (the unit tests use the documented shapes); MCP calls.

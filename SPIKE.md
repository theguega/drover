# drover v2 spike: approvals through harness hooks

Run on 2026-10-27, Linux, `claude` 2.1.312 (`--model haiku`), `cursor-agent` 2026.10.22-27dcf5f.
Scripts are in `spike/`. Every run writes to `/tmp/spike-*`. Rerun from the repo root.

- `spike/hook_server.py PORT`: logs each hook POST to `$LOG`, answers by `$MODE` (`allow`, `deny`, `allow-session`, `sleep:<s>` then allow, `silent:<s>` never answers).
- `spike/settings.sh PORT TIMEOUT`: the `--settings` JSON, with `http` hooks for `PermissionRequest` (`/perm`) and `PreToolUse` (`/pre`).
- `spike/q1.sh MODE TIMEOUT`: one headless stream-json run. `spike/show.sh OUT` prints its log and stream.

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

```sh
OUT=/tmp/spike-q1 spike/q1.sh allow 30; spike/show.sh /tmp/spike-q1
```
```
{"path":"/pre","event":"PreToolUse","tool":"Bash","input":{"command":"touch spike-file.txt",...}}
{"path":"/perm","event":"PermissionRequest","tool":"Bash","input":{"command":"touch spike-file.txt",...}}
{"reply":{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}}
{"type":"user","c":[{"type":"tool_result","content":"(Bash completed with no output)","is_error":false,...}]}
```
Payload: `session_id, cwd, prompt_id, permission_mode, effort, hook_event_name, tool_name, tool_input, permission_suggestions` (`transcript_path` too). `permission_suggestions` holds ready-made `updatedPermissions` entries (`addDirectories`, `setMode`).

First attempt used `echo spike-$((6*7))`: only `PreToolUse` fired, because a read-only command needs no permission. drover must expect no `PermissionRequest` for allowlisted or read-only calls.

## 2. Timeout

```sh
PORT=8801 OUT=/tmp/spike-q2-t5   spike/q1.sh sleep:20 5      # hook times out
PORT=8802 OUT=/tmp/spike-q2-t100 spike/q1.sh sleep:75 100    # > 60 s
PORT=8803 OUT=/tmp/spike-q2-t99999 spike/q1.sh sleep:75 99999
spike/q2_long.sh 650 100000 8811                            # 650 s hold
spike/q2_long.sh 3700 100000 8812                           # 3700 s hold
```
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

```sh
OUT=/tmp/spike-q3 PROMPT="Run this exact bash command with the Bash tool: touch spike-file.txt. If it is denied, quote the denial reason verbatim, then say the word CONTINUED." spike/q1.sh deny 30; spike/show.sh /tmp/spike-q3
```
```
{"type":"user","c":[{"type":"tool_result","content":"REASON-XYZZY: blocked by drover spike","is_error":true,...}]}
{"type":"assistant","c":[{"type":"text","text":"The command was denied. The denial reason, verbatim: `REASON-XYZZY: blocked by drover spike`\n\nCONTINUED"}]}
```
The file was not created. The `message` is the tool_result text, and the turn ends normally with a `result`.

## 4. "Allow for this session"

```sh
spike/q4.sh                                       # allow + updatedPermissions(session)
OUT=/tmp/spike-q4-plain PORT=8796 spike/q4.sh allow  # control: plain allow
```
```
allow-session: same process, 2 identical calls: PermissionRequest count = 1
               after --resume <sid>, 3rd identical call: PermissionRequest count = 2
plain allow:   same process, 2 identical calls: PermissionRequest count = 2
               after --resume <sid>, 3rd identical call: PermissionRequest count = 3
```
The rule sent: `"updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch spike-file.txt"}],"behavior":"allow","destination":"session"}]`. Session rules live in the process, so after `--resume` drover must answer again (or use `destination: "localSettings"`, not tested).

## 5. Interactive pane session

```sh
spike/q5.sh
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

```sh
spike/q6.sh 3     # hook holds 3 s then denies
spike/q6.sh 20    # holds 20 s
```
Config written by the script: `.cursor/hooks.json` = `{"version":1,"hooks":{"beforeShellExecution":[{"command":"env ... spike/cursor_hook.sh"}]}}`. The hook gets JSON on stdin and prints `{"permission":"deny","user_message":...,"agent_message":...}`.
```
start 1793165691.01 {"conversation_id":...,"model":...,"command":"touch spike-file.txt","cwd":"","sandbox":false,"tool_use_id":...,"session_id":...}
end   1793165694.01
file:                                   # not created
{"type":"assistant",...,"text":"The command was denied with the following message:\n\n```text\nRejected: Command execution was blocked by a hook: blocked by drover spike ..."}
```
With HOLD=20: `start ...707.64`, `end ...727.64`, total run 30.1 s, against 15.7 s with the 3 s hold, so the call waits for the hook. Run with `--force --trust`: the hook still denies, so a hook outranks `--force`. The `agent_message` text was not what the model quoted; it quoted the `user_message`.

## Not tested

- `PermissionRequest` for tools other than Bash.
- `allow` with `destination: "localSettings"` (would persist across `--resume`).
- Interactive claude in auto mode (the user's default).

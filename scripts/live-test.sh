#!/usr/bin/env bash
# Live test against a real `claude --model haiku` and this machine's herdr, through its own
# drover serve: a temp state dir, its own HTTP port, a throwaway repo. Cleans up after itself.
#   scripts/live-test.sh             LIVE_PORT (default 7399), WAIT seconds per event (default 180)
# Each check prints `ok` or `FAIL`; exit 0 when all pass. The pane run opens a herdr workspace
# and answers Claude's folder-trust dialog with drover keys.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo build -q --manifest-path "$root/Cargo.toml"
bin=$root/target/debug/drover
for cmd in claude herdr jq curl git; do
  command -v "$cmd" >/dev/null || { echo "live-test needs $cmd on PATH" >&2; exit 1; }
done

# short: a unix socket path is capped at 108 bytes
tmp=$(mktemp -d /tmp/drover-live.XXXX)
port=${LIVE_PORT:-7399}
export DROVER_STATE_DIR=$tmp/state DROVER_CONFIG=$tmp/config.toml
# the test speaks as you
unset DROVER_RUN
mkdir -p "$DROVER_STATE_DIR" "$tmp/repos"
printf 'host = "live"\nrepos_root = "%s"\nhttp_port = %s\nhook_timeout = 30\nidle_kill = 30\nreader_allow = ["echo *"]\n' "$tmp/repos" "$port" >"$DROVER_CONFIG"

repo=$tmp/repos/livetest
git init -q --bare "$tmp/repos/origin.git"
git clone -q "$tmp/repos/origin.git" "$repo" 2>/dev/null
git -C "$repo" switch -q -c main
# a custom command, there before Claude starts: it reads them once
mkdir -p "$repo/.claude/commands"
printf -- '---\ndescription: drover live test\n---\nReply with exactly: CUSTOM-PONG\n' >"$repo/.claude/commands/livepong.md"
git -C "$repo" add .claude
git -C "$repo" commit -q -m init
git -C "$repo" push -q origin main

"$bin" serve >"$tmp/serve.log" 2>&1 &
serve=$!
runs=()
cleanup() {
  for r in "${runs[@]}"; do "$bin" done "$r" >/dev/null 2>&1 || true; done
  kill "$serve" 2>/dev/null || true
  # herdr also opens a workspace on the main clone
  herdr workspace list | jq -r --arg t "$tmp/" '.result.workspaces[] | select(.worktree.checkout_path // "" | startswith($t)) | .workspace_id' |
    while read -r ws; do herdr workspace close "$ws" >/dev/null || true; done
  rm -rf "$tmp"
}
trap cleanup EXIT
for _ in $(seq 50); do [[ -S $DROVER_STATE_DIR/drover.sock ]] && break; sleep 0.1; done

ok() { echo "ok   $*"; }
fail() {
  echo "FAIL $*"
  echo "--- events"
  timeout 1 "$bin" watch --since 0 | jq -c 'select(.kind != "text") | {run, kind, data}' | tail -8 | cut -c1-300 || true
  echo "--- serve.log"
  tail -20 "$tmp/serve.log"
  exit 1
}
# one field of drover's TOON output
field() { awk -v k="$1:" '$1 == k { $1 = ""; sub(/^ /, ""); print; exit }'; }

# The next event of a run whose kind matches a regex and that passes a jq condition, read from
# `drover watch` after the run's cursor. Sets $event and moves the cursor past it.
declare -A cursor
await() {
  local run=$1 kind=$2 cond=${3:-true} fd pid line
  exec {fd}< <(exec timeout "${WAIT:-180}" "$bin" watch --run "$run" --since "${cursor[$run]:-0}")
  pid=$!
  while IFS= read -r -u "$fd" line; do
    if jq -e --arg k "^($kind)$" '(.kind | test($k)) and ('"$cond"')' <<<"$line" >/dev/null; then
      cursor[$run]=$(jq .id <<<"$line")
      event=$line
      kill "$pid" 2>/dev/null || true
      exec {fd}<&-
      return 0
    fi
  done
  exec {fd}<&-
  return 1
}
text() { jq -r .data.text <<<"$event"; }

# ── headless ──
h=$("$bin" run --repo livetest --branch live-h --headless --permission-mode default --model haiku --prompt "Reply with exactly: PONG-1" | field run)
runs+=("$h")
await "$h" reply '.data.text | test("PONG-1")' || fail "headless $h: first reply"
ok "headless $h replied: $(text)"

[[ $("$bin" show "$h" | field warm) == true ]] || fail "headless $h: not warm after its turn"
session=$("$bin" show "$h" | field session)
"$bin" send "$h" "Reply with exactly: PONG-2" >/dev/null
await "$h" reply '.data.text | test("PONG-2")' || fail "headless $h: second reply"
[[ $("$bin" show "$h" | field session) == "$session" ]] || fail "headless $h: the session changed"
ok "the warm process took the second prompt: $(text)"

cwd=$("$bin" show "$h" | field cwd)
"$bin" send "$h" "Run this exact bash command with the Bash tool: touch approved.txt. Then reply with exactly: DONE-3" >/dev/null
await "$h" needs '.data.perm != null' || fail "headless $h: no needs for touch"
perm=$(jq -r .data.perm <<<"$event")
[[ $("$bin" show "$h" | field phase) == needs ]] || fail "headless $h: phase is not needs"
ok "needs $perm: $(jq -c .data.input.command <<<"$event")"
"$bin" approve "$h" "$perm" >/dev/null
await "$h" reply || fail "headless $h: no reply after approve"
[[ -f $cwd/approved.txt ]] || fail "headless $h: approved.txt was not created"
ok "approved, and $cwd/approved.txt exists"

"$bin" send "$h" "Run this exact bash command with the Bash tool: touch denied.txt. If it is denied, quote the denial reason verbatim." >/dev/null
await "$h" needs '.data.perm != null' || fail "headless $h: no needs for the denied touch"
perm=$(jq -r .data.perm <<<"$event")
"$bin" deny "$h" "$perm" --reason "REASON-XYZZY no files today" >/dev/null
await "$h" reply '.data.text | test("REASON-XYZZY")' || fail "headless $h: the reason did not reach the reply"
[[ ! -e $cwd/denied.txt ]] || fail "headless $h: denied.txt was created"
ok "denied $perm, and the reply quotes the reason: $(text | tr '\n' ' ')"

cost=$("$bin" show "$h" | field cost_usd)
awk -v c="$cost" 'BEGIN { exit !(c > 0) }' || fail "headless $h: no usage"
ok "usage summed on the run: cost_usd $cost"

sse=$(curl -sN --max-time 3 "http://127.0.0.1:$port/events?run=$h&since=0" || true)
[[ $(grep -c '^event: reply' <<<"$sse") -ge 4 ]] || fail "SSE: fewer than 4 replies"
ok "GET /events streams the same events: $(grep -c '^event: ' <<<"$sse") of them"

# ── a question: AskUserQuestion is a needs, and approve takes the answer ──
"$bin" send "$h" "Use the AskUserQuestion tool to ask me one question: 'Which color?' with the options Red and Blue. Then reply with exactly the label I picked, prefixed ANSWER:" >/dev/null
await "$h" needs '.data.tool == "AskUserQuestion"' || fail "headless $h: AskUserQuestion did not reach drover"
perm=$(jq -r .data.perm <<<"$event")
! "$bin" approve "$h" "$perm" >/dev/null 2>&1 || fail "headless $h: a question was approved with no answer"
"$bin" approve "$h" "$perm" --answer Blue >/dev/null
await "$h" reply '.data.text | test("ANSWER: ?Blue")' || fail "headless $h: the answer did not reach the reply"
ok "AskUserQuestion $perm answered through approve --answer: $(text)"

# ── plan mode: ExitPlanMode is a needs, and approve leaves plan mode ──
pl=$("$bin" run --repo livetest --branch live-plan --headless --permission-mode plan --model haiku --prompt "Plan to create the file planned.txt containing hi, in one line, then call ExitPlanMode with it. Once it is approved, create the file and reply with exactly: PLAN-DONE" | field run)
runs+=("$pl")
await "$pl" needs '.data.tool == "ExitPlanMode"' || fail "headless $pl: ExitPlanMode did not reach drover"
"$bin" approve "$pl" "$(jq -r .data.perm <<<"$event")" >/dev/null
# out of plan mode, the write may ask too
while await "$pl" 'needs|reply' '.kind == "reply" or .data.perm != null'; do
  [[ $(jq -r .kind <<<"$event") == reply ]] && break
  "$bin" approve "$pl" "$(jq -r .data.perm <<<"$event")" >/dev/null
done
[[ -f $("$bin" show "$pl" | field cwd)/planned.txt ]] || fail "headless $pl: planned.txt was not created after the plan was approved"
ok "plan approved through drover, and $pl wrote planned.txt"

# ── a background shell: the process stays warm past idle_kill, and its turn after reaches drover ──
bg=$("$bin" run --repo livetest --branch live-bg --headless --model haiku --prompt "Run this exact command with the Bash tool, with run_in_background set to true: sleep 45 && touch bg.txt. Do not wait for it: end your turn now. When it has finished, reply with exactly: BG-FINISHED" | field run)
runs+=("$bg")
await "$bg" reply || fail "headless $bg: no first reply"
await "$bg" reply '.data.text | test("BG-FINISHED")' || fail "headless $bg: no reply when its background shell ended"
[[ -f $("$bin" show "$bg" | field cwd)/bg.txt ]] || fail "headless $bg: the background shell was ended early"
ok "$bg's background shell outlived idle_kill (30 s), and Claude's own next turn replied: $(text)"

# ── a permission no hook decides ends with a deny, never a hang ──
# a run drover does not know: the hook answers with a deny, not {}
unknown=$(curl -s -X POST -H 'Content-Type: application/json' "http://127.0.0.1:$port/hooks/r-ffffff" \
  -d '{"hook_event_name":"PermissionRequest","session_id":"317266eb-32ce-4e67-9457-7a097c828473","tool_name":"Bash","tool_input":{"command":"true"}}')
[[ $(jq -r .hookSpecificOutput.decision.behavior <<<"$unknown") == deny ]] || fail "a permission hook for an unknown run got: $unknown"
# drover frozen past claude's hook timeout (30 s): the hook gives no decision, claude waits on its
# stdin, and drover's backstop denies the call there once it runs again
"$bin" send "$h" "Run this exact bash command with the Bash tool: touch frozen.txt. If it is denied, reply with exactly: FROZEN-DENIED" >/dev/null
await "$h" needs '.data.perm != null' || fail "headless $h: no needs for frozen.txt"
kill -STOP "$serve"
sleep 37
kill -CONT "$serve"
await "$h" reply || fail "headless $h: the call no hook decided hung the turn"
[[ ! -e $cwd/frozen.txt ]] || fail "headless $h: frozen.txt was created with no decision"
grep -q "left unanswered, denied" "$tmp/serve.log" || fail "headless $h: the backstop did not deny the call"
ok "a hook for an unknown run is denied, and a call no hook decided was denied on stdin: $(text | tr '\n' ' ' | cut -c1-100)"

# ── a custom command as a prompt ──
"$bin" send "$h" "/livepong" >/dev/null
await "$h" reply '.data.text | test("CUSTOM-PONG")' || fail "headless $h: /livepong did not run"
ok "headless $h ran the custom command /livepong"

# ── a client that sends nothing does not hold a socket ──
fds() { ls "/proc/$serve/fd" | wc -l; }
before=$(fds)
socks=()
for _ in $(seq 300); do
  exec {s}<>"/dev/tcp/127.0.0.1/$port"
  socks+=("$s")
done
held=$(fds)
sleep 7
after=$(fds)
for s in "${socks[@]}"; do exec {s}>&-; done
((after < before + 20)) || fail "300 silent sockets: serve went from $before fds to $held, and $after after 7 s"
ok "300 silent sockets: serve from $before fds to $held, back to $after after 7 s"

# ── a reader runs only what reader_allow names: `echo *` is no licence for `&& touch` ──
rd=$("$bin" run --profile reader --model haiku --prompt "Run exactly one Bash call: echo ok. Then reply with exactly: R-1" | field run)
runs+=("$rd")
await "$rd" tool_end '.data.tool == "Bash"' || fail "reader $rd: no Bash call"
[[ $(jq .data.ok <<<"$event") == true ]] || fail "reader $rd: echo ok was denied"
await "$rd" reply || fail "reader $rd: no reply"
"$bin" send "$rd" "Run exactly one Bash call, as is: echo a && touch $tmp/canary. Then reply with exactly: R-2" >/dev/null
await "$rd" tool_end '.data.tool == "Bash"' || fail "reader $rd: no second Bash call"
[[ $(jq .data.ok <<<"$event") == false ]] || fail "reader $rd: a chained command ran"
[[ ! -e $tmp/canary ]] || fail "reader $rd: the canary was touched"
ok "reader $rd ran echo ok, and was denied echo a && touch canary"

# ── folders: --dir, a reader's --dir and --repo scout, --scratch ──
reply_after_approving() {
  while await "$1" 'needs|reply' '.kind == "reply" or .data.perm != null'; do
    [[ $(jq -r .kind <<<"$event") == reply ]] && return 0
    "$bin" approve "$1" "$(jq -r .data.perm <<<"$event")" >/dev/null
  done
  return 1
}
dir=$tmp/folder
mkdir -p "$dir"
echo "the secret word is PELICAN" >"$dir/note.txt"
f=$("$bin" run --dir "$dir" --key "dir-$$" --headless --permission-mode default --model haiku --prompt "Run this exact bash command with the Bash tool: touch made.txt. Then reply with exactly: DIR-DONE" | field run)
runs+=("$f")
reply_after_approving "$f" || fail "dir $f: no reply"
[[ $("$bin" show "$f" | field place) == dir && $("$bin" show "$f" | field cwd) == "$dir" ]] || fail "dir $f: not a dir run in $dir"
[[ -f $dir/made.txt ]] || fail "dir $f: made.txt was not written in $dir"
out=$("$bin" run --dir "$dir" --headless --prompt "second" 2>&1 || true)
grep -q "open already, in run $f" <<<"$out" || fail "a second actor in $dir was not refused: $out"
[[ $("$bin" run --dir "$dir" --key "dir-$$" --headless --prompt "retry" | field run) == "$f" ]] || fail "dir $f: a retry with its key started another run"
out=$("$bin" done "$f" --remove 2>&1 || true)
grep -q "never deletes" <<<"$out" || fail "done --remove on a dir run was not refused: $out"
"$bin" done "$f" >/dev/null
[[ -f $dir/made.txt && -f $dir/note.txt ]] || fail "dir $f: done touched $dir"
ok "actor --dir $f wrote made.txt there, a second actor and done --remove were refused, a retry with its --key returned it, and done left the folder"

rf=$("$bin" run --profile reader --dir "$dir" --model haiku --prompt "Read note.txt and reply with exactly the secret word in it, nothing else" | field run)
runs+=("$rf")
await "$rf" reply '.data.text | test("PELICAN")' || fail "reader $rf: did not read $dir/note.txt"
"$bin" send "$rf" "Run exactly one Bash call, as is: touch nope.txt. Then reply with exactly: W-DONE" >/dev/null
await "$rf" tool_end '.data.tool == "Bash"' || fail "reader $rf: no Bash call"
[[ $(jq .data.ok <<<"$event") == false ]] || fail "reader $rf: touch ran"
[[ ! -e $dir/nope.txt ]] || fail "reader $rf: wrote into $dir"
ok "reader --dir $rf answered from $dir/note.txt, and was denied a write there"

sc=$("$bin" run --profile reader --repo livetest --model haiku --prompt "Read .claude/commands/livepong.md and reply with exactly the word after 'Reply with exactly:' in it" | field run)
runs+=("$sc")
await "$sc" reply '.data.text | test("CUSTOM-PONG")' || fail "scout $sc: no answer from the repo's main checkout"
[[ $("$bin" show "$sc" | field cwd) == "$repo" ]] || fail "scout $sc: not in $repo"
ok "reader --repo scout $sc read the main checkout: $(text)"

sh_run=$("$bin" run --scratch --headless --model haiku --prompt "Reply with exactly: SCRATCH-PONG" | field run)
runs+=("$sh_run")
await "$sh_run" reply '.data.text | test("SCRATCH-PONG")' || fail "scratch $sh_run: no reply"
scratch=$("$bin" show "$sh_run" | field cwd)
[[ -d $scratch ]] || fail "scratch $sh_run: no folder $scratch"
"$bin" done "$sh_run" >/dev/null
[[ ! -e $scratch ]] || fail "scratch $sh_run: $scratch is still there after done"
ok "headless --scratch $sh_run ran in $scratch, gone after done"

sp=$("$bin" run --scratch --model haiku --prompt "Reply with exactly: SCRATCH-PANE" | field run)
runs+=("$sp")
await "$sp" 'needs|started' || fail "scratch pane $sp: neither started nor a startup dialog"
if [[ $(jq -r .kind <<<"$event") == needs ]]; then
  "$bin" keys "$sp" down enter >/dev/null
  await "$sp" started || fail "scratch pane $sp: not started after the dialog"
fi
await "$sp" reply '.data.text | test("SCRATCH-PANE")' || fail "scratch pane $sp: no reply"
scratch=$("$bin" show "$sp" | field cwd)
[[ -d $scratch ]] || fail "scratch pane $sp: no folder $scratch"
"$bin" done "$sp" >/dev/null
[[ ! -e $scratch ]] || fail "scratch pane $sp: $scratch is still there after done"
ok "pane --scratch $sp ran in $scratch, gone after done"

# ── a stopped process takes the tools it started with it ──
leak=drvleak$$
printf 'exec -a %s tail -f /dev/null\n' "$leak" >"$tmp/wait.sh"
leaking() { pgrep -f "^$leak" >/dev/null; }
# the run starts a tool that never ends; any call it asks for on the way is allowed
tool() {
  "$bin" send "$h" "Run this exact bash command with the Bash tool, in the foreground: bash $tmp/wait.sh. It waits on purpose until I stop you; that is expected, so run it as is." >/dev/null
  for _ in 1 2 3; do
    await "$h" 'needs|reply' '.kind == "reply" or .data.perm != null' || fail "headless $h: no tool call"
    [[ $(jq -r .kind <<<"$event") == reply ]] && fail "headless $h replied instead of running wait.sh: $(text)"
    "$bin" approve "$h" "$(jq -r .data.perm <<<"$event")" >/dev/null
    if jq -e '.data.input.command | test("wait.sh") and (test("^cat|^head|^less") | not)' <<<"$event" >/dev/null; then
      for _ in $(seq 60); do leaking && return 0; sleep 0.5; done
    fi
  done
  fail "headless $h: wait.sh never ran"
}
tool
"$bin" stop "$h" >/dev/null
for _ in $(seq 20); do leaking || break; sleep 0.5; done
if leaking; then
  ps -o pid,ppid,sid,cmd -p "$(pgrep -f "^$leak" | paste -sd,)"
  pkill -f "^$leak"
  fail "stop $h left its tool running"
fi
ok "stop $h ended claude and the tool it ran"

# ── a killed service leaves no second process on a session ──
tool
on_session() { pgrep -fc -- "$session" || true; }
[[ $(on_session) == 1 ]] || fail "headless $h: $(on_session) processes on its session before the kill"
kill -KILL "$serve"
wait "$serve" 2>/dev/null || true
"$bin" serve >>"$tmp/serve.log" 2>&1 &
serve=$!
for _ in $(seq 50); do "$bin" ls >/dev/null 2>&1 && break; sleep 0.1; done
"$bin" send "$h" "Reply with exactly: AFTER-RESTART" >/dev/null
await "$h" reply '.data.text | test("AFTER-RESTART")' || fail "headless $h: no reply after the restart"
[[ $(on_session) == 1 ]] || fail "headless $h: $(on_session) processes on its session after the restart"
! leaking || fail "the restarted service left the old process's tool running"
ok "after kill -9 of serve, one process on the session, and the old one's tool is gone"
# the resumed process reports the session's totals from its start, which the run counted already
awk -v t="$(jq .usage.cost_usd <<<"$event")" -v all="$("$bin" show "$h" | field cost_usd)" 'BEGIN { exit !(t < all / 2) }' ||
  fail "headless $h: the resumed turn counted the whole session again: $(jq -c .usage <<<"$event")"
ok "the turn after the resume cost only itself: $(jq .usage.cost_usd <<<"$event") of $("$bin" show "$h" | field cost_usd)"

# ── pane ──
p=$("$bin" run --repo livetest --branch live-p --model haiku --prompt "Reply with exactly: PANE-PONG" | field run)
runs+=("$p")
await "$p" 'needs|started' || fail "pane $p: neither started nor a startup dialog"
if [[ $(jq -r .kind <<<"$event") == needs ]]; then
  "$bin" keys "$p" down enter >/dev/null
  ok "pane $p: answered the folder-trust dialog"
  await "$p" started || fail "pane $p: not started after the dialog"
fi
ok "pane $p started"
await "$p" reply '.data.text | test("PANE-PONG")' || fail "pane $p: no reply from its Stop hook"
[[ $("$bin" show "$p" | field session) =~ ^[0-9a-f-]{36}$ ]] || fail "pane $p: no session from its hooks"
ok "pane $p replied through its hooks: $(text)"

# a command the TUI runs itself posts no hook: the run stays idle for the next send
"$bin" send "$p" "/context" >/dev/null
sleep 3
[[ $("$bin" show "$p" | field phase) == idle ]] || fail "pane $p: /context left it $("$bin" show "$p" | field phase)"
"$bin" keys "$p" esc >/dev/null
# one that is a turn makes it working until its reply: a custom command, and /compact, whose
# hooks are PreCompact and PostCompact
seen_working() {
  for _ in $(seq 100); do
    [[ $("$bin" show "$p" | field phase) == working ]] && return 0
    sleep 0.1
  done
  return 1
}
"$bin" send "$p" "/livepong" >/dev/null
seen_working || fail "pane $p: /livepong never made it working"
await "$p" reply '.data.text | test("CUSTOM-PONG")' || fail "pane $p: /livepong did not run after /context"
[[ $("$bin" show "$p" | field phase) == idle ]] || fail "pane $p: not idle after /livepong replied"
"$bin" send "$p" "/compact" >/dev/null
seen_working || fail "pane $p: /compact never made it working"
await "$p" reply || fail "pane $p: /compact ended with no reply"
[[ $("$bin" show "$p" | field phase) == idle ]] || fail "pane $p: not idle after /compact"
ok "pane $p: /context kept it idle; /livepong and /compact were working until their replies"

"$bin" send "$p" "Use the AskUserQuestion tool to ask me one question: 'Which color?' with the options Red and Blue. Then reply with exactly the label I picked, prefixed ANSWER:" >/dev/null
await "$p" needs '.data.tool == "AskUserQuestion"' || fail "pane $p: AskUserQuestion did not reach drover"
# send answers the question that waits, as it does cursor's
"$bin" send "$p" Red >/dev/null
await "$p" reply '.data.text | test("ANSWER: ?Red")' || fail "pane $p: the answer did not reach the reply"
ok "pane $p: AskUserQuestion answered through drover send: $(text)"

# ── a pane in the user's own mode (auto): harmless calls never reach drover, an escalated one does ──
a=$("$bin" run --repo livetest --branch live-a --model haiku --prompt "Run this exact bash command with the Bash tool: ls. Then reply with exactly: LS-DONE" | field run)
runs+=("$a")
needs_perm=false
while await "$a" 'needs|reply' '.kind == "reply" or .data.perm != null or .data.dialog != null'; do
  case $(jq -r '.kind + (.data.dialog // "" | if . == "" then "" else " dialog" end)' <<<"$event") in
    "needs dialog") "$bin" keys "$a" down enter >/dev/null ;;
    needs) needs_perm=true; break ;;
    reply) break ;;
  esac
done
[[ $needs_perm == false ]] || fail "pane $a: ls reached drover as needs: $(jq -c .data <<<"$event")"
grep -q LS-DONE <<<"$(text)" || fail "pane $a: no LS-DONE reply: $(text)"
ok "pane $a in auto mode ran ls with no needs"
# an ask rule is what makes auto mode put a call to the user
mkdir -p "$("$bin" show "$a" | field cwd)/.claude"
echo '{"permissions":{"ask":["Bash(touch *)"]}}' >"$("$bin" show "$a" | field cwd)/.claude/settings.local.json"
"$bin" send "$a" "Run this exact bash command with the Bash tool: touch asked.txt. Then reply with exactly: TOUCH-DONE" >/dev/null
await "$a" needs '.data.perm != null' || fail "pane $a: the escalated touch did not reach drover"
"$bin" approve "$a" "$(jq -r .data.perm <<<"$event")" >/dev/null
await "$a" reply '.data.text | test("TOUCH-DONE")' || fail "pane $a: no reply after approve"
[[ -f $("$bin" show "$a" | field cwd)/asked.txt ]] || fail "pane $a: asked.txt was not created"
ok "pane $a: an ask rule reached drover as needs, and the call ran after approve"

# ── attach: the headless session goes on in a pane ──
"$bin" attach "$h" >/dev/null
for _ in $(seq 60); do
  case $("$bin" show "$h" | field phase) in
    idle) break ;;
    needs) "$bin" keys "$h" down enter >/dev/null; sleep 2 ;;
    *) sleep 1 ;;
  esac
done
[[ $("$bin" show "$h" | field mode) == pane ]] || fail "attach $h: not a pane run"
"$bin" send "$h" "What reason did you quote? Reply in one line starting with ATTACHED:" >/dev/null
await "$h" reply '.mode == "pane" and (.data.text | test("ATTACHED.*XYZZY"))' || fail "attach $h: the pane did not resume the session"
ok "attached $h in a pane, same session: $(text)"

echo "all checks passed"

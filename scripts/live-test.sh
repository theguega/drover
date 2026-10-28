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
printf 'host = "live"\nrepos_root = "%s"\nhttp_port = %s\nhook_timeout = 300\n' "$tmp/repos" "$port" >"$DROVER_CONFIG"

repo=$tmp/repos/livetest
git init -q --bare "$tmp/repos/origin.git"
git clone -q "$tmp/repos/origin.git" "$repo" 2>/dev/null
git -C "$repo" switch -q -c main
git -C "$repo" commit -q --allow-empty -m init
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
h=$("$bin" run --repo livetest --branch live-h --headless --model haiku --prompt "Reply with exactly: PONG-1" | field run)
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

#!/usr/bin/env bash
# Live test against a real cursor-agent over ACP, through its own drover serve: a temp state dir,
# its own HTTP port, a throwaway repo. Cleans up after itself.
#   scripts/live-test-cursor.sh            on this machine
#   REMOTE=chicken scripts/live-test-cursor.sh   the run on that ssh remote, drover here
# LIVE_PORT (default 7381), WAIT seconds per event (default 180), MODEL (default auto).
# A remote needs CURSOR_API_KEY in this environment: drover sends it on the agent's stdin.
# Each check prints `ok` or `FAIL`; exit 0 when all pass.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo build -q --manifest-path "$root/Cargo.toml"
bin=$root/target/debug/drover
for cmd in cursor-agent herdr jq git python3; do
  command -v "$cmd" >/dev/null || { echo "live-test-cursor needs $cmd on PATH" >&2; exit 1; }
done
remote=${REMOTE:-}
if [[ -n $remote && -z ${CURSOR_API_KEY:-} ]]; then
  echo "a remote run needs CURSOR_API_KEY: a mac's keychain is locked to ssh" >&2
  exit 1
fi

# short: a unix socket path is capped at 108 bytes
tmp=$(mktemp -d /tmp/drover-cur.XXXX)
port=${LIVE_PORT:-7381}
export DROVER_STATE_DIR=$tmp/state DROVER_CONFIG=$tmp/config.toml
# cursor's own config, in allowlist mode so shell calls ask; your approvalMode may run everything.
# A remote run gets the same path, written there too, as its CURSOR_CONFIG_DIR.
export CURSOR_CONFIG_DIR=$tmp/cursor DROVER_REMOTE_CURSOR_CONFIG_DIR=$tmp/cursor
# the test speaks as you
unset DROVER_RUN
mkdir -p "$DROVER_STATE_DIR"
{
  printf 'host = "live"\nrepos_root = "%s"\nhttp_port = %s\n' "$tmp/repos" "$port"
  [[ -z $remote ]] || printf 'remotes = ["%s"]\nremote_repos_root = "%s"\n' "$remote" "$tmp/repos"
} >"$DROVER_CONFIG"

# a shell command on the run's machine
on() {
  if [[ -n $remote ]]; then ssh -o BatchMode=yes "$remote" "$1"; else sh -c "$1"; fi
}
on "set -e
mkdir -p $tmp/repos $tmp/cursor
echo '{\"version\":1,\"permissions\":{\"allow\":[],\"deny\":[]},\"approvalMode\":\"allowlist\"}' >$tmp/cursor/cli-config.json
git init -q --bare $tmp/repos/origin.git
git clone -q $tmp/repos/origin.git $tmp/repos/livetest 2>/dev/null
cd $tmp/repos/livetest && git switch -q -c main
echo live >README && git add README && git -c user.name=live -c user.email=live@test commit -q -m init && git push -q origin main"

"$bin" serve >"$tmp/serve.log" 2>&1 &
serve=$!
runs=()
cleanup() {
  for r in "${runs[@]}"; do "$bin" done "$r" >/dev/null 2>&1 || true; done
  kill "$serve" 2>/dev/null || true
  on "herdr workspace list" 2>/dev/null | jq -r --arg t "$tmp/" '.result.workspaces[] | select(.worktree.checkout_path // "" | contains($t)) | .workspace_id' |
    while read -r ws; do on "herdr workspace close $ws" >/dev/null || true; done
  on "pkill -f '^sleep 2717$'" 2>/dev/null || true
  [[ -z $remote ]] || on "rm -rf $tmp" || true
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
# the agent process drover keeps for a run, as drover.db records it
pid_of() {
  python3 -I -c 'import sqlite3, sys
c = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True)
r = c.execute("SELECT pid FROM procs WHERE run = ?", (sys.argv[2],)).fetchone()
print(r[0] if r else "")' "$DROVER_STATE_DIR/drover.db" "$1"
}

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

where=${remote:+--machine $remote}
# shellcheck disable=SC2086
h=$("$bin" run --harness cursor $where --repo livetest --branch live-c --model "${MODEL:-auto}" --prompt "Reply with exactly: PONG-1" | field run)
runs+=("$h")
await "$h" reply '.data.text | test("PONG-1")' || fail "cursor $h: first reply"
ok "cursor $h on ${remote:-this machine} replied: $(text)"

cwd=$("$bin" show "$h" | field cwd)
session=$("$bin" show "$h" | field session)
[[ $session =~ ^[0-9a-f-]{36}$ ]] || fail "cursor $h: no session"
[[ $("$bin" show "$h" | field mode) == headless ]] || fail "cursor $h: not headless"
pid=$(pid_of "$h")
[[ -n $pid ]] || fail "cursor $h: no process recorded"

"$bin" send "$h" "Reply with exactly: PONG-2" >/dev/null
await "$h" reply '.data.text | test("PONG-2")' || fail "cursor $h: second reply"
[[ $(pid_of "$h") == "$pid" ]] || fail "cursor $h: the second turn ran in another process"
[[ $("$bin" show "$h" | field session) == "$session" ]] || fail "cursor $h: the session changed"
ok "the second turn ran in the same warm process $pid: $(text)"

"$bin" send "$h" "Run this exact shell command: touch approved.txt. Then reply with exactly: DONE-3" >/dev/null
await "$h" needs '.data.perm != null' || fail "cursor $h: no needs for touch"
perm=$(jq -r .data.perm <<<"$event")
[[ $(jq -r .data.tool <<<"$event") == Shell ]] || fail "cursor $h: the needs is not a shell call: $event"
[[ $("$bin" show "$h" | field phase) == needs ]] || fail "cursor $h: phase is not needs"
on "test ! -e '$cwd/approved.txt'" || fail "cursor $h: approved.txt exists before the approval"
ok "needs $perm: $(jq -c .data.input.command <<<"$event")"
"$bin" approve "$h" "$perm" >/dev/null
await "$h" reply || fail "cursor $h: no reply after approve"
on "test -f '$cwd/approved.txt'" || fail "cursor $h: approved.txt was not created"
ok "approved, and approved.txt exists"

# an edit under .cursor/ asks, even in the worktree
"$bin" send "$h" "Use your file edit tool, not the shell, to create the file .cursor/notes.json containing {\"a\": 1}. If it is denied, quote the denial reason verbatim." >/dev/null
await "$h" needs '.data.perm != null' || fail "cursor $h: no needs for the edit"
perm=$(jq -r .data.perm <<<"$event")
[[ $(jq -r .data.tool <<<"$event") == Edit ]] || fail "cursor $h: the needs is not an edit: $event"
"$bin" deny "$h" "$perm" --reason "REASON-XYZZY no files today" >/dev/null
await "$h" reply '.data.text | test("REASON-XYZZY")' || fail "cursor $h: the reason did not reach the reply"
on "test ! -e '$cwd/.cursor/notes.json'" || fail "cursor $h: notes.json was created"
ok "denied the edit $perm, and the reply quotes the reason: $(text | tr '\n' ' ' | cut -c1-120)"

# stop cancels the turn, and the tool it runs goes with it
"$bin" send "$h" "Run this exact shell command in the foreground: sleep 2717. It waits on purpose; that is expected." >/dev/null
await "$h" needs '.data.perm != null' || fail "cursor $h: no needs for sleep"
"$bin" approve "$h" "$(jq -r .data.perm <<<"$event")" >/dev/null
for _ in $(seq 60); do on "pgrep -f '^sleep 2717$'" >/dev/null && break; sleep 0.5; done
on "pgrep -f '^sleep 2717$'" >/dev/null || fail "cursor $h: sleep never ran"
"$bin" stop "$h" >/dev/null
await "$h" reply || fail "cursor $h: no reply after stop"
for _ in $(seq 30); do on "pgrep -f '^sleep 2717$'" >/dev/null || break; sleep 0.5; done
on "pgrep -f '^sleep 2717$'" >/dev/null && fail "stop $h left its tool running"
[[ $("$bin" show "$h" | field phase) == idle ]] || fail "cursor $h: not idle after stop"
[[ $(pid_of "$h") == "$pid" ]] || fail "cursor $h: stop ended the process"
ok "stop $h cancelled the turn and the sleep, and the process stays warm"

# the agent dies between turns: the next send loads its session in a new process
on "kill -9 $pid"
for _ in $(seq 50); do [[ -z $(pid_of "$h") ]] && break; sleep 0.2; done
[[ -z $(pid_of "$h") ]] || fail "cursor $h: drover did not see the process die"
[[ $("$bin" show "$h" | field phase) == idle ]] || fail "cursor $h: a process that died idle failed the run"
"$bin" send "$h" "What exact word did you reply with in your very first reply in this conversation? Reply with that word only." >/dev/null
await "$h" reply '.data.text | test("PONG-1")' || fail "cursor $h: the resumed session does not know its history"
[[ $("$bin" show "$h" | field session) == "$session" ]] || fail "cursor $h: the session changed on resume"
[[ $(pid_of "$h") != "$pid" ]] || fail "cursor $h: no new process"
ok "kill -9 $pid, then session/load in process $(pid_of "$h"): $(text)"

# done ends the agent, and drover forgets it only once it is gone
pid=$(pid_of "$h")
"$bin" done "$h" >/dev/null
runs=()
for _ in $(seq 50); do on "kill -0 $pid" 2>/dev/null || break; sleep 0.2; done
on "kill -0 $pid" 2>/dev/null && fail "done $h left its agent $pid running"
for _ in $(seq 50); do [[ -z $(pid_of "$h") ]] && break; sleep 0.2; done
[[ -z $(pid_of "$h") ]] || fail "cursor $h: its process is still recorded"
ok "done $h ended its agent $pid"
echo "all checks passed"

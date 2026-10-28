#!/usr/bin/env bash
# Live test against a real cursor-agent, through its own drover serve: a temp state dir, its own
# HTTP port, a throwaway repo. Cleans up after itself.
#   scripts/live-test-cursor.sh      LIVE_PORT (default 7381), WAIT seconds per event (default 180),
#                                    MODEL (default auto)
# Each check prints `ok` or `FAIL`; exit 0 when all pass. Headless only: cursor runs a process per
# turn, resumed by session id, and holds shell calls with the hooks drover puts in the run's cwd.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo build -q --manifest-path "$root/Cargo.toml"
bin=$root/target/debug/drover
for cmd in cursor-agent herdr jq git; do
  command -v "$cmd" >/dev/null || { echo "live-test-cursor needs $cmd on PATH" >&2; exit 1; }
done

# short: a unix socket path is capped at 108 bytes
tmp=$(mktemp -d /tmp/drover-cur.XXXX)
port=${LIVE_PORT:-7381}
export DROVER_STATE_DIR=$tmp/state DROVER_CONFIG=$tmp/config.toml
# the test speaks as you
unset DROVER_RUN
mkdir -p "$DROVER_STATE_DIR" "$tmp/repos"
printf 'host = "live"\nrepos_root = "%s"\nhttp_port = %s\nhook_timeout = 20\n' "$tmp/repos" "$port" >"$DROVER_CONFIG"

repo=$tmp/repos/livetest
git init -q --bare "$tmp/repos/origin.git"
git clone -q "$tmp/repos/origin.git" "$repo" 2>/dev/null
git -C "$repo" switch -q -c main
# a repo with hooks of its own: they keep running, and come back as they were
mkdir "$repo/.cursor"
echo '{"version":1,"hooks":{}}' >"$repo/.cursor/hooks.json"
git -C "$repo" add .cursor
git -C "$repo" commit -q -m init
git -C "$repo" push -q origin main

"$bin" serve >"$tmp/serve.log" 2>&1 &
serve=$!
runs=()
cleanup() {
  for r in "${runs[@]}"; do "$bin" done "$r" >/dev/null 2>&1 || true; done
  kill "$serve" 2>/dev/null || true
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

h=$("$bin" run --harness cursor --repo livetest --branch live-c --headless --model "${MODEL:-auto}" --prompt "Reply with exactly: PONG-1" | field run)
runs+=("$h")
await "$h" reply '.data.text | test("PONG-1")' || fail "cursor $h: first reply"
ok "cursor $h replied: $(text)"

cwd=$("$bin" show "$h" | field cwd)
session=$("$bin" show "$h" | field session)
[[ $session =~ ^[0-9a-f-]{36}$ ]] || fail "cursor $h: no session"
[[ $("$bin" show "$h" | field input) -gt 0 ]] || fail "cursor $h: no usage"
ok "session $session, and the turn's tokens are on the run"

# no warm process: the second turn is a second process, resumed by the session
"$bin" send "$h" "Reply with exactly: PONG-2" >/dev/null
await "$h" reply '.data.text | test("PONG-2")' || fail "cursor $h: second reply"
[[ $("$bin" show "$h" | field session) == "$session" ]] || fail "cursor $h: the session changed"
ok "the second turn resumed the session: $(text)"

[[ $(jq -r '.hooks.beforeShellExecution | length' "$cwd/.cursor/hooks.json") == 1 ]] || fail "cursor $h: no hooks installed in $cwd"

"$bin" send "$h" "Run this exact shell command: touch approved.txt. Then reply with exactly: DONE-3" >/dev/null
await "$h" needs '.data.perm != null' || fail "cursor $h: no needs for touch"
perm=$(jq -r .data.perm <<<"$event")
[[ $("$bin" show "$h" | field phase) == needs ]] || fail "cursor $h: phase is not needs"
[[ ! -e $cwd/approved.txt ]] || fail "cursor $h: approved.txt exists before the approval"
ok "needs $perm: $(jq -c .data.input.command <<<"$event")"
"$bin" approve "$h" "$perm" >/dev/null
await "$h" reply || fail "cursor $h: no reply after approve"
[[ -f $cwd/approved.txt ]] || fail "cursor $h: approved.txt was not created"
ok "approved, and $cwd/approved.txt exists"

"$bin" send "$h" "Run this exact shell command: touch denied.txt. If it is denied, quote the denial reason verbatim." >/dev/null
await "$h" needs '.data.perm != null' || fail "cursor $h: no needs for the denied touch"
perm=$(jq -r .data.perm <<<"$event")
"$bin" deny "$h" "$perm" --reason "REASON-XYZZY no files today" >/dev/null
await "$h" reply '.data.text | test("REASON-XYZZY")' || fail "cursor $h: the reason did not reach the reply"
[[ ! -e $cwd/denied.txt ]] || fail "cursor $h: denied.txt was created"
ok "denied $perm, and the reply quotes the reason: $(text | tr '\n' ' ')"

# a service that stops answering: cursor's hook timeout would let the call run, so the hook gives up
# first with a deny
"$bin" send "$h" "Run this exact shell command: touch stopped.txt. If it is denied, say DENIED-4." >/dev/null
await "$h" needs '.data.perm != null' || fail "cursor $h: no needs for the stopped touch"
kill -STOP "$serve"
sleep 25
kill -CONT "$serve"
await "$h" reply || fail "cursor $h: no reply after the service came back"
[[ ! -e $cwd/stopped.txt ]] || fail "cursor $h: stopped.txt was created while the service was stopped"
ok "serve stopped past the hook timeout: stopped.txt was not created"

"$bin" done "$h" >/dev/null
runs=()
[[ -z $(git -C "$cwd" status --porcelain -- .cursor) ]] || fail "cursor $h: the repo's .cursor/hooks.json was not restored"
ok "done $h, and the repo's own .cursor/hooks.json is back as it was"

echo "all checks passed"

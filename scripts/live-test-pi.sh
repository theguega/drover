#!/usr/bin/env bash
# Live test of the pi adapter against a real `pi` (its configured provider and model) through its
# own drover serve: a temp state dir, its own HTTP port, a throwaway repo. Cleans up after itself.
#   scripts/live-test-pi.sh          LIVE_PORT (default 7382), WAIT seconds per event (default 180)
# Each check prints `ok` or `FAIL`; exit 0 when all pass. Needs harness/pi.ts in pi's extensions:
# it is linked there for the run when install.sh has not done it.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo build -q --manifest-path "$root/Cargo.toml"
bin=$root/target/debug/drover
for cmd in pi herdr jq curl git; do
  command -v "$cmd" >/dev/null || { echo "live-test needs $cmd on PATH" >&2; exit 1; }
done

# short: a unix socket path is capped at 108 bytes
tmp=$(mktemp -d /tmp/drover-live.XXXX)
port=${LIVE_PORT:-7382}
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
ext=~/.pi/agent/extensions/drover.ts
if [[ ! -e $ext ]]; then
  mkdir -p "$(dirname "$ext")"
  ln -s "$root/harness/pi.ts" "$ext"
  linked=1
fi
cleanup() {
  [[ -z ${linked:-} ]] || rm -f "$ext"
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
h=$("$bin" run --harness pi --repo livetest --branch live-h --headless --prompt "Reply with exactly: PONG-1" | field run)
runs+=("$h")
await "$h" reply '.data.text | test("PONG-1")' || fail "headless $h: first reply"
ok "headless $h replied: $(text)"

[[ $("$bin" show "$h" | field warm) == true ]] || fail "headless $h: not warm after its turn"
session=$("$bin" show "$h" | field session)
[[ -n $session ]] || fail "headless $h: no session"
"$bin" send "$h" "Reply with exactly: PONG-2" >/dev/null
await "$h" reply '.data.text | test("PONG-2")' || fail "headless $h: second reply"
[[ $("$bin" show "$h" | field session) == "$session" ]] || fail "headless $h: the session changed"
ok "the warm process took the second prompt: $(text)"

cwd=$("$bin" show "$h" | field cwd)
"$bin" send "$h" "Run this exact bash command with the bash tool: touch approved.txt. Then reply with exactly: DONE-3" >/dev/null
await "$h" needs '.data.perm != null' || fail "headless $h: no needs for touch"
perm=$(jq -r .data.perm <<<"$event")
[[ $("$bin" show "$h" | field phase) == needs ]] || fail "headless $h: phase is not needs"
sleep 2
[[ ! -e $cwd/approved.txt ]] || fail "headless $h: the call ran before it was approved"
ok "held $perm: $(jq -c .data.input.command <<<"$event")"
"$bin" approve "$h" "$perm" >/dev/null
await "$h" reply || fail "headless $h: no reply after approve"
[[ -f $cwd/approved.txt ]] || fail "headless $h: approved.txt was not created"
ok "approved, and $cwd/approved.txt exists"

"$bin" send "$h" "Run this exact bash command with the bash tool: touch denied.txt. If it is denied, quote the denial reason verbatim." >/dev/null
await "$h" needs '.data.perm != null' || fail "headless $h: no needs for the denied touch"
perm=$(jq -r .data.perm <<<"$event")
"$bin" deny "$h" "$perm" --reason "REASON-XYZZY no files today" >/dev/null
await "$h" reply '.data.text | test("REASON-XYZZY")' || fail "headless $h: the reason did not reach the reply"
[[ ! -e $cwd/denied.txt ]] || fail "headless $h: denied.txt was created"
ok "denied $perm, and the reply quotes the reason: $(text | tr '\n' ' ')"


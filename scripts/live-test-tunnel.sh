#!/usr/bin/env bash
# Live test of the hooks tunnel against an ssh remote, through its own drover serve: a temp state
# dir and its own HTTP port. serve is killed with SIGKILL, with SIGTERM, and restarted; each time
# the remote's port must be bound again within 60 s, and after the test nothing holds it.
#   REMOTE=chicken scripts/live-test-tunnel.sh
# LIVE_PORT (default 7391). Each check prints `ok` or `FAIL`; exit 0 when all pass.
set -euo pipefail

remote=${REMOTE:?REMOTE=<ssh name> is required}
port=${LIVE_PORT:-7391}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo build -q --manifest-path "$root/Cargo.toml"
bin=$root/target/debug/drover
tmp=$(mktemp -d /tmp/drover-tun.XXXX)
export DROVER_STATE_DIR=$tmp/state DROVER_CONFIG=$tmp/config.toml
unset DROVER_RUN
mkdir -p "$DROVER_STATE_DIR"
printf 'host = "live"\nrepos_root = "%s"\nhttp_port = %s\nremotes = ["%s"]\nremote_repos_root = "%s"\n' \
  "$tmp/repos" "$port" "$remote" "$tmp/repos" >"$DROVER_CONFIG"

# ControlPath=none: a mux master of yours must not carry or outlive the checks
listening() { ssh -o BatchMode=yes -o ControlPath=none "$remote" "lsof -nP -iTCP:$port -sTCP:LISTEN" 2>/dev/null | grep -c LISTEN || true; }
if [[ $(listening) != 0 ]]; then echo "port $port is already bound on $remote: set LIVE_PORT" >&2; exit 1; fi

fail=0
check() { if [[ $2 == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got $2, want $3"; fail=1; fi; }
serve=
start() { "$bin" serve >>"$tmp/serve.log" 2>&1 & serve=$!; }
# seconds until the remote's port is bound, 61 when it is not within 60
bound_in() {
  local t=0
  until [[ $(listening) == 1 ]]; do
    sleep 1; t=$((t + 1))
    [[ $t -le 60 ]] || break
  done
  echo $t
}
cleanup() {
  kill "$serve" 2>/dev/null || true
  sleep 2
  # the leftovers a failing test may have: only this port's tunnel
  pkill -f -- "-R 127.0.0.1:$port:127.0.0.1:$port $remote" || true
  rm -rf "$tmp"
}
trap cleanup EXIT

start
t=$(bound_in); [[ $t -le 60 ]] && ok=1 || ok=0; check "first bind within 60 s ($t s)" "$ok" 1
for sig in KILL TERM; do
  kill -"$sig" "$serve"; wait "$serve" 2>/dev/null || true
  start
  t=$(bound_in); [[ $t -le 60 ]] && ok=1 || ok=0; check "SIG$sig, restart: bound within 60 s ($t s)" "$ok" 1
done
# the tunnel's ssh is gone with serve, not left on a live connection
kill "$serve"; wait "$serve" 2>/dev/null || true
sleep 3
check "no ssh tunnel left here" "$(pgrep -fc -- "-R 127.0.0.1:$port:127.0.0.1:$port $remote" || true)" 0
check "nothing holds $port on $remote" "$(listening)" 0
exit $fail

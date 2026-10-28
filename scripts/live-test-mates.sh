#!/usr/bin/env bash
# Live test of the mate flow against a real `claude --model haiku`, through its own drover serve:
# a temp state dir, its own HTTP port, a throwaway repo. The mate is a real headless run that we
# speak for with DROVER_RUN, so every step is deterministic; the crew it requests is a real haiku.
#   scripts/live-test-mates.sh      LIVE_PORT (default 7383), WAIT seconds per wait (default 180)
# Each check prints `ok` or `FAIL`; exit 0 when all pass.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo build -q --manifest-path "$root/Cargo.toml"
bin=$root/target/debug/drover
for cmd in claude herdr jq git; do
  command -v "$cmd" >/dev/null || { echo "live-test-mates needs $cmd on PATH" >&2; exit 1; }
done

# short: a unix socket path is capped at 108 bytes
tmp=$(mktemp -d /tmp/drover-mates.XXXX)
port=${LIVE_PORT:-7383}
export DROVER_STATE_DIR=$tmp/state DROVER_CONFIG=$tmp/config.toml
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
field() { awk -v k="$1:" '$1 == k { $1 = ""; sub(/^ /, ""); print; exit }'; }
# the mate's own commands: drover with DROVER_RUN set
as_mate() { DROVER_RUN=$mate "$bin" "$@"; }

# ── the mate ──
mate=$("$bin" run --role mate --repo livetest --branch mate --headless --model haiku --prompt "Reply with exactly: MATE-READY" | field run)
runs+=("$mate")
[[ $("$bin" show "$mate" | field role) == mate ]] || fail "mate $mate: role is not mate"
for _ in $(seq 120); do [[ $("$bin" show "$mate" | field phase) == idle ]] && break; sleep 1; done
[[ $("$bin" show "$mate" | field phase) == idle ]] || fail "mate $mate: never replied"
ok "mate $mate: role mate, idle after its first turn"

# ── crew it requests waits for you ──
crew=$(as_mate run --parent "$mate" --repo livetest --branch crew-a --headless --model haiku --prompt "Reply with exactly: CREW-PONG" | field run)
runs+=("$crew")
[[ $("$bin" show "$crew" | field phase) == pending ]] || fail "crew $crew: not pending"
"$bin" | grep -q "$crew" || fail "home does not list pending $crew"
as_mate approve-run "$crew" >/dev/null 2>&1 && fail "the mate approved its own crew"
ok "crew $crew waits in pending, in the home view; the mate cannot approve it"
"$bin" approve-run "$crew" >/dev/null

# ── wait returns its reply, once ──
got=$(as_mate wait --any --parent "$mate" --timeout "${WAIT:-180}")
grep -q "CREW-PONG" <<<"$got" || fail "wait did not return the crew's reply: $got"
grep -q "run.*$crew\|$crew" <<<"$got" || fail "wait did not name $crew"
again=$(as_mate wait --any --parent "$mate" --timeout 2)
grep -q 'events\[0\]' <<<"$again" || fail "a second wait returned the reply again: $again"
ok "wait --any --parent returned the crew's reply once"

# ── the crew's tool call goes to its mate ──
as_mate send "$crew" "Run this exact bash command with the Bash tool: touch approved.txt. Then reply with exactly: DONE-TOUCH" >/dev/null
got=$(as_mate wait --any --parent "$mate" --timeout "${WAIT:-180}")
perm=$(grep -o 'p[0-9]\+' <<<"$got" | head -1)
[[ -n $perm ]] && grep -q needs <<<"$got" || fail "wait did not return a needs with a perm: $got"
"$bin" approve "$crew" "$perm" >/dev/null 2>&1 && fail "you approved the mate's crew call"
as_mate approve "$crew" "$perm" >/dev/null
got=$(as_mate wait --any --parent "$mate" --timeout "${WAIT:-180}")
grep -q DONE-TOUCH <<<"$got" || fail "no reply after the mate approved: $got"
[[ -f $("$bin" show "$crew" | field cwd)/approved.txt ]] || fail "approved.txt was not created"
ok "crew needs $perm went to the mate: you were refused, the mate approved, the call ran"

# ── the goal graph ──
cat >"$tmp/goal.json" <<'JSON'
{"goal":"touch a file","done_when":"approved.txt exists",
 "nodes":[{"id":"touch","task":"touch approved.txt","check":"ls approved.txt","model":"haiku"},
          {"id":"docs","task":"say so","check":"true","review":"code"}],
 "edges":[["touch","docs"]]}
JSON
as_mate goal set --file "$tmp/goal.json" >/dev/null
as_mate goal node touch --status pass --round 2 --crew "$crew" >/dev/null
"$bin" show "$mate" | grep -q "touch,touch approved.txt,ls approved.txt,haiku,none,pass,2,$crew" || fail "show $mate: the node update is not in the goal"
node=$(timeout 1 "$bin" watch --since 0 | jq -c 'select(.kind == "goal") | .data.graph.nodes[] | select(.id == "touch") | [.status, .round, .crew]' | tail -1 || true)
[[ $node == "[\"pass\",2,\"$crew\"]" ]] || fail "the goal event does not carry the node update: $node"
as_mate goal node nope --status pass >/dev/null 2>&1 && fail "a node that is not in the graph was accepted"
ok "goal set and goal node round-trip through show and the goal event"

# ── escalate ──
as_mate escalate --text - <<<"which branch should the docs go on?" >/dev/null
home=$("$bin")
grep -q "$mate" <<<"$home" && grep -q 'docs go on' <<<"$home" || fail "home does not show the escalation: $home"
[[ $("$bin" ls --phase needs | grep -c "$mate") == 1 ]] || fail "ls does not list $mate as needs"
ok "escalate shows in drover and in ls as needing you"
"$bin" send "$mate" "Reply with exactly: ANSWERED" >/dev/null || fail "the answer to the escalation was refused"
for _ in $(seq 120); do [[ $("$bin" show "$mate" | field phase) == idle ]] && break; sleep 1; done
[[ $("$bin" show "$mate" | field phase) == idle ]] || fail "mate $mate: not idle after the answer"
ok "your send answered it, and the mate went on"

echo "all checks passed"

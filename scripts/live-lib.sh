# shellcheck shell=bash
# Sourced by the live scripts. Each run keeps its temp dir `<prefix>.XXXX`, with the script's pid
# in `owner`. A run killed without its EXIT trap (SIGKILL) leaves its serve and its herdr
# workspaces; the next run removes them, except from a dir whose owner is still running.

# $1 prefix, $2 the herdr command for the machine the workspaces are on
live_sweep() {
  local prefix=$1 herdr=$2 dir pid p
  for dir in "$prefix".*; do
    [[ -d $dir ]] || continue
    pid=$(cat "$dir/owner" 2>/dev/null || true)
    if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then continue; fi
    for p in $(live_pids "$dir/state"); do kill "$p" 2>/dev/null || true; done
    for _ in $(seq 50); do [[ -n $(live_pids "$dir/state") ]] || break; sleep 0.1; done
    for p in $(live_pids "$dir/state"); do kill -KILL "$p" 2>/dev/null || true; done
    live_close_workspaces "$dir" "$herdr"
    rm -rf "$dir"
  done
}

# Closes the herdr workspaces whose checkout is under $1.
live_close_workspaces() {
  local dir=$1 herdr=$2
  "$herdr" workspace list 2>/dev/null | jq -r --arg t "$dir/" '.result.workspaces[] | select((.worktree.checkout_path // "") | startswith($t)) | .workspace_id' |
    while read -r ws; do "$herdr" workspace close "$ws" >/dev/null 2>&1 || true; done
  return 0
}

# The pids whose environment has DROVER_STATE_DIR=$1.
live_pids() {
  local e
  for e in /proc/[0-9]*/environ; do
    if tr "\0" "\n" 2>/dev/null <"$e" | grep -qxF "DROVER_STATE_DIR=$1"; then
      e=${e#/proc/}
      echo "${e%/environ}"
    fi
  done
  return 0
}

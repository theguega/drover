#!/bin/sh
# Install drover from nothing:
#   curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh
# Pass --remote to only set up a remote machine:  ... | sh -s -- --remote
# Clones into $DROVER_HOME (default ~/.local/share/drover), then runs its install.sh.
set -eu

home=${DROVER_HOME:-$HOME/.local/share/drover}
repo=${DROVER_REPO:-https://github.com/theguega/drover.git}

command -v git >/dev/null || { echo "drover needs git" >&2; exit 1; }

if [ -d "$home/.git" ]; then
  git -C "$home" pull --ff-only -q
else
  mkdir -p "$(dirname "$home")"
  git clone -q "$repo" "$home"
fi

exec "$home/install.sh" "$@"

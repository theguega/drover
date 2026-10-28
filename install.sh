#!/usr/bin/env bash
# Install drover from this checkout. Rerun after a pull. Skills are copied by hand, see README.
#   ./install.sh           host: write ~/.config/drover/config.toml, build, link the tools, run the service
#   ./install.sh --remote  remote: link drover-wt and the client only; no service, no config
# The desk's own config (keys, aliases, REPOS_ROOT) stays in your dotfiles.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
remote=0
[[ "${1:-}" == --remote ]] && remote=1
bin=~/.local/bin
mkdir -p "$bin"

for cmd in git jq herdr ssh; do
  command -v "$cmd" >/dev/null || echo "warning: $cmd is not on PATH" >&2
done

ln -sfn "$root/bin/drover-wt" "$bin/drover-wt"

# ask on the terminal, so this also works when the script arrives through curl
ask() {
  local value default=${2:-}
  read -rp "$1${default:+ [$default]}: " value </dev/tty
  echo "${value:-$default}"
}

default_repos=$HOME/.drover-workspace

if ((remote)); then
  repos=$default_repos
  [[ -r /dev/tty ]] && repos=$(ask "REPOS_ROOT: where repositories and worktrees live" "$default_repos")
  mkdir -p "${repos/#\~/$HOME}"
  ln -sfn "$root/bin/drover-ssh" "$bin/drover"
  echo "linked drover-wt and drover (set DROVER_HOST to the host's ssh name)"
  [[ "$repos" == "$default_repos" ]] || echo "set REPOS_ROOT=$repos in your shell environment"
  echo "this machine needs herdr, python3 and ssh from the host"
  exit 0
fi

config=~/.config/drover/config.toml
if [[ ! -f "$config" ]]; then
  [[ -r /dev/tty ]] || { echo "no $config: write it by hand, see README, Configuration" >&2; exit 1; }
  host=$(ask "this machine's name, as herdr's machine label" "$(hostname -s)")
  remotes=$(ask "remotes (ssh names, space-separated, empty for none)")
  repos=$(ask "where repositories and worktrees live" "$default_repos")
  mkdir -p "${repos/#\~/$HOME}" "$(dirname "$config")"
  list=
  for r in $remotes; do list+="\"$r\", "; done
  {
    echo "host = \"$host\""
    echo "remotes = [${list%, }]"
    echo "repos_root = \"$repos\""
    # a v1 checkout's tasks become runs on the first start
    if [[ -f "$root/state.json" ]]; then echo "v1_state = \"$root/state.json\""; fi
  } >"$config"
  echo "wrote $config"
fi

command -v cargo >/dev/null || { echo "drover is built with cargo: install Rust from https://rustup.rs" >&2; exit 1; }
cargo build --release --manifest-path "$root/Cargo.toml"
ln -sfn "$root/target/release/drover" "$bin/drover"

zsh=$(command -v zsh || true)
[[ -n "$zsh" ]] || { echo "the service starts through zsh: install it first" >&2; exit 1; }
# zsh -c, so the login dotfiles' PATH and REPOS_ROOT apply
run="exec ./target/release/drover serve"

case "$(uname -s)" in
  Linux)
    mkdir -p ~/.config/systemd/user
    cat >~/.config/systemd/user/drover.service <<UNIT
[Unit]
Description=drover, agent runtime for herdr
After=network-online.target

[Service]
WorkingDirectory=$root
ExecStart=$zsh -c '$run'
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
UNIT
    loginctl enable-linger "$USER"
    systemctl --user daemon-reload
    systemctl --user enable -q drover
    systemctl --user restart drover
    echo "logs: journalctl --user -u drover -f"
    ;;
  Darwin)
    plist=~/Library/LaunchAgents/dev.drover.plist
    mkdir -p ~/Library/LaunchAgents
    cat >"$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.drover</string>
  <key>WorkingDirectory</key><string>$root</string>
  <key>ProgramArguments</key><array><string>$zsh</string><string>-c</string><string>$run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$root/drover.log</string>
  <key>StandardErrorPath</key><string>$root/drover.log</string>
</dict>
</plist>
PLIST
    launchctl bootout "gui/$(id -u)/dev.drover" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$plist"
    echo "logs: tail -f $root/drover.log"
    ;;
esac

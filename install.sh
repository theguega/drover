#!/usr/bin/env bash
# Install drover from this checkout. Rerun after a pull. Skills are copied by hand, see README.
#   ./install.sh           host: ask for .env, build, link the tools, run the service
#   ./install.sh --remote  remote: link drover-wt only; no service, no .env
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

mkdir -p ~/.local/share/bash-completion/completions ~/.local/share/zsh/site-functions
ln -sfn "$root/completions/drover.bash" ~/.local/share/bash-completion/completions/drover
ln -sfn "$root/completions/_drover" ~/.local/share/zsh/site-functions/_drover
echo "zsh: put \`fpath=(~/.local/share/zsh/site-functions \$fpath)\` before compinit in .zshrc"

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
  echo "linked drover-wt and completions"
  [[ "$repos" == "$default_repos" ]] || echo "set REPOS_ROOT=$repos in your shell environment"
  echo "this machine needs herdr, python3 and ssh from the host"
  exit 0
fi

if [[ ! -f "$root/.env" ]]; then
  [[ -r /dev/tty ]] || { echo "no .env: copy .env.example to .env and fill it, then rerun" >&2; exit 1; }
  echo "drover needs a Discord bot, a server and a channel. See README.md, Discord setup."
  token=
  read -rsp "DISCORD_TOKEN: " token </dev/tty; echo
  guild=$(ask DISCORD_GUILD_ID)
  channel=$(ask DISCORD_CHANNEL_ID)
  allowed=$(ask "ALLOWED_USER_IDS (comma-separated)")
  host=$(ask HOST_NAME "$(hostname -s)")
  remotes=$(ask "REMOTES (ssh names, comma-separated, empty for none)")
  repos=$(ask "REPOS_ROOT: where repositories and worktrees live" "$default_repos")
  repos=${repos/#\~/$HOME}
  mkdir -p "${repos/#\~/$HOME}"
  umask 077
  cat >"$root/.env" <<ENV
DISCORD_TOKEN=$token
DISCORD_GUILD_ID=$guild
DISCORD_CHANNEL_ID=$channel
ALLOWED_USER_IDS=$allowed
HOST_NAME=$host
REMOTES=$remotes
REPOS_ROOT=$repos
ENV
  echo "wrote $root/.env"
fi

command -v cargo >/dev/null || { echo "drover is built with cargo: install Rust from https://rustup.rs" >&2; exit 1; }
cargo build --release --manifest-path "$root/Cargo.toml"
ln -sfn "$root/target/release/drover" "$bin/drover"

zsh=$(command -v zsh || true)
[[ -n "$zsh" ]] || { echo "the service starts through zsh: install it first" >&2; exit 1; }
run="set -a; . ./.env; set +a; exec ./target/release/drover serve"

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

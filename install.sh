#!/usr/bin/env bash
# Build drover and run it as a user service from this checkout. Rerun after a pull.
# herdr-wt, PATH, REPOS_ROOT and HERDR_WORKTREE_PREFIX come from the dotfiles: the service starts through zsh.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
zsh=$(command -v zsh)
[[ -f "$root/.env" ]] || { cp "$root/.env.example" "$root/.env"; echo "fill $root/.env, then rerun"; exit 1; }
cargo build --release --manifest-path "$root/Cargo.toml"
run="set -a; . ./.env; set +a; exec ./target/release/drover"

case "$(uname -s)" in
  Linux)
    mkdir -p ~/.config/systemd/user
    cat >~/.config/systemd/user/drover.service <<UNIT
[Unit]
Description=drover, Discord bridge for herdr
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

#!/usr/bin/env bash
# Install drover: clone (or update), build, put herdr-wt on PATH, seed .env.
#   curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/install.sh | bash
set -euo pipefail

REPO="${DROVER_REPO:-https://github.com/theguega/drover.git}"
REF="${DROVER_REF:-main}"
ROOT="${DROVER_HOME:-}"
if [[ -z "$ROOT" ]]; then
  repos="${REPOS_ROOT:-$HOME/Developer}"
  repos="${repos/#\~/$HOME}"
  ROOT="$repos/drover"
fi
ROOT="${ROOT/#\~/$HOME}"

need() {
  command -v "$1" >/dev/null || {
    echo "missing on PATH: $1" >&2
    exit 1
  }
}

echo "drover → $ROOT"
need git
need cargo
need herdr
need jq
need ssh

if [[ -d "$ROOT/.git" ]]; then
  git -C "$ROOT" fetch -q origin
  git -C "$ROOT" checkout -q "$REF"
  git -C "$ROOT" pull -q --ff-only origin "$REF" || true
else
  mkdir -p "$(dirname "$ROOT")"
  git clone -q --branch "$REF" "$REPO" "$ROOT"
fi

cargo build --release --manifest-path "$ROOT/Cargo.toml"

mkdir -p "$HOME/.local/bin"
ln -sfn "$ROOT/bin/herdr-wt" "$HOME/.local/bin/herdr-wt"

if [[ ! -f "$ROOT/.env" ]]; then
  cp "$ROOT/.env.example" "$ROOT/.env"
  echo "wrote $ROOT/.env — fill DISCORD_* and ALLOWED_USER_IDS"
fi

case "$(uname -s)" in
  Linux)
    unit_dir="$HOME/.config/systemd/user"
    mkdir -p "$unit_dir"
    cat >"$unit_dir/drover.service" <<EOF
[Unit]
Description=drover, Discord bridge for herdr
After=network-online.target

[Service]
WorkingDirectory=$ROOT
ExecStart=$ROOT/target/release/drover
Environment=PATH=$HOME/.local/bin:/home/linuxbrew/.linuxbrew/bin:/opt/homebrew/bin:/usr/bin:/bin
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload
    echo "systemd unit → $unit_dir/drover.service"
    echo "after .env:  systemctl --user enable --now drover"
    ;;
  Darwin)
    plist="$HOME/Library/LaunchAgents/dev.drover.plist"
    mkdir -p "$HOME/Library/LaunchAgents" "$ROOT/logs"
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.drover</string>
  <key>WorkingDirectory</key><string>$ROOT</string>
  <key>ProgramArguments</key>
  <array>
    <string>$ROOT/target/release/drover</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$HOME/.local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
    <key>HOME</key><string>$HOME</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$ROOT/logs/drover.log</string>
  <key>StandardErrorPath</key><string>$ROOT/logs/drover.log</string>
</dict>
</plist>
EOF
    echo "launchd plist → $plist"
    echo "after .env:  launchctl bootstrap gui/\$(id -u) $plist"
    ;;
esac

echo
echo "next:"
echo "  1. edit $ROOT/.env"
echo "  2. merge $ROOT/desk/herdr.toml into ~/.config/herdr/config.toml"
echo "  3. start the service (see above)"
echo "  herdr-wt → $HOME/.local/bin/herdr-wt"
echo "  binary   → $ROOT/target/release/drover"

#!/bin/bash
set -euo pipefail

COORDINATOR_PUBKEY="ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICdbEiNQXhR+Jw/LBKTK0FNE1gr2gNUPD5IbaFn6zjLw fabian.boesiger@ajila.com"

echo "==> Installing Rust..."
if command -v rustup &>/dev/null; then
    echo "    Rust already installed, updating..."
    rustup update
else
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

echo "==> Installing cargo-nextest..."
cargo install cargo-nextest --locked

echo "==> Installing partest..."
if [ -d /tmp/partest-install ]; then
    rm -rf /tmp/partest-install
fi
git clone https://github.com/fabianboesiger/partest.git /tmp/partest-install
cargo install --path /tmp/partest-install
rm -rf /tmp/partest-install

echo "==> Enabling Remote Login (SSH)..."
if sudo systemsetup -getremotelogin | grep -q "On"; then
    echo "    Remote Login already enabled."
else
    sudo systemsetup -setremotelogin on
fi

echo "==> Adding coordinator public key to authorized_keys..."
mkdir -p ~/.ssh
chmod 700 ~/.ssh
touch ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
if grep -qF "$COORDINATOR_PUBKEY" ~/.ssh/authorized_keys; then
    echo "    Key already present."
else
    echo "$COORDINATOR_PUBKEY" >> ~/.ssh/authorized_keys
    echo "    Key added."
fi

echo "==> Installing partest daemon as a background service..."
cat > ~/Library/LaunchAgents/com.partest.daemon.plist << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.partest.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>sh</string>
        <string>-lc</string>
        <string>\$HOME/.cargo/bin/partest daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/partest.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/partest.log</string>
</dict>
</plist>
EOF

launchctl unload ~/Library/LaunchAgents/com.partest.daemon.plist 2>/dev/null || true
launchctl load ~/Library/LaunchAgents/com.partest.daemon.plist

echo ""
echo "==> Done! partest daemon is running as a background service."
echo "    Logs: /tmp/partest.log"

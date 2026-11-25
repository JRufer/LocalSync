#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
CONFIG_DIR="$HOME/.config/localsync"
DATA_DIR="$HOME/.local/share/localsync"
SYNC_ROOT="$HOME/DriveShares"
CONFIG_FILE="$CONFIG_DIR/config.toml"
SHARES_CONF="$CONFIG_DIR/shares-syncd.conf"

echo "=== Installing LocalSync (Drive <-> SMB3 System Tray & Background Service) ==="

# 1. Detect Tailscale IPv4 (fallback to 127.0.0.1 if Tailscale is not connected yet)
TS_IP="127.0.0.1"
if command -v tailscale >/dev/null 2>&1; then
    DETECTED_IP="$(tailscale ip -4 2>/dev/null | head -n1 || true)"
    if [[ -n "$DETECTED_IP" ]]; then
        TS_IP="$DETECTED_IP"
    fi
fi
echo "-> Detected Tailscale / Bind IP: $TS_IP"

# 2. Build optimized release binaries
echo "-> Building release binaries (LTO optimized)..."
cd "$REPO_DIR"
cargo build --release --workspace

mkdir -p "$BIN_DIR" "$CONFIG_DIR" "$DATA_DIR" "$SYNC_ROOT/Today"
install -Dm755 target/release/drive-syncd "$BIN_DIR/drive-syncd"
install -Dm755 target/release/drivectl "$BIN_DIR/drivectl"
echo "-> Installed binaries to $BIN_DIR/drive-syncd and $BIN_DIR/drivectl"

# 3. Create default user config if not already present
if [[ ! -f "$CONFIG_FILE" ]]; then
    cat > "$CONFIG_FILE" <<EOF
# LocalSync User Configuration
root_folder_id = "root"
local_root = "$SYNC_ROOT"
db_path = "$DATA_DIR/state.db"
tailscale_ip = "$TS_IP"
web_port = 8420
trash_on_delete = true
poll_interval_secs = 15
debounce_ms = 500
max_concurrent_transfers = 8

[oauth]
client_id = "YOUR_GOOGLE_OAUTH_CLIENT_ID.apps.googleusercontent.com"
client_secret = "YOUR_GOOGLE_OAUTH_CLIENT_SECRET"
token_path = "$DATA_DIR/token.json"
redirect_port = 8899

[samba]
shares_conf_path = "$SHARES_CONF"
testparm_bin = "testparm"
smbcontrol_bin = "smbcontrol"
macos_fruit = true
create_mask = "0664"
directory_mask = "0775"
force_user = "$USER"

[[shares]]
share_name = "today"
rel_path = "Today"
read_only = false
comment = "Today folder synced with Google Drive"
EOF
    chmod 0600 "$CONFIG_FILE"
    echo "-> Created configuration at $CONFIG_FILE"
else
    echo "-> Keeping existing configuration at $CONFIG_FILE"
fi

touch "$SHARES_CONF"

# 4. Optional system-wide Samba hookup (if user has sudo and Samba installed)
if [[ "${1:-}" == "--with-system-samba" ]]; then
    echo "-> Configuring /etc/samba/shares-syncd.conf and sudoers rule..."
    sudo ln -sf "$SHARES_CONF" /etc/samba/shares-syncd.conf
    sudo install -Dm440 "$REPO_DIR/deploy/polkit-or-sudoers.example" /etc/sudoers.d/drive-syncd
    if ! grep -q "shares-syncd.conf" /etc/samba/smb.conf 2>/dev/null; then
        echo "   NOTE: Add 'include = /etc/samba/shares-syncd.conf' to [global] in /etc/samba/smb.conf"
    fi
fi

# 5. Install systemd --user service for drive-syncd
mkdir -p "$HOME/.config/systemd/user"
install -Dm644 "$REPO_DIR/deploy/drive-syncd-user.service" "$HOME/.config/systemd/user/drive-syncd.service"
systemctl --user daemon-reload
systemctl --user enable --now drive-syncd.service || true

# 6. Install Desktop App Launcher & KDE/XDG Autostart Entry for System Tray
mkdir -p "$HOME/.local/share/applications" "$HOME/.config/autostart"
install -Dm644 "$REPO_DIR/deploy/localsync-tray.desktop" "$HOME/.local/share/applications/localsync-tray.desktop"
install -Dm644 "$REPO_DIR/deploy/localsync-tray.desktop" "$HOME/.config/autostart/localsync-tray.desktop"

# 7. Launch the System Tray icon immediately in the background
pkill -f "drivectl.*tray" 2>/dev/null || true
nohup "$BIN_DIR/drivectl" --config "$CONFIG_FILE" tray >/dev/null 2>&1 &

echo ""
echo "=================================================================="
echo "✅ LocalSync is installed and running in your KDE System Tray!"
echo "=================================================================="
echo "• Look for the cloud-folder icon in your system tray:"
echo "  - Left-click (or select 'Add / Edit Shares') to open the visual Share Manager"
echo "    at http://$TS_IP:8420/"
echo "  - Right-click the tray icon to quick-toggle folder shares, switch Read-Only"
echo "    vs Read/Write, open the Terminal TUI, or trigger 'Sync Now'."
echo "• Synced folders live in: $SYNC_ROOT"
echo "• Config file: $CONFIG_FILE"
echo "  (To hook into system smbd reloads without password prompts, run:"
echo "   ./install.sh --with-system-samba)"

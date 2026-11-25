# LocalSync (`drive-smb-sync`)

Bidirectional **Google Drive API v3** ↔ **Samba (SMB3) over Tailscale** synchronization engine, folder tree browser (`ratatui` TUI + Tailscale-bound `axum` Web UI), and atomic Samba share manager targeting **CachyOS Linux** with cross-platform read/write support for **Linux, Android, macOS, and Windows** (~10,000 files × ~2 MB, ~20 GB).

---

## Workspace Architecture

```text
.
├── Cargo.toml
├── config.toml.example
├── crates/
│   ├── sync-core/       # TOML config, SQLite WAL state DB, OAuth2 0600 store, Drive v3 client, sync engine, notify watcher
│   ├── share-smb/       # Share name sanitization, nested/enclosing overlap detection, atomic shares-syncd.conf writer, testparm & smbcontrol reloader
│   ├── share-ui/        # Interactive Ratatui + tui-tree-widget TUI & Tailscale-only Axum Web UI
│   ├── drive-syncd/     # Headless systemd daemon (bootstrap, Changes.list delta poll, notify debounce, SIGHUP reload)
│   └── drivectl/        # CLI (`ui`, `web`, `status`, `share add/rm/list`, `sync`, `auth`)
└── deploy/
    ├── drive-syncd.service
    ├── smb-shares-syncd.conf.example
    └── polkit-or-sudoers.example
```

---

## Key Capabilities

1. **Atomic Local Writes:** Remote Google Drive downloads stream into a hidden sibling temporary file (`.tmp.<uuid>.<name>`), verify MD5 and compute BLAKE3 hashes, `fsync`, and atomically `rename()` to the final path so SMB3 clients never observe half-written files.
2. **Echo Suppression:**
   - Local → Drive uploads set `suppress_remote_until_ms = now + 5000` so the subsequent `Changes.list` delta event is ignored.
   - Drive → Local downloads/deletes set `suppress_local_until_ms = now + 2000` so `notify` filesystem watcher events caused by the daemon's own disk writes do not re-upload.
3. **Zero-Data-Loss Conflict Resolution:** Concurrent divergent edits on both Google Drive and an SMB3 client preserve the newest version at `rel_path` and save the divergent copy to `<stem>.conflict-<unix_epoch_secs>.<ext>` in the same folder, syncing both across all devices.
4. **Sub-Millisecond 10,000-File Tree Navigation:** `StateDb` uses SQLite WAL mode with indexed parent lookups and bottom-up rollup queries to compute descendant file counts, folder counts, byte totals, conflict badges, and active SMB3 share badges.
5. **Tailscale-Only SMB3 & Web Binding:** `AppConfig` enforces that `tailscale_ip` belongs to the Tailscale CGNAT range (`100.64.0.0/10`, or `127.0.0.1` for local testing).

---

## Quick Start on CachyOS Linux

### 1. Build & Install Binaries

```bash
cargo build --release --workspace
sudo install -Dm755 target/release/drive-syncd /usr/local/bin/drive-syncd
sudo install -Dm755 target/release/drivectl /usr/local/bin/drivectl
```

### 2. Configure Samba, Privilege Separation, and `config.toml`

1. Copy `config.toml.example` to `/etc/drive-smb-sync/config.toml` and set your `root_folder_id`, `tailscale_ip`, and Google `[oauth]` credentials.
2. Include `/etc/samba/shares-syncd.conf` in `/etc/samba/smb.conf` (see `deploy/smb-shares-syncd.conf.example`).
3. Install `/etc/sudoers.d/drive-syncd` from `deploy/polkit-or-sudoers.example` and enable the systemd service:

```bash
drivectl --config /etc/drive-smb-sync/config.toml auth
sudo cp deploy/drive-syncd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now drive-syncd.service
```

### 3. Browse Folders & Export SMB3 Shares

```bash
# Interactive terminal folder tree browser
drivectl ui

# Or Tailscale-bound web UI (http://100.x.y.z:8420)
drivectl web

# Or manage shares directly from the CLI
drivectl share add "Today" --name today
drivectl share list
drivectl status
```

### 4. Mounting Shares Across Client OSes Over Tailscale

- **Linux (CIFS / GVFS):**
  ```bash
  sudo mount -t cifs //100.64.0.10/today /mnt/today -o vers=3.1.1,username=drivesync,uid=$(id -u),gid=$(id -g)
  ```
- **Windows 11:**
  ```powershell
  net use Z: \\100.64.0.10\today /persistent:yes
  ```
- **macOS Finder:**
  Press `Cmd+K` in Finder and enter `smb://100.64.0.10/today` (`vfs_fruit` and `streams_xattr` are enabled automatically).
- **Android:**
  In Material Files, Solid Explorer, or Cx File Explorer, add an SMBv3 connection to `100.64.0.10` / share `today`.

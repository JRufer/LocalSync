# Building a High-Speed LocalSync Client (SMB3 + Tailscale + SQLite Index API)

This guide covers how to architect and build a **lightning-fast client** (for **Linux, Android, macOS, Windows, or cross-platform Rust / Flutter / Kotlin / Swift**) that connects to a **LocalSync (`drive-syncd`)** host over **Tailscale** and browses ~10,000 files (~2 MB each, ~20 GB total) with sub-millisecond folder navigation and zero UI stutter.

---

## 1. Architecture Overview: Why Hybrid Metadata + SMB3 I/O is Fastest

Pure SMB3 directory enumeration (`SMB2_QUERY_DIRECTORY` across hundreds of folders over a WireGuard/Tailscale tunnel) incurs a network round-trip per directory opened, plus extra `SMB2_CREATE` / `SMB2_QUERY_INFO` calls if a client probes thumbnails or subfolder counts lazily.

**LocalSync** exposes two complementary interfaces bound strictly to the host's Tailscale IP (`100.x.y.z`):

1. **Instant Metadata & Tree Index Plane (`http://100.x.y.z:8420/api/tree`)**
   - Backed by the host's in-memory-mapped SQLite WAL database (`state.db`).
   - Returns the entire folder hierarchy, pre-computed recursive file counts, byte totals, BLAKE3/MD5 state, conflict counts, and active SMB3 share mappings in a **single ~5–15 KB JSON response (< 2 ms)**.
2. **High-Throughput File Data Plane (`smb://100.x.y.z/<share_name>`)**
   - Backed by **Samba SMB 3.1.1** over Tailscale with `vfs_fruit` and atomic temp-file writes (`.tmp.<uuid>.<name>` → `rename()`).
   - Used for streaming 2 MB file payloads, range reads, and writing edits back to the host.

```text
+-------------------------------------------------------------------------+
|                        Custom Client Application                        |
|                                                                         |
|  +-------------------------------+   +-------------------------------+  |
|  | 1. Instant Tree & Search Cache|   | 2. Pipelined SMB3 Reader/Writer| |
|  |  - Fetches /api/tree on open  |   |  - Persistent SMB 3.1.1 session| |
|  |  - 0ms local search & expand  |   |  - 64 KiB–1 MiB buffered I/O   | |
|  |  - On-demand share creation   |   |  - Temp-file + rename writes   | |
|  +---------------+---------------+   +---------------+---------------+  |
+------------------|-----------------------------------|------------------+
                   | HTTP GET/POST (:8420)             | SMB 3.1.1 (:445)
                   | over Tailscale (100.x.y.z)        | over Tailscale
+------------------v-----------------------------------v------------------+
|                     CachyOS LocalSync Host (100.x.y.z)                  |
|   [axum Web API + SQLite WAL Index]        [smbd + /srv/drive-images]   |
+-------------------------------------------------------------------------+
```

---

## 2. Step 1: Instant Folder Tree & On-Demand Share Creation via REST API

Before mounting or opening an SMB share, query `http://<tailscale_ip>:8420/api/tree` to populate your client's navigation tree in a single request.

### `GET http://100.x.y.z:8420/api/tree`

Returns:

```json
{
  "tailscale_ip": "100.124.79.120",
  "local_root": "/home/jrufer/DriveShares",
  "root_folder_id": "1A2B3C...",
  "oauth_configured": true,
  "summary": {
    "total_files": 10000,
    "total_dirs": 110,
    "total_bytes": 20971520000,
    "synced_count": 10000,
    "pending_upload_count": 0,
    "pending_download_count": 0,
    "pending_delete_count": 0,
    "conflict_count": 0,
    "active_shares_count": 1,
    "last_full_scan_at": 1764032000000,
    "last_delta_sync_at": 1764032800000
  },
  "folders": [
    {
      "rel_path": "Today",
      "name": "Today",
      "subfolder_count": 4,
      "file_count": 320,
      "total_bytes": 671088640,
      "human_size": "640.0 MiB",
      "conflict_count": 0,
      "pending_count": 0,
      "share_name": "today",
      "share_read_only": false,
      "suggested_share_name": "today"
    }
  ],
  "shares": [
    {
      "share_name": "today",
      "rel_path": "Today",
      "read_only": false,
      "created_at": 1764032000000
    }
  ],
  "events": []
}
```

### Dynamically Exporting Any Folder as an SMB3 Share from Your Client

If the user clicks a folder in your client that isn't shared yet (`"share_name": null`), your client can export it on the fly and immediately connect to it over SMB3:

```http
POST http://100.x.y.z:8420/api/shares/add
Content-Type: application/json

{
  "rel_path": "2026/Vacation",
  "share_name": "vacation",
  "read_only": false
}
```

The host atomically updates `/etc/samba/shares-syncd.conf`, validates with `testparm -s`, and runs `smbcontrol smbd reload-config` in < 30 ms without dropping existing SMB3 connections. Your client can immediately connect to `\\100.x.y.z\vacation`.

---

## 3. Step 2: SMB3 Connection & Directory Browsing Best Practices

When connecting to `\\100.x.y.z\<share_name>` over Tailscale to list individual files and stream 2 MB payloads, apply these six rules for maximum speed:

### Rule 1: Negotiate SMB 3.1.1 with Large MTU (`large_mtu`)
- Always request **SMB 3.02 or SMB 3.1.1**.
- Enable **Large MTU / Multi-Credit requests** so a single 2 MB file read completes in 1–2 pipelined SMB2 `READ` requests (up to 1 MiB–8 MiB per credit) instead of 32 sequential 64 KiB round-trips.

### Rule 2: Use Compound `SMB2_CREATE` + `SMB2_QUERY_DIRECTORY` + `SMB2_CLOSE`
- When listing a folder's files over SMB3, issue a **compound PDU** (Open directory → Query Directory with wildcard `*` and `FileIdBothDirectoryInformation` → Close handle) in a **single network round-trip**.
- Request a 64 KiB output buffer per `SMB2_QUERY_DIRECTORY` call; this returns ~250–400 file entries (name, size, mtime, attributes) in a single packet.
- **Never `stat()` (`SMB2_QUERY_INFO`) each file individually while rendering a directory list.** Everything needed to render a file grid/list (filename, byte size, creation/modification timestamps, directory flag) is already inside the `SMB2_QUERY_DIRECTORY` response buffer.

### Rule 3: Filter Temporary & Conflict Files in the Client UI
- Ignore any entry starting with `.tmp.` (though LocalSync's atomic `rename()` ensures partial files are never visible under their final name, hiding `.tmp.*` prevents temporary staging files from flickering in a live view).
- Highlight files matching `*.conflict-<epoch>*` with a warning badge so the user can review or merge concurrent edits.

### Rule 4: Keep One Persistent SMB3 Session Pool & Bounded Prefetch Queue
- Establishing a new TCP + SMB3 session setup (`NEGOTIATE` → `SESSION_SETUP` → `TREE_CONNECT`) takes 3–4 round-trips. Keep the `TREE_CONNECT` handle open for the lifetime of the app session.
- For an image grid of 2 MB JPEGs/WebPs, decode thumbnails using a bounded worker pool (e.g., **4–6 parallel readers** sharing the persistent SMB3 session or local kernel mount) and cancel in-flight reads when a user scrolls past a viewport.

### Rule 5: Write Files Atomically from the Client
- When saving an edited 2 MB file back to the SMB3 share from a client, write to `.tmp.<random>.<filename>` first, flush, and rename to `<filename>`.
- Because LocalSync's `notify` watcher ignores `.tmp.*` paths (see `is_ignored_temp_path` in `crates/sync-core/src/watcher.rs`), the host daemon will wait until the client finishes writing and renames the file before uploading the complete file to Google Drive.

### Rule 6: Avoid Redundant Encryption Overhead Over Tailscale
- Tailscale already encrypts all traffic end-to-end with WireGuard (ChaCha20-Poly1305).
- In LocalSync's `smb.conf`, `smb encrypt = desired` (or `default`) allows clients to skip double-encryption (`seal`) if desired while still enforcing SMB 3.1.1 signing/authentication. On Linux `mount.cifs`, omitting `seal` while over the `100.x.y.z` Tailscale interface avoids double-encrypting every 2 MB payload.

---

## 4. Reference Client Implementations

### A. Linux Native Mount Client (`mount.cifs` Tuned for Tailscale + 2 MB Files)

For Linux desktop/CLI clients, mounting via the kernel's `cifs.ko` driver gives the fastest possible local VFS speed when tuned with metadata caching (`actimeo=5`) and 1 MiB read/write buffers:

```bash
sudo mkdir -p /mnt/localsync-today
sudo mount -t cifs //100.124.79.120/today /mnt/localsync-today \
  -o vers=3.1.1,username=jrufer,uid=$(id -u),gid=$(id -g),rsize=1048576,wsize=1048576,actimeo=5,echo_interval=15,nostrictsync,cache=strict
```

*Why these flags make browsing instantaneous:*
- `vers=3.1.1`: Uses SMB 3.1.1 with multichannel/multi-credit support.
- `rsize=1048576,wsize=1048576`: Reads and writes 2 MB files in just two 1 MiB chunks.
- `actimeo=5`: Caches directory and file attributes in the client kernel for 5 seconds so rapid `ls` / file-picker navigation does zero redundant network lookups.
- `cache=strict`: Uses SMB3 leases (oplocks) so repeated reads of a 2 MB file are served directly from the client's RAM page cache until another client modifies the file.

---

### B. Rust Reference Client (Hybrid REST Tree + Async Concurrent File Browser)

Below is a minimal, self-contained Rust client pattern that fetches the instant folder tree from LocalSync's API, ensures the target folder is shared via SMB3, and reads directory entries and 2 MB files with zero blocking:

```rust
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Deserialize)]
pub struct TreeApiResponse {
    pub tailscale_ip: String,
    pub folders: Vec<RemoteFolder>,
}

#[derive(Debug, Deserialize)]
pub struct RemoteFolder {
    pub rel_path: String,
    pub name: String,
    pub file_count: usize,
    pub subfolder_count: usize,
    pub human_size: String,
    pub share_name: Option<String>,
    pub suggested_share_name: String,
}

#[derive(Debug, Serialize)]
struct AddShareReq<'a> {
    rel_path: &'a str,
    share_name: &'a str,
    read_only: bool,
}

pub struct LocalSyncClient {
    http: Client,
    base_url: String,
}

impl LocalSyncClient {
    pub fn new(tailscale_ip: &str, web_port: u16) -> Result<Self> {
        let http = Client::builder()
            .pool_max_idle_per_host(8)
            .tcp_nodelay(true)
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base_url: format!("http://{tailscale_ip}:{web_port}"),
        })
    }

    /// 1. Fetch the entire 10,000-file folder tree and stats in a single <2ms call.
    pub async fn fetch_folder_tree(&self) -> Result<TreeApiResponse> {
        let url = format!("{}/api/tree", self.base_url);
        let tree = self.http.get(&url).send().await?.json::<TreeApiResponse>().await?;
        Ok(tree)
    }

    /// 2. Ensure a folder is exported as an SMB3 share and return its share name.
    pub async fn ensure_folder_shared(&self, folder: &RemoteFolder) -> Result<String> {
        if let Some(ref active) = folder.share_name {
            return Ok(active.clone());
        }
        let url = format!("{}/api/shares/add", self.base_url);
        self.http
            .post(&url)
            .json(&AddShareReq {
                rel_path: &folder.rel_path,
                share_name: &folder.suggested_share_name,
                read_only: false,
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(folder.suggested_share_name.clone())
    }

    /// 3. List files inside a mounted SMB3 share path, filtering out `.tmp.*` staging files.
    pub async fn list_mounted_share_files(mount_root: &Path) -> Result<Vec<(String, u64, bool)>> {
        let mut rd = fs::read_dir(mount_root).await?;
        let mut entries = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".tmp.") || name == ".DS_Store" {
                continue;
            }
            let meta = entry.metadata().await?;
            let is_conflict = name.contains(".conflict-");
            if meta.is_file() {
                entries.push((name, meta.len(), is_conflict));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// 4. Atomic client-side write to a mounted SMB3 share so `drive-syncd` never uploads a partial file.
    pub async fn write_file_atomic(mount_root: &Path, file_name: &str, data: &[u8]) -> Result<PathBuf> {
        let tmp_name = format!(".tmp.client-{}.{}", uuid::Uuid::new_v4(), file_name);
        let tmp_path = mount_root.join(tmp_name);
        let final_path = mount_root.join(file_name);

        let mut f = fs::File::create(&tmp_path).await?;
        f.write_all(data).await?;
        f.flush().await?;
        drop(f);

        fs::rename(&tmp_path, &final_path).await?;
        Ok(final_path)
    }
}
```

---

### C. Android / Kotlin Client (Using `smbj` + OkHttp over Tailscale)

When building a custom Android client (e.g., Kotlin + Jetpack Compose):

1. **Dependencies (`build.gradle.kts`):**
   - Use **`com.hierynomus:smbj:0.13.0`** (supports SMB 3.1.1, multi-credit large reads, and compound directory queries) + **OkHttp / Retrofit** for `/api/tree` + **Coil** for image loading.
2. **SmbConfig Tuning for Tailscale:**
   ```kotlin
   val config = SmbConfig.builder()
       .withDialects(SMB2Dialect.SMB_3_1_1, SMB2Dialect.SMB_3_0_2)
       .withMultiProtocolNegotiate(true)
       .withReadBufferSize(1024 * 1024)   // 1 MiB read chunks for 2 MB files
       .withWriteBufferSize(1024 * 1024)  // 1 MiB write chunks
       .withTransactBufferSize(64 * 1024) // 64 KiB directory listing buffer
       .withTimeout(15, TimeUnit.SECONDS)
       .build()
   val client = SMBClient(config)
   ```
3. **Fast Directory Listing in `smbj`:**
   ```kotlin
   val connection = client.connect("100.124.79.120")
   val session = connection.authenticate(AuthenticationContext("jrufer", password.toCharArray(), "WORKGROUP"))
   val share = session.connectShare("today") as DiskShare

   // Single compound SMB2_QUERY_DIRECTORY call returning filename, size, and mtime:
   val files = share.list("")
       .filter { !it.fileName.startsWith(".") && !it.fileName.startsWith(".tmp.") }
       .map { info ->
           ClientFileItem(
               name = info.fileName,
               sizeBytes = info.endOfFile,
               modifiedMs = info.changeTime.toEpochMillis(),
               isConflict = info.fileName.contains(".conflict-")
           )
       }
   ```

---

### D. macOS & Windows Client Tuning

- **macOS (`NSFileManager` / Finder / Swift):**
  - LocalSync automatically enables `vfs objects = fruit streams_xattr`, `fruit:metadata = stream`, and `fruit:posix_rename = yes` in `/etc/samba/shares-syncd.conf`.
  - Disable `.DS_Store` creation on network volumes on macOS clients so Finder does not emit unnecessary metadata writes during browsing:
    ```bash
    defaults write com.apple.desktopservices DSDontWriteNetworkStores -bool TRUE
    ```
- **Windows 11 (Win32 `FindFirstFileExW` / .NET):**
  - Use `FindFirstFileExW` with `FindExInfoBasic` (which skips querying unneeded 8.3 short filenames) and `FIND_FIRST_EX_LARGE_FETCH` so the Windows SMB3 redirector fetches directory entries in 64 KiB batches.

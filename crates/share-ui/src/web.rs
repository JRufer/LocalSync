use crate::format_bytes;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use share_smb::{add_or_update_share, model::suggest_share_name, remove_share_by_name};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use sync_core::{
    config::{AppConfig, is_valid_tailscale_or_loopback_ip},
    db::{GlobalSyncSummary, ShareRow, StateDb, SyncEventRow, now_epoch_ms, parent_and_name},
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::info;

#[derive(Debug, Error)]
pub enum WebUiError {
    #[error("refusing to bind web UI to non-Tailscale IP {0}; must be in 100.64.0.0/10 or 127.0.0.1")]
    NonTailscaleIp(String),
    #[error("failed to bind TCP listener on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone)]
pub struct WebAppState {
    pub config: Arc<Mutex<AppConfig>>,
    pub config_path: Option<PathBuf>,
    pub db: StateDb,
    pub skip_samba_commands: bool,
}

#[derive(Debug, Serialize)]
pub struct ApiTreeResponse {
    pub tailscale_ip: String,
    pub local_root: String,
    pub root_folder_id: String,
    pub oauth_configured: bool,
    pub summary: GlobalSyncSummary,
    pub folders: Vec<FolderStatsEnhanced>,
    pub shares: Vec<ShareRow>,
    pub events: Vec<SyncEventRow>,
}

#[derive(Debug, Serialize)]
pub struct FolderStatsEnhanced {
    pub rel_path: String,
    pub name: String,
    pub subfolder_count: usize,
    pub file_count: usize,
    pub total_bytes: u64,
    pub human_size: String,
    pub conflict_count: usize,
    pub pending_count: usize,
    pub share_name: Option<String>,
    pub share_read_only: Option<bool>,
    pub suggested_share_name: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSharePayload {
    pub rel_path: String,
    pub share_name: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize)]
pub struct RemoveSharePayload {
    pub share_name: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateFolderPayload {
    pub rel_path: String,
    #[serde(default)]
    pub share_immediately: bool,
    pub share_name: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSettingsPayload {
    pub root_folder_id: String,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

pub fn build_router(state: WebAppState) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/api/tree", get(api_tree))
        .route("/api/shares/add", post(api_add_share))
        .route("/api/shares/remove", post(api_remove_share))
        .route("/api/folders/create", post(api_create_folder))
        .route("/api/settings/update", post(api_update_settings))
        .with_state(state)
}

/// Starts the Axum web UI strictly bound to `config.tailscale_ip:config.web_port`.
pub async fn serve_tailscale_web_ui(
    config: AppConfig,
    config_path: Option<PathBuf>,
    db: StateDb,
    skip_samba_commands: bool,
) -> Result<(), WebUiError> {
    if !is_valid_tailscale_or_loopback_ip(config.tailscale_ip) {
        return Err(WebUiError::NonTailscaleIp(config.tailscale_ip.to_string()));
    }

    let bind_addr = SocketAddr::from((config.tailscale_ip, config.web_port));
    let state = WebAppState {
        config: Arc::new(Mutex::new(config)),
        config_path,
        db,
        skip_samba_commands,
    };

    let app = build_router(state);
    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(|source| WebUiError::Bind {
            addr: bind_addr,
            source,
        })?;

    info!(%bind_addr, "Tailscale-bound LocalSync web UI listening");
    axum::serve(listener, app)
        .await
        .map_err(|source| WebUiError::Bind {
            addr: bind_addr,
            source,
        })?;
    Ok(())
}

/// Scans `local_root` on disk and registers any local folders not yet in SQLite so the UI
/// always displays all local folders immediately even before Google Drive OAuth is configured.
fn index_local_directories_on_disk(local_root: &std::path::Path, db: &StateDb) {
    let mut stack = vec![local_root.to_path_buf()];
    let now = now_epoch_ms();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(local_root) else {
                continue;
            };
            let Some(rel_str) = rel.to_str() else {
                continue;
            };
            if rel_str.is_empty() || rel_str.split('/').any(|s| s.starts_with('.')) {
                continue;
            }
            let norm = rel_str.replace('\\', "/");
            if db.get_entry(&norm).ok().flatten().is_none() {
                let (parent_rel_path, name) = parent_and_name(&norm);
                let _ = db.upsert_entry(&sync_core::db::EntryRow {
                    rel_path: norm,
                    parent_rel_path,
                    name,
                    is_dir: true,
                    drive_file_id: None,
                    drive_parent_id: None,
                    md5_checksum: None,
                    blake3_hash: None,
                    size_bytes: 0,
                    remote_modified_ms: None,
                    local_modified_ms: Some(now),
                    sync_status: sync_core::db::SyncStatus::Synced,
                    last_synced_hash: None,
                    suppress_remote_until_ms: 0,
                    suppress_local_until_ms: 0,
                    updated_at: now,
                });
            }
            stack.push(path);
        }
    }
}

async fn api_tree(State(state): State<WebAppState>) -> impl IntoResponse {
    let cfg = state.config.lock().await;
    let existing_names: Vec<String> = cfg.shares.iter().map(|s| s.share_name.clone()).collect();
    let tailscale_ip = cfg.tailscale_ip.to_string();
    let local_root = cfg.local_root.clone();
    let root_folder_id = cfg.root_folder_id.clone();
    let oauth_configured = !cfg.oauth.client_id.is_empty()
        && !cfg.oauth.client_id.starts_with("YOUR_GOOGLE_OAUTH");
    drop(cfg);

    index_local_directories_on_disk(&local_root, &state.db);

    let stats_map = match state.db.compute_all_folder_stats() {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let mut folders: Vec<FolderStatsEnhanced> = stats_map
        .into_values()
        .map(|st| {
            let suggested = suggest_share_name(&st.rel_path, &existing_names);
            FolderStatsEnhanced {
                human_size: format_bytes(st.total_bytes),
                rel_path: st.rel_path,
                name: st.name,
                subfolder_count: st.subfolder_count,
                file_count: st.file_count,
                total_bytes: st.total_bytes,
                conflict_count: st.conflict_count,
                pending_count: st.pending_count,
                share_name: st.share_name,
                share_read_only: st.share_read_only,
                suggested_share_name: suggested,
            }
        })
        .collect();
    folders.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    let summary = state.db.get_global_summary().unwrap_or_default();
    let shares = state.db.list_shares().unwrap_or_default();
    let events = state.db.list_recent_events(20).unwrap_or_default();

    Json(ApiTreeResponse {
        tailscale_ip,
        local_root: local_root.display().to_string(),
        root_folder_id,
        oauth_configured,
        summary,
        folders,
        shares,
        events,
    })
    .into_response()
}

async fn api_add_share(
    State(state): State<WebAppState>,
    Json(payload): Json<CreateSharePayload>,
) -> impl IntoResponse {
    let mut cfg = state.config.lock().await;
    let existing_names: Vec<String> = cfg.shares.iter().map(|s| s.share_name.clone()).collect();
    let share_name = payload
        .share_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| suggest_share_name(&payload.rel_path, &existing_names));

    match add_or_update_share(
        &mut cfg,
        state.config_path.as_deref(),
        &state.db,
        &share_name,
        &payload.rel_path,
        payload.read_only,
        None,
        state.skip_samba_commands,
    ) {
        Ok(res) => {
            let warnings: Vec<String> = res
                .overlap_warnings
                .iter()
                .map(|w| w.describe())
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "share_name": res.share.share_name,
                    "rel_path": res.share.rel_path,
                    "warnings": warnings,
                })),
            )
                .into_response()
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_remove_share(
    State(state): State<WebAppState>,
    Json(payload): Json<RemoveSharePayload>,
) -> impl IntoResponse {
    let mut cfg = state.config.lock().await;
    match remove_share_by_name(
        &mut cfg,
        state.config_path.as_deref(),
        &state.db,
        &payload.share_name,
        state.skip_samba_commands,
    ) {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "removed": payload.share_name })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_create_folder(
    State(state): State<WebAppState>,
    Json(payload): Json<CreateFolderPayload>,
) -> impl IntoResponse {
    let mut cfg = state.config.lock().await;
    let norm = match sync_core::config::normalize_rel_path(&payload.rel_path) {
        Ok(n) => n,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": e })),
            )
                .into_response();
        }
    };

    let target = cfg.local_root.join(&norm);
    if let Err(e) = std::fs::create_dir_all(&target) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response();
    }

    index_local_directories_on_disk(&cfg.local_root, &state.db);

    if payload.share_immediately {
        let existing_names: Vec<String> = cfg.shares.iter().map(|s| s.share_name.clone()).collect();
        let share_name = payload
            .share_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| suggest_share_name(&norm, &existing_names));

        if let Err(e) = add_or_update_share(
            &mut cfg,
            state.config_path.as_deref(),
            &state.db,
            &share_name,
            &norm,
            payload.read_only,
            None,
            state.skip_samba_commands,
        ) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            )
                .into_response();
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "rel_path": norm })),
    )
        .into_response()
}

async fn api_update_settings(
    State(state): State<WebAppState>,
    Json(payload): Json<UpdateSettingsPayload>,
) -> impl IntoResponse {
    let mut cfg = state.config.lock().await;
    if !payload.root_folder_id.trim().is_empty() {
        cfg.root_folder_id = payload.root_folder_id.trim().to_string();
    }
    if let Some(cid) = payload.client_id {
        if !cid.trim().is_empty() {
            cfg.oauth.client_id = cid.trim().to_string();
        }
    }
    if let Some(csec) = payload.client_secret {
        if !csec.trim().is_empty() {
            cfg.oauth.client_secret = csec.trim().to_string();
        }
    }
    if let Some(ref path) = state.config_path {
        if let Err(e) = cfg.save_atomic(path) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            )
                .into_response();
        }
    }
    let _ = state.db.ensure_sync_state(&cfg.root_folder_id);
    (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
}

async fn index_html() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>LocalSync · Share Manager & Google Drive Control Panel</title>
<style>
  :root { --bg: #0f172a; --card: #1e293b; --border: #334155; --accent: #0284c7; --accent-hover: #0369a1; --green: #059669; --red: #dc2626; --text: #f1f5f9; --muted: #94a3b8; }
  body { font-family: system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--text); margin: 0; padding: 24px 32px; }
  .top-bar { display: flex; justify-content: space-between; align-items: center; margin-bottom: 18px; flex-wrap: wrap; gap: 12px; }
  h1 { margin: 0; font-size: 1.45rem; color: #38bdf8; }
  .summary { background: var(--card); padding: 14px 18px; border-radius: 10px; margin-bottom: 18px; display: flex; gap: 28px; flex-wrap: wrap; border: 1px solid var(--border); }
  .grid { display: grid; grid-template-columns: 2fr 1fr; gap: 18px; }
  @media (max-width: 960px) { .grid { grid-template-columns: 1fr; } }
  .card { background: var(--card); border-radius: 10px; padding: 18px; margin-bottom: 18px; border: 1px solid var(--border); }
  .card h2 { margin-top: 0; font-size: 1.1rem; color: #7dd3fc; }
  input[type="text"], input[type="password"] { background: var(--bg); border: 1px solid var(--border); color: var(--text); padding: 8px 12px; border-radius: 6px; width: 100%; box-sizing: border-box; }
  table { width: 100%; border-collapse: collapse; margin-top: 12px; }
  th, td { text-align: left; padding: 10px 8px; border-bottom: 1px solid var(--border); font-size: 0.92rem; }
  .badge { background: var(--green); color: white; padding: 3px 8px; border-radius: 4px; font-size: 0.8rem; font-weight: 600; display: inline-block; }
  .badge-ro { background: #d97706; }
  .btn { background: var(--accent); color: white; border: none; padding: 7px 13px; border-radius: 6px; cursor: pointer; font-weight: 600; font-size: 0.86rem; }
  .btn:hover { background: var(--accent-hover); }
  .btn-secondary { background: #475569; }
  .btn-danger { background: var(--red); }
  .form-row { display: flex; gap: 10px; align-items: center; margin-bottom: 10px; }
  .uri-box { font-family: monospace; font-size: 0.82rem; color: var(--muted); margin-top: 4px; }
</style>
</head>
<body>
  <div class="top-bar">
    <h1>LocalSync · Google Drive v3 ↔ SMB3 over Tailscale</h1>
    <div>
      <button class="btn" onclick="openNewFolderModal()">+ New Folder / Share</button>
      <button class="btn btn-secondary" onclick="loadTree()">↻ Refresh</button>
    </div>
  </div>

  <div id="summary" class="summary">Loading sync state...</div>

  <div class="grid">
    <div>
      <div class="card">
        <h2>Folders & SMB3 Shares</h2>
        <input type="text" id="filter" placeholder="Search folders (e.g. Today, 2026)..." oninput="renderTable()">
        <table>
          <thead>
            <tr>
              <th>Folder</th>
              <th>Files</th>
              <th>Size</th>
              <th>SMB3 Share & Tailscale Mount URI</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody id="folders-body"></tbody>
        </table>
      </div>
    </div>

    <div>
      <div class="card">
        <h2>Quick Create Folder & Share</h2>
        <div class="form-row">
          <input type="text" id="new-folder-path" placeholder="Folder relative path (e.g. Today or 2026/Trips)">
        </div>
        <div class="form-row">
          <input type="text" id="new-share-name" placeholder="Optional share name (auto-suggested if blank)">
        </div>
        <div class="form-row">
          <label><input type="checkbox" id="new-share-now" checked> Export as SMB3 Share immediately</label>
          <label><input type="checkbox" id="new-share-ro"> Read-Only</label>
        </div>
        <button class="btn" onclick="createFolderAndShare()">Create & Share</button>
      </div>

      <div class="card">
        <h2>Google Drive Sync Settings</h2>
        <div class="form-row">
          <input type="text" id="cfg-folder-id" placeholder="Google Drive Root Folder ID">
        </div>
        <div class="form-row">
          <input type="text" id="cfg-client-id" placeholder="OAuth Client ID (optional update)">
        </div>
        <div class="form-row">
          <input type="password" id="cfg-client-secret" placeholder="OAuth Client Secret (optional update)">
        </div>
        <button class="btn btn-secondary" onclick="saveSettings()">Save Config</button>
      </div>

      <div class="card">
        <h2>Recent Sync & Share Events</h2>
        <div id="events-list" style="font-size:0.85rem; color: var(--muted);"></div>
      </div>
    </div>
  </div>

<script>
let stateData = null;
async function loadTree() {
  const res = await fetch('/api/tree');
  stateData = await res.json();
  const s = stateData.summary;
  document.getElementById('cfg-folder-id').value = stateData.root_folder_id || '';
  document.getElementById('summary').innerHTML =
    `<div><b>Tailscale IP:</b> ${stateData.tailscale_ip}</div>` +
    `<div><b>Local Root:</b> <code>${stateData.local_root}</code></div>` +
    `<div><b>Files:</b> ${s.total_files}</div>` +
    `<div><b>Folders:</b> ${s.total_dirs}</div>` +
    `<div><b>Active SMB3 Shares:</b> ${s.active_shares_count}</div>` +
    `<div><b>Conflicts:</b> ${s.conflict_count}</div>`;
  renderTable();
  renderEvents();
}

function renderTable() {
  if (!stateData) return;
  const q = document.getElementById('filter').value.toLowerCase();
  const tbody = document.getElementById('folders-body');
  tbody.innerHTML = '';
  if (stateData.folders.length === 0) {
    tbody.innerHTML = `<tr><td colspan="5" style="color:var(--muted)">No folders yet. Use "Quick Create Folder & Share" on the right to create your first shared folder!</td></tr>`;
    return;
  }
  for (const f of stateData.folders) {
    if (q && !f.rel_path.toLowerCase().includes(q)) continue;
    const tr = document.createElement('tr');
    const isRo = f.share_read_only === true;
    const shareCell = f.share_name
      ? `<span class="badge ${isRo ? 'badge-ro' : ''}">[${f.share_name}] (${isRo ? 'Read-Only' : 'Read/Write'})</span>` +
        `<div class="uri-box">smb://${stateData.tailscale_ip}/${f.share_name} &nbsp;|&nbsp; \\\\${stateData.tailscale_ip}\\${f.share_name}</div>`
      : '<span style="color:var(--muted)">Not shared</span>';

    const actionCell = f.share_name
      ? `<button class="btn btn-secondary" onclick="toggleMode('${f.rel_path}', '${f.share_name}', ${!isRo})">${isRo ? 'Make RW' : 'Make RO'}</button> ` +
        `<button class="btn btn-danger" onclick="unshare('${f.share_name}')">Unshare</button>`
      : `<button class="btn" onclick="shareFolder('${f.rel_path}', '${f.suggested_share_name}')">Share Folder</button>`;

    tr.innerHTML = `<td>📁 <b>${f.rel_path}</b></td><td>${f.file_count}</td><td>${f.human_size}</td><td>${shareCell}</td><td>${actionCell}</td>`;
    tbody.appendChild(tr);
  }
}

function renderEvents() {
  const box = document.getElementById('events-list');
  if (!stateData || !stateData.events.length) {
    box.innerHTML = 'No events recorded yet.';
    return;
  }
  box.innerHTML = stateData.events.slice(0, 8).map(e =>
    `<div style="margin-bottom:6px;"><b>[${e.level.toUpperCase()}]</b> ${e.message}</div>`
  ).join('');
}

async function shareFolder(rel_path, suggested) {
  const name = prompt('SMB3 Share Name ([a-z0-9-_]):', suggested);
  if (!name) return;
  const ro = confirm('Export as Read-Only? (Click Cancel for Read/Write, OK for Read-Only)');
  const res = await fetch('/api/shares/add', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ rel_path, share_name: name, read_only: ro })
  });
  const out = await res.json();
  if (!out.ok) alert('Error: ' + out.error);
  else if (out.warnings && out.warnings.length) alert(out.warnings.join('\n'));
  await loadTree();
}

async function toggleMode(rel_path, share_name, read_only) {
  await fetch('/api/shares/add', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ rel_path, share_name, read_only })
  });
  await loadTree();
}

async function unshare(share_name) {
  if (!confirm(`Remove SMB3 share [${share_name}]? (Files remain intact)`)) return;
  await fetch('/api/shares/remove', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ share_name })
  });
  await loadTree();
}

async function createFolderAndShare() {
  const rel_path = document.getElementById('new-folder-path').value.trim();
  if (!rel_path) { alert('Enter a folder path first.'); return; }
  const share_name = document.getElementById('new-share-name').value.trim() || null;
  const share_immediately = document.getElementById('new-share-now').checked;
  const read_only = document.getElementById('new-share-ro').checked;

  const res = await fetch('/api/folders/create', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ rel_path, share_immediately, share_name, read_only })
  });
  const out = await res.json();
  if (!out.ok) alert('Error: ' + out.error);
  else {
    document.getElementById('new-folder-path').value = '';
    document.getElementById('new-share-name').value = '';
    await loadTree();
  }
}

function openNewFolderModal() {
  document.getElementById('new-folder-path').focus();
}

async function saveSettings() {
  const root_folder_id = document.getElementById('cfg-folder-id').value.trim();
  const client_id = document.getElementById('cfg-client-id').value.trim() || null;
  const client_secret = document.getElementById('cfg-client-secret').value.trim() || null;
  const res = await fetch('/api/settings/update', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ root_folder_id, client_id, client_secret })
  });
  const out = await res.json();
  if (out.ok) alert('Settings saved to config.toml.');
  else alert('Error: ' + out.error);
  await loadTree();
}

loadTree();
</script>
</body>
</html>"#,
    )
}

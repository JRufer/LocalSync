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
    db::{GlobalSyncSummary, ShareRow, StateDb, SyncEventRow},
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

pub fn build_router(state: WebAppState) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/api/tree", get(api_tree))
        .route("/api/shares/add", post(api_add_share))
        .route("/api/shares/remove", post(api_remove_share))
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

async fn api_tree(State(state): State<WebAppState>) -> impl IntoResponse {
    let cfg = state.config.lock().await;
    let existing_names: Vec<String> = cfg.shares.iter().map(|s| s.share_name.clone()).collect();
    let tailscale_ip = cfg.tailscale_ip.to_string();
    drop(cfg);

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

async fn index_html() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>LocalSync · Tailscale SMB3 & Google Drive Browser</title>
<style>
  body { font-family: system-ui, -apple-system, sans-serif; background: #0f172a; color: #e2e8f0; margin: 0; padding: 24px; }
  h1 { margin-top: 0; font-size: 1.4rem; color: #38bdf8; }
  .summary { background: #1e293b; padding: 14px 18px; border-radius: 8px; margin-bottom: 18px; display: flex; gap: 24px; flex-wrap: wrap; }
  .card { background: #1e293b; border-radius: 8px; padding: 16px; margin-bottom: 18px; }
  input[type="text"] { background: #0f172a; border: 1px solid #334155; color: #f8fafc; padding: 8px 12px; border-radius: 6px; width: 300px; }
  table { width: 100%; border-collapse: collapse; margin-top: 12px; }
  th, td { text-align: left; padding: 10px; border-bottom: 1px solid #334155; font-size: 0.92rem; }
  .badge { background: #059669; color: white; padding: 3px 8px; border-radius: 4px; font-size: 0.8rem; font-weight: 600; }
  .btn { background: #0284c7; color: white; border: none; padding: 6px 12px; border-radius: 5px; cursor: pointer; font-weight: 600; }
  .btn-danger { background: #dc2626; }
</style>
</head>
<body>
  <h1>LocalSync · Google Drive v3 ↔ SMB3 over Tailscale</h1>
  <div id="summary" class="summary">Loading sync state...</div>
  <div class="card">
    <input type="text" id="filter" placeholder="Filter folders (e.g. Today)..." oninput="renderTable()">
    <table>
      <thead>
        <tr>
          <th>Folder Path</th>
          <th>Files</th>
          <th>Subfolders</th>
          <th>Total Size</th>
          <th>SMB3 Share</th>
          <th>Action</th>
        </tr>
      </thead>
      <tbody id="folders-body"></tbody>
    </table>
  </div>
<script>
let stateData = null;
async function loadTree() {
  const res = await fetch('/api/tree');
  stateData = await res.json();
  const s = stateData.summary;
  document.getElementById('summary').innerHTML =
    `<div><b>Tailscale IP:</b> ${stateData.tailscale_ip}</div>` +
    `<div><b>Files:</b> ${s.total_files}</div>` +
    `<div><b>Folders:</b> ${s.total_dirs}</div>` +
    `<div><b>Active SMB3 Shares:</b> ${s.active_shares_count}</div>` +
    `<div><b>Conflicts:</b> ${s.conflict_count}</div>`;
  renderTable();
}
function renderTable() {
  if (!stateData) return;
  const q = document.getElementById('filter').value.toLowerCase();
  const tbody = document.getElementById('folders-body');
  tbody.innerHTML = '';
  for (const f of stateData.folders) {
    if (q && !f.rel_path.toLowerCase().includes(q)) continue;
    const tr = document.createElement('tr');
    const shareCell = f.share_name
      ? `<span class="badge">\\\\${stateData.tailscale_ip}\\${f.share_name} (${f.share_read_only ? 'RO' : 'RW'})</span>`
      : '—';
    const actionCell = f.share_name
      ? `<button class="btn btn-danger" onclick="unshare('${f.share_name}')">Unshare</button>`
      : `<button class="btn" onclick="shareFolder('${f.rel_path}', '${f.suggested_share_name}')">Share Folder</button>`;
    tr.innerHTML = `<td>📁 ${f.rel_path}</td><td>${f.file_count}</td><td>${f.subfolder_count}</td><td>${f.human_size}</td><td>${shareCell}</td><td>${actionCell}</td>`;
    tbody.appendChild(tr);
  }
}
async function shareFolder(rel_path, suggested) {
  const name = prompt('SMB3 Share Name ([a-z0-9-_]):', suggested);
  if (!name) return;
  const res = await fetch('/api/shares/add', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ rel_path, share_name: name, read_only: false })
  });
  const out = await res.json();
  if (!out.ok) alert('Error: ' + out.error);
  else if (out.warnings && out.warnings.length) alert(out.warnings.join('\n'));
  await loadTree();
}
async function unshare(share_name) {
  if (!confirm(`Remove SMB3 share [${share_name}]?`)) return;
  await fetch('/api/shares/remove', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ share_name })
  });
  await loadTree();
}
loadTree();
</script>
</body>
</html>"#,
    )
}

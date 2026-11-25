use crate::auth::{AuthError, TokenManager};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use rand::Rng;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncWriteExt, BufWriter};
use uuid::Uuid;

pub const DRIVE_FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const DEFAULT_DRIVE_API_BASE: &str = "https://www.googleapis.com/drive/v3";
const DEFAULT_DRIVE_UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";

#[derive(Debug, Error)]
pub enum DriveError {
    #[error("authentication error: {0}")]
    Auth(#[from] AuthError),
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("local file I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Google Drive API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },
    #[error("download verification failed for {path}: {reason}")]
    VerificationFailed { path: String, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub md5_checksum: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub trashed: bool,
}

impl DriveFile {
    pub fn is_dir(&self) -> bool {
        self.mime_type == DRIVE_FOLDER_MIME
    }

    pub fn size_bytes(&self) -> i64 {
        self.size
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
    }

    pub fn modified_time_ms(&self) -> Option<i64> {
        self.modified_time.as_deref().and_then(|ts| {
            chrono::DateTime::parse_from_rfc3339(ts)
                .ok()
                .map(|dt| dt.timestamp_millis())
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFileListResponse {
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub files: Vec<DriveFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveChange {
    pub file_id: String,
    #[serde(default)]
    pub removed: bool,
    #[serde(default)]
    pub file: Option<DriveFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveChangesListResponse {
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub new_start_page_token: Option<String>,
    #[serde(default)]
    pub changes: Vec<DriveChange>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartPageTokenResponse {
    start_page_token: String,
}

/// Abstraction over Drive API v3 operations so both the real HTTP client and deterministic
/// test doubles can drive the bidirectional sync engine.
#[allow(async_fn_in_trait)]
pub trait DriveApi: Send + Sync {
    async fn get_start_page_token(&self) -> Result<String, DriveError>;
    async fn list_children(&self, folder_id: &str) -> Result<Vec<DriveFile>, DriveError>;
    async fn list_changes(
        &self,
        page_token: &str,
    ) -> Result<(Vec<DriveChange>, String), DriveError>;
    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<DriveFile, DriveError>;
    async fn upload_file(
        &self,
        parent_id: &str,
        name: &str,
        existing_file_id: Option<&str>,
        local_path: &Path,
    ) -> Result<DriveFile, DriveError>;
    async fn download_file_atomic(
        &self,
        file_id: &str,
        dest_path: &Path,
        expected_md5: Option<&str>,
    ) -> Result<(String, u64), DriveError>;
    async fn delete_or_trash(&self, file_id: &str, trash: bool) -> Result<(), DriveError>;
}

#[derive(Clone)]
pub struct DriveClient {
    http: Client,
    tokens: TokenManager,
    api_base: String,
    upload_base: String,
    max_retries: u32,
}

impl DriveClient {
    pub fn new(tokens: TokenManager) -> Result<Self, DriveError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(16)
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true)
            .build()?;
        Ok(Self {
            http,
            tokens,
            api_base: DEFAULT_DRIVE_API_BASE.to_string(),
            upload_base: DEFAULT_DRIVE_UPLOAD_BASE.to_string(),
            max_retries: 5,
        })
    }

    pub fn with_base_urls(
        http: Client,
        tokens: TokenManager,
        api_base: String,
        upload_base: String,
    ) -> Self {
        Self {
            http,
            tokens,
            api_base,
            upload_base,
            max_retries: 4,
        }
    }

    /// Executes an HTTP request builder factory with exponential backoff + jitter on HTTP 429 or 5xx.
    async fn send_with_retry<F>(&self, mut make_req: F) -> Result<reqwest::Response, DriveError>
    where
        F: FnMut(&Client, &str) -> reqwest::RequestBuilder,
    {
        let mut attempt = 0u32;
        loop {
            let token = self.tokens.get_access_token().await?;
            let resp_res = make_req(&self.http, &token).send().await;

            match resp_res {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp);
                    }
                    let should_retry = status == StatusCode::TOO_MANY_REQUESTS
                        || status.is_server_error();
                    if should_retry && attempt < self.max_retries {
                        attempt += 1;
                        let backoff_ms = (200u64 << attempt.min(6))
                            + rand::thread_rng().gen_range(0..250);
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                    let code = status.as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(DriveError::Api {
                        status: code,
                        message: body,
                    });
                }
                Err(err) => {
                    if attempt < self.max_retries {
                        attempt += 1;
                        let backoff_ms = (200u64 << attempt.min(6))
                            + rand::thread_rng().gen_range(0..250);
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                    return Err(DriveError::Http(err));
                }
            }
        }
    }
}

impl DriveApi for DriveClient {
    async fn get_start_page_token(&self) -> Result<String, DriveError> {
        let url = format!("{}/changes/startPageToken", self.api_base);
        let resp = self
            .send_with_retry(|client, token| client.get(&url).bearer_auth(token))
            .await?;
        let parsed: StartPageTokenResponse = resp.json().await?;
        Ok(parsed.start_page_token)
    }

    async fn list_children(&self, folder_id: &str) -> Result<Vec<DriveFile>, DriveError> {
        let mut all_files = Vec::new();
        let mut page_token: Option<String> = None;
        let escaped_id = folder_id.replace('\'', "\\'");
        let query = format!("'{escaped_id}' in parents and trashed = false");
        let fields = "nextPageToken,files(id,name,mimeType,parents,md5Checksum,size,modifiedTime,trashed)";

        loop {
            let url = format!("{}/files", self.api_base);
            let pt = page_token.clone();
            let q = query.clone();
            let resp = self
                .send_with_retry(|client, token| {
                    let mut req = client.get(&url).bearer_auth(token).query(&[
                        ("q", q.as_str()),
                        ("fields", fields),
                        ("pageSize", "1000"),
                        ("supportsAllDrives", "true"),
                        ("includeItemsFromAllDrives", "true"),
                    ]);
                    if let Some(ref page) = pt {
                        req = req.query(&[("pageToken", page.as_str())]);
                    }
                    req
                })
                .await?;

            let parsed: DriveFileListResponse = resp.json().await?;
            all_files.extend(parsed.files);
            match parsed.next_page_token {
                Some(next) if !next.is_empty() => page_token = Some(next),
                _ => break,
            }
        }

        Ok(all_files)
    }

    async fn list_changes(
        &self,
        page_token: &str,
    ) -> Result<(Vec<DriveChange>, String), DriveError> {
        let mut all_changes = Vec::new();
        let mut current_token = page_token.to_string();
        let fields = "nextPageToken,newStartPageToken,changes(fileId,removed,file(id,name,mimeType,parents,md5Checksum,size,modifiedTime,trashed))";

        loop {
            let url = format!("{}/changes", self.api_base);
            let tok_param = current_token.clone();
            let resp = self
                .send_with_retry(|client, token| {
                    client.get(&url).bearer_auth(token).query(&[
                        ("pageToken", tok_param.as_str()),
                        ("fields", fields),
                        ("pageSize", "1000"),
                        ("includeRemoved", "true"),
                        ("supportsAllDrives", "true"),
                        ("includeItemsFromAllDrives", "true"),
                    ])
                })
                .await?;

            let parsed: DriveChangesListResponse = resp.json().await?;
            all_changes.extend(parsed.changes);

            if let Some(next) = parsed.next_page_token {
                if !next.is_empty() {
                    current_token = next;
                    continue;
                }
            }
            let new_start = parsed.new_start_page_token.unwrap_or(current_token);
            return Ok((all_changes, new_start));
        }
    }

    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<DriveFile, DriveError> {
        let url = format!("{}/files", self.api_base);
        let metadata = serde_json::json!({
            "name": name,
            "mimeType": DRIVE_FOLDER_MIME,
            "parents": [parent_id],
        });

        let resp = self
            .send_with_retry(|client, token| {
                client
                    .post(&url)
                    .bearer_auth(token)
                    .query(&[(
                        "fields",
                        "id,name,mimeType,parents,md5Checksum,size,modifiedTime,trashed",
                    )])
                    .json(&metadata)
            })
            .await?;

        Ok(resp.json().await?)
    }

    async fn upload_file(
        &self,
        parent_id: &str,
        name: &str,
        existing_file_id: Option<&str>,
        local_path: &Path,
    ) -> Result<DriveFile, DriveError> {
        let file_bytes = fs::read(local_path).await?;
        let boundary = format!("localsync_boundary_{}", Uuid::new_v4().simple());

        let metadata = if existing_file_id.is_some() {
            serde_json::json!({ "name": name })
        } else {
            serde_json::json!({
                "name": name,
                "parents": [parent_id]
            })
        };
        let meta_bytes = serde_json::to_vec(&metadata)?;

        let mut body = Vec::with_capacity(meta_bytes.len() + file_bytes.len() + 256);
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
        body.extend_from_slice(&meta_bytes);
        body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(&file_bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let (method, url) = match existing_file_id {
            Some(fid) => (Method::PATCH, format!("{}/files/{fid}", self.upload_base)),
            None => (Method::POST, format!("{}/files", self.upload_base)),
        };
        let content_type = format!("multipart/related; boundary={boundary}");

        let resp = self
            .send_with_retry(|client, token| {
                client
                    .request(method.clone(), &url)
                    .bearer_auth(token)
                    .header("Content-Type", &content_type)
                    .query(&[
                        ("uploadType", "multipart"),
                        (
                            "fields",
                            "id,name,mimeType,parents,md5Checksum,size,modifiedTime,trashed",
                        ),
                    ])
                    .body(body.clone())
            })
            .await?;

        Ok(resp.json().await?)
    }

    async fn download_file_atomic(
        &self,
        file_id: &str,
        dest_path: &Path,
        expected_md5: Option<&str>,
    ) -> Result<(String, u64), DriveError> {
        if let Some(parent) = dest_path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await?;
            }
        }

        let file_name = dest_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download");
        let parent_dir = dest_path.parent().unwrap_or_else(|| Path::new("."));
        let tmp_path = parent_dir.join(format!(".tmp.{}.{}", Uuid::new_v4(), file_name));

        let url = format!("{}/files/{file_id}", self.api_base);
        let resp = self
            .send_with_retry(|client, token| {
                client
                    .get(&url)
                    .bearer_auth(token)
                    .query(&[("alt", "media"), ("supportsAllDrives", "true")])
            })
            .await?;

        let raw_file = fs::File::create(&tmp_path).await?;
        let mut writer = BufWriter::with_capacity(64 * 1024, raw_file);
        let mut md5_hasher = Md5::new();
        let mut blake3_hasher = blake3::Hasher::new();
        let mut size: u64 = 0;

        let mut stream = resp.bytes_stream();
        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path).await;
                    return Err(DriveError::Http(e));
                }
            };
            size += chunk.len() as u64;
            md5_hasher.update(&chunk);
            blake3_hasher.update(&chunk);
            if let Err(e) = writer.write_all(&chunk).await {
                let _ = fs::remove_file(&tmp_path).await;
                return Err(DriveError::Io(e));
            }
        }

        if let Err(e) = writer.flush().await {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(DriveError::Io(e));
        }
        let inner_file = writer.into_inner();
        inner_file.sync_all().await?;
        drop(inner_file);

        let actual_md5 = hex::encode(md5_hasher.finalize());
        if let Some(exp) = expected_md5 {
            if !exp.is_empty() && !exp.eq_ignore_ascii_case(&actual_md5) {
                let _ = fs::remove_file(&tmp_path).await;
                return Err(DriveError::VerificationFailed {
                    path: dest_path.display().to_string(),
                    reason: format!("MD5 mismatch: expected {exp}, got {actual_md5}"),
                });
            }
        }

        let blake3_hash = blake3_hasher.finalize().to_hex().to_string();
        fs::rename(&tmp_path, dest_path).await?;
        Ok((blake3_hash, size))
    }

    async fn delete_or_trash(&self, file_id: &str, trash: bool) -> Result<(), DriveError> {
        let url = format!("{}/files/{file_id}", self.api_base);
        if trash {
            let body = serde_json::json!({ "trashed": true });
            self.send_with_retry(|client, token| {
                client
                    .patch(&url)
                    .bearer_auth(token)
                    .query(&[("supportsAllDrives", "true")])
                    .json(&body)
            })
            .await?;
        } else {
            self.send_with_retry(|client, token| {
                client
                    .delete(&url)
                    .bearer_auth(token)
                    .query(&[("supportsAllDrives", "true")])
            })
            .await?;
        }
        Ok(())
    }
}

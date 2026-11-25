use crate::config::OAuthConfig;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use url::Url;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("OAuth client_id or client_secret is not configured")]
    MissingClientCredentials,
    #[error("token store file I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse or serialize token JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HTTP request error during OAuth flow: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OAuth token endpoint returned status {status}: {body}")]
    TokenEndpoint { status: u16, body: String },
    #[error("no refresh token available; run `drivectl auth` to complete desktop login")]
    NoRefreshToken,
    #[error("OAuth loopback callback error: {0}")]
    Loopback(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_epoch_secs: i64,
    pub token_type: String,
}

impl StoredToken {
    pub fn is_expired_or_expiring(&self, skew_secs: i64) -> bool {
        let now = chrono::Utc::now().timestamp();
        now + skew_secs >= self.expires_at_epoch_secs
    }
}

#[derive(Debug, Deserialize)]
struct TokenEndpointResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default = "default_bearer")]
    token_type: String,
}

fn default_bearer() -> String {
    "Bearer".to_string()
}

/// Write token JSON atomically with `0600` permissions on Unix.
pub fn write_token_file_0600(path: &Path, token: &StoredToken) -> Result<(), AuthError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).map_err(|source| AuthError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    let json_bytes = serde_json::to_vec_pretty(token)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = parent.join(format!(".tmp.token.{}", uuid::Uuid::new_v4()));

    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path).map_err(|source| AuthError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        file.write_all(&json_bytes)
            .and_then(|_| file.sync_all())
            .map_err(|source| AuthError::Io {
                path: tmp_path.clone(),
                source,
            })?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&tmp_path, perms).map_err(|source| AuthError::Io {
            path: tmp_path.clone(),
            source,
        })?;
    }

    fs::rename(&tmp_path, path).map_err(|source| AuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

pub fn read_token_file(path: &Path) -> Result<StoredToken, AuthError> {
    let raw = fs::read_to_string(path).map_err(|source| AuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let token: StoredToken = serde_json::from_str(&raw)?;
    Ok(token)
}

#[derive(Clone)]
pub struct TokenManager {
    config: OAuthConfig,
    http: Client,
    token_url: String,
    cached: Arc<RwLock<Option<StoredToken>>>,
}

impl TokenManager {
    pub fn from_config(config: OAuthConfig) -> Self {
        Self::new(config, Client::new())
    }

    pub fn new(config: OAuthConfig, http: Client) -> Self {
        Self::with_token_url(config, http, GOOGLE_TOKEN_URL.to_string())
    }

    pub fn with_token_url(config: OAuthConfig, http: Client, token_url: String) -> Self {
        let initial = read_token_file(&config.token_path).ok();
        Self {
            config,
            http,
            token_url,
            cached: Arc::new(RwLock::new(initial)),
        }
    }

    pub async fn set_token(&self, token: StoredToken) -> Result<(), AuthError> {
        write_token_file_0600(&self.config.token_path, &token)?;
        *self.cached.write().await = Some(token);
        Ok(())
    }

    /// Returns a valid Bearer access token, refreshing automatically via `refresh_token` if needed.
    pub async fn get_access_token(&self) -> Result<String, AuthError> {
        {
            let guard = self.cached.read().await;
            if let Some(tok) = guard.as_ref() {
                if !tok.is_expired_or_expiring(60) {
                    return Ok(tok.access_token.clone());
                }
            }
        }

        let mut guard = self.cached.write().await;
        if guard.is_none() {
            if let Ok(disk_tok) = read_token_file(&self.config.token_path) {
                *guard = Some(disk_tok);
            }
        }

        if let Some(tok) = guard.as_ref() {
            if !tok.is_expired_or_expiring(60) {
                return Ok(tok.access_token.clone());
            }
            let refresh_token = tok
                .refresh_token
                .clone()
                .ok_or(AuthError::NoRefreshToken)?;
            let refreshed = self.refresh_access_token(&refresh_token).await?;
            write_token_file_0600(&self.config.token_path, &refreshed)?;
            let access = refreshed.access_token.clone();
            *guard = Some(refreshed);
            Ok(access)
        } else {
            Err(AuthError::NoRefreshToken)
        }
    }

    pub async fn refresh_access_token(&self, refresh_token: &str) -> Result<StoredToken, AuthError> {
        if self.config.client_id.trim().is_empty() || self.config.client_secret.trim().is_empty() {
            return Err(AuthError::MissingClientCredentials);
        }

        let resp = self
            .http
            .post(&self.token_url)
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("client_secret", self.config.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;

        let status = resp.status().as_u16();
        let body = resp.text().await?;
        if !(200..300).contains(&status) {
            return Err(AuthError::TokenEndpoint { status, body });
        }

        let parsed: TokenEndpointResponse = serde_json::from_str(&body)?;
        let now = chrono::Utc::now().timestamp();
        Ok(StoredToken {
            access_token: parsed.access_token,
            refresh_token: parsed
                .refresh_token
                .or_else(|| Some(refresh_token.to_string())),
            expires_at_epoch_secs: now + parsed.expires_in,
            token_type: parsed.token_type,
        })
    }

    pub fn build_authorization_url(&self, redirect_uri: &str, state: &str) -> Result<String, AuthError> {
        if self.config.client_id.trim().is_empty() {
            return Err(AuthError::MissingClientCredentials);
        }
        let mut url = Url::parse(GOOGLE_AUTH_URL)
            .map_err(|e| AuthError::Loopback(format!("invalid auth URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", DRIVE_SCOPE)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("state", state);
        Ok(url.into())
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<StoredToken, AuthError> {
        if self.config.client_id.trim().is_empty() || self.config.client_secret.trim().is_empty() {
            return Err(AuthError::MissingClientCredentials);
        }
        let resp = self
            .http
            .post(&self.token_url)
            .form(&[
                ("code", code),
                ("client_id", self.config.client_id.as_str()),
                ("client_secret", self.config.client_secret.as_str()),
                ("redirect_uri", redirect_uri),
                ("grant_type", "authorization_code"),
            ])
            .send()
            .await?;

        let status = resp.status().as_u16();
        let body = resp.text().await?;
        if !(200..300).contains(&status) {
            return Err(AuthError::TokenEndpoint { status, body });
        }

        let parsed: TokenEndpointResponse = serde_json::from_str(&body)?;
        let now = chrono::Utc::now().timestamp();
        let token = StoredToken {
            access_token: parsed.access_token,
            refresh_token: parsed.refresh_token,
            expires_at_epoch_secs: now + parsed.expires_in,
            token_type: parsed.token_type,
        };
        self.set_token(token.clone()).await?;
        Ok(token)
    }

    /// Runs a desktop loopback OAuth2 flow on `127.0.0.1:<redirect_port>`, prints the authorization URL,
    /// waits for the browser redirect with `code`, exchanges it, and saves the token with `0600` permissions.
    pub async fn run_loopback_flow(&self) -> Result<StoredToken, AuthError> {
        let listener = TcpListener::bind(("127.0.0.1", self.config.redirect_port))
            .await
            .map_err(|e| AuthError::Loopback(format!("failed to bind loopback port: {e}")))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| AuthError::Loopback(e.to_string()))?;
        let redirect_uri = format!("http://127.0.0.1:{}/callback", local_addr.port());
        let state = uuid::Uuid::new_v4().to_string();
        let auth_url = self.build_authorization_url(&redirect_uri, &state)?;

        println!("Open the following URL in your browser to authorize Google Drive access:\n\n  {auth_url}\n");

        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| AuthError::Loopback(format!("accept failed: {e}")))?;

        let mut reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .await
            .map_err(|e| AuthError::Loopback(format!("read request line failed: {e}")))?;

        let path_part = request_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| AuthError::Loopback("malformed HTTP request".to_string()))?;
        let full_url = Url::parse(&format!("http://127.0.0.1{path_part}"))
            .map_err(|e| AuthError::Loopback(format!("failed to parse callback URL: {e}")))?;

        let mut code_opt = None;
        let mut state_opt = None;
        for (k, v) in full_url.query_pairs() {
            if k == "code" {
                code_opt = Some(v.into_owned());
            } else if k == "state" {
                state_opt = Some(v.into_owned());
            }
        }

        if state_opt.as_deref() != Some(state.as_str()) {
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\n\r\nOAuth state mismatch.")
                .await;
            return Err(AuthError::Loopback("CSRF state mismatch".to_string()));
        }

        let code = code_opt.ok_or_else(|| AuthError::Loopback("missing code parameter".to_string()))?;
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body><h2>LocalSync Google Drive authorization complete.</h2><p>You may close this tab.</p></body></html>";
        let _ = stream.write_all(response).await;

        self.exchange_code(&code, &redirect_uri).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_token_with_0600_permissions() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("token.json");
        let tok = StoredToken {
            access_token: "ya29.test_access".to_string(),
            refresh_token: Some("1//test_refresh".to_string()),
            expires_at_epoch_secs: chrono::Utc::now().timestamp() + 3600,
            token_type: "Bearer".to_string(),
        };

        write_token_file_0600(&token_path, &tok).expect("write token");
        let loaded = read_token_file(&token_path).expect("read token");
        assert_eq!(loaded, tok);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}

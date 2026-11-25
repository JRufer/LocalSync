use serde::{Deserialize, Serialize};
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse TOML config: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("failed to serialize TOML config: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("root_folder_id must not be empty")]
    EmptyRootFolderId,
    #[error("local_root '{0}' must be an absolute path without parent traversals")]
    InvalidLocalRoot(PathBuf),
    #[error("db_path '{0}' must be a valid file path with a parent directory")]
    InvalidDbPath(PathBuf),
    #[error("tailscale_ip '{0}' is not a valid Tailscale CGNAT (100.64.0.0/10) or loopback IPv4 address")]
    InvalidTailscaleIp(String),
    #[error("max_concurrent_transfers must be between 1 and 64, got {0}")]
    InvalidConcurrency(usize),
    #[error("share '{name}' has invalid relative path '{rel_path}': {reason}")]
    InvalidSharePath {
        name: String,
        rel_path: String,
        reason: String,
    },
    #[error("duplicate share name '{0}' or relative path '{1}'")]
    DuplicateShare(String, String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub root_folder_id: String,
    pub local_root: PathBuf,
    pub db_path: PathBuf,
    pub tailscale_ip: Ipv4Addr,
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    #[serde(default = "default_trash_on_delete")]
    pub trash_on_delete: bool,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_max_concurrent_transfers")]
    pub max_concurrent_transfers: usize,
    #[serde(default)]
    pub oauth: OAuthConfig,
    #[serde(default)]
    pub samba: SambaConfig,
    #[serde(default)]
    pub shares: Vec<ShareConfigEntry>,
}

fn default_web_port() -> u16 {
    8420
}
fn default_trash_on_delete() -> bool {
    true
}
fn default_poll_interval_secs() -> u64 {
    15
}
fn default_debounce_ms() -> u64 {
    500
}
fn default_max_concurrent_transfers() -> usize {
    8
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthConfig {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default = "default_token_path")]
    pub token_path: PathBuf,
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: String::new(),
            token_path: default_token_path(),
            redirect_port: default_redirect_port(),
        }
    }
}

fn default_token_path() -> PathBuf {
    PathBuf::from("/var/lib/drive-syncd/token.json")
}
fn default_redirect_port() -> u16 {
    8899
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SambaConfig {
    #[serde(default = "default_shares_conf_path")]
    pub shares_conf_path: PathBuf,
    #[serde(default = "default_testparm_bin")]
    pub testparm_bin: String,
    #[serde(default = "default_smbcontrol_bin")]
    pub smbcontrol_bin: String,
    #[serde(default = "default_macos_fruit")]
    pub macos_fruit: bool,
    #[serde(default = "default_create_mask")]
    pub create_mask: String,
    #[serde(default = "default_directory_mask")]
    pub directory_mask: String,
    #[serde(default)]
    pub force_user: Option<String>,
    #[serde(default)]
    pub force_group: Option<String>,
}

impl Default for SambaConfig {
    fn default() -> Self {
        Self {
            shares_conf_path: default_shares_conf_path(),
            testparm_bin: default_testparm_bin(),
            smbcontrol_bin: default_smbcontrol_bin(),
            macos_fruit: default_macos_fruit(),
            create_mask: default_create_mask(),
            directory_mask: default_directory_mask(),
            force_user: None,
            force_group: None,
        }
    }
}

fn default_shares_conf_path() -> PathBuf {
    PathBuf::from("/etc/samba/shares-syncd.conf")
}
fn default_testparm_bin() -> String {
    "testparm".to_string()
}
fn default_smbcontrol_bin() -> String {
    "smbcontrol".to_string()
}
fn default_macos_fruit() -> bool {
    true
}
fn default_create_mask() -> String {
    "0664".to_string()
}
fn default_directory_mask() -> String {
    "0775".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareConfigEntry {
    pub share_name: String,
    pub rel_path: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

/// Returns true if the IPv4 address is in Tailscale's CGNAT block (100.64.0.0/10)
/// or loopback (127.0.0.0/8) for local testing.
pub fn is_valid_tailscale_or_loopback_ip(ip: Ipv4Addr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    let octets = ip.octets();
    // 100.64.0.0/10 means first octet == 100 and second octet in 64..=127
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

/// Normalizes a relative path under `local_root` into forward-slash separated components,
/// rejecting absolute paths, `..` traversal, or empty paths.
pub fn normalize_rel_path(rel_path: &str) -> Result<String, String> {
    let trimmed = rel_path.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err("relative path cannot be empty".to_string());
    }
    if rel_path.trim_start().starts_with('/') || rel_path.contains('\\') {
        return Err("path must be relative and use '/' separators".to_string());
    }

    let path = Path::new(trimmed);
    let mut parts = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(os) => {
                let s = os
                    .to_str()
                    .ok_or_else(|| "path contains invalid UTF-8".to_string())?;
                if s.is_empty() {
                    continue;
                }
                parts.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("parent directory '..' or root components are not allowed".to_string());
            }
        }
    }

    if parts.is_empty() {
        return Err("relative path cannot be empty".to_string());
    }

    Ok(parts.join("/"))
}

/// Resolves and canonicalizes `local_root.join(rel_path)`, ensuring the target stays strictly
/// inside `local_root` even if symlinks or non-existent leaf directories exist.
pub fn resolve_under_local_root(local_root: &Path, rel_path: &str) -> Result<PathBuf, String> {
    let norm = normalize_rel_path(rel_path)?;
    let canonical_root = if local_root.exists() {
        local_root
            .canonicalize()
            .map_err(|e| format!("failed to canonicalize local_root: {e}"))?
    } else {
        local_root.to_path_buf()
    };

    let joined = canonical_root.join(&norm);
    if joined.exists() {
        let canonical_target = joined
            .canonicalize()
            .map_err(|e| format!("failed to canonicalize target path: {e}"))?;
        if !canonical_target.starts_with(&canonical_root) {
            return Err(format!(
                "resolved path '{}' escapes local_root '{}'",
                canonical_target.display(),
                canonical_root.display()
            ));
        }
        Ok(canonical_target)
    } else if local_root.exists() {
        // Walk existing ancestors inside `local_root` to ensure no intermediate symlink escapes `local_root`
        let mut cur = joined.as_path();
        while let Some(parent) = cur.parent() {
            if !parent.starts_with(&canonical_root) {
                break;
            }
            if parent.exists() {
                let canon_parent = parent
                    .canonicalize()
                    .map_err(|e| format!("failed to canonicalize ancestor: {e}"))?;
                if !canon_parent.starts_with(&canonical_root) {
                    return Err(format!(
                        "ancestor '{}' escapes local_root '{}'",
                        canon_parent.display(),
                        canonical_root.display()
                    ));
                }
                break;
            }
            cur = parent;
        }
        Ok(joined)
    } else {
        Ok(joined)
    }
}

impl AppConfig {
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse_and_validate(&raw)
    }

    pub fn parse_and_validate(toml_str: &str) -> Result<Self, ConfigError> {
        let mut cfg: Self = toml::from_str(toml_str)?;
        cfg.validate_and_normalize()?;
        Ok(cfg)
    }

    pub fn validate_and_normalize(&mut self) -> Result<(), ConfigError> {
        if self.root_folder_id.trim().is_empty() {
            return Err(ConfigError::EmptyRootFolderId);
        }

        if !self.local_root.is_absolute()
            || self
                .local_root
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(ConfigError::InvalidLocalRoot(self.local_root.clone()));
        }

        if self.local_root.exists() {
            if let Ok(canon) = self.local_root.canonicalize() {
                self.local_root = canon;
            }
        }

        if !self.db_path.is_absolute()
            || self.db_path.parent().is_none()
            || self
                .db_path
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(ConfigError::InvalidDbPath(self.db_path.clone()));
        }

        if !is_valid_tailscale_or_loopback_ip(self.tailscale_ip) {
            return Err(ConfigError::InvalidTailscaleIp(
                self.tailscale_ip.to_string(),
            ));
        }

        if self.max_concurrent_transfers == 0 || self.max_concurrent_transfers > 64 {
            return Err(ConfigError::InvalidConcurrency(
                self.max_concurrent_transfers,
            ));
        }

        let mut seen_names = std::collections::HashSet::new();
        let mut seen_paths = std::collections::HashSet::new();

        for share in &mut self.shares {
            let norm = normalize_rel_path(&share.rel_path).map_err(|reason| {
                ConfigError::InvalidSharePath {
                    name: share.share_name.clone(),
                    rel_path: share.rel_path.clone(),
                    reason,
                }
            })?;
            resolve_under_local_root(&self.local_root, &norm).map_err(|reason| {
                ConfigError::InvalidSharePath {
                    name: share.share_name.clone(),
                    rel_path: share.rel_path.clone(),
                    reason,
                }
            })?;
            share.rel_path = norm;

            let lower_name = share.share_name.to_ascii_lowercase();
            if !seen_names.insert(lower_name) || !seen_paths.insert(share.rel_path.clone()) {
                return Err(ConfigError::DuplicateShare(
                    share.share_name.clone(),
                    share.rel_path.clone(),
                ));
            }
        }

        Ok(())
    }

    /// Atomically saves the updated configuration back to `path` (temp file + rename).
    pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        let serialized = toml::to_string_pretty(self)?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let tmp_path = parent.join(format!(
            ".tmp.{}.{}",
            uuid::Uuid::new_v4(),
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("config.toml")
        ));
        fs::write(&tmp_path, serialized.as_bytes()).map_err(|source| ConfigError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_toml() -> &'static str {
        r#"
        root_folder_id = "12345_folder"
        local_root = "/srv/drive-images"
        db_path = "/var/lib/drive-syncd/state.db"
        tailscale_ip = "100.64.0.10"

        [[shares]]
        share_name = "today"
        rel_path = "Today/Sub/"
        read_only = false
        "#
    }

    #[test]
    fn parses_valid_config_and_normalizes_share_path() {
        let cfg = AppConfig::parse_and_validate(valid_toml()).expect("valid config");
        assert_eq!(cfg.root_folder_id, "12345_folder");
        assert_eq!(cfg.shares.len(), 1);
        assert_eq!(cfg.shares[0].rel_path, "Today/Sub");
    }

    #[test]
    fn rejects_non_tailscale_ip() {
        let bad = valid_toml().replace("100.64.0.10", "192.168.1.50");
        let err = AppConfig::parse_and_validate(&bad).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidTailscaleIp(_)));
    }

    #[test]
    fn rejects_relative_local_root() {
        let bad = valid_toml().replace("/srv/drive-images", "relative/path");
        let err = AppConfig::parse_and_validate(&bad).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidLocalRoot(_)));
    }

    #[test]
    fn rejects_traversal_in_share_path() {
        let bad = valid_toml().replace("Today/Sub/", "../etc/passwd");
        let err = AppConfig::parse_and_validate(&bad).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidSharePath { .. }));
    }
}

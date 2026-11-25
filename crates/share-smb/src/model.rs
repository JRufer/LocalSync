use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use sync_core::config::{normalize_rel_path, resolve_under_local_root};
use thiserror::Error;

pub const MAX_SHARE_NAME_LEN: usize = 80;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ShareModelError {
    #[error("share name cannot be empty")]
    EmptyShareName,
    #[error("share name '{0}' exceeds maximum length of 80 characters")]
    ShareNameTooLong(String),
    #[error("share name '{0}' is reserved by Samba ([global], [homes], [printers], [ipc$])")]
    ReservedShareName(String),
    #[error("share name '{0}' contains invalid characters; only [a-z0-9-_] are permitted")]
    InvalidShareNameChars(String),
    #[error("invalid share relative path '{0}': {1}")]
    InvalidRelPath(String, String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareDefinition {
    pub share_name: String,
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub read_only: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OverlapKind {
    NestedInsideExisting {
        existing_share: String,
        existing_rel_path: String,
    },
    EnclosesExisting {
        existing_share: String,
        existing_rel_path: String,
    },
    ExactDuplicatePath {
        existing_share: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlapWarning {
    pub new_share: String,
    pub new_rel_path: String,
    pub kind: OverlapKind,
}

impl OverlapWarning {
    pub fn describe(&self) -> String {
        match &self.kind {
            OverlapKind::NestedInsideExisting {
                existing_share,
                existing_rel_path,
            } => format!(
                "Share '{}' ({}) is nested inside already-shared '{}' ({})",
                self.new_share, self.new_rel_path, existing_share, existing_rel_path
            ),
            OverlapKind::EnclosesExisting {
                existing_share,
                existing_rel_path,
            } => format!(
                "Share '{}' ({}) encloses already-shared '{}' ({})",
                self.new_share, self.new_rel_path, existing_share, existing_rel_path
            ),
            OverlapKind::ExactDuplicatePath { existing_share } => format!(
                "Share '{}' points to the same folder as '{}'",
                self.new_share, existing_share
            ),
        }
    }
}

/// Suggests a clean, lowercase `[a-z0-9-_]` share name (max 80 chars) from a relative folder path.
/// For example, `"Today"` -> `"today"`, `"2026/Vacation Photos!"` -> `"vacation_photos"`.
pub fn suggest_share_name(rel_path: &str, existing_names: &[String]) -> String {
    let leaf = rel_path
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("share");
    let mut sanitized = sanitize_share_name(leaf);
    if sanitized.is_empty() {
        sanitized = "share".to_string();
    }
    if is_reserved_name(&sanitized) {
        sanitized = format!("{sanitized}_share");
    }
    sanitized.truncate(MAX_SHARE_NAME_LEN);

    let lower_existing: Vec<String> = existing_names
        .iter()
        .map(|n| n.to_ascii_lowercase())
        .collect();
    if !lower_existing.contains(&sanitized) {
        return sanitized;
    }

    for idx in 2..1000 {
        let suffix = format!("_{idx}");
        let max_base = MAX_SHARE_NAME_LEN.saturating_sub(suffix.len());
        let base: String = sanitized.chars().take(max_base).collect();
        let candidate = format!("{base}{suffix}");
        if !lower_existing.contains(&candidate) {
            return candidate;
        }
    }
    sanitized
}

pub fn sanitize_share_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_sep = false;

    for ch in input.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_sep = false;
        } else if (ch == '-' || ch == '_' || ch.is_whitespace() || ch == '/') && !prev_sep && !out.is_empty() {
            out.push(if ch == '-' { '-' } else { '_' });
            prev_sep = true;
        }
    }

    let trimmed = out.trim_matches(|c| c == '_' || c == '-').to_string();
    trimmed.chars().take(MAX_SHARE_NAME_LEN).collect()
}

fn is_reserved_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "global" | "homes" | "printers" | "ipc$" | "print$"
    )
}

pub fn validate_share_name(name: &str) -> Result<String, ShareModelError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ShareModelError::EmptyShareName);
    }
    if trimmed.len() > MAX_SHARE_NAME_LEN {
        return Err(ShareModelError::ShareNameTooLong(trimmed.to_string()));
    }
    let lower = trimmed.to_ascii_lowercase();
    if is_reserved_name(&lower) {
        return Err(ShareModelError::ReservedShareName(trimmed.to_string()));
    }
    if !lower
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(ShareModelError::InvalidShareNameChars(trimmed.to_string()));
    }
    Ok(lower)
}

pub fn validate_share_definition(
    local_root: &Path,
    share_name: &str,
    rel_path: &str,
    read_only: bool,
    comment: Option<String>,
) -> Result<ShareDefinition, ShareModelError> {
    let valid_name = validate_share_name(share_name)?;
    let norm_rel = normalize_rel_path(rel_path)
        .map_err(|reason| ShareModelError::InvalidRelPath(rel_path.to_string(), reason))?;
    let abs_path = resolve_under_local_root(local_root, &norm_rel)
        .map_err(|reason| ShareModelError::InvalidRelPath(rel_path.to_string(), reason))?;

    Ok(ShareDefinition {
        share_name: valid_name,
        rel_path: norm_rel,
        abs_path,
        read_only,
        comment,
    })
}

/// Checks if `candidate` is nested inside or encloses any existing share.
pub fn check_overlap(
    candidate: &ShareDefinition,
    existing: &[ShareDefinition],
) -> Vec<OverlapWarning> {
    let mut warnings = Vec::new();
    let cand_prefix = format!("{}/", candidate.rel_path);

    for ex in existing {
        if ex.share_name.eq_ignore_ascii_case(&candidate.share_name) {
            continue;
        }
        let ex_prefix = format!("{}/", ex.rel_path);
        if candidate.rel_path == ex.rel_path {
            warnings.push(OverlapWarning {
                new_share: candidate.share_name.clone(),
                new_rel_path: candidate.rel_path.clone(),
                kind: OverlapKind::ExactDuplicatePath {
                    existing_share: ex.share_name.clone(),
                },
            });
        } else if candidate.rel_path.starts_with(&ex_prefix) {
            warnings.push(OverlapWarning {
                new_share: candidate.share_name.clone(),
                new_rel_path: candidate.rel_path.clone(),
                kind: OverlapKind::NestedInsideExisting {
                    existing_share: ex.share_name.clone(),
                    existing_rel_path: ex.rel_path.clone(),
                },
            });
        } else if ex.rel_path.starts_with(&cand_prefix) {
            warnings.push(OverlapWarning {
                new_share: candidate.share_name.clone(),
                new_rel_path: candidate.rel_path.clone(),
                kind: OverlapKind::EnclosesExisting {
                    existing_share: ex.share_name.clone(),
                    existing_rel_path: ex.rel_path.clone(),
                },
            });
        }
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_share_name_sanitization_and_uniqueness() {
        assert_eq!(suggest_share_name("Today", &[]), "today");
        assert_eq!(
            suggest_share_name("2026/Summer Vacation!", &[]),
            "summer_vacation"
        );
        assert_eq!(
            suggest_share_name("Today", &["today".to_string()]),
            "today_2"
        );
    }

    #[test]
    fn test_nested_and_enclosing_overlap_detection() {
        let root = PathBuf::from("/srv/drive-images");
        let existing = vec![
            validate_share_definition(&root, "2026", "2026", false, None).unwrap(),
            validate_share_definition(&root, "jan_trips", "2026/January/Trips", false, None)
                .unwrap(),
        ];

        let candidate =
            validate_share_definition(&root, "january", "2026/January", false, None).unwrap();
        let warnings = check_overlap(&candidate, &existing);
        assert_eq!(warnings.len(), 2);
        assert!(warnings
            .iter()
            .any(|w| matches!(w.kind, OverlapKind::NestedInsideExisting { .. })));
        assert!(warnings
            .iter()
            .any(|w| matches!(w.kind, OverlapKind::EnclosesExisting { .. })));
    }
}

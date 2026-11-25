pub mod model;
pub mod reload;
pub mod render;

use crate::model::{OverlapWarning, ShareDefinition, check_overlap, validate_share_definition};
use crate::reload::{ReloadOutcome, validate_and_reload_samba};
use crate::render::write_shares_conf_atomic;
use std::path::Path;
use sync_core::config::{AppConfig, ShareConfigEntry};
use sync_core::db::{ShareRow, StateDb, now_epoch_ms};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShareManagerError {
    #[error("share model validation error: {0}")]
    Model(#[from] model::ShareModelError),
    #[error("config error: {0}")]
    Config(#[from] sync_core::config::ConfigError),
    #[error("database error: {0}")]
    Db(#[from] sync_core::db::DbError),
    #[error("failed to render shares configuration: {0}")]
    Render(#[from] render::RenderError),
    #[error("Samba validation/reload failed: {0}")]
    Reload(#[from] reload::ReloadError),
    #[error("share '{0}' not found")]
    NotFound(String),
}

#[derive(Debug, Clone)]
pub struct ShareOperationResult {
    pub share: ShareDefinition,
    pub overlap_warnings: Vec<OverlapWarning>,
    pub reload_outcome: ReloadOutcome,
}

/// High-level helper that coordinates adding a share across `config.toml`, SQLite `shares`,
/// `/etc/samba/shares-syncd.conf`, and `testparm` + `smbcontrol smbd reload-config`.
pub fn add_or_update_share(
    config: &mut AppConfig,
    config_path: Option<&Path>,
    db: &StateDb,
    share_name: &str,
    rel_path: &str,
    read_only: bool,
    comment: Option<String>,
    skip_samba_commands: bool,
) -> Result<ShareOperationResult, ShareManagerError> {
    let def = validate_share_definition(&config.local_root, share_name, rel_path, read_only, comment)?;

    let existing_defs: Vec<ShareDefinition> = config
        .shares
        .iter()
        .filter(|s| !s.share_name.eq_ignore_ascii_case(&def.share_name))
        .filter_map(|s| {
            validate_share_definition(
                &config.local_root,
                &s.share_name,
                &s.rel_path,
                s.read_only,
                s.comment.clone(),
            )
            .ok()
        })
        .collect();

    let warnings = check_overlap(&def, &existing_defs);

    // Ensure target directory exists on disk so Samba testparm/smbd never complains about missing path
    if !def.abs_path.exists() {
        let _ = std::fs::create_dir_all(&def.abs_path);
    }

    // Update in-memory config
    config
        .shares
        .retain(|s| !s.share_name.eq_ignore_ascii_case(&def.share_name) && s.rel_path != def.rel_path);
    config.shares.push(ShareConfigEntry {
        share_name: def.share_name.clone(),
        rel_path: def.rel_path.clone(),
        read_only: def.read_only,
        comment: def.comment.clone(),
    });
    config.shares.sort_by(|a, b| a.share_name.cmp(&b.share_name));

    if let Some(cfg_file) = config_path {
        config.save_atomic(cfg_file)?;
    }

    db.upsert_share(&ShareRow {
        share_name: def.share_name.clone(),
        rel_path: def.rel_path.clone(),
        read_only: def.read_only,
        created_at: now_epoch_ms(),
    })?;

    write_shares_conf_atomic(config)?;
    let reload_outcome = validate_and_reload_samba(&config.samba, skip_samba_commands)?;

    db.record_event(
        "info",
        Some(&def.rel_path),
        &format!(
            "Configured SMB3 share [{}] -> {} (read_only={})",
            def.share_name, def.rel_path, def.read_only
        ),
    )?;

    Ok(ShareOperationResult {
        share: def,
        overlap_warnings: warnings,
        reload_outcome,
    })
}

/// Removes an SMB3 share by name from `config.toml`, SQLite `shares`, and `/etc/samba/shares-syncd.conf`,
/// then triggers a Samba reload.
pub fn remove_share_by_name(
    config: &mut AppConfig,
    config_path: Option<&Path>,
    db: &StateDb,
    share_name: &str,
    skip_samba_commands: bool,
) -> Result<ReloadOutcome, ShareManagerError> {
    let before = config.shares.len();
    config
        .shares
        .retain(|s| !s.share_name.eq_ignore_ascii_case(share_name));
    let removed_db = db.remove_share(share_name)?;

    if config.shares.len() == before && !removed_db {
        return Err(ShareManagerError::NotFound(share_name.to_string()));
    }

    if let Some(cfg_file) = config_path {
        config.save_atomic(cfg_file)?;
    }

    write_shares_conf_atomic(config)?;
    let outcome = validate_and_reload_samba(&config.samba, skip_samba_commands)?;

    db.record_event(
        "info",
        None,
        &format!("Removed SMB3 share [{share_name}]"),
    )?;
    Ok(outcome)
}

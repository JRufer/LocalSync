use crate::config::AppConfig;
use crate::db::{DbError, EntryRow, StateDb, SyncStatus, now_epoch_ms, parent_and_name};
use crate::drive_client::{DriveApi, DriveChange, DriveError, DriveFile};
use crate::watcher::{DebouncedLocalEvent, LocalFsChangeKind, is_ignored_temp_path};
use md5::{Digest, Md5};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use thiserror::Error;
use tokio::fs;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

const REMOTE_ECHO_SUPPRESS_MS: i64 = 5_000;
const LOCAL_ECHO_SUPPRESS_MS: i64 = 2_000;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("database error: {0}")]
    Db(#[from] DbError),
    #[error("Google Drive API error: {0}")]
    Drive(#[from] DriveError),
    #[error("filesystem I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncCycleReport {
    pub uploaded_files: usize,
    pub created_remote_dirs: usize,
    pub downloaded_files: usize,
    pub created_local_dirs: usize,
    pub deleted_remote: usize,
    pub deleted_local: usize,
    pub conflicts_resolved: usize,
}

#[derive(Debug, Clone)]
pub struct LocalFileHashes {
    pub md5_hex: String,
    pub blake3_hex: String,
    pub size_bytes: i64,
    pub modified_ms: i64,
}

/// Computes MD5, BLAKE3, size, and mtime for a local file.
pub async fn hash_local_file(path: &Path) -> Result<LocalFileHashes, std::io::Error> {
    let bytes = fs::read(path).await?;
    let meta = fs::metadata(path).await?;
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(now_epoch_ms);

    let mut md5_hasher = Md5::new();
    md5_hasher.update(&bytes);
    let md5_hex = hex::encode(md5_hasher.finalize());
    let blake3_hex = blake3::hash(&bytes).to_hex().to_string();

    Ok(LocalFileHashes {
        md5_hex,
        blake3_hex,
        size_bytes: bytes.len() as i64,
        modified_ms,
    })
}

/// Formats a conflict sibling path: `<stem>.conflict-<unix_epoch_secs><ext>` in the same folder.
pub fn build_conflict_rel_path(rel_path: &str, epoch_secs: i64) -> String {
    let (parent_opt, file_name) = parent_and_name(rel_path);
    let conflict_name = if let Some((stem, ext)) = file_name.rsplit_once('.') {
        if stem.is_empty() {
            format!("{file_name}.conflict-{epoch_secs}")
        } else {
            format!("{stem}.conflict-{epoch_secs}.{ext}")
        }
    } else {
        format!("{file_name}.conflict-{epoch_secs}")
    };

    match parent_opt {
        Some(parent) => format!("{parent}/{conflict_name}"),
        None => conflict_name,
    }
}

pub struct SyncEngine<D: DriveApi> {
    config: AppConfig,
    db: StateDb,
    drive: Arc<D>,
    transfer_semaphore: Arc<Semaphore>,
}

impl<D: DriveApi> SyncEngine<D> {
    pub fn new(config: AppConfig, db: StateDb, drive: Arc<D>) -> Self {
        let sem = Arc::new(Semaphore::new(config.max_concurrent_transfers.max(1)));
        Self {
            config,
            db,
            drive,
            transfer_semaphore: sem,
        }
    }

    pub fn db(&self) -> &StateDb {
        &self.db
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Runs an initial bootstrap or full reconciliation between `local_root` and Google Drive's
    /// `root_folder_id`, populating SQLite and saving the Drive `startPageToken`.
    pub async fn bootstrap_or_full_reconcile(&self) -> Result<SyncCycleReport, EngineError> {
        fs::create_dir_all(&self.config.local_root).await?;
        self.db.ensure_sync_state(&self.config.root_folder_id)?;

        let mut report = SyncCycleReport::default();
        let start_token = self.drive.get_start_page_token().await?;

        // 1. Walk remote tree breadth-first starting from root_folder_id
        let remote_entries = self.walk_remote_tree().await?;
        // 2. Walk local filesystem tree under local_root
        let local_paths = self.walk_local_tree().await?;

        let existing_rows: HashMap<String, EntryRow> = self
            .db
            .list_all_entries()?
            .into_iter()
            .map(|r| (r.rel_path.clone(), r))
            .collect();

        // Process all remote directories first (top-down)
        for (rel_path, rfile) in &remote_entries {
            if !rfile.is_dir() {
                continue;
            }
            let local_dir = self.config.local_root.join(rel_path);
            let created_locally = !local_dir.exists();
            if created_locally {
                fs::create_dir_all(&local_dir).await?;
                report.created_local_dirs += 1;
            }
            let now = now_epoch_ms();
            let (parent_rel_path, name) = parent_and_name(rel_path);
            let row = EntryRow {
                rel_path: rel_path.clone(),
                parent_rel_path,
                name,
                is_dir: true,
                drive_file_id: Some(rfile.id.clone()),
                drive_parent_id: rfile.parents.first().cloned(),
                md5_checksum: None,
                blake3_hash: None,
                size_bytes: 0,
                remote_modified_ms: rfile.modified_time_ms(),
                local_modified_ms: Some(now),
                sync_status: SyncStatus::Synced,
                last_synced_hash: None,
                suppress_remote_until_ms: 0,
                suppress_local_until_ms: if created_locally {
                    now + LOCAL_ECHO_SUPPRESS_MS
                } else {
                    0
                },
                updated_at: now,
            };
            self.db.upsert_entry(&row)?;
        }

        // Process remote files (download if missing locally, reconcile if present on both)
        for (rel_path, rfile) in &remote_entries {
            if rfile.is_dir() {
                continue;
            }
            let local_file = self.config.local_root.join(rel_path);
            let baseline = existing_rows.get(rel_path);

            if !local_file.exists() {
                // If we had a baseline row and it was deleted locally while remote did not change,
                // honor the local deletion; otherwise download from Drive.
                if let Some(prev) = baseline {
                    let remote_unchanged = prev.md5_checksum.is_some()
                        && prev.md5_checksum == rfile.md5_checksum;
                    if remote_unchanged && prev.sync_status == SyncStatus::PendingDelete {
                        self.delete_remote_entry(prev, &mut report).await?;
                        continue;
                    }
                }
                self.download_remote_file(rel_path, rfile, &mut report).await?;
            } else {
                // File exists both locally and on Drive
                let local_info = hash_local_file(&local_file).await?;
                let remote_md5 = rfile.md5_checksum.clone().unwrap_or_default();

                if !remote_md5.is_empty() && remote_md5.eq_ignore_ascii_case(&local_info.md5_hex) {
                    // Already identical
                    let now = now_epoch_ms();
                    let (parent_rel_path, name) = parent_and_name(rel_path);
                    self.db.upsert_entry(&EntryRow {
                        rel_path: rel_path.clone(),
                        parent_rel_path,
                        name,
                        is_dir: false,
                        drive_file_id: Some(rfile.id.clone()),
                        drive_parent_id: rfile.parents.first().cloned(),
                        md5_checksum: Some(local_info.md5_hex.clone()),
                        blake3_hash: Some(local_info.blake3_hex.clone()),
                        size_bytes: local_info.size_bytes,
                        remote_modified_ms: rfile.modified_time_ms(),
                        local_modified_ms: Some(local_info.modified_ms),
                        sync_status: SyncStatus::Synced,
                        last_synced_hash: Some(local_info.md5_hex),
                        suppress_remote_until_ms: 0,
                        suppress_local_until_ms: 0,
                        updated_at: now,
                    })?;
                } else {
                    self.reconcile_divergent_file(rel_path, &local_info, rfile, baseline, &mut report)
                        .await?;
                }
            }
        }

        // Process local-only directories and files (not yet on Drive)
        let remote_path_set: HashSet<&str> =
            remote_entries.iter().map(|(p, _)| p.as_str()).collect();
        for (rel_path, is_dir) in &local_paths {
            if remote_path_set.contains(rel_path.as_str()) {
                continue;
            }
            if is_ignored_temp_path(rel_path) {
                continue;
            }
            if *is_dir {
                self.ensure_remote_directory(rel_path, &mut report).await?;
            } else {
                self.upload_local_file(rel_path, &mut report).await?;
            }
        }

        let now = now_epoch_ms();
        self.db
            .update_sync_tokens(Some(&start_token), Some(now), Some(now))?;
        self.db.record_event(
            "info",
            None,
            &format!(
                "Full reconciliation finished: up={}, down={}, conflicts={}",
                report.uploaded_files, report.downloaded_files, report.conflicts_resolved
            ),
        )?;

        Ok(report)
    }

    /// Applies incremental remote changes from Google Drive `Changes.list` using the stored `start_page_token`.
    pub async fn run_remote_delta_sync(&self) -> Result<SyncCycleReport, EngineError> {
        let state = match self.db.get_sync_state()? {
            Some(s) => s,
            None => return self.bootstrap_or_full_reconcile().await,
        };
        let page_token = match state.start_page_token {
            Some(ref tok) if !tok.is_empty() => tok.clone(),
            _ => return self.bootstrap_or_full_reconcile().await,
        };

        let mut report = SyncCycleReport::default();
        let (changes, new_token) = self.drive.list_changes(&page_token).await?;

        for change in changes {
            self.apply_single_remote_change(change, &mut report).await?;
        }

        let now = now_epoch_ms();
        self.db
            .update_sync_tokens(Some(&new_token), None, Some(now))?;
        Ok(report)
    }

    /// Processes a batch of debounced local filesystem events from `notify`.
    pub async fn apply_local_events(
        &self,
        events: &[DebouncedLocalEvent],
    ) -> Result<SyncCycleReport, EngineError> {
        let mut report = SyncCycleReport::default();
        let now = now_epoch_ms();

        for ev in events {
            if is_ignored_temp_path(&ev.rel_path) {
                continue;
            }

            if let Some(existing) = self.db.get_entry(&ev.rel_path)? {
                if existing.suppress_local_until_ms > now {
                    debug!(
                        rel_path = %ev.rel_path,
                        suppress_until = existing.suppress_local_until_ms,
                        "Skipping suppressed local filesystem echo event"
                    );
                    continue;
                }
            }

            match ev.kind {
                LocalFsChangeKind::Upsert => {
                    let abs_path = self.config.local_root.join(&ev.rel_path);
                    if !abs_path.exists() {
                        self.handle_local_removal(&ev.rel_path, &mut report).await?;
                        continue;
                    }
                    let meta = fs::metadata(&abs_path).await?;
                    if meta.is_dir() {
                        self.ensure_remote_directory(&ev.rel_path, &mut report).await?;
                    } else {
                        self.upload_local_file(&ev.rel_path, &mut report).await?;
                    }
                }
                LocalFsChangeKind::Remove => {
                    self.handle_local_removal(&ev.rel_path, &mut report).await?;
                }
            }
        }

        Ok(report)
    }

    async fn apply_single_remote_change(
        &self,
        change: DriveChange,
        report: &mut SyncCycleReport,
    ) -> Result<(), EngineError> {
        let now = now_epoch_ms();
        let existing_by_id = self.db.get_entry_by_drive_id(&change.file_id)?;

        if let Some(ref existing) = existing_by_id {
            if existing.suppress_remote_until_ms > now {
                debug!(
                    rel_path = %existing.rel_path,
                    file_id = %change.file_id,
                    "Skipping suppressed remote echo event from Changes.list"
                );
                return Ok(());
            }
        }

        let is_removed_or_trashed = change.removed
            || change
                .file
                .as_ref()
                .map(|f| f.trashed)
                .unwrap_or(true);

        if is_removed_or_trashed {
            if let Some(existing) = existing_by_id {
                let local_target = self.config.local_root.join(&existing.rel_path);
                self.db.set_suppression(
                    &existing.rel_path,
                    None,
                    Some(now + LOCAL_ECHO_SUPPRESS_MS),
                )?;
                if local_target.exists() {
                    if existing.is_dir {
                        fs::remove_dir_all(&local_target).await?;
                    } else {
                        fs::remove_file(&local_target).await?;
                    }
                    report.deleted_local += 1;
                }
                self.db.delete_entry_and_descendants(&existing.rel_path)?;
                self.db.record_event(
                    "info",
                    Some(&existing.rel_path),
                    "Removed local entry after remote Drive deletion/trash",
                )?;
            }
            return Ok(());
        }

        let rfile = match change.file {
            Some(f) => f,
            None => return Ok(()),
        };

        // Resolve where this remote file sits under our root_folder_id
        let rel_path = match self.resolve_remote_rel_path(&rfile, existing_by_id.as_ref())? {
            Some(p) => p,
            None => return Ok(()), // Outside our tracked root folder
        };

        // Handle rename/move if existing entry had a different rel_path
        if let Some(ref prev) = existing_by_id {
            if prev.rel_path != rel_path {
                let old_local = self.config.local_root.join(&prev.rel_path);
                let new_local = self.config.local_root.join(&rel_path);
                if old_local.exists() {
                    if let Some(p) = new_local.parent() {
                        fs::create_dir_all(p).await?;
                    }
                    fs::rename(&old_local, &new_local).await?;
                }
                self.db.delete_entry_and_descendants(&prev.rel_path)?;
            }
        }

        if rfile.is_dir() {
            let local_dir = self.config.local_root.join(&rel_path);
            let created = !local_dir.exists();
            if created {
                fs::create_dir_all(&local_dir).await?;
                report.created_local_dirs += 1;
            }
            let (parent_rel_path, name) = parent_and_name(&rel_path);
            self.db.upsert_entry(&EntryRow {
                rel_path,
                parent_rel_path,
                name,
                is_dir: true,
                drive_file_id: Some(rfile.id.clone()),
                drive_parent_id: rfile.parents.first().cloned(),
                md5_checksum: None,
                blake3_hash: None,
                size_bytes: 0,
                remote_modified_ms: rfile.modified_time_ms(),
                local_modified_ms: Some(now),
                sync_status: SyncStatus::Synced,
                last_synced_hash: None,
                suppress_remote_until_ms: 0,
                suppress_local_until_ms: now + LOCAL_ECHO_SUPPRESS_MS,
                updated_at: now,
            })?;
            return Ok(());
        }

        let local_path = self.config.local_root.join(&rel_path);
        let baseline = self.db.get_entry(&rel_path)?;

        if !local_path.exists() {
            self.download_remote_file(&rel_path, &rfile, report).await?;
        } else {
            let local_info = hash_local_file(&local_path).await?;
            let remote_md5 = rfile.md5_checksum.clone().unwrap_or_default();
            if !remote_md5.is_empty() && remote_md5.eq_ignore_ascii_case(&local_info.md5_hex) {
                return Ok(());
            }
            self.reconcile_divergent_file(
                &rel_path,
                &local_info,
                &rfile,
                baseline.as_ref(),
                report,
            )
            .await?;
        }

        Ok(())
    }

    fn resolve_remote_rel_path(
        &self,
        rfile: &DriveFile,
        existing: Option<&EntryRow>,
    ) -> Result<Option<String>, EngineError> {
        if let Some(parent_id) = rfile.parents.first() {
            if parent_id == &self.config.root_folder_id {
                return Ok(Some(rfile.name.clone()));
            }
            if let Some(parent_row) = self.db.get_entry_by_drive_id(parent_id)? {
                return Ok(Some(format!("{}/{}", parent_row.rel_path, rfile.name)));
            }
        }
        Ok(existing.map(|e| e.rel_path.clone()))
    }

    async fn reconcile_divergent_file(
        &self,
        rel_path: &str,
        local_info: &LocalFileHashes,
        rfile: &DriveFile,
        baseline: Option<&EntryRow>,
        report: &mut SyncCycleReport,
    ) -> Result<(), EngineError> {
        let remote_md5 = rfile.md5_checksum.clone().unwrap_or_default();
        let last_synced = baseline.and_then(|b| b.last_synced_hash.as_deref());

        let local_changed = match last_synced {
            Some(base_hash) => !base_hash.eq_ignore_ascii_case(&local_info.md5_hex),
            None => true,
        };
        let remote_changed = match last_synced {
            Some(base_hash) => !remote_md5.is_empty() && !base_hash.eq_ignore_ascii_case(&remote_md5),
            None => true,
        };

        if local_changed && !remote_changed {
            // Only local changed since baseline -> upload local to Drive
            self.upload_local_file(rel_path, report).await?;
            return Ok(());
        }

        if remote_changed && !local_changed {
            // Only remote changed since baseline -> download remote to local
            self.download_remote_file(rel_path, rfile, report).await?;
            return Ok(());
        }

        // Both local and remote changed with different hashes (or no baseline and hashes differ):
        // Conflict resolution: preserve the divergent version at `<stem>.conflict-<unix_epoch_secs><ext>`
        // and keep the newest version at `rel_path`, then upload the conflict copy so both exist everywhere.
        let epoch_secs = chrono::Utc::now().timestamp();
        let conflict_rel = build_conflict_rel_path(rel_path, epoch_secs);
        let local_abs = self.config.local_root.join(rel_path);
        let conflict_abs = self.config.local_root.join(&conflict_rel);
        let remote_mtime = rfile.modified_time_ms().unwrap_or(0);

        warn!(
            rel_path = %rel_path,
            conflict_rel = %conflict_rel,
            "Bidirectional conflict detected; preserving both versions"
        );

        if local_info.modified_ms >= remote_mtime {
            // Local is newer (or active SMB write): keep local at `rel_path`, download remote to `conflict_rel`,
            // and update remote `rel_path` with local content.
            let _permit = self
                .transfer_semaphore
                .acquire()
                .await
                .expect("semaphore open");
            self.drive
                .download_file_atomic(&rfile.id, &conflict_abs, rfile.md5_checksum.as_deref())
                .await?;
            drop(_permit);

            self.upload_local_file(rel_path, report).await?;
            self.upload_local_file(&conflict_rel, report).await?;
        } else {
            // Remote is newer: copy local to `conflict_rel`, download remote to `rel_path`,
            // and upload `conflict_rel` to Drive.
            fs::copy(&local_abs, &conflict_abs).await?;
            self.download_remote_file(rel_path, rfile, report).await?;
            self.upload_local_file(&conflict_rel, report).await?;
        }

        // Mark conflict event in SQLite
        if let Some(mut entry) = self.db.get_entry(&conflict_rel)? {
            entry.sync_status = SyncStatus::Conflict;
            self.db.upsert_entry(&entry)?;
        }
        report.conflicts_resolved += 1;
        self.db.record_event(
            "conflict",
            Some(rel_path),
            &format!("Conflict detected on '{rel_path}'; divergent copy saved to '{conflict_rel}'"),
        )?;

        Ok(())
    }

    async fn download_remote_file(
        &self,
        rel_path: &str,
        rfile: &DriveFile,
        report: &mut SyncCycleReport,
    ) -> Result<(), EngineError> {
        let dest = self.config.local_root.join(rel_path);
        let permit = self
            .transfer_semaphore
            .acquire()
            .await
            .expect("semaphore open");
        let (blake3_hex, size_u64) = self
            .drive
            .download_file_atomic(&rfile.id, &dest, rfile.md5_checksum.as_deref())
            .await?;
        drop(permit);

        let now = now_epoch_ms();
        let (parent_rel_path, name) = parent_and_name(rel_path);
        let md5 = rfile.md5_checksum.clone();

        self.db.upsert_entry(&EntryRow {
            rel_path: rel_path.to_string(),
            parent_rel_path,
            name,
            is_dir: false,
            drive_file_id: Some(rfile.id.clone()),
            drive_parent_id: rfile.parents.first().cloned(),
            md5_checksum: md5.clone(),
            blake3_hash: Some(blake3_hex),
            size_bytes: size_u64 as i64,
            remote_modified_ms: rfile.modified_time_ms(),
            local_modified_ms: Some(now),
            sync_status: SyncStatus::Synced,
            last_synced_hash: md5,
            suppress_remote_until_ms: 0,
            suppress_local_until_ms: now + LOCAL_ECHO_SUPPRESS_MS,
            updated_at: now,
        })?;

        report.downloaded_files += 1;
        info!(rel_path = %rel_path, "Downloaded file atomically from Google Drive");
        Ok(())
    }

    async fn ensure_remote_directory(
        &self,
        rel_path: &str,
        report: &mut SyncCycleReport,
    ) -> Result<String, EngineError> {
        if let Some(existing) = self.db.get_entry(rel_path)? {
            if let Some(fid) = existing.drive_file_id {
                return Ok(fid);
            }
        }

        let (parent_rel_opt, name) = parent_and_name(rel_path);
        let parent_drive_id = match parent_rel_opt {
            Some(ref parent_rel) => Box::pin(self.ensure_remote_directory(parent_rel, report)).await?,
            None => self.config.root_folder_id.clone(),
        };

        let created = self.drive.create_folder(&parent_drive_id, &name).await?;
        let now = now_epoch_ms();

        self.db.upsert_entry(&EntryRow {
            rel_path: rel_path.to_string(),
            parent_rel_path: parent_rel_opt,
            name,
            is_dir: true,
            drive_file_id: Some(created.id.clone()),
            drive_parent_id: Some(parent_drive_id),
            md5_checksum: None,
            blake3_hash: None,
            size_bytes: 0,
            remote_modified_ms: created.modified_time_ms(),
            local_modified_ms: Some(now),
            sync_status: SyncStatus::Synced,
            last_synced_hash: None,
            suppress_remote_until_ms: now + REMOTE_ECHO_SUPPRESS_MS,
            suppress_local_until_ms: 0,
            updated_at: now,
        })?;

        report.created_remote_dirs += 1;
        Ok(created.id)
    }

    async fn upload_local_file(
        &self,
        rel_path: &str,
        report: &mut SyncCycleReport,
    ) -> Result<(), EngineError> {
        let abs_path = self.config.local_root.join(rel_path);
        if !abs_path.exists() {
            return Ok(());
        }

        let local_info = hash_local_file(&abs_path).await?;
        let existing = self.db.get_entry(rel_path)?;

        if let Some(ref row) = existing {
            if row.md5_checksum.as_deref() == Some(local_info.md5_hex.as_str())
                && row.drive_file_id.is_some()
            {
                return Ok(());
            }
        }

        let (parent_rel_opt, name) = parent_and_name(rel_path);
        let parent_drive_id = match parent_rel_opt {
            Some(ref parent_rel) => self.ensure_remote_directory(parent_rel, report).await?,
            None => self.config.root_folder_id.clone(),
        };

        let existing_file_id = existing.as_ref().and_then(|r| r.drive_file_id.as_deref());

        let permit = self
            .transfer_semaphore
            .acquire()
            .await
            .expect("semaphore open");
        let uploaded = self
            .drive
            .upload_file(&parent_drive_id, &name, existing_file_id, &abs_path)
            .await?;
        drop(permit);

        let now = now_epoch_ms();
        let remote_mtime = uploaded.modified_time_ms().or(Some(now));
        let md5_final = uploaded
            .md5_checksum
            .clone()
            .unwrap_or_else(|| local_info.md5_hex.clone());

        self.db.upsert_entry(&EntryRow {
            rel_path: rel_path.to_string(),
            parent_rel_path: parent_rel_opt,
            name,
            is_dir: false,
            drive_file_id: Some(uploaded.id),
            drive_parent_id: Some(parent_drive_id),
            md5_checksum: Some(md5_final.clone()),
            blake3_hash: Some(local_info.blake3_hex),
            size_bytes: local_info.size_bytes,
            remote_modified_ms: remote_mtime,
            local_modified_ms: Some(local_info.modified_ms),
            sync_status: SyncStatus::Synced,
            last_synced_hash: Some(md5_final),
            suppress_remote_until_ms: now + REMOTE_ECHO_SUPPRESS_MS,
            suppress_local_until_ms: 0,
            updated_at: now,
        })?;

        report.uploaded_files += 1;
        info!(rel_path = %rel_path, "Uploaded local file to Google Drive");
        Ok(())
    }

    async fn handle_local_removal(
        &self,
        rel_path: &str,
        report: &mut SyncCycleReport,
    ) -> Result<(), EngineError> {
        let now = now_epoch_ms();
        if let Some(existing) = self.db.get_entry(rel_path)? {
            if existing.suppress_local_until_ms > now {
                return Ok(());
            }
            self.delete_remote_entry(&existing, report).await?;
        }
        Ok(())
    }

    async fn delete_remote_entry(
        &self,
        entry: &EntryRow,
        report: &mut SyncCycleReport,
    ) -> Result<(), EngineError> {
        if let Some(ref fid) = entry.drive_file_id {
            let now = now_epoch_ms();
            self.db.set_suppression(
                &entry.rel_path,
                Some(now + REMOTE_ECHO_SUPPRESS_MS),
                None,
            )?;
            self.drive
                .delete_or_trash(fid, self.config.trash_on_delete)
                .await?;
            report.deleted_remote += 1;
        }
        self.db.delete_entry_and_descendants(&entry.rel_path)?;
        self.db.record_event(
            "info",
            Some(&entry.rel_path),
            if self.config.trash_on_delete {
                "Moved remote Drive item to trash after local deletion"
            } else {
                "Permanently deleted remote Drive item after local deletion"
            },
        )?;
        Ok(())
    }

    async fn walk_remote_tree(&self) -> Result<Vec<(String, DriveFile)>, EngineError> {
        let mut results = Vec::new();
        let mut queue: VecDeque<(String, Option<String>)> = VecDeque::new();
        queue.push_back((self.config.root_folder_id.clone(), None));

        while let Some((folder_id, prefix_opt)) = queue.pop_front() {
            let children = self.drive.list_children(&folder_id).await?;
            for child in children {
                if child.trashed {
                    continue;
                }
                let rel_path = match prefix_opt {
                    Some(ref p) => format!("{p}/{}", child.name),
                    None => child.name.clone(),
                };
                if child.is_dir() {
                    queue.push_back((child.id.clone(), Some(rel_path.clone())));
                }
                results.push((rel_path, child));
            }
        }

        Ok(results)
    }

    async fn walk_local_tree(&self) -> Result<Vec<(String, bool)>, EngineError> {
        let mut results = Vec::new();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();
        queue.push_back(self.config.local_root.clone());
        let mut visited_dirs = HashSet::new();

        while let Some(dir) = queue.pop_front() {
            let mut rd = match fs::read_dir(&dir).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Some(entry) = rd.next_entry().await? {
                let path = entry.path();
                let rel = match path.strip_prefix(&self.config.local_root) {
                    Ok(s) => match s.to_str() {
                        Some(st) => st.replace('\\', "/"),
                        None => continue,
                    },
                    Err(_) => continue,
                };
                if rel.is_empty() || is_ignored_temp_path(&rel) {
                    continue;
                }
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    if visited_dirs.insert(rel.clone()) {
                        results.push((rel, true));
                        queue.push_back(path);
                    }
                } else if ft.is_file() {
                    results.push((rel, false));
                }
            }
        }

        results.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OAuthConfig, SambaConfig};
    use std::net::Ipv4Addr;
    use std::sync::Mutex;
    use tempfile::tempdir;

    #[derive(Default)]
    struct MockDriveState {
        files: HashMap<String, (DriveFile, Vec<u8>)>,
        changes: Vec<DriveChange>,
        next_id: usize,
        page_token: usize,
    }

    #[derive(Default)]
    struct MockDrive {
        state: Mutex<MockDriveState>,
    }

    impl MockDrive {
        fn put_remote_file(&self, id: &str, parent_id: &str, name: &str, data: &[u8], mtime_ms: i64) {
            let mut st = self.state.lock().unwrap();
            let mut md5_hasher = Md5::new();
            md5_hasher.update(data);
            let md5_hex = hex::encode(md5_hasher.finalize());
            let dt = chrono::DateTime::from_timestamp_millis(mtime_ms)
                .unwrap()
                .to_rfc3339();
            let df = DriveFile {
                id: id.to_string(),
                name: name.to_string(),
                mime_type: "image/jpeg".to_string(),
                parents: vec![parent_id.to_string()],
                md5_checksum: Some(md5_hex),
                size: Some(data.len().to_string()),
                modified_time: Some(dt),
                trashed: false,
            };
            st.files.insert(id.to_string(), (df.clone(), data.to_vec()));
            st.changes.push(DriveChange {
                file_id: id.to_string(),
                removed: false,
                file: Some(df),
            });
        }
    }

    impl DriveApi for MockDrive {
        async fn get_start_page_token(&self) -> Result<String, DriveError> {
            let st = self.state.lock().unwrap();
            Ok(st.page_token.to_string())
        }

        async fn list_children(&self, folder_id: &str) -> Result<Vec<DriveFile>, DriveError> {
            let st = self.state.lock().unwrap();
            Ok(st
                .files
                .values()
                .filter(|(f, _)| !f.trashed && f.parents.iter().any(|p| p == folder_id))
                .map(|(f, _)| f.clone())
                .collect())
        }

        async fn list_changes(
            &self,
            _page_token: &str,
        ) -> Result<(Vec<DriveChange>, String), DriveError> {
            let mut st = self.state.lock().unwrap();
            let drained: Vec<DriveChange> = st.changes.drain(..).collect();
            st.page_token += 1;
            Ok((drained, st.page_token.to_string()))
        }

        async fn create_folder(&self, parent_id: &str, name: &str) -> Result<DriveFile, DriveError> {
            let mut st = self.state.lock().unwrap();
            st.next_id += 1;
            let id = format!("folder_{}", st.next_id);
            let df = DriveFile {
                id: id.clone(),
                name: name.to_string(),
                mime_type: crate::drive_client::DRIVE_FOLDER_MIME.to_string(),
                parents: vec![parent_id.to_string()],
                md5_checksum: None,
                size: None,
                modified_time: Some(chrono::Utc::now().to_rfc3339()),
                trashed: false,
            };
            st.files.insert(id, (df.clone(), Vec::new()));
            Ok(df)
        }

        async fn upload_file(
            &self,
            parent_id: &str,
            name: &str,
            existing_file_id: Option<&str>,
            local_path: &Path,
        ) -> Result<DriveFile, DriveError> {
            let bytes = std::fs::read(local_path)?;
            let mut md5_hasher = Md5::new();
            md5_hasher.update(&bytes);
            let md5_hex = hex::encode(md5_hasher.finalize());

            let mut st = self.state.lock().unwrap();
            let id = match existing_file_id {
                Some(fid) => fid.to_string(),
                None => {
                    st.next_id += 1;
                    format!("file_{}", st.next_id)
                }
            };
            let df = DriveFile {
                id: id.clone(),
                name: name.to_string(),
                mime_type: "application/octet-stream".to_string(),
                parents: vec![parent_id.to_string()],
                md5_checksum: Some(md5_hex),
                size: Some(bytes.len().to_string()),
                modified_time: Some(chrono::Utc::now().to_rfc3339()),
                trashed: false,
            };
            st.files.insert(id.clone(), (df.clone(), bytes));
            st.changes.push(DriveChange {
                file_id: id,
                removed: false,
                file: Some(df.clone()),
            });
            Ok(df)
        }

        async fn download_file_atomic(
            &self,
            file_id: &str,
            dest_path: &Path,
            _expected_md5: Option<&str>,
        ) -> Result<(String, u64), DriveError> {
            let bytes = {
                let st = self.state.lock().unwrap();
                st.files
                    .get(file_id)
                    .map(|(_, b)| b.clone())
                    .ok_or_else(|| DriveError::Api {
                        status: 404,
                        message: "not found".to_string(),
                    })?
            };
            if let Some(p) = dest_path.parent() {
                std::fs::create_dir_all(p)?;
            }
            let tmp = dest_path.with_extension("tmp_atomic");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, dest_path)?;
            let b3 = blake3::hash(&bytes).to_hex().to_string();
            Ok((b3, bytes.len() as u64))
        }

        async fn delete_or_trash(&self, file_id: &str, trash: bool) -> Result<(), DriveError> {
            let mut st = self.state.lock().unwrap();
            if trash {
                if let Some((f, _)) = st.files.get_mut(file_id) {
                    f.trashed = true;
                }
            } else {
                st.files.remove(file_id);
            }
            st.changes.push(DriveChange {
                file_id: file_id.to_string(),
                removed: !trash,
                file: None,
            });
            Ok(())
        }
    }

    fn test_config(local_root: PathBuf, db_path: PathBuf) -> AppConfig {
        AppConfig {
            root_folder_id: "root_images".to_string(),
            local_root,
            db_path,
            tailscale_ip: Ipv4Addr::new(100, 64, 0, 10),
            web_port: 8420,
            trash_on_delete: true,
            poll_interval_secs: 15,
            debounce_ms: 100,
            max_concurrent_transfers: 4,
            oauth: OAuthConfig::default(),
            samba: SambaConfig::default(),
            shares: vec![],
        }
    }

    #[tokio::test]
    async fn test_bidirectional_sync_echo_suppression_and_conflict_generation() {
        let dir = tempdir().unwrap();
        let local_root = dir.path().join("srv_images");
        let db_path = dir.path().join("state.db");
        std::fs::create_dir_all(&local_root).unwrap();

        let cfg = test_config(local_root.clone(), db_path.clone());
        let db = StateDb::open(&db_path).unwrap();
        let mock_drive = Arc::new(MockDrive::default());

        // 1. Seed a remote file and a local file
        mock_drive.put_remote_file("rem_1", "root_images", "remote_pic.jpg", b"remote_v1", now_epoch_ms());
        std::fs::write(local_root.join("local_pic.jpg"), b"local_v1").unwrap();

        let engine = SyncEngine::new(cfg, db.clone(), mock_drive.clone());
        let rep1 = engine.bootstrap_or_full_reconcile().await.unwrap();
        assert_eq!(rep1.downloaded_files, 1);
        assert_eq!(rep1.uploaded_files, 1);
        assert!(local_root.join("remote_pic.jpg").exists());

        // 2. Echo suppression check: running delta sync right after upload should skip our own upload echo
        let rep_echo = engine.run_remote_delta_sync().await.unwrap();
        assert_eq!(rep_echo.downloaded_files, 0);
        assert_eq!(rep_echo.conflicts_resolved, 0);

        // 3. Simulate concurrent conflicting edits on both local and remote for `remote_pic.jpg`
        // Clear suppression window so the test doesn't wait 5 seconds
        db.set_suppression("remote_pic.jpg", Some(0), Some(0)).unwrap();
        std::fs::write(local_root.join("remote_pic.jpg"), b"local_edited_v2").unwrap();
        mock_drive.put_remote_file(
            "rem_1",
            "root_images",
            "remote_pic.jpg",
            b"remote_edited_v2",
            now_epoch_ms() + 10_000,
        );

        let rep_conflict = engine.run_remote_delta_sync().await.unwrap();
        assert_eq!(rep_conflict.conflicts_resolved, 1);

        // Verify both main file and `.conflict-<ts>.jpg` exist in local_root
        let files_in_root: Vec<String> = std::fs::read_dir(&local_root)
            .unwrap()
            .filter_map(|e| e.ok().and_then(|de| de.file_name().into_string().ok()))
            .collect();
        assert!(files_in_root.iter().any(|n| n == "remote_pic.jpg"));
        assert!(
            files_in_root
                .iter()
                .any(|n| n.starts_with("remote_pic.conflict-") && n.ends_with(".jpg")),
            "expected conflict file in {files_in_root:?}"
        );
    }
}

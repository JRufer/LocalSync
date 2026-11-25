use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error initializing database directory: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Synced,
    PendingUpload,
    PendingDownload,
    PendingDelete,
    Conflict,
}

impl SyncStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::PendingUpload => "pending_upload",
            Self::PendingDownload => "pending_download",
            Self::PendingDelete => "pending_delete",
            Self::Conflict => "conflict",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "synced" => Self::Synced,
            "pending_upload" => Self::PendingUpload,
            "pending_download" => Self::PendingDownload,
            "pending_delete" => Self::PendingDelete,
            "conflict" => Self::Conflict,
            _ => Self::Synced,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncStateRow {
    pub root_folder_id: String,
    pub start_page_token: Option<String>,
    pub last_full_scan_at: Option<i64>,
    pub last_delta_sync_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryRow {
    pub rel_path: String,
    pub parent_rel_path: Option<String>,
    pub name: String,
    pub is_dir: bool,
    pub drive_file_id: Option<String>,
    pub drive_parent_id: Option<String>,
    pub md5_checksum: Option<String>,
    pub blake3_hash: Option<String>,
    pub size_bytes: i64,
    pub remote_modified_ms: Option<i64>,
    pub local_modified_ms: Option<i64>,
    pub sync_status: SyncStatus,
    pub last_synced_hash: Option<String>,
    pub suppress_remote_until_ms: i64,
    pub suppress_local_until_ms: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareRow {
    pub share_name: String,
    pub rel_path: String,
    pub read_only: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncEventRow {
    pub id: i64,
    pub level: String,
    pub rel_path: Option<String>,
    pub message: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderStats {
    pub rel_path: String,
    pub name: String,
    pub subfolder_count: usize,
    pub file_count: usize,
    pub total_bytes: u64,
    pub conflict_count: usize,
    pub pending_count: usize,
    pub share_name: Option<String>,
    pub share_read_only: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GlobalSyncSummary {
    pub total_files: usize,
    pub total_dirs: usize,
    pub total_bytes: u64,
    pub synced_count: usize,
    pub pending_upload_count: usize,
    pub pending_download_count: usize,
    pub pending_delete_count: usize,
    pub conflict_count: usize,
    pub active_shares_count: usize,
    pub last_full_scan_at: Option<i64>,
    pub last_delta_sync_at: Option<i64>,
}

#[derive(Clone)]
pub struct StateDb {
    conn: Arc<Mutex<Connection>>,
}

pub fn now_epoch_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn parent_and_name(rel_path: &str) -> (Option<String>, String) {
    let trimmed = rel_path.trim_matches('/');
    if let Some((parent, name)) = trimmed.rsplit_once('/') {
        (Some(parent.to_string()), name.to_string())
    } else {
        (None, trimmed.to_string())
    }
}

impl StateDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_schema()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<(), DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA temp_store = MEMORY;

            CREATE TABLE IF NOT EXISTS sync_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                root_folder_id TEXT NOT NULL,
                start_page_token TEXT,
                last_full_scan_at INTEGER,
                last_delta_sync_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS entries (
                rel_path TEXT PRIMARY KEY,
                parent_rel_path TEXT,
                name TEXT NOT NULL,
                is_dir INTEGER NOT NULL,
                drive_file_id TEXT,
                drive_parent_id TEXT,
                md5_checksum TEXT,
                blake3_hash TEXT,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                remote_modified_ms INTEGER,
                local_modified_ms INTEGER,
                sync_status TEXT NOT NULL,
                last_synced_hash TEXT,
                suppress_remote_until_ms INTEGER DEFAULT 0,
                suppress_local_until_ms INTEGER DEFAULT 0,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries(parent_rel_path, is_dir, name);
            CREATE INDEX IF NOT EXISTS idx_entries_drive_id ON entries(drive_file_id);
            CREATE INDEX IF NOT EXISTS idx_entries_status ON entries(sync_status);

            CREATE TABLE IF NOT EXISTS shares (
                share_name TEXT PRIMARY KEY,
                rel_path TEXT NOT NULL UNIQUE,
                read_only INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sync_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                level TEXT NOT NULL,
                rel_path TEXT,
                message TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn ensure_sync_state(&self, root_folder_id: &str) -> Result<SyncStateRow, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            r#"
            INSERT INTO sync_state (id, root_folder_id)
            VALUES (1, ?1)
            ON CONFLICT(id) DO UPDATE SET root_folder_id = excluded.root_folder_id
            "#,
            params![root_folder_id],
        )?;
        drop(conn);
        self.get_sync_state()
            .map(|opt| opt.expect("sync_state row must exist"))
    }

    pub fn get_sync_state(&self) -> Result<Option<SyncStateRow>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.query_row(
            "SELECT root_folder_id, start_page_token, last_full_scan_at, last_delta_sync_at FROM sync_state WHERE id = 1",
            [],
            |row| {
                Ok(SyncStateRow {
                    root_folder_id: row.get(0)?,
                    start_page_token: row.get(1)?,
                    last_full_scan_at: row.get(2)?,
                    last_delta_sync_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(DbError::from)
    }

    pub fn update_sync_tokens(
        &self,
        start_page_token: Option<&str>,
        full_scan_at: Option<i64>,
        delta_sync_at: Option<i64>,
    ) -> Result<(), DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            r#"
            UPDATE sync_state
            SET start_page_token = COALESCE(?1, start_page_token),
                last_full_scan_at = COALESCE(?2, last_full_scan_at),
                last_delta_sync_at = COALESCE(?3, last_delta_sync_at)
            WHERE id = 1
            "#,
            params![start_page_token, full_scan_at, delta_sync_at],
        )?;
        Ok(())
    }

    pub fn upsert_entry(&self, entry: &EntryRow) -> Result<(), DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        Self::upsert_entry_on_conn(&conn, entry)
    }

    pub fn upsert_entries_batch(&self, entries: &[EntryRow]) -> Result<(), DbError> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        for entry in entries {
            Self::upsert_entry_on_conn(&tx, entry)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn upsert_entry_on_conn(conn: &Connection, entry: &EntryRow) -> Result<(), DbError> {
        conn.execute(
            r#"
            INSERT INTO entries (
                rel_path, parent_rel_path, name, is_dir,
                drive_file_id, drive_parent_id, md5_checksum, blake3_hash,
                size_bytes, remote_modified_ms, local_modified_ms,
                sync_status, last_synced_hash,
                suppress_remote_until_ms, suppress_local_until_ms, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7, ?8,
                ?9, ?10, ?11,
                ?12, ?13,
                ?14, ?15, ?16
            )
            ON CONFLICT(rel_path) DO UPDATE SET
                parent_rel_path = excluded.parent_rel_path,
                name = excluded.name,
                is_dir = excluded.is_dir,
                drive_file_id = excluded.drive_file_id,
                drive_parent_id = excluded.drive_parent_id,
                md5_checksum = excluded.md5_checksum,
                blake3_hash = excluded.blake3_hash,
                size_bytes = excluded.size_bytes,
                remote_modified_ms = excluded.remote_modified_ms,
                local_modified_ms = excluded.local_modified_ms,
                sync_status = excluded.sync_status,
                last_synced_hash = excluded.last_synced_hash,
                suppress_remote_until_ms = excluded.suppress_remote_until_ms,
                suppress_local_until_ms = excluded.suppress_local_until_ms,
                updated_at = excluded.updated_at
            "#,
            params![
                entry.rel_path,
                entry.parent_rel_path,
                entry.name,
                if entry.is_dir { 1 } else { 0 },
                entry.drive_file_id,
                entry.drive_parent_id,
                entry.md5_checksum,
                entry.blake3_hash,
                entry.size_bytes,
                entry.remote_modified_ms,
                entry.local_modified_ms,
                entry.sync_status.as_str(),
                entry.last_synced_hash,
                entry.suppress_remote_until_ms,
                entry.suppress_local_until_ms,
                entry.updated_at,
            ],
        )?;
        Ok(())
    }

    fn map_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
        let is_dir_int: i64 = row.get(3)?;
        let status_str: String = row.get(11)?;
        Ok(EntryRow {
            rel_path: row.get(0)?,
            parent_rel_path: row.get(1)?,
            name: row.get(2)?,
            is_dir: is_dir_int != 0,
            drive_file_id: row.get(4)?,
            drive_parent_id: row.get(5)?,
            md5_checksum: row.get(6)?,
            blake3_hash: row.get(7)?,
            size_bytes: row.get(8)?,
            remote_modified_ms: row.get(9)?,
            local_modified_ms: row.get(10)?,
            sync_status: SyncStatus::from_str(&status_str),
            last_synced_hash: row.get(12)?,
            suppress_remote_until_ms: row.get(13)?,
            suppress_local_until_ms: row.get(14)?,
            updated_at: row.get(15)?,
        })
    }

    pub fn get_entry(&self, rel_path: &str) -> Result<Option<EntryRow>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.query_row(
            r#"
            SELECT rel_path, parent_rel_path, name, is_dir,
                   drive_file_id, drive_parent_id, md5_checksum, blake3_hash,
                   size_bytes, remote_modified_ms, local_modified_ms,
                   sync_status, last_synced_hash,
                   suppress_remote_until_ms, suppress_local_until_ms, updated_at
            FROM entries WHERE rel_path = ?1
            "#,
            params![rel_path],
            Self::map_entry_row,
        )
        .optional()
        .map_err(DbError::from)
    }

    pub fn get_entry_by_drive_id(&self, drive_file_id: &str) -> Result<Option<EntryRow>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.query_row(
            r#"
            SELECT rel_path, parent_rel_path, name, is_dir,
                   drive_file_id, drive_parent_id, md5_checksum, blake3_hash,
                   size_bytes, remote_modified_ms, local_modified_ms,
                   sync_status, last_synced_hash,
                   suppress_remote_until_ms, suppress_local_until_ms, updated_at
            FROM entries WHERE drive_file_id = ?1
            "#,
            params![drive_file_id],
            Self::map_entry_row,
        )
        .optional()
        .map_err(DbError::from)
    }

    pub fn list_all_entries(&self) -> Result<Vec<EntryRow>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            r#"
            SELECT rel_path, parent_rel_path, name, is_dir,
                   drive_file_id, drive_parent_id, md5_checksum, blake3_hash,
                   size_bytes, remote_modified_ms, local_modified_ms,
                   sync_status, last_synced_hash,
                   suppress_remote_until_ms, suppress_local_until_ms, updated_at
            FROM entries
            ORDER BY is_dir DESC, rel_path ASC
            "#,
        )?;
        let rows = stmt
            .query_map([], Self::map_entry_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_entries_by_status(&self, status: SyncStatus) -> Result<Vec<EntryRow>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            r#"
            SELECT rel_path, parent_rel_path, name, is_dir,
                   drive_file_id, drive_parent_id, md5_checksum, blake3_hash,
                   size_bytes, remote_modified_ms, local_modified_ms,
                   sync_status, last_synced_hash,
                   suppress_remote_until_ms, suppress_local_until_ms, updated_at
            FROM entries
            WHERE sync_status = ?1
            ORDER BY is_dir DESC, rel_path ASC
            "#,
        )?;
        let rows = stmt
            .query_map(params![status.as_str()], Self::map_entry_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_entry_and_descendants(&self, rel_path: &str) -> Result<usize, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let prefix = format!("{rel_path}/%");
        let affected = conn.execute(
            "DELETE FROM entries WHERE rel_path = ?1 OR rel_path LIKE ?2",
            params![rel_path, prefix],
        )?;
        Ok(affected)
    }

    pub fn set_suppression(
        &self,
        rel_path: &str,
        suppress_remote_until_ms: Option<i64>,
        suppress_local_until_ms: Option<i64>,
    ) -> Result<(), DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            r#"
            UPDATE entries
            SET suppress_remote_until_ms = COALESCE(?2, suppress_remote_until_ms),
                suppress_local_until_ms = COALESCE(?3, suppress_local_until_ms),
                updated_at = ?4
            WHERE rel_path = ?1
            "#,
            params![
                rel_path,
                suppress_remote_until_ms,
                suppress_local_until_ms,
                now_epoch_ms()
            ],
        )?;
        Ok(())
    }

    pub fn upsert_share(&self, share: &ShareRow) -> Result<(), DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            r#"
            INSERT INTO shares (share_name, rel_path, read_only, created_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(share_name) DO UPDATE SET
                rel_path = excluded.rel_path,
                read_only = excluded.read_only
            "#,
            params![
                share.share_name,
                share.rel_path,
                if share.read_only { 1 } else { 0 },
                share.created_at
            ],
        )?;
        Ok(())
    }

    pub fn remove_share(&self, share_name: &str) -> Result<bool, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let count = conn.execute(
            "DELETE FROM shares WHERE lower(share_name) = lower(?1)",
            params![share_name],
        )?;
        Ok(count > 0)
    }

    pub fn list_shares(&self) -> Result<Vec<ShareRow>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT share_name, rel_path, read_only, created_at FROM shares ORDER BY share_name ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let ro: i64 = row.get(2)?;
                Ok(ShareRow {
                    share_name: row.get(0)?,
                    rel_path: row.get(1)?,
                    read_only: ro != 0,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn record_event(
        &self,
        level: &str,
        rel_path: Option<&str>,
        message: &str,
    ) -> Result<(), DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO sync_events (level, rel_path, message, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![level, rel_path, message, now_epoch_ms()],
        )?;
        Ok(())
    }

    pub fn list_recent_events(&self, limit: usize) -> Result<Vec<SyncEventRow>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, level, rel_path, message, created_at FROM sync_events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(SyncEventRow {
                    id: row.get(0)?,
                    level: row.get(1)?,
                    rel_path: row.get(2)?,
                    message: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Lists child directories under `parent_rel_path` (`None` for top-level folders).
    pub fn list_child_directories(
        &self,
        parent_rel_path: Option<&str>,
    ) -> Result<Vec<EntryRow>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut rows = Vec::new();
        match parent_rel_path {
            Some(parent) => {
                let mut stmt = conn.prepare(
                    r#"
                    SELECT rel_path, parent_rel_path, name, is_dir,
                           drive_file_id, drive_parent_id, md5_checksum, blake3_hash,
                           size_bytes, remote_modified_ms, local_modified_ms,
                           sync_status, last_synced_hash,
                           suppress_remote_until_ms, suppress_local_until_ms, updated_at
                    FROM entries
                    WHERE parent_rel_path = ?1 AND is_dir = 1
                    ORDER BY name ASC
                    "#,
                )?;
                for r in stmt.query_map(params![parent], Self::map_entry_row)? {
                    rows.push(r?);
                }
            }
            None => {
                let mut stmt = conn.prepare(
                    r#"
                    SELECT rel_path, parent_rel_path, name, is_dir,
                           drive_file_id, drive_parent_id, md5_checksum, blake3_hash,
                           size_bytes, remote_modified_ms, local_modified_ms,
                           sync_status, last_synced_hash,
                           suppress_remote_until_ms, suppress_local_until_ms, updated_at
                    FROM entries
                    WHERE parent_rel_path IS NULL AND is_dir = 1
                    ORDER BY name ASC
                    "#,
                )?;
                for r in stmt.query_map([], Self::map_entry_row)? {
                    rows.push(r?);
                }
            }
        }
        Ok(rows)
    }

    /// Computes live descendant statistics (`subfolder_count`, `file_count`, `total_bytes`,
    /// `conflict_count`, `pending_count`, and share badge) for a specific directory using a
    /// recursive CTE over `idx_entries_parent`.
    pub fn get_folder_stats(&self, folder_rel_path: &str) -> Result<FolderStats, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let (_, name) = parent_and_name(folder_rel_path);

        let (subfolders, files, total_bytes, conflicts, pending): (i64, i64, i64, i64, i64) = conn
            .query_row(
                r#"
                WITH RECURSIVE descendants(rel_path, is_dir, size_bytes, sync_status) AS (
                    SELECT rel_path, is_dir, size_bytes, sync_status
                    FROM entries
                    WHERE parent_rel_path = ?1
                    UNION ALL
                    SELECT e.rel_path, e.is_dir, e.size_bytes, e.sync_status
                    FROM entries e
                    INNER JOIN descendants d ON e.parent_rel_path = d.rel_path
                )
                SELECT
                    COALESCE(SUM(CASE WHEN is_dir = 1 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN is_dir = 0 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN is_dir = 0 THEN size_bytes ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN sync_status = 'conflict' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN sync_status IN ('pending_upload', 'pending_download', 'pending_delete') THEN 1 ELSE 0 END), 0)
                FROM descendants
                "#,
                params![folder_rel_path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )?;

        let share_info: Option<(String, bool)> = conn
            .query_row(
                "SELECT share_name, read_only FROM shares WHERE rel_path = ?1",
                params![folder_rel_path],
                |row| {
                    let ro: i64 = row.get(1)?;
                    Ok((row.get(0)?, ro != 0))
                },
            )
            .optional()?;

        Ok(FolderStats {
            rel_path: folder_rel_path.to_string(),
            name,
            subfolder_count: subfolders as usize,
            file_count: files as usize,
            total_bytes: total_bytes.max(0) as u64,
            conflict_count: conflicts as usize,
            pending_count: pending as usize,
            share_name: share_info.as_ref().map(|s| s.0.clone()),
            share_read_only: share_info.as_ref().map(|s| s.1),
        })
    }

    /// Computes folder stats for all directories in the database in a single bottom-up pass.
    /// Designed to render the 10,000-entry folder tree in under a few milliseconds.
    pub fn compute_all_folder_stats(&self) -> Result<HashMap<String, FolderStats>, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut map: HashMap<String, FolderStats> = HashMap::new();

        let mut share_map: HashMap<String, (String, bool)> = HashMap::new();
        {
            let mut share_stmt =
                conn.prepare("SELECT rel_path, share_name, read_only FROM shares")?;
            let rows = share_stmt.query_map([], |row| {
                let ro: i64 = row.get(2)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    ro != 0,
                ))
            })?;
            for r in rows {
                let (rel_path, sname, ro) = r?;
                share_map.insert(rel_path, (sname, ro));
            }
        }

        let mut stmt = conn.prepare(
            "SELECT rel_path, parent_rel_path, name, is_dir, size_bytes, sync_status FROM entries",
        )?;
        let rows = stmt.query_map([], |row| {
            let is_dir_int: i64 = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                is_dir_int != 0,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut all_items = Vec::new();
        for r in rows {
            let item = r?;
            if item.3 {
                let share = share_map.get(&item.0);
                map.insert(
                    item.0.clone(),
                    FolderStats {
                        rel_path: item.0.clone(),
                        name: item.2.clone(),
                        subfolder_count: 0,
                        file_count: 0,
                        total_bytes: 0,
                        conflict_count: 0,
                        pending_count: 0,
                        share_name: share.map(|s| s.0.clone()),
                        share_read_only: share.map(|s| s.1),
                    },
                );
            }
            all_items.push(item);
        }

        for (_rel_path, parent_opt, _name, is_dir, size_bytes, status_str) in &all_items {
            let is_conflict = status_str == "conflict";
            let is_pending = matches!(
                status_str.as_str(),
                "pending_upload" | "pending_download" | "pending_delete"
            );
            let mut curr_parent = parent_opt.clone();
            while let Some(p) = curr_parent {
                if let Some(folder) = map.get_mut(&p) {
                    if *is_dir {
                        folder.subfolder_count += 1;
                    } else {
                        folder.file_count += 1;
                        folder.total_bytes += (*size_bytes).max(0) as u64;
                    }
                    if is_conflict {
                        folder.conflict_count += 1;
                    }
                    if is_pending {
                        folder.pending_count += 1;
                    }
                }
                curr_parent = parent_and_name(&p).0;
            }
        }

        Ok(map)
    }

    pub fn get_global_summary(&self) -> Result<GlobalSyncSummary, DbError> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let (
            total_files,
            total_dirs,
            total_bytes,
            synced,
            pending_up,
            pending_down,
            pending_del,
            conflicts,
        ): (i64, i64, i64, i64, i64, i64, i64, i64) = conn.query_row(
            r#"
            SELECT
                COALESCE(SUM(CASE WHEN is_dir = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN is_dir = 1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN is_dir = 0 THEN size_bytes ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN sync_status = 'synced' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN sync_status = 'pending_upload' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN sync_status = 'pending_download' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN sync_status = 'pending_delete' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN sync_status = 'conflict' THEN 1 ELSE 0 END), 0)
            FROM entries
            "#,
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )?;

        let active_shares_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM shares", [], |r| r.get(0))?;

        let state = conn
            .query_row(
                "SELECT last_full_scan_at, last_delta_sync_at FROM sync_state WHERE id = 1",
                [],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));

        Ok(GlobalSyncSummary {
            total_files: total_files as usize,
            total_dirs: total_dirs as usize,
            total_bytes: total_bytes.max(0) as u64,
            synced_count: synced as usize,
            pending_upload_count: pending_up as usize,
            pending_download_count: pending_down as usize,
            pending_delete_count: pending_del as usize,
            conflict_count: conflicts as usize,
            active_shares_count: active_shares_count as usize,
            last_full_scan_at: state.0,
            last_delta_sync_at: state.1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_10000_file_hierarchy_stats_and_queries() {
        let db = StateDb::open_in_memory().expect("in memory db");
        db.ensure_sync_state("root_123").expect("sync state");

        let now = now_epoch_ms();
        let mut batch = Vec::with_capacity(10_110);

        // Create 10 top-level folders (Folder_00 .. Folder_09), each with 10 subfolders,
        // and each subfolder with 100 files (10 * 10 * 100 = 10,000 files, each 2 MB).
        let file_size = 2 * 1024 * 1024_i64; // 2 MiB
        for i in 0..10 {
            let top_rel = format!("Folder_{i:02}");
            batch.push(EntryRow {
                rel_path: top_rel.clone(),
                parent_rel_path: None,
                name: top_rel.clone(),
                is_dir: true,
                drive_file_id: Some(format!("drv_top_{i}")),
                drive_parent_id: Some("root_123".to_string()),
                md5_checksum: None,
                blake3_hash: None,
                size_bytes: 0,
                remote_modified_ms: Some(now),
                local_modified_ms: Some(now),
                sync_status: SyncStatus::Synced,
                last_synced_hash: None,
                suppress_remote_until_ms: 0,
                suppress_local_until_ms: 0,
                updated_at: now,
            });

            for j in 0..10 {
                let sub_name = format!("Sub_{j:02}");
                let sub_rel = format!("{top_rel}/{sub_name}");
                batch.push(EntryRow {
                    rel_path: sub_rel.clone(),
                    parent_rel_path: Some(top_rel.clone()),
                    name: sub_name,
                    is_dir: true,
                    drive_file_id: Some(format!("drv_sub_{i}_{j}")),
                    drive_parent_id: Some(format!("drv_top_{i}")),
                    md5_checksum: None,
                    blake3_hash: None,
                    size_bytes: 0,
                    remote_modified_ms: Some(now),
                    local_modified_ms: Some(now),
                    sync_status: SyncStatus::Synced,
                    last_synced_hash: None,
                    suppress_remote_until_ms: 0,
                    suppress_local_until_ms: 0,
                    updated_at: now,
                });

                for k in 0..100 {
                    let fname = format!("img_{k:03}.jpg");
                    let frel = format!("{sub_rel}/{fname}");
                    let status = if i == 0 && j == 0 && k < 3 {
                        SyncStatus::Conflict
                    } else {
                        SyncStatus::Synced
                    };
                    batch.push(EntryRow {
                        rel_path: frel,
                        parent_rel_path: Some(sub_rel.clone()),
                        name: fname,
                        is_dir: false,
                        drive_file_id: Some(format!("drv_f_{i}_{j}_{k}")),
                        drive_parent_id: Some(format!("drv_sub_{i}_{j}")),
                        md5_checksum: Some("d41d8cd98f00b204e9800998ecf8427e".to_string()),
                        blake3_hash: Some("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262".to_string()),
                        size_bytes: file_size,
                        remote_modified_ms: Some(now),
                        local_modified_ms: Some(now),
                        sync_status: status,
                        last_synced_hash: Some("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262".to_string()),
                        suppress_remote_until_ms: 0,
                        suppress_local_until_ms: 0,
                        updated_at: now,
                    });
                }
            }
        }

        db.upsert_entries_batch(&batch).expect("batch insert 10k");
        db.upsert_share(&ShareRow {
            share_name: "folder_00".to_string(),
            rel_path: "Folder_00".to_string(),
            read_only: false,
            created_at: now,
        })
        .expect("insert share");

        let top_dirs = db.list_child_directories(None).expect("top dirs");
        assert_eq!(top_dirs.len(), 10);

        let stats_00 = db.get_folder_stats("Folder_00").expect("folder 00 stats");
        assert_eq!(stats_00.subfolder_count, 10);
        assert_eq!(stats_00.file_count, 1000);
        assert_eq!(stats_00.total_bytes, 1000 * (file_size as u64));
        assert_eq!(stats_00.conflict_count, 3);
        assert_eq!(stats_00.share_name.as_deref(), Some("folder_00"));

        let all_stats = db.compute_all_folder_stats().expect("all folder stats");
        assert_eq!(all_stats.len(), 110); // 10 top + 100 sub
        assert_eq!(all_stats["Folder_00"], stats_00);

        let summary = db.get_global_summary().expect("global summary");
        assert_eq!(summary.total_files, 10_000);
        assert_eq!(summary.total_dirs, 110);
        assert_eq!(summary.total_bytes, 10_000 * (file_size as u64));
        assert_eq!(summary.conflict_count, 3);
        assert_eq!(summary.active_shares_count, 1);
    }
}

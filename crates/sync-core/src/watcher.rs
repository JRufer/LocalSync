use crate::config::normalize_rel_path;
use notify::{Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("filesystem notify error: {0}")]
    Notify(#[from] notify::Error),
    #[error("local_root does not exist: {0}")]
    MissingRoot(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalFsChangeKind {
    Upsert,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebouncedLocalEvent {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub kind: LocalFsChangeKind,
}

/// Returns true if any segment of the path is a temporary hidden staging file (e.g. `.tmp.<uuid>.<name>`).
pub fn is_ignored_temp_path(rel_path: &str) -> bool {
    rel_path.split('/').any(|seg| {
        seg.starts_with(".tmp.")
            || seg.ends_with(".swp")
            || seg == ".DS_Store"
            || seg.starts_with(".goutputstream-")
    })
}

/// Starts a recursive filesystem watcher on `local_root` and coalesces rapid events
/// within `debounce_ms` into clean `DebouncedLocalEvent` batches.
pub fn spawn_debounced_watcher(
    local_root: PathBuf,
    debounce_ms: u64,
) -> Result<(RecommendedWatcher, mpsc::Receiver<Vec<DebouncedLocalEvent>>), WatcherError> {
    if !local_root.exists() {
        return Err(WatcherError::MissingRoot(local_root));
    }
    let canonical_root = local_root
        .canonicalize()
        .unwrap_or_else(|_| local_root.clone());

    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Event>();
    let (out_tx, out_rx) = mpsc::channel::<Vec<DebouncedLocalEvent>>(256);

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = raw_tx.send(event);
            }
        },
        NotifyConfig::default(),
    )?;

    watcher.watch(&canonical_root, RecursiveMode::Recursive)?;

    let root_for_task = canonical_root.clone();
    let window = Duration::from_millis(debounce_ms.max(50));

    tokio::spawn(async move {
        let mut pending: HashMap<String, (PathBuf, LocalFsChangeKind, Instant)> = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_millis((debounce_ms / 2).max(25)));

        loop {
            tokio::select! {
                maybe_ev = raw_rx.recv() => {
                    match maybe_ev {
                        Some(event) => {
                            let change_kind = match event.kind {
                                EventKind::Create(_) | EventKind::Modify(_) => Some(LocalFsChangeKind::Upsert),
                                EventKind::Remove(_) => Some(LocalFsChangeKind::Remove),
                                _ => None,
                            };
                            if let Some(kind) = change_kind {
                                for path in event.paths {
                                    if let Some(rel) = to_relative_under_root(&root_for_task, &path) {
                                        if !is_ignored_temp_path(&rel) {
                                            let effective_kind = if path.exists() {
                                                LocalFsChangeKind::Upsert
                                            } else {
                                                kind.clone()
                                            };
                                            pending.insert(rel, (path, effective_kind, Instant::now()));
                                        }
                                    }
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    if pending.is_empty() {
                        continue;
                    }
                    let now = Instant::now();
                    let ready_keys: Vec<String> = pending
                        .iter()
                        .filter_map(|(k, (_, _, ts))| {
                            if now.duration_since(*ts) >= window {
                                Some(k.clone())
                            } else {
                                None
                            }
                        })
                        .collect();

                    if !ready_keys.is_empty() {
                        let mut batch = Vec::with_capacity(ready_keys.len());
                        for k in ready_keys {
                            if let Some((abs_path, kind, _)) = pending.remove(&k) {
                                let final_kind = if abs_path.exists() {
                                    LocalFsChangeKind::Upsert
                                } else {
                                    kind
                                };
                                batch.push(DebouncedLocalEvent {
                                    rel_path: k,
                                    abs_path,
                                    kind: final_kind,
                                });
                            }
                        }
                        batch.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
                        if out_tx.send(batch).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok((watcher, out_rx))
}

fn to_relative_under_root(root: &Path, path: &Path) -> Option<String> {
    let stripped = path.strip_prefix(root).ok()?;
    let raw = stripped.to_str()?;
    normalize_rel_path(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_temp_and_swap_files() {
        assert!(is_ignored_temp_path("Today/.tmp.1234.img_001.jpg"));
        assert!(is_ignored_temp_path(".tmp.abcd.test.txt"));
        assert!(is_ignored_temp_path("Today/photo.jpg.swp"));
        assert!(!is_ignored_temp_path("Today/img_001.jpg"));
        assert!(!is_ignored_temp_path("Today/img_001.conflict-1700000000.jpg"));
    }
}

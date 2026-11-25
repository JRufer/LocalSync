use anyhow::{Context, Result};
use clap::Parser;
use share_smb::{reload::validate_and_reload_samba, render::write_shares_conf_atomic};
use share_ui::web::serve_tailscale_web_ui;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sync_core::{
    auth::TokenManager,
    config::AppConfig,
    db::{ShareRow, StateDb, now_epoch_ms},
    drive_client::DriveClient,
    engine::SyncEngine,
    watcher::spawn_debounced_watcher,
};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "drive-syncd",
    about = "Headless bidirectional Google Drive v3 <-> SMB3 over Tailscale sync daemon for CachyOS"
)]
struct Args {
    /// Path to TOML configuration file
    #[arg(short, long, default_value = "/etc/drive-smb-sync/config.toml")]
    config: PathBuf,

    /// Also start the Tailscale-bound Axum web UI inside the daemon process
    #[arg(long)]
    with_web_ui: bool,

    /// Skip calling testparm / smbcontrol on startup (useful for unprivileged dry-runs)
    #[arg(long)]
    no_samba_reload: bool,
    /// Number of Tokio worker threads (defaults to 2 for minimal background CPU/RAM overhead)
    #[arg(long, default_value_t = 2)]
    worker_threads: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.worker_threads.clamp(1, 16))
        .thread_name("drive-syncd-worker")
        .enable_all()
        .build()
        .context("failed to initialize Tokio runtime")?;

    runtime.block_on(async_main(args))
}

async fn async_main(args: Args) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let config = AppConfig::load_from_path(&args.config)
        .with_context(|| format!("failed to load configuration from {}", args.config.display()))?;

    std::fs::create_dir_all(&config.local_root).with_context(|| {
        format!(
            "failed to create local_root directory {}",
            config.local_root.display()
        )
    })?;

    let db = StateDb::open(&config.db_path)
        .with_context(|| format!("failed to open SQLite database {}", config.db_path.display()))?;
    db.ensure_sync_state(&config.root_folder_id)?;

    // Synchronize [[shares]] from config.toml into SQLite and render /etc/samba/shares-syncd.conf
    sync_configured_shares_to_db_and_samba(&config, &db, args.no_samba_reload)?;

    if args.with_web_ui {
        let web_cfg = config.clone();
        let web_cfg_path = Some(args.config.clone());
        let web_db = db.clone();
        let skip_smb = args.no_samba_reload;
        tokio::spawn(async move {
            if let Err(err) = serve_tailscale_web_ui(web_cfg, web_cfg_path, web_db, skip_smb).await {
                error!(error = %err, "Tailscale Axum web UI exited with error");
            }
        });
    }

    let tokens = TokenManager::from_config(config.oauth.clone());
    let drive_client = Arc::new(DriveClient::new(tokens)?);
    let engine = Arc::new(SyncEngine::new(config.clone(), db.clone(), drive_client));

    info!(
        local_root = %config.local_root.display(),
        root_folder_id = %config.root_folder_id,
        tailscale_ip = %config.tailscale_ip,
        "Starting initial bootstrap / reconciliation with Google Drive"
    );
    match engine.bootstrap_or_full_reconcile().await {
        Ok(rep) => info!(?rep, "Initial reconciliation completed"),
        Err(err) => warn!(
            error = %err,
            "Initial reconciliation encountered an error (will retry on poll loop)"
        ),
    }

    let (_watcher, mut fs_rx) =
        spawn_debounced_watcher(config.local_root.clone(), config.debounce_ms)
            .context("failed to start recursive filesystem watcher")?;

    let mut poll_ticker =
        tokio::time::interval(Duration::from_secs(config.poll_interval_secs.max(5)));
    let mut sighup = signal(SignalKind::hangup()).context("failed to register SIGHUP handler")?;

    info!("drive-syncd event loop running (watching filesystem + polling Drive Changes.list)");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT/SIGTERM; shutting down drive-syncd cleanly");
                break;
            }
            _ = sighup.recv() => {
                info!("Received SIGHUP; reloading config.toml and regenerating Samba shares");
                match AppConfig::load_from_path(&args.config) {
                    Ok(new_cfg) => {
                        if let Err(err) = sync_configured_shares_to_db_and_samba(&new_cfg, &db, args.no_samba_reload) {
                            error!(error = %err, "Failed to apply reloaded Samba shares");
                        }
                    }
                    Err(err) => error!(error = %err, "Failed to reload config.toml on SIGHUP"),
                }
            }
            Some(batch) = fs_rx.recv() => {
                if let Err(err) = engine.apply_local_events(&batch).await {
                    error!(error = %err, count = batch.len(), "Failed applying local filesystem events");
                    let _ = db.record_event("error", None, &format!("Local event sync error: {err}"));
                }
            }
            _ = poll_ticker.tick() => {
                if let Err(err) = engine.run_remote_delta_sync().await {
                    warn!(error = %err, "Remote delta sync cycle failed");
                    let _ = db.record_event("warn", None, &format!("Delta sync error: {err}"));
                }
            }
        }
    }

    Ok(())
}

fn sync_configured_shares_to_db_and_samba(
    config: &AppConfig,
    db: &StateDb,
    skip_samba_reload: bool,
) -> Result<()> {
    let now = now_epoch_ms();
    for s in &config.shares {
        let abs_dir = config.local_root.join(&s.rel_path);
        if !abs_dir.exists() {
            let _ = std::fs::create_dir_all(&abs_dir);
        }
        db.upsert_share(&ShareRow {
            share_name: s.share_name.clone(),
            rel_path: s.rel_path.clone(),
            read_only: s.read_only,
            created_at: now,
        })?;
    }

    if let Err(err) = write_shares_conf_atomic(config) {
        warn!(
            error = %err,
            path = %config.samba.shares_conf_path.display(),
            "Could not write Samba shares config on startup"
        );
    } else if let Err(err) = validate_and_reload_samba(&config.samba, skip_samba_reload) {
        warn!(error = %err, "Samba validation/reload warning on startup");
    }

    Ok(())
}

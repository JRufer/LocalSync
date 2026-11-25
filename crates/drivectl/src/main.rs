use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use share_smb::{add_or_update_share, model::suggest_share_name, remove_share_by_name};
use share_ui::{format_bytes, tray::run_system_tray, tui::run_tui, web::serve_tailscale_web_ui};
use std::path::PathBuf;
use std::sync::Arc;
use sync_core::{
    auth::TokenManager,
    config::AppConfig,
    db::StateDb,
    drive_client::DriveClient,
    engine::SyncEngine,
};

#[derive(Debug, Parser)]
#[command(
    name = "drivectl",
    about = "Control CLI, Ratatui folder browser, Tailscale Web UI, and SMB3 share manager for LocalSync"
)]
struct Cli {
    /// Path to config.toml
    #[arg(short, long, default_value = "/etc/drive-smb-sync/config.toml")]
    config: PathBuf,

    /// Skip running `testparm` and `smbcontrol` when modifying shares
    #[arg(long)]
    no_samba_reload: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run the desktop system tray applet (with integrated Share Manager Web UI & click-to-share menu)
    Tray {
        /// Also start the embedded Tailscale Web UI if drive-syncd is not already hosting it
        #[arg(long, default_value_t = true)]
        with_web: bool,
    },
    /// Launch interactive Ratatui folder tree browser and SMB3 share manager
    Ui,
    /// Start Tailscale-bound Axum web UI for browsing folders and toggling SMB3 shares
    Web,
    /// Show sync status summary, active SMB3 shares, and recent sync/conflict events
    Status {
        /// Output status as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage SMB3 shares exported over Tailscale
    Share {
        #[command(subcommand)]
        action: ShareAction,
    },
    /// Run a one-shot bidirectional sync cycle (full bootstrap or incremental delta sync)
    Sync {
        /// Force a full remote + local tree scan instead of an incremental delta sync
        #[arg(long)]
        full: bool,
    },
    /// Run Google OAuth2 desktop loopback authorization flow and persist 0600 token file
    Auth,
}

#[derive(Debug, Subcommand)]
enum ShareAction {
    /// Export a folder under local_root as an SMB3 share over Tailscale
    Add {
        /// Relative folder path under local_root (e.g. "Today" or "2026/Vacation")
        rel_path: String,
        /// Optional SMB3 share name (defaults to sanitized folder name, e.g. "today")
        #[arg(short, long)]
        name: Option<String>,
        /// Export share as read-only (default: false / read-write)
        #[arg(long)]
        read_only: bool,
        /// Optional share comment
        #[arg(long)]
        comment: Option<String>,
    },
    /// Remove an active SMB3 share by share name
    Rm {
        /// Share name to remove
        share_name: String,
    },
    /// List all configured SMB3 shares and their Tailscale mount URIs
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let mut config = AppConfig::load_from_path(&cli.config)
        .with_context(|| format!("failed to load config from {}", cli.config.display()))?;
    let db = StateDb::open(&config.db_path)
        .with_context(|| format!("failed to open SQLite DB at {}", config.db_path.display()))?;
    db.ensure_sync_state(&config.root_folder_id)?;

    match cli.command {
        Commands::Tray { with_web } => {
            std::fs::create_dir_all(&config.local_root).ok();
            if with_web {
                let web_cfg = config.clone();
                let web_cfg_path = Some(cli.config.clone());
                let web_db = db.clone();
                let skip_smb = cli.no_samba_reload;
                tokio::spawn(async move {
                    // If drive-syncd already bound the port, ignore the bind error cleanly
                    let _ = serve_tailscale_web_ui(web_cfg, web_cfg_path, web_db, skip_smb).await;
                });
            }
            let sync_cfg_path = cli.config.clone();
            let trigger = Arc::new(move || {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe)
                        .arg("--config")
                        .arg(&sync_cfg_path)
                        .arg("sync")
                        .spawn();
                }
            });
            run_system_tray(
                config,
                Some(cli.config),
                db,
                cli.no_samba_reload,
                Some(trigger),
            )
            .await?;
        }
        Commands::Ui => {
            run_tui(
                config,
                Some(cli.config),
                db,
                cli.no_samba_reload,
            )?;
        }
        Commands::Web => {
            println!(
                "Starting LocalSync Web UI strictly on http://{}:{}/",
                config.tailscale_ip, config.web_port
            );
            serve_tailscale_web_ui(
                config,
                Some(cli.config),
                db,
                cli.no_samba_reload,
            )
            .await?;
        }
        Commands::Status { json } => {
            let summary = db.get_global_summary()?;
            let shares = db.list_shares()?;
            let events = db.list_recent_events(10)?;

            if json {
                let payload = serde_json::json!({
                    "tailscale_ip": config.tailscale_ip.to_string(),
                    "local_root": config.local_root,
                    "root_folder_id": config.root_folder_id,
                    "summary": summary,
                    "shares": shares,
                    "recent_events": events,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("=== LocalSync Status (CachyOS Drive v3 <-> SMB3) ===");
                println!("Tailscale IP    : {}", config.tailscale_ip);
                println!("Local Root      : {}", config.local_root.display());
                println!("Drive Folder ID : {}", config.root_folder_id);
                println!(
                    "Indexed Files   : {} ({})",
                    summary.total_files,
                    format_bytes(summary.total_bytes)
                );
                println!("Indexed Folders : {}", summary.total_dirs);
                println!(
                    "Sync States     : {} synced, {} pending upload, {} pending download, {} conflicts",
                    summary.synced_count,
                    summary.pending_upload_count,
                    summary.pending_download_count,
                    summary.conflict_count
                );
                println!("\n--- Active SMB3 Shares ({}) ---", shares.len());
                if shares.is_empty() {
                    println!("  (no active shares; use `drivectl share add <Folder>` or `drivectl ui`)");
                } else {
                    for s in &shares {
                        let mode = if s.read_only { "RO" } else { "RW" };
                        println!(
                            "  [{:<16}] ({}) -> {:<24} | smb://{}/{} | \\\\{}\\{}",
                            s.share_name,
                            mode,
                            s.rel_path,
                            config.tailscale_ip,
                            s.share_name,
                            config.tailscale_ip,
                            s.share_name
                        );
                    }
                }
                println!("\n--- Recent Events ---");
                for ev in &events {
                    println!(
                        "  [{:<8}] {}{}",
                        ev.level.to_uppercase(),
                        ev.rel_path
                            .as_deref()
                            .map(|p| format!("({p}) "))
                            .unwrap_or_default(),
                        ev.message
                    );
                }
            }
        }
        Commands::Share { action } => match action {
            ShareAction::Add {
                rel_path,
                name,
                read_only,
                comment,
            } => {
                let existing_names: Vec<String> =
                    config.shares.iter().map(|s| s.share_name.clone()).collect();
                let share_name = name.unwrap_or_else(|| suggest_share_name(&rel_path, &existing_names));

                let res = add_or_update_share(
                    &mut config,
                    Some(&cli.config),
                    &db,
                    &share_name,
                    &rel_path,
                    read_only,
                    comment,
                    cli.no_samba_reload,
                )?;

                for w in &res.overlap_warnings {
                    eprintln!("WARNING: {}", w.describe());
                }
                println!(
                    "Configured SMB3 share [{}] -> {}",
                    res.share.share_name,
                    res.share.abs_path.display()
                );
                println!(
                    "  Linux / macOS : smb://{}/{}",
                    config.tailscale_ip, res.share.share_name
                );
                println!(
                    "  Windows       : \\\\{}\\{}",
                    config.tailscale_ip, res.share.share_name
                );
                println!("  Samba Status  : {}", res.reload_outcome.details);
            }
            ShareAction::Rm { share_name } => {
                let outcome = remove_share_by_name(
                    &mut config,
                    Some(&cli.config),
                    &db,
                    &share_name,
                    cli.no_samba_reload,
                )?;
                println!(
                    "Removed SMB3 share [{}] ({})",
                    share_name, outcome.details
                );
            }
            ShareAction::List => {
                if config.shares.is_empty() {
                    println!("No SMB3 shares configured.");
                } else {
                    for s in &config.shares {
                        let mode = if s.read_only { "RO" } else { "RW" };
                        println!(
                            "{:<18} {:<4} {:<28} smb://{}/{}",
                            s.share_name, mode, s.rel_path, config.tailscale_ip, s.share_name
                        );
                    }
                }
            }
        },
        Commands::Sync { full } => {
            let tokens = TokenManager::from_config(config.oauth.clone());
            let drive = Arc::new(DriveClient::new(tokens)?);
            let engine = SyncEngine::new(config, db, drive);

            let report = if full {
                engine.bootstrap_or_full_reconcile().await?
            } else {
                engine.run_remote_delta_sync().await?
            };
            println!(
                "Sync cycle complete: uploaded={}, downloaded={}, remote_dirs_created={}, local_dirs_created={}, deleted_remote={}, deleted_local={}, conflicts={}",
                report.uploaded_files,
                report.downloaded_files,
                report.created_remote_dirs,
                report.created_local_dirs,
                report.deleted_remote,
                report.deleted_local,
                report.conflicts_resolved
            );
        }
        Commands::Auth => {
            let tokens = TokenManager::from_config(config.oauth.clone());
            let stored = tokens.run_loopback_flow().await?;
            println!(
                "OAuth2 token saved with 0600 permissions to {} (expires at epoch {})",
                config.oauth.token_path.display(),
                stored.expires_at_epoch_secs
            );
        }
    }

    Ok(())
}

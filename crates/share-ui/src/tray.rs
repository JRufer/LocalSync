use crate::format_bytes;
use anyhow::Result;
use ksni::{MenuItem, Tray, TrayMethods, menu::StandardItem};
use share_smb::{add_or_update_share, model::suggest_share_name, remove_share_by_name};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use sync_core::{
    config::AppConfig,
    db::{FolderStats, GlobalSyncSummary, ShareRow, StateDb},
};
use tracing::{error, info, warn};

/// Callback triggered when the user clicks "Sync Now" in the system tray menu.
pub type SyncTriggerFn = Arc<dyn Fn() + Send + Sync>;

pub struct LocalSyncTray {
    pub config: AppConfig,
    pub config_path: Option<PathBuf>,
    pub db: StateDb,
    pub skip_samba_commands: bool,
    pub web_url: String,
    pub summary: GlobalSyncSummary,
    pub shares: Vec<ShareRow>,
    pub top_folders: Vec<FolderStats>,
    pub sync_trigger: Option<SyncTriggerFn>,
}

impl LocalSyncTray {
    pub fn new(
        config: AppConfig,
        config_path: Option<PathBuf>,
        db: StateDb,
        skip_samba_commands: bool,
        sync_trigger: Option<SyncTriggerFn>,
    ) -> Self {
        let web_url = format!("http://{}:{}/", config.tailscale_ip, config.web_port);
        let mut tray = Self {
            config,
            config_path,
            db,
            skip_samba_commands,
            web_url,
            summary: GlobalSyncSummary::default(),
            shares: Vec::new(),
            top_folders: Vec::new(),
            sync_trigger,
        };
        tray.reload_snapshot();
        tray
    }

    pub fn reload_snapshot(&mut self) {
        if let Some(ref path) = self.config_path {
            if let Ok(fresh_cfg) = AppConfig::load_from_path(path) {
                self.web_url = format!("http://{}:{}/", fresh_cfg.tailscale_ip, fresh_cfg.web_port);
                self.config = fresh_cfg;
            }
        }
        self.summary = self.db.get_global_summary().unwrap_or_default();
        self.shares = self.db.list_shares().unwrap_or_default();
        if let Ok(stats_map) = self.db.compute_all_folder_stats() {
            let mut folders: Vec<FolderStats> = stats_map.into_values().collect();
            folders.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
            folders.truncate(25);
            self.top_folders = folders;
        }
    }

    fn open_web_manager(&self) {
        let _ = Command::new("xdg-open").arg(&self.web_url).spawn();
    }

    fn open_local_folder(&self, rel_path: Option<&str>) {
        let target = match rel_path {
            Some(r) => self.config.local_root.join(r),
            None => self.config.local_root.clone(),
        };
        let _ = std::fs::create_dir_all(&target);
        let _ = Command::new("xdg-open").arg(target).spawn();
    }

    fn launch_terminal_tui(&self) {
        let cfg_arg = self
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "/etc/drive-smb-sync/config.toml".to_string());

        let drivectl_bin = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|dir| dir.join("drivectl")))
            .filter(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("drivectl"));

        let bin_str = drivectl_bin.display().to_string();

        // Try KDE Konsole first, then Alacritty, Kitty, Foot, or xdg-terminal-exec
        let candidates: &[(&str, &[&str])] = &[
            ("konsole", &["-e", &bin_str, "--config", &cfg_arg, "ui"]),
            ("alacritty", &["-e", &bin_str, "--config", &cfg_arg, "ui"]),
            ("kitty", &[&bin_str, "--config", &cfg_arg, "ui"]),
            ("foot", &[&bin_str, "--config", &cfg_arg, "ui"]),
        ];

        for (term, args) in candidates {
            if Command::new(term).args(*args).spawn().is_ok() {
                return;
            }
        }
        // Fallback to Web UI if no terminal emulator is found
        self.open_web_manager();
    }

    fn toggle_share_for_folder(&mut self, rel_path: &str, currently_shared: Option<&str>) {
        if let Some(share_name) = currently_shared {
            if let Err(err) = remove_share_by_name(
                &mut self.config,
                self.config_path.as_deref(),
                &self.db,
                share_name,
                self.skip_samba_commands,
            ) {
                error!(error = %err, "Failed to unshare folder from tray");
            } else {
                info!(share_name, "Removed SMB3 share from system tray");
            }
        } else {
            let existing: Vec<String> = self
                .config
                .shares
                .iter()
                .map(|s| s.share_name.clone())
                .collect();
            let suggested = suggest_share_name(rel_path, &existing);
            if let Err(err) = add_or_update_share(
                &mut self.config,
                self.config_path.as_deref(),
                &self.db,
                &suggested,
                rel_path,
                false,
                None,
                self.skip_samba_commands,
            ) {
                error!(error = %err, "Failed to share folder from tray");
            } else {
                info!(share_name = %suggested, rel_path, "Created SMB3 share from system tray");
            }
        }
        self.reload_snapshot();
    }

    fn toggle_share_read_only(&mut self, share: &ShareRow) {
        if let Err(err) = add_or_update_share(
            &mut self.config,
            self.config_path.as_deref(),
            &self.db,
            &share.share_name,
            &share.rel_path,
            !share.read_only,
            None,
            self.skip_samba_commands,
        ) {
            error!(error = %err, "Failed to toggle share read-only mode from tray");
        }
        self.reload_snapshot();
    }
}

impl Tray for LocalSyncTray {
    fn id(&self) -> String {
        "localsync-drive-smb".to_string()
    }

    fn title(&self) -> String {
        format!(
            "LocalSync ({} shares · {} files)",
            self.shares.len(),
            self.summary.total_files
        )
    }

    fn icon_name(&self) -> String {
        if self.summary.conflict_count > 0 {
            "dialog-warning".to_string()
        } else {
            "folder-cloud".to_string()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
            title: format!("LocalSync · Tailscale {}", self.config.tailscale_ip),
            description: format!(
                "{} files ({}) · {} active SMB3 shares · {} conflicts\nClick to add or edit SMB3 shares",
                self.summary.total_files,
                format_bytes(self.summary.total_bytes),
                self.shares.len(),
                self.summary.conflict_count
            ),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // Left-clicking the tray icon opens the visual Share Manager in the default browser
        self.open_web_manager();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        // Status Header
        items.push(
            StandardItem {
                label: format!(
                    "LocalSync · {} ({} files, {})",
                    self.config.tailscale_ip,
                    self.summary.total_files,
                    format_bytes(self.summary.total_bytes)
                ),
                enabled: false,
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // Primary Actions: Open Web UI or Terminal TUI
        items.push(
            StandardItem {
                label: "🌐 Add / Edit Shares (Web Manager)...".to_string(),
                icon_name: "applications-internet".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.open_web_manager();
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(
            StandardItem {
                label: "🖥 Open Terminal Folder Tree (TUI)...".to_string(),
                icon_name: "utilities-terminal".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.launch_terminal_tui();
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(
            StandardItem {
                label: format!("📂 Open Local Root ({})", self.config.local_root.display()),
                icon_name: "folder-open".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.open_local_folder(None);
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // Active SMB3 Shares Submenu (Edit Read/Write vs Read-Only, Open Folder, or Unshare)
        let mut share_subitems: Vec<MenuItem<Self>> = Vec::new();
        if self.shares.is_empty() {
            share_subitems.push(
                StandardItem {
                    label: "(No active SMB3 shares yet)".to_string(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        } else {
            for sh in &self.shares {
                let sh_clone1 = sh.clone();
                let sh_clone2 = sh.clone();
                let sh_clone3 = sh.clone();
                let mode_str = if sh.read_only { "Read-Only" } else { "Read/Write" };
                let toggle_label = if sh.read_only {
                    "Switch to Read/Write (RW)"
                } else {
                    "Switch to Read-Only (RO)"
                };

                let actions: Vec<MenuItem<Self>> = vec![
                    StandardItem {
                        label: format!(
                            "URI: smb://{}/{}  (\\\\{}\\{})",
                            self.config.tailscale_ip,
                            sh.share_name,
                            self.config.tailscale_ip,
                            sh.share_name
                        ),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "📂 Open Folder in File Manager".to_string(),
                        activate: Box::new(move |this: &mut Self| {
                            this.open_local_folder(Some(&sh_clone1.rel_path));
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: toggle_label.to_string(),
                        activate: Box::new(move |this: &mut Self| {
                            this.toggle_share_read_only(&sh_clone2);
                        }),
                        ..Default::default()
                    }
                    .into(),
                    MenuItem::Separator,
                    StandardItem {
                        label: "❌ Remove SMB3 Share".to_string(),
                        activate: Box::new(move |this: &mut Self| {
                            this.toggle_share_for_folder(
                                &sh_clone3.rel_path,
                                Some(&sh_clone3.share_name),
                            );
                        }),
                        ..Default::default()
                    }
                    .into(),
                ];

                share_subitems.push(
                    ksni::menu::SubMenu {
                        label: format!(
                            "🟢 [{}] → {} ({})",
                            sh.share_name, sh.rel_path, mode_str
                        ),
                        submenu: actions,
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }

        items.push(
            ksni::menu::SubMenu {
                label: format!("📡 Active SMB3 Shares ({})", self.shares.len()),
                submenu: share_subitems,
                ..Default::default()
            }
            .into(),
        );

        // Quick-Toggle Folder Share Submenu directly from the tray
        let mut folder_subitems: Vec<MenuItem<Self>> = Vec::new();
        if self.top_folders.is_empty() {
            folder_subitems.push(
                StandardItem {
                    label: "(No folders indexed yet — run Sync or add folders)".to_string(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        } else {
            for f in &self.top_folders {
                let rel = f.rel_path.clone();
                let current_share = f.share_name.clone();
                let label = match &current_share {
                    Some(sn) => format!(
                        "☑ {}  [Shared as '{}'] ({} files)",
                        f.rel_path, sn, f.file_count
                    ),
                    None => format!("☐ {}  ({} files, {})", f.rel_path, f.file_count, format_bytes(f.total_bytes)),
                };
                folder_subitems.push(
                    StandardItem {
                        label,
                        activate: Box::new(move |this: &mut Self| {
                            this.toggle_share_for_folder(&rel, current_share.as_deref());
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }

        items.push(
            ksni::menu::SubMenu {
                label: "⚡ Quick-Share / Unshare Folders".to_string(),
                submenu: folder_subitems,
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        items.push(
            StandardItem {
                label: "🔄 Sync Now with Google Drive".to_string(),
                icon_name: "view-refresh".to_string(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(ref trig) = this.sync_trigger {
                        trig();
                    }
                    this.reload_snapshot();
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(
            StandardItem {
                label: "Quit Tray".to_string(),
                icon_name: "application-exit".to_string(),
                activate: Box::new(|_| {
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

/// Spawns the KDE/Wayland StatusNotifierItem system tray icon and periodically refreshes its menu state.
pub async fn run_system_tray(
    config: AppConfig,
    config_path: Option<PathBuf>,
    db: StateDb,
    skip_samba_commands: bool,
    sync_trigger: Option<SyncTriggerFn>,
) -> Result<()> {
    let tray = LocalSyncTray::new(config, config_path, db, skip_samba_commands, sync_trigger);
    let handle = match tray.spawn().await {
        Ok(h) => h,
        Err(err) => {
            warn!(error = %err, "Could not register StatusNotifierItem system tray on D-Bus");
            return Err(anyhow::anyhow!("Failed to spawn system tray: {err}"));
        }
    };

    info!("LocalSync system tray icon active in desktop panel");

    let mut ticker = tokio::time::interval(Duration::from_secs(4));
    loop {
        ticker.tick().await;
        handle
            .update(|tray_inst: &mut LocalSyncTray| {
                tray_inst.reload_snapshot();
            })
            .await;
    }
}

use std::path::Path;
use std::process::Command;
use sync_core::config::SambaConfig;
use thiserror::Error;
use tracing::{info, warn};

#[derive(Debug, Error)]
pub enum ReloadError {
    #[error("testparm validation failed (exit {code}): {stderr}")]
    TestparmFailed { code: i32, stderr: String },
    #[error("smbcontrol reload-config failed (exit {code}): {stderr}")]
    SmbcontrolFailed { code: i32, stderr: String },
    #[error("failed to execute '{binary}': {source}")]
    CommandIo {
        binary: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadOutcome {
    pub testparm_passed: bool,
    pub smbd_reloaded: bool,
    pub details: String,
}

/// Validates the generated share configuration with `testparm -s` and triggers
/// `smbcontrol smbd reload-config`. If `skip_commands` is true (e.g. unit tests or `--no-reload`),
/// returns immediately after writing the config.
pub fn validate_and_reload_samba(
    samba: &SambaConfig,
    skip_commands: bool,
) -> Result<ReloadOutcome, ReloadError> {
    if skip_commands {
        return Ok(ReloadOutcome {
            testparm_passed: true,
            smbd_reloaded: false,
            details: "Samba CLI validation/reload skipped by flag".to_string(),
        });
    }

    // 1. Validate with `testparm -s <smb.conf or shares_conf_path>`
    let testparm_target = if Path::new("/etc/samba/smb.conf").exists() {
        Path::new("/etc/samba/smb.conf")
    } else {
        samba.shares_conf_path.as_path()
    };
    let testparm_res = Command::new(&samba.testparm_bin)
        .arg("-s")
        .arg(testparm_target)
        .output();

    let testparm_passed = match testparm_res {
        Ok(output) => {
            if !output.status.success() {
                let code = output.status.code().unwrap_or(-1);
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                return Err(ReloadError::TestparmFailed { code, stderr });
            }
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            warn!(
                binary = %samba.testparm_bin,
                "testparm binary not found on PATH; skipping syntax check"
            );
            false
        }
        Err(source) => {
            return Err(ReloadError::CommandIo {
                binary: samba.testparm_bin.clone(),
                source,
            });
        }
    };

    // 2. Reload smbd configuration with `smbcontrol smbd reload-config` (via sudo -n if non-root)
    let is_root = std::env::var("USER").as_deref() == Ok("root")
        || std::env::var("EUID").as_deref() == Ok("0");
    let smbcontrol_res = if is_root {
        Command::new(&samba.smbcontrol_bin)
            .args(["smbd", "reload-config"])
            .output()
    } else {
        Command::new("sudo")
            .args(["-n", &samba.smbcontrol_bin, "smbd", "reload-config"])
            .output()
            .or_else(|_| {
                Command::new(&samba.smbcontrol_bin)
                    .args(["smbd", "reload-config"])
                    .output()
            })
    };

    match smbcontrol_res {
        Ok(output) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                warn!(
                    stderr = %stderr,
                    "smbcontrol reload-config returned non-zero (smbd may not be running yet); shares-syncd.conf is updated on disk"
                );
                return Ok(ReloadOutcome {
                    testparm_passed,
                    smbd_reloaded: false,
                    details: format!("shares-syncd.conf written (smbd reload skipped: {stderr})"),
                });
            }
            info!("Reloaded smbd configuration via smbcontrol");
            Ok(ReloadOutcome {
                testparm_passed,
                smbd_reloaded: true,
                details: "testparm validated and smbd reloaded".to_string(),
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            warn!(
                binary = %samba.smbcontrol_bin,
                "smbcontrol binary not found on PATH; config written but smbd not signaled"
            );
            Ok(ReloadOutcome {
                testparm_passed,
                smbd_reloaded: false,
                details: "shares-syncd.conf written (smbcontrol not installed)".to_string(),
            })
        }
        Err(source) => Err(ReloadError::CommandIo {
            binary: samba.smbcontrol_bin.clone(),
            source,
        }),
    }
}

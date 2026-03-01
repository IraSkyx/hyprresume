use anyhow::{Context, Result};

use crate::ipc::client::HyprCtl;
use crate::models::{SessionFile, WindowEntry};

pub struct RestoreEngine {
    restore_geometry: bool,
}

impl RestoreEngine {
    pub const fn new(restore_geometry: bool) -> Self {
        Self { restore_geometry }
    }

    pub async fn restore(&self, session: &SessionFile, ctl: &HyprCtl) -> Result<RestoreReport> {
        let mut report = RestoreReport::default();
        let total = session.windows.len();
        tracing::info!(
            "restoring session '{}' ({total} apps)",
            session.session.name
        );

        for (i, window) in session.windows.iter().enumerate() {
            tracing::info!(
                "[{}/{}] restoring {} on workspace {}",
                i + 1,
                total,
                window.app_id,
                window.workspace
            );

            match self.restore_window(window, ctl).await {
                Ok(()) => {
                    report.restored += 1;
                    tracing::info!("  launched {}", window.app_id);
                }
                Err(e) => {
                    report.failed += 1;
                    report.errors.push((window.app_id.clone(), e.to_string()));
                    tracing::warn!("  failed to restore {}: {e}", window.app_id);
                }
            }
        }

        tracing::info!(
            "restore complete: {}/{total} apps ({} failed)",
            report.restored,
            report.failed
        );

        Ok(report)
    }

    async fn restore_window(&self, window: &WindowEntry, ctl: &HyprCtl) -> Result<()> {
        let mut rules = vec![format!("workspace {} silent", window.workspace)];

        if self.restore_geometry {
            if window.floating {
                rules.push("float".to_string());
                if let Some((w, h)) = window.size {
                    rules.push(format!("size {w} {h}"));
                }
                if let Some((x, y)) = window.position {
                    rules.push(format!("move {x} {y}"));
                }
            }
            if window.fullscreen {
                rules.push("fullscreen".to_string());
            }
        }

        ctl.exec_with_rules(&rules, &window.launch_cmd)
            .await
            .with_context(|| format!("launching {}", window.launch_cmd))
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

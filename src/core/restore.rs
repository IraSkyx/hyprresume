use anyhow::{Context, Result};
use std::time::Duration;

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
        let class = &window.app_id;
        let class_matcher = format!("class:^({class})$");

        let rule = format!("workspace {} silent, {class_matcher}", window.workspace);
        ctl.add_window_rule(&rule).await?;

        if window.floating && self.restore_geometry {
            ctl.add_window_rule(&format!("float, {class_matcher}"))
                .await?;

            if let Some((w, h)) = window.size {
                ctl.add_window_rule(&format!("size {w} {h}, {class_matcher}"))
                    .await?;
            }
            if let Some((x, y)) = window.position {
                ctl.add_window_rule(&format!("move {x} {y}, {class_matcher}"))
                    .await?;
            }
        }

        ctl.exec(&window.launch_cmd)
            .await
            .with_context(|| format!("launching {}", window.launch_cmd))?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        drop(ctl.remove_window_rule(&class_matcher).await);

        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

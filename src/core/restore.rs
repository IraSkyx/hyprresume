use anyhow::{Context, Result};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener::parse_event;
use crate::models::{HyprEvent, SessionFile, WindowEntry};

const WINDOW_APPEAR_TIMEOUT: Duration = Duration::from_secs(15);

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

        let (event_tx, mut event_rx) = mpsc::channel::<HyprEvent>(256);
        let socket2 = ctl.socket_paths().socket2.clone();
        let listener = tokio::spawn(async move {
            let Ok(stream) = tokio::net::UnixStream::connect(&socket2).await else {
                tracing::error!("failed to connect to socket2 for restore events");
                return;
            };
            let reader = tokio::io::BufReader::new(stream);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let event = parse_event(line.trim());
                if matches!(&event, HyprEvent::OpenWindow { .. })
                    && event_tx.send(event).await.is_err()
                {
                    break;
                }
            }
        });

        for (i, window) in session.windows.iter().enumerate() {
            tracing::info!(
                "[{}/{}] restoring {} on workspace {}",
                i + 1,
                total,
                window.app_id,
                window.workspace
            );

            match self.restore_window(window, ctl, &mut event_rx).await {
                Ok(()) => {
                    report.restored += 1;
                    tracing::info!("  restored {}", window.app_id);
                }
                Err(e) => {
                    report.failed += 1;
                    report.errors.push((window.app_id.clone(), e.to_string()));
                    tracing::warn!("  failed to restore {}: {e}", window.app_id);
                }
            }
        }

        listener.abort();

        tracing::info!(
            "restore complete: {}/{total} apps ({} failed)",
            report.restored,
            report.failed
        );

        Ok(report)
    }

    async fn restore_window(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
    ) -> Result<()> {
        let rule_name = format!("hyprresume-{}", window.app_id.replace(['.', ' '], "-"));
        let class_escaped = regex::escape(&window.app_id);

        // Named window rule matched by class for workspace placement.
        // Unlike exec rules, this applies to ALL windows of the class
        // regardless of process forks (fixes Electron apps).
        ctl.keyword(&format!(
            "'windowrule[{rule_name}]:match:class ^({class_escaped})$'"
        ))
        .await?;
        ctl.keyword(&format!(
            "'windowrule[{rule_name}]:workspace {} silent'",
            window.workspace
        ))
        .await?;

        ctl.dispatch(&format!("exec {}", window.launch_cmd))
            .await
            .with_context(|| format!("launching {}", window.launch_cmd))?;

        let addr = self.wait_for_open_event(events, &window.app_id).await;

        // Disable the rule now that the window exists
        drop(
            ctl.keyword(&format!("'windowrule[{rule_name}]:enable false'"))
                .await,
        );

        let Some(addr) = addr else {
            tracing::warn!(
                "{} did not appear within {}s",
                window.app_id,
                WINDOW_APPEAR_TIMEOUT.as_secs()
            );
            return Ok(());
        };

        tracing::debug!("  {} appeared at 0x{addr}", window.app_id);

        if self.restore_geometry {
            if window.floating
                && let (Some((x, y)), Some((w, h))) = (window.position, window.size)
            {
                ctl.dispatch(&format!("setfloating address:0x{addr}"))
                    .await?;
                ctl.dispatch(&format!("resizewindowpixel exact {w} {h},address:0x{addr}"))
                    .await?;
                ctl.dispatch(&format!("movewindowpixel exact {x} {y},address:0x{addr}"))
                    .await?;
            }

            if window.fullscreen {
                ctl.dispatch(&format!("fullscreen 0,address:0x{addr}"))
                    .await?;
            }
        }

        Ok(())
    }

    async fn wait_for_open_event(
        &self,
        events: &mut mpsc::Receiver<HyprEvent>,
        app_id: &str,
    ) -> Option<String> {
        tokio::time::timeout(WINDOW_APPEAR_TIMEOUT, async {
            loop {
                match events.recv().await {
                    Some(HyprEvent::OpenWindow { address, class, .. }) if class == app_id => {
                        return Some(address);
                    }
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await
        .unwrap_or(None)
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

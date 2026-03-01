use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::core::layout::{self, RestoreStep};
use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener::parse_event;
use crate::models::{HyprEvent, SessionFile, WindowEntry};

const WINDOW_APPEAR_TIMEOUT: Duration = Duration::from_secs(15);

pub struct RestoreEngine {
    restore_geometry: bool,
    restore_layout: bool,
}

impl RestoreEngine {
    pub const fn new(restore_geometry: bool, restore_layout: bool) -> Self {
        Self {
            restore_geometry,
            restore_layout,
        }
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

        if self.restore_layout {
            self.restore_with_layout(session, ctl, &mut event_rx, &mut report)
                .await?;
        } else {
            self.restore_simple(session, ctl, &mut event_rx, &mut report)
                .await?;
        }

        listener.abort();

        tracing::info!(
            "restore complete: {}/{total} apps ({} failed)",
            report.restored,
            report.failed
        );

        Ok(report)
    }

    async fn restore_simple(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
    ) -> Result<()> {
        let total = session.windows.len();
        for (i, window) in session.windows.iter().enumerate() {
            tracing::info!(
                "[{}/{}] restoring {} on workspace {}",
                i + 1,
                total,
                window.app_id,
                window.workspace
            );

            match self.restore_window(window, ctl, events).await {
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
        Ok(())
    }

    /// Restore windows using BSP tree inference to reconstruct tiling layouts.
    ///
    /// Groups tiled windows by workspace, infers the BSP tree from their saved
    /// geometry, then opens them in the correct order with `layoutmsg preselect`
    /// to reproduce the exact split structure. Floating windows are restored
    /// normally with exact geometry.
    async fn restore_with_layout(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
    ) -> Result<()> {
        // Build pointer-to-index map for the layout planner
        let mut window_index: HashMap<*const WindowEntry, usize> = HashMap::new();
        for (i, w) in session.windows.iter().enumerate() {
            window_index.insert(w as *const WindowEntry, i);
        }

        // Separate floating and tiled windows
        let floating: Vec<usize> = session
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.floating)
            .map(|(i, _)| i)
            .collect();

        // Group tiled windows by workspace
        let mut ws_groups: HashMap<&str, Vec<&WindowEntry>> = HashMap::new();
        for w in session.windows.iter().filter(|w| !w.floating) {
            ws_groups
                .entry(&w.workspace)
                .or_default()
                .push(w);
        }

        // For each workspace, try to build a BSP plan
        let mut ws_plans: HashMap<&str, Vec<RestoreStep>> = HashMap::new();
        let mut fallback_windows: Vec<usize> = Vec::new();

        for (ws, wins) in &ws_groups {
            match layout::build_workspace_plan(wins, &window_index) {
                Some(plan) => {
                    tracing::info!(
                        "workspace {ws}: inferred BSP layout for {} windows",
                        wins.len()
                    );
                    ws_plans.insert(ws, plan);
                }
                None => {
                    tracing::warn!(
                        "workspace {ws}: could not infer BSP layout, falling back to simple restore"
                    );
                    for w in wins {
                        fallback_windows.push(window_index[&(*w as *const WindowEntry)]);
                    }
                }
            }
        }

        // Track address of each opened window by its index
        let mut addresses: HashMap<usize, String> = HashMap::new();

        // Restore tiled windows per workspace in BSP order
        // Sort workspaces for deterministic order
        let mut sorted_ws: Vec<&&str> = ws_plans.keys().collect();
        sorted_ws.sort();

        for ws in sorted_ws {
            let plan = &ws_plans[*ws];
            for step in plan {
                let window = &session.windows[step.window_idx];
                tracing::info!(
                    "[layout] restoring {} on workspace {} (focus={:?}, presel={:?})",
                    window.app_id,
                    window.workspace,
                    step.focus_idx,
                    step.preselect,
                );

                // Focus the sibling window and preselect before opening
                if let (Some(focus_idx), Some(presel)) = (step.focus_idx, step.preselect)
                    && let Some(focus_addr) = addresses.get(&focus_idx)
                {
                    ctl.dispatch(&format!("focuswindow address:0x{focus_addr}"))
                        .await?;
                    ctl.dispatch(&format!("layoutmsg preselect {presel}"))
                        .await?;
                }

                match self.launch_and_track(window, ctl, events).await {
                    Ok(Some(addr)) => {
                        addresses.insert(step.window_idx, addr);
                        report.restored += 1;
                        tracing::info!("  restored {}", window.app_id);
                    }
                    Ok(None) => {
                        report.restored += 1;
                        tracing::info!("  launched {} (no window event)", window.app_id);
                    }
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                        tracing::warn!("  failed to restore {}: {e}", window.app_id);
                    }
                }
            }
        }

        // Resize tiled windows to match saved ratios.
        // After BSP reconstruction with default 50/50 splits, query each
        // window's actual size and apply deltas to match the saved size.
        for (idx, addr) in &addresses {
            let window = &session.windows[*idx];
            if let Some((saved_w, saved_h)) = window.size
                && let Ok(Some(client)) = ctl.get_client_by_address(addr).await
            {
                let (cur_w, cur_h) = client.size;
                let dx = saved_w - cur_w;
                let dy = saved_h - cur_h;
                if dx.abs() > 2 || dy.abs() > 2 {
                    tracing::debug!(
                        "  resizing {} by ({dx}, {dy}) to match saved layout",
                        window.app_id
                    );
                    let _ = ctl
                        .dispatch(&format!("resizewindowpixel {dx} {dy},address:0x{addr}"))
                        .await;
                }
            }
        }

        // Restore fullscreen state for tiled windows
        for (idx, addr) in &addresses {
            let window = &session.windows[*idx];
            if window.fullscreen {
                ctl.dispatch(&format!("fullscreen 0,address:0x{addr}"))
                    .await?;
            }
        }

        // Restore fallback tiled windows (BSP inference failed for their workspace)
        for idx in &fallback_windows {
            let window = &session.windows[*idx];
            tracing::info!(
                "[fallback] restoring {} on workspace {}",
                window.app_id,
                window.workspace
            );
            match self.restore_window(window, ctl, events).await {
                Ok(()) => {
                    report.restored += 1;
                }
                Err(e) => {
                    report.failed += 1;
                    report.errors.push((window.app_id.clone(), e.to_string()));
                }
            }
        }

        // Restore floating windows
        for idx in &floating {
            let window = &session.windows[*idx];
            tracing::info!(
                "[float] restoring {} on workspace {}",
                window.app_id,
                window.workspace
            );
            match self.restore_window(window, ctl, events).await {
                Ok(()) => {
                    report.restored += 1;
                }
                Err(e) => {
                    report.failed += 1;
                    report.errors.push((window.app_id.clone(), e.to_string()));
                }
            }
        }

        Ok(())
    }

    /// Launch a window with workspace placement via named rules, wait for it
    /// to appear, then return its address. Does NOT apply geometry/float.
    async fn launch_and_track(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
    ) -> Result<Option<String>> {
        let rule_name = format!(
            "hyprresume-{}",
            window.app_id.replace(['.', ' '], "-")
        );
        let class_escaped = regex::escape(&window.app_id);

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

        drop(
            ctl.keyword(&format!("'windowrule[{rule_name}]:enable false'"))
                .await,
        );

        if let Some(ref addr) = addr {
            tracing::debug!("  {} appeared at 0x{addr}", window.app_id);
        } else {
            tracing::warn!(
                "{} did not appear within {}s",
                window.app_id,
                WINDOW_APPEAR_TIMEOUT.as_secs()
            );
        }

        Ok(addr)
    }

    async fn restore_window(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
    ) -> Result<()> {
        let addr = self.launch_and_track(window, ctl, events).await?;

        let Some(addr) = addr else {
            return Ok(());
        };

        if self.restore_geometry {
            if window.floating
                && let (Some((x, y)), Some((w, h))) = (window.position, window.size)
            {
                ctl.dispatch(&format!("setfloating address:0x{addr}"))
                    .await?;
                ctl.dispatch(&format!(
                    "resizewindowpixel exact {w} {h},address:0x{addr}"
                ))
                .await?;
                ctl.dispatch(&format!(
                    "movewindowpixel exact {x} {y},address:0x{addr}"
                ))
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
                    Some(_) => continue,
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

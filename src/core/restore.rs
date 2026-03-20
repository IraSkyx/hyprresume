use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::core::layout::dwindle::{self, DwindlePlan};
use crate::core::layout::master::{self, MasterPlan};
use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener::parse_event;
use crate::models::{HyprEvent, SessionFile, WindowEntry};

const WINDOW_APPEAR_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the background watcher keeps listening for slow-starting apps
/// after the main restore loop finishes.
const LATE_WINDOW_GRACE_PERIOD: Duration = Duration::from_secs(60);

/// Known terminal working-directory flags, keyed by binary name.
/// Known terminal working-directory flags, keyed by binary name.
const TERMINAL_CWD_FLAGS: &[(&str, &str)] = &[
    ("ghostty", "--working-directory="),
    ("kitty", "--directory="),
    ("alacritty", "--working-directory="),
    ("wezterm", "--cwd="),
    ("foot", "--working-directory="),
    ("tilix", "--working-directory="),
    ("terminator", "--working-directory="),
];

/// Flags that force single-instance behavior via D-Bus, which prevents
/// each launched process from being independent (breaking CWD).
const SINGLE_INSTANCE_FLAGS: &[&str] = &["--gtk-single-instance=true", "--single-instance"];

/// A window that was launched but didn't appear within the per-window timeout.
/// Handed off to the background watcher for deferred placement.
/// A window that was launched but didn't appear within the per-window timeout.
/// Handed off to the background watcher for deferred placement.
struct PendingWindow {
    app_id: String,
    workspace: String,
    floating: bool,
    fullscreen: bool,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    rule_name: String,
}

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

    pub async fn restore(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
    ) -> Result<(RestoreReport, Option<JoinHandle<()>>)> {
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

        bind_workspaces_to_monitors(session, ctl).await;

        let had_focus_on_activate = ctl
            .get_option("misc:focus_on_activate")
            .await
            .unwrap_or(true);
        if had_focus_on_activate {
            drop(ctl.keyword("misc:focus_on_activate false").await);
        }

        let mut active_rules = Vec::new();
        let mut pending = Vec::new();

        if self.restore_layout {
            self.restore_with_layout(
                session,
                ctl,
                &mut event_rx,
                &mut report,
                &mut active_rules,
                &mut pending,
            )
            .await?;
        } else {
            self.restore_simple(
                session,
                ctl,
                &mut event_rx,
                &mut report,
                &mut active_rules,
                &mut pending,
            )
            .await?;
        }

        if had_focus_on_activate {
            drop(ctl.keyword("misc:focus_on_activate true").await);
        }
        listener.abort();

        tracing::info!(
            "restore complete: {}/{total} apps ({} failed, {} pending)",
            report.restored,
            report.failed,
            pending.len()
        );

        let watcher_handle = if pending.is_empty() {
            disable_all_rules(ctl, &active_rules).await;
            None
        } else {
            tracing::info!(
                "spawning late-window watcher for {} app(s): {}",
                pending.len(),
                pending
                    .iter()
                    .map(|p| p.app_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let socket_paths = ctl.socket_paths().clone();
            let restore_geometry = self.restore_geometry;
            Some(tokio::spawn(async move {
                watch_late_windows(
                    socket_paths,
                    pending,
                    active_rules,
                    restore_geometry,
                    LATE_WINDOW_GRACE_PERIOD,
                )
                .await;
            }))
        };

        Ok((report, watcher_handle))
    }

    async fn restore_simple(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
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

            match self
                .restore_window(window, ctl, events, active_rules, pending)
                .await
            {
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

    /// Restore windows using layout-aware strategies.
    ///
    /// Auto-detects the active Hyprland layout (dwindle, master, ...) and
    /// dispatches to the appropriate strategy. Falls back to simple restore
    /// for unknown layouts.
    /// Restore windows using layout-aware strategies.
    ///
    /// Auto-detects the active Hyprland layout (dwindle, master, ...) and
    /// dispatches to the appropriate strategy. Falls back to simple restore
    /// for unknown layouts.
    async fn restore_with_layout(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
    ) -> Result<()> {
        let layout = ctl.get_layout().await.unwrap_or_default();
        tracing::info!("detected layout: {layout:?}");

        match layout.as_str() {
            "dwindle" => {
                self.restore_dwindle(session, ctl, events, report, active_rules, pending)
                    .await
            }
            "master" => {
                self.restore_master(session, ctl, events, report, active_rules, pending)
                    .await
            }
            other => {
                tracing::warn!(
                    "layout {other:?} has no layout-aware restore, falling back to simple"
                );
                self.restore_simple(session, ctl, events, report, active_rules, pending)
                    .await
            }
        }
    }

    /// Dwindle restore: BSP inference, preselect-based placement, then
    /// splitratio application and convergence.
    async fn restore_dwindle(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
    ) -> Result<()> {
        let (floating, ws_plans, fallback_windows) = Self::build_dwindle_plans(session);

        let addresses = self
            .execute_bsp_plans(
                session,
                ctl,
                events,
                report,
                &ws_plans,
                active_rules,
                pending,
            )
            .await?;

        self.apply_split_ratios(ctl, &ws_plans, &addresses).await;
        self.converge_tiled_sizes(session, ctl, &addresses).await;
        self.apply_fullscreen(session, ctl, &addresses).await?;

        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &fallback_windows,
            "fallback",
            active_rules,
            pending,
        )
        .await?;
        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &floating,
            "float",
            active_rules,
            pending,
        )
        .await?;

        Ok(())
    }

    /// Master layout restore: infer master/stack split, set orientation and
    /// mfact, open master windows first then stack windows.
    async fn restore_master(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
    ) -> Result<()> {
        let (floating, master_plans, fallback_windows) = Self::build_master_plans(session);

        let mut sorted_ws: Vec<&String> = master_plans.keys().collect();
        sorted_ws.sort();

        for ws in sorted_ws {
            let plan = &master_plans[ws];
            tracing::info!(
                "[master] workspace {ws}: orientation={}, mfact={:.3}, {} master + {} stack",
                plan.orientation,
                plan.mfact,
                plan.master_indices.len(),
                plan.stack_indices.len()
            );

            // Set the default mfact before opening windows so the layout
            // engine uses it for initial placement on this workspace.
            drop(
                ctl.keyword(&format!("master:mfact {:.6}", plan.mfact))
                    .await,
            );

            ctl.dispatch(&format!("workspace {ws}")).await?;
            drop(
                ctl.dispatch(&format!("layoutmsg orientation{}", plan.orientation))
                    .await,
            );

            // Open the first master window.
            if let Some(&first_idx) = plan.master_indices.first() {
                let window = &session.windows[first_idx];
                tracing::info!("[master] opening master: {}", window.app_id);
                match self
                    .restore_window(window, ctl, events, active_rules, pending)
                    .await
                {
                    Ok(()) => report.restored += 1,
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                    }
                }
            }

            // Open additional master windows and promote them.
            for &idx in plan.master_indices.iter().skip(1) {
                let window = &session.windows[idx];
                tracing::info!("[master] opening extra master: {}", window.app_id);
                match self
                    .restore_window(window, ctl, events, active_rules, pending)
                    .await
                {
                    Ok(()) => {
                        report.restored += 1;
                        drop(ctl.dispatch("layoutmsg addmaster").await);
                    }
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                    }
                }
            }

            // Open stack windows in order.
            for &idx in &plan.stack_indices {
                let window = &session.windows[idx];
                tracing::info!("[master] opening stack: {}", window.app_id);
                match self
                    .restore_window(window, ctl, events, active_rules, pending)
                    .await
                {
                    Ok(()) => report.restored += 1,
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                    }
                }
            }

            // Nothing else needed: the `master:mfact` keyword set before
            // window placement is used by the layout engine for this workspace.
        }

        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &fallback_windows,
            "fallback",
            active_rules,
            pending,
        )
        .await?;
        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &floating,
            "float",
            active_rules,
            pending,
        )
        .await?;

        Ok(())
    }

    fn build_dwindle_plans(
        session: &SessionFile,
    ) -> (Vec<usize>, HashMap<String, DwindlePlan>, Vec<usize>) {
        let floating: Vec<usize> = session
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.floating)
            .map(|(i, _)| i)
            .collect();

        let mut ws_groups: HashMap<&str, (Vec<&WindowEntry>, Vec<usize>)> = HashMap::new();
        for (i, w) in session.windows.iter().enumerate() {
            if !w.floating {
                let entry = ws_groups.entry(&w.workspace).or_default();
                entry.0.push(w);
                entry.1.push(i);
            }
        }

        let mut ws_plans: HashMap<String, DwindlePlan> = HashMap::new();
        let mut fallback_windows: Vec<usize> = Vec::new();

        for (ws, (wins, indices)) in &ws_groups {
            if let Some(plan) = dwindle::build_workspace_plan(wins, indices) {
                tracing::info!(
                    "workspace {ws}: inferred BSP layout for {} windows ({} ratio steps)",
                    wins.len(),
                    plan.ratio_steps.len(),
                );
                ws_plans.insert((*ws).to_string(), plan);
            } else {
                tracing::warn!(
                    "workspace {ws}: could not infer BSP layout, falling back to simple restore"
                );
                fallback_windows.extend_from_slice(indices);
            }
        }

        (floating, ws_plans, fallback_windows)
    }

    fn build_master_plans(
        session: &SessionFile,
    ) -> (Vec<usize>, HashMap<String, MasterPlan>, Vec<usize>) {
        let floating: Vec<usize> = session
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.floating)
            .map(|(i, _)| i)
            .collect();

        let mut ws_groups: HashMap<&str, (Vec<&WindowEntry>, Vec<usize>)> = HashMap::new();
        for (i, w) in session.windows.iter().enumerate() {
            if !w.floating {
                let entry = ws_groups.entry(&w.workspace).or_default();
                entry.0.push(w);
                entry.1.push(i);
            }
        }

        let mut master_plans: HashMap<String, MasterPlan> = HashMap::new();
        let mut fallback_windows: Vec<usize> = Vec::new();

        for (ws, (wins, indices)) in &ws_groups {
            if let Some(plan) = master::build_workspace_plan(wins, indices) {
                tracing::info!(
                    "workspace {ws}: inferred master layout ({} master, {} stack, orientation={})",
                    plan.master_indices.len(),
                    plan.stack_indices.len(),
                    plan.orientation,
                );
                master_plans.insert((*ws).to_string(), plan);
            } else {
                tracing::warn!(
                    "workspace {ws}: could not infer master layout, falling back to simple restore"
                );
                fallback_windows.extend_from_slice(indices);
            }
        }

        (floating, master_plans, fallback_windows)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_bsp_plans(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        ws_plans: &HashMap<String, DwindlePlan>,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
    ) -> Result<HashMap<usize, String>> {
        let mut addresses: HashMap<usize, String> = HashMap::new();
        let mut sorted_ws: Vec<&String> = ws_plans.keys().collect();
        sorted_ws.sort();
        let mut rule_counter = 0usize;

        for ws in sorted_ws {
            let plan = &ws_plans[ws];
            for (i, step) in plan.steps.iter().enumerate() {
                let window = &session.windows[step.window_idx];
                tracing::info!(
                    "[layout] restoring {} on workspace {} (focus={:?}, presel={:?})",
                    window.app_id,
                    window.workspace,
                    step.focus_idx,
                    step.preselect,
                );

                if let (Some(focus_idx), Some(presel)) = (step.focus_idx, step.preselect)
                    && let Some(focus_addr) = addresses.get(&focus_idx)
                {
                    ctl.dispatch(&format!("focuswindow address:0x{focus_addr}"))
                        .await?;
                    ctl.dispatch(&format!("layoutmsg preselect {presel}"))
                        .await?;
                } else if i == 0 {
                    ctl.dispatch(&format!("workspace {ws}")).await?;
                }

                match self
                    .bsp_launch_and_track(
                        window,
                        ctl,
                        events,
                        active_rules,
                        pending,
                        &mut rule_counter,
                    )
                    .await
                {
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

        Ok(addresses)
    }

    /// BSP-specific launch: switches to the workspace first (so preselect
    /// works even for single-instance apps), strips single-instance flags,
    /// and does NOT use `[workspace N silent]` since we are already on the
    /// target workspace.
    ///
    /// The rule is disabled immediately once the window appears so that a
    /// subsequent window of the same class never sees a stale rule. Only
    /// rules for timed-out windows are kept alive for the background watcher.
    async fn bsp_launch_and_track(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        rule_counter: &mut usize,
    ) -> Result<Option<String>> {
        let rule_name = format!(
            "hyprresume-{}-{}",
            window.app_id.replace(['.', ' '], "-"),
            rule_counter
        );
        *rule_counter += 1;
        let class_escaped = regex::escape(&window.app_id);

        ctl.keyword(&format!(
            "windowrule[{rule_name}]:match:class ^({class_escaped})$"
        ))
        .await?;
        ctl.keyword(&format!(
            "windowrule[{rule_name}]:workspace {} silent",
            window.workspace
        ))
        .await?;
        let launch_cmd = build_bsp_launch_cmd(window);
        ctl.dispatch(&format!("exec {launch_cmd}"))
            .await
            .with_context(|| format!("launching {}", window.launch_cmd))?;

        if let Some(ref addr) = self.wait_for_open_event(events, &window.app_id).await {
            tracing::debug!("  {} appeared at 0x{addr}", window.app_id);
            disable_all_rules(ctl, &[rule_name]).await;
            drop(
                ctl.dispatch(&format!(
                    "movetoworkspacesilent {},address:0x{addr}",
                    window.workspace
                ))
                .await,
            );
            Ok(Some(addr.clone()))
        } else {
            tracing::warn!(
                "{} did not appear within {}s, deferring to late-window watcher",
                window.app_id,
                WINDOW_APPEAR_TIMEOUT.as_secs()
            );
            active_rules.push(rule_name.clone());
            pending.push(PendingWindow {
                app_id: window.app_id.clone(),
                workspace: window.workspace.clone(),
                floating: window.floating,
                fullscreen: window.fullscreen,
                position: window.position,
                size: window.size,
                rule_name,
            });
            Ok(None)
        }
    }

    /// Apply `layoutmsg splitratio <delta>` for each split node in the BSP tree
    /// that has a direct leaf child. The delta is computed from the default 0.5
    /// ratio since freshly-created windows always start at the default.
    async fn apply_split_ratios(
        &self,
        ctl: &HyprCtl,
        ws_plans: &HashMap<String, DwindlePlan>,
        addresses: &HashMap<usize, String>,
    ) {
        let mut applied = 0usize;
        let mut sorted_ws: Vec<&String> = ws_plans.keys().collect();
        sorted_ws.sort();

        for ws in sorted_ws {
            let plan = &ws_plans[ws];
            for step in &plan.ratio_steps {
                let Some(addr) = addresses.get(&step.focus_window_idx) else {
                    continue;
                };
                if let Err(e) = ctl.dispatch(&format!("focuswindow address:0x{addr}")).await {
                    tracing::warn!("splitratio: focus failed: {e}");
                    continue;
                }
                let delta = step.ratio - 0.5;
                match ctl
                    .dispatch(&format!("layoutmsg splitratio {delta:.6}"))
                    .await
                {
                    Ok(resp) if resp.trim() != "ok" => {
                        tracing::warn!("splitratio: unexpected response: {resp}");
                    }
                    Err(e) => tracing::warn!("splitratio: ipc error: {e}"),
                    _ => applied += 1,
                }
            }
        }

        if applied > 0 {
            tracing::info!("applied {applied} split ratios");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn converge_tiled_sizes(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        addresses: &HashMap<usize, String>,
    ) {
        const MAX_PASSES: usize = 4;
        const TOLERANCE: i32 = 6;

        for pass in 0..MAX_PASSES {
            let mut all_ok = true;

            for (idx, addr) in addresses {
                let window = &session.windows[*idx];
                let Some((saved_w, saved_h)) = window.size else {
                    continue;
                };
                let Ok(Some(client)) = ctl.get_client_by_address(addr).await else {
                    continue;
                };

                let dw = saved_w - client.size.0;
                let dh = saved_h - client.size.1;

                if dw.abs() > TOLERANCE || dh.abs() > TOLERANCE {
                    all_ok = false;
                    tracing::debug!(
                        "  pass {}: resize {} by ({dw}, {dh})",
                        pass + 1,
                        window.app_id,
                    );
                    match ctl
                        .dispatch(&format!("resizewindowpixel {dw} {dh},address:0x{addr}"))
                        .await
                    {
                        Ok(resp) if resp.trim() != "ok" => {
                            tracing::warn!("  resize failed: {resp}");
                        }
                        Err(e) => tracing::warn!("  resize ipc error: {e}"),
                        _ => {}
                    }
                }
            }

            if all_ok {
                tracing::debug!("  tiled sizes converged after {} pass(es)", pass + 1);
                return;
            }

            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        tracing::debug!("  tiled sizes settled after {MAX_PASSES} passes");
    }

    /// Iterative convergence: re-query window sizes and apply pixel corrections
    async fn apply_fullscreen(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        addresses: &HashMap<usize, String>,
    ) -> Result<()> {
        for (idx, addr) in addresses {
            let window = &session.windows[*idx];
            if window.fullscreen {
                ctl.dispatch(&format!("fullscreen 0,address:0x{addr}"))
                    .await?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn restore_indexed(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        indices: &[usize],
        label: &str,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
    ) -> Result<()> {
        for &idx in indices {
            let window = &session.windows[idx];
            tracing::info!(
                "[{label}] restoring {} on workspace {}",
                window.app_id,
                window.workspace
            );
            match self
                .restore_window(window, ctl, events, active_rules, pending)
                .await
            {
                Ok(()) => report.restored += 1,
                Err(e) => {
                    report.failed += 1;
                    report.errors.push((window.app_id.clone(), e.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Launch a window with workspace placement via named rules, wait for it
    /// to appear, then return its address.
    ///
    /// The rule is disabled immediately once the window appears so that a
    /// subsequent window of the same class never sees a stale rule. Only
    /// rules for timed-out windows are kept alive (pushed to `active_rules`)
    /// for the background late-window watcher.
    async fn launch_and_track(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
    ) -> Result<Option<String>> {
        let rule_name = format!(
            "hyprresume-{}-{}",
            window.app_id.replace(['.', ' '], "-"),
            active_rules.len()
        );
        let class_escaped = regex::escape(&window.app_id);

        ctl.keyword(&format!(
            "windowrule[{rule_name}]:match:class ^({class_escaped})$"
        ))
        .await?;
        ctl.keyword(&format!(
            "windowrule[{rule_name}]:workspace {} silent",
            window.workspace
        ))
        .await?;

        let launch_cmd = build_launch_cmd(window);
        ctl.dispatch(&format!(
            "exec [workspace {} silent] {launch_cmd}",
            window.workspace
        ))
        .await
        .with_context(|| format!("launching {}", window.launch_cmd))?;

        if let Some(ref addr) = self.wait_for_open_event(events, &window.app_id).await {
            tracing::debug!("  {} appeared at 0x{addr}", window.app_id);
            disable_all_rules(ctl, &[rule_name]).await;
            drop(
                ctl.dispatch(&format!(
                    "movetoworkspacesilent {},address:0x{addr}",
                    window.workspace
                ))
                .await,
            );
            Ok(Some(addr.clone()))
        } else {
            tracing::warn!(
                "{} did not appear within {}s, deferring to late-window watcher",
                window.app_id,
                WINDOW_APPEAR_TIMEOUT.as_secs()
            );
            active_rules.push(rule_name.clone());
            pending.push(PendingWindow {
                app_id: window.app_id.clone(),
                workspace: window.workspace.clone(),
                floating: window.floating,
                fullscreen: window.fullscreen,
                position: window.position,
                size: window.size,
                rule_name,
            });
            Ok(None)
        }
    }

    async fn restore_window(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
    ) -> Result<()> {
        let addr = self
            .launch_and_track(window, ctl, events, active_rules, pending)
            .await?;

        let Some(addr) = addr else {
            return Ok(());
        };

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
            while let Some(event) = events.recv().await {
                if let HyprEvent::OpenWindow { address, class, .. } = event
                    && class == app_id
                {
                    return Some(address);
                }
            }
            None
        })
        .await
        .unwrap_or(None)
    }
}

/// Background task that watches for windows that didn't appear during the main
/// restore loop. Keeps their Hyprland window rules active and listens on
/// socket2 until every pending window appears or the grace period expires.
async fn watch_late_windows(
    paths: crate::ipc::client::HyprSocketPaths,
    mut pending: Vec<PendingWindow>,
    all_rules: Vec<String>,
    restore_geometry: bool,
    grace_period: Duration,
) {
    let ctl = HyprCtl::new(paths.clone());
    let mut resolved_rules: Vec<String> = Vec::new();

    let Ok(stream) = tokio::net::UnixStream::connect(&paths.socket2).await else {
        tracing::error!("late-window watcher: failed to connect to socket2, disabling rules");
        disable_all_rules(&ctl, &all_rules).await;
        return;
    };

    let reader = tokio::io::BufReader::new(stream);
    let mut lines = reader.lines();
    let deadline = tokio::time::Instant::now() + grace_period;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                let event = parse_event(line.trim());
                if let HyprEvent::OpenWindow { address, class, .. } = event
                    && let Some(idx) = pending.iter().position(|p| p.app_id == class)
                {
                    let pw = pending.remove(idx);
                    tracing::info!(
                        "late-window watcher: {} appeared at 0x{address}, \
                         moving to workspace {}",
                        pw.app_id,
                        pw.workspace
                    );
                    apply_late_window(&ctl, &pw, &address, restore_geometry).await;
                    resolved_rules.push(pw.rule_name);

                    if pending.is_empty() {
                        tracing::info!("late-window watcher: all pending windows resolved");
                        break;
                    }
                }
            }
            Ok(Ok(None) | Err(_)) => {
                tracing::warn!("late-window watcher: socket2 stream ended");
                break;
            }
            Err(_) => break,
        }
    }

    // Eagerly disable rules for windows that were resolved during the watch.
    disable_all_rules(&ctl, &resolved_rules).await;

    if !pending.is_empty() {
        tracing::warn!(
            "late-window watcher: {} window(s) never appeared after {}s: {}",
            pending.len(),
            grace_period.as_secs(),
            pending
                .iter()
                .map(|p| p.app_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Disable all remaining rules (including for windows that appeared in the
    // main loop but whose rules were kept alive for forking-app safety).
    let remaining_rules: Vec<String> = all_rules
        .into_iter()
        .filter(|r| !resolved_rules.contains(r))
        .collect();
    disable_all_rules(&ctl, &remaining_rules).await;
}

async fn apply_late_window(
    ctl: &HyprCtl,
    pw: &PendingWindow,
    address: &str,
    restore_geometry: bool,
) {
    drop(
        ctl.dispatch(&format!(
            "movetoworkspacesilent {},address:0x{address}",
            pw.workspace
        ))
        .await,
    );

    if restore_geometry
        && pw.floating
        && let (Some((x, y)), Some((w, h))) = (pw.position, pw.size)
    {
        drop(
            ctl.dispatch(&format!("setfloating address:0x{address}"))
                .await,
        );
        drop(
            ctl.dispatch(&format!(
                "resizewindowpixel exact {w} {h},address:0x{address}"
            ))
            .await,
        );
        drop(
            ctl.dispatch(&format!(
                "movewindowpixel exact {x} {y},address:0x{address}"
            ))
            .await,
        );
    }

    if pw.fullscreen {
        drop(
            ctl.dispatch(&format!("fullscreen 0,address:0x{address}"))
                .await,
        );
    }
}

/// Build the exec command, injecting browser profile flags and/or saved CWD.
///
/// Profile flags (e.g. `-P work`, `--profile-directory=Profile 1`) are
/// appended to the base launch command before CWD handling, since browsers
/// and terminals are mutually exclusive in practice.
///
/// For known terminals: strips single-instance flags (so each launch is
/// its own process) and appends `--working-directory=<path>`.
/// For other apps with CWD: wraps with `cd <path> && exec <cmd>`.
fn build_launch_cmd(window: &WindowEntry) -> String {
    let cmd = window.profile.as_ref().map_or_else(
        || window.launch_cmd.clone(),
        |profile| format!("{} {profile}", window.launch_cmd),
    );

    let Some(cwd) = window.cwd.as_deref() else {
        return cmd;
    };

    terminal_cwd_flag(&cmd).map_or_else(
        || {
            let escaped = shell_escape(cwd);
            format!("sh -c 'cd {escaped} && exec {cmd}'")
        },
        |flag| {
            let clean = strip_single_instance_flags(&cmd);
            format!("{clean} {flag}{cwd}")
        },
    )
}

fn strip_single_instance_flags(cmd: &str) -> String {
    cmd.split_whitespace()
        .filter(|arg| !SINGLE_INSTANCE_FLAGS.contains(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build launch command for BSP restore: always strips single-instance flags
/// so each launch creates an independent process (critical for preselect to
/// work, since single-instance apps create windows in the existing process's
/// workspace, bypassing the preselection on the target workspace).
fn build_bsp_launch_cmd(window: &WindowEntry) -> String {
    let base = build_launch_cmd(window);
    strip_single_instance_flags(&base)
}

/// Match the binary name in a launch command against known terminals
/// and return the appropriate `--working-directory=` style flag.
fn terminal_cwd_flag(launch_cmd: &str) -> Option<&'static str> {
    let bin = launch_cmd.split_whitespace().next()?.rsplit('/').next()?;
    TERMINAL_CWD_FLAGS
        .iter()
        .find(|(name, _)| *name == bin)
        .map(|(_, flag)| *flag)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Before restoring windows, move each workspace to the monitor it was
/// originally saved on. Only binds to monitors that are currently connected;
/// workspaces targeting unavailable monitors get default Hyprland placement
/// (typically the first available monitor).
async fn bind_workspaces_to_monitors(session: &SessionFile, ctl: &HyprCtl) {
    let available: HashSet<String> = match ctl.get_monitors().await {
        Ok(monitors) => monitors.into_iter().map(|m| m.name).collect(),
        Err(e) => {
            tracing::warn!("could not query monitors, skipping workspace-monitor binding: {e}");
            return;
        }
    };

    let mut seen = HashSet::new();
    let mut missing_monitors: HashSet<&str> = HashSet::new();
    let mut bound = 0usize;

    for window in &session.windows {
        let Some(monitor) = window.monitor.as_deref().filter(|m| !m.is_empty()) else {
            continue;
        };
        if !seen.insert((&window.workspace, monitor)) {
            continue;
        }
        if !available.contains(monitor) {
            missing_monitors.insert(monitor);
            continue;
        }
        tracing::info!(
            "binding workspace {} to monitor {monitor}",
            window.workspace
        );
        drop(
            ctl.dispatch(&format!(
                "moveworkspacetomonitor {} {monitor}",
                window.workspace
            ))
            .await,
        );
        bound += 1;
    }

    if !missing_monitors.is_empty() {
        let names: Vec<&str> = missing_monitors.into_iter().collect();
        tracing::info!(
            "saved monitor(s) no longer connected ({}), \
             affected workspaces will use default placement",
            names.join(", ")
        );
    }
    if bound > 0 {
        tracing::info!("bound {bound} workspace(s) to their saved monitors");
    }
}

async fn disable_all_rules(ctl: &HyprCtl, rules: &[String]) {
    for rule in rules {
        drop(
            ctl.keyword(&format!("windowrule[{rule}]:enable false"))
                .await,
        );
    }
    if !rules.is_empty() {
        tracing::debug!("disabled {} window rules", rules.len());
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::client::HyprSocketPaths;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    /// Mock socket1 that records all received IPC commands and responds "ok".
    struct RecordingSocket1 {
        listener: UnixListener,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingSocket1 {
        fn new(path: &std::path::Path) -> (Self, Arc<Mutex<Vec<String>>>) {
            let listener = UnixListener::bind(path).unwrap();
            let log = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    listener,
                    log: log.clone(),
                },
                log,
            )
        }

        async fn serve(self) {
            loop {
                let Ok((mut stream, _)) = self.listener.accept().await else {
                    break;
                };
                let log = self.log.clone();
                tokio::spawn(async move {
                    let mut buf = String::new();
                    drop(stream.read_to_string(&mut buf).await);
                    log.lock().await.push(buf);
                    drop(stream.write_all(b"ok").await);
                });
            }
        }
    }

    /// Bind a socket2 listener (synchronously creates the file) and spawn
    /// a task that accepts one connection and emits events with delays.
    fn spawn_delayed_socket2(
        path: &std::path::Path,
        events: Vec<(Duration, String)>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for (delay, event) in &events {
                tokio::time::sleep(*delay).await;
                let line = format!("{event}\n");
                if stream.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        })
    }

    fn make_entry(
        app_id: &str,
        launch_cmd: &str,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> WindowEntry {
        WindowEntry {
            app_id: app_id.to_string(),
            launch_cmd: launch_cmd.to_string(),
            workspace: "1".to_string(),
            monitor: None,
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            cwd: cwd.map(String::from),
            profile: profile.map(String::from),
        }
    }

    #[test]
    fn build_cmd_no_profile_no_cwd() {
        let entry = make_entry("firefox", "firefox", None, None);
        assert_eq!(build_launch_cmd(&entry), "firefox");
    }

    #[test]
    fn build_cmd_with_profile() {
        let entry = make_entry("firefox", "firefox", Some("-P work"), None);
        assert_eq!(build_launch_cmd(&entry), "firefox -P work");
    }

    #[test]
    fn build_cmd_with_no_remote_profile() {
        let entry = make_entry("firefox", "firefox", Some("-no-remote -P dev"), None);
        assert_eq!(build_launch_cmd(&entry), "firefox -no-remote -P dev");
    }

    #[test]
    fn build_cmd_chromium_profile() {
        let entry = make_entry(
            "chromium",
            "chromium",
            Some("--profile-directory=Profile 1"),
            None,
        );
        assert_eq!(
            build_launch_cmd(&entry),
            "chromium --profile-directory=Profile 1"
        );
    }

    #[test]
    fn build_cmd_flatpak_profile() {
        let entry = make_entry(
            "org.mozilla.firefox",
            "flatpak run org.mozilla.firefox",
            Some("-P work"),
            None,
        );
        assert_eq!(
            build_launch_cmd(&entry),
            "flatpak run org.mozilla.firefox -P work"
        );
    }

    #[test]
    fn build_cmd_with_cwd_no_profile() {
        let entry = make_entry("ghostty", "ghostty", None, Some("/home/user/project"));
        assert_eq!(
            build_launch_cmd(&entry),
            "ghostty --working-directory=/home/user/project"
        );
    }

    #[test]
    fn build_cmd_profile_does_not_affect_cwd() {
        let entry = make_entry("ghostty", "ghostty", None, Some("/tmp"));
        let cmd = build_launch_cmd(&entry);
        assert!(cmd.contains("--working-directory=/tmp"));
    }

    #[tokio::test]
    async fn late_watcher_catches_delayed_window() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(100),
                "openwindow>>abc123,4,slow-app,Slow App Title".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "slow-app".to_string(),
            workspace: "3".to_string(),
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            rule_name: "hyprresume-slow-app".to_string(),
        }];
        let all_rules = vec!["hyprresume-slow-app".to_string()];

        watch_late_windows(paths, pending, all_rules, false, Duration::from_secs(5)).await;

        let commands = log.lock().await;
        let has_move = commands
            .iter()
            .any(|c| c.contains("movetoworkspacesilent 3,address:0xabc123"));
        assert!(
            has_move,
            "expected movetoworkspacesilent dispatch, got: {commands:?}"
        );

        let has_rule_disable = commands
            .iter()
            .any(|c| c.contains("windowrule[hyprresume-slow-app]:enable false"));
        assert!(has_rule_disable, "expected rule cleanup, got: {commands:?}");
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// Verifies that when a pending window never appears, the watcher
    /// times out after the grace period and still cleans up all rules.
    #[tokio::test]
    async fn late_watcher_times_out_and_disables_rules() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(50),
                "openwindow>>xyz,1,other-app,Other".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "missing-app".to_string(),
            workspace: "2".to_string(),
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            rule_name: "hyprresume-missing-app".to_string(),
        }];
        let all_rules = vec!["hyprresume-missing-app".to_string()];

        let start = tokio::time::Instant::now();
        watch_late_windows(paths, pending, all_rules, false, Duration::from_millis(300)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(250),
            "should wait for grace period, only waited {elapsed:?}"
        );

        let commands = log.lock().await;
        let has_rule_disable = commands
            .iter()
            .any(|c| c.contains("windowrule[hyprresume-missing-app]:enable false"));
        assert!(
            has_rule_disable,
            "rules must be cleaned up even on timeout, got: {commands:?}"
        );

        let has_move = commands.iter().any(|c| c.contains("movetoworkspacesilent"));
        assert!(
            !has_move,
            "no movetoworkspacesilent for a window that never appeared, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// Floating window geometry (position + size) is applied when a late
    /// window arrives and `restore_geometry` is enabled.
    #[tokio::test]
    async fn late_watcher_restores_floating_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(50),
                "openwindow>>flt001,9,floater,Floating App".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "floater".to_string(),
            workspace: "5".to_string(),
            floating: true,
            fullscreen: false,
            position: Some((200, 150)),
            size: Some((800, 600)),
            rule_name: "hyprresume-floater".to_string(),
        }];
        let all_rules = vec!["hyprresume-floater".to_string()];

        watch_late_windows(paths, pending, all_rules, true, Duration::from_secs(5)).await;

        let commands = log.lock().await;
        let has_move = commands
            .iter()
            .any(|c| c.contains("movetoworkspacesilent 5,address:0xflt001"));
        assert!(has_move, "expected workspace move, got: {commands:?}");

        let has_float = commands
            .iter()
            .any(|c| c.contains("setfloating address:0xflt001"));
        assert!(has_float, "expected setfloating, got: {commands:?}");

        let has_resize = commands
            .iter()
            .any(|c| c.contains("resizewindowpixel exact 800 600,address:0xflt001"));
        assert!(has_resize, "expected resize, got: {commands:?}");

        let has_pos = commands
            .iter()
            .any(|c| c.contains("movewindowpixel exact 200 150,address:0xflt001"));
        assert!(has_pos, "expected position, got: {commands:?}");
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// Multiple slow-starting apps: the watcher resolves all of them as they
    /// arrive and exits early (before the grace period) when the last one appears.
    #[tokio::test]
    async fn late_watcher_handles_multiple_pending_windows() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![
                (
                    Duration::from_millis(50),
                    "openwindow>>aaa,1,app-a,App A".to_string(),
                ),
                (
                    Duration::from_millis(50),
                    "openwindow>>bbb,2,app-b,App B".to_string(),
                ),
            ],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![
            PendingWindow {
                app_id: "app-a".to_string(),
                workspace: "1".to_string(),
                floating: false,
                fullscreen: false,
                position: None,
                size: None,
                rule_name: "hyprresume-app-a".to_string(),
            },
            PendingWindow {
                app_id: "app-b".to_string(),
                workspace: "4".to_string(),
                floating: false,
                fullscreen: false,
                position: None,
                size: None,
                rule_name: "hyprresume-app-b".to_string(),
            },
        ];
        let all_rules = vec![
            "hyprresume-app-a".to_string(),
            "hyprresume-app-b".to_string(),
        ];

        let start = tokio::time::Instant::now();
        watch_late_windows(paths, pending, all_rules, false, Duration::from_secs(10)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "should exit early when all pending resolved, took {elapsed:?}"
        );

        let commands = log.lock().await;
        let has_move_a = commands
            .iter()
            .any(|c| c.contains("movetoworkspacesilent 1,address:0xaaa"));
        assert!(has_move_a, "expected move for app-a, got: {commands:?}");

        let has_move_b = commands
            .iter()
            .any(|c| c.contains("movetoworkspacesilent 4,address:0xbbb"));
        assert!(has_move_b, "expected move for app-b, got: {commands:?}");
        drop(commands);

        s1.abort();
        s2.abort();
    }
}

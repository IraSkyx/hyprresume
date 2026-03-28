use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::core::layout::dwindle::{self, DwindlePlan};
use crate::core::layout::master::{self, MasterPlan};
use crate::ipc::client::{HyprCtl, SessionctlExpectation};
use crate::ipc::event_listener::parse_event;
use crate::models::{HyprEvent, SessionFile, WindowEntry};

const WINDOW_APPEAR_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the background watcher keeps listening for slow-starting apps
/// after the main restore loop finishes.
const LATE_WINDOW_GRACE_PERIOD: Duration = Duration::from_secs(60);

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

pub struct RestoreEngine {
    restore_layout: bool,
}

impl RestoreEngine {
    pub const fn new(restore_layout: bool) -> Self {
        Self { restore_layout }
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

        // Register all expected windows with the compositor via sessionctl.
        // The compositor applies saved state (workspace, monitor, floating,
        // position/size) atomically during mapWindow(), before the layout
        // engine touches the window.
        self.register_sessionctl_expectations(session, ctl).await?;

        let mut pending = Vec::new();
        let mut launched = HashSet::new();

        if self.restore_layout {
            self.restore_with_layout(
                session,
                ctl,
                &mut event_rx,
                &mut report,
                &mut pending,
                &mut launched,
            )
            .await?;
        } else {
            self.restore_simple(
                session,
                ctl,
                &mut event_rx,
                &mut report,
                &mut pending,
                &mut launched,
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
            // All windows appeared — finish immediately.
            if let Err(e) = ctl.sessionctl_finish().await {
                tracing::warn!("sessionctl finish failed: {e}");
            }
            None
        } else {
            tracing::info!(
                "spawning late-window watcher for {} app(s): {}",
                pending.len(),
                pending.join(", ")
            );
            let socket_paths = ctl.socket_paths().clone();
            Some(tokio::spawn(async move {
                watch_late_windows(socket_paths, pending, LATE_WINDOW_GRACE_PERIOD).await;
            }))
        };

        Ok((report, watcher_handle))
    }

    /// Register all session windows as expectations with the compositor via
    /// the sessionctl IPC.
    async fn register_sessionctl_expectations(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
    ) -> Result<()> {
        if !ctl.sessionctl_available().await {
            bail!("compositor does not support sessionctl — cannot restore session");
        }

        if !ctl.sessionctl_begin().await.unwrap_or(false) {
            bail!("sessionctl begin failed");
        }

        let mut registered = 0usize;
        for window in &session.windows {
            let pos = window.position.unwrap_or((0, 0));
            let size = window.size.unwrap_or((0, 0));
            let monitor = window.monitor.as_deref().unwrap_or("");

            match ctl
                .sessionctl_expect(&SessionctlExpectation {
                    app_id: &window.app_id,
                    workspace: &window.workspace,
                    monitor,
                    floating: window.floating,
                    fullscreen: window.fullscreen,
                    maximized: false,
                    x: f64::from(pos.0),
                    y: f64::from(pos.1),
                    w: f64::from(size.0),
                    h: f64::from(size.1),
                })
                .await
            {
                Ok(true) => registered += 1,
                Ok(false) => {
                    tracing::warn!("sessionctl expect failed for {}", window.app_id);
                }
                Err(err) => {
                    tracing::warn!("sessionctl expect error for {}: {err}", window.app_id);
                }
            }
        }

        if !ctl.sessionctl_end().await.unwrap_or(false) {
            bail!("sessionctl end failed");
        }

        tracing::info!(
            "registered {registered}/{} windows with compositor sessionctl",
            session.windows.len()
        );
        Ok(())
    }

    async fn restore_simple(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        pending: &mut Vec<String>,
        launched: &mut HashSet<String>,
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
                .launch_and_track(window, ctl, events, pending, launched, false)
                .await
            {
                Ok(_) => {
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
    async fn restore_with_layout(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        pending: &mut Vec<String>,
        launched: &mut HashSet<String>,
    ) -> Result<()> {
        let layout = ctl.get_layout().await.unwrap_or_default();
        tracing::info!("detected layout: {layout:?}");

        match layout.as_str() {
            "dwindle" => {
                self.restore_dwindle(session, ctl, events, report, pending, launched)
                    .await
            }
            "master" => {
                self.restore_master(session, ctl, events, report, pending, launched)
                    .await
            }
            other => {
                tracing::warn!(
                    "layout {other:?} has no layout-aware restore, falling back to simple"
                );
                self.restore_simple(session, ctl, events, report, pending, launched)
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
        pending: &mut Vec<String>,
        launched: &mut HashSet<String>,
    ) -> Result<()> {
        let (floating, ws_plans, fallback_windows) = Self::build_dwindle_plans(session);

        let addresses = self
            .execute_bsp_plans(session, ctl, events, report, &ws_plans, pending, launched)
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
            pending,
            launched,
        )
        .await?;
        self.restore_indexed(
            session, ctl, events, report, &floating, "float", pending, launched,
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
        pending: &mut Vec<String>,
        launched: &mut HashSet<String>,
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
                    .launch_and_track(window, ctl, events, pending, launched, false)
                    .await
                {
                    Ok(_) => report.restored += 1,
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
                    .launch_and_track(window, ctl, events, pending, launched, false)
                    .await
                {
                    Ok(_) => {
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
                    .launch_and_track(window, ctl, events, pending, launched, false)
                    .await
                {
                    Ok(_) => report.restored += 1,
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                    }
                }
            }
        }

        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &fallback_windows,
            "fallback",
            pending,
            launched,
        )
        .await?;
        self.restore_indexed(
            session, ctl, events, report, &floating, "float", pending, launched,
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
        pending: &mut Vec<String>,
        launched: &mut HashSet<String>,
    ) -> Result<HashMap<usize, String>> {
        let mut addresses: HashMap<usize, String> = HashMap::new();
        let mut sorted_ws: Vec<&String> = ws_plans.keys().collect();
        sorted_ws.sort();

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
                    .launch_and_track(window, ctl, events, pending, launched, true)
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
        pending: &mut Vec<String>,
        launched: &mut HashSet<String>,
    ) -> Result<()> {
        for &idx in indices {
            let window = &session.windows[idx];
            tracing::info!(
                "[{label}] restoring {} on workspace {}",
                window.app_id,
                window.workspace
            );
            match self
                .launch_and_track(window, ctl, events, pending, launched, false)
                .await
            {
                Ok(_) => report.restored += 1,
                Err(e) => {
                    report.failed += 1;
                    report.errors.push((window.app_id.clone(), e.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Launch a window and wait for it to appear. Sessionctl handles all
    /// workspace placement, floating state, and geometry during `mapWindow()`.
    /// When `strip_single_instance` is true, single-instance flags are removed
    /// so each exec creates an independent process (needed for BSP preselect).
    async fn launch_and_track(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        pending: &mut Vec<String>,
        launched: &mut HashSet<String>,
        strip_single_instance: bool,
    ) -> Result<Option<String>> {
        if launched.insert(window.launch_cmd.clone()) {
            let mut launch_cmd = build_launch_cmd(window);
            if strip_single_instance {
                launch_cmd = strip_single_instance_flags(&launch_cmd);
            }
            ctl.dispatch(&format!("exec {launch_cmd}"))
                .await
                .with_context(|| format!("launching {}", window.launch_cmd))?;
        } else {
            tracing::debug!(
                "  {} already launched, waiting for window from existing instance",
                window.app_id
            );
        }

        self.wait_for_open_event(events, &window.app_id)
            .await
            .as_ref()
            .map_or_else(
                || {
                    tracing::warn!(
                        "{} did not appear within {}s, deferring to late-window watcher",
                        window.app_id,
                        WINDOW_APPEAR_TIMEOUT.as_secs()
                    );
                    pending.push(window.app_id.clone());
                    Ok(None)
                },
                |addr| {
                    tracing::debug!("  {} appeared at 0x{addr}", window.app_id);
                    Ok(Some(addr.clone()))
                },
            )
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
/// restore loop. Sessionctl expectations are still active in the compositor, so
/// late windows will be placed correctly automatically — we just log them.
async fn watch_late_windows(
    paths: crate::ipc::client::HyprSocketPaths,
    mut pending: Vec<String>,
    grace_period: Duration,
) {
    let Ok(stream) = tokio::net::UnixStream::connect(&paths.socket2).await else {
        tracing::error!("late-window watcher: failed to connect to socket2");
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
                    && let Some(idx) = pending.iter().position(|p| *p == class)
                {
                    pending.remove(idx);
                    tracing::info!(
                        "late-window watcher: {class} appeared at 0x{address} (sessionctl handles placement)",
                    );

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

    if !pending.is_empty() {
        tracing::warn!(
            "late-window watcher: {} window(s) never appeared after {}s: {}",
            pending.len(),
            grace_period.as_secs(),
            pending.join(", ")
        );
    }

    // Clear remaining expectations so new windows of the same class
    // aren't auto-placed after the restore window closes.
    let ctl = HyprCtl::new(paths);
    if let Err(e) = ctl.sessionctl_finish().await {
        tracing::warn!("late-window watcher: sessionctl finish failed: {e}");
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

/// Before restoring windows, bind each workspace to its saved monitor using
/// `keyword workspace` rules. Unlike `dispatch moveworkspacetomonitor`, these
/// rules are persistent and apply even when a workspace doesn't exist yet —
/// Hyprland will create it on the correct monitor when a window is placed there.
/// Only binds to monitors that are currently connected; workspaces targeting
/// unavailable monitors get default Hyprland placement.
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
        if let Err(e) = ctl
            .keyword(&format!("workspace {},monitor:{monitor}", window.workspace))
            .await
        {
            tracing::warn!(
                "failed to bind workspace {} to {monitor}: {e}",
                window.workspace
            );
        }
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
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

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

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(100),
                "openwindow>>abc123,4,slow-app,Slow App Title".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec!["slow-app".to_string()];

        watch_late_windows(paths, pending, Duration::from_secs(5)).await;

        // sessionctl handles placement — watcher just logs and exits
        s2.abort();
    }

    /// Verifies that when a pending window never appears, the watcher
    /// times out after the grace period.
    #[tokio::test]
    async fn late_watcher_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(50),
                "openwindow>>xyz,1,other-app,Other".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec!["missing-app".to_string()];

        let start = tokio::time::Instant::now();
        watch_late_windows(paths, pending, Duration::from_millis(300)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(250),
            "should wait for grace period, only waited {elapsed:?}"
        );

        s2.abort();
    }

    /// Multiple slow-starting apps: the watcher resolves all of them as they
    /// arrive and exits early (before the grace period) when the last one appears.
    #[tokio::test]
    async fn late_watcher_handles_multiple_pending_windows() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

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
        let pending = vec!["app-a".to_string(), "app-b".to_string()];

        let start = tokio::time::Instant::now();
        watch_late_windows(paths, pending, Duration::from_secs(10)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "should exit early when all pending resolved, took {elapsed:?}"
        );

        s2.abort();
    }
}

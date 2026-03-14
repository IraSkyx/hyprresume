use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::core::layout::{self, WorkspacePlan};
use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener::parse_event;
use crate::models::{HyprEvent, SessionFile, WindowEntry};

const WINDOW_APPEAR_TIMEOUT: Duration = Duration::from_secs(15);

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

        let had_focus_on_activate = ctl
            .get_option("misc:focus_on_activate")
            .await
            .unwrap_or(true);
        if had_focus_on_activate {
            drop(ctl.keyword("misc:focus_on_activate false").await);
        }

        let mut active_rules = Vec::new();

        if self.restore_layout {
            self.restore_with_layout(session, ctl, &mut event_rx, &mut report, &mut active_rules)
                .await?;
        } else {
            self.restore_simple(session, ctl, &mut event_rx, &mut report, &mut active_rules)
                .await?;
        }

        disable_all_rules(ctl, &active_rules).await;
        if had_focus_on_activate {
            drop(ctl.keyword("misc:focus_on_activate true").await);
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
        active_rules: &mut Vec<String>,
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

            match self.restore_window(window, ctl, events, active_rules).await {
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
        active_rules: &mut Vec<String>,
    ) -> Result<()> {
        let (floating, ws_plans, fallback_windows) = Self::build_layout_plans(session);

        let addresses = self
            .execute_bsp_plans(session, ctl, events, report, &ws_plans, active_rules)
            .await?;

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
        )
        .await?;

        Ok(())
    }

    fn build_layout_plans(
        session: &SessionFile,
    ) -> (Vec<usize>, HashMap<String, WorkspacePlan>, Vec<usize>) {
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

        let mut ws_plans: HashMap<String, WorkspacePlan> = HashMap::new();
        let mut fallback_windows: Vec<usize> = Vec::new();

        for (ws, (wins, indices)) in &ws_groups {
            if let Some(plan) = layout::build_workspace_plan(wins, indices) {
                tracing::info!(
                    "workspace {ws}: inferred BSP layout for {} windows",
                    wins.len()
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

    async fn execute_bsp_plans(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        ws_plans: &HashMap<String, WorkspacePlan>,
        active_rules: &mut Vec<String>,
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
                    .bsp_launch_and_track(window, ctl, events, active_rules, &mut rule_counter)
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
    async fn bsp_launch_and_track(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        active_rules: &mut Vec<String>,
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
        active_rules.push(rule_name);

        let launch_cmd = build_bsp_launch_cmd(window);
        ctl.dispatch(&format!("exec {launch_cmd}"))
            .await
            .with_context(|| format!("launching {}", window.launch_cmd))?;

        let addr = self.wait_for_open_event(events, &window.app_id).await;

        if let Some(ref addr) = addr {
            tracing::debug!("  {} appeared at 0x{addr}", window.app_id);
            drop(
                ctl.dispatch(&format!(
                    "movetoworkspacesilent {},address:0x{addr}",
                    window.workspace
                ))
                .await,
            );
        } else {
            tracing::warn!(
                "{} did not appear within {}s",
                window.app_id,
                WINDOW_APPEAR_TIMEOUT.as_secs()
            );
        }

        Ok(addr)
    }

    /// Iteratively resize tiled windows to match their saved sizes.
    /// Each pass queries live geometry and applies pixel deltas; multiple
    /// passes handle cascading effects where resizing one window shifts
    /// its neighbours.
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
    ) -> Result<()> {
        for &idx in indices {
            let window = &session.windows[idx];
            tracing::info!(
                "[{label}] restoring {} on workspace {}",
                window.app_id,
                window.workspace
            );
            match self.restore_window(window, ctl, events, active_rules).await {
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
    /// to appear, then return its address. Rule cleanup is deferred to the
    /// caller so forking apps (Electron) keep their workspace rule active
    /// until the entire restore completes.
    async fn launch_and_track(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        active_rules: &mut Vec<String>,
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
        active_rules.push(rule_name);

        let launch_cmd = build_launch_cmd(window);
        ctl.dispatch(&format!(
            "exec [workspace {} silent] {launch_cmd}",
            window.workspace
        ))
        .await
        .with_context(|| format!("launching {}", window.launch_cmd))?;

        let addr = self.wait_for_open_event(events, &window.app_id).await;

        if let Some(ref addr) = addr {
            tracing::debug!("  {} appeared at 0x{addr}", window.app_id);
            drop(
                ctl.dispatch(&format!(
                    "movetoworkspacesilent {},address:0x{addr}",
                    window.workspace
                ))
                .await,
            );
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
        active_rules: &mut Vec<String>,
    ) -> Result<()> {
        let addr = self
            .launch_and_track(window, ctl, events, active_rules)
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
/// work — single-instance apps create windows in the existing process's
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
}

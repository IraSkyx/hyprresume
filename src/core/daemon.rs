use anyhow::{Context, Result};
use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook_tokio::Signals;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;
use crate::core::restore::RestoreEngine;
use crate::core::snapshot::SnapshotEngine;
use crate::core::state::StateManager;
use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener;
use crate::models::HyprEvent;
use crate::resolver::AppResolver;

pub async fn run(config: Config) -> Result<()> {
    let ctl = Arc::new(HyprCtl::from_env()?);
    let resolver = Arc::new(AppResolver::new(&config));
    let state = Arc::new(Mutex::new(StateManager::new(&config)));
    let snapshot = Arc::new(SnapshotEngine::new(&config)?);

    let (event_tx, mut event_rx) = mpsc::channel::<HyprEvent>(256);
    let paths = ctl.socket_paths().clone();
    let event_handle = tokio::spawn(async move {
        if let Err(e) = event_listener::listen(&paths, event_tx).await {
            tracing::error!("event listener error: {e}");
        }
    });

    populate_initial_state(&state, &resolver, &ctl).await?;

    if config.general.restore_on_start && snapshot.exists("last") {
        tracing::info!("waiting for compositor to settle...");
        wait_for_settle(&mut event_rx).await;
        tracing::info!("restoring previous session...");
        match snapshot.load("last") {
            Ok(session) => {
                let engine = RestoreEngine::new(
                    config.general.restore_geometry,
                    config.experimental.restore_layout,
                );
                let report = engine.restore(&session, &ctl).await?;
                if report.failed > 0 {
                    for (app, err) in &report.errors {
                        tracing::warn!("restore failed for {app}: {err}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("failed to load session 'last': {e}");
            }
        }
    }

    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGUSR1]).context("registering signal handlers")?;

    let save_interval = Duration::from_secs(config.general.save_interval);
    let mut save_timer = tokio::time::interval(save_interval);
    save_timer.tick().await;

    tracing::info!(
        "daemon running, tracking {} windows, saving every {}s",
        state.lock().await.window_count(),
        config.general.save_interval
    );

    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                handle_event(event, &state, &resolver, &ctl).await;
            }

            _ = save_timer.tick() => {
                let state = state.lock().await;
                if state.window_count() > 0
                    && let Err(e) = snapshot.save(&state, "last")
                {
                    tracing::error!("periodic save failed: {e}");
                }
            }

            Some(sig) = futures::StreamExt::next(&mut signals) => {
                match sig {
                    SIGTERM | SIGINT => {
                        tracing::info!("received signal {sig}, saving and exiting...");
                        let state = state.lock().await;
                        if let Err(e) = snapshot.save(&state, "last") {
                            tracing::error!("final save failed: {e}");
                        }
                        break;
                    }
                    SIGUSR1 => {
                        tracing::info!("received SIGUSR1, saving session...");
                        let state = state.lock().await;
                        if let Err(e) = snapshot.save(&state, "last") {
                            tracing::error!("manual save failed: {e}");
                        }
                    }
                    _ => {}
                }
            }

            else => {
                tracing::info!("event stream ended, saving final session...");
                let state = state.lock().await;
                if let Err(e) = snapshot.save(&state, "last") {
                    tracing::error!("final save failed: {e}");
                }
                break;
            }
        }
    }

    event_handle.abort();
    tracing::info!("daemon stopped");
    Ok(())
}

/// Drain events until there's a 1-second gap, indicating the compositor
/// and its extensions (bars, shells) have finished their startup burst.
async fn wait_for_settle(events: &mut mpsc::Receiver<HyprEvent>) {
    let quiet = Duration::from_secs(1);
    while let Ok(Some(_)) = tokio::time::timeout(quiet, events.recv()).await {}
    tracing::info!("compositor settled");
}

async fn populate_initial_state(
    state: &Arc<Mutex<StateManager>>,
    resolver: &Arc<AppResolver>,
    ctl: &HyprCtl,
) -> Result<()> {
    let clients = ctl.get_clients().await?;
    let mut state = state.lock().await;

    for c in clients {
        if !state.should_track(&c.class) {
            continue;
        }

        let launch_cmd = resolver.resolve(&c.class, c.pid).unwrap_or_default();

        state.add(crate::models::TrackedWindow {
            address: c.address,
            app_id: c.class,
            launch_cmd,
            workspace: c.workspace.name,
            position: c.at,
            size: c.size,
            floating: c.floating,
            fullscreen: c.fullscreen_mode > 0,
        });
    }

    let count = state.window_count();
    drop(state);
    tracing::info!("populated initial state: {count} windows");
    Ok(())
}

async fn handle_event(
    event: HyprEvent,
    state: &Arc<Mutex<StateManager>>,
    resolver: &Arc<AppResolver>,
    ctl: &HyprCtl,
) {
    match event {
        HyprEvent::OpenWindow {
            address,
            workspace,
            class,
        } => {
            // Check tracking eligibility without holding mutex across IPC
            let should_track = {
                let state = state.lock().await;
                state.should_track(&class)
            };

            if !should_track {
                return;
            }

            let (pid, position, size, floating, fullscreen) =
                match ctl.get_client_by_address(&address).await {
                    Ok(Some(c)) => (c.pid, c.at, c.size, c.floating, c.fullscreen_mode > 0),
                    _ => (-1, (0, 0), (0, 0), false, false),
                };

            let launch_cmd = resolver.resolve(&class, pid).unwrap_or_default();

            let mut state = state.lock().await;
            state.add(crate::models::TrackedWindow {
                address,
                app_id: class,
                launch_cmd,
                workspace,
                position,
                size,
                floating,
                fullscreen,
            });
        }
        HyprEvent::CloseWindow { address } => {
            let mut state = state.lock().await;
            state.remove(&address);
        }
        HyprEvent::MoveWindow { address, workspace } => {
            let mut state = state.lock().await;
            state.update_workspace(&address, &workspace);
        }
        HyprEvent::ChangeFloatingMode { address, floating } => {
            let mut state = state.lock().await;
            state.update_floating(&address, floating);
        }
        HyprEvent::Fullscreen { state } => {
            tracing::debug!("fullscreen toggled: {state}");
        }
        HyprEvent::Unknown(ref raw) => {
            tracing::trace!("unhandled event: {raw}");
        }
    }
}

use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::ipc::client::HyprSocketPaths;
use crate::models::HyprEvent;

pub fn parse_event(line: &str) -> HyprEvent {
    let Some((event, data)) = line.split_once(">>") else {
        return HyprEvent::Unknown(line.to_string());
    };

    match event {
        "openwindow" => {
            let parts: Vec<&str> = data.splitn(4, ',').collect();
            if parts.len() >= 4 {
                HyprEvent::OpenWindow {
                    address: parts[0].to_string(),
                    workspace: parts[1].to_string(),
                    class: parts[2].to_string(),
                }
            } else {
                HyprEvent::Unknown(line.to_string())
            }
        }
        "closewindow" => HyprEvent::CloseWindow {
            address: data.to_string(),
        },
        "movewindowv2" => {
            let parts: Vec<&str> = data.splitn(3, ',').collect();
            if parts.len() >= 3 {
                HyprEvent::MoveWindow {
                    address: parts[0].to_string(),
                    workspace: parts[2].to_string(),
                }
            } else if let Some((addr, ws)) = data.split_once(',') {
                HyprEvent::MoveWindow {
                    address: addr.to_string(),
                    workspace: ws.to_string(),
                }
            } else {
                HyprEvent::Unknown(line.to_string())
            }
        }
        "movewindow" => {
            if let Some((addr, ws)) = data.split_once(',') {
                HyprEvent::MoveWindow {
                    address: addr.to_string(),
                    workspace: ws.to_string(),
                }
            } else {
                HyprEvent::Unknown(line.to_string())
            }
        }
        "changefloatingmode" => {
            if let Some((addr, mode)) = data.split_once(',') {
                HyprEvent::ChangeFloatingMode {
                    address: addr.to_string(),
                    floating: mode.trim() == "1",
                }
            } else {
                HyprEvent::Unknown(line.to_string())
            }
        }
        "fullscreen" => HyprEvent::Fullscreen {
            state: data.trim() == "1",
        },
        _ => HyprEvent::Unknown(line.to_string()),
    }
}

/// Listen for events from the default Hyprland socket2 (from env).
pub async fn listen(paths: &HyprSocketPaths, tx: mpsc::Sender<HyprEvent>) -> Result<()> {
    listen_on(&paths.socket2, tx).await
}

/// Listen for events from a specific socket path.
pub async fn listen_on(path: &Path, tx: mpsc::Sender<HyprEvent>) -> Result<()> {
    tracing::info!("connecting to event socket: {}", path.display());

    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to socket2 at {}", path.display()))?;

    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let event = parse_event(&line);
        match &event {
            HyprEvent::Unknown(_) => {
                tracing::trace!("unhandled event: {line}");
            }
            _ => {
                tracing::debug!("event: {event:?}");
            }
        }

        if tx.send(event).await.is_err() {
            tracing::info!("event channel closed, stopping listener");
            break;
        }
    }

    tracing::info!("socket2 stream ended (Hyprland exited?)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openwindow() {
        let event = parse_event("openwindow>>abc123,2,firefox,Mozilla Firefox");
        match event {
            HyprEvent::OpenWindow {
                address,
                workspace,
                class,
            } => {
                assert_eq!(address, "abc123");
                assert_eq!(workspace, "2");
                assert_eq!(class, "firefox");
            }
            other => panic!("expected OpenWindow, got {other:?}"),
        }
    }

    #[test]
    fn parse_openwindow_title_with_commas() {
        let event = parse_event("openwindow>>abc,1,code,file.rs - project, edited");
        match event {
            HyprEvent::OpenWindow { class, .. } => {
                assert_eq!(class, "code");
            }
            other => panic!("expected OpenWindow, got {other:?}"),
        }
    }

    #[test]
    fn parse_openwindow_insufficient_parts() {
        let event = parse_event("openwindow>>abc,1");
        assert!(matches!(event, HyprEvent::Unknown(_)));
    }

    #[test]
    fn parse_closewindow() {
        let event = parse_event("closewindow>>0x55bee6666ea0");
        match event {
            HyprEvent::CloseWindow { address } => {
                assert_eq!(address, "0x55bee6666ea0");
            }
            other => panic!("expected CloseWindow, got {other:?}"),
        }
    }

    #[test]
    fn parse_movewindow() {
        let event = parse_event("movewindow>>0xabc,3");
        match event {
            HyprEvent::MoveWindow { address, workspace } => {
                assert_eq!(address, "0xabc");
                assert_eq!(workspace, "3");
            }
            other => panic!("expected MoveWindow, got {other:?}"),
        }
    }

    #[test]
    fn parse_movewindowv2_three_parts() {
        let event = parse_event("movewindowv2>>0xabc,5,workspace_5");
        match event {
            HyprEvent::MoveWindow { address, workspace } => {
                assert_eq!(address, "0xabc");
                assert_eq!(workspace, "workspace_5");
            }
            other => panic!("expected MoveWindow, got {other:?}"),
        }
    }

    #[test]
    fn parse_movewindow_no_comma() {
        let event = parse_event("movewindow>>just_an_address");
        assert!(matches!(event, HyprEvent::Unknown(_)));
    }

    #[test]
    fn parse_changefloatingmode_on() {
        let event = parse_event("changefloatingmode>>0xabc,1");
        match event {
            HyprEvent::ChangeFloatingMode { address, floating } => {
                assert_eq!(address, "0xabc");
                assert!(floating);
            }
            other => panic!("expected ChangeFloatingMode, got {other:?}"),
        }
    }

    #[test]
    fn parse_changefloatingmode_off() {
        let event = parse_event("changefloatingmode>>0xabc,0");
        match event {
            HyprEvent::ChangeFloatingMode { floating, .. } => {
                assert!(!floating);
            }
            other => panic!("expected ChangeFloatingMode, got {other:?}"),
        }
    }

    #[test]
    fn parse_changefloatingmode_no_comma() {
        let event = parse_event("changefloatingmode>>0xabc");
        assert!(matches!(event, HyprEvent::Unknown(_)));
    }

    #[test]
    fn parse_fullscreen_enter() {
        let event = parse_event("fullscreen>>1");
        match event {
            HyprEvent::Fullscreen { state } => assert!(state),
            other => panic!("expected Fullscreen, got {other:?}"),
        }
    }

    #[test]
    fn parse_fullscreen_exit() {
        let event = parse_event("fullscreen>>0");
        match event {
            HyprEvent::Fullscreen { state } => assert!(!state),
            other => panic!("expected Fullscreen, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_event() {
        let event = parse_event("workspace>>2");
        assert!(matches!(event, HyprEvent::Unknown(_)));
    }

    #[test]
    fn parse_no_separator() {
        let event = parse_event("garbage data without separator");
        assert!(matches!(event, HyprEvent::Unknown(_)));
    }

    #[test]
    fn parse_empty_string() {
        let event = parse_event("");
        assert!(matches!(event, HyprEvent::Unknown(_)));
    }

    #[test]
    fn parse_event_with_special_workspace() {
        let event = parse_event("openwindow>>abc,special:scratchpad,kitty,Terminal");
        match event {
            HyprEvent::OpenWindow { workspace, .. } => {
                assert_eq!(workspace, "special:scratchpad");
            }
            other => panic!("expected OpenWindow, got {other:?}"),
        }
    }
}

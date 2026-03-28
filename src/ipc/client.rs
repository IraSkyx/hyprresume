use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::models::{HyprClient, HyprMonitor};

pub struct SessionctlExpectation<'a> {
    pub app_id: &'a str,
    pub workspace: &'a str,
    pub monitor: &'a str,
    pub floating: bool,
    pub fullscreen: bool,
    pub maximized: bool,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Resolved Hyprland socket paths for a running instance.
#[derive(Debug, Clone)]
pub struct HyprSocketPaths {
    pub socket1: PathBuf,
    pub socket2: PathBuf,
}

impl HyprSocketPaths {
    pub fn from_env() -> Result<Self> {
        let his = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .context("HYPRLAND_INSTANCE_SIGNATURE not set — is Hyprland running?")?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
        let dir = PathBuf::from(runtime).join("hypr").join(his);
        Ok(Self {
            socket1: dir.join(".socket.sock"),
            socket2: dir.join(".socket2.sock"),
        })
    }

    #[cfg(test)]
    pub const fn new(socket1: PathBuf, socket2: PathBuf) -> Self {
        Self { socket1, socket2 }
    }
}

async fn send_recv(socket_path: &Path, request: &str) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to {}", socket_path.display()))?;
    stream.write_all(request.as_bytes()).await?;
    stream.shutdown().await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}

/// Single IPC handle to a Hyprland instance. Create once, pass by reference.
pub struct HyprCtl {
    paths: HyprSocketPaths,
}

impl HyprCtl {
    pub const fn new(paths: HyprSocketPaths) -> Self {
        Self { paths }
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self {
            paths: HyprSocketPaths::from_env()?,
        })
    }

    pub const fn socket_paths(&self) -> &HyprSocketPaths {
        &self.paths
    }

    async fn json(&self, command: &str) -> Result<String> {
        send_recv(&self.paths.socket1, &format!("j/{command}")).await
    }

    async fn plain(&self, command: &str) -> Result<String> {
        send_recv(&self.paths.socket1, command).await
    }

    pub async fn get_clients(&self) -> Result<Vec<HyprClient>> {
        let raw = self.json("clients").await?;
        serde_json::from_str(&raw).context("parsing hyprctl clients JSON")
    }

    pub async fn get_client_by_address(&self, address: &str) -> Result<Option<HyprClient>> {
        let clients = self.get_clients().await?;
        let normalized = address.trim_start_matches("0x");
        Ok(clients
            .into_iter()
            .find(|c| c.address.trim_start_matches("0x") == normalized))
    }

    pub async fn dispatch(&self, args: &str) -> Result<String> {
        self.plain(&format!("dispatch {args}")).await
    }

    pub async fn keyword(&self, args: &str) -> Result<String> {
        self.plain(&format!("keyword {args}")).await
    }

    pub async fn get_monitors(&self) -> Result<Vec<HyprMonitor>> {
        let raw = self.json("monitors").await?;
        serde_json::from_str(&raw).context("parsing hyprctl monitors JSON")
    }

    pub async fn get_monitor_map(&self) -> Result<HashMap<i64, String>> {
        let monitors = self.get_monitors().await?;
        Ok(monitors.into_iter().map(|m| (m.id, m.name)).collect())
    }

    pub async fn get_option(&self, name: &str) -> Result<bool> {
        let raw = self.plain(&format!("getoption {name}")).await?;
        Ok(raw.contains("int: 1"))
    }

    pub async fn get_option_str(&self, name: &str) -> Result<String> {
        let raw = self.plain(&format!("getoption {name}")).await?;
        for line in raw.lines() {
            if let Some(val) = line.strip_prefix("str: ") {
                return Ok(val.trim().trim_matches('"').to_string());
            }
        }
        anyhow::bail!("no str value in getoption response for {name}")
    }

    /// Query the active tiling layout (e.g. "dwindle", "master").
    pub async fn get_layout(&self) -> Result<String> {
        self.get_option_str("general:layout").await
    }

    // sessionctl IPC

    /// Begin an IPC-driven session restore. Clears any previous expectations.
    pub async fn sessionctl_begin(&self) -> Result<bool> {
        let resp = self.plain("sessionctl begin").await?;
        Ok(resp.trim() == "ok")
    }

    /// Register an expected window for compositor-side auto-restore.
    /// The compositor will apply this state when a window with the given
    /// `app_id` maps, without requiring the app to support the protocol.
    pub async fn sessionctl_expect(&self, exp: &SessionctlExpectation<'_>) -> Result<bool> {
        let cmd = format!(
            "sessionctl expect {} {} {} {} {} {} {} {} {} {}",
            exp.app_id,
            exp.workspace,
            exp.monitor,
            u8::from(exp.floating),
            u8::from(exp.fullscreen),
            u8::from(exp.maximized),
            exp.x,
            exp.y,
            exp.w,
            exp.h,
        );
        let resp = self.plain(&cmd).await?;
        Ok(resp.trim() == "ok")
    }

    /// Finalize the expectation registration.
    pub async fn sessionctl_end(&self) -> Result<bool> {
        let resp = self.plain("sessionctl end").await?;
        Ok(resp.trim() == "ok")
    }

    /// Signal that the restore is fully complete (including late windows).
    /// Clears all remaining expectations so new windows of the same class
    /// are no longer auto-placed.
    pub async fn sessionctl_finish(&self) -> Result<bool> {
        let resp = self.plain("sessionctl finish").await?;
        Ok(resp.trim() == "ok")
    }

    /// Check if the compositor supports sessionctl.
    pub async fn sessionctl_available(&self) -> bool {
        self.plain("sessionctl status")
            .await
            .map(|r| !r.contains("error") && !r.contains("unknown"))
            .unwrap_or(false)
    }

    // plugin management

    pub async fn plugin_load(&self, path: &std::path::Path) -> Result<String> {
        self.plain(&format!("plugin load {}", path.display())).await
    }

    pub async fn plugin_unload(&self, path: &std::path::Path) -> Result<String> {
        self.plain(&format!("plugin unload {}", path.display()))
            .await
    }

    pub async fn plugin_list(&self) -> Result<String> {
        self.plain("plugin list").await
    }

    pub async fn sessionctl_status(&self) -> Result<String> {
        self.plain("sessionctl status").await
    }
}

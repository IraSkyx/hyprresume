use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::models::{HyprClient, HyprMonitor};

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
}

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::models::HyprClient;

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
    #[cfg(test)]
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

    /// Launch a command with inline window rules (e.g. workspace placement, float).
    /// Rules are passed directly to the exec'd window, avoiding class-based matching races.
    pub async fn exec_with_rules(&self, rules: &[String], cmd: &str) -> Result<()> {
        let rules_str = rules.join("; ");
        self.dispatch(&format!("exec [{rules_str}] {cmd}")).await?;
        Ok(())
    }
}

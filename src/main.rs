mod config;
mod core;
mod ipc;
mod models;
mod resolver;

#[cfg(test)]
mod tests;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "hyprresume",
    version,
    about = "Session persistence daemon for Hyprland"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Log verbosity: -v (info), -vv (debug), -vvv (trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Suppress all output
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Save current session to disk
    Save {
        /// Session name (default: "last")
        name: Option<String>,
    },
    /// Restore a saved session
    Restore {
        /// Session name (default: "last")
        name: Option<String>,
    },
    /// List saved sessions
    List,
    /// Delete a saved session
    Delete {
        /// Session name to delete
        name: String,
    },
    /// Show what command a window class resolves to
    Resolve {
        /// Window class to resolve
        class: String,
    },
    /// Show current daemon status
    Status,
    /// Manage the Hyprland sessionctl plugin
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
}

#[derive(Subcommand)]
enum PluginAction {
    /// Install and load the plugin into Hyprland
    Install,
    /// Unload and remove the plugin
    Uninstall,
    /// Check if the plugin is loaded
    Status,
}

fn init_logging(verbose: u8, quiet: bool) {
    let level = if quiet {
        "error"
    } else {
        match verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };

    let filter = format!("hyprresume={level}");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose, cli.quiet);

    let cfg = config::Config::load(cli.config.as_deref())?;

    match cli.command {
        None => {
            tracing::info!("starting hyprresume daemon v{}", env!("CARGO_PKG_VERSION"));
            core::daemon::run(cfg).await?;
        }

        Some(Command::Save { name }) => {
            cmd_save(&cfg, name.as_deref()).await?;
        }

        Some(Command::Restore { name }) => {
            cmd_restore(&cfg, name.as_deref()).await?;
        }

        Some(Command::List) => {
            cmd_list(&cfg)?;
        }

        Some(Command::Delete { name }) => {
            let snapshot = core::snapshot::SnapshotEngine::new(&cfg)?;
            snapshot.delete(&name)?;
            println!("Deleted session '{name}'.");
        }

        Some(Command::Resolve { class }) => {
            cmd_resolve(&cfg, &class).await?;
        }

        Some(Command::Status) => {
            cmd_status(&cfg).await?;
        }

        Some(Command::Plugin { action }) => {
            cmd_plugin(action).await?;
        }
    }

    Ok(())
}

async fn cmd_save(cfg: &config::Config, name: Option<&str>) -> Result<()> {
    let name = name.unwrap_or("last");
    let ctl = ipc::client::HyprCtl::from_env()?;
    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;
    let resolver = resolver::AppResolver::new(cfg);
    let mut state = core::state::StateManager::new(cfg);

    let clients = ctl.get_clients().await?;
    let monitor_map = ctl.get_monitor_map().await.unwrap_or_default();
    for c in clients {
        if !state.should_track(&c.class) {
            continue;
        }
        let launch_cmd = resolver.resolve(&c.class, c.pid).unwrap_or_default();
        let profile = resolver::profile::detect_browser_profile(c.pid);
        let monitor = monitor_map.get(&c.monitor).cloned().unwrap_or_default();
        state.add(models::TrackedWindow {
            address: c.address,
            app_id: c.class,
            launch_cmd,
            workspace: c.workspace.name,
            monitor,
            position: c.at,
            size: c.size,
            floating: c.floating,
            fullscreen: c.fullscreen_mode > 0,
            pid: c.pid,
            profile,
        });
    }

    let path = snapshot.save(&state, name)?;
    println!("Session '{name}' saved to {}", path.display());
    Ok(())
}

async fn cmd_restore(cfg: &config::Config, name: Option<&str>) -> Result<()> {
    let name = name.unwrap_or("last");
    let ctl = ipc::client::HyprCtl::from_env()?;
    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;

    if !snapshot.exists(name) {
        bail!(
            "session '{name}' not found in {}",
            snapshot.session_dir().display()
        );
    }

    let session = snapshot.load(name)?;
    let engine = core::restore::RestoreEngine::new(cfg.general.restore_layout);
    let (report, watcher) = engine.restore(&session, &ctl).await?;

    println!(
        "Restored {}/{} apps{}",
        report.restored,
        report.restored + report.failed,
        if report.failed > 0 {
            format!(" ({} failed)", report.failed)
        } else {
            String::new()
        }
    );
    for (app, err) in &report.errors {
        eprintln!("  {app}: {err}");
    }

    if let Some(handle) = watcher {
        println!("Waiting for slow-starting apps (up to 60s)...");
        drop(handle.await);
    }
    Ok(())
}

fn cmd_list(cfg: &config::Config) -> Result<()> {
    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;
    let sessions = snapshot.list()?;

    if sessions.is_empty() {
        println!("No saved sessions.");
    } else {
        println!("{:<20} {:<24} ", "NAME", "SAVED AT");
        for (name, ts) in sessions {
            let dt = chrono::DateTime::from_timestamp(ts, 0).map_or_else(
                || "unknown".to_string(),
                |d| d.format("%Y-%m-%d %H:%M:%S").to_string(),
            );
            println!("{name:<20} {dt:<24}");
        }
    }
    Ok(())
}

async fn cmd_resolve(cfg: &config::Config, class: &str) -> Result<()> {
    let resolver = resolver::AppResolver::new(cfg);

    let pid = match ipc::client::HyprCtl::from_env() {
        Ok(ctl) => ctl.get_clients().await.map_or(-1, |clients| {
            clients
                .iter()
                .find(|c| c.class == class)
                .map_or(-1, |c| c.pid)
        }),
        Err(_) => -1,
    };

    if let Some(cmd) = resolver.resolve(class, pid) {
        println!("{class} → {cmd}");
    } else {
        eprintln!("Could not resolve '{class}' to a launch command");
        std::process::exit(1);
    }
    Ok(())
}

fn plugin_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
        .join("hyprresume")
}

fn plugin_path() -> std::path::PathBuf {
    plugin_dir().join("hyprland-sessionctl.so")
}

/// Locate the built plugin .so next to the current binary or in the
/// cargo target directory (dev builds).
fn find_bundled_plugin() -> Option<std::path::PathBuf> {
    let candidates = [
        // next to the hyprresume binary (installed via `make install`)
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("libhyprresume_plugin.so"))),
        // common build output paths
        std::env::current_exe().ok().and_then(|p| {
            p.parent()
                .and_then(std::path::Path::parent)
                .map(|d| d.join("libhyprresume_plugin.so"))
        }),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

async fn cmd_plugin(action: PluginAction) -> Result<()> {
    match action {
        PluginAction::Install => {
            let dest = plugin_path();
            let src = find_bundled_plugin().ok_or_else(|| {
                anyhow::anyhow!(
                    "plugin .so not found. build it first:\n  \
                     HYPRLAND_SOURCE=<path> cargo build -p hyprresume-plugin --release"
                )
            })?;

            std::fs::create_dir_all(plugin_dir())?;
            std::fs::copy(&src, &dest)?;
            println!("Installed plugin to {}", dest.display());

            let ctl = ipc::client::HyprCtl::from_env()?;
            let resp = ctl.plugin_load(&dest).await?;
            if resp.contains("ok") {
                println!("Plugin loaded.");
            } else {
                bail!("failed to load plugin: {resp}");
            }
            Ok(())
        }

        PluginAction::Uninstall => {
            let dest = plugin_path();
            if let Ok(ctl) = ipc::client::HyprCtl::from_env() {
                drop(ctl.plugin_unload(&dest).await);
            }
            if dest.exists() {
                std::fs::remove_file(&dest)?;
                println!("Plugin removed.");
            } else {
                println!("Plugin not installed.");
            }
            Ok(())
        }

        PluginAction::Status => {
            let ctl = ipc::client::HyprCtl::from_env()?;
            let resp = ctl.plugin_list().await?;
            if resp.contains("hyprland-sessionctl") {
                println!("Plugin is loaded.");
                let status = ctl.sessionctl_status().await?;
                print!("{status}");
            } else {
                println!("Plugin is not loaded.");
                if plugin_path().exists() {
                    println!("  Installed at: {}", plugin_path().display());
                    println!("  Run `hyprresume plugin install` to load it.");
                } else {
                    println!("  Run `hyprresume plugin install` to install and load it.");
                }
            }
            Ok(())
        }
    }
}

async fn cmd_status(cfg: &config::Config) -> Result<()> {
    let ctl = ipc::client::HyprCtl::from_env()?;
    match ctl.get_clients().await {
        Ok(clients) => {
            println!("Hyprland is running ({} windows)", clients.len());
        }
        Err(e) => {
            eprintln!("Cannot connect to Hyprland: {e}");
            std::process::exit(1);
        }
    }

    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;
    if snapshot.exists("last") {
        match snapshot.load("last") {
            Ok(session) => {
                let dt = chrono::DateTime::from_timestamp(session.session.timestamp, 0)
                    .map_or_else(
                        || "unknown".to_string(),
                        |d| d.format("%Y-%m-%d %H:%M:%S").to_string(),
                    );
                println!(
                    "Last session: {} apps, saved at {dt}",
                    session.windows.len()
                );
            }
            Err(e) => println!("Last session: error reading ({e})"),
        }
    } else {
        println!("No saved session.");
    }

    println!("Session dir: {}", snapshot.session_dir().display());
    println!("Config: {}", config::Config::default_path().display());
    Ok(())
}

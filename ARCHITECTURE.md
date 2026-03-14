# Architecture

Event-driven session persistence daemon for Hyprland. Tracks open windows via
IPC events, resolves launch commands through `.desktop` files and `/proc`, saves
sessions to disk and restores them on next login.

## Overview

```
┌──────────────────────────────────────────────────────┐
│                      hyprresume                      │
│                                                      │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────┐  │
│  │ Event Listener│─▶│ State Manager│─▶│ Snapshot  │  │
│  │  (socket2)   │  │  (in-memory) │  │ (to disk) │  │
│  └──────────────┘  └──────┬───────┘  └───────────┘  │
│                           │                          │
│  ┌──────────────┐  ┌──────▼───────┐  ┌───────────┐  │
│  │ App Resolver │◀─│    Daemon    │─▶│  Restore  │  │
│  │ (.desktop +  │  │  (main loop) │  │  Engine   │  │
│  │  /proc)      │  └──────────────┘  └─────┬─────┘  │
│  └──────────────┘                          │        │
│                                     ┌──────▼──────┐ │
│  ┌──────────────┐                   │  HyprCtl    │ │
│  │    Config    │                   │  (socket1)  │ │
│  │   (TOML)    │                   └─────────────┘ │
│  └──────────────┘                                   │
│                                                      │
│  CLI: save | restore | list | delete | daemon        │
└──────────────────────────────────────────────────────┘
```

## Components

### IPC (`src/ipc/`)

Two Hyprland Unix sockets:

- **socket1** (`HyprCtl`): request/response — query clients, dispatch commands, inject window rules.
- **socket2** (`EventListener`): streaming events — `openwindow`, `closewindow`, `movewindow`, `changefloatingmode`, `fullscreen`.

### Resolver (`src/resolver/`)

Given a window class and PID, resolves the command to relaunch it.

Resolution order:
1. User overrides from config (exact match or regex)
2. XDG `.desktop` file lookup (by `StartupWMClass`, desktop ID, or `Exec` basename)
3. Flatpak cgroup detection → `flatpak run <app-id>`
4. `/proc/pid/cmdline` fallback

CWD resolution (`cwd.rs`) walks child processes to find shell working directories for terminal windows.

Results are cached per window class.

### Core (`src/core/`)

- **State** (`state.rs`): in-memory `HashMap<address, TrackedWindow>` updated by events.
- **Snapshot** (`snapshot.rs`): serializes state to TOML, atomic writes via temp file + rename.
- **Restore** (`restore.rs`): injects temporary window rules then dispatches `exec` per app.
- **Layout** (`layout.rs`): infers BSP tree from saved window geometry for tiled layout reconstruction (dwindle only).
- **Daemon** (`daemon.rs`): main loop, connects event listener, runs periodic saves, handles signals.

### Config (`src/config.rs`)

TOML at `~/.config/hypr/hyprresume.toml`. Handles save interval, session directory, exclude patterns, per-class overrides, restore options.

### Models (`src/models.rs`)

Shared types: `TrackedWindow`, `SessionFile`, `HyprClient`, `HyprWorkspace`, `HyprEvent`, `DesktopEntry`.

## Signals

| Signal | Action |
|--------|--------|
| `SIGTERM` / `SIGINT` | Save session, exit |
| `SIGUSR1` | Immediate save |

## Source tree

```
src/
├── main.rs              # CLI (clap) + entry point
├── models.rs            # Shared data types
├── config.rs            # TOML config + defaults
├── ipc/
│   ├── client.rs        # socket1 (HyprCtl)
│   └── event_listener.rs # socket2 event stream
├── core/
│   ├── daemon.rs        # Main event loop
│   ├── state.rs         # In-memory session state
│   ├── snapshot.rs      # Save/load sessions
│   ├── restore.rs       # Restore sessions
│   └── layout.rs        # BSP layout inference (dwindle only)
├── resolver/
│   ├── mod.rs           # Resolution chain + cache
│   ├── desktop.rs       # XDG .desktop indexer
│   ├── proc.rs          # /proc/pid/cmdline
│   ├── flatpak.rs       # Flatpak cgroup detection
│   └── cwd.rs           # Terminal CWD resolution
└── tests/
    ├── cli.rs           # CLI integration tests
    ├── mock_ipc.rs      # Mock Hyprland sockets
    └── simulation.rs    # E2E session lifecycle
```

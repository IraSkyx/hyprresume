# hyprresume

Session persistence for [Hyprland](https://hyprland.org). Saves your open applications and puts them back on the right workspaces after a reboot.

- Resolves launch commands from `.desktop` files, Flatpak cgroups or `/proc`
- Saves sessions as human-readable TOML
- Restores apps to their original workspaces with floating window geometry
- Reconstructs tiling layouts by inferring the BSP tree from saved window positions (dwindle layout only)

## Install

**Arch Linux (AUR):**

```sh
paru -S hyprresume    # or yay, etc.
```

**From source:**

```sh
cargo install --path .
```

## Usage

Add to your `hyprland.conf`:

```conf
exec-once = hyprresume
```

This starts the daemon, which restores your previous session, tracks window changes and saves periodically. It also saves on logout/shutdown via SIGTERM.

### Commands

```sh
hyprresume save [name]      # snapshot current session
hyprresume restore [name]   # restore a saved session
hyprresume list             # list saved sessions
hyprresume delete <name>    # delete a session
hyprresume resolve <class>  # show what command a window class maps to
hyprresume status           # show daemon/session info
```

Use `-v` for info logs, `-vv` for debug, `-vvv` for trace.

## Configuration

Place a config file at `~/.config/hypr/hyprresume.toml`. All fields are optional; defaults are sane.

```toml
[general]
save_interval = 120           # seconds between auto-saves
session_dir = "~/.local/share/hyprresume"
restore_on_start = true
per_window_launch = true      # one process per window, not per unique app
restore_geometry = true       # restore floating window position/size
restore_layout = true         # reconstruct tiling layout (dwindle only)

[rules]
exclude = [
    "^xdg-desktop-portal.*",
    "^org\\.kde\\.polkit.*",
]
# include = ["^firefox$", "^code$"]  # allowlist mode

[overrides]
# "app.zen_browser.zen" = "flatpak run app.zen_browser.zen"
# "steam_app_.*" = ""  # empty = skip
```

A full example config is in [`assets/hyprresume.toml`](assets/hyprresume.toml).

### Tiling layout restoration

When `restore_layout` is enabled (the default), hyprresume saves every tiled window's position and size and reconstructs the layout on restore. It does this by:

1. Inferring a BSP (Binary Space Partition) tree from the saved window geometry
2. Replaying the tree via `layoutmsg preselect` to recreate the split structure
3. Iteratively resizing windows with `resizewindowpixel` to match saved proportions

**This requires the dwindle layout.** Dwindle is a BSP layout where `preselect` controls where the next window opens. The master layout uses a fundamentally different model and is not supported, windows will still be restored to the correct workspaces but tiling positions will fall back to default placement.

Your Hyprland config should include `preserve_split = true` so that split directions are stable:

```conf
dwindle {
    preserve_split = true
}
```

## Contributing

Requires Rust 1.93+ (pinned in `rust-toolchain.toml`).

```sh
git clone https://github.com/IraSkyx/hyprresume.git
cd hyprresume
```

Build and test:

```sh
make build      # dev build
make test       # run all tests
make lint       # clippy (pedantic)
make format     # rustfmt
```

The test suite runs entirely without a live Hyprland instance. It uses mock IPC sockets and simulated sessions.

## License

BSD-3-Clause

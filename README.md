# hyprresume

Session persistence for [Hyprland](https://hyprland.org). Remembers your open applications and restores them on startup, like you never logged out.

hyprresume runs as a background daemon, listens to Hyprland window events via IPC, and periodically saves your session to disk. On next launch it restores your apps to the right workspaces with the correct geometry.

## How it works

- Tracks window open/close/move/float events through Hyprland's socket2
- Resolves launch commands from `.desktop` files, Flatpak cgroups, or `/proc`
- Saves sessions as human-readable TOML
- Restores apps to their original workspaces with floating position and size
- Saves automatically on SIGTERM/SIGINT (logout, shutdown) and on a timer

## Install

```sh
cargo install --path .
```

Or build a release binary:

```sh
cargo build --release
# binary at target/release/hyprresume
```

## Usage

Start the daemon (typically from your `hyprland.conf`):

```conf
exec-once = hyprresume
```

Manual commands:

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
per_window_launch = false     # one process per app, not per window
restore_geometry = true       # restore floating window position/size

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

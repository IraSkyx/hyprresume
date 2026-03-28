# hyprresume

Session persistence for [Hyprland](https://hyprland.org). Saves your open applications and puts them back on the right workspaces after a reboot.

- Resolves launch commands from `.desktop` files, Flatpak cgroups or `/proc`
- Saves sessions as human-readable TOML
- Restores apps to their original workspaces with floating window geometry
- Reconstructs tiling layouts from saved window positions (dwindle and master layouts)
- Ships a Hyprland plugin for compositor-level window placement

## Install

### 1. Patched Hyprland

The plugin needs a small patch to Hyprland (a `window.preMap` event, [10-line diff](https://github.com/IraSkyx/Hyprland/commit/main)). Until this lands upstream, install Hyprland from the fork. This replaces your system Hyprland package.

**Arch Linux:**

```sh
cd pkg && makepkg -si -p PKGBUILD-hyprland
# reboot to run the patched compositor
```

**Other distros:** build [IraSkyx/Hyprland](https://github.com/IraSkyx/Hyprland) from source following the [Hyprland wiki](https://wiki.hyprland.org/Getting-Started/Installation/).

### 2. hyprresume + plugin

After rebooting into the patched Hyprland:

```sh
paru -S hyprresume              # AUR, or: cargo install --path .
hyprpm update --hl-url https://github.com/IraSkyx/Hyprland
hyprpm add https://github.com/IraSkyx/hyprresume
hyprpm enable hyprland-sessionctl
```

> `--hl-url` is needed because hyprpm fetches headers from upstream Hyprland by default, which doesn't have the `preMap` event yet. This flag will no longer be needed once the patch lands upstream. Use the same flag on subsequent `hyprpm update` calls after Hyprland updates.

### 3. Configuration

Add to `hyprland.conf`:

```conf
exec-once = hyprresume
```

The plugin loads automatically via hyprpm. The daemon saves sessions periodically and restores on startup.

## Usage

```sh
hyprresume                      # start daemon (auto-restore + periodic save)
hyprresume save [name]          # snapshot current session
hyprresume restore [name]       # restore a saved session
hyprresume list                 # list saved sessions
hyprresume delete <name>        # delete a session
hyprresume resolve <class>      # show what command a window class maps to
hyprresume status               # show daemon/session info
hyprresume plugin status        # check if the Hyprland plugin is loaded
```

Use `-v` for info logs, `-vv` for debug, `-vvv` for trace.

## Configuration

Place a config file at `~/.config/hypr/hyprresume.toml`. All fields are optional; defaults are sane.

```toml
[general]
save_interval = 120           # seconds between auto-saves
session_dir = "~/.local/share/hyprresume"
restore_on_start = true
restore_layout = true         # reconstruct tiling layout (dwindle + master)

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

When `restore_layout` is enabled (the default), hyprresume saves every tiled window's position and size and reconstructs the layout on restore. The active layout is auto-detected via `general:layout`.

**Dwindle** — infers a BSP tree from saved geometry, replays it via `layoutmsg preselect` and applies exact split ratios with `layoutmsg splitratio exact`. Your Hyprland config should include `preserve_split = true` so that split directions are stable:

```conf
dwindle {
    preserve_split = true
}
```

**Master** — infers orientation, master/stack partition and `mfact` from saved geometry. Windows are opened in the correct order (masters first, then stack) so the layout engine places them naturally.

Unsupported layouts fall back to simple restore: windows land on the correct workspaces but use Hyprland's default placement.

## Contributing

Requires Rust 1.94 (pinned in `rust-toolchain.toml`).

```sh
git clone https://github.com/IraSkyx/hyprresume.git
cd hyprresume
```

```sh
make build      # dev build
make test       # run all tests
make lint       # clippy (workspace-wide)
make format     # rustfmt
```

The test suite runs entirely without a live Hyprland instance. It uses mock IPC sockets and simulated sessions.

## License

BSD-3-Clause

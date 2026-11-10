# uwc

**uwc** is a Wayland compositor with an infinite 2D canvas, written in Rust on [smithay](https://github.com/Smithay/smithay).
Windows float at any position on an unbounded canvas, the screen is a camera you pan and zoom, and dwm-style tags give
each of nine workspaces its own canvas and view. It is built to be configured: looks and behaviour live in a KDL
config file, not in code.

It is the Rust rewrite of [uwm](https://github.com/undrscores/uwm) (C, wlroots), with the same ideas, config syntax and
IPC, improved where uwm was awkward.

> **Early software.** Everything is tested against headless and nested runs, but the real-hardware backend (DRM/KMS,
> libinput, VT switching) **has not been run on a GPU yet**, and neither has NVIDIA. Keep another way to log in.
> See [docs/STATUS.md](docs/STATUS.md).

## Features

- **Infinite canvas.** Pan, zoom about the pointer, overview, fit/center, viewport bookmarks. Zoom is sharp: clients
  re-render at the zoomed size instead of being stretched. Panning never resizes or refocuses a window.
- **Tags, not just workspaces.** Nine tags, each with its own canvas, viewport and zoom; view, toggle and send windows
  to any combination.
- **Find any window.** `Mod+/` labels every window and jumps to the one you type; `uwc msg jump firefox` fuzzy-finds by
  app-id and title on any tag, and `examples/uwc-jump` does it from a launcher.
- **Everything floats.** Windows can also have their own scale on the canvas, be pinned to the screen, minimized to a
  scratchpad, or go fullscreen.
- **Rice it.** Rounded corners, gradient borders, shadows, focus glow, opacity and dimming, animations (open, close,
  move, tag switch, eased pan and zoom, touchpad momentum), and your own GLSL for borders and the background.
  Window rules, `include`, `vars` and live reload. No client-side decorations and no built-in bar: bring your own.
- **Input.** Keyboard, mouse (Mod+drag move, resize and pan), touchpad gestures, touchscreens, tablets, XKB layouts and
  libinput settings; pointer constraints for games; input methods (fcitx5 and friends).
- **Multiple monitors** on one shared canvas, with a view per monitor and output management (wdisplays, kanshi).
- **Plays well with the rest of the desktop.** layer-shell bars and launchers, foreign-toplevel and ext-workspace for
  taskbars, Quickshell window previews, screen capture and portal screen sharing, session lock, idle notify and
  inhibit, clipboard managers, virtual keyboard and pointer, Xwayland.
- **Scriptable.** A control socket (`uwc msg`) speaks one JSON object per line: state, subscribe, get/set options,
  move and focus windows, pan and zoom.
- **Doesn't fall over.** `uwc doctor` checks your seat, GPU and libraries before the first run; a bug in a handler is
  logged and survived instead of taking every window down.
- **Session restore.** Window positions, sizes, tags and views come back when the apps do.

## Installation

**[INSTALL.md](INSTALL.md)** has instructions for NixOS (flake and module), Gentoo (ebuild), and building from source on
any other distro. In short, from a checkout:

```sh
make && sudo make install PREFIX=/usr
```

Then pick "uwc" in your display manager, or run `uwc-session` on a TTY. uwc needs seat access (logind, or seatd with
your user in its group). To try it without logging out, run `cargo run -- -s foot` inside an existing Wayland or X11
session: uwc opens as a window.

## Quick start

`Mod` is the Super key. `uwc` ships a default config and a terminal is the first thing you want:

| Keys | Action |
|---|---|
| `Mod+Return`, `Mod+T` | open `foot` |
| `Mod+Q` | close window |
| `Mod+H/J/K/L` | focus left/down/up/right |
| `Mod+Shift+H/J/K/L` | move the window; `Mod+Ctrl+H/J/K/L` resize it |
| `Mod+Alt+H/J/K/L` | pan the view |
| `Mod+O` | overview: fit every window on the screen |
| `Mod+/` | jump: label every window, type a label to go there |
| `Mod+minus`, `Mod+equal`, `Mod+Shift+0` | zoom out, zoom in, reset zoom |
| `Mod+1..9` | view tag; `Mod+Shift+1..9` send the window to a tag; `Mod+Ctrl+1..9` toggle a tag |
| `Mod+E`, `Mod+Shift+Space` | fullscreen, pin to the screen |
| `Mod+drag` | move (left button), pan (middle), resize (right); drag empty canvas to pan |
| `Mod+scroll` | zoom |
| `Ctrl+Alt+Backspace` | quit |

Every bind is in [`config.def.kdl`](config.def.kdl), which is also the documentation of every option.

## Configuration

uwc reads `~/.config/uwc/config.kdl` (or `-c FILE`, or `$UWC_CONFIG`) and reloads it when you save. The file only
needs what you change:

```kdl
options {
    corner-radius 10
    focus-gradient "#7aa2f7" "#bb9af7"
    shadow-size 24
}

rule {
    match app-id="mpv"
    open { fullscreen }
}
```

Start your bar, notification daemon and wallpaper from an executable `~/.config/uwc/startup`. Check a config with
`uwc -t`. Use `uwc msg --help` for the control socket, for example `uwc msg state`, `uwc msg set focus-color "#ff8800"`
or `uwc msg subscribe`. Your own shaders are documented in `config.def.kdl`, with examples in [`shaders/`](shaders).

## Documentation

- [INSTALL.md](INSTALL.md): installing, first start, startup script, screen sharing, troubleshooting
- [docs/STATUS.md](docs/STATUS.md): what works, what is untested, the roadmap
- [docs/NVIDIA.md](docs/NVIDIA.md): kernel and driver requirements
- [docs/HACKING.md](docs/HACKING.md): building and testing, source layout, coming from uwm
- [config.def.kdl](config.def.kdl): the default config and every option

## Contributing

`cargo build` and `cargo clippy --all-targets` stay warning-free, `cargo fmt` is run, and `cargo test` passes (it starts
a headless uwc and real Wayland clients). Read [docs/HACKING.md](docs/HACKING.md) and [CLAUDE.md](CLAUDE.md) first.
Bug reports: include `~/.local/state/uwc.log`, your GPU and driver.

## License

MIT.

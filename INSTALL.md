# Installing uwc

uwc is a Wayland compositor: you start it from a display manager or a TTY. Run inside another Wayland or X11
session it opens as a window, which is for trying it out. For now uwc is only available by
[building from source](#build-from-source); there are no distro packages yet.

Afterwards: [first start](#first-start), [config and startup](#config-and-startup),
[screen sharing](#screen-sharing), [troubleshooting](#troubleshooting).

> **Status.** The real-hardware backend (`--backend udev`, what a session uses) has been run on NVIDIA hardware by its
> author, but on little else. Expect rough edges on other setups, and keep another way to log in (a second TTY, or your
> current session) until you have seen it work. "Troubleshooting" says what to send when it fails.

## Build from source

You need:

- **Rust 1.88 or newer** (`rustup` is the easy way: <https://rustup.rs>; distro packages are often older).
- **A C toolchain and `pkg-config`.**
- **Development packages** for libxkbcommon, libinput, libudev, libgbm (Mesa), libseat, libEGL, libGLES and libdrm.
  EGL and GLES are opened at run time, so the *runtime* Mesa (or vendor GL) packages must be installed too.
- Optional: `Xwayland` (X11 apps), `xdg-desktop-portal-wlr` and `xdg-desktop-portal-gtk` (screen sharing), a bar and a
  notification daemon.

Package names (Debian/Ubuntu is what the author builds with; the others are the same libraries under their usual
names and were **not tried**):

```sh
# Debian / Ubuntu
sudo apt install build-essential pkg-config libwayland-dev libxkbcommon-dev libxkbcommon-x11-0 libinput-dev \
    libudev-dev libgbm-dev libseat-dev libegl-dev libgles-dev libdrm-dev

# Fedora
sudo dnf install gcc pkgconf-pkg-config wayland-devel libxkbcommon-devel libinput-devel systemd-devel \
    mesa-libgbm-devel libseat-devel mesa-libEGL-devel mesa-libGLES-devel libdrm-devel

# Arch
sudo pacman -S --needed base-devel pkgconf wayland libxkbcommon libinput systemd-libs mesa libdrm seatd

# openSUSE
sudo zypper install gcc pkgconf-pkg-config wayland-devel libxkbcommon-devel libinput-devel libudev-devel \
    Mesa-libgbm-devel libseat-devel Mesa-libEGL-devel Mesa-libGLESv2-devel libdrm-devel

# Void
sudo xbps-install base-devel pkg-config wayland-devel libxkbcommon-devel libinput-devel eudev-devel \
    MesaLib-devel seatd-devel libdrm-devel
```

Build and install. The Makefile builds with cargo and installs the binary, the session script, the display-manager
entry, the portal config, and `config.def.kdl` plus the example shaders under `share/uwc`:

```sh
git clone https://github.com/undrscores/uwc
cd uwc
make                         # cargo build --release, as your user
sudo make install PREFIX=/usr
```

`PREFIX` defaults to `/usr/local`, but many display managers only read `/usr/share/wayland-sessions`, so use
`PREFIX=/usr` (or copy `uwc.desktop` there by hand). `sudo make uninstall PREFIX=/usr` removes what `install` put down.

You do not have to install to try it:

- Inside a Wayland or X11 session: `cargo run -- -s foot` starts uwc as a window and runs `foot` in it.
- On a TTY: `target/release/uwc --backend udev` starts a real session. `uwc-session` (the installed script) also
  logs to `~/.local/state/uwc.log` and runs your startup script.

Seat access: logind (systemd or elogind) hands it to a login session on a TTY or from a display manager. With seatd,
run `seatd` and be in its group (`seat` or `_seatd`, depending on the distro).

## First start

Before the first real session, run `uwc doctor`. It checks the seat, the GPU and driver (including the NVIDIA
settings), input devices, the libraries uwc opens, Xwayland and the portals, and says what to do about each problem;
it exits non-zero if something would stop a session from starting.

1. Switch to a free TTY (Ctrl+Alt+F3), log in and run `uwc-session`, or choose "uwc" in your display manager.
2. You get an empty canvas with no bar. Mod is the Super key; Mod+Return (or Mod+T) opens `foot`, so have a terminal
   installed or change the bind. The other default binds are listed in `config.def.kdl`
   (installed as `share/uwc/config.def.kdl`, e.g. `/usr/share/uwc/config.def.kdl`). Ctrl+Alt+Backspace quits, Ctrl+Alt+F1..F12 switch VTs.
3. If the screen stays black or it exits at once, the reason is in `~/.local/state/uwc.log` (the run before is in
   `uwc.log.old`).

## Config and startup

- `~/.config/uwc/config.kdl` is read when present and reloaded when you save it; a file only needs what it changes.
  `config.def.kdl` is the built-in default and documents every option. `uwc -t` checks a config and exits.
- A config with a `binds` section **replaces** the default binds, so copy the whole file first when you want your own.
- `~/.config/uwc/startup`, if executable, runs once the compositor is up: start your bar, notification daemon,
  wallpaper and idle daemon from it. For example:

  ```sh
  #!/bin/sh
  waybar &
  mako &
  exec foot
  ```

- `uwc msg state`, `uwc msg subscribe` and `uwc msg --help` talk to the running compositor.

## Screen sharing

Browsers, OBS and video-call apps share the screen through xdg-desktop-portal. Install `xdg-desktop-portal`,
`xdg-desktop-portal-wlr` and `xdg-desktop-portal-gtk`; `uwc-portals.conf` (installed with uwc, to
`share/xdg-desktop-portal/`) selects the wlr backend for ScreenCast and Screenshot. The session script imports
`WAYLAND_DISPLAY` and `XDG_CURRENT_DESKTOP=uwc` into D-Bus and systemd so the portals see the session. `grim` works
with no portal. Sharing goes through shared-memory buffers (no dmabuf path yet): fine for a window or one monitor,
heavier at high resolutions.

## Troubleshooting

- **Anything else:** run `uwc doctor` first and include its output in a bug report.
- **Black screen or exits at once:** read `~/.local/state/uwc.log`; `RUST_LOG=debug uwc-session` logs more.
- **Cannot open a session or seat:** you are not on a logind or seatd session. Log in on a real TTY (not over ssh) and,
  with seatd, join its group.
- **NVIDIA:** the driver needs `nvidia_drm.modeset=1`; uwc refuses to start without it and says how to fix it. Driver 560
  or newer for explicit sync. docs/NVIDIA.md has the details.
- **Reporting a failure:** attach `~/.local/state/uwc.log` and `uwc.log.old`, your GPU and driver, and whether it was
  started from a TTY or a display manager.

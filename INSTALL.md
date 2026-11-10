# Installing uwc

uwc is a Wayland compositor: you start it from a display manager or a TTY. Run inside another Wayland or X11
session it opens as a window, which is for trying it out. Pick your distro:

- [NixOS](#nixos) (flake and module)
- [Gentoo](#gentoo) (live ebuild in a local overlay)
- [Any other distro: build from source](#any-other-distro-build-from-source)

Afterwards: [first start](#first-start), [config and startup](#config-and-startup),
[screen sharing](#screen-sharing), [troubleshooting](#troubleshooting).

> **Status.** The real-hardware backend (`--backend udev`, what a session uses) has not been run on a GPU yet, and
> neither has NVIDIA. Expect rough edges on the first run, and keep another way to log in (a second TTY, or your
> current session) until you have seen it work. "Troubleshooting" says what to send when it fails.

## NixOS

The repository is a flake. Add it as an input and enable the module:

```nix
# flake.nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.uwc.url = "github:undrscores/uwc";
  inputs.uwc.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, uwc, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        uwc.nixosModules.default
        { programs.uwc.enable = true; }
        ./configuration.nix
      ];
    };
  };
}
```

`programs.uwc.enable` installs the package, adds the "uwc" session to your display manager (it also runs as
`uwc-session` from a TTY), turns on `hardware.graphics`, and sets up xdg-desktop-portal so screen sharing works
(`programs.uwc.portals = false` leaves portals alone). Seat access comes from logind, which NixOS enables by default.

Without the module, or just to try it:

```sh
nix run github:undrscores/uwc        # nested in your current Wayland/X11 session
nix build github:undrscores/uwc      # ./result/bin/uwc, ./result/share/wayland-sessions/uwc.desktop
nix develop github:undrscores/uwc    # a shell with the Rust toolchain, the libraries and the test tools
```

An overlay is available too: `nixpkgs.overlays = [ uwc.overlays.default ];` gives you `pkgs.uwc`.

NVIDIA on NixOS needs `hardware.nvidia.modesetting.enable = true;` (it sets `nvidia_drm.modeset=1`); docs/NVIDIA.md has
the rest.

**What was tested:** the package builds from `packaging/nix/package.nix` against current nixpkgs and starts headless;
the NixOS module evaluates inside a full system configuration and produces the session, the graphics setting and the
portal config. **What was not:** `flake.nix` itself was only parsed, not evaluated (flake inputs could not be fetched
where I wrote it), so the `nix run` / `nix develop` lines above are untested, and no `flake.lock` exists yet. Nix
creates it on first use; commit it.

## Gentoo

There is a live (`-9999`) ebuild in `packaging/gentoo`, which is a small overlay by itself. Point Portage at a checkout:

```sh
git clone https://github.com/undrscores/uwc ~/src/uwc

# /etc/portage/repos.conf/uwc.conf
[uwc]
location = /home/YOU/src/uwc/packaging/gentoo
auto-sync = no
```

Live ebuilds are unkeyworded, and cargo downloads the crates while the package unpacks, so the build needs the
network. As root:

```sh
mkdir -p /etc/portage/env /etc/portage/package.accept_keywords /etc/portage/package.env
echo 'gui-wm/uwc **' >> /etc/portage/package.accept_keywords/uwc
echo 'FEATURES="-network-sandbox"' > /etc/portage/env/no-network-sandbox.conf
echo 'gui-wm/uwc no-network-sandbox.conf' >> /etc/portage/package.env/uwc
emerge --ask gui-wm/uwc
```

To update: `git -C ~/src/uwc pull`, then `emerge -1 gui-wm/uwc` (or `emerge @live-rebuild`).

USE flags: `portals` (default on) pulls xdg-desktop-portal with the wlr and GTK backends for screen sharing and
installs `uwc-portals.conf`; `xwayland` pulls Xwayland for X11 apps (the config enables it by default).

Seat access: with **elogind** or **systemd**, build `sys-auth/seatd` with `USE=elogind` or `USE=systemd` so libseat
talks to logind, and log in on a TTY or from a display manager that starts a logind session. With plain **seatd**,
enable the `seatd` service and add your user to the `seat` group. The compiler must be Rust 1.88 or newer
(`dev-lang/rust` or `dev-lang/rust-bin`).

**This ebuild has not been built on a Gentoo system or run through `pkgcheck`.** If `emerge` complains about a
dependency atom or a USE flag, the dependency block in `packaging/gentoo/gui-wm/uwc/uwc-9999.ebuild` is the place to fix
it; a report of what needed changing would be welcome.

## Any other distro: build from source

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

- **Black screen or exits at once:** read `~/.local/state/uwc.log`; `RUST_LOG=debug uwc-session` logs more.
- **Cannot open a session or seat:** you are not on a logind or seatd session. Log in on a real TTY (not over ssh) and,
  with seatd, join its group.
- **NVIDIA:** the driver needs `nvidia_drm.modeset=1`; uwc refuses to start without it and says how to fix it. Driver 560
  or newer for explicit sync. docs/NVIDIA.md has the details.
- **Reporting a failure:** attach `~/.local/state/uwc.log` and `uwc.log.old`, your GPU and driver, and whether it was
  started from a TTY or a display manager.

# uwc

A Wayland compositor with an infinite 2D canvas and real workspaces, in Rust on [smithay](https://github.com/Smithay/smithay).
The rewrite of [uwm](https://github.com/undrscores/uwm) (C, wlroots + scenefx): the same ideas, the same config
syntax and the same IPC.

- Windows have positions on an unbounded canvas; the screen is a camera (viewport) with zoom.
- dwm-style tags (9), each with its own canvas, viewport and zoom.
- Everything floats. No tiling, no client-side decoration, no built-in bar.
- Looks and behaviour are configuration (`config.def.kdl`), not code.

## Status

What works (covered by `cargo test`; rendering also checked by eye nested under Xvfb):

| Area | State |
|---|---|
| Canvas, viewport, zoom (about the pointer), pan, overview, fit/center | done |
| Tags: view / toggle / send-to, per-tag viewport, previous | done |
| xdg-shell toplevels and popups, xdg-decoration (server side), shm, linux-dmabuf, seat, data device, output | done |
| Focus: sloppy focus, click to focus, directional and cyclic keys, pan-to-reveal for keyboard focus only | done |
| Mouse: Mod+drag move / resize / pan, empty-canvas drag pans, Mod+scroll zooms | done |
| Pointer drawn by uwc: xcursor theme (`cursor-theme`, `cursor-size`), client cursor surfaces, built-in fallback arrow | done |
| Config: KDL (uwm's parser, ported), `include`, `vars`, validation as a whole, live reload | done |
| `input { keyboard / touchpad / mouse }` (XKB layout, repeat, libinput settings), `monitor "NAME" { mode scale transform off }` | done |
| IPC: `state` (also `work_area`, `layers`, `keyboard_focus`, `backend`), `subscribe`, `get`, `set` (any option but `mod-key`, live until the next reload), `pan`, `panto`, `zoom`, `view`, `toggleview`, `focus`, `close`, `move`, `resize`, `overview`, `home`, `reload`, `quit` | done |
| Backends: `udev` (real session), `winit` (nested), `headless` (tests) | done, see below |
| Buffer sync: explicit sync (`linux-drm-syncobj-v1`) on udev; commits wait for a dmabuf's acquire point or implicit fences (all backends) | done |
| NVIDIA: modesetting check with the fix in the error, `hardware-cursor` / `direct-scanout` / `explicit-sync` switches | done |
| Decoration (uwm's options and names): `corner-radius`, gradient borders (`border-` / `focus-gradient`, `border-spin-ms`), shadows and the focus glow (`shadow-*`, `focus-shadow-*`), `backdrop-color`, `opacity` / `inactive-opacity`, `inactive-dim`; all scale with zoom; GLES shaders, square solid borders if they fail to compile | done |
| ext-workspace-v1: the tags as workspaces "1".."9" (ids `uwc-tag-N`, 1-D coordinates, active/urgent) for bars that speak it; activate/deactivate applied per commit (only activations = view those tags, with a deactivation = toggle) | done |
| Pinned windows (screen-fixed, unzoomed, on every tag), minimize/restore (the scratchpad, newest first, comes back on your tag), fullscreen (unscaled, undecorated, over bars and `fullscreen-bg`, size restored after; client requests too); actions `toggle-pin`, `minimize-window`, `restore-window`, `toggle-fullscreen`; IPC `pin`/`unpin`, `minimize`/`restore`, `fullscreen`/`unfullscreen` ID | done |
| Window rules (uwc's own design, not uwm's): `rule { match ...; exclude ...; open { ... }; <decoration> }`; exact or glob app-id/title, state matchers (`focused`, `tag`, `pinned`, `fullscreen`, `urgent`) that decoration follows live; `open { tag size position pinned fullscreen minimized focus }` applied once (size and fullscreen in the first configure); IPC `rule { ... }` / `rule clear` / `rules`, `get-window ID` (a window's resolved decoration) | done |
| Viewport bookmarks: 9 per tag, `bookmark-save` / `bookmark-goto` (Mod+Alt+[Shift+]1..9), IPC `bookmark save\|goto N`, listed per tag in the snapshot | done |
| wlr-layer-shell: bars, docks, launchers, notifications, wallpapers; exclusive zones shrink the work area (where windows open and what reveal/center/fit use); exclusive and on-demand keyboard interactivity; layer popups | done |

**The udev backend has not been run on hardware yet.** It builds against smithay 0.7's real DRM/GBM/libinput/libseat
types and was reviewed against smithay's source, but the machine it was written on has no GPU or seat. Expect the first
run on a real TTY to turn up something. What it does: libseat session (VT switching with Ctrl+Alt+F1..F12, pause and
resume), the primary GPU via udev, KMS through smithay's `DrmOutputManager` (hardware cursor plane when available),
connector hotplug, libinput with the config's touchpad/mouse settings, linux-dmabuf with feedback for GL clients.
Limits: one GPU, and one output at a time (the first connected one not marked `off`) until multi-monitor.

Not yet (roughly in the order to do them):

1. Multiple monitors (the state has one `Monitor`; on hold).
2. Gestures (`swipe`, `pinch`, touchpad pan/zoom); `theme` and `palette` come with the ricing below.
3. Animations (open/close/move/tag-switch, eased pan, momentum), driven from the frame callback, never timers.
4. The rest of the ricing: blur (and ext-background-effect for bars), the canvas grid, wallpaper, per-tag looks, window
   rules for decoration, `focus-ms` fades, theme switching and the `themes/` directory. Until the grid exists, uwm's
   themes (which set `grid`) are rejected as a whole.
5. Popup grabs (clicking outside a menu closes it), xdg-activation, idle-inhibit/idle-notify, session-lock, pointer
   constraints, gestures, fractional-scale, Xwayland, input-method/text-input, screencopy, output-management.
6. Multi-GPU, animated cursors, DPMS.

## Build and run

Needs a Rust toolchain (edition 2021) and the system libraries smithay links: `libwayland`, `libxkbcommon`,
`libinput`, `libudev`, `libgbm`, `libseat`, `libegl`/`libgles`, `libdrm`. On Debian/Ubuntu:
`apt install libwayland-dev libxkbcommon-dev libxkbcommon-x11-0 libinput-dev libudev-dev libgbm-dev libseat-dev libegl-dev libgles-dev libdrm-dev`.

```sh
cargo build
cargo run -- -s foot                 # nested (winit) when WAYLAND_DISPLAY/DISPLAY is set; -s runs a command once up
cargo run -- --headless 1280x720     # no display, for tests
cargo run -- -t                      # validate the config and exit
cargo test                           # unit tests + end-to-end tests against a headless uwc
python3 tests/nested.py              # drawing and real pointer input, nested in Xvfb (needs Xvfb, xdotool, ImageMagick, PIL)
cargo run --example paint -- "red" 400 300 d03030    # a test client that draws a solid window
cargo run --example paint -- --bar 32 30a0d0         # ... or a layer-shell bar
```

### As a session

```sh
make                      # cargo build --release
sudo make install         # /usr/local/bin/uwc, uwc-session, share/wayland-sessions/uwc.desktop
```

Then pick "uwc" in your display manager, or log in on a TTY and run `uwc-session` (plain `uwc` works too; with no
`WAYLAND_DISPLAY`/`DISPLAY` set it picks the udev backend). It needs seat access: logind (systemd) or seatd with your
user in its group. `uwc-session` logs to `~/.local/state/uwc.log` (the previous run in `uwc.log.old`), exports the
session variables to D-Bus/systemd for portals, then runs `~/.config/uwc/startup` if it is executable (start your bar,
notification daemon, etc. there). Ctrl+Alt+Backspace quits; Ctrl+Alt+F1..F12 switch VTs.

### NVIDIA

The proprietary driver works through the same GBM/KMS path as Mesa drivers, with these conditions:

- **Kernel modesetting must be on**: `nvidia_drm.modeset=1` (and `nvidia_drm.fbdev=1`) on the kernel command line,
  or `options nvidia_drm modeset=1 fbdev=1` in `/etc/modprobe.d/` plus an initramfs rebuild. uwc checks
  `/sys/module/nvidia_drm/parameters/modeset` and refuses to start with that instruction if it is off.
- **Driver 560 or newer** for explicit sync (`linux-drm-syncobj-v1`). NVIDIA has no implicit sync, so without it
  GL/Vulkan clients (browsers, games, Electron apps) can flicker or show old frames. uwc offers the protocol whenever the
  kernel driver supports syncobj eventfds and logs whether it did.
- If the pointer or fullscreen apps misbehave, try `hardware-cursor false` or `direct-scanout false` in `options`.
- Shaders (for the decoration work) are written for NVIDIA's stricter GLSL compiler: no implicit int/float conversion,
  no redefined builtins (uwm hit this with scenefx's `clamp`).

None of this has run on NVIDIA hardware yet.

Config: `-c FILE`, else `$UWC_CONFIG`, else `~/.config/uwc/config.kdl`. `config.def.kdl` is the built-in default and the
documentation; a file only needs what it changes, and it is reloaded when saved. Control socket: `$UWC_SOCKET`
(`$XDG_RUNTIME_DIR/uwc-<wayland display>.sock`); `socat - UNIX-CONNECT:$UWC_SOCKET` then type `state`.

## Layout

```
src/main.rs        CLI, event loop, signals, config watching
src/canvas.rs      Viewport maths (pure, unit-tested), tag constants
src/config.rs      config model, includes/vars, binds, validation
src/options.rs     every option once: name, field, range -> parsing, set, get
src/kdl.rs         the KDL subset parser (a port of uwm's kdl.h)
src/state.rs       Uwc state: windows, tags, viewport, focus, actions
src/handlers.rs    wayland protocol handlers
src/input.rs       keyboard/pointer/drag handling, backend-independent
src/render.rs      the canvas as a list of render elements (zoom = RescaleRenderElement)
src/ipc.rs         control socket
src/layer.rs       wlr-layer-shell: hit-testing, keyboard interactivity, work area, drawing
src/rules.rs       window rules and the per-window decoration settings (`Deco`)
src/deco.rs        decoration shaders: rounded box/ring with gradients, shadow, rounded window content
src/workspace.rs   ext-workspace-v1 (hand-written: smithay has none)
src/sync.rs        explicit sync and dmabuf readiness (commits wait for the GPU)
src/cursor.rs      the pointer image (xcursor theme, client surfaces, fallback arrow)
src/backend/       udev (real session), winit (nested), headless (tests)
tests/headless.rs  end-to-end: spawn uwc --headless, connect real wayland clients, drive over IPC
```

## Differences from uwm worth knowing

- Zoom is free: smithay draws a list of elements, so a window is a `RescaleRenderElement` around its surface tree.
  uwm needs `zoomtree()` to rewrite every scene node every frame.
- The pointer is kept in canvas coordinates as far as clients are concerned (the seat's location is canvas px, the
  focus origin is the surface's canvas position), so zoom needs no special casing in surface hit-testing.
- The `kdl` crate was tried and dropped: it rejects `vars { accent "#fff" }` (no `;` before the brace), which uwm
  configs use. `src/kdl.rs` accepts exactly what uwm's parser does.
- The environment variable names are `UWC_CONFIG` / `UWC_SOCKET`.

## License

MIT, see `LICENSE` once added (uwm's is the model).

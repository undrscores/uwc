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
| Per-window scale: a window can be smaller or larger on the canvas than its size (a sticky note), still usable and sharp; `scale-window F` / `scale-window-reset` (Mod+Ctrl+minus/equal/0), Mod+Ctrl+scroll on a window, IPC `window-scale ID S`, rule `open { scale }`; pinned windows keep theirs | done |
| Your own shaders: `border-shader` (per window through rules) and `background-shader` GLSL files, a small documented contract (`border(p, size, time, color, focused)`, `background(p, size, time, view, zoom)`); reloaded on save, redrawn every frame only if they use time, built-in fallback with the GLSL compiler's own message (line numbers into your file) in the log and the IPC state (`shader_errors`); examples in `shaders/` (neon border, canvas dot grid, aurora) | done |
| Session restore: views, bookmarks and every window's layout (position or pinned spot, size, tags, scale, minimized, fullscreen) saved to `~/.local/state/uwc/session.json` on quit and every few seconds; windows go back when their apps open again (app-id + title, else app-id); `session-restore`, IPC `session save\|forget` | done |
| Sharp zoom: windows are asked (fractional-scale-v1, viewporter, wl_surface v6 preferred buffer scale) to render at output scale x zoom once the zoom settles, so zooming in shows real detail; bounded by `zoom-render-scale-min`/`-max` | done |
| Eased view moves: pan keys, zoom (scroll, keys, reset, IPC `zoom`; about the pointer or the given point), center/fit, reveal, bookmarks, `home` and IPC `pan`/`panto` glide to their target over `view-ms` (0 = jump), built on frame callbacks with exponential smoothing so a new target mid-move just bends the path; dragging stays instant; IPC reports the target and `view_moving` | done |
| Window animations: a new window fades and grows in (`open-ms`, `open-scale`), moves by key or IPC glide (`move-ms`; dragging stays instant), windows that come into view with a tag switch fade in (`tag-ms`), and a closing window's last frame — captured when the close is asked for, before the client destroys its surface — fades out and shrinks (`close-ms`), border, shadow and dim fading with it; visual only (`Client.fx`, applied in `place`; ghosts are never hit-tested, focused, saved or reported), so IPC positions are the resting ones and `animating` says when everything has settled; `0` turns each off | done |
| Tags: view / toggle / send-to, per-tag viewport, previous | done |
| xdg-shell toplevels and popups (with grabs: a click outside a menu closes it, and is not also a focus change or a drag), xdg-decoration (server side), shm, linux-dmabuf, seat, data device, output | done |
| xdg-activation (apps ask for focus: a fresh token focuses the window and reveals it, an unseen one goes `urgent`, answered by focusing), ext-idle-notify + zwp-idle-inhibit (idle daemons; video players keep the screen on; real input resets the timers) | done |
| ext-session-lock-v1 (swaylock, waylock, Quickshell's lock screen): the lock surface covers every output and takes all input; nothing else is drawn, key bindings do not run, screencopy and window previews fail while locked; a lock client that dies without unlocking leaves the screen blank (locked) | done |
| zwp-pointer-constraints-v1 + zwp-relative-pointer-manager-v1 (games, 3D apps): a confined pointer is kept inside the window (or the region it names), a locked one holds the cursor still (uwc's cursor is not drawn) and hands the motion to the client as relative deltas; constraints activate when the pointer is on the surface and deactivate when it leaves | done |
| Xwayland (`xwayland true`, needs the Xwayland binary; real sessions only): X11 windows become clients like any other -- pinned to the monitor they asked for, with uwc's decoration, focus, move / resize / fullscreen and the IPC; override-redirect windows (X menus, tooltips) are drawn on top and take the pointer by their own geometry; no clipboard bridging yet | done |
| Multiple monitors: all outputs tiled left to right, one shared canvas and tagset, a viewport and bookmarks per monitor per tag; the pointer's monitor is where view actions, new windows and pins land; a window is drawn on every monitor whose view shows it, fullscreen covers the monitor it is on | done |
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
| Window previews: hyprland-toplevel-export-v1 (what Quickshell's `ScreencopyView` uses for a `ToplevelManager` toplevel; also by uwc window id): the window alone at the output scale, no decoration or zoom, into an shm buffer; damage-waiting copies are answered on the window's next commit, so live previews follow the app's frame rate | done |
| Output capture: wlr-screencopy (grim, screen recorders, Quickshell's monitor `ScreencopyView`): the whole output or a region, pointer optional, `copy_with_damage` answered on the next drawn frame | done |
| wlr-foreign-toplevel-management: the window list for taskbars and shells (Quickshell's ToplevelManager, Waybar): title, app-id, output, activated/minimized/fullscreen; activate (restores and reveals), close, minimize, fullscreen | done |
| wlr-layer-shell: bars, docks, launchers, notifications, wallpapers; exclusive zones shrink the work area (where windows open and what reveal/center/fit use); exclusive and on-demand keyboard interactivity; layer popups | done |

**The udev backend has not been run on hardware yet.** It builds against smithay 0.7's real DRM/GBM/libinput/libseat
types and was reviewed against smithay's source, but the machine it was written on has no GPU or seat. Expect the first
run on a real TTY to turn up something. What it does: libseat session (VT switching with Ctrl+Alt+F1..F12, pause and
resume), the primary GPU via udev, KMS through smithay's `DrmOutputManager` (hardware cursor plane when available),
connector hotplug, libinput with the config's touchpad/mouse settings, linux-dmabuf with feedback for GL clients.
Limits: one GPU. Multiple outputs are scanned out per connector (each gets its own view of the shared
canvas), but the machine this was written on has no GPU or seat, so the udev path is untested on hardware.

Not yet (roughly in the order to do them):

1. Gestures (`swipe`, `pinch`, touchpad pan/zoom); `theme` and `palette` come with the ricing below.
2. The rest of the animations (momentum; open, move, tag fade, the close fade and eased pan and zoom are done), driven
   from the frame callback, never timers.
3. The rest of the ricing: blur (and ext-background-effect for bars), the canvas grid, wallpaper, per-tag looks, window
   rules for decoration, `focus-ms` fades, theme switching and the `themes/` directory. Until the grid exists, uwm's
   themes (which set `grid`) are rejected as a whole.
4. Input-method/text-input, output-management, clipboard bridging for Xwayland (popup grabs,
   xdg-activation, idle-inhibit/idle-notify, session-lock, pointer constraints and Xwayland are
   done).
5. Multi-GPU, animated cursors, DPMS.

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
(`$XDG_RUNTIME_DIR/uwc-<wayland display>.sock`). `uwc msg state`, `uwc msg -p get`, `uwc msg subscribe`,
`uwc msg set focus-color "#ff8800"`; `uwc msg --help` lists every command. The protocol is text lines in, one JSON
object per line out (`src/protocol.rs`), so `socat - UNIX-CONNECT:$UWC_SOCKET` works too.

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
src/protocol.rs    the IPC protocol, typed: Request (parsed from a line), the snapshot and events (serde)
src/ipc.rs         control socket: connections, subscriptions, running requests
src/layer.rs       wlr-layer-shell: hit-testing, keyboard interactivity, work area, drawing
src/rules.rs       window rules and the per-window decoration settings (`Deco`)
shaders/           example user shaders (border, background)
src/deco.rs        decoration shaders: rounded box/ring with gradients, shadow, rounded window content
src/toplevels.rs   wlr-foreign-toplevel-management (the window list for taskbars and shells)
src/capture.rs     drawing into a client's buffer (offscreen render, read back into shm)
src/export.rs      hyprland-toplevel-export-v1: window previews
src/protocols.rs   protocols no crate has, generated from protocols/*.xml
src/screencopy.rs  wlr-screencopy: output captures
src/workspace.rs   ext-workspace-v1 (hand-written: smithay has none)
src/scale.rs       sharp zoom: preferred render scale per window (fractional scale)
src/session.rs     session restore (serde, JSON in XDG_STATE_HOME)
src/sync.rs        explicit sync and dmabuf readiness (commits wait for the GPU)
src/cursor.rs      the pointer image (xcursor theme, client surfaces, fallback arrow)
src/backend/       udev (real session), winit (nested), headless (tests)
tests/headless.rs  end-to-end: spawn uwc --headless, connect real wayland clients, drive over IPC
```

### Window previews in a shell

uwc speaks the protocols Quickshell uses for live window thumbnails, so an overview, minimap or alt-tab
switcher can show real window contents (the canvas data itself is in `uwc msg state`):

```qml
import Quickshell.Wayland
Repeater {
    model: ToplevelManager.toplevels
    ScreencopyView { required property var modelData; captureSource: modelData; live: true; width: 240; height: 150 }
}
```

`cargo run --example capture -- --title TITLE out.ppm` grabs one window from the command line,
`--output out.ppm` or `--region X Y W H out.ppm` the screen (so does `grim`).

## Differences from uwm worth knowing

- Ricing goes past fixed effects: borders and the background can be your own GLSL.
- Windows can have their own scale on the canvas, on top of the view's zoom (no other compositor does this).
- Zoom is sharp: clients re-render at output scale x zoom with fractional scale (uwm stretches, or asks for an
  integer scale), and the renderer draws the bigger buffer at its logical size.

- Zoom is free: smithay draws a list of elements, so a window is a `RescaleRenderElement` around its surface tree.
  uwm needs `zoomtree()` to rewrite every scene node every frame.
- The pointer is kept in canvas coordinates as far as clients are concerned (the seat's location is canvas px, the
  focus origin is the surface's canvas position), so zoom needs no special casing in surface hit-testing.
- The `kdl` crate was tried and dropped: it rejects `vars { accent "#fff" }` (no `;` before the brace), which uwm
  configs use. `src/kdl.rs` accepts exactly what uwm's parser does.
- The environment variable names are `UWC_CONFIG` / `UWC_SOCKET`.

## License

MIT, see `LICENSE` once added (uwm's is the model).

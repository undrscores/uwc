# Hacking on uwc

Build, test and where things live. The conventions every change follows are in [CLAUDE.md](../CLAUDE.md).

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

## Layout

```
src/main.rs        CLI, event loop, signals, config watching
src/canvas.rs      Viewport maths (pure, unit-tested), tag constants
src/config.rs      config model, includes/vars, binds, validation
src/options.rs     every option once: name, field, range -> parsing, set, get
src/kdl.rs         the KDL subset parser (a port of uwm's kdl.h)
src/state.rs       Uwc state: windows, tags, viewport, focus, actions
src/handlers.rs    wayland protocol handlers
src/input.rs       keyboard/pointer/drag handling and touchpad gestures (`gestures { }` in the config), backend-independent
src/render.rs      the canvas as a list of render elements (zoom = RescaleRenderElement)
src/protocol.rs    the IPC protocol, typed: Request (parsed from a line), the snapshot and events (serde)
src/ipc.rs         control socket: connections, subscriptions, running requests
src/layer.rs       wlr-layer-shell: hit-testing, keyboard interactivity, work area, drawing
src/layout.rs      tiling layouts as pure maths (area + count + gaps -> rects; unit-tested)
src/tiling.rs      per-tag tiling: when windows are re-placed, floating, the `tile` actions
src/rules.rs       window rules and the per-window decoration settings (`Deco`)
shaders/           example user shaders (border, background)
src/deco.rs        decoration shaders: rounded box/ring with gradients, shadow, rounded window content
src/toplevels.rs   wlr-foreign-toplevel-management (the window list for taskbars and shells)
src/blur.rs        background blur: a pre-pass draws what is behind a window, dual Kawase filter, element under the window
src/presentation.rs  frame pacing: presentation-time, fifo, commit-timing
src/sandbox.rs     security-context: sandboxed clients and the globals hidden from them
src/capture.rs     drawing into a client's buffer (offscreen render, read back into shm)
src/export.rs      hyprland-toplevel-export-v1: window previews
src/protocols.rs   protocols no crate has, generated from protocols/*.xml
src/screencopy.rs  wlr-screencopy: output captures
src/workspace.rs   ext-workspace-v1 (hand-written: smithay has none)
src/scale.rs       sharp zoom: preferred render scale per window (fractional scale)
src/session.rs     session restore (serde, JSON in XDG_STATE_HOME)
src/sync.rs        explicit sync and dmabuf readiness (commits wait for the GPU)
src/cursor.rs      the pointer image (xcursor theme, animated cursors, client surfaces, fallback arrow)
src/output_management.rs  zwlr-output-management-v1: heads and apply
src/output_power.rs       zwlr-output-power-management-v1: DPMS blanking
src/extras.rs      small protocols: cursor-shape, data-control, virtual keyboard, shortcuts inhibit, xdg-foreign, ...
src/touch.rs       touchscreen input and its pan/pinch gesture maths
src/virtual_pointer.rs  wlr-virtual-pointer
src/ext_capture.rs ext-foreign-toplevel-list and ext-image-copy-capture (portal screen sharing)
src/xwayland.rs    Xwayland: X11 windows as clients, clipboard/primary bridging
src/backend/       udev (real session), winit (nested), headless (tests)
tests/headless.rs  end-to-end: spawn uwc --headless, connect real wayland clients, drive over IPC
```

## Window previews in a shell

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

## Differences from uwm

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

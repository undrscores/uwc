# uwc

A Wayland compositor with an infinite 2D canvas and real workspaces, in Rust on [smithay](https://github.com/Smithay/smithay).
The rewrite of [uwm](https://github.com/undrscores/uwm) (C, wlroots + scenefx): the same ideas, the same config
syntax and the same IPC, so a shell written for uwm (ushell) can follow uwc.

- Windows have positions on an unbounded canvas; the screen is a camera (viewport) with zoom.
- dwm-style tags (9), each with its own canvas, viewport and zoom.
- Everything floats. No tiling, no client-side decoration, no built-in bar.
- Looks and behaviour are configuration (`config.def.kdl`), not code.

## Status: a start

What works today (all of it covered by `cargo test`, and the render path checked by eye under Xvfb):

| Area | State |
|---|---|
| Canvas, viewport, zoom (about the pointer), pan, overview, fit/center | done |
| Tags: view / toggle / send-to, per-tag viewport, previous | done |
| xdg-shell toplevels and popups, xdg-decoration (server side), shm, seat, data device, output | done |
| Focus: sloppy focus, click to focus, directional and cyclic keys, pan-to-reveal for keyboard focus only | done |
| Mouse: Mod+drag move / resize / pan, empty-canvas drag pans, Mod+scroll zooms | done |
| Config: KDL (uwm's parser, ported), `include`, `vars`, validation as a whole, live reload | done |
| IPC: `state`, `subscribe`, `get`, `pan`, `panto`, `zoom`, `view`, `toggleview`, `focus`, `close`, `move`, `resize`, `overview`, `home`, `reload`, `quit` | done |
| Backends: `winit` (nested, for development) and `headless` (tests) | done |
| Borders (solid, focus color) | done |

Not yet (roughly in the order to do them):

1. **DRM/libinput/libseat backend**, so it can run as a session and not only nested. The cursor image must be drawn then.
2. **`wlr-layer-shell`** (bars, panels, wallpaper programs) and `ext-workspace-v1`: needed for Quickshell/ushell.
3. Multiple monitors (state has one `Monitor` today), pinned windows, minimize/scratchpad, fullscreen.
4. The rest of uwm's IPC (`set`, `theme`, `rule`, `bookmark`, `swipe`, `pinch`, `palette`, ...) and window rules.
5. Animations (open/close/move/tag-switch, eased pan, momentum), driven from the frame callback, never timers.
6. Decoration: rounded corners, shadows, blur, gradients, dim. scenefx has no Rust equivalent, so this means custom GLES
   shaders on smithay's `GlesRenderer` (`Frame::render_texture_from_to` with a texture program, `compile_custom_*`).
7. Wallpaper, grid, per-tag looks, theme switching.
8. Popup grabs (clicking outside a menu closes it), xdg-activation, idle-inhibit, pointer constraints, gestures,
   Xwayland, input-method/text-input, screencopy.

## Build and run

Needs a Rust toolchain (edition 2021) and the system libraries smithay links: `libwayland`, `libxkbcommon`,
`libinput`, `libudev`, `libgbm`, `libseat`, `libegl`/`libgles`, `libdrm`. On Debian/Ubuntu:
`apt install libwayland-dev libxkbcommon-dev libxkbcommon-x11-0 libinput-dev libudev-dev libgbm-dev libseat-dev libegl-dev libgles-dev libdrm-dev`.

```sh
cargo build
cargo run -- -s foot                 # nested in your current session; -s runs a command once it is up
cargo run -- --headless 1280x720     # no display, for tests
cargo run -- -t                      # validate the config and exit
cargo test                           # unit tests + end-to-end tests against a headless uwc
cargo run --example paint -- "red" 400 300 d03030    # a test client that draws a solid window
```

Config: `-c FILE`, else `$UWC_CONFIG`, else `~/.config/uwc/config.kdl`. `config.def.kdl` is the built-in default and the
documentation; a file only needs what it changes, and it is reloaded when saved. Control socket: `$UWC_SOCKET`
(`$XDG_RUNTIME_DIR/uwc-<wayland display>.sock`); `socat - UNIX-CONNECT:$UWC_SOCKET` then type `state`.

## Layout

```
src/main.rs        CLI, event loop, signals, config watching
src/canvas.rs      Viewport maths (pure, unit-tested), tag constants
src/config.rs      config model, includes/vars, binds, validation
src/kdl.rs         the KDL subset parser (a port of uwm's kdl.h)
src/state.rs       Uwc state: windows, tags, viewport, focus, actions
src/handlers.rs    wayland protocol handlers
src/input.rs       keyboard/pointer/drag handling, backend-independent
src/render.rs      the canvas as a list of render elements (zoom = RescaleRenderElement)
src/ipc.rs         control socket
src/backend/       winit (nested), headless (tests)
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

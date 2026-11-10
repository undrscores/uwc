# CLAUDE.md

## What this is

uwc: the Rust/smithay rewrite of uwm (`undrscores/uwm`, C and wlroots). An infinite 2D canvas with dwm-style tags, each
with its own canvas, viewport and zoom; everything floats; meant to be ricked out through
configuration. Keep the **config syntax, action names and IPC schema compatible with uwm** unless there is a
reason not to; `README.md` has the status table and the roadmap. Not goals: client-side decorations, a built-in bar.

## Build and test

- `cargo build` must stay warning-free (`cargo clippy --all-targets` too). `cargo fmt` (rustfmt.toml: width 120).
- `cargo test` runs the unit tests (canvas maths, KDL parser, config) and `tests/headless.rs`: each test spawns
  `uwc --headless` in its own `XDG_RUNTIME_DIR`, connects real Wayland clients (wayland-client) and reads state
  through the IPC socket. Add a check when fixing a bug. Never `pkill` by name; kill test processes by PID.
- `tests/nested.py` checks drawing and real pointer input: uwc nested in Xvfb with software GL, `examples/paint`
  clients, `xdotool` moving the X pointer, pixels read from `import -window root` screenshots, focus read over IPC
  (needs Xvfb, xdotool, ImageMagick, python3-pil). Run it after touching rendering, stacking or hit-testing, and
  extend it rather than looking at screenshots by hand. Sample pixels away from paint's 10 px lighter frame.
- Windows have a placement (`Uwc::place`): canvas windows follow the viewport and zoom; pinned and fullscreen
  windows are fixed to the screen at zoom 1. Drawing, hit-testing (`window_under`), the pointer location given to
  the seat and drags all go through it; never compute a window's screen position from the viewport directly.
- System libs: see README. The sandbox needs `libxkbcommon-x11-0` for winit's X11 path.
- The udev backend cannot run in a sandbox (no GPU, no seat); it only builds there. Say so when changing it, and keep
  backend-independent logic (input, cursor, rendering, frame callbacks) out of it so the nested and headless runs
  exercise it.
- `Uwc.backend` is taken out with `std::mem::take` when its renderer and the state are needed together. Check the
  variant *before* taking it (`matches!`), or a mismatch silently leaves `Backend::None` behind; `tests/headless.rs`
  checks `"backend"` in the IPC state for this.

## Conventions

- One crate, small modules (see the layout in README). `Uwc` (state.rs) owns everything; protocol handlers, input and
  IPC are `impl Uwc` blocks in their own files.
- Canvas px are what clients are told and IPC reports (`Client.canvas`). Screen px = `Viewport::canvas_to_screen`.
  The seat's pointer location is in the space of what is under it (canvas px for canvas windows, screen px for
  pinned/fullscreen windows and layer surfaces); `Uwc.cursor` is screen px.
- Panning only changes the viewport; it never reconfigures clients and never changes keyboard focus. Keyboard/IPC focus
  changes reveal (`reveal`/`focus_and_reveal`); mouse clicks and sloppy focus do not.
- Anything that needs a new frame calls `damage()` (sets `redraw`, marks IPC dirty). IPC subscribers get one snapshot
  per event-loop turn (`ipc_flush`).
- Adding an option: a field in `Config`, an arm in `set_option` (with a range if a bad value could break something),
  a line in `config.def.kdl` (documented), an entry in `ipc_options`, and a test. Adding an action: `Action`,
  `parse_action`, `run_action`, a default bind if sensible.
- Animations are stepped from the frame handler, never timers: set `animating` while drawing a frame that needs a
  next one (the border spin does), and the backend asks for it.
- Riceable is a goal: new visual features are an option in `config.def.kdl` (documented there, uwm's name if uwm has
  it), settable with IPC `set`, reported by `get`, and tested. Decoration reads `Uwc.config` when drawing, so `set`
  needs nothing else. Shader elements (`deco.rs`) keep a stable `Id` per window and role and take `deco_commit`;
  bump it when their look changes without their geometry changing.
- GLSL must compile on NVIDIA too: GLSL ES 1.00, float literals (`1.0`, not `1`), no implicit int/float mixing, no
  names that shadow builtins, constant loop bounds, `highp` only under `GL_FRAGMENT_PRECISION_HIGH`.

## Workflow

- Develop on the branch the task names; commit when it builds warning-free and tests pass. Commit message: short
  imperative subject, body says what was and was not tested.
- Git identity is not configured: `-c user.name=undrscores -c user.email=264885796+undrscores@users.noreply.github.com`.
- Force pushes, other repos and releases need a confirmation first.

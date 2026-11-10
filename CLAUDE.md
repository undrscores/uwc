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
- To see rendering: `Xvfb :77 &`, then `DISPLAY=:77 WINIT_UNIX_BACKEND=x11 LIBGL_ALWAYS_SOFTWARE=1 target/debug/uwc`,
  run `cargo run --example paint` against its `WAYLAND_DISPLAY`, and screenshot with `import -window root`.
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
  The seat's pointer location is in **canvas** px; `Uwc.cursor` is screen px.
- Panning only changes the viewport; it never reconfigures clients and never changes keyboard focus. Keyboard/IPC focus
  changes reveal (`reveal`/`focus_and_reveal`); mouse clicks and sloppy focus do not.
- Anything that needs a new frame calls `damage()` (sets `redraw`, marks IPC dirty). IPC subscribers get one snapshot
  per event-loop turn (`ipc_flush`).
- Adding an option: a field in `Config`, an arm in `set_option` (with a range if a bad value could break something),
  a line in `config.def.kdl` (documented), an entry in `ipc_options`, and a test. Adding an action: `Action`,
  `parse_action`, `run_action`, a default bind if sensible.
- Animations (when added) are stepped from the frame handler, never timers.

## Workflow

- Develop on the branch the task names; commit when it builds warning-free and tests pass. Commit message: short
  imperative subject, body says what was and was not tested.
- Git identity is not configured: `-c user.name=undrscores -c user.email=264885796+undrscores@users.noreply.github.com`.
- Force pushes, other repos and releases need a confirmation first.

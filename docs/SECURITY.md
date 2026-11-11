# Security model

uwc is a single-user desktop compositor. This page says what it trusts, what it defends against, and what it
does not. Report problems privately to the repository owner rather than in a public issue.

## Trust boundaries

- **Wayland clients are not sandboxed from each other.** Any program that can connect to the Wayland socket (it lives
  in your private `$XDG_RUNTIME_DIR`) can use every protocol uwc offers, including the privileged ones: screen and
  window capture (wlr-screencopy, ext-image-copy-capture, hyprland-toplevel-export), reading and setting the
  clipboard (data-control), typing and clicking for you (virtual keyboard and pointer), becoming the input method,
  locking the session (ext-session-lock), changing outputs (output-management, power management), and taking
  the keyboard with a layer surface or a shortcuts inhibitor. This matches the other wlroots-style compositors. A
  Flatpak or other sandbox must not be handed the real socket; uwc does not yet implement security-context to give
  sandboxed apps a restricted one. Treat "can reach the socket" as "is you".
- **The control socket** (`$XDG_RUNTIME_DIR/uwc-*.sock`, mode 0600 in a private directory) can move, resize and
  close windows and change options. It cannot run programs: hooks and key binds come from the config file only.
  Anything that can write your config can run commands as you, as with any compositor.
- **Xwayland** is started through smithay with no extra authority setup, so the X server's own defaults apply (I have
  not verified what they allow other local users). X11 clients can read each other's windows and keystrokes, as
  always under X. Turn it off (`xwayland false`) if you do not need X11 apps.
- **Window titles and app-ids belong to the apps.** They reach scripts through the IPC and the hook environment.
  Quote them (`"$UWC_TITLE"`), never `eval` them. `uwc msg` refuses newlines in its arguments so a title cannot
  smuggle in a second command.

## What uwc defends against

- A client that sends hostile numbers (huge sizes, margins, offsets, strides) must not crash or corrupt uwc.
  Layer surface numbers are clamped; capture buffers are checked with overflow-safe arithmetic; X11 requests are
  clamped; IPC numbers are bounded; IPC lines are capped; configs nest at most 32 blocks deep. `tests/headless.rs`
  runs hostile clients and IPC input, and every test fails if uwc logged a panic.
- A lock screen cannot trap you: VT switching (Ctrl+Alt+F1..F12) works while the session is locked and while a
  client holds the shortcuts; every other bind is off.
- Files that name your windows (the session file, the log written by `uwc-session`) are created for you alone.

## Known limits

- A panic inside a Wayland protocol handler is caught like any other (see docs/STATUS.md); three in ten seconds end the session, with the session file saved.
- Dependencies are not audited here: run `cargo audit` in CI or before a release.
- The udev backend, NVIDIA and real input devices are untested on hardware.

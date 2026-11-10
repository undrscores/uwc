# Status

What works, what is untested, and what is next. Back to the [README](../README.md).

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
| Xwayland (`xwayland true`, needs the Xwayland binary; real sessions only): X11 windows become clients like any other -- pinned to the monitor they asked for, with uwc's decoration, focus, move / resize / fullscreen and the IPC; override-redirect windows (X menus, tooltips) are drawn on top and take the pointer by their own geometry; the clipboard and primary selection are bridged both ways (an X selection is re-published to wayland clients, a wayland one is offered to X clients) | done |
| IME: zwp-text-input-v3 (clients) + zwp-input-method-v2 (fcitx5 and friends): text input enters and leaves with the keyboard focus; while an input method holds the keyboard grab every key goes to it first and it answers through text input (uwc's own key bindings do not fire then); its popups (candidate lists) are drawn on top, under the focused window's corner, and take the pointer | done |
| Gestures: a three-finger touchpad swipe pans the view and a three-finger pinch zooms it; gestures with fewer fingers (a browser's two-finger pinch) are forwarded to the client under the pointer (zwp-pointer-gestures-v1) | done |
| Multiple monitors: all outputs tiled left to right, one shared canvas and tagset, a viewport and bookmarks per monitor per tag; the pointer's monitor is where view actions, new windows and pins land; a window is drawn on every monitor whose view shows it, fullscreen covers the monitor it is on | done |
| zwlr-output-management-v1 (wdisplays, wlr-randr, Quickshell's OutputManager): heads with modes, enabled, transform, scale; an apply reconfigures the outputs (positions are accepted but ignored -- uwc tiles) and rescans the connectors; a headless or winit session can only change scale and transform | done |
| zwlr-output-power-management-v1 (swayidle's DPMS): `on`/`off` per output; an output that is off draws nothing (blank), so idle blanking costs no redraws | done |
| Focus: sloppy focus, click to focus, directional and cyclic keys, pan-to-reveal for keyboard focus only | done |
| Mouse: Mod+drag move / resize / pan, empty-canvas drag pans, Mod+scroll zooms | done |
| Pointer drawn by uwc: xcursor theme (`cursor-theme`, `cursor-size`), client cursor surfaces, built-in fallback arrow; animated cursors cycle their frames (redraws keep coming while one is shown) | done |
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
| Everyday protocols: cursor-shape-v1 (GTK/Qt cursors), data-control (wlr and ext: wl-clipboard, clipboard managers), virtual-keyboard-v1 (wtype, on-screen keyboards), wlr-virtual-pointer (wayvnc, wlrctl, input sharing), single-pixel-buffer, xdg-foreign, xdg-dialog, content-type, xdg-system-bell | done |
| keyboard-shortcuts-inhibit (VMs, remote desktop): an inhibitor on the focused surface takes every key; `Mod+Shift+Escape` (`toggle-shortcuts-inhibit`) takes them back and is the one bind that still runs | done |
| Dialogs: a toplevel with an xdg parent (or X11 transient-for) opens centred over its parent, on its tags and scale, pinned where the parent is pinned, instead of somewhere in the middle of the view | done |
| Touchscreens: a touch on a window or layer surface goes to its client (and focuses it like a click); on empty canvas one finger drags the view, two pan and pinch-zoom it. Graphics tablets act as an absolute mouse (pen = pointer, tip = left button; no pressure or tilt yet) | done, untested on hardware |
| Jump to a window: `jump-hints` (Mod+Slash) zooms out, labels every window from the home row (`hint-keys`, `hint-scale`; pixel-font badges, no font stack) and glides to the one you type at the zoom you came from; IPC `jump QUERY` fuzzy-matches app-id and title on any tag (`#ID` for an id), restoring minimized windows; `examples/uwc-jump` feeds it from fuzzel, wofi, rofi or bemenu | done |
| `uwc doctor`: checks the runtime dir, seat access (logind, elogind or seatd), GPUs and drivers, the NVIDIA modesetting and driver version, input devices, the libraries uwc loads, Xwayland, portals and that the config's spawn binds exist; prints what to fix; exit status 1 on a failure | done |
| Panic recovery: a panic in any handler is logged with its backtrace, the half-done state (drags, gestures, hint mode, touches) is dropped and the session carries on; the session file is saved at once, and three panics in ten seconds (or one while drawing) save it and exit | done |
| Frames: labelled regions of a tag's canvas drawn behind the windows (tinted fill, border, title strip, pixel-font label; `frame-color`, `frame-opacity`, `frame-label-scale`, `frame-border-width`). Drag the title strip to move a frame with the windows whose centre is inside it, the bottom-right corner to resize; `frame-new` (Mod+B) around the focused window, `frame-remove`, `frame-fit`, `frame-next`/`frame-previous` fit the view to a frame; IPC `frame add\|label\|move\|resize\|remove\|fit`, listed with their windows in the snapshot; saved with the session | done |
| Snapping: dragging a window snaps its edges and centre lines to other windows and frames within `snap-distance` screen px (Ctrl drags freely), optionally to a grid (`snap-grid`, canvas px), with guide lines in `snap-color` | done |
| Event hooks: `hooks { window-open, window-close, focus, tag-change, urgent }` run a command through `sh -c` with `UWC_EVENT`, `UWC_WINDOW_ID`, `UWC_APP_ID`, `UWC_TITLE`, `UWC_TAGS` and `UWC_TAGSET` in its environment; unknown hook names are config errors | done |
| Per-tag looks: `tag N { root-color "#..."; background-shader "file.glsl" }` gives a tag its own colour behind the canvas and its own background shader (watched for changes like the others) | done |
| Hot zones: `hot-zones { top-left { action; } ... left { } }` runs any bind action when the pointer rests (`hot-zone-delay-ms`) in a screen corner or edge (`hot-zone-size` deep, once per visit, not during a drag or lock) | done |
| Themes: `themes/` ships Tokyo Night, Gruvbox dark, Catppuccin Mocha and Nord as ordinary config files (colours, focus gradient, frame and snap colours); `include` one before your own options (installed under `share/uwc/themes`) | done |
| Keyboard layout: the snapshot reports the active layout (`keyboard_layout`: index, name, count) for bars; `next-layout` (Mod+Space) and IPC `layout next\|N` switch it, XKB layout-toggle options are noticed too; `per-window-layout` remembers one per window | done |
| Edge pan: dragging a window against a screen edge pans the view that way with the window under the pointer (`edge-pan-size`, `edge-pan-speed`) | done |
| Momentum: let go of a three-finger swipe or a touch drag while moving and the view glides on (`pan-momentum-ms`, IPC `fling VX VY`) | done |
| Screen capture for portals: ext-foreign-toplevel-list, ext-image-capture-source, ext-image-copy-capture (outputs and windows, shm, `paint_cursors`); with wlr-screencopy this is what xdg-desktop-portal-wlr, OBS and browsers' screen sharing use. `uwc-portals.conf` (installed by `make install`) picks the wlr portal for ScreenCast and Screenshot | done, no dmabuf yet |

**The udev backend has not been run on hardware yet.** It builds against smithay 0.7's real DRM/GBM/libinput/libseat
types and was reviewed against smithay's source, but the machine it was written on has no GPU or seat. Expect the first
run on a real TTY to turn up something. What it does: libseat session (VT switching with Ctrl+Alt+F1..F12, pause and
resume), the primary GPU via udev, KMS through smithay's `DrmOutputManager` (hardware cursor plane when available),
connector hotplug, libinput with the config's touchpad/mouse settings, linux-dmabuf with feedback for GL clients.
Limits: one GPU. Multiple outputs are scanned out per connector (each gets its own view of the shared
canvas), but the machine this was written on has no GPU or seat, so the udev path is untested on hardware.

## Roadmap

Not yet (roughly in the order to do them):

0. **Run it on real hardware**: the udev backend, NVIDIA, a portal for screen sharing, touch and tablet devices.
   Everything above is tested against headless and nested runs only.
1. **A dmabuf path for screen capture** (ext-image-copy-capture offers shm only). It needs the renderer's DRM node and
   a GPU to test, so it waits for item 0.
2. **tablet-v2** (pressure and tilt reach clients; pens act as a mouse today). Needs a tablet to test.
3. **Blur** (and ext-background-effect for bars), the headline missing visual effect: it needs a render pass that
   captures what is behind a window, blurs it and draws it under the window. The canvas grid is covered by `snap-grid`
   and the example shaders; uwm's themes (which set `grid`) are still rejected as a whole.
4. Protocols still missing: presentation-time, fifo/commit-timing (a client waiting on a fifo barrier stalls unless it
   is signalled every frame, so it has to be done properly), security-context (Flatpak; only worth it together with
   restricting privileged globals), alpha-modifier, xdg-toplevel-icon.
5. A minimap or overview strip for Quickshell (the window previews it needs already work), and `focus-ms` fades.
6. Multi-GPU.

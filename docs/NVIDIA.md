# NVIDIA

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

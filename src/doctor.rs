//! `uwc doctor`: checks what a real session needs (a seat, a GPU, the NVIDIA settings, input
//! devices, the libraries and helpers uwc loads or starts) and says what to do about each
//! problem. It changes nothing; run it from a TTY (or over ssh, knowing the seat checks then say
//! what that session lacks). Exit status 1 if anything failed, so it can gate a bug report.

use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    pub what: String,
    /// What to do, for anything that is not Ok.
    pub fix: Option<String>,
}

fn check(level: Level, what: impl Into<String>, fix: Option<&str>) -> Check {
    Check { level, what: what.into(), fix: fix.map(str::to_string) }
}

/// The NVIDIA driver's major version from /sys/module/nvidia/version ("560.35.03").
pub fn nvidia_major(version: &str) -> Option<u32> {
    version.trim().split('.').next()?.parse().ok()
}

/// The program a spawn bind starts (its first word), for the "is it installed" check.
pub fn first_word(cmd: &str) -> Option<&str> {
    cmd.split_whitespace().next()
}

/// Whether an executable called `name` is in `$PATH` (or the extra directories).
pub fn in_path(name: &str, extra: &[&str]) -> bool {
    if name.contains('/') {
        return Path::new(name).is_file();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).chain(extra.iter().map(PathBuf::from)).any(|d| d.join(name).is_file())
}

/// The kernel driver of a DRM card: the basename of its `device/driver` symlink.
fn driver_of(card: &Path) -> Option<String> {
    fs::read_link(card.join("device/driver")).ok()?.file_name().map(|n| n.to_string_lossy().into_owned())
}

fn session_checks(out: &mut Vec<Check>) {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(d) if d.is_dir() => {
            let probe = d.join(format!(".uwc-doctor-{}", std::process::id()));
            if fs::write(&probe, b"").is_ok() {
                let _ = fs::remove_file(&probe);
                out.push(check(Level::Ok, format!("runtime dir {} is writable", d.display()), None));
            } else {
                out.push(check(
                    Level::Fail,
                    format!("runtime dir {} is not writable", d.display()),
                    Some("the Wayland and control sockets live there; log in through logind or set XDG_RUNTIME_DIR to a directory you own with mode 0700"),
                ));
            }
        }
        _ => out.push(check(
            Level::Fail,
            "XDG_RUNTIME_DIR is not set",
            Some("log in through logind (a display manager or a TTY login), or create /run/user/$UID and export XDG_RUNTIME_DIR"),
        )),
    }
    let nested = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    if nested {
        out.push(check(
            Level::Info,
            "running inside another Wayland or X11 session: uwc starts as a window here (nested)",
            Some("for a real session, switch to a free TTY (Ctrl+Alt+F3), log in and run uwc-session"),
        ));
    }

    // Seat access: logind hands it to a session, seatd to a member of its group.
    let logind = std::env::var_os("XDG_SESSION_ID").is_some() && Path::new("/run/systemd/seats").exists()
        || Path::new("/run/elogind").exists() && std::env::var_os("XDG_SESSION_ID").is_some();
    let seatd_sock = std::env::var_os("SEATD_SOCK").map(PathBuf::from).unwrap_or_else(|| "/run/seatd.sock".into());
    let seatd = seatd_sock.exists() && fs::OpenOptions::new().read(true).write(true).open(&seatd_sock).is_ok();
    if logind {
        out.push(check(Level::Ok, "seat access: a logind/elogind session (XDG_SESSION_ID is set)", None));
    } else if seatd {
        out.push(check(Level::Ok, format!("seat access: seatd ({} is accessible)", seatd_sock.display()), None));
    } else if seatd_sock.exists() {
        out.push(check(
            Level::Fail,
            format!("seat access: {} exists but you cannot open it", seatd_sock.display()),
            Some("add your user to the seat group (often `seat` or `_seatd`) and log in again"),
        ));
    } else {
        out.push(check(
            Level::Fail,
            "seat access: no logind session and no seatd socket",
            Some("log in on a TTY or through a display manager so logind starts a session, or run seatd and join its group; over ssh there is no seat"),
        ));
    }
    if let Ok(vt) = std::env::var("XDG_VTNR") {
        out.push(check(Level::Info, format!("on VT {vt}"), None));
    }
}

fn gpu_checks(out: &mut Vec<Check>) {
    let mut cards: Vec<PathBuf> = fs::read_dir("/sys/class/drm")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().is_some_and(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with("card") && !n.contains('-')
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    cards.sort();
    if cards.is_empty() {
        out.push(check(
            Level::Fail,
            "no DRM devices (/sys/class/drm has no card*)",
            Some("a GPU driver has to be loaded: amdgpu, i915, nouveau or nvidia (with nvidia_drm); in a VM use virtio-gpu"),
        ));
        return;
    }
    for c in &cards {
        let name = c.file_name().unwrap().to_string_lossy().into_owned();
        let driver = driver_of(c).unwrap_or_else(|| "unknown driver".into());
        out.push(check(Level::Ok, format!("GPU {name}: {driver}"), None));
        let node = Path::new("/dev/dri").join(&name);
        if !node.exists() {
            out.push(check(
                Level::Warn,
                format!("/dev/dri/{name} does not exist"),
                Some("the driver is bound but udev has not made the device node; check dmesg"),
            ));
        }
    }
    let render = fs::read_dir("/dev/dri")
        .map(|d| d.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with("renderD")))
        .unwrap_or(false);
    if !render {
        out.push(check(
            Level::Warn,
            "no render node (/dev/dri/renderD*)",
            Some("clients cannot use the GPU for rendering without one; the driver may be old or software-only"),
        ));
    }
    nvidia_checks(out, cards.iter().any(|c| driver_of(c).as_deref() == Some("nvidia")));
}

fn nvidia_checks(out: &mut Vec<Check>, in_use: bool) {
    let version = fs::read_to_string("/sys/module/nvidia/version").ok();
    if !in_use && version.is_none() {
        return;
    }
    let modeset = fs::read_to_string("/sys/module/nvidia_drm/parameters/modeset").unwrap_or_default();
    if modeset.trim() == "Y" {
        out.push(check(Level::Ok, "NVIDIA: nvidia_drm.modeset=1", None));
    } else {
        out.push(check(
            Level::Fail,
            "NVIDIA: kernel modesetting is off (nvidia_drm.modeset is not Y)",
            Some("add nvidia_drm.modeset=1 (and nvidia_drm.fbdev=1) to the kernel command line, or `options nvidia_drm modeset=1 fbdev=1` in /etc/modprobe.d/ and rebuild the initramfs; uwc refuses to start without it"),
        ));
    }
    match version.as_deref().and_then(nvidia_major) {
        Some(v) if v >= 560 => {
            out.push(check(Level::Ok, format!("NVIDIA driver {v}: explicit sync is available"), None))
        }
        Some(v) => out.push(check(
            Level::Warn,
            format!("NVIDIA driver {v}: older than 560, so no explicit sync"),
            Some("GL and Vulkan apps (browsers, games) can flicker or show old frames; update the driver"),
        )),
        None => {}
    }
}

fn input_checks(out: &mut Vec<Check>) {
    let n = fs::read_dir("/dev/input")
        .map(|d| d.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with("event")).count())
        .unwrap_or(0);
    if n == 0 {
        out.push(check(
            Level::Warn,
            "no input devices (/dev/input/event*)",
            Some("in a VM or container, pass keyboard and mouse through; on hardware check the evdev module"),
        ));
    } else {
        out.push(check(Level::Ok, format!("{n} input devices"), None));
    }
}

fn library_checks(out: &mut Vec<Check>) {
    // `ldconfig -p` knows the libraries uwc opens or links; absent on NixOS, where the package
    // carries its own paths.
    let Ok(o) = std::process::Command::new("ldconfig").arg("-p").output() else {
        out.push(check(Level::Info, "libraries not checked (no ldconfig; fine on NixOS)", None));
        return;
    };
    let listing = String::from_utf8_lossy(&o.stdout);
    let mut missing = false;
    for (lib, pkg) in [
        ("libEGL.so.1", "libglvnd / Mesa (libegl1)"),
        ("libGLESv2.so.2", "libglvnd / Mesa (libgles2)"),
        ("libgbm.so.1", "Mesa gbm (libgbm1)"),
        ("libxkbcommon.so.0", "libxkbcommon"),
        ("libinput.so.10", "libinput"),
        ("libseat.so.1", "libseat (seatd)"),
        ("libdrm.so.2", "libdrm"),
    ] {
        if !listing.contains(lib) {
            missing = true;
            out.push(check(Level::Fail, format!("{lib} not found"), Some(&format!("install {pkg}"))));
        }
    }
    if !missing {
        out.push(check(Level::Ok, "EGL, GLES, gbm, xkbcommon, libinput, libseat and libdrm are installed", None));
    }
}

fn helper_checks(out: &mut Vec<Check>, cfg: Option<&crate::config::Config>) {
    if cfg.is_none_or(|c| c.xwayland) {
        if in_path("Xwayland", &["/usr/libexec"]) {
            out.push(check(Level::Ok, "Xwayland found (X11 apps work)", None));
        } else {
            out.push(check(
                Level::Warn,
                "Xwayland not found, and the config has `xwayland true`",
                Some("install Xwayland (xorg-xwayland, xwayland), or set `xwayland false` in the options"),
            ));
        }
    }
    let portals = ["/usr/libexec", "/usr/lib/xdg-desktop-portal-wlr", "/usr/lib"];
    if in_path("xdg-desktop-portal-wlr", &portals) || in_path("xdg-desktop-portal", &portals) {
        out.push(check(Level::Ok, "xdg-desktop-portal found (screen sharing)", None));
    } else {
        out.push(check(
            Level::Warn,
            "xdg-desktop-portal not found",
            Some("install xdg-desktop-portal, xdg-desktop-portal-wlr and xdg-desktop-portal-gtk for screen sharing in browsers and OBS"),
        ));
    }
    // The first word of every spawn bind should be something you have.
    if let Some(cfg) = cfg {
        let mut missing: Vec<String> = Vec::new();
        for b in &cfg.keys {
            if let crate::config::Action::Spawn(argv) = &b.action {
                if let Some(p) = argv.first().and_then(|a| first_word(a)) {
                    if !in_path(p, &[]) && !missing.iter().any(|m| m == p) {
                        missing.push(p.to_string());
                    }
                }
            }
        }
        for m in missing {
            out.push(check(
                Level::Warn,
                format!("a key bind starts `{m}`, which is not installed"),
                Some("install it or change the bind (the default terminal is foot)"),
            ));
        }
    }
}

/// Every check, in the order they are printed.
pub fn run_checks(config_path: Option<&Path>) -> Vec<Check> {
    let mut out = Vec::new();
    session_checks(&mut out);
    gpu_checks(&mut out);
    input_checks(&mut out);
    library_checks(&mut out);
    let cfg = match crate::config::Config::load(config_path) {
        Ok(c) => {
            let from = match config_path {
                Some(p) if p.exists() => p.display().to_string(),
                _ => "(none: the built-in default is used)".into(),
            };
            out.push(check(Level::Ok, format!("config {from}"), None));
            Some(c)
        }
        Err(e) => {
            out.push(check(Level::Fail, format!("config: {e:#}"), Some("fix the file, then check it with `uwc -t`")));
            None
        }
    };
    helper_checks(&mut out, cfg.as_ref());
    out
}

pub fn render(checks: &[Check]) -> String {
    let mut s = String::from("uwc doctor\n");
    for c in checks {
        let tag = match c.level {
            Level::Ok => "ok  ",
            Level::Info => "info",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        };
        s.push_str(&format!("  {tag}  {}\n", c.what));
        if let Some(f) = &c.fix {
            s.push_str(&format!("        -> {f}\n"));
        }
    }
    let (fails, warns) = (
        checks.iter().filter(|c| c.level == Level::Fail).count(),
        checks.iter().filter(|c| c.level == Level::Warn).count(),
    );
    s.push_str(&format!("{fails} failed, {warns} warnings\n"));
    s
}

/// `uwc doctor [-c FILE]`: prints the report; true if nothing failed.
pub fn main(args: &[String]) -> bool {
    let mut file = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-c" | "--config" => file = it.next().cloned(),
            _ => {
                eprintln!("usage: uwc doctor [-c FILE]");
                return false;
            }
        }
    }
    let path = crate::config::config_path(file.as_deref());
    let checks = run_checks(path.as_deref());
    print!("{}", render(&checks));
    !checks.iter().any(|c| c.level == Level::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_versions() {
        assert_eq!(nvidia_major("560.35.03\n"), Some(560));
        assert_eq!(nvidia_major("470.256.02"), Some(470));
        assert_eq!(nvidia_major("garbage"), None);
    }

    #[test]
    fn finds_programs_in_the_path() {
        assert!(in_path("sh", &[]));
        assert!(!in_path("definitely-not-a-program-uwc", &[]));
        assert!(in_path("/bin/sh", &[]));
        assert_eq!(first_word("foot -e fish"), Some("foot"));
    }

    #[test]
    fn the_report_counts_and_explains() {
        let checks = vec![
            check(Level::Ok, "fine", None),
            check(Level::Warn, "meh", Some("do this")),
            check(Level::Fail, "broken", Some("fix that")),
        ];
        let r = render(&checks);
        assert!(r.contains("ok    fine"));
        assert!(r.contains("WARN  meh"));
        assert!(r.contains("-> fix that"));
        assert!(r.ends_with("1 failed, 1 warnings\n"));
    }
}

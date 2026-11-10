//! uwc: a Wayland compositor with an infinite 2D canvas and dwm-style tags, in Rust on smithay.
//! The successor to uwm (the same ideas, the same config and IPC), see README.md.

mod backend;
mod canvas;
mod config;
mod cursor;
mod deco;
mod handlers;
mod input;
mod ipc;
mod kdl;
mod layer;
mod render;
mod rules;
mod state;
mod sync;
mod workspace;

use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use calloop::signals::{Signal, Signals};
use smithay::reexports::{
    calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    wayland_server::Display,
};

use crate::{config::Config, state::Uwc};

const USAGE: &str = "usage: uwc [-c FILE] [-s CMD] [-t] [--backend udev|winit] [--headless [WxH]]
  -c, --config FILE   config file (default: $UWC_CONFIG, else ~/.config/uwc/config.kdl)
  -s CMD              run CMD (through sh) once the compositor is up
  -t, --check-config  validate the config and exit
      --backend NAME  udev: a real session (run from a TTY); winit: a window in the current session.
                      Default: winit if WAYLAND_DISPLAY or DISPLAY is set, else udev
      --headless [WxH]  no display: a virtual output of WxH (default 1280x720), for tests
  -h, --help";

#[derive(Debug, Clone, Copy, PartialEq)]
enum BackendKind {
    Udev,
    Winit,
    Headless(i32, i32),
}

struct Args {
    config: Option<String>,
    startup: Option<String>,
    check: bool,
    backend: Option<BackendKind>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args { config: None, startup: None, check: false, backend: None };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-c" | "--config" => a.config = Some(it.next().context("-c needs a file")?),
            "-s" => a.startup = Some(it.next().context("-s needs a command")?),
            "-t" | "--check-config" => a.check = true,
            "--headless" => {
                let size = match it.peek().filter(|n| !n.starts_with('-')) {
                    Some(s) => {
                        let (w, h) = s.split_once('x').context("--headless size must look like 1280x720")?;
                        let size = (w.parse()?, h.parse()?);
                        it.next();
                        size
                    }
                    None => (1280, 720),
                };
                if size.0 < 1 || size.1 < 1 || size.0 > 16384 || size.1 > 16384 {
                    bail!("--headless size out of range");
                }
                a.backend = Some(BackendKind::Headless(size.0, size.1));
            }
            "--backend" => {
                a.backend = Some(match it.next().as_deref() {
                    Some("udev") => BackendKind::Udev,
                    Some("winit") => BackendKind::Winit,
                    Some("headless") => BackendKind::Headless(1280, 720),
                    _ => bail!("--backend needs udev, winit or headless"),
                })
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument `{other}`\n{USAGE}"),
        }
    }
    Ok(a)
}

fn main() {
    if let Err(e) = run() {
        // One readable line: this is what someone starting a session from a TTY sees.
        eprintln!("uwc: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = parse_args()?;
    let path = config::config_path(args.config.as_deref());

    if args.check {
        match Config::load(path.as_deref()) {
            Ok(_) => {
                println!("config ok{}", path.as_ref().map(|p| format!(": {}", p.display())).unwrap_or_default());
                return Ok(());
            }
            Err(e) => {
                eprintln!("config error: {e:#}");
                std::process::exit(1);
            }
        }
    }

    let cfg = Config::load(path.as_deref()).context("config")?;
    let mut event_loop: EventLoop<Uwc> = EventLoop::try_new()?;
    let display: Display<Uwc> = Display::new()?;
    let mut state = Uwc::new(display, event_loop.handle(), event_loop.get_signal(), cfg, path)?;
    let handle = event_loop.handle();

    state.apply_input_config();

    let nested = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let kind = args.backend.unwrap_or(if nested { BackendKind::Winit } else { BackendKind::Udev });
    match kind {
        BackendKind::Headless(w, h) => backend::headless::init(&mut state, &handle, (w, h))?,
        BackendKind::Winit => backend::winit::init(&mut state, &handle)?,
        BackendKind::Udev => backend::udev::init(&mut state, &handle)?,
    }
    tracing::info!("backend: {}", state.backend.name());

    // SIGINT/SIGTERM end the session cleanly (the socket file is removed on drop).
    let signal = event_loop.get_signal();
    handle
        .insert_source(Signals::new(&[Signal::SIGINT, Signal::SIGTERM])?, move |_, _, _| signal.stop())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Config reload: poll the mtime of the config and its includes twice a second.
    let mut stamp = config_stamp(&state.config.files);
    handle
        .insert_source(Timer::from_duration(Duration::from_millis(500)), move |_, _, state| {
            let now = config_stamp(&state.config.files);
            if now != stamp {
                stamp = now;
                if state.reload_config().is_ok() {
                    tracing::info!("config reloaded");
                    stamp = config_stamp(&state.config.files);
                }
            }
            TimeoutAction::ToDuration(Duration::from_millis(500))
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    std::env::set_var("UWC_SOCKET", &state.ipc.path);
    std::env::set_var("XDG_CURRENT_DESKTOP", "uwc");
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    std::env::set_var("XCURSOR_SIZE", state.config.cursor_size.to_string());
    if !state.config.cursor_theme.is_empty() {
        std::env::set_var("XCURSOR_THEME", &state.config.cursor_theme);
    }
    tracing::info!(
        "listening on WAYLAND_DISPLAY={} control socket {}",
        state.socket_name.to_string_lossy(),
        state.ipc.path.display()
    );

    if let Some(cmd) = &args.startup {
        state.spawn("sh", &["-c".to_string(), cmd.clone()]);
    }

    event_loop.run(None, &mut state, |state| {
        state.udev_render();
        state.workspace_refresh();
        state.ipc_flush();
        let _ = state.display_handle.flush_clients();
    })?;
    Ok(())
}

/// Modification times of the config files; a change means reload.
fn config_stamp(files: &[PathBuf]) -> Vec<Option<std::time::SystemTime>> {
    files.iter().map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok()).collect()
}

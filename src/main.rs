//! uwc: a Wayland compositor with an infinite 2D canvas and dwm-style tags, in Rust on smithay.
//! The successor to uwm (the same ideas, the same config and IPC), see README.md.

mod activation;
mod backend;
mod canvas;
mod capture;
mod config;
mod constraints;
mod cursor;
mod deco;
mod export;
mod handlers;
mod idle;
mod ime;
mod input;
mod ipc;
mod kdl;
mod layer;
mod options;
mod output_management;
mod output_power;
mod protocol;
mod protocols;
mod render;
mod rules;
mod scale;
mod screencopy;
mod session;
mod sessionlock;
mod state;
mod sync;
mod toplevels;
mod workspace;
mod xwayland;

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
       uwc msg [-p] COMMAND...   talk to a running uwc (`uwc msg --help` lists the commands)
  -c, --config FILE   config file (default: $UWC_CONFIG, else ~/.config/uwc/config.kdl)
  -s CMD              run CMD (through sh) once the compositor is up
  -t, --check-config  validate the config and exit
      --backend NAME  udev: a real session (run from a TTY); winit: a window in the current session.
                      Default: winit if WAYLAND_DISPLAY or DISPLAY is set, else udev
      --headless [WxH,WxH,...]  no display: virtual outputs (default one 1280x720), for tests
  -h, --help";

#[derive(Debug, Clone, PartialEq)]
enum BackendKind {
    Udev,
    Winit,
    Headless(Vec<(i32, i32)>),
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
                let sizes = match it.peek().filter(|n| !n.starts_with('-')) {
                    Some(s) => {
                        let sizes: Vec<(i32, i32)> = s
                            .split(',')
                            .map(|p| {
                                let (w, h) = p.split_once('x').context("--headless size must look like 1280x720")?;
                                Ok((w.parse::<i32>()?, h.parse::<i32>()?))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        it.next();
                        sizes
                    }
                    None => vec![(1280, 720)],
                };
                if sizes.is_empty() || sizes.iter().any(|&(w, h)| w < 1 || h < 1 || w > 16384 || h > 16384) {
                    bail!("--headless size out of range");
                }
                a.backend = Some(BackendKind::Headless(sizes));
            }
            "--backend" => {
                a.backend = Some(match it.next().as_deref() {
                    Some("udev") => BackendKind::Udev,
                    Some("winit") => BackendKind::Winit,
                    Some("headless") => BackendKind::Headless(vec![(1280, 720)]),
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
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("msg") {
        if let Err(e) = msg(&argv[1..]) {
            eprintln!("uwc msg: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = run() {
        // One readable line: this is what someone starting a session from a TTY sees.
        eprintln!("uwc: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
            .with(GlLog)
            .init();
    }
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
        BackendKind::Headless(sizes) => backend::headless::init(&mut state, &handle, &sizes)?,
        BackendKind::Winit => backend::winit::init(&mut state, &handle)?,
        BackendKind::Udev => backend::udev::init(&mut state, &handle)?,
    }
    tracing::info!("backend: {}", state.backend.name());
    if state.config.xwayland && !matches!(state.backend, backend::Backend::Headless) {
        if let Err(e) = state.start_xwayland() {
            tracing::error!("xwayland: {e}");
        }
    }
    state.session_load();

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

    // The session is written every few seconds when it changed, so a crash loses little.
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(5)), |_, _, state| {
            if let Err(e) = state.session_save(false) {
                tracing::warn!("saving the session: {e:#}");
            }
            TimeoutAction::ToDuration(Duration::from_secs(5))
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    event_loop.run(None, &mut state, |state| {
        state.udev_render();
        state.sync_scales();
        state.workspace_refresh();
        state.toplevels_refresh();
        state.screencopy_flush();
        state.ipc_flush();
        let _ = state.display_handle.flush_clients();
    })?;
    if let Err(e) = state.session_save(true) {
        tracing::warn!("saving the session: {e:#}");
    }
    Ok(())
}

/// `uwc msg`: send one command to the running compositor and print the reply (one JSON line; `-p`
/// pretty). `subscribe` keeps printing. An error reply goes to stderr with exit status 1.
fn msg(args: &[String]) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let (pretty, args) = match args.first().map(String::as_str) {
        Some("-p" | "--pretty") => (true, &args[1..]),
        _ => (false, args),
    };
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        println!("usage: uwc msg [-p] COMMAND...\n\ncommands:");
        for (c, what) in protocol::COMMANDS {
            println!("  {c:<36} {what}");
        }
        return Ok(());
    }
    let line = args.join(" ");
    // Check it here so a typo is reported without a round trip.
    let req = protocol::Request::parse(&line).map_err(|e| anyhow::anyhow!(e))?;
    let path = match std::env::var_os("UWC_SOCKET") {
        Some(p) => PathBuf::from(p),
        None => {
            // The newest uwc socket in the runtime dir.
            let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
            std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with("uwc-") && n.ends_with(".sock")
                })
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
                .map(|e| e.path())
                .context("no running uwc found (UWC_SOCKET is not set and there is no uwc-*.sock)")?
        }
    };
    let mut s = std::os::unix::net::UnixStream::connect(&path).with_context(|| format!("{}", path.display()))?;
    writeln!(s, "{line}")?;
    let mut lines = BufReader::new(s).lines();
    loop {
        let Some(l) = lines.next() else { bail!("uwc closed the connection") };
        let v: serde_json::Value = serde_json::from_str(&l?)?;
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            bail!("{e}");
        }
        let out = if pretty { serde_json::to_string_pretty(&v)? } else { v.to_string() };
        let mut stdout = std::io::stdout().lock();
        // A closed pipe (`uwc msg subscribe | head`) ends quietly.
        if writeln!(stdout, "{out}").and_then(|_| stdout.flush()).is_err() {
            return Ok(());
        }
        if req != protocol::Request::Subscribe {
            return Ok(());
        }
    }
}

/// Keeps the last `[GL]` compiler message smithay logs (see `deco::GL_LOG`).
struct GlLog;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for GlLog {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(Option<String>);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if f.name() == "message" {
                    self.0 = Some(format!("{v:?}"));
                }
            }
        }
        if !e.metadata().target().starts_with("smithay::backend::renderer::gles") {
            return;
        }
        let mut m = Message(None);
        e.record(&mut m);
        if let Some(msg) = m.0.filter(|m| m.starts_with("[GL]")) {
            if let Ok(mut log) = deco::GL_LOG.lock() {
                *log = Some(msg.trim_start_matches("[GL]").trim().to_string());
            }
        }
    }
}

/// Modification times of the config files; a change means reload.
fn config_stamp(files: &[PathBuf]) -> Vec<Option<std::time::SystemTime>> {
    files.iter().map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok()).collect()
}

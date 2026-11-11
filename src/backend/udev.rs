//! A real session: libseat for device access and VT switching, udev for GPUs and hotplug, DRM/KMS +
//! GBM + GLES for output, libinput for input.
//!
//! Scope for now: the primary GPU only, and one output on it (the state has one monitor until
//! multi-monitor lands): the first connected connector that the config does not turn `off`. Connector
//! hotplug rescans, so unplugging that output moves to the next one.
//!
//! Frame pacing: a frame is rendered when something asked for one (`Uwc.redraw`) and the previous
//! one has been scanned out (its vblank arrived). Frame callbacks go out right after a frame is queued;
//! when nothing changed on screen they go out one refresh later, so a client committing without
//! damage cannot spin.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
        },
        egl::{EGLContext, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{gles::GlesRenderer, ImportDma},
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{all_gpus, driver, primary_gpu, UdevBackend, UdevEvent},
    },
    output::{Mode as WlMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            LoopHandle, RegistrationToken,
        },
        drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags},
        input::{
            AccelProfile as LiAccel, ClickMethod as LiClick, Device as InputDevice, DeviceCapability, Libinput,
            ScrollMethod as LiScroll, SendEventsMode, TapButtonMap,
        },
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Transform},
    wayland::{
        dmabuf::DmabufFeedbackBuilder,
        drm_syncobj::{supports_syncobj_eventfd, DrmSyncobjState},
    },
};

use super::Backend;
use crate::{
    config::{AccelProfile, ClickMethod, Config, MonitorConfig, Pointer, ScrollMethod, SendEvents},
    render::UwcElement,
    state::Uwc,
};

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
/// What each queued frame carries until its vblank: the presentation feedback clients asked for.
type Queued = smithay::desktop::utils::OutputPresentationFeedback;
type OutputManager = DrmOutputManager<Allocator, Exporter, Queued, DrmDeviceFd>;
type GbmDrmOutput = DrmOutput<Allocator, Exporter, Queued, DrmDeviceFd>;

/// Scan-out formats to try, in order.
const FORMATS: [Fourcc; 2] = [Fourcc::Argb8888, Fourcc::Xrgb8888];

pub struct UdevData {
    session: LibSeatSession,
    /// The GPU uwc renders with and drives outputs from.
    primary: DrmNode,
    gpu: Option<Gpu>,
    libinput: Libinput,
    /// Pointer devices, so a config reload can apply touchpad / mouse settings to them.
    pointers: Vec<InputDevice>,
    touchpad: Pointer,
    mouse: Pointer,
    monitors: Vec<MonitorConfig>,
    /// The monitor settings changed: set the output up again on the next scan.
    reinit: bool,
    /// Why the primary GPU could not be opened, for the startup error.
    gpu_error: Option<String>,
    hardware_cursor: bool,
    direct_scanout: bool,
    explicit_sync: bool,
}

struct Gpu {
    node: DrmNode,
    path: PathBuf,
    manager: OutputManager,
    renderer: GlesRenderer,
    notifier: RegistrationToken,
    outs: Vec<Out>,
}

/// One scan-out output in use.
struct Out {
    crtc: crtc::Handle,
    connector: connector::Handle,
    output: Output,
    global: GlobalId,
    drm_output: GbmDrmOutput,
    /// A frame is queued and its vblank has not arrived yet.
    pending: bool,
}

impl UdevData {
    pub fn switch_vt(&mut self, vt: i32) {
        if let Err(e) = self.session.change_vt(vt) {
            tracing::warn!("switch to VT {vt}: {e}");
        }
    }

    pub fn renderer_mut(&mut self) -> Option<&mut GlesRenderer> {
        self.gpu.as_mut().map(|g| &mut g.renderer)
    }

    pub fn config_changed(&mut self, config: &Config) {
        self.direct_scanout = config.direct_scanout;
        self.touchpad = config.touchpad.clone();
        self.mouse = config.mouse_input.clone();
        if self.monitors != config.monitors {
            self.monitors = config.monitors.clone();
            self.reinit = true;
        }
        for d in &mut self.pointers {
            apply_pointer_config(d, &self.touchpad, &self.mouse);
        }
    }
}

pub fn init(state: &mut Uwc, handle: &LoopHandle<'static, Uwc>) -> Result<()> {
    let (session, notifier) = LibSeatSession::new().map_err(|e| {
        anyhow!(
            "could not start a session ({e}); run uwc from a TTY with seatd or logind, or nested with --backend winit"
        )
    })?;
    let seat = session.seat();

    let path = primary_gpu(&seat)
        .ok()
        .flatten()
        .or_else(|| all_gpus(&seat).ok().and_then(|g| g.into_iter().next()))
        .context("no GPU found")?;
    let primary = DrmNode::from_path(&path).with_context(|| format!("{}", path.display()))?;
    tracing::info!("primary GPU {}", path.display());

    let mut libinput = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput.udev_assign_seat(&seat).map_err(|()| anyhow!("libinput could not assign seat {seat}"))?;
    handle
        .insert_source(LibinputInputBackend::new(libinput.clone()), |event, _, state| state.libinput_event(event))
        .map_err(|e| anyhow!("{e}"))?;

    handle
        .insert_source(notifier, |event, _, state| match event {
            SessionEvent::PauseSession => state.udev_pause(),
            SessionEvent::ActivateSession => state.udev_resume(),
        })
        .map_err(|e| anyhow!("{e}"))?;

    state.backend = Backend::Udev(Box::new(UdevData {
        session,
        primary,
        gpu: None,
        libinput,
        pointers: Vec::new(),
        touchpad: state.config.touchpad.clone(),
        mouse: state.config.mouse_input.clone(),
        monitors: state.config.monitors.clone(),
        reinit: false,
        gpu_error: None,
        hardware_cursor: state.config.hardware_cursor,
        direct_scanout: state.config.direct_scanout,
        explicit_sync: state.config.explicit_sync,
    }));

    let udev = UdevBackend::new(&seat)?;
    for (dev, path) in udev.device_list() {
        state.udev_device_added(dev, path);
    }
    if let Backend::Udev(u) = &state.backend {
        if u.gpu.is_none() {
            let why = u.gpu_error.clone().unwrap_or_else(|| "it was not among the DRM devices udev lists".into());
            anyhow::bail!("could not use the GPU {}: {why}", path.display());
        }
    }
    handle
        .insert_source(udev, |event, _, state| match event {
            UdevEvent::Added { device_id, path } => state.udev_device_added(device_id, &path),
            UdevEvent::Changed { device_id } => state.udev_device_changed(device_id),
            UdevEvent::Removed { device_id } => state.udev_device_removed(device_id),
        })
        .map_err(|e| anyhow!("{e}"))?;
    // The loop sleeps until something happens; draw the first frame without waiting for that.
    handle.insert_idle(|state| state.udev_render());
    Ok(())
}

/// The mode for a connector: the configured size (nearest refresh), else the preferred mode.
fn pick_mode(
    modes: &[smithay::reexports::drm::control::Mode],
    cfg: Option<&MonitorConfig>,
) -> Option<smithay::reexports::drm::control::Mode> {
    if let Some((w, h, hz)) = cfg.and_then(|c| c.mode) {
        let sized = modes.iter().filter(|m| m.size() == (w as u16, h as u16));
        let best = match hz {
            Some(hz) => {
                sized.min_by(|a, b| (a.vrefresh() as f64 - hz).abs().total_cmp(&(b.vrefresh() as f64 - hz).abs()))
            }
            None => sized.max_by_key(|m| m.vrefresh()),
        };
        if let Some(m) = best {
            return Some(*m);
        }
        tracing::warn!("no {w}x{h} mode on {}; using the preferred one", cfg.unwrap().name);
    }
    modes.iter().find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).or(modes.first()).copied()
}

fn connector_name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

fn apply_pointer_config(d: &mut InputDevice, touchpad: &Pointer, mouse: &Pointer) {
    if !d.has_capability(DeviceCapability::Pointer) {
        return;
    }
    // Devices that can tap are touchpads; everything else is a mouse.
    let p = if d.config_tap_finger_count() > 0 { touchpad } else { mouse };
    if let Some(v) = p.tap {
        let _ = d.config_tap_set_enabled(v);
    }
    if let Some(v) = p.tap_and_drag {
        let _ = d.config_tap_set_drag_enabled(v);
    }
    if let Some(v) = p.drag_lock {
        let _ = d.config_tap_set_drag_lock_enabled(v);
    }
    if let Some(v) = p.natural_scroll {
        let _ = d.config_scroll_set_natural_scroll_enabled(v);
    }
    if let Some(v) = p.disable_while_typing {
        let _ = d.config_dwt_set_enabled(v);
    }
    if let Some(v) = p.left_handed {
        let _ = d.config_left_handed_set(v);
    }
    if let Some(v) = p.middle_emulation {
        let _ = d.config_middle_emulation_set_enabled(v);
    }
    if let Some(v) = p.scroll_method {
        let _ = d.config_scroll_set_method(match v {
            ScrollMethod::None => LiScroll::NoScroll,
            ScrollMethod::TwoFinger => LiScroll::TwoFinger,
            ScrollMethod::Edge => LiScroll::Edge,
            ScrollMethod::OnButtonDown => LiScroll::OnButtonDown,
        });
    }
    if let Some(v) = p.click_method {
        match v {
            // libinput has no "none" click method value; leave the device's own.
            ClickMethod::None => {}
            ClickMethod::ButtonAreas => {
                let _ = d.config_click_set_method(LiClick::ButtonAreas);
            }
            ClickMethod::Clickfinger => {
                let _ = d.config_click_set_method(LiClick::Clickfinger);
            }
        }
    }
    if let Some(v) = p.accel_profile {
        let _ = d.config_accel_set_profile(match v {
            AccelProfile::Flat => LiAccel::Flat,
            AccelProfile::Adaptive => LiAccel::Adaptive,
        });
    }
    if let Some(v) = p.accel_speed {
        let _ = d.config_accel_set_speed(v);
    }
    if let Some(lrm) = p.tap_button_map_lrm {
        let _ = d.config_tap_set_button_map(if lrm {
            TapButtonMap::LeftRightMiddle
        } else {
            TapButtonMap::LeftMiddleRight
        });
    }
    if let Some(v) = p.send_events {
        let _ = d.config_send_events_set_mode(match v {
            SendEvents::Enabled => SendEventsMode::ENABLED,
            SendEvents::Disabled => SendEventsMode::DISABLED,
            SendEvents::DisabledOnExternalMouse => SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE,
        });
    }
}

impl Uwc {
    fn udev(&mut self) -> Option<&mut UdevData> {
        match &mut self.backend {
            Backend::Udev(u) => Some(u),
            _ => None,
        }
    }

    fn libinput_event(&mut self, event: InputEvent<LibinputInputBackend>) {
        match &event {
            InputEvent::DeviceAdded { device } => {
                let mut d = device.clone();
                if let Some(u) = self.udev() {
                    apply_pointer_config(&mut d, &u.touchpad, &u.mouse);
                    if d.has_capability(DeviceCapability::Pointer) {
                        u.pointers.push(d);
                    }
                }
            }
            InputEvent::DeviceRemoved { device } => {
                if let Some(u) = self.udev() {
                    u.pointers.retain(|d| d != device);
                }
            }
            _ => {}
        }
        self.process_input_event(event);
    }

    fn udev_device_added(&mut self, dev: libc_dev_t, path: &Path) {
        let Some(u) = self.udev() else { return };
        let Ok(node) = DrmNode::from_dev_id(dev) else { return };
        if node != u.primary || u.gpu.is_some() {
            // Secondary GPUs are not driven yet.
            return;
        }
        match self.open_gpu(node, dev, path) {
            Ok(()) => self.udev_scan_connectors(),
            Err(e) => {
                tracing::error!("GPU {}: {e:#}", path.display());
                if let Some(u) = self.udev() {
                    u.gpu_error = Some(format!("{e:#}"));
                }
            }
        }
    }

    fn open_gpu(&mut self, node: DrmNode, dev: libc_dev_t, path: &Path) -> Result<()> {
        let drv = driver(dev).ok().flatten().map(|d| d.to_string_lossy().into_owned());
        let modeset = std::fs::read_to_string("/sys/module/nvidia_drm/parameters/modeset").ok();
        if let Some(problem) = nvidia_problem(drv.as_deref(), modeset.as_deref()) {
            anyhow::bail!(problem);
        }
        let nvidia = is_nvidia(drv.as_deref());
        tracing::info!("GPU driver: {}", drv.as_deref().unwrap_or("unknown"));
        let Backend::Udev(u) = &mut self.backend else { return Ok(()) };
        let (hardware_cursor, explicit_sync) = (u.hardware_cursor, u.explicit_sync);
        let fd = u
            .session
            .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK)
            .map_err(|e| anyhow!("open: {e}"))?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true).context("DRM device")?;
        let gbm = GbmDevice::new(fd.clone()).context("GBM device")?;
        // Safety: the EGL display and context live as long as the renderer, which owns them.
        let egl = unsafe { EGLDisplay::new(gbm.clone()) }.context("EGL display")?;
        let context = EGLContext::new(&egl).context("EGL context")?;
        let renderer = unsafe { GlesRenderer::new(context) }.context("GLES renderer")?;

        let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
        let allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
        let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(node));
        // Without a GBM device for it, the manager does not use the cursor plane.
        let cursor_gbm = hardware_cursor.then_some(gbm);
        let manager = OutputManager::new(drm, allocator, exporter, cursor_gbm, FORMATS, render_formats);

        let notifier = self
            .loop_handle
            .insert_source(drm_notifier, |event, metadata, state| match event {
                DrmEvent::VBlank(crtc) => state.udev_vblank(crtc, metadata.take()),
                DrmEvent::Error(e) => tracing::error!("DRM: {e}"),
            })
            .map_err(|e| anyhow!("{e}"))?;

        // GL clients need linux-dmabuf; the feedback tells them which device to allocate on.
        let feedback =
            DmabufFeedbackBuilder::new(node.dev_id(), renderer.dmabuf_formats()).build().context("dmabuf feedback")?;
        if let Some(old) = self.dmabuf_global.take() {
            self.dmabuf_state.destroy_global::<Uwc>(&self.display_handle, old);
        }
        self.dmabuf_global =
            Some(self.dmabuf_state.create_global_with_default_feedback::<Uwc>(&self.display_handle, &feedback));
        // Output captures into a client's dmabuf: what the renderer can draw to, on this device (`capture-dmabuf`).
        self.capture_dmabuf = Some(crate::capture::DmabufOffer::new(
            node.dev_id(),
            renderer.egl_context().dmabuf_render_formats().iter().copied(),
        ));

        // Explicit sync, when the kernel driver can do syncobj eventfds (NVIDIA 560+, recent Mesa drivers).
        if explicit_sync && self.syncobj_state.is_none() {
            if supports_syncobj_eventfd(&fd) {
                self.syncobj_state = Some(DrmSyncobjState::new::<Uwc>(&self.display_handle, fd));
                tracing::info!("explicit sync (linux-drm-syncobj-v1) enabled");
            } else {
                tracing::info!("explicit sync not supported by this GPU driver");
                if nvidia {
                    tracing::warn!(
                        "NVIDIA without explicit sync: GL/Vulkan apps may flicker; NVIDIA 560 or newer has it"
                    );
                }
            }
        }

        let Backend::Udev(u) = &mut self.backend else { unreachable!() };
        u.gpu = Some(Gpu { node, path: path.into(), manager, renderer, notifier, outs: Vec::new() });
        Ok(())
    }

    fn udev_device_changed(&mut self, dev: libc_dev_t) {
        let Some(u) = self.udev() else { return };
        if u.gpu.as_ref().is_some_and(|g| g.node.dev_id() == dev) {
            self.udev_scan_connectors();
        }
    }

    fn udev_device_removed(&mut self, dev: libc_dev_t) {
        let Some(u) = self.udev() else { return };
        if !u.gpu.as_ref().is_some_and(|g| g.node.dev_id() == dev) {
            return;
        }
        let gpu = u.gpu.take().unwrap();
        tracing::warn!("primary GPU {} removed", gpu.path.display());
        self.loop_handle.remove(gpu.notifier);
        for out in gpu.outs {
            self.display_handle.remove_global::<Uwc>(out.global);
        }
    }

    /// Make sure the output in use is still connected, else pick the first usable connected one.
    pub fn udev_scan_connectors(&mut self) {
        // Check before taking: a failed match must not leave the backend taken.
        if !matches!(self.backend, Backend::Udev(_)) {
            return;
        }
        let Backend::Udev(mut u) = std::mem::take(&mut self.backend) else { unreachable!() };
        let reinit = std::mem::take(&mut u.reinit);
        if let Some(gpu) = u.gpu.as_mut() {
            if let Err(e) = self.scan_gpu(gpu, &u.monitors, reinit) {
                tracing::error!("outputs: {e:#}");
            }
        }
        self.backend = Backend::Udev(u);
        self.damage();
    }

    fn scan_gpu(&mut self, gpu: &mut Gpu, monitors: &[MonitorConfig], reinit: bool) -> Result<()> {
        let device = gpu.manager.device();
        let res = device.resource_handles().context("DRM resources")?;
        let connected: Vec<connector::Info> = res
            .connectors()
            .iter()
            .filter_map(|c| device.get_connector(*c, true).ok())
            .filter(|i| i.state() == connector::State::Connected && !i.modes().is_empty())
            .collect();

        // Drop outputs that are gone, turned off by config, or when a full reinit is wanted.
        let (kept, dropped): (Vec<Out>, Vec<Out>) = gpu.outs.drain(..).partition(|out: &Out| {
            let still = connected.iter().any(|i| i.handle() == out.connector);
            let off = monitors.iter().any(|m| m.name == out.output.name() && m.off);
            let keep = still && !off && !reinit;
            if !keep {
                tracing::info!("output {} {}", out.output.name(), if still && !off { "reconfigured" } else { "gone" });
            }
            keep
        });
        gpu.outs = kept;
        for out in dropped {
            self.display_handle.remove_global::<Uwc>(out.global);
            drop(out.drm_output);
            self.remove_monitor(out.output.name().as_str());
        }

        for info in &connected {
            let name = connector_name(info);
            let cfg = monitors.iter().find(|m| m.name == name);
            if cfg.is_some_and(|c| c.off) {
                continue;
            }
            if gpu.outs.iter().any(|o| o.connector == info.handle()) {
                continue;
            }
            let device = gpu.manager.device();
            let crtc = info
                .encoders()
                .iter()
                .filter_map(|e| device.get_encoder(*e).ok())
                .flat_map(|e| res.filter_crtcs(e.possible_crtcs()))
                .find(|c| !gpu.outs.iter().any(|o| o.crtc == *c));
            let (Some(crtc), Some(mode)) = (crtc, pick_mode(info.modes(), cfg)) else { continue };

            let (mm_w, mm_h) = info.size().unwrap_or((0, 0));
            let output = Output::new(
                name.clone(),
                PhysicalProperties {
                    size: (mm_w as i32, mm_h as i32).into(),
                    subpixel: Subpixel::Unknown,
                    make: "Unknown".into(),
                    model: "Unknown".into(),
                },
            );
            let wl_mode = WlMode::from(mode);
            let scale = cfg.and_then(|c| c.scale).unwrap_or(1.0);
            let transform = cfg.and_then(|c| c.transform).unwrap_or(Transform::Normal);
            output.change_current_state(
                Some(wl_mode),
                Some(transform),
                Some(Scale::Fractional(scale)),
                Some((0, 0).into()),
            );
            output.set_preferred(wl_mode);
            let global = output.create_global::<Uwc>(&self.display_handle);

            let drm_output = match gpu.manager.initialize_output::<_, UwcElement>(
                crtc,
                mode,
                &[info.handle()],
                &output,
                None,
                &mut gpu.renderer,
                &DrmOutputRenderElements::new(),
            ) {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!("output {name}: {e}");
                    self.display_handle.remove_global::<Uwc>(global);
                    continue;
                }
            };
            let logical = transform.transform_size(wl_mode.size).to_f64().to_logical(scale).to_i32_round();
            tracing::info!(
                "output {name}: {}x{}@{:.2} scale {scale}, {}x{} logical",
                wl_mode.size.w,
                wl_mode.size.h,
                wl_mode.refresh as f64 / 1000.0,
                logical.w,
                logical.h
            );
            self.add_monitor(output.clone(), logical);
            // Windows already mapped need to know about the (new) output for their scale.
            for c in &self.clients {
                if let Some(surface) = c.surface() {
                    smithay::desktop::utils::output_update(
                        &output,
                        Some(smithay::utils::Rectangle::from_size(logical)),
                        &surface,
                    );
                }
            }
            gpu.outs.push(Out { crtc, connector: info.handle(), output, global, drm_output, pending: false });
        }
        if gpu.outs.is_empty() {
            tracing::warn!("no usable output connected");
        }
        Ok(())
    }

    fn udev_vblank(&mut self, crtc: crtc::Handle, metadata: Option<smithay::backend::drm::DrmEventMetadata>) {
        use smithay::backend::drm::DrmEventTime as DrmTime;
        let Some(u) = self.udev() else { return };
        let Some(out) = u.gpu.as_mut().and_then(|g| g.outs.iter_mut().find(|o| o.crtc == crtc)) else { return };
        let feedback = match out.drm_output.frame_submitted() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("frame submitted: {e}");
                None
            }
        };
        out.pending = false;
        let output = out.output.clone();
        // Clients that asked when their content was shown are told what the kernel measured: the flip's
        // own timestamp (CLOCK_MONOTONIC unless the driver only has the realtime clock, then now) and its
        // sequence number.
        if let Some(feedback) = feedback {
            let (time, seq) = match metadata {
                Some(m) => (
                    match m.time {
                        DrmTime::Monotonic(d) => Some(d),
                        DrmTime::Realtime(_) => None,
                    },
                    m.sequence as u64,
                ),
                None => (None, self.frames_drawn),
            };
            use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
            self.frame_presented(&output, feedback, time, seq, Kind::Vsync | Kind::HwClock | Kind::HwCompletion);
        }
        // The next frame, if one is wanted, is rendered at the end of this event-loop turn.
    }

    /// Called once per event-loop turn: draw if something changed and the output is free.
    pub fn udev_render(&mut self) {
        if !self.redraw {
            return;
        }
        // Check before taking: a failed match must not leave the backend taken.
        if !matches!(self.backend, Backend::Udev(_)) {
            return;
        }
        let Backend::Udev(mut u) = std::mem::take(&mut self.backend) else { unreachable!() };
        let active = u.session.is_active();
        let frame_flags = if u.direct_scanout {
            FrameFlags::DEFAULT
        } else {
            // Composite everything; only the cursor may still use its plane.
            FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT
        };
        if let (true, Some(gpu)) = (active, u.gpu.as_mut()) {
            let mut pending_left = false;
            for out in gpu.outs.iter_mut() {
                if out.pending {
                    pending_left = true;
                    continue;
                }
                let m = self.mon_by_output(&out.output).unwrap_or(0);
                let elements = self.frame_elements(&mut gpu.renderer, m);
                let queued =
                    match out.drm_output.render_frame(&mut gpu.renderer, &elements, self.tag_look().0, frame_flags) {
                        Ok(r) if !r.is_empty => match out.drm_output.queue_frame(self.take_feedback(&out.output)) {
                            Ok(()) => {
                                out.pending = true;
                                self.frames_drawn += 1;
                                true
                            }
                            Err(e) => {
                                tracing::warn!("queue frame: {e}");
                                false
                            }
                        },
                        Ok(_) => false,
                        Err(e) => {
                            tracing::warn!("render: {e}");
                            false
                        }
                    };
                let output = out.output.clone();
                if queued {
                    super::send_frame_callbacks(self, &output);
                } else {
                    // Nothing changed on screen: answer frame callbacks one refresh later.
                    let refresh = output.current_mode().map(|m| m.refresh).unwrap_or(60_000).max(1000);
                    let d = Duration::from_micros(1_000_000_000 / refresh as u64);
                    let _ = self.loop_handle.insert_source(Timer::from_duration(d), move |_, _, state| {
                        super::frame_done(state, &output);
                        TimeoutAction::Drop
                    });
                }
            }
            self.redraw = pending_left;
            if std::mem::take(&mut self.animating) {
                // Drawn again once this frame is on screen.
                self.redraw = true;
            }
        }
        self.popups.cleanup();
        self.backend = Backend::Udev(u);
    }

    /// Switched away from our VT: stop touching devices.
    fn udev_pause(&mut self) {
        let Some(u) = self.udev() else { return };
        tracing::info!("session paused");
        u.libinput.suspend();
        if let Some(gpu) = u.gpu.as_mut() {
            gpu.manager.pause();
            for out in gpu.outs.iter_mut() {
                out.pending = false;
            }
        }
    }

    /// Back on our VT: take the devices again and redraw everything.
    fn udev_resume(&mut self) {
        let Some(u) = self.udev() else { return };
        tracing::info!("session resumed");
        if u.libinput.resume().is_err() {
            tracing::error!("libinput resume failed");
        }
        if let Some(gpu) = u.gpu.as_mut() {
            if let Err(e) = gpu.manager.activate(false) {
                tracing::error!("DRM activate: {e}");
            }
            for out in gpu.outs.iter_mut() {
                out.pending = false;
            }
        }
        // Monitors may have been plugged in while we were away.
        self.udev_scan_connectors();
        self.damage();
    }
}

#[allow(non_camel_case_types)]
type libc_dev_t = smithay::reexports::rustix::fs::Dev;

fn is_nvidia(driver: Option<&str>) -> bool {
    driver.is_some_and(|d| d.starts_with("nvidia"))
}

/// What is wrong with an NVIDIA setup that cannot work, if anything. `modeset` is the content of
/// /sys/module/nvidia_drm/parameters/modeset (absent when nvidia_drm is not loaded).
fn nvidia_problem(driver: Option<&str>, modeset: Option<&str>) -> Option<String> {
    if !is_nvidia(driver) {
        return None;
    }
    const FIX: &str = "add `nvidia_drm.modeset=1 nvidia_drm.fbdev=1` to the kernel command line (or \
        `options nvidia_drm modeset=1 fbdev=1` in /etc/modprobe.d/nvidia.conf, then rebuild the initramfs) and reboot";
    match modeset.map(str::trim) {
        Some("Y") => None,
        Some(_) => Some(format!("the NVIDIA driver has kernel modesetting off; {FIX}")),
        None => Some(format!("the NVIDIA driver is loaded but nvidia_drm is not; {FIX}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_modeset_is_required() {
        assert_eq!(nvidia_problem(Some("amdgpu"), None), None);
        assert_eq!(nvidia_problem(Some("i915"), Some("N")), None);
        assert_eq!(nvidia_problem(None, None), None);
        assert_eq!(nvidia_problem(Some("nvidia"), Some("Y\n")), None);
        assert!(nvidia_problem(Some("nvidia"), Some("N\n")).unwrap().contains("nvidia_drm.modeset=1"));
        assert!(nvidia_problem(Some("nvidia"), None).unwrap().contains("nvidia_drm is not"));
        // nouveau is not the proprietary driver and needs none of this.
        assert_eq!(nvidia_problem(Some("nouveau"), None), None);
    }
}

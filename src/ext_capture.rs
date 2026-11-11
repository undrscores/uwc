//! ext-foreign-toplevel-list-v1, ext-image-capture-source-v1 and ext-image-copy-capture-v1: the
//! standard way to capture an output or a single window, which is what xdg-desktop-portal's
//! screen sharing (browsers, OBS, video calls) is built on.
//!
//! Every mapped window is announced on the toplevel list under a stable identifier. A capture
//! source is an output or one of those toplevels. A session on a source offers one ARGB8888 shm
//! buffer size; its frames are copied through the same capture core as wlr-screencopy and the
//! window previews. A frame waits until the source has something new (a window commit, a drawn
//! frame), so recorders get one picture per change. Cursor sessions are accepted but stay
//! silent: ask for `paint_cursors` instead. There is no dmabuf path yet (shm only).

use std::sync::{Arc, Mutex, OnceLock};

use smithay::{
    delegate_foreign_toplevel_list,
    output::Output,
    reexports::wayland_server::{
        backend::ClientId, protocol::wl_buffer::WlBuffer, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch,
        New, Resource,
    },
    utils::{Physical, Rectangle, Size},
    wayland::foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState},
};
use wayland_protocols::ext::{
    image_capture_source::v1::server::{
        ext_foreign_toplevel_image_capture_source_manager_v1::{self, ExtForeignToplevelImageCaptureSourceManagerV1},
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::{self, ExtOutputImageCaptureSourceManagerV1},
    },
    image_copy_capture::v1::server::{
        ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
        ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1, Options},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

use crate::{capture, state::Uwc};

/// What a capture source is.
#[derive(Clone)]
pub enum Source {
    Output(Output),
    /// A window, by uwc's id.
    Window(u32),
}

pub struct SourceData(Source);

/// A capture session: its source, and what it last told the client.
pub struct SessionData {
    /// The session's own resource, for events that come from a frame's request.
    resource: OnceLock<ExtImageCopyCaptureSessionV1>,
    source: Source,
    paint_cursors: bool,
    inner: Mutex<SessionInner>,
}

struct SessionInner {
    /// The size the client was last told to use.
    size: Size<i32, Physical>,
    /// A frame exists (only one at a time).
    has_frame: bool,
    /// What the source looked like at the last capture (window commits / frames drawn).
    last_key: Option<u64>,
    stopped: bool,
}

pub struct FrameData {
    session: Arc<SessionData>,
    inner: Mutex<FrameInner>,
}

#[derive(Default)]
struct FrameInner {
    buffer: Option<WlBuffer>,
    captured: bool,
}

struct Pending {
    frame: ExtImageCopyCaptureFrameV1,
    session_res: ExtImageCopyCaptureSessionV1,
    session: Arc<SessionData>,
}

pub struct ExtCapture {
    list: ForeignToplevelListState,
    /// Announced windows: uwc id and handle.
    windows: Vec<(u32, ForeignToplevelHandle)>,
    pending: Vec<Pending>,
}

impl ExtCapture {
    pub fn new(dh: &DisplayHandle) -> ExtCapture {
        dh.create_global::<Uwc, ExtOutputImageCaptureSourceManagerV1, ()>(1, ());
        dh.create_global::<Uwc, ExtForeignToplevelImageCaptureSourceManagerV1, ()>(1, ());
        dh.create_global::<Uwc, ExtImageCopyCaptureManagerV1, ()>(1, ());
        ExtCapture {
            list: ForeignToplevelListState::new_with_filter::<Uwc>(dh, |c| !crate::sandbox::is_sandboxed(c)),
            windows: Vec::new(),
            pending: Vec::new(),
        }
    }
}

impl ForeignToplevelListHandler for Uwc {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.ext_capture.list
    }
}
delegate_foreign_toplevel_list!(Uwc);

impl Uwc {
    /// Keep the toplevel list in step with the windows: new ones announced, retitled ones
    /// updated, gone (or unmapped) ones closed.
    pub fn ext_toplevels_refresh(&mut self) {
        let ids: Vec<u32> = self.clients.iter().filter(|c| c.mapped).map(|c| c.id).collect();
        let gone: Vec<usize> =
            (0..self.ext_capture.windows.len()).filter(|&k| !ids.contains(&self.ext_capture.windows[k].0)).collect();
        for k in gone.into_iter().rev() {
            let (_, h) = self.ext_capture.windows.remove(k);
            self.ext_capture.list.remove_toplevel(&h);
        }
        for id in ids {
            let Some(i) = self.client_index(id) else { continue };
            let (title, app_id) = (self.clients[i].title(), self.clients[i].app_id());
            match self.ext_capture.windows.iter().find(|(w, _)| *w == id) {
                None => {
                    let h = self.ext_capture.list.new_toplevel::<Uwc>(&title, &app_id);
                    h.send_done();
                    self.ext_capture.windows.push((id, h));
                }
                Some((_, h)) => {
                    let mut changed = false;
                    if h.title() != title {
                        h.send_title(&title);
                        changed = true;
                    }
                    if h.app_id() != app_id {
                        h.send_app_id(&app_id);
                        changed = true;
                    }
                    if changed {
                        h.send_done();
                    }
                }
            }
        }
        self.ext_capture.list.cleanup_closed_handles();
    }

    /// The size of what `source` shows, in physical px; None if it is gone.
    fn source_size(&self, source: &Source) -> Option<Size<i32, Physical>> {
        match source {
            Source::Output(o) => self.mon_by_output(o).map(|m| self.output_pixels(m)),
            Source::Window(id) => self.window_capture_size(*id),
        }
    }

    /// A number that changes whenever the source has new content.
    fn source_key(&self, source: &Source) -> u64 {
        match source {
            Source::Output(_) => self.frames_drawn,
            Source::Window(id) => self.client_index(*id).map(|i| self.clients[i].commits).unwrap_or(0),
        }
    }

    fn announce_constraints(&self, session: &ExtImageCopyCaptureSessionV1, size: Size<i32, Physical>, source: &Source) {
        session.buffer_size(size.w as u32, size.h as u32);
        session.shm_format(capture::FORMAT);
        // Dmabufs for whole outputs only; a window capture stays shm.
        if let (Source::Output(_), Some(offer)) = (source, self.dmabuf_offer()) {
            session.dmabuf_device(offer.device_bytes());
            for (code, mods) in &offer.formats {
                session.dmabuf_format(*code as u32, capture::DmabufOffer::modifier_bytes(mods));
            }
        }
        session.done();
    }

    /// Try to answer a frame's capture. True when it was answered (ready or failed), false when
    /// the source has nothing new yet.
    fn ext_try_capture(
        &mut self,
        frame: &ExtImageCopyCaptureFrameV1,
        session_res: &ExtImageCopyCaptureSessionV1,
        session: &Arc<SessionData>,
        force: bool,
    ) -> bool {
        let Some(fd) = frame.data::<FrameData>() else { return true };
        let buffer = fd.inner.lock().unwrap().buffer.clone();
        let Some(buffer) = buffer else { return true };
        let Some(size) = self.source_size(&session.source).filter(|_| !self.is_locked()) else {
            // The source went away (or the screen is locked).
            let stop = !self.is_locked();
            frame.failed(if stop { FailureReason::Stopped } else { FailureReason::Unknown });
            let mut s = session.inner.lock().unwrap();
            if stop && !s.stopped {
                s.stopped = true;
                session_res.stopped();
            }
            return true;
        };
        let key = self.source_key(&session.source);
        {
            let mut s = session.inner.lock().unwrap();
            if s.size != size {
                // The source was resized: new constraints, and this frame cannot be filled.
                s.size = size;
                drop(s);
                self.announce_constraints(session_res, size, &session.source);
                frame.failed(FailureReason::BufferConstraints);
                return true;
            }
            if !force && s.last_key == Some(key) {
                return false;
            }
        }
        let checked = match &session.source {
            Source::Output(_) => capture::check_output_buffer(&buffer, size, self.dmabuf_offer()),
            Source::Window(_) => capture::check_buffer(&buffer, size),
        };
        if let Err(e) = checked {
            tracing::debug!("image copy capture: {e}");
            frame.failed(FailureReason::BufferConstraints);
            return true;
        }
        let res = match &session.source {
            Source::Window(id) => self.capture_window(*id, &buffer),
            Source::Output(o) => match self.mon_by_output(o) {
                Some(m) => {
                    let full = Rectangle::from_size(size);
                    self.capture_output(m, full, session.paint_cursors, &buffer)
                }
                None => Err("the output is gone".into()),
            },
        };
        match res {
            Ok(()) => {
                session.inner.lock().unwrap().last_key = Some(key);
                frame.transform(smithay::reexports::wayland_server::protocol::wl_output::Transform::Normal);
                frame.damage(0, 0, size.w, size.h);
                let (secs, nsec) = capture::now();
                frame.presentation_time((secs >> 32) as u32, secs as u32, nsec);
                frame.ready();
            }
            Err(e) => {
                tracing::debug!("image copy capture failed: {e}");
                frame.failed(FailureReason::Unknown);
            }
        }
        true
    }

    /// Once per event-loop turn: answer the captures that were waiting for something new.
    pub fn ext_capture_flush(&mut self) {
        if self.ext_capture.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.ext_capture.pending);
        for p in pending {
            if !self.ext_try_capture(&p.frame, &p.session_res, &p.session, false) {
                self.ext_capture.pending.push(p);
            }
        }
    }
}

// ---- sources --------------------------------------------------------------------------------

impl GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, ()> for Uwc {
    fn can_view(client: smithay::reexports::wayland_server::Client, _: &()) -> bool {
        !crate::sandbox::is_sandboxed(&client)
    }

    fn bind(
        _: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtOutputImageCaptureSourceManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        _: &ExtOutputImageCaptureSourceManagerV1,
        request: ext_output_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        if let ext_output_image_capture_source_manager_v1::Request::CreateSource { source, output } = request {
            match Output::from_resource(&output) {
                Some(o) => {
                    init.init(source, SourceData(Source::Output(o)));
                }
                // An output that is gone: a source nothing can be captured from.
                None => {
                    init.init(source, SourceData(Source::Window(0)));
                }
            }
        }
    }
}

impl GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Uwc {
    fn can_view(client: smithay::reexports::wayland_server::Client, _: &()) -> bool {
        !crate::sandbox::is_sandboxed(&client)
    }

    fn bind(
        _: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtForeignToplevelImageCaptureSourceManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: ext_foreign_toplevel_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        if let ext_foreign_toplevel_image_capture_source_manager_v1::Request::CreateSource { source, toplevel_handle } =
            request
        {
            // The handle names a window through the list; one that is gone names window 0, which
            // does not exist, so a session on it stops at once.
            let id = ForeignToplevelHandle::from_resource(&toplevel_handle)
                .and_then(|h| state.ext_capture.windows.iter().find(|(_, w)| w.identifier() == h.identifier()))
                .map(|(id, _)| *id)
                .unwrap_or(0);
            init.init(source, SourceData(Source::Window(id)));
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, SourceData> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        _: &ExtImageCaptureSourceV1,
        _: <ExtImageCaptureSourceV1 as Resource>::Request,
        _: &SourceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
    }
}

// ---- sessions and frames ------------------------------------------------------------------

impl GlobalDispatch<ExtImageCopyCaptureManagerV1, ()> for Uwc {
    fn can_view(client: smithay::reexports::wayland_server::Client, _: &()) -> bool {
        !crate::sandbox::is_sandboxed(&client)
    }

    fn bind(
        _: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtImageCopyCaptureManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        manager: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        use ext_image_copy_capture_manager_v1::Request as R;
        match request {
            R::CreateSession { session, source, options } => {
                let Some(SourceData(src)) = source.data::<SourceData>() else { return };
                let paint = match options.into_result() {
                    Ok(o) => o.contains(Options::PaintCursors),
                    Err(_) => {
                        manager.post_error(ext_image_copy_capture_manager_v1::Error::InvalidOption, "unknown option");
                        return;
                    }
                };
                let size = state.source_size(src);
                let data = Arc::new(SessionData {
                    resource: OnceLock::new(),
                    source: src.clone(),
                    paint_cursors: paint,
                    inner: Mutex::new(SessionInner {
                        size: size.unwrap_or_default(),
                        has_frame: false,
                        last_key: None,
                        stopped: size.is_none(),
                    }),
                });
                let session = init.init(session, data.clone());
                let _ = data.resource.set(session.clone());
                match size.filter(|_| !state.is_locked()) {
                    Some(size) => state.announce_constraints(&session, size, &src.clone()),
                    None => session.stopped(),
                }
            }
            // A cursor session that never reports a cursor; clients get the cursor with
            // `paint_cursors`. Its capture session is stopped at once.
            R::CreatePointerCursorSession { session, source, .. } => {
                let src = source.data::<SourceData>().map(|s| s.0.clone());
                init.init(session, src.unwrap_or(Source::Window(0)).into_cursor());
            }
            _ => {}
        }
    }
}

/// The source a cursor session was made for (kept so `get_capture_session` can be answered).
pub struct CursorData {
    source: Source,
    made: Mutex<bool>,
}

impl Source {
    fn into_cursor(self) -> CursorData {
        CursorData { source: self, made: Mutex::new(false) }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, CursorData> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        cursor: &ExtImageCopyCaptureCursorSessionV1,
        request: ext_image_copy_capture_cursor_session_v1::Request,
        data: &CursorData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        if let ext_image_copy_capture_cursor_session_v1::Request::GetCaptureSession { session } = request {
            let mut made = data.made.lock().unwrap();
            if *made {
                cursor.post_error(ext_image_copy_capture_cursor_session_v1::Error::DuplicateSession, "already made");
                return;
            }
            *made = true;
            let session = init.init(
                session,
                Arc::new(SessionData {
                    resource: OnceLock::new(),
                    source: data.source.clone(),
                    paint_cursors: false,
                    inner: Mutex::new(SessionInner {
                        size: Size::default(),
                        has_frame: false,
                        last_key: None,
                        stopped: true,
                    }),
                }),
            );
            session.stopped();
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, Arc<SessionData>> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        session: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        data: &Arc<SessionData>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        if let ext_image_copy_capture_session_v1::Request::CreateFrame { frame } = request {
            {
                let mut s = data.inner.lock().unwrap();
                if s.has_frame {
                    session
                        .post_error(ext_image_copy_capture_session_v1::Error::DuplicateFrame, "a frame already exists");
                    return;
                }
                s.has_frame = true;
            }
            init.init(frame, FrameData { session: data.clone(), inner: Mutex::new(FrameInner::default()) });
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, FrameData> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        use ext_image_copy_capture_frame_v1::Request as R;
        match request {
            R::AttachBuffer { buffer } => {
                let mut f = data.inner.lock().unwrap();
                if f.captured {
                    frame.post_error(ext_image_copy_capture_frame_v1::Error::AlreadyCaptured, "already captured");
                    return;
                }
                f.buffer = Some(buffer);
            }
            R::DamageBuffer { x, y, width, height } => {
                if x < 0 || y < 0 || width <= 0 || height <= 0 {
                    frame.post_error(ext_image_copy_capture_frame_v1::Error::InvalidBufferDamage, "bad damage");
                }
            }
            R::Capture => {
                {
                    let mut f = data.inner.lock().unwrap();
                    if f.captured {
                        frame.post_error(ext_image_copy_capture_frame_v1::Error::AlreadyCaptured, "already captured");
                        return;
                    }
                    if f.buffer.is_none() {
                        frame.post_error(ext_image_copy_capture_frame_v1::Error::NoBuffer, "no buffer attached");
                        return;
                    }
                    f.captured = true;
                }
                let Some(session_res) = data.session.resource.get().cloned() else {
                    frame.failed(FailureReason::Unknown);
                    return;
                };
                let session = data.session.clone();
                if !state.ext_try_capture(frame, &session_res, &session, false) {
                    state.ext_capture.pending.push(Pending { frame: frame.clone(), session_res, session });
                }
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Uwc, _: ClientId, frame: &ExtImageCopyCaptureFrameV1, data: &FrameData) {
        data.session.inner.lock().unwrap().has_frame = false;
        state.ext_capture.pending.retain(|p| &p.frame != frame);
    }
}

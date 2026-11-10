//! Buffer synchronisation: do not show a client's buffer before the GPU has finished drawing it.
//!
//! - Explicit sync (`linux-drm-syncobj-v1`): the client attaches an acquire point to the commit;
//!   the commit is held back until that point signals. This is what NVIDIA's driver needs (it has no
//!   implicit sync), so without it Firefox, Chromium, games and anything else drawing with Vulkan or
//!   GL show stale or torn frames there. Smithay signals the release points itself.
//! - Implicit sync: a dmabuf without an acquire point is held until its fences say it is readable
//!   (on Mesa drivers the kernel tracks this).
//!
//! The global is offered only on the udev backend, on a GPU whose DRM driver supports syncobj
//! eventfds (and `explicit-sync` is not turned off).

use smithay::{
    delegate_drm_syncobj,
    reexports::{
        calloop::Interest,
        wayland_server::{protocol::wl_surface::WlSurface, Resource},
    },
    wayland::{
        compositor::{
            add_blocker, add_pre_commit_hook, with_states, BufferAssignment, CompositorHandler, SurfaceAttributes,
        },
        dmabuf::get_dmabuf,
        drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
    },
};

use crate::state::Uwc;

impl Uwc {
    /// Install the hook on every new surface (from `CompositorHandler::new_surface`).
    pub fn watch_buffer_readiness(surface: &WlSurface) {
        add_pre_commit_hook::<Uwc, _>(surface, |state, _dh, surface| {
            let mut acquire = None;
            let dmabuf = with_states(surface, |s| {
                acquire.clone_from(&s.cached_state.get::<DrmSyncobjCachedState>().pending().acquire_point);
                s.cached_state.get::<SurfaceAttributes>().pending().buffer.as_ref().and_then(|a| match a {
                    BufferAssignment::NewBuffer(b) => get_dmabuf(b).cloned().ok(),
                    _ => None,
                })
            });
            let Some(dmabuf) = dmabuf else { return };
            let Some(client) = surface.client() else { return };

            if let Some(point) = acquire {
                if let Ok((blocker, source)) = point.generate_blocker() {
                    let c = client.clone();
                    let res = state.loop_handle.insert_source(source, move |_, _, state| {
                        let dh = state.display_handle.clone();
                        state.client_compositor_state(&c).blocker_cleared(state, &dh);
                        Ok(())
                    });
                    if res.is_ok() {
                        add_blocker(surface, blocker);
                        return;
                    }
                }
            }
            // No explicit sync: wait for the dmabuf's implicit fences (already ready = no blocker).
            if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                let res = state.loop_handle.insert_source(source, move |_, _, state| {
                    let dh = state.display_handle.clone();
                    state.client_compositor_state(&client).blocker_cleared(state, &dh);
                    Ok(())
                });
                if res.is_ok() {
                    add_blocker(surface, blocker);
                }
            }
        });
    }
}

impl DrmSyncobjHandler for Uwc {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

delegate_drm_syncobj!(Uwc);

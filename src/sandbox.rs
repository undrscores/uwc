//! Sandboxed clients (Flatpak and friends): `wp_security_context_v1`.
//!
//! A sandbox engine asks for a listening socket for one app. Clients that connect through it are marked
//! (`ClientState.security`) and are not shown the privileged globals: the ones that read the screen or other
//! windows (screencopy, image capture, toplevel export and lists), type or click for the user (virtual
//! keyboard and pointer, input methods), read every clipboard (data control), lock the session, change the
//! outputs or the workspaces, or put surfaces over everything (layer shell), nor the security-context manager
//! itself (a sandbox must not make a socket that is less sandboxed). They keep what an ordinary app needs:
//! compositing, xdg-shell, seat, clipboard and drag and drop through the data device, dmabuf, and so on.
//!
//! A global is hidden by its filter or its `can_view`; both are `|c| !is_sandboxed(c)`. A new privileged global
//! must do the same (see `docs/SECURITY.md`). The IPC socket is not a Wayland global: it is protected by
//! its mode and by the sandbox not being able to see the runtime directory.

use std::sync::Arc;

use smithay::{
    delegate_security_context,
    reexports::wayland_server::Client,
    wayland::security_context::{
        SecurityContext, SecurityContextHandler, SecurityContextListenerSource, SecurityContextState,
    },
};

use crate::state::{ClientState, Uwc};

/// Whether `client` came in through a security context (a sandboxed app). Xwayland's client and the
/// normal socket's clients are not.
pub fn is_sandboxed(client: &Client) -> bool {
    client.get_data::<ClientState>().is_some_and(|d| d.security.is_some())
}

/// The `wp_security_context_manager_v1` global, hidden from sandboxed clients.
pub fn create_state(dh: &smithay::reexports::wayland_server::DisplayHandle) -> SecurityContextState {
    SecurityContextState::new::<Uwc, _>(dh, |c| !is_sandboxed(c))
}

impl SecurityContextHandler for Uwc {
    fn context_created(&mut self, source: SecurityContextListenerSource, context: SecurityContext) {
        tracing::info!(
            "sandbox socket for {:?} app {:?} (instance {:?})",
            context.sandbox_engine,
            context.app_id,
            context.instance_id
        );
        let res = self.loop_handle.insert_source(source, move |stream, _, state| {
            let client = ClientState { security: Some(context.clone()), ..Default::default() };
            if let Err(e) = state.display_handle.insert_client(stream, Arc::new(client)) {
                tracing::warn!("rejecting sandboxed client: {e}");
            }
        });
        if let Err(e) = res {
            tracing::warn!("listening on a sandbox socket: {e}");
        }
    }
}

delegate_security_context!(Uwc);

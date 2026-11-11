//! Small protocols that need little more than a global and a handler: cursor-shape (themed cursors
//! for GTK/Qt), data-control (wl-clipboard, clipboard managers), virtual keyboard (wtype, on-screen
//! keyboards), single-pixel buffers, xdg-foreign (dialogs parented across processes), xdg-dialog
//! (modal hints), content-type, xdg-system-bell and keyboard-shortcuts-inhibit (VMs, remote desktop).

use smithay::{
    delegate_content_type, delegate_cursor_shape, delegate_data_control, delegate_ext_data_control,
    delegate_keyboard_shortcuts_inhibit, delegate_single_pixel_buffer, delegate_virtual_keyboard_manager,
    delegate_xdg_dialog, delegate_xdg_foreign, delegate_xdg_system_bell,
    input::{keyboard::KeyboardHandle, SeatHandler},
    reexports::wayland_server::{protocol::wl_surface::WlSurface, DisplayHandle},
    wayland::{
        content_type::ContentTypeState,
        cursor_shape::CursorShapeManagerState,
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
        },
        selection::{ext_data_control, wlr_data_control},
        shell::xdg::{dialog::XdgDialogHandler, dialog::XdgDialogState, ToplevelSurface},
        single_pixel_buffer::SinglePixelBufferState,
        tablet_manager::TabletSeatHandler,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_foreign::{XdgForeignHandler, XdgForeignState},
        xdg_system_bell::{XdgSystemBellHandler, XdgSystemBellState},
    },
};

use crate::state::Uwc;

/// The states that keep these globals alive.
pub struct Extras {
    _cursor_shape: CursorShapeManagerState,
    _content_type: ContentTypeState,
    _single_pixel: SinglePixelBufferState,
    _virtual_keyboard: VirtualKeyboardManagerState,
    _system_bell: XdgSystemBellState,
    pub wlr_data_control: wlr_data_control::DataControlState,
    pub ext_data_control: ext_data_control::DataControlState,
    pub foreign: XdgForeignState,
    _dialog: XdgDialogState,
    pub inhibit: KeyboardShortcutsInhibitState,
}

impl Extras {
    pub fn new(
        dh: &DisplayHandle,
        primary: &smithay::wayland::selection::primary_selection::PrimarySelectionState,
    ) -> Self {
        Self {
            _cursor_shape: CursorShapeManagerState::new::<Uwc>(dh),
            _content_type: ContentTypeState::new::<Uwc>(dh),
            _single_pixel: SinglePixelBufferState::new::<Uwc>(dh),
            _virtual_keyboard: VirtualKeyboardManagerState::new::<Uwc, _>(dh, |c| !crate::sandbox::is_sandboxed(c)),
            _system_bell: XdgSystemBellState::new::<Uwc>(dh),
            wlr_data_control: wlr_data_control::DataControlState::new::<Uwc, _>(dh, Some(primary), |c| {
                !crate::sandbox::is_sandboxed(c)
            }),
            ext_data_control: ext_data_control::DataControlState::new::<Uwc, _>(dh, Some(primary), |c| {
                !crate::sandbox::is_sandboxed(c)
            }),
            foreign: XdgForeignState::new::<Uwc>(dh),
            _dialog: XdgDialogState::new::<Uwc>(dh),
            inhibit: KeyboardShortcutsInhibitState::new::<Uwc>(dh),
        }
    }
}

impl TabletSeatHandler for Uwc {}
delegate_cursor_shape!(Uwc);
delegate_content_type!(Uwc);
delegate_single_pixel_buffer!(Uwc);
delegate_virtual_keyboard_manager!(Uwc);

impl XdgSystemBellHandler for Uwc {
    // No sound; a bar can listen for it through the IPC later.
    fn ring(&mut self, _surface: Option<WlSurface>) {}
}
delegate_xdg_system_bell!(Uwc);

impl wlr_data_control::DataControlHandler for Uwc {
    fn data_control_state(&self) -> &wlr_data_control::DataControlState {
        &self.extras.wlr_data_control
    }
}
impl ext_data_control::DataControlHandler for Uwc {
    fn data_control_state(&self) -> &ext_data_control::DataControlState {
        &self.extras.ext_data_control
    }
}
delegate_data_control!(Uwc);
delegate_ext_data_control!(Uwc);

impl XdgForeignHandler for Uwc {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.extras.foreign
    }
}
delegate_xdg_foreign!(Uwc);

impl XdgDialogHandler for Uwc {
    fn modal_changed(&mut self, _toplevel: ToplevelSurface, _is_modal: bool) {
        self.ipc.dirty = true;
    }
}
delegate_xdg_dialog!(Uwc);

impl KeyboardShortcutsInhibitHandler for Uwc {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.extras.inhibit
    }

    // A client that has the keyboard may take the shortcuts (a VM, a remote desktop). Mod+Shift+Escape
    // (`toggle-shortcuts-inhibit`) is the way out, and it keeps working while inhibited.
    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        if self.keyboard().current_focus().as_ref() == Some(inhibitor.wl_surface()) {
            inhibitor.activate();
        }
    }
}
delegate_keyboard_shortcuts_inhibit!(Uwc);

impl Uwc {
    /// Whether the focused surface holds the shortcuts (the compositor's key binds stand down).
    pub fn shortcuts_inhibited(&self) -> bool {
        use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
        self.seat.keyboard_shortcuts_inhibited()
    }

    /// The escape hatch: drop the focused surface's inhibitor.
    pub fn toggle_shortcuts_inhibit(&mut self) {
        use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
        let Some(focus) = self.keyboard().current_focus() else { return };
        if let Some(i) = self.seat.keyboard_shortcuts_inhibitor_for_surface(&focus) {
            if i.is_active() {
                i.inactivate();
            } else {
                i.activate();
            }
        }
    }
}

#[allow(dead_code)]
fn _assert_seat_handler<D: SeatHandler>(_: &KeyboardHandle<D>) {}

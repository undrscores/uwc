//! Protocols no crate has, generated from the XML in protocols/ with wayland-scanner (the same way
//! the wayland-protocols crates do it).

/// hyprland-toplevel-export-v1 (hyprwm/hyprland-protocols, BSD-3): capture one window. What
/// Quickshell's ScreencopyView uses for window previews.
pub mod hyprland_toplevel_export {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
    #![allow(missing_docs, clippy::all)]

    pub mod server {
        use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::*;
        use smithay::reexports::wayland_server;
        use smithay::reexports::wayland_server::protocol::*;

        pub mod __interfaces {
            use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::__interfaces::*;
            use smithay::reexports::wayland_server::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_server_code!("protocols/hyprland-toplevel-export-v1.xml");
    }
}

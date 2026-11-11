//! ext-background-effect-v1: a surface (a bar, through wlr-layer-shell) asks for the background
//! behind it to be blurred (the `blur` capability). The radius is uwc's own `blur-radius` option;
//! the client only says which part of its surface (`set_blur_region`, null = the whole surface).
//! uwc blurs that region's bounding box under the surface, the same way as a blurred window
//! (blur.rs), so bars and the like can be translucent with a real backdrop.

use smithay::{
    reexports::wayland_server::{
        protocol::wl_surface::WlSurface, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    },
    utils::{Logical, Rectangle},
    wayland::compositor::get_region_attributes,
};
use wayland_protocols::ext::background_effect::v1::server::{
    ext_background_effect_manager_v1::{self, Capability, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};

use crate::state::Uwc;

/// What one surface asked for: a key for blur.rs's per-surface textures, and the region to blur
/// (surface-local, logical px; `None` = the whole surface).
pub struct BgEffect {
    pub key: u32,
    pub region: Option<Rectangle<i32, Logical>>,
}

/// The effect object's user data: which surface it belongs to.
pub struct BgEffectSurfaceData {
    pub surface: WlSurface,
}

impl Uwc {
    /// The `ext_background_effect_manager_v1` global, hidden from sandboxed clients like the
    /// layer shell (only they have surfaces that could ask).
    pub fn bg_effect_new(dh: &DisplayHandle) {
        dh.create_global::<Uwc, ExtBackgroundEffectManagerV1, ()>(1, ());
    }

    /// The background effect of `surface`, if it asked for one.
    pub fn bg_effect_of(&self, surface: &WlSurface) -> Option<&BgEffect> {
        self.bg_effect.get(surface)
    }
}

impl GlobalDispatch<ExtBackgroundEffectManagerV1, ()> for Uwc {
    fn can_view(client: Client, _: &()) -> bool {
        !crate::sandbox::is_sandboxed(&client)
    }

    fn bind(
        _: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtBackgroundEffectManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        let mgr = init.init(resource, ());
        mgr.capabilities(Capability::Blur);
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        manager: &ExtBackgroundEffectManagerV1,
        request: ext_background_effect_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        match request {
            ext_background_effect_manager_v1::Request::Destroy => {}
            ext_background_effect_manager_v1::Request::GetBackgroundEffect { id, surface } => {
                if state.bg_effect.contains_key(&surface) {
                    manager.post_error(
                        ext_background_effect_manager_v1::Error::BackgroundEffectExists,
                        "the surface already has a background effect",
                    );
                    return;
                }
                let key = state.next_blur_key;
                state.next_blur_key += 1;
                state.bg_effect.insert(surface.clone(), BgEffect { key, region: None });
                init.init(id, BgEffectSurfaceData { surface });
                state.damage();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, BgEffectSurfaceData> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        _: &ExtBackgroundEffectSurfaceV1,
        request: ext_background_effect_surface_v1::Request,
        data: &BgEffectSurfaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        match request {
            ext_background_effect_surface_v1::Request::Destroy => {
                state.bg_effect.remove(&data.surface);
            }
            ext_background_effect_surface_v1::Request::SetBlurRegion { region } => {
                let bbox = region.as_ref().and_then(|r| {
                    let a = get_region_attributes(r);
                    a.rects.iter().map(|(_, r)| *r).reduce(|acc, x| {
                        let loc = (acc.loc.x.min(x.loc.x), acc.loc.y.min(x.loc.y)).into();
                        let end = (
                            (acc.loc.x + acc.size.w).max(x.loc.x + x.size.w),
                            (acc.loc.y + acc.size.h).max(x.loc.y + x.size.h),
                        );
                        Rectangle::new(loc, ((end.0 - loc.x).max(0), (end.1 - loc.y).max(0)).into())
                    })
                });
                if let Some(be) = state.bg_effect.get_mut(&data.surface) {
                    be.region = bbox;
                }
                state.damage();
            }
            _ => {}
        }
    }
}

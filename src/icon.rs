//! xdg-toplevel-icon-v1: a client can set its toplevel's icon, either a stock icon name or pixel
//! buffers (wl_buffer, usually shm, at one or more scales). uwc keeps what it can show: the name
//! goes into the window's IPC entry, and the largest shm buffer is written to
//! `$XDG_CACHE_HOME/uwc/icons/<window id>.pam` (RGBA, premultiplied, the way wl_shm carries it),
//! whose path is the window's `icon` in the IPC state — a bar or an overview strip can display it.
//! Dmabuf icon buffers are not decoded.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use smithay::{
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm},
    wayland::{compositor::with_states, shm::with_buffer_contents, xdg_toplevel_icon::ToplevelIconCachedState},
};

use crate::state::Uwc;

/// `$XDG_CACHE_HOME/uwc`, else `~/.cache/uwc`.
fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(d).join("uwc");
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".cache").join("uwc");
    }
    PathBuf::from("/tmp/uwc")
}

/// One icon buffer's pixels as RGBA plus a hash of them, if it is a readable shm buffer in a
/// format uwc knows. The closure runs while the pool is mapped; the pixels are copied out.
fn icon_pixels(buffer: &WlBuffer) -> Option<(i32, i32, Vec<u8>, u64)> {
    with_buffer_contents(buffer, |ptr, len, data| {
        let (w, h, stride, off) = (data.width, data.height, data.stride, data.offset);
        if w <= 0 || h <= 0 || stride < w * 4 || off < 0 {
            return None;
        }
        let start = off as usize;
        if start + (h as usize - 1) * stride as usize + w as usize * 4 > len {
            return None;
        }
        // Safety: the slice is only read inside this closure, before the pool is unmapped.
        let src = unsafe { std::slice::from_raw_parts(ptr.add(start), len - start) };
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let b = y * stride as usize + x * 4;
                if b + 4 > src.len() {
                    return None;
                }
                let px = match data.format {
                    // Little-endian 0xAARRGGBB / 0x00RRGGBB: bytes B, G, R, A/X.
                    wl_shm::Format::Argb8888 => [src[b + 2], src[b + 1], src[b], src[b + 3]],
                    wl_shm::Format::Xrgb8888 => [src[b + 2], src[b + 1], src[b], 255],
                    // Little-endian 0xAABBGGRR / 0x00BBGGRR: bytes R, G, B, A/X.
                    wl_shm::Format::Abgr8888 => [src[b], src[b + 1], src[b + 2], src[b + 3]],
                    wl_shm::Format::Xbgr8888 => [src[b], src[b + 1], src[b + 2], 255],
                    _ => return None,
                };
                rgba[(y * w as usize + x) * 4..][..4].copy_from_slice(&px);
            }
        }
        let mut hash = DefaultHasher::new();
        w.hash(&mut hash);
        h.hash(&mut hash);
        format!("{:?}", data.format).hash(&mut hash);
        rgba.hash(&mut hash);
        Some((w, h, rgba, hash.finish()))
    })
    .ok()
    .flatten()
}

impl Uwc {
    /// Read the icon the client last set on window `i`'s surface (xdg-toplevel-icon-v1) and keep
    /// it: the name on the window, the pixels as a PAM file under the cache dir when they changed.
    pub fn note_icon(&mut self, i: usize) {
        let Some(surface) = self.clients[i].surface() else { return };
        let st = with_states(&surface, |s| s.cached_state.get::<ToplevelIconCachedState>().current().clone());
        let name = st.icon_name().map(str::to_string);
        // Icons come in several scales; keep the largest.
        let buffer = st.buffers().iter().max_by_key(|(_, scale)| *scale).map(|(b, _)| b.clone());
        let extracted = buffer.as_ref().and_then(icon_pixels);
        let id = self.clients[i].id;
        let mut dirty = false;
        {
            let c = &mut self.clients[i];
            c.icon_name = name;
            match extracted {
                Some((w, h, rgba, hash)) if hash != c.icon_hash || c.icon_path.is_none() => {
                    let dir = cache_dir().join("icons");
                    if std::fs::create_dir_all(&dir).is_ok() {
                        let path = dir.join(format!("{id}.pam"));
                        let mut file =
                            format!("P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n")
                                .into_bytes();
                        file.extend_from_slice(&rgba);
                        if std::fs::write(&path, &file).is_ok() {
                            c.icon_path = Some(path);
                            c.icon_hash = hash;
                            dirty = true;
                        }
                    }
                }
                Some(_) => {}
                None => {
                    if c.icon_path.is_some() {
                        c.icon_path = None;
                        c.icon_hash = 0;
                        dirty = true;
                    }
                }
            }
        }
        if dirty {
            self.ipc.dirty = true;
        }
    }
}

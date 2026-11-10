//! Event hooks: run a command when something happens, so a desktop can be scripted without a bar or
//! a listener process (`uwc msg subscribe` still exists for anything richer).
//!
//! ```kdl
//! hooks {
//!     window-open  "notify-send opened \"$UWC_TITLE\""
//!     window-close "echo closed $UWC_APP_ID >> ~/.cache/uwc-closed"
//!     focus        "~/.config/uwc/on-focus"
//!     tag-change   "pkill -RTMIN+8 waybar"
//!     urgent       "canberra-gtk-play -i bell"
//! }
//! ```
//!
//! Each command runs through `sh -c` with the event in its environment: `UWC_EVENT`, and for
//! window events `UWC_WINDOW_ID`, `UWC_APP_ID`, `UWC_TITLE` and `UWC_TAGS` (a bit mask, tag 1 = 1);
//! `UWC_TAGSET` is the tags in view. Commands run in the background and must not rely on uwc waiting
//! for them.

use std::process::{Command, Stdio};

use crate::state::Uwc;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hooks {
    pub window_open: String,
    pub window_close: String,
    pub focus: String,
    pub tag_change: String,
    pub urgent: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hook {
    WindowOpen,
    WindowClose,
    Focus,
    TagChange,
    Urgent,
}

impl Hook {
    fn name(self) -> &'static str {
        match self {
            Hook::WindowOpen => "window-open",
            Hook::WindowClose => "window-close",
            Hook::Focus => "focus",
            Hook::TagChange => "tag-change",
            Hook::Urgent => "urgent",
        }
    }

    fn command(self, h: &Hooks) -> &str {
        match self {
            Hook::WindowOpen => &h.window_open,
            Hook::WindowClose => &h.window_close,
            Hook::Focus => &h.focus,
            Hook::TagChange => &h.tag_change,
            Hook::Urgent => &h.urgent,
        }
    }
}

impl Hooks {
    /// The `hooks { ... }` section: one command per event.
    pub fn parse(node: &crate::kdl::Node) -> anyhow::Result<Hooks> {
        let mut h = Hooks::default();
        for k in &node.kids {
            let cmd = k
                .args
                .first()
                .and_then(|a| a.as_str())
                .ok_or_else(|| anyhow::anyhow!("hook `{}` needs a command in quotes (line {})", k.name, k.line))?
                .to_string();
            match k.name.as_str() {
                "window-open" => h.window_open = cmd,
                "window-close" => h.window_close = cmd,
                "focus" => h.focus = cmd,
                "tag-change" => h.tag_change = cmd,
                "urgent" => h.urgent = cmd,
                other => anyhow::bail!(
                    "unknown hook `{other}` (line {}); known: window-open, window-close, focus, tag-change, urgent",
                    k.line
                ),
            }
        }
        Ok(h)
    }
}

impl Uwc {
    /// Run the command for `hook`, if one is set, with window `client` (an index) described in its
    /// environment.
    pub fn run_hook(&self, hook: Hook, client: Option<usize>) {
        let cmd = hook.command(&self.config.hooks);
        if cmd.is_empty() {
            return;
        }
        let mut c = Command::new("sh");
        c.args(["-c", cmd])
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("UWC_SOCKET", &self.ipc.path)
            .env("UWC_EVENT", hook.name())
            .env("UWC_TAGSET", self.tagset.to_string())
            .stdin(Stdio::null());
        if let Some(w) = client.and_then(|i| self.clients.get(i)) {
            c.env("UWC_WINDOW_ID", w.id.to_string())
                .env("UWC_APP_ID", w.app_id())
                .env("UWC_TITLE", w.title())
                .env("UWC_TAGS", w.tags.to_string());
        }
        match c.spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::warn!("hook {}: {e}", hook.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_hooks_and_rejects_the_rest() {
        let nodes = crate::kdl::parse("hooks {\n window-open \"a\"\n tag-change \"b\"\n}").unwrap();
        let h = Hooks::parse(&nodes[0]).unwrap();
        assert_eq!((h.window_open.as_str(), h.tag_change.as_str(), h.focus.as_str()), ("a", "b", ""));
        let bad = crate::kdl::parse("hooks { sunrise \"x\" }").unwrap();
        assert!(Hooks::parse(&bad[0]).unwrap_err().to_string().contains("unknown hook `sunrise`"));
        let none = crate::kdl::parse("hooks { focus }").unwrap();
        assert!(Hooks::parse(&none[0]).is_err());
    }
}

//! Frame pacing for clients: presentation-time (`wp_presentation`), `wp_fifo_v1` and `wp_commit_timing_v1`.
//!
//! - presentation-time: a client asks for feedback on a commit and is told when that content reached the screen
//!   (video players use it to pace and to sync audio, games to measure latency). The feedback is taken from the
//!   surfaces when a frame is rendered (`Uwc::take_feedback`) and answered when the frame is on screen
//!   (`Uwc::frame_presented`): at the vblank on the udev backend, straight away on the others, which have none.
//! - fifo: a commit can set a barrier and a later one wait for it. Every presented frame signals the barriers of
//!   every window (also hidden ones, which are not drawn but must not stall), so a client waiting on one runs at
//!   the refresh rate of the output it is on, no faster.
//! - commit timing: a commit can ask not to be applied before a time. It is applied by the first frame whose
//!   presentation lies at or after it; a timer wakes the loop for commits that wait while nothing is drawn.
//!
//! None of this is optional for a client that uses it: a client waiting on a fifo barrier that is never
//! signalled hangs, which is why the signalling lives here and not in smithay's examples.

use std::time::Duration;

use smithay::{
    delegate_commit_timing, delegate_fifo, delegate_presentation,
    desktop::utils::{take_presentation_feedback_surface_tree, with_surfaces_surface_tree, OutputPresentationFeedback},
    output::Output,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::{protocol::wl_surface::WlSurface, Client, Resource},
    },
    utils::{Clock, Monotonic, Time},
    wayland::{
        commit_timing::{CommitTimerBarrierStateUserData, Timestamp},
        compositor::{with_states, CompositorHandler},
        fifo::FifoBarrierCachedState,
        presentation::Refresh,
    },
};

use crate::state::Uwc;

/// The clock `wp_presentation` advertises and every timestamp here is on: CLOCK_MONOTONIC.
pub type PresentClock = Clock<Monotonic>;

/// The time one refresh of `output` takes (60 Hz when it has no mode).
pub fn refresh_period(output: &Output) -> Duration {
    let mhz = output.current_mode().map(|m| m.refresh).unwrap_or(60_000).max(1000);
    Duration::from_nanos(1_000_000_000_000 / mhz as u64)
}

impl Uwc {
    /// Every surface that takes part in pacing: windows (visible or not) and the layer surfaces of `output`
    /// (or of every output). Popups and cursor surfaces get frame callbacks but no pacing.
    fn pacing_roots(&self, output: Option<&Output>) -> Vec<WlSurface> {
        let mut roots: Vec<WlSurface> = self.clients.iter().filter_map(|c| c.surface()).collect();
        let outputs: Vec<Output> = match output {
            Some(o) => vec![o.clone()],
            None => self.monitors.iter().map(|m| m.output.clone()).collect(),
        };
        for o in outputs {
            let map = smithay::desktop::layer_map_for_output(&o);
            roots.extend(map.layers().map(|l| l.wl_surface().clone()));
        }
        roots
    }

    /// Collect the presentation feedback the visible surfaces committed, for the frame about to be shown on
    /// `output`. Dropping the result without presenting it tells the clients the content was discarded.
    pub fn take_feedback(&self, output: &Output) -> OutputPresentationFeedback {
        let mut fb = OutputPresentationFeedback::new(output);
        let primary = |_: &WlSurface, _: &smithay::wayland::compositor::SurfaceData| Some(output.clone());
        let flags =
            |_: &WlSurface, _: &smithay::wayland::compositor::SurfaceData| wp_presentation_feedback::Kind::empty();
        for c in self.clients.iter().filter(|c| self.visible(c)) {
            if let Some(s) = c.surface() {
                take_presentation_feedback_surface_tree(&s, &mut fb, primary, flags);
                for (popup, _) in smithay::desktop::PopupManager::popups_for_surface(&s) {
                    take_presentation_feedback_surface_tree(popup.wl_surface(), &mut fb, primary, flags);
                }
            }
        }
        let map = smithay::desktop::layer_map_for_output(output);
        for l in map.layers() {
            take_presentation_feedback_surface_tree(l.wl_surface(), &mut fb, primary, flags);
        }
        fb
    }

    /// The frame `feedback` was taken for is on screen `output` at `time` (CLOCK_MONOTONIC; `None` = now),
    /// as refresh number `seq`: answer the clients and let waiting commits through.
    pub fn frame_presented(
        &mut self,
        output: &Output,
        mut feedback: OutputPresentationFeedback,
        time: Option<Duration>,
        seq: u64,
        flags: wp_presentation_feedback::Kind,
    ) {
        let period = refresh_period(output);
        let time: Time<Monotonic> = time.map(Time::from).unwrap_or_else(|| self.present_clock.now());
        feedback.presented(time, Refresh::fixed(period), seq, flags);
        self.release_pacing(Some(output), period);
    }

    /// Signal fifo barriers and commit timers that this presentation satisfies; tell the clients whose commits
    /// were waiting. `period` is how far ahead of now the next frame is: commits timed for before then go.
    pub fn release_pacing(&mut self, output: Option<&Output>, period: Duration) {
        let deadline: Timestamp = (self.present_clock.now() + period).into();
        let mut cleared: Vec<Client> = Vec::new();
        for root in self.pacing_roots(output) {
            let mut signalled = false;
            with_surfaces_surface_tree(&root, |_, states| {
                let barrier = states.cached_state.get::<FifoBarrierCachedState>().current().barrier.take();
                if let Some(b) = barrier {
                    b.signal();
                    signalled = true;
                }
                if let Some(t) = states.data_map.get::<CommitTimerBarrierStateUserData>() {
                    signalled |= t.lock().unwrap().signal_until(deadline);
                }
            });
            if signalled {
                if let Some(c) = root.client() {
                    if !cleared.contains(&c) {
                        cleared.push(c);
                    }
                }
            }
        }
        let dh = self.display_handle.clone();
        for c in cleared {
            self.client_compositor_state(&c).blocker_cleared(self, &dh);
        }
        self.arm_commit_timer();
    }

    /// A timer for the earliest commit that waits for a time, so it is applied even when nothing else draws.
    fn arm_commit_timer(&mut self) {
        if self.commit_timer_armed {
            return;
        }
        let mut next: Option<Timestamp> = None;
        for root in self.pacing_roots(None) {
            with_surfaces_surface_tree(&root, |_, states| {
                if let Some(t) = states.data_map.get::<CommitTimerBarrierStateUserData>() {
                    if let Some(d) = t.lock().unwrap().next_deadline() {
                        next = Some(next.map_or(d, |n| n.min(d)));
                    }
                }
            });
        }
        let Some(next) = next else { return };
        let at: Time<Monotonic> = next.into();
        let wait = Duration::from(at).saturating_sub(Duration::from(self.present_clock.now()));
        self.commit_timer_armed = true;
        let _ = self.loop_handle.insert_source(Timer::from_duration(wait), |_, _, state| {
            state.commit_timer_armed = false;
            state.damage();
            state.release_pacing(None, Duration::from_millis(1));
            TimeoutAction::Drop
        });
    }

    /// After a commit: a hidden window is never presented, so a fifo barrier it sets would stall it for good.
    /// Release it on the next turn of the loop (not inside the commit, which would re-enter the client's
    /// commit handling).
    pub fn pace_hidden_window(&mut self, surface: &WlSurface) {
        let hidden = self.client_by_surface(surface).is_some_and(|i| !self.visible(&self.clients[i]));
        let waiting =
            with_states(surface, |s| s.cached_state.get::<FifoBarrierCachedState>().current().barrier.is_some());
        if hidden && waiting && !self.pace_idle {
            self.pace_idle = true;
            self.loop_handle.insert_idle(|state| {
                state.pace_idle = false;
                state.release_pacing(None, Duration::from_millis(1));
            });
        }
    }
}

delegate_presentation!(Uwc);
delegate_fifo!(Uwc);
delegate_commit_timing!(Uwc);

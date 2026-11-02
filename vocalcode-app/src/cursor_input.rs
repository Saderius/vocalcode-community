//! Cursor-native dictation: double-click an editable target to start or stop.
//!
//! The gesture is deliberately observed, never consumed. A low-level mouse hook
//! cannot see `WM_LBUTTONDBLCLK` — the target window's own class generates it —
//! so the click is delivered to the application before this code can act on it.
//! That is why a double-click still selects a word in the field, and why the
//! dot has to collapse that selection before text is delivered.
//!
//! Two rules keep this from firing where it should not:
//!
//! * the point probe must say the target under the pointer accepts text, and
//! * the engine must be ready and idle (or, for a stop, actually recording).
//!
//! Delivery is a third, independent gate inside the injector, so a wrong guess
//! here can only start a recording that ends up in History instead of a field.

use std::sync::Arc;
use std::time::Instant;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXDOUBLECLK, SM_CYDOUBLECLK,
};

// Declared rather than imported: `GetDoubleClickTime` lives behind the
// `Win32_UI_Input_KeyboardAndMouse` feature, which this crate does not enable
// and does not otherwise need.
#[link(name = "user32")]
unsafe extern "system" {
    fn GetDoubleClickTime() -> u32;
}

use crate::cursor_dot::CursorVisual;
use crate::dictation_control::{Action, Bridge};
use crate::overlay::{Phase, Snapshot};
use vocalcode_platform::{PointProbe, PointerClick};

/// A click pair older than this is two separate clicks.
fn double_click_time_ms() -> u32 {
    let value = unsafe { GetDoubleClickTime() };
    if value == 0 {
        500
    } else {
        value
    }
}

/// How far apart two clicks may land and still count as one gesture.
fn double_click_slop() -> (i32, i32) {
    let x = unsafe { GetSystemMetrics(SM_CXDOUBLECLK) };
    let y = unsafe { GetSystemMetrics(SM_CYDOUBLECLK) };
    (if x > 0 { x } else { 4 }, if y > 0 { y } else { 4 })
}

/// Whether two presses, in system ticks and screen coordinates, are one
/// double-click. Pulled out so the arithmetic is testable on its own, including
/// the 32-bit wrap Windows' mouse tick eventually performs.
fn is_double_click(previous: (u32, i32, i32), click: (u32, i32, i32)) -> bool {
    let (window, (slop_x, slop_y)) = (double_click_time_ms(), double_click_slop());
    let elapsed = click.0.wrapping_sub(previous.0);
    let near = (click.1 - previous.1).abs() <= slop_x && (click.2 - previous.2).abs() <= slop_y;
    elapsed <= window && near
}

/// Everything the controller needs from the desktop.
///
/// Exists so the gesture can be exercised without a mouse, a hook or a running
/// engine: the live implementation reads the real pointer, the test one is
/// scripted.
pub(crate) trait Desktop {
    fn position(&self) -> Option<(i32, i32)>;
    fn last_click(&self) -> Option<PointerClick>;
    fn aim(&self, x: i32, y: i32);
    fn armed(&self) -> bool;
    fn last_target(&self) -> Option<vocalcode_platform::inject::PinnedTarget>;
}

pub(crate) struct WindowsDesktop {
    probe: PointProbe,
}

impl Desktop for WindowsDesktop {
    fn position(&self) -> Option<(i32, i32)> {
        crate::cursor_dot::pointer_position()
    }
    fn last_click(&self) -> Option<PointerClick> {
        vocalcode_platform::last_left_click()
    }
    fn aim(&self, x: i32, y: i32) {
        self.probe.set_point(x, y);
    }
    fn armed(&self) -> bool {
        self.probe.armed()
    }
    fn last_target(&self) -> Option<vocalcode_platform::inject::PinnedTarget> {
        self.probe.last_target()
    }
}

pub struct CursorInput<D: Desktop = WindowsDesktop> {
    desktop: D,
    bridge: Arc<Bridge>,
    /// Last click sequence already considered, so one press is never counted
    /// twice by the frame loop.
    last_seq: u64,
    /// Previous click as `(system tick, x, y)`, the candidate first half of a
    /// double-click.
    last_click: Option<(u32, i32, i32)>,
    visual: CursorVisual,
    /// A field is pinned for the current utterance.
    session: bool,
    /// The session has actually been seen recording or transcribing, so a
    /// later idle really means it finished.
    seen_active: bool,
}

impl CursorInput<WindowsDesktop> {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self::with_desktop(
            WindowsDesktop {
                probe: PointProbe::new(),
            },
            bridge,
        )
    }
}

impl<D: Desktop> CursorInput<D> {
    pub(crate) fn with_desktop(desktop: D, bridge: Arc<Bridge>) -> Self {
        Self {
            desktop,
            bridge,
            last_seq: 0,
            last_click: None,
            visual: CursorVisual::Idle,
            session: false,
            seen_active: false,
        }
    }

    /// One frame. Returns what the dot should show.
    pub fn tick(
        &mut self,
        snapshot: Snapshot,
        ready: bool,
        recording: bool,
        enabled: bool,
        now: Instant,
    ) -> CursorVisual {
        if !enabled {
            self.last_click = None;
            self.visual = CursorVisual::Idle;
            vocalcode_platform::inject::clear_pinned_target();
            self.session = false;
            return self.visual;
        }
        // The utterance is over once the engine has stopped recording and
        // finished transcribing. Release the field then, so a later dictation
        // can never be captured by a stale pin.
        if self.session {
            if recording || snapshot.phase == Phase::Transcribing {
                self.seen_active = true;
            } else if self.seen_active {
                // The utterance really finished. Release the field so a later
                // dictation cannot be captured by a stale pin.
                vocalcode_platform::inject::clear_pinned_target();
                self.session = false;
                self.seen_active = false;
            }
        }
        if let Some((x, y)) = self.desktop.position() {
            self.desktop.aim(x, y);
        }
        self.consume_click(snapshot, ready, recording, now);
        let next = if recording {
            CursorVisual::Listening
        } else if snapshot.phase == Phase::Transcribing {
            CursorVisual::Transcribing
        } else if self.desktop.armed() {
            CursorVisual::Armed
        } else {
            CursorVisual::Idle
        };
        if next != self.visual {
            // One line per transition: this is the only way to see, from a log,
            // whether the probe agrees with the desktop.
            log::info!("cursor target: {:?} -> {:?}", self.visual, next);
            self.visual = next;
        }
        self.visual
    }

    /// The field rectangle the widget should sit on while a session is bound.
    ///
    /// `None` means the dot follows the pointer, which is the resting state.
    pub fn snap_rect(&self) -> Option<(i32, i32, i32, i32)> {
        if self.session {
            vocalcode_platform::inject::pinned_target_rect()
        } else {
            None
        }
    }

    fn consume_click(&mut self, snapshot: Snapshot, ready: bool, recording: bool, now: Instant) {
        let Some(click) = self.desktop.last_click() else {
            return;
        };
        if click.seq == self.last_seq {
            return;
        }
        self.last_seq = click.seq;
        #[cfg(debug_assertions)]
        log::info!(
            "cursor input: press seq={} at {},{} tick={}",
            click.seq,
            click.x,
            click.y,
            click.time_ms
        );

        let candidate = (click.time_ms, click.x, click.y);

        // Click away to commit. While recording, a click that lands anywhere
        // that is not a text target ends the utterance — the natural "I am done
        // with this field" gesture, and the one people already expect from
        // focus. A click *inside* the field is left alone: it is the user
        // moving the caret, or the first half of the double-click that stops.
        if recording && !self.desktop.armed() {
            self.last_click = None;
            self.toggle(snapshot, ready, recording, now);
            return;
        }

        if let Some(previous) = self.last_click {
            if is_double_click(previous, candidate) {
                // Consume the pair: a third click must start a fresh gesture
                // rather than toggle twice.
                self.last_click = None;
                self.toggle(snapshot, ready, recording, now);
                return;
            }
        }
        self.last_click = Some(candidate);
    }

    fn toggle(&mut self, snapshot: Snapshot, ready: bool, recording: bool, now: Instant) {
        let action = if recording {
            Action::Stop
        } else {
            Action::Start
        };
        if action == Action::Start && !self.desktop.armed() {
            // The click did not land on anything that takes text. Say nothing
            // and let the application have its double-click.
            log::info!("cursor input: double-click ignored, no text target");
            return;
        }
        match self.bridge.submit(action, snapshot, ready, recording, now) {
            Ok(()) => {
                log::info!("cursor input: {action:?} requested");
                if action == Action::Start {
                    // Bind the utterance to the field the user double-clicked.
                    // From here on the pointer may go anywhere; the words still
                    // belong to this field.
                    if let Some(target) = self.desktop.last_target() {
                        log::info!("cursor input: pinning the field under the click");
                        vocalcode_platform::inject::pin_target(target);
                        self.session = true;
                    } else {
                        log::info!("cursor input: no field to pin");
                    }
                    // The double-click selected a word before we saw it; put the
                    // caret after that word so dictation appends instead of
                    // replacing it. Not in tests: this would type into whatever
                    // window the test runner happens to have focused.
                    #[cfg(not(test))]
                    vocalcode_platform::inject::collapse_selection();
                }
            }
            Err(error) => log::info!("cursor input: {action:?} refused: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::Phase;
    use std::cell::Cell;
    use vocalcode_core::traits::TriggerEvent;
    use vocalcode_core::trigger_event_channel;

    /// A scripted desktop: the pointer, the last press and the arming answer are
    /// all set by the test.
    struct FakeDesktop {
        position: Cell<(i32, i32)>,
        armed: Cell<bool>,
        click: Cell<Option<PointerClick>>,
    }

    impl FakeDesktop {
        fn new(armed: bool) -> Self {
            Self {
                position: Cell::new((500, 400)),
                armed: Cell::new(armed),
                click: Cell::new(None),
            }
        }

        fn press(&self, seq: u64, at: (i32, i32), tick: u32) {
            self.position.set(at);
            self.click.set(Some(PointerClick {
                seq,
                x: at.0,
                y: at.1,
                time_ms: tick,
            }));
        }

        fn aim_at(&self, at: (i32, i32), armed: bool) {
            self.position.set(at);
            self.armed.set(armed);
        }
    }

    impl Desktop for FakeDesktop {
        fn position(&self) -> Option<(i32, i32)> {
            Some(self.position.get())
        }
        fn last_click(&self) -> Option<PointerClick> {
            self.click.get()
        }
        fn aim(&self, x: i32, y: i32) {
            self.position.set((x, y));
        }
        fn armed(&self) -> bool {
            self.armed.get()
        }
        fn last_target(&self) -> Option<vocalcode_platform::inject::PinnedTarget> {
            None
        }
    }

    fn snapshot(phase: Phase) -> Snapshot {
        Snapshot { revision: 1, phase }
    }

    /// Drive one frame and return whatever the controller asked the engine to
    /// do, if anything.
    fn drain(rx: &vocalcode_core::TriggerEventReceiver) -> Vec<TriggerEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn setup(
        armed: bool,
    ) -> (
        CursorInput<FakeDesktop>,
        Arc<Bridge>,
        vocalcode_core::TriggerEventReceiver,
    ) {
        let (tx, rx) = trigger_event_channel();
        let bridge = Arc::new(Bridge::default());
        bridge.connect(tx);
        let desktop = FakeDesktop::new(armed);
        let mut input = CursorInput::with_desktop(desktop, bridge.clone());
        // First frame establishes the baseline sequence; the controller treats
        // any press it has not seen as new.
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        (input, bridge, rx)
    }

    #[test]
    fn a_double_click_on_a_text_target_starts_hands_free_dictation() {
        let (mut input, bridge, rx) = setup(true);
        input.desktop.press(1, (500, 400), 1_000);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        input.desktop.press(2, (501, 401), 1_120);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());

        assert_eq!(drain(&rx), vec![TriggerEvent::Wake], "one wake per gesture");

        let dispatch = bridge
            .take(snapshot(Phase::Idle), true, false, true, Instant::now())
            .expect("the gesture must survive the mailbox");
        assert!(matches!(dispatch.event, TriggerEvent::HandsFreeStart(_)));
    }

    #[test]
    fn a_second_double_click_while_recording_stops_and_delivers() {
        let (mut input, bridge, rx) = setup(true);
        input.desktop.press(1, (500, 400), 2_000);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        input.desktop.press(2, (500, 400), 2_120);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        let _ = drain(&rx);
        let _ = bridge.take(snapshot(Phase::Idle), true, false, true, Instant::now());

        input.desktop.press(3, (500, 400), 5_000);
        input.tick(snapshot(Phase::Recording), true, true, true, Instant::now());
        input.desktop.press(4, (500, 400), 5_120);
        input.tick(snapshot(Phase::Recording), true, true, true, Instant::now());

        let dispatch = bridge
            .take(snapshot(Phase::Recording), true, true, true, Instant::now())
            .expect("stop must survive the mailbox");
        assert_eq!(dispatch.event, TriggerEvent::ForceStop);
    }

    #[test]
    fn a_double_click_off_a_text_target_does_nothing() {
        let (mut input, _bridge, rx) = setup(false);
        input.desktop.press(1, (500, 400), 3_000);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        input.desktop.press(2, (500, 400), 3_120);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        assert!(drain(&rx).is_empty(), "no wake may be sent");
    }

    #[test]
    fn two_slow_clicks_are_two_clicks_not_a_gesture() {
        let (mut input, _bridge, rx) = setup(true);
        let window = double_click_time_ms();
        input.desktop.press(1, (500, 400), 4_000);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        input.desktop.press(2, (500, 400), 4_000 + window + 50);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn one_press_is_never_counted_twice() {
        let (mut input, _bridge, rx) = setup(true);
        input.desktop.press(1, (500, 400), 6_000);
        for _ in 0..5 {
            input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        }
        assert!(drain(&rx).is_empty(), "a single press is only a candidate");
    }

    #[test]
    fn a_third_click_starts_a_new_gesture_rather_than_toggling_twice() {
        let (mut input, _bridge, rx) = setup(true);
        for (seq, tick) in [(1, 7_000), (2, 7_100)] {
            input.desktop.press(seq, (500, 400), tick);
            input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        }
        assert_eq!(drain(&rx).len(), 1, "the pair is one gesture");
        // A third click inside the window must not pair with the consumed one.
        input.desktop.press(3, (500, 400), 7_200);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn clicking_away_while_recording_stops_and_delivers() {
        let (mut input, bridge, rx) = setup(true);
        // Start the utterance with a double-click on the field.
        input.desktop.press(1, (500, 400), 9_000);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        input.desktop.press(2, (500, 400), 9_120);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        let _ = drain(&rx);
        let _ = bridge.take(snapshot(Phase::Idle), true, false, true, Instant::now());

        // A single click somewhere that takes no text ends it.
        input.desktop.aim_at((900, 700), false);
        input.desktop.press(3, (900, 700), 12_000);
        input.tick(snapshot(Phase::Recording), true, true, true, Instant::now());

        let dispatch = bridge
            .take(snapshot(Phase::Recording), true, true, true, Instant::now())
            .expect("click-away must stop the recording");
        assert_eq!(dispatch.event, TriggerEvent::ForceStop);
    }

    #[test]
    fn a_single_click_inside_the_field_does_not_stop_the_recording() {
        let (mut input, bridge, rx) = setup(true);
        input.desktop.press(1, (500, 400), 10_000);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        input.desktop.press(2, (500, 400), 10_120);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        let _ = drain(&rx);
        let _ = bridge.take(snapshot(Phase::Idle), true, false, true, Instant::now());

        // Repositioning the caret inside the field must be an ordinary click.
        input.desktop.press(3, (520, 410), 13_000);
        input.tick(snapshot(Phase::Recording), true, true, true, Instant::now());

        assert!(drain(&rx).is_empty(), "an inside click sends nothing");
        assert!(
            bridge
                .take(snapshot(Phase::Recording), true, true, true, Instant::now())
                .is_none(),
            "the mailbox must stay empty"
        );
    }

    #[test]
    fn the_gesture_arithmetic_handles_the_tick_wrap() {
        assert!(is_double_click((u32::MAX - 50, 10, 10), (120, 10, 10)));
        let (slop_x, _) = double_click_slop();
        assert!(!is_double_click(
            (1_000, 10, 10),
            (1_100, 10 + slop_x + 1, 10)
        ));
    }

    #[test]
    fn disabling_the_mode_resets_the_gesture() {
        let (mut input, _bridge, rx) = setup(true);
        input.desktop.press(1, (500, 400), 8_000);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        input.tick(snapshot(Phase::Idle), true, false, false, Instant::now());
        input.desktop.press(2, (500, 400), 8_100);
        input.tick(snapshot(Phase::Idle), true, false, true, Instant::now());
        assert!(drain(&rx).is_empty());
    }
}

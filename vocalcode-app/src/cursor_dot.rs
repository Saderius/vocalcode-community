//! The cursor dot: a tiny, click-through, always-on-top window that follows the
//! pointer and encodes dictation state.
//!
//! Deliberately **not** a WebView. A 16 px surface does not need a browser: a
//! layered window painted from a pre-multiplied 32-bit DIB gives a smooth
//! antialiased circle for a fraction of the memory, and it can be moved at
//! pointer cadence without waking a compositor or parsing a document. The
//! interactive capsule (`control_bar.rs`) stays a WebView because it has
//! buttons, text and focus behaviour; the dot has none of that.
//!
//! Two properties matter more than the look:
//!
//! * **It never takes a click or the focus.** `WS_EX_TRANSPARENT`,
//!   `WS_EX_NOACTIVATE` and `WS_EX_TOOLWINDOW` are set together, and the window
//!   is shown with `SW_SHOWNOACTIVATE`. The gesture lives on the text field, not
//!   on the dot, so the dot must be invisible to hit-testing.
//! * **It is positioned in physical pixels.** Windows sends `WM_DPICHANGED` and
//!   the DPI of the monitor under the pointer decides the dot's diameter; the
//!   window is re-asserted every frame because tao will otherwise resize it
//!   when the pointer crosses a monitor boundary.

use std::time::{Duration, Instant};

use tao::event_loop::EventLoopWindowTarget;
use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Window, WindowBuilder};
use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromPoint, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    UpdateLayeredWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE,
    ULW_ALPHA, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TRANSPARENT,
};

/// Diameter of the dot at 100 % scale, in logical pixels.
const DOT_LOGICAL: f64 = 14.0;
/// Gap between the pointer hotspot and the dot's top-left corner.
const OFFSET_LOGICAL: f64 = 10.0;
/// Pointer cadence while the dot is on. 30 Hz is smooth for a 14 px object and
/// costs one `SetWindowPos` per moved frame.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// What the dot is telling the user. Deliberately separate from
/// `overlay::Phase`, which is a 3-bit field that also gates control permissions.
///
/// The dictation states are constructed by the controller in `cursor_input.rs`;
/// only `Idle` is reachable until the target probe lands (M2).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorVisual {
    /// The pointer is not over a target that would accept text.
    Idle,
    /// The pointer is over an editable target; a double-click would dictate.
    Armed,
    Listening,
    Transcribing,
    Delivering,
    /// Elevated, secure or fullscreen target: show nothing at all.
    Blocked,
}

/// Straight-alpha colour used by the painter: `(r, g, b)` in 0..=255, alpha 0..=1.
type Rgba = (f32, f32, f32, f32);

impl CursorVisual {
    fn visible(self) -> bool {
        self != Self::Blocked
    }

    /// `(fill, rim)` for the disc.
    fn palette(self) -> (Rgba, Rgba) {
        match self {
            Self::Idle => ((58.0, 61.0, 68.0, 0.82), (139.0, 143.0, 152.0, 0.55)),
            Self::Armed => ((255.0, 175.0, 105.0, 0.95), (255.0, 232.0, 214.0, 0.90)),
            Self::Listening => ((255.0, 143.0, 63.0, 0.98), (255.0, 214.0, 180.0, 0.95)),
            Self::Transcribing => ((198.0, 200.0, 208.0, 0.95), (255.0, 255.0, 255.0, 0.75)),
            Self::Delivering => ((123.0, 216.0, 143.0, 0.98), (226.0, 255.0, 232.0, 0.85)),
            Self::Blocked => ((0.0, 0.0, 0.0, 0.0), (0.0, 0.0, 0.0, 0.0)),
        }
    }
}

pub struct CursorDot {
    window: Window,
    shown: bool,
    /// Last `(x, y, diameter)` pushed to the window, so a still pointer does not
    /// spam `SetWindowPos` 30 times a second.
    last_bounds: Option<(i32, i32, i32)>,
    /// Last visual actually uploaded, so the DIB is only rebuilt on change.
    last_visual: Option<CursorVisual>,
    diameter: i32,
}

impl CursorDot {
    pub fn new<T: 'static>(target: &EventLoopWindowTarget<T>) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title("VocalCode Cursor")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_focused(false)
            .with_focusable(false)
            .with_visible(false)
            .with_skip_taskbar(true)
            .with_undecorated_shadow(false)
            .with_inner_size(tao::dpi::LogicalSize::new(DOT_LOGICAL, DOT_LOGICAL))
            .build(target)?;
        let hwnd = window.hwnd() as HWND;
        apply_styles(hwnd);
        // If this fails the dot would swallow clicks, which is the one thing it
        // must never do, so treat it as a hard failure rather than a warning.
        window.set_ignore_cursor_events(true).map_err(|error| {
            anyhow::anyhow!("cursor dot could not become click-through: {error}")
        })?;
        Ok(Self {
            window,
            shown: false,
            last_bounds: None,
            last_visual: None,
            diameter: DOT_LOGICAL.round() as i32,
        })
    }

    /// Follow the pointer and paint `visual`. Returns whether the dot is on
    /// screen afterwards.
    pub fn tick(&mut self, visual: CursorVisual) -> bool {
        let Some((cursor_x, cursor_y)) = pointer_position() else {
            self.hide();
            return false;
        };
        if !visual.visible() {
            self.hide();
            return false;
        }
        let (work, scale) = self.screen_context(cursor_x, cursor_y);
        let diameter = (DOT_LOGICAL * scale).round().max(6.0) as i32;
        let offset = (OFFSET_LOGICAL * scale).round() as i32;
        // Keep the dot on the monitor the pointer is on. A dot half off the
        // edge is unreadable, and on the far right the offset would leave the
        // screen entirely.
        let x = (cursor_x + offset).clamp(work.0, work.2 - diameter);
        let y = (cursor_y + offset).clamp(work.1, work.3 - diameter);
        let bounds = (x, y, diameter);
        if self.last_bounds != Some(bounds) {
            self.diameter = diameter;
            let hwnd = self.window.hwnd() as HWND;
            unsafe {
                SetWindowPos(hwnd, HWND_TOPMOST, x, y, diameter, diameter, SWP_NOACTIVATE);
            }
            self.last_bounds = Some(bounds);
        }
        if self.last_visual != Some(visual) || !self.shown {
            if !self.paint(visual) {
                self.hide();
                return false;
            }
            self.last_visual = Some(visual);
        }
        if !self.shown {
            unsafe {
                ShowWindow(self.window.hwnd() as HWND, SW_SHOWNOACTIVATE);
            }
            // tao restores its own style bits while showing; put ours back.
            apply_styles(self.window.hwnd() as HWND);
            self.shown = true;
        }
        true
    }

    /// Work area as `(left, top, right, bottom)` and the DPI scale for `(x, y)`.
    fn screen_context(&self, x: i32, y: i32) -> ((i32, i32, i32, i32), f64) {
        unsafe {
            let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..MONITORINFO::default()
            };
            let work = if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
                (
                    info.rcWork.left,
                    info.rcWork.top,
                    info.rcWork.right,
                    info.rcWork.bottom,
                )
            } else {
                (0, 0, i32::MAX / 2, i32::MAX / 2)
            };
            let mut dx = 0;
            let mut dy = 0;
            let scale = if !monitor.is_null()
                && GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) >= 0
                && dx > 0
            {
                f64::from(dx) / 96.0
            } else {
                self.window.scale_factor()
            };
            (work, scale)
        }
    }

    fn hide(&mut self) {
        if self.shown {
            unsafe {
                ShowWindow(self.window.hwnd() as HWND, SW_HIDE);
            }
        }
        self.shown = false;
        self.last_bounds = None;
        self.last_visual = None;
    }

    /// Rebuild the circle and hand it to the compositor. `false` means the
    /// upload failed and the caller should stop trying to show the window.
    fn paint(&mut self, visual: CursorVisual) -> bool {
        let d = self.diameter.max(6);
        let pixels = render(visual, d);
        let hwnd = self.window.hwnd() as HWND;
        unsafe { upload(hwnd, &pixels, d) }
    }
}

/// Anti-aliased disc with a lighter rim, as pre-multiplied BGRA rows.
fn render(visual: CursorVisual, d: i32) -> Vec<u8> {
    let (fill, rim) = visual.palette();
    let radius = d as f32 / 2.0;
    let rim_width = (d as f32 / 10.0).clamp(0.75, 1.5);
    let mut out = vec![0u8; (d * d * 4) as usize];
    for y in 0..d {
        for x in 0..d {
            let px = x as f32 + 0.5 - radius;
            let py = y as f32 + 0.5 - radius;
            let distance = (px * px + py * py).sqrt();
            // One-pixel coverage ramp on the outer edge.
            let coverage = (radius - distance + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let t = ((distance - (radius - rim_width)) / rim_width).clamp(0.0, 1.0);
            let r = fill.0 + (rim.0 - fill.0) * t;
            let g = fill.1 + (rim.1 - fill.1) * t;
            let b = fill.2 + (rim.2 - fill.2) * t;
            let a = (fill.3 + (rim.3 - fill.3) * t) * coverage;
            let i = ((y * d + x) * 4) as usize;
            out[i] = (b * a).round() as u8;
            out[i + 1] = (g * a).round() as u8;
            out[i + 2] = (r * a).round() as u8;
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}

/// The pointer's position, asked of the window manager.
///
/// `vocalcode_platform::cursor_position()` is the hook's sample and is the
/// right source for attributing a *click* to a point. For following the
/// pointer it is the wrong tool: it only updates when the hook delivers an
/// event, so a programmatic `SetCursorPos` (and some remote-desktop paths)
/// leave it stale. `GetCursorPos` costs microseconds and is always current.
pub(crate) fn pointer_position() -> Option<(i32, i32)> {
    let mut point = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut point) } == 0 {
        None
    } else {
        Some((point.x, point.y))
    }
}

/// Re-assert the extended styles the dot depends on.
///
/// tao rewrites `GWL_EXSTYLE` when it shows a window — it restores the
/// `WS_EX_APPWINDOW` bit from its own bookkeeping — so this has to run again
/// after every show, not only at creation.
fn apply_styles(hwnd: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        // LAYERED is what makes per-pixel alpha possible; TRANSPARENT keeps the
        // dot out of hit-testing entirely; NOACTIVATE keeps focus where the user
        // put it; TOOLWINDOW keeps it out of Alt+Tab and the taskbar.
        let wanted = (style & !WS_EX_APPWINDOW)
            | WS_EX_LAYERED
            | WS_EX_TRANSPARENT
            | WS_EX_NOACTIVATE
            | WS_EX_TOOLWINDOW;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, wanted as isize);
    }
}

/// `UpdateLayeredWindow` with a top-down 32-bit DIB. Everything created here is
/// released before returning, including on the failure paths.
unsafe fn upload(hwnd: HWND, pixels: &[u8], d: i32) -> bool {
    let screen = GetDC(std::ptr::null_mut());
    if screen.is_null() {
        return false;
    }
    let memory = CreateCompatibleDC(screen);
    if memory.is_null() {
        ReleaseDC(std::ptr::null_mut(), screen);
        return false;
    }
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = d;
    // Negative height: top-down rows, matching `render`'s order.
    info.bmiHeader.biHeight = -d;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = 0; // BI_RGB
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let dib = CreateDIBSection(
        memory,
        &info,
        DIB_RGB_COLORS,
        &mut bits,
        std::ptr::null_mut(),
        0,
    );
    if dib.is_null() || bits.is_null() {
        if !dib.is_null() {
            DeleteObject(dib);
        }
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
        return false;
    }
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
    let old = SelectObject(memory, dib);
    let destination = POINT { x: 0, y: 0 };
    let size = SIZE { cx: d, cy: d };
    let source = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // The destination point is relative to the window's own position, so
    // (0, 0) plus `SetWindowPos` is all the positioning this needs.
    let ok = UpdateLayeredWindow(
        hwnd,
        screen,
        &destination,
        &size,
        memory,
        &source,
        0,
        &blend,
        ULW_ALPHA,
    ) != 0;
    SelectObject(memory, old);
    DeleteObject(dib);
    DeleteDC(memory);
    ReleaseDC(std::ptr::null_mut(), screen);
    ok
}

/// Frame cadence: the dot is a follower, so it always wants the next sample
/// while it is on.
pub fn next_wake(now: Instant) -> Instant {
    now + FRAME_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_armed_are_visible_but_blocked_is_not() {
        assert!(CursorVisual::Idle.visible());
        assert!(CursorVisual::Armed.visible());
        assert!(!CursorVisual::Blocked.visible());
    }

    #[test]
    fn the_disc_is_fully_transparent_outside_its_radius() {
        let d = 16;
        let pixels = render(CursorVisual::Idle, d);
        assert_eq!(pixels.len(), (d * d * 4) as usize);
        // Corners are outside the circle.
        for (x, y) in [(0, 0), (d - 1, 0), (0, d - 1), (d - 1, d - 1)] {
            let alpha = pixels[((y * d + x) * 4 + 3) as usize];
            assert_eq!(alpha, 0, "corner ({x},{y}) should be transparent");
        }
        // The centre is opaque and pre-multiplied (colour <= alpha).
        let centre = ((d / 2) * d + d / 2) * 4;
        let alpha = pixels[centre as usize + 3];
        assert!(alpha > 200, "centre alpha was {alpha}");
        for channel in 0..3 {
            assert!(pixels[centre as usize + channel] <= alpha);
        }
    }
}

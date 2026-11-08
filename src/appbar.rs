//! Docking the mini player to a screen edge, the way the taskbar is docked.
//!
//! On Windows the window registers as an application desktop toolbar
//! ("appbar", `SHAppBarMessage`): the shell takes its strip out of the work
//! area, so maximized windows, Snap and snap layouts all use the rest of the
//! screen. The strip is the window's width and the work area's height.
//! Elsewhere docking does nothing.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Which edge of the screen the mini player docks to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockEdge {
    #[default]
    Off,
    Left,
    Right,
}

impl DockEdge {
    pub fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

/// Whether this platform can dock windows to a screen edge.
pub const SUPPORTED: bool = cfg!(windows);

/// A rectangle in physical screen pixels: left, top, right, bottom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The narrowest a docked strip gets, in physical pixels.
pub const MIN_WIDTH: i32 = 120;

/// The strip `width` pixels wide on `edge` of `area`, as tall as `area`.
/// `area` is what the shell allows after other toolbars, such as a
/// taskbar on the same side, have their room. The strip never takes more
/// than half of `area`.
pub fn strip(area: Bounds, edge: DockEdge, width: i32) -> Bounds {
    let width = width
        .max(MIN_WIDTH)
        .min(((area.right - area.left) / 2).max(MIN_WIDTH));
    match edge {
        DockEdge::Right => Bounds {
            left: area.right - width,
            ..area
        },
        _ => Bounds {
            right: area.left + width,
            ..area
        },
    }
}

/// How long the window's width must hold still before the strip follows
/// it. Re-reserving the strip moves every other window, so doing it on each
/// frame of a resize makes the resize crawl and the window jump.
pub const SETTLE: Duration = Duration::from_millis(300);

/// What docking should do on this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Reserve the strip this many pixels wide and put the window in it.
    Place(i32),
    /// The window is being resized; leave it alone for now.
    Wait,
    /// The window is where it should be.
    Stay,
}

/// What the last placement asked for and got, and the width last seen.
#[derive(Clone, Copy, Debug)]
pub struct Settle {
    /// The edge, the area it was placed in, and the strip the window got.
    placed: Option<(DockEdge, Bounds, Bounds)>,
    seen_width: i32,
    seen_at: Instant,
}

impl Settle {
    pub fn new(now: Instant) -> Self {
        Self {
            placed: None,
            seen_width: 0,
            seen_at: now,
        }
    }

    pub fn placed(&mut self, edge: DockEdge, area: Bounds, got: Bounds) {
        self.placed = Some((edge, area, got));
    }

    /// Decides from where the window `visible`ly is now. A first dock or a
    /// new edge is placed at once; a new width waits until it settles, and
    /// nothing moves while the mouse button is `held` (a resize may still
    /// be under way); a window moved off its strip otherwise is put back.
    pub fn step(
        &mut self,
        now: Instant,
        edge: DockEdge,
        area: Bounds,
        visible: Bounds,
        held: bool,
    ) -> Step {
        let width = visible.right - visible.left;
        let Some((placed_edge, placed_area, got)) = self.placed else {
            self.seen_width = width;
            self.seen_at = now;
            return Step::Place(width);
        };
        if placed_edge != edge {
            return Step::Place(got.right - got.left);
        }
        if held {
            self.seen_at = now;
            return Step::Wait;
        }
        if width != self.seen_width {
            self.seen_width = width;
            self.seen_at = now;
            return Step::Wait;
        }
        if now.duration_since(self.seen_at) < SETTLE {
            return Step::Wait;
        }
        if placed_area == area && visible == got {
            Step::Stay
        } else {
            Step::Place(width)
        }
    }
}

/// The docked state of one window. Dropping it undocks.
#[derive(Default)]
pub struct Dock {
    /// The window the shell knows about, and its placement, when docked.
    #[cfg(windows)]
    docked: Option<(isize, Settle)>,
}

impl std::fmt::Debug for Dock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dock")
    }
}

impl Dock {
    pub fn is_docked(&self) -> bool {
        #[cfg(windows)]
        {
            self.docked.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Docks `hwnd` to `edge`, or undocks when `edge` is off or there is no
    /// window. Runs every frame: it follows the window's width once a
    /// resize settles, its monitor, and the work area (the taskbar moving),
    /// and puts the window back if anything else moved it.
    #[cfg_attr(not(windows), allow(unused_variables))]
    pub fn sync(&mut self, hwnd: Option<isize>, edge: DockEdge) {
        #[cfg(windows)]
        {
            let Some(hwnd) = hwnd.filter(|_| !edge.is_off()) else {
                self.release();
                return;
            };
            if let Some((docked, _)) = self.docked
                && docked != hwnd
            {
                self.release();
            }
            let (Some(area), Some(visible)) =
                (native::monitor_area(hwnd), native::visible_bounds(hwnd))
            else {
                return;
            };
            if self.docked.is_none() {
                if !native::register(hwnd) {
                    return;
                }
                self.docked = Some((hwnd, Settle::new(Instant::now())));
            }
            let Some((_, settle)) = self.docked.as_mut() else {
                return;
            };
            let held = native::mouse_held();
            if let Step::Place(width) = settle.step(Instant::now(), edge, area, visible, held) {
                let got = native::place(hwnd, edge, area, width);
                settle.placed(edge, area, got);
            }
        }
    }

    /// Gives the strip back to the other windows.
    pub fn release(&mut self) {
        #[cfg(windows)]
        if let Some((hwnd, _)) = self.docked.take() {
            native::unregister(hwnd);
        }
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(windows)]
mod native {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows_sys::Win32::UI::Shell::{
        ABE_LEFT, ABE_RIGHT, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA,
        SHAppBarMessage,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, RegisterWindowMessageW, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
    };

    use super::{Bounds, DockEdge, strip};

    fn data(hwnd: isize) -> APPBARDATA {
        APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: hwnd as _,
            ..Default::default()
        }
    }

    fn to_rect(b: Bounds) -> RECT {
        RECT {
            left: b.left,
            top: b.top,
            right: b.right,
            bottom: b.bottom,
        }
    }

    fn from_rect(r: RECT) -> Bounds {
        Bounds {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    /// The monitor the window is on, as tall as its work area (above or
    /// below a horizontal taskbar) and as wide as the whole monitor, so
    /// that the shell can place the strip beside a vertical taskbar.
    pub fn monitor_area(hwnd: isize) -> Option<Bounds> {
        // SAFETY: the window handle is this process's live window; the
        // info struct is sized as the call asks.
        unsafe {
            let monitor = MonitorFromWindow(hwnd as _, MONITOR_DEFAULTTONEAREST);
            if monitor.is_null() {
                return None;
            }
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                rcMonitor: RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                rcWork: RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                dwFlags: 0,
            };
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
            Some(Bounds {
                left: info.rcMonitor.left,
                right: info.rcMonitor.right,
                top: info.rcWork.top,
                bottom: info.rcWork.bottom,
            })
        }
    }

    fn window_rect(hwnd: isize) -> Option<Bounds> {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: our own window; a RECT to fill.
        (unsafe { GetWindowRect(hwnd as _, &mut rect) } != 0).then(|| from_rect(rect))
    }

    /// Where the window shows on screen. A resizable window's rectangle
    /// also takes in invisible resize borders, which would otherwise hold
    /// the window off the screen's edge.
    pub fn visible_bounds(hwnd: isize) -> Option<Bounds> {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: our own window; a RECT-sized buffer for a RECT attribute.
        let ok = unsafe {
            DwmGetWindowAttribute(
                hwnd as _,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&raw mut rect).cast(),
                std::mem::size_of::<RECT>() as u32,
            )
        } == 0;
        if ok && rect.right > rect.left {
            Some(from_rect(rect))
        } else {
            window_rect(hwnd)
        }
    }

    /// Whether the left mouse button is down anywhere, as during a resize.
    pub fn mouse_held() -> bool {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
        // SAFETY: reads the button's state; no pointers.
        (unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON)) } as u16 & 0x8000) != 0
    }

    pub fn register(hwnd: isize) -> bool {
        let name: Vec<u16> = "SpotifastAppBar\0".encode_utf16().collect();
        let mut data = data(hwnd);
        // SAFETY: a nul-terminated name; the data names our own window.
        unsafe {
            data.uCallbackMessage = RegisterWindowMessageW(name.as_ptr());
            SHAppBarMessage(ABM_NEW, &mut data) != 0
        }
    }

    pub fn unregister(hwnd: isize) {
        let mut data = data(hwnd);
        // SAFETY: as in `register`.
        unsafe {
            SHAppBarMessage(ABM_REMOVE, &mut data);
        }
    }

    /// Asks the shell for the strip, reserves it, and moves the window so
    /// that what shows of it fills the strip. Returns the strip it got.
    pub fn place(hwnd: isize, edge: DockEdge, area: Bounds, width: i32) -> Bounds {
        let wanted = strip(area, edge, width);
        let mut data = data(hwnd);
        data.uEdge = if edge == DockEdge::Right {
            ABE_RIGHT
        } else {
            ABE_LEFT
        };
        data.rc = to_rect(wanted);
        // The invisible borders around what shows, on each side.
        let (outer, shown) = (window_rect(hwnd), visible_bounds(hwnd));
        let border = match (outer, shown) {
            (Some(o), Some(v)) => Bounds {
                left: v.left - o.left,
                top: v.top - o.top,
                right: o.right - v.right,
                bottom: o.bottom - v.bottom,
            },
            _ => Bounds::default(),
        };
        // SAFETY: as in `register`; SetWindowPos moves our own window.
        unsafe {
            // The shell moves the proposal off other toolbars on that edge;
            // the width is then put back on the side away from the edge.
            SHAppBarMessage(ABM_QUERYPOS, &mut data);
            let mut granted = from_rect(data.rc);
            let span = wanted.right - wanted.left;
            match edge {
                DockEdge::Right => granted.left = granted.right - span,
                _ => granted.right = granted.left + span,
            }
            data.rc = to_rect(granted);
            SHAppBarMessage(ABM_SETPOS, &mut data);
            let got = from_rect(data.rc);
            SetWindowPos(
                hwnd as _,
                std::ptr::null_mut(),
                got.left - border.left,
                got.top - border.top,
                (got.right - got.left) + border.left + border.right,
                (got.bottom - got.top) + border.top + border.bottom,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            got
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Bounds = Bounds {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1392,
    };

    #[test]
    fn a_resize_waits_until_it_settles() {
        use std::time::Duration;
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let strip_at = |width: i32| strip(SCREEN, DockEdge::Right, width);
        let mut settle = Settle::new(start);

        // Docking places at once.
        assert_eq!(
            settle.step(at(0), DockEdge::Right, SCREEN, strip_at(400), false),
            Step::Place(400)
        );
        settle.placed(DockEdge::Right, SCREEN, strip_at(400));
        assert_eq!(
            settle.step(at(400), DockEdge::Right, SCREEN, strip_at(400), false),
            Step::Stay
        );

        // Dragging the inner edge: nothing is re-reserved mid-drag.
        let dragged = |width: i32| Bounds {
            left: SCREEN.right - width,
            ..SCREEN
        };
        for (ms, width) in [(500, 420), (516, 450), (532, 480), (548, 500)] {
            assert_eq!(
                settle.step(at(ms), DockEdge::Right, SCREEN, dragged(width), false),
                Step::Wait
            );
        }
        assert_eq!(
            settle.step(at(700), DockEdge::Right, SCREEN, dragged(500), false),
            Step::Wait
        );
        // The button is still down after a pause: still nothing.
        assert_eq!(
            settle.step(at(900), DockEdge::Right, SCREEN, dragged(500), true),
            Step::Wait
        );
        assert_eq!(
            settle.step(at(1000), DockEdge::Right, SCREEN, dragged(500), false),
            Step::Wait
        );
        // Let go and held still long enough: the strip follows, once.
        assert_eq!(
            settle.step(at(1300), DockEdge::Right, SCREEN, dragged(500), false),
            Step::Place(500)
        );
        settle.placed(DockEdge::Right, SCREEN, strip_at(500));
        assert_eq!(
            settle.step(at(1700), DockEdge::Right, SCREEN, strip_at(500), false),
            Step::Stay
        );
    }

    #[test]
    fn a_window_moved_off_its_strip_is_put_back() {
        use std::time::Duration;
        let start = Instant::now();
        let mut settle = Settle::new(start);
        let docked = strip(SCREEN, DockEdge::Left, 400);
        settle.step(start, DockEdge::Left, SCREEN, docked, false);
        settle.placed(DockEdge::Left, SCREEN, docked);
        // Restored to an old position, same width, a moment later.
        let moved = Bounds {
            left: 600,
            top: 120,
            right: 1000,
            bottom: 900,
        };
        let later = start + Duration::from_secs(1);
        assert_eq!(
            settle.step(later, DockEdge::Left, SCREEN, moved, false),
            Step::Place(400)
        );
        // A new edge moves it at once.
        assert_eq!(
            settle.step(later, DockEdge::Right, SCREEN, docked, false),
            Step::Place(400)
        );
    }

    #[test]
    fn a_strip_hugs_its_edge_at_full_height() {
        let left = strip(SCREEN, DockEdge::Left, 400);
        assert_eq!(
            left,
            Bounds {
                left: 0,
                top: 0,
                right: 400,
                bottom: 1392
            }
        );
        let right = strip(SCREEN, DockEdge::Right, 400);
        assert_eq!(
            right,
            Bounds {
                left: 2160,
                top: 0,
                right: 2560,
                bottom: 1392
            }
        );
    }

    #[test]
    fn a_strip_is_never_too_thin_or_more_than_half() {
        assert_eq!(strip(SCREEN, DockEdge::Left, 10).right, MIN_WIDTH);
        assert_eq!(strip(SCREEN, DockEdge::Left, 5000).right, 1280);
    }

    #[test]
    fn a_second_monitor_keeps_its_own_coordinates() {
        let second = Bounds {
            left: 2560,
            top: 0,
            right: 4480,
            bottom: 1040,
        };
        assert_eq!(strip(second, DockEdge::Right, 300).left, 4180);
        assert_eq!(strip(second, DockEdge::Left, 300).left, 2560);
    }
}

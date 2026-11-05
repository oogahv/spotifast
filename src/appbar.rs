//! Docking the mini player to a screen edge, the way the taskbar is docked.
//!
//! On Windows the window registers as an application desktop toolbar
//! ("appbar", `SHAppBarMessage`): the shell takes its strip out of the work
//! area, so maximized windows, Snap and snap layouts all use the rest of the
//! screen. The strip is the window's width and the work area's height.
//! Elsewhere docking does nothing.

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

/// The docked state of one window. Dropping it undocks.
#[derive(Default)]
pub struct Dock {
    /// The window, edge and strip the shell knows about, when docked.
    #[cfg(windows)]
    docked: Option<(isize, DockEdge, Bounds)>,
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

    /// Docks `hwnd` to `edge`, `width` physical pixels wide, or undocks when
    /// `edge` is off or there is no window. Cheap when nothing changed, so it
    /// can run every frame; it also follows the window to another monitor
    /// and a change in the work area, such as the taskbar moving.
    #[cfg_attr(not(windows), allow(unused_variables))]
    pub fn sync(&mut self, hwnd: Option<isize>, edge: DockEdge, width: i32) {
        #[cfg(windows)]
        {
            let Some(hwnd) = hwnd.filter(|_| !edge.is_off()) else {
                self.release();
                return;
            };
            if let Some((docked, _, _)) = self.docked
                && docked != hwnd
            {
                self.release();
            }
            let Some(area) = native::monitor_area(hwnd) else {
                return;
            };
            if self.docked.is_none() {
                if !native::register(hwnd) {
                    return;
                }
                self.docked = Some((hwnd, DockEdge::Off, Bounds::default()));
            }
            let wanted = strip(area, edge, width);
            let Some((_, docked_edge, docked_strip)) = self.docked else {
                return;
            };
            if docked_edge == edge && docked_strip == wanted {
                return;
            }
            let placed = native::place(hwnd, edge, area, width);
            self.docked = Some((hwnd, edge, placed));
        }
    }

    /// Gives the strip back to the other windows.
    pub fn release(&mut self) {
        #[cfg(windows)]
        if let Some((hwnd, _, _)) = self.docked.take() {
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
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows_sys::Win32::UI::Shell::{
        ABE_LEFT, ABE_RIGHT, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA,
        SHAppBarMessage,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        RegisterWindowMessageW, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
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

    /// Asks the shell for the strip, reserves it, and moves the window
    /// into it. Returns the strip as asked for, which `sync` compares
    /// against to tell whether anything changed.
    pub fn place(hwnd: isize, edge: DockEdge, area: Bounds, width: i32) -> Bounds {
        let wanted = strip(area, edge, width);
        let mut data = data(hwnd);
        data.uEdge = if edge == DockEdge::Right {
            ABE_RIGHT
        } else {
            ABE_LEFT
        };
        data.rc = to_rect(wanted);
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
            let placed = from_rect(data.rc);
            SetWindowPos(
                hwnd as _,
                std::ptr::null_mut(),
                placed.left,
                placed.top,
                placed.right - placed.left,
                placed.bottom - placed.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        wanted
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

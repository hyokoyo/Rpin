use std::collections::HashSet;
use std::mem::{size_of, transmute};
use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GW_HWNDNEXT, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetCursorPos, GetDesktopWindow,
    GetShellWindow, GetSystemMetrics, GetTopWindow, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SM_CXDRAG, SM_CYDRAG,
    SYSTEM_METRICS_INDEX, WS_EX_LAYERED, WS_EX_TRANSPARENT, WindowFromPoint,
};
use windows::core::{s, w};

use crate::capture::frame::{Point, Rect};

const MAX_WINDOW_HANDLES: usize = 4096;

/// Window geometry frozen alongside the desktop pixels, in front-to-back order.
#[derive(Default)]
pub(crate) struct WindowTargets {
    initial_hit: Option<(Point, Option<Rect>)>,
    rects: Vec<Rect>,
}

impl WindowTargets {
    pub(crate) fn snapshot() -> Self {
        // Visible pins are hidden and the overlay has not yet been shown.
        // SAFETY: these queries run on the per-monitor-aware UI thread. All
        // output buffers are valid and window handles are used only for queries.
        unsafe {
            let desktop = GetDesktopWindow();
            let shell = GetShellWindow();
            let own_process = std::process::id();
            let mut cursor = POINT::default();
            let initial = GetCursorPos(&mut cursor).ok().map(|()| {
                (
                    Point::new(cursor.x, cursor.y),
                    GetAncestor(WindowFromPoint(cursor), GA_ROOT),
                )
            });
            let mut initial_rect = None;
            let mut rects = Vec::new();
            let mut seen = HashSet::new();
            let mut next = GetTopWindow(None);
            while let Ok(hwnd) = next {
                // Z order can change while it is being read. Bound the traversal
                // and stop if a handle reappears.
                if seen.len() == MAX_WINDOW_HANDLES || !seen.insert(hwnd.0) {
                    break;
                }
                next = GetWindow(hwnd, GW_HWNDNEXT);
                if let Some(rect) = selectable_rect(hwnd, desktop, shell, own_process) {
                    rects.push(rect);
                    if initial.is_some_and(|(_, hit)| hit == hwnd) {
                        initial_rect = Some(rect);
                    }
                }
            }
            let initial_hit = initial.map(|(point, hwnd)| {
                let rect = if seen.contains(&hwnd.0) {
                    initial_rect
                } else {
                    selectable_rect(hwnd, desktop, shell, own_process)
                };
                (point, rect.filter(|rect| rect.contains(point)))
            });
            Self { initial_hit, rects }
        }
    }

    pub(crate) fn clipped(mut self, bounds: Rect) -> Self {
        if let Some((point, rect)) = &mut self.initial_hit {
            *rect = rect
                .and_then(|rect| rect.intersect(bounds))
                .filter(|rect| rect.contains(*point));
        }
        self.rects.retain_mut(|rect| {
            let Some(clipped) = rect.intersect(bounds) else {
                return false;
            };
            *rect = clipped;
            true
        });
        self
    }

    pub(crate) fn at(&self, point: Point) -> Option<Rect> {
        // Win32's hit test also accounts for controls and window hit regions at
        // the initial point. Hover over the frozen desktop only tests rectangles.
        if let Some((_, rect)) = self.initial_hit.filter(|(initial, _)| *initial == point) {
            return rect;
        }
        self.rects.iter().copied().find(|rect| rect.contains(point))
    }

    #[cfg(test)]
    pub(crate) fn from_rects(rects: Vec<Rect>) -> Self {
        Self {
            initial_hit: None,
            rects,
        }
    }
}

/// This query may race a window closing. A failed query drops that candidate.
unsafe fn selectable_rect(
    hwnd: HWND,
    desktop: HWND,
    shell: HWND,
    own_process: u32,
) -> Option<Rect> {
    // SAFETY: handles are queried by value and all outputs are writable.
    unsafe {
        if hwnd.is_invalid()
            || hwnd == desktop
            || hwnd == shell
            || !IsWindowVisible(hwnd).as_bool()
            || IsIconic(hwnd).as_bool()
        {
            return None;
        }
        let mut process = 0;
        if GetWindowThreadProcessId(hwnd, Some(&mut process)) == 0 || process == own_process {
            return None;
        }
        // Windows routes mouse input through layered windows with both styles.
        let click_through = WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0;
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & click_through == click_through {
            return None;
        }
        let mut class = [0; 64];
        let len = GetClassNameW(hwnd, &mut class);
        if len == 0
            || [
                "Shell_TrayWnd",
                "Shell_SecondaryTrayWnd",
                "Progman",
                "WorkerW",
            ]
            .iter()
            .any(|name| {
                class[..len as usize]
                    .iter()
                    .copied()
                    .eq(name.encode_utf16())
            })
        {
            return None;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return None;
        }
        let mut bounds = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut bounds).cast(),
            size_of::<RECT>() as u32,
        )
        .is_err()
        {
            // The caller is per-monitor DPI aware, so the fallback is in the
            // same desktop coordinate space as the capture.
            GetWindowRect(hwnd, &mut bounds).ok()?;
        }
        let rect = Rect::new(bounds.left, bounds.top, bounds.right, bounds.bottom);
        (!rect.is_empty()).then_some(rect)
    }
}

pub(crate) fn drag_threshold(dpi: u32) -> Point {
    type MetricsForDpi = unsafe extern "system" fn(SYSTEM_METRICS_INDEX, u32) -> i32;
    static METRICS_FOR_DPI: OnceLock<Option<MetricsForDpi>> = OnceLock::new();
    let metrics = METRICS_FOR_DPI.get_or_init(|| {
        // SAFETY: user32 is already loaded. This named export has the declared
        // signature when present (Windows 10 version 1607 and later).
        unsafe {
            let user32 = GetModuleHandleW(w!("user32.dll")).ok()?;
            GetProcAddress(user32, s!("GetSystemMetricsForDpi"))
                .map(|proc| transmute::<_, MetricsForDpi>(proc))
        }
    });
    let pixels = |index| {
        // SAFETY: the caller supplies one of the two valid drag metrics below.
        let metric =
            unsafe { metrics.map_or_else(|| GetSystemMetrics(index), |get| get(index, dpi)) };
        match metric.saturating_abs() {
            0 => 4,
            pixels => pixels,
        }
    };
    Point::new(pixels(SM_CXDRAG), pixels(SM_CYDRAG))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_drag_thresholds_are_positive() {
        for dpi in [96, 144] {
            let threshold = drag_threshold(dpi);
            assert!(threshold.x > 0 && threshold.y > 0);
        }
    }

    #[test]
    fn window_targets_keep_front_to_back_order_and_clip_at_desktop_edges() {
        let front = Rect::new(-20, -5, 12, 20);
        let behind = Rect::new(-15, 5, 30, 30);
        let edge = Rect::new(-16, 0, 12, 20);
        let initial = Point::new(-2, 8);
        let targets = WindowTargets {
            initial_hit: Some((initial, Some(behind))),
            rects: vec![front, behind, Rect::new(40, 10, 50, 40)],
        }
        .clipped(Rect::new(-16, 0, 32, 32));

        assert_eq!(targets.at(initial), Some(behind));
        assert_eq!(targets.at(Point::new(-1, 8)), Some(edge));
        assert_eq!(targets.at(Point::new(12, 8)), Some(behind));
        assert_eq!(targets.at(Point::new(42, 12)), None);
    }
}

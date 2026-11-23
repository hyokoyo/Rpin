//! Native layered screenshot windows and their shared pixel budget.

mod layout;
mod surface;
mod window;

use std::{cell::Cell, rc::Rc};

use anyhow::ensure;
use windows::Win32::{Foundation::HWND, Graphics::Dwm::DwmFlush};

use crate::{
    capture::frame::{Frame, Point},
    config::PinsConfig,
};

use window::PinWindow;

const MAX_OPEN_PINS: usize = 16;
const PIXEL_BUDGET: usize = 192 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinId(u64);

pub struct PinSummary {
    pub id: PinId,
    pub image_width: u32,
    pub image_height: u32,
    pub visible: bool,
}

impl PinSummary {
    pub fn label(&self) -> String {
        let hidden = if self.visible { "" } else { "（已隐藏）" };
        format!(
            "贴图 {} · {} × {}{hidden}",
            self.id.0, self.image_width, self.image_height
        )
    }
}

pub struct NativePinManager {
    owner: HWND,
    windows: Vec<PinWindow>,
    next_id: u64,
    pixels: Rc<Cell<usize>>,
}

impl NativePinManager {
    pub fn new(owner: HWND) -> Self {
        Self {
            owner,
            windows: Vec::new(),
            next_id: 1,
            pixels: Rc::new(Cell::new(0)),
        }
    }

    pub fn show(&mut self, frame: Frame, at: Point, config: &PinsConfig) -> anyhow::Result<PinId> {
        self.windows.retain(PinWindow::is_alive);
        ensure!(
            self.windows.len() < MAX_OPEN_PINS,
            "已达到 {MAX_OPEN_PINS} 张贴图的上限"
        );
        let id = PinId(self.next_id);
        let pin = PinWindow::create(self.owner, id, frame, at, config, &self.pixels)?;
        self.next_id += 1;
        self.windows.push(pin);
        Ok(id)
    }

    pub fn summaries(&self) -> Vec<PinSummary> {
        self.windows.iter().filter_map(PinWindow::summary).collect()
    }

    pub fn show_existing(&self, id: PinId) -> anyhow::Result<()> {
        if let Some(pin) = self
            .windows
            .iter()
            .find(|pin| pin.id == id && pin.is_alive())
        {
            pin.reveal()?;
        }
        Ok(())
    }

    pub fn hide_all_for_capture(&mut self) {
        let mut any_visible = false;
        for pin in &mut self.windows {
            any_visible |= pin.hide_for_capture();
        }
        if any_visible {
            // SAFETY: wait for the compositor to apply the visibility changes.
            if let Err(err) = unsafe { DwmFlush() } {
                crate::logging::warn(format!("DwmFlush after hiding pins: {err}"));
            }
        }
    }

    pub fn restore_after_capture(&mut self) {
        for pin in &mut self.windows {
            pin.restore_after_capture();
        }
    }

    pub fn close_all(&mut self) {
        self.windows.clear();
    }
}

/// Charges the original pixels and last presented surface against a shared
/// budget. A resize reserves its growth before calling Windows and rolls back
/// on failure. The temporary upload bitmap is separate and scoped to the call.
struct Reservation {
    total: Rc<Cell<usize>>,
    bytes: usize,
}

impl Reservation {
    fn reserve(total: &Rc<Cell<usize>>, bytes: usize) -> anyhow::Result<Self> {
        let mut reservation = Self {
            total: Rc::clone(total),
            bytes: 0,
        };
        reservation.set(bytes)?;
        Ok(reservation)
    }

    fn set(&mut self, bytes: usize) -> anyhow::Result<()> {
        let next = (self.total.get() - self.bytes).saturating_add(bytes);
        ensure!(
            next <= PIXEL_BUDGET,
            "贴图像素内存已达上限，请关闭部分贴图后重试"
        );
        self.total.set(next);
        self.bytes = bytes;
        Ok(())
    }

    fn present(
        &mut self,
        bytes: usize,
        upload: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let previous = self.bytes;
        if bytes > previous {
            self.set(bytes)?;
        }
        if let Err(err) = upload() {
            self.release_to(previous);
            return Err(err);
        }
        if bytes < previous {
            self.release_to(bytes);
        }
        Ok(())
    }

    fn release_to(&mut self, bytes: usize) {
        self.total.set(self.total.get() - (self.bytes - bytes));
        self.bytes = bytes;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.release_to(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_pin_budget_on_failure_resize_and_close() {
        let budget = Rc::new(Cell::new(0));
        let mut first = Reservation::reserve(&budget, PIXEL_BUDGET - 10).unwrap();
        assert!(Reservation::reserve(&budget, 11).is_err());
        let mut last = Reservation::reserve(&budget, 10).unwrap();
        first.present(40, || Ok(())).unwrap();
        assert_eq!(budget.get(), 50);
        assert!(last.present(50, || anyhow::bail!("upload failed")).is_err());
        assert_eq!(budget.get(), 50);
        drop(first);
        assert_eq!(budget.get(), 10);
        drop(last);
        assert_eq!(budget.get(), 0);
    }
}

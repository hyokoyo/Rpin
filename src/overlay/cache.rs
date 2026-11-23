use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1CreateFactory, ID2D1Factory,
};
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer, USER_TIMER_MAXIMUM};

use crate::logging::{self, Level};
use crate::platform::process::log_memory;

// This timer belongs to the main window, not the selection window.
pub(crate) const DIRECT2D_IDLE_TIMER: usize = 1;

pub(crate) struct Direct2dCache {
    hwnd: HWND,
    factory: Option<ID2D1Factory>,
    idle_since: Option<Instant>,
}

impl Direct2dCache {
    pub(crate) fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            factory: None,
            idle_since: None,
        }
    }

    pub(crate) fn factory(&mut self) -> windows::core::Result<ID2D1Factory> {
        self.cancel_timer();
        if let Some(factory) = &self.factory {
            return Ok(factory.clone());
        }
        // SAFETY: the factory is created and used exclusively on the UI thread.
        let factory =
            unsafe { D2D1CreateFactory::<ID2D1Factory>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        self.factory = Some(factory.clone());
        Ok(factory)
    }

    pub(crate) fn start_idle(&mut self, seconds: u32) {
        if self.factory.is_some() {
            self.idle_since.get_or_insert_with(Instant::now);
            self.update_timer(seconds);
        }
    }

    pub(crate) fn update_timer(&mut self, seconds: u32) {
        let Some(since) = self.idle_since else {
            return;
        };
        // KillTimer can leave a queued WM_TIMER. Check the current idle period
        // so a stale message never shortens the lifetime of a reused factory.
        let remaining = Duration::from_secs(u64::from(seconds)).saturating_sub(since.elapsed());
        if !remaining.is_zero() {
            let millis = (remaining.as_millis() + 1).min(u128::from(USER_TIMER_MAXIMUM)) as u32;
            // SAFETY: hwnd and its timer are owned by this UI thread.
            if unsafe { SetTimer(Some(self.hwnd), DIRECT2D_IDLE_TIMER, millis, None) } != 0 {
                return;
            }
            logging::warn("Direct2D idle timer could not be set; releasing the idle factory");
        }
        self.clear();
        log_memory("after releasing idle Direct2D factory", Level::Debug);
    }

    pub(crate) fn clear(&mut self) {
        self.cancel_timer();
        self.factory = None;
    }

    fn cancel_timer(&mut self) {
        if self.idle_since.take().is_some() {
            // SAFETY: cancels only the timer created for this window.
            let _ = unsafe { KillTimer(Some(self.hwnd), DIRECT2D_IDLE_TIMER) };
        }
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

    use super::*;

    #[test]
    fn idle_deadline_is_checked_after_reuse_and_config_changes() -> anyhow::Result<()> {
        let hwnd = crate::platform::window::create_hidden_window()?;
        let mut cache = Direct2dCache::new(hwnd);
        let result = (|| {
            let first = cache.factory()?;
            cache.start_idle(60);
            assert_eq!(cache.factory()?, first);

            // A queued timer cannot evict the factory during the next selection.
            cache.update_timer(0);
            assert!(cache.factory.is_some());
            drop(first);

            cache.start_idle(60);
            cache.idle_since = Some(Instant::now() - Duration::from_secs(10));
            cache.update_timer(60);
            assert!(cache.factory.is_some());

            // Reloading a shorter timeout uses the existing idle period.
            cache.update_timer(5);
            assert!(cache.factory.is_none());
            assert!(cache.idle_since.is_none());

            drop(cache.factory()?);
            cache.start_idle(0);
            assert!(cache.factory.is_none());
            Ok(())
        })();
        cache.clear();
        // SAFETY: this test owns the hidden window and has canceled its timer.
        unsafe { DestroyWindow(hwnd)? };
        result
    }
}

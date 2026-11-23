use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

/// Initializes COM on the UI thread. Drop calls `CoUninitialize` exactly once.
pub struct ComGuard {
    active: bool,
}

impl ComGuard {
    pub fn init() -> Self {
        // SAFETY: called on the main thread before any COM object is created.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_ok() {
            crate::logging::debug("COM initialized (STA)");
            Self { active: true }
        } else {
            crate::logging::warn(format!(
                "CoInitializeEx failed: 0x{:08X}; continuing without COM",
                hr.0
            ));
            Self { active: false }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: matches the successful CoInitializeEx in `init`.
            unsafe { CoUninitialize() };
            self.active = false;
        }
    }
}

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, MENU_ITEM_FLAGS, MF_POPUP, MF_SEPARATOR,
    MF_STRING, PostMessageW, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    WM_NULL,
};
use windows::core::{PCWSTR, Result};

pub struct PopupMenu(HMENU);

impl PopupMenu {
    pub fn new() -> Result<Self> {
        // SAFETY: the returned menu is owned by the caller.
        unsafe { CreatePopupMenu().map(Self) }
    }

    pub fn item(&self, id: usize, text: &str, flags: MENU_ITEM_FLAGS) -> Result<()> {
        let text: Vec<_> = text.encode_utf16().chain([0]).collect();
        // SAFETY: the menu is live and AppendMenuW copies the label.
        unsafe { AppendMenuW(self.0, MF_STRING | flags, id, PCWSTR(text.as_ptr())) }
    }

    pub fn submenu(&self, menu: Self, text: &str) -> Result<()> {
        self.item(menu.0.0 as usize, text, MF_POPUP)?;
        // DestroyMenu on the parent also destroys its attached submenus.
        std::mem::forget(menu);
        Ok(())
    }

    pub fn separator(&self) -> Result<()> {
        // SAFETY: there is no label or command associated with a separator.
        unsafe { AppendMenuW(self.0, MF_SEPARATOR, 0, PCWSTR::null()) }
    }

    pub fn track(&self, owner: HWND, point: POINT) -> usize {
        // SAFETY: owner is a UI-thread window. The popup runs a nested message loop.
        unsafe {
            let _ = SetForegroundWindow(owner);
            let command = TrackPopupMenu(
                self.0,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                owner,
                None,
            );
            let _ = PostMessageW(Some(owner), WM_NULL, Default::default(), Default::default());
            command.0 as usize
        }
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        // SAFETY: this guard owns the menu and its attached submenus.
        let _ = unsafe { DestroyMenu(self.0) };
    }
}

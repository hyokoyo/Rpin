use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, HICON, IDI_APPLICATION, LoadIconW, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED,
};
use windows::core::{Error, PCWSTR, w};

use crate::app::Command;
use crate::pins::PinSummary;

use super::{menu::PopupMenu, window::hinstance};

const TRAY_ID: u32 = 1;
const TRAY_TIP: &str = "RPin";

const MENU_REGION: usize = 1001;
const MENU_FULLSCREEN: usize = 1002;
const MENU_OPEN_CONFIG: usize = 1003;
const MENU_RELOAD_CONFIG: usize = 1004;
const MENU_AUTOSTART: usize = 1005;
const MENU_EXIT: usize = 1006;
const MENU_FULLSCREEN_SAVE: usize = 1007;
const MENU_NEW_PIN: usize = 1008;
const MENU_CLOSE_PINS: usize = 1009;
const MENU_PIN_BASE: usize = 2000;

pub struct TrayMenuState {
    pub autostart: bool,
    pub region_hint: String,
    pub fullscreen_hint: String,
    pub fullscreen_save_hint: String,
    pub pin_hint: String,
    pub pins: Vec<PinSummary>,
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    added: bool,
}

impl Tray {
    #[cfg(test)]
    pub(crate) fn inactive(hwnd: HWND) -> Self {
        Self {
            hwnd,
            icon: HICON::default(),
            added: false,
        }
    }

    pub fn new(hwnd: HWND) -> anyhow::Result<Self> {
        let icon = load_app_icon();
        let mut tray = Self {
            hwnd,
            icon,
            added: false,
        };
        if let Err(err) = tray.add() {
            // SAFETY: the shell taskbar class name is static. Explorer can start
            // after an HKCU Run application and will broadcast TaskbarCreated.
            if unsafe {
                windows::Win32::UI::WindowsAndMessaging::FindWindowW(
                    w!("Shell_TrayWnd"),
                    PCWSTR::null(),
                )
            }
            .is_ok()
            {
                return Err(err);
            }
            crate::logging::warn("taskbar unavailable; waiting for TaskbarCreated");
        }
        Ok(tray)
    }

    pub fn add(&mut self) -> anyhow::Result<()> {
        let mut data = self.base_data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = crate::app::WM_APP_TRAY;
        data.hIcon = self.icon;
        copy_wide(&mut data.szTip, TRAY_TIP);

        // SAFETY: data is fully initialized and points to the hidden main window.
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            anyhow::bail!(
                "Shell_NotifyIconW(NIM_ADD) failed: {}",
                Error::from_thread()
            );
        }
        self.added = true;
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: the shell has just registered this icon for our window.
        if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
            anyhow::bail!("Shell_NotifyIconW(NIM_SETVERSION) failed");
        }
        crate::logging::info(format!("tray ready pid={}", std::process::id()));
        Ok(())
    }

    pub fn remove(&mut self) {
        if !self.added {
            return;
        }
        let data = self.base_data();
        // SAFETY: removing our own icon; failure is non-fatal during shutdown.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
        self.added = false;
    }

    pub fn notify(&self, title: &str, message: &str, error: bool) {
        if !self.added {
            return;
        }
        let mut data = self.base_data();
        data.uFlags = NIF_INFO;
        copy_wide(&mut data.szInfoTitle, title);
        copy_wide(&mut data.szInfo, message);
        data.dwInfoFlags = if error { NIIF_ERROR } else { NIIF_INFO };
        // SAFETY: data is initialized; NIM_MODIFY only reads it.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    pub fn show_menu(hwnd: HWND, state: &TrayMenuState) -> Option<Command> {
        let menu = match state.menu() {
            Ok(menu) => menu,
            Err(err) => {
                crate::logging::warn(format!("failed to build tray menu: {err}"));
                return None;
            }
        };

        let mut pt = POINT::default();
        // SAFETY: pt is valid output storage.
        let _ = unsafe { GetCursorPos(&mut pt) };
        match menu.track(hwnd, pt) {
            MENU_REGION => Some(Command::Region),
            MENU_FULLSCREEN => Some(Command::Fullscreen),
            MENU_FULLSCREEN_SAVE => Some(Command::FullscreenSave),
            MENU_NEW_PIN => Some(Command::NewPin),
            MENU_CLOSE_PINS => Some(Command::ClosePins),
            MENU_OPEN_CONFIG => Some(Command::OpenConfig),
            MENU_RELOAD_CONFIG => Some(Command::ReloadConfig),
            MENU_AUTOSTART => Some(Command::ToggleAutostart),
            MENU_EXIT => Some(Command::Exit),
            id => id
                .checked_sub(MENU_PIN_BASE)
                .and_then(|index| state.pins.get(index))
                .map(|pin| Command::ShowPin(pin.id)),
        }
    }

    fn base_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }
}

impl TrayMenuState {
    fn menu(&self) -> windows::core::Result<PopupMenu> {
        let menu = PopupMenu::new()?;
        let plain = MENU_ITEM_FLAGS(0);
        for (id, title, shortcut) in [
            (MENU_REGION, "区域截图", &self.region_hint),
            (MENU_FULLSCREEN, "全屏截图", &self.fullscreen_hint),
            (
                MENU_FULLSCREEN_SAVE,
                "全屏截图并保存",
                &self.fullscreen_save_hint,
            ),
        ] {
            menu.item(id, &shortcut_label(title, shortcut), plain)?;
        }
        let pins = PopupMenu::new()?;
        pins.item(
            MENU_NEW_PIN,
            &shortcut_label("新建贴图", &self.pin_hint),
            plain,
        )?;
        pins.separator()?;
        if self.pins.is_empty() {
            pins.item(0, "暂无贴图", MF_GRAYED)?;
        } else {
            for (index, pin) in self.pins.iter().enumerate() {
                pins.item(MENU_PIN_BASE + index, &pin.label(), plain)?;
            }
            pins.separator()?;
            pins.item(MENU_CLOSE_PINS, "关闭全部贴图", plain)?;
        }
        menu.submenu(pins, &format!("贴图 ({})", self.pins.len()))?;
        menu.separator()?;
        menu.item(MENU_OPEN_CONFIG, "打开配置文件", plain)?;
        menu.item(MENU_RELOAD_CONFIG, "重新加载配置", plain)?;
        menu.item(
            MENU_AUTOSTART,
            "开机自启",
            if self.autostart { MF_CHECKED } else { plain },
        )?;
        menu.separator()?;
        menu.item(MENU_EXIT, "退出", plain)?;
        Ok(menu)
    }
}

fn shortcut_label(title: &str, shortcut: &str) -> String {
    if shortcut.is_empty() {
        title.to_owned()
    } else {
        format!("{title}\t{shortcut}")
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

fn load_app_icon() -> HICON {
    let instance = hinstance().ok();
    // Resource ID 101 is defined in assets/app.rc.
    if let Ok(icon) = unsafe { LoadIconW(instance, PCWSTR(101 as _)) } {
        return icon;
    }
    // SAFETY: IDI_APPLICATION is a system resource and always available.
    unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() }
}

fn copy_wide(dest: &mut [u16], text: &str) {
    if dest.is_empty() {
        return;
    }
    let mut count = 0usize;
    for unit in text.encode_utf16().take(dest.len() - 1) {
        dest[count] = unit;
        count += 1;
    }
    dest[count] = 0;
    for slot in &mut dest[count + 1..] {
        *slot = 0;
    }
}

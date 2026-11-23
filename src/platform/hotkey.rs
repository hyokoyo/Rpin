use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
}

#[derive(Debug, Clone)]
pub struct ParsedHotkey {
    pub hotkey: Hotkey,
    pub warning: Option<String>,
}

impl Hotkey {
    pub fn spec(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers & MOD_CONTROL.0 != 0 {
            parts.push("Ctrl".to_string());
        }
        if self.modifiers & MOD_ALT.0 != 0 {
            parts.push("Alt".to_string());
        }
        if self.modifiers & MOD_SHIFT.0 != 0 {
            parts.push("Shift".to_string());
        }
        if self.modifiers & MOD_WIN.0 != 0 {
            parts.push("Win".to_string());
        }
        parts.push(vk_name(self.vk));
        parts.join("+")
    }
}

pub fn parse(spec: &str) -> Result<ParsedHotkey, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("hotkey is empty".to_string());
    }

    let mut modifiers = 0u32;
    let mut tokens: Vec<&str> = trimmed.split('+').map(str::trim).collect();
    let key_token = tokens.pop().ok_or_else(|| "missing key".to_string())?;
    if key_token.is_empty() {
        return Err(format!("missing key in '{spec}'"));
    }

    for token in tokens {
        if token.is_empty() {
            return Err(format!("empty modifier in '{spec}'"));
        }
        let lower = token.to_ascii_lowercase();
        let bit = match lower.as_str() {
            "ctrl" | "control" => MOD_CONTROL.0,
            "alt" => MOD_ALT.0,
            "shift" => MOD_SHIFT.0,
            "win" | "windows" => MOD_WIN.0,
            _ => return Err(format!("unknown modifier '{token}'")),
        };
        if modifiers & bit != 0 {
            return Err(format!("duplicate modifier '{token}'"));
        }
        modifiers |= bit;
    }

    let vk = parse_vk(key_token).ok_or_else(|| format!("unknown key '{key_token}'"))?;
    let warning = if modifiers == 0 && is_plain_character(vk) {
        Some(format!(
            "hotkey '{}' has no modifier and may steal global input",
            key_token
        ))
    } else {
        None
    };

    Ok(ParsedHotkey {
        hotkey: Hotkey { modifiers, vk },
        warning,
    })
}

pub fn register(hwnd: HWND, id: i32, hotkey: Hotkey) -> windows::core::Result<()> {
    let modifiers = HOT_KEY_MODIFIERS(hotkey.modifiers | MOD_NOREPEAT.0);
    // SAFETY: hwnd is the hidden main window owned by the UI thread.
    unsafe { RegisterHotKey(Some(hwnd), id, modifiers, hotkey.vk) }
}

pub fn unregister(hwnd: HWND, id: i32) {
    // SAFETY: unregistering an id that may or may not be registered is safe.
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), id);
    }
}

fn is_plain_character(vk: u32) -> bool {
    (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk)
}

fn parse_vk(token: &str) -> Option<u32> {
    let upper = token.trim().to_ascii_uppercase();
    if upper.len() == 1 {
        let ch = upper.as_bytes()[0];
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            return Some(ch as u32);
        }
    }

    if let Some(number) = upper.strip_prefix('F') {
        if let Ok(n) = number.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1);
            }
        }
    }

    Some(match upper.as_str() {
        "PRINTSCREEN" | "PRTSC" | "PRINT" => 0x2C,
        "INSERT" | "INS" => 0x2D,
        "DELETE" | "DEL" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21,
        "PAGEDOWN" | "PGDN" => 0x22,
        "LEFT" => 0x25,
        "UP" => 0x26,
        "RIGHT" => 0x27,
        "DOWN" => 0x28,
        "SPACE" => 0x20,
        "TAB" => 0x09,
        "ENTER" | "RETURN" => 0x0D,
        "BACKSPACE" | "BACK" => 0x08,
        "ESC" | "ESCAPE" => 0x1B,
        "PAUSE" => 0x13,
        "CAPSLOCK" => 0x14,
        "NUMLOCK" => 0x90,
        "SCROLLLOCK" => 0x91,
        "SEMICOLON" => 0xBA,
        "EQUAL" => 0xBB,
        "COMMA" => 0xBC,
        "MINUS" => 0xBD,
        "PERIOD" => 0xBE,
        "SLASH" => 0xBF,
        "BACKTICK" => 0xC0,
        "LBRACKET" => 0xDB,
        "BACKSLASH" => 0xDC,
        "RBRACKET" => 0xDD,
        "QUOTE" => 0xDE,
        _ => return None,
    })
}

fn vk_name(vk: u32) -> String {
    if (0x41..=0x5A).contains(&vk) {
        return char::from_u32(vk)
            .map(|c| c.to_string())
            .unwrap_or_default();
    }
    if (0x30..=0x39).contains(&vk) {
        return char::from_u32(vk)
            .map(|c| c.to_string())
            .unwrap_or_default();
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x70 + 1);
    }
    match vk {
        0x2C => "PrintScreen".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x20 => "Space".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x08 => "Backspace".to_string(),
        0x1B => "Esc".to_string(),
        _ => format!("VK_{vk:02X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_region_hotkey() {
        let parsed = parse("F1").expect("parse");
        assert_eq!(parsed.hotkey.modifiers, 0);
        assert_eq!(parsed.hotkey.vk, 0x70);
        assert!(parsed.warning.is_none());
    }

    #[test]
    fn parses_shift_f1_case_insensitively() {
        let parsed = parse(" shift + f1 ").expect("parse");
        assert_eq!(parsed.hotkey.modifiers, MOD_SHIFT.0);
        assert_eq!(parsed.hotkey.vk, 0x70);
    }

    #[test]
    fn warns_for_plain_letter() {
        let parsed = parse("A").expect("parse");
        assert!(parsed.warning.is_some());
    }

    #[test]
    fn rejects_unknown_key_and_duplicate_modifier() {
        assert!(parse("Ctrl+NotAKey").is_err());
        assert!(parse("Ctrl+Ctrl+A").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn spec_round_trips_display() {
        let parsed = parse("Ctrl+Shift+F12").expect("parse");
        assert_eq!(parsed.hotkey.spec(), "Ctrl+Shift+F12");
    }
}

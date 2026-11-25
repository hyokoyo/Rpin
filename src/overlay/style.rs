use windows::Win32::Foundation::{COLORREF, HWND, POINT};
use windows::Win32::UI::Controls::Dialogs::{CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW};
use windows::Win32::UI::WindowsAndMessaging::{MF_CHECKED, MF_UNCHECKED};

use crate::annotation::StrokeWidth;
use crate::capture::frame::Point;
use crate::platform::menu::PopupMenu;

const FONT_SIZES: [u16; 14] = [12, 14, 16, 18, 20, 24, 28, 32, 36, 40, 48, 64, 80, 96];

fn choose_option<T: Copy + Eq, L: AsRef<str>>(
    owner: HWND,
    position: Point,
    current: T,
    options: &[T],
    label: impl Fn(T) -> L,
) -> windows::core::Result<Option<T>> {
    let menu = PopupMenu::new()?;
    for (index, option) in options.iter().copied().enumerate() {
        let flag = if option == current {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        menu.item(index + 1, label(option).as_ref(), flag)?;
    }
    let selected = menu.track(
        owner,
        POINT {
            x: position.x,
            y: position.y,
        },
    );
    Ok(selected
        .checked_sub(1)
        .and_then(|index| options.get(index).copied()))
}

pub(super) fn choose_size(
    owner: HWND,
    position: Point,
    current: u16,
) -> windows::core::Result<Option<u16>> {
    choose_option(owner, position, current, &FONT_SIZES, |size| {
        format!("{size} px")
    })
}

pub(super) fn choose_width(
    owner: HWND,
    position: Point,
    current: StrokeWidth,
) -> windows::core::Result<Option<StrokeWidth>> {
    choose_option(
        owner,
        position,
        current,
        &StrokeWidth::OPTIONS,
        StrokeWidth::label,
    )
}

pub(super) fn color_ref(rgb: [u8; 3]) -> COLORREF {
    COLORREF(u32::from_le_bytes([rgb[0], rgb[1], rgb[2], 0]))
}

pub(super) fn choose_color(
    owner: HWND,
    rgb: [u8; 3],
    custom: &mut [COLORREF; 16],
) -> Option<[u8; 3]> {
    let mut dialog = CHOOSECOLORW {
        lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: owner,
        rgbResult: color_ref(rgb),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_FULLOPEN | CC_RGBINIT,
        ..Default::default()
    };
    // SAFETY: the owner and color palette remain live during the modal dialog.
    if !unsafe { ChooseColorW(&mut dialog) }.as_bool() {
        return None;
    }
    let [r, g, b, _] = dialog.rgbResult.0.to_le_bytes();
    Some([r, g, b])
}

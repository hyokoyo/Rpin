use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;

pub fn stem(template: &str) -> String {
    // SAFETY: this API returns a SYSTEMTIME by value.
    render_stem(template, unsafe { GetLocalTime() })
}

fn render_stem(template: &str, time: SYSTEMTIME) -> String {
    let mut name = template.to_owned();
    for (token, value) in [
        ("{yyyy}", format!("{:04}", time.wYear)),
        ("{MM}", format!("{:02}", time.wMonth)),
        ("{dd}", format!("{:02}", time.wDay)),
        ("{HH}", format!("{:02}", time.wHour)),
        ("{mm}", format!("{:02}", time.wMinute)),
        ("{ss}", format!("{:02}", time.wSecond)),
        ("{ms}", format!("{:03}", time.wMilliseconds)),
    ] {
        name = name.replace(token, &value);
    }
    let name = name.trim_matches([' ', '.']);
    let name = match name.rsplit_once('.') {
        Some((stem, extension)) if extension.eq_ignore_ascii_case("png") => stem,
        _ => name,
    };
    // Bound UTF-16 units, leaving room for a collision suffix and extension.
    let mut units = 0;
    let mut sanitized: String = name
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= 180
        })
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    sanitized.truncate(sanitized.trim_end_matches([' ', '.']).len());
    if sanitized.is_empty() {
        return "RPin".into();
    }
    let base = sanitized
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if reserved {
        sanitized.insert(0, '_');
    }
    sanitized
}

pub fn wide(value: &OsStr) -> anyhow::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    anyhow::ensure!(!value.contains(&0), "path contains a NUL character");
    value.push(0);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_sanitizes_default_names() {
        let time = SYSTEMTIME {
            wYear: 2026,
            wMonth: 1,
            wDay: 3,
            wHour: 4,
            wMinute: 5,
            wSecond: 6,
            wMilliseconds: 7,
            ..Default::default()
        };
        assert_eq!(
            render_stem("截图/{yyyy}-{MM}-{dd}_{HH}:{mm}:{ss}.{ms}.PNG", time),
            "截图_2026-01-03_04_05_06.007"
        );
        assert_eq!(render_stem("LPT².png", time), "_LPT²");
        assert_eq!(render_stem(" .. ", time), "RPin");
        assert_eq!(
            render_stem(&"😀".repeat(200), time).encode_utf16().count(),
            180
        );
    }
}

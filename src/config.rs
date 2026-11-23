use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const DEFAULT_FILENAME: &str = "RPin_{yyyy}-{MM}-{dd}_{HH}-{mm}-{ss}";
const DEFAULT_FULLSCREEN_SAVE_HOTKEY: &str = "Ctrl+Shift+F1";
const DEFAULT_PIN_HOTKEY: &str = "Ctrl+Alt+P";

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hotkeys: Hotkeys,
    pub capture: CaptureConfig,
    pub ui: UiConfig,
    pub pins: PinsConfig,
    pub app: AppConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub region: String,
    pub fullscreen: String,
    pub fullscreen_save: String,
    /// Empty disables the global pin hotkey.
    pub pin: String,
    /// Reserved for v1.4. Empty means disabled.
    pub window: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            region: "F1".to_string(),
            fullscreen: "Shift+F1".to_string(),
            fullscreen_save: DEFAULT_FULLSCREEN_SAVE_HOTKEY.to_string(),
            pin: DEFAULT_PIN_HOTKEY.to_string(),
            window: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub include_cursor: bool,
    pub copy_to_clipboard: bool,
    pub notifications: bool,
    pub auto_save: bool,
    pub save_dir: String,
    pub filename: String,
    pub format: String,
    pub png_compression: String,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            include_cursor: false,
            copy_to_clipboard: true,
            notifications: true,
            auto_save: false,
            save_dir: String::new(),
            filename: DEFAULT_FILENAME.to_string(),
            format: "png".to_string(),
            png_compression: "fast".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub magnifier: bool,
    pub magnifier_zoom: u32,
    pub dim_alpha: f32,
    pub show_size: bool,
    pub show_rgb: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            magnifier: true,
            magnifier_zoom: 6,
            dim_alpha: 0.45,
            show_size: true,
            show_rgb: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PinsConfig {
    pub always_on_top: bool,
    pub default_opacity: f32,
}

impl Default for PinsConfig {
    fn default() -> Self {
        Self {
            always_on_top: true,
            default_opacity: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub start_minimized: bool,
    pub play_sound: bool,
    pub log_level: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            start_minimized: true,
            play_sound: false,
            log_level: "info".to_string(),
        }
    }
}

impl Config {
    /// Clamps values that would otherwise break the UI or the capture pipeline.
    /// Returns human-readable warnings for the log.
    pub fn validate(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();

        if crate::platform::hotkey::parse(&self.hotkeys.region).is_err() {
            warnings.push("hotkeys.region 无法解析，使用默认 F1".to_string());
            self.hotkeys.region = "F1".to_string();
        }
        if crate::platform::hotkey::parse(&self.hotkeys.fullscreen).is_err() {
            warnings.push("hotkeys.fullscreen 无法解析，使用默认 Shift+F1".to_string());
            self.hotkeys.fullscreen = "Shift+F1".to_string();
        }

        let parse_key = |spec: &str| crate::platform::hotkey::parse(spec).ok().map(|v| v.hotkey);
        let region = parse_key(&self.hotkeys.region);
        let mut fullscreen = parse_key(&self.hotkeys.fullscreen);
        if region == fullscreen {
            self.hotkeys.fullscreen = if region == parse_key("Shift+F1") {
                "Ctrl+Alt+F"
            } else {
                "Shift+F1"
            }
            .into();
            fullscreen = parse_key(&self.hotkeys.fullscreen);
            warnings.push(format!(
                "热键重复，全屏热键改为 {}",
                self.hotkeys.fullscreen
            ));
        }
        let save = &mut self.hotkeys.fullscreen_save;
        if !save.trim().is_empty() {
            if parse_key(save).is_none() {
                warnings.push(format!(
                    "hotkeys.fullscreen_save 无法解析，使用默认 {DEFAULT_FULLSCREEN_SAVE_HOTKEY}"
                ));
                *save = DEFAULT_FULLSCREEN_SAVE_HOTKEY.to_string();
            }
            let key = parse_key(save);
            if key == region || key == fullscreen {
                warnings.push(format!("热键重复，已停用全屏保存热键 {save}"));
                save.clear();
            }
        }
        let pin = &mut self.hotkeys.pin;
        if !pin.trim().is_empty() {
            match parse_key(pin) {
                None => {
                    warnings.push("hotkeys.pin 无法解析，已停用贴图热键".to_string());
                    pin.clear();
                }
                key if [region, fullscreen, parse_key(save)].contains(&key) => {
                    warnings.push(format!("热键重复，已停用贴图热键 {pin}"));
                    pin.clear();
                }
                _ => {}
            }
        }
        if self.capture.filename.trim().is_empty() {
            warnings.push("capture.filename 为空，使用默认模板".to_string());
            self.capture.filename = DEFAULT_FILENAME.to_string();
        }
        if self.capture.format != "png" {
            warnings.push(format!(
                "capture.format={} 暂不支持，回退为 png",
                self.capture.format
            ));
            self.capture.format = "png".to_string();
        }
        if self.capture.png_compression != "fast" && self.capture.png_compression != "small" {
            warnings.push(format!(
                "capture.png_compression={} 非法，回退为 fast",
                self.capture.png_compression
            ));
            self.capture.png_compression = "fast".to_string();
        }

        let alpha = self.ui.dim_alpha;
        if !(0.0..=0.8).contains(&alpha) {
            warnings.push(format!("ui.dim_alpha={alpha} 超出 0.0..=0.8，已修正"));
            self.ui.dim_alpha = if alpha.is_nan() {
                UiConfig::default().dim_alpha
            } else {
                alpha.clamp(0.0, 0.8)
            };
        }
        let zoom = self.ui.magnifier_zoom;
        if !(2..=12).contains(&zoom) {
            warnings.push(format!("ui.magnifier_zoom={zoom} 超出 2..=12，已修正"));
            self.ui.magnifier_zoom = zoom.clamp(2, 12);
        }

        if !matches!(
            self.app.log_level.as_str(),
            "error" | "warn" | "info" | "debug"
        ) {
            warnings.push(format!(
                "app.log_level={} 非法，回退为 info",
                self.app.log_level
            ));
            self.app.log_level = "info".to_string();
        }

        if self.pins.default_opacity.is_nan() {
            self.pins.default_opacity = PinsConfig::default().default_opacity;
        } else if !(0.1..=1.0).contains(&self.pins.default_opacity) {
            self.pins.default_opacity = self.pins.default_opacity.clamp(0.1, 1.0);
        }

        warnings
    }
}

pub fn config_path() -> PathBuf {
    appdata_dir().join("RPin").join("config.toml")
}

pub fn logs_dir() -> PathBuf {
    local_appdata_dir().join("RPin").join("logs")
}

fn appdata_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn local_appdata_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Loads config. Never fails hard: a broken file is renamed and defaults are used.
pub fn load_or_default() -> (Config, Vec<String>) {
    let path = config_path();
    let mut warnings = Vec::new();

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let cfg = Config::default();
            if let Err(save_err) = save(&cfg) {
                warnings.push(format!("写入默认配置失败: {save_err}"));
            } else {
                warnings.push(format!("已创建默认配置: {}", path.display()));
            }
            return (cfg, warnings);
        }
        Err(err) => {
            warnings.push(format!("读取配置失败: {err}; 使用默认配置"));
            return (Config::default(), warnings);
        }
    };

    match toml::from_str::<Config>(&text) {
        Ok(mut cfg) => {
            warnings.extend(cfg.validate());
            (cfg, warnings)
        }
        Err(err) => {
            warnings.push(format!("配置解析失败: {err}; 使用默认配置"));
            let backup = bad_config_path(&path);
            if let Err(rename_err) = fs::rename(&path, &backup) {
                warnings.push(format!("备份坏配置失败: {rename_err}"));
                return (Config::default(), warnings);
            } else {
                warnings.push(format!("坏配置已备份到: {}", backup.display()));
            }
            let cfg = Config::default();
            if let Err(save_err) = save(&cfg) {
                warnings.push(format!("写入默认配置失败: {save_err}"));
            }
            (cfg, warnings)
        }
    }
}

pub fn save(cfg: &Config) -> anyhow::Result<()> {
    let path = config_path();
    write_config(&path, cfg)
}

fn write_config(path: &Path, cfg: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg)?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn bad_config_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!("{name}.bad.{stamp}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("deserialize");
        assert_eq!(parsed.hotkeys.region, "F1");
        assert_eq!(parsed.hotkeys.fullscreen, "Shift+F1");
        assert_eq!(parsed.hotkeys.fullscreen_save, "Ctrl+Shift+F1");
        assert_eq!(parsed.capture.format, "png");
        assert!(parsed.capture.notifications);
        assert!(parsed.pins.always_on_top);
        assert_eq!(parsed.hotkeys.pin, "Ctrl+Alt+P");
    }

    #[test]
    fn capture_notifications_can_be_disabled_in_an_existing_config() {
        let old: Config = toml::from_str("[capture]\nauto_save = true").unwrap();
        let new: Config =
            toml::from_str("[capture]\nauto_save = true\nnotifications = false").unwrap();
        assert!(old.capture.notifications);
        assert!(!new.capture.notifications);
        assert!(new.capture.auto_save);
    }

    #[test]
    fn validate_prevents_duplicate_hotkeys() {
        let mut cfg = Config::default();
        cfg.hotkeys.region = " shift + F1 ".into();
        cfg.hotkeys.fullscreen = "SHIFT+f1".into();
        assert!(!cfg.validate().is_empty());
        assert_eq!(cfg.hotkeys.fullscreen, "Ctrl+Alt+F");
    }

    #[test]
    fn fullscreen_save_hotkey_defaults_and_can_be_disabled_or_changed() {
        let legacy = "[hotkeys]\nregion = 'Ctrl+Alt+1'\n";
        let mut cfg: Config = toml::from_str(legacy).unwrap();
        assert!(cfg.validate().is_empty());
        assert_eq!(cfg.hotkeys.fullscreen_save, "Ctrl+Shift+F1");

        for spec in ["", "Ctrl+Alt+S"] {
            let mut cfg: Config =
                toml::from_str(&format!("{legacy}fullscreen_save = '{spec}'")).unwrap();
            assert!(cfg.validate().is_empty());
            assert_eq!(cfg.hotkeys.fullscreen_save, spec);
        }
    }

    #[test]
    fn fullscreen_save_hotkey_preserves_existing_bindings() {
        let mut cfg = Config::default();
        cfg.hotkeys.fullscreen_save = "invalid".into();
        assert!(!cfg.validate().is_empty());
        assert_eq!(cfg.hotkeys.fullscreen_save, "Ctrl+Shift+F1");

        cfg.hotkeys.fullscreen = "Shift+Ctrl+F1".into();
        assert!(!cfg.validate().is_empty());
        assert!(cfg.hotkeys.fullscreen_save.is_empty());
        assert_eq!(cfg.hotkeys.fullscreen, "Shift+Ctrl+F1");
    }

    #[test]
    fn validate_clamps_values() {
        let mut cfg = Config::default();
        cfg.ui.dim_alpha = 2.0;
        cfg.ui.magnifier_zoom = 99;
        cfg.capture.format = "jpg".to_string();
        cfg.app.log_level = "verbose".to_string();
        let warnings = cfg.validate();
        assert!(!warnings.is_empty());
        assert_eq!(cfg.ui.dim_alpha, 0.8);
        assert_eq!(cfg.ui.magnifier_zoom, 12);
        assert_eq!(cfg.capture.format, "png");
        assert_eq!(cfg.app.log_level, "info");
    }
}

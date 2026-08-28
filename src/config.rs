use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "top" or "bottom"
    pub position: String,
    /// Bar height in logical pixels.
    pub height: u32,
    /// Reserve space so windows never sit under the bar.
    pub exclusive: bool,
    pub font: PathBuf,
    pub font_size: f32,
    /// Colours as #RRGGBB or #AARRGGBB.
    pub background: String,
    pub foreground: String,
    pub accent: String,
    pub muted: String,
    pub warning: String,
    /// Horizontal padding on each side of the bar and between modules.
    pub padding: u32,
    pub gap: u32,
    pub clock_format: String,
    pub date_format: String,
    pub show_date: bool,
    /// Start pipewire/wireplumber/pipewire-pulse at launch if installed and
    /// not running (Raven has no systemd user session to do it).
    pub start_pipewire: bool,
    /// ALSA fallback card (used only when PipeWire is not running), e.g. "hw:2". Empty = auto-detect.
    pub alsa_card: String,
    pub alsa_mixer: String,
    /// Volume change per scroll notch, percent.
    pub volume_step: i64,
    /// Bluetooth MAC (AA:BB:CC:DD:EE:FF) to connect on click. Empty = last connected/paired.
    pub bluetooth_device: String,
    pub battery: String,
    pub wifi_interface: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position: "top".into(),
            height: 26,
            exclusive: true,
            font: "/usr/share/fonts/JetBrainsMonoNerdFontMono-Regular.ttf".into(),
            font_size: 13.0,
            background: "#D916161F".into(),
            foreground: "#C0CAF5".into(),
            accent: "#7AA2F7".into(),
            muted: "#565F89".into(),
            warning: "#F7768E".into(),
            padding: 12,
            gap: 18,
            clock_format: "%H:%M".into(),
            date_format: "%a %d %b".into(),
            show_date: true,
            start_pipewire: true,
            alsa_card: String::new(),
            alsa_mixer: "Master".into(),
            volume_step: 5,
            bluetooth_device: String::new(),
            battery: "BAT0".into(),
            wifi_interface: String::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("roostbar").join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("roostbar: {}: {e}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }
}

/// Premultiplied ARGB from "#RRGGBB" / "#AARRGGBB".
pub fn parse_color(s: &str) -> [u8; 4] {
    let hex = s.trim().trim_start_matches('#');
    let v = u32::from_str_radix(hex, 16).unwrap_or(0xFFFFFFFF);
    let (a, r, g, b) = if hex.len() == 6 {
        (255u32, (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff)
    } else {
        ((v >> 24) & 0xff, (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff)
    };
    [a as u8, r as u8, g as u8, b as u8]
}

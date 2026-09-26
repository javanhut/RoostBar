use serde::Deserialize;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "top" or "bottom"
    pub position: String,
    /// Bar height in logical pixels.
    pub height: u32,
    /// Reserve space so windows never sit under the bar.
    pub exclusive: bool,
    /// The face the bar's words and numbers are set in. Empty or missing
    /// on disk = every character from `font`, as before there were two.
    pub ui_font: PathBuf,
    /// The Nerd Font: icon glyphs, and anything `ui_font` cannot draw.
    pub font: PathBuf,
    pub font_size: f32,
    /// Colours as #RRGGBB or #AARRGGBB.
    pub background: String,
    pub foreground: String,
    pub accent: String,
    pub muted: String,
    pub warning: String,
    /// Colour of the battery reading while plugged in. Defaults to `accent`
    /// so a charging laptop reads the same as a connected headset: something
    /// good is happening. Empty = same as `accent`.
    pub charging: String,
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
    /// Battery percentage at or below which the reading turns `warning`,
    /// unless it is charging.
    pub battery_low: u32,
    pub wifi_interface: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position: "top".into(),
            height: 28,
            exclusive: true,
            ui_font: "/usr/share/fonts/noto/NotoSans-Regular.ttf".into(),
            font: "/usr/share/fonts/JetBrainsMonoNerdFontMono-Regular.ttf".into(),
            font_size: 13.5,
            background: "#D816161F".into(),
            foreground: "#E8E8F0".into(),
            accent: "#7AA2F7".into(),
            muted: "#ABABC2".into(),
            warning: "#F7768E".into(),
            charging: String::new(),
            padding: 12,
            gap: 16,
            clock_format: "%H:%M".into(),
            date_format: "%a %d %b".into(),
            show_date: true,
            start_pipewire: true,
            alsa_card: String::new(),
            alsa_mixer: "Master".into(),
            volume_step: 5,
            bluetooth_device: String::new(),
            battery: "BAT0".into(),
            battery_low: 15,
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

    /// When the config file was last written, for the reload check in the
    /// slow poll. `None` when there is no file (or it cannot be stat'd),
    /// which is itself a state worth noticing: a file that appears is a
    /// change too.
    ///
    /// `desktop.toml` is stat'd alongside it: the colours this file leaves
    /// out come from there, so an edit to it is a change to the bar too.
    pub fn mtime() -> (Option<SystemTime>, Option<SystemTime>) {
        let stat = |p: PathBuf| std::fs::metadata(p).ok()?.modified().ok();
        (stat(Self::path()), stat(desktop_path()))
    }

    pub fn load() -> Self {
        let path = Self::path();
        let desktop = Desktop::load();
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::from_text(&text, &desktop) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("roostbar: {}: {e}; using defaults", path.display());
                    Self::from_desktop(&desktop)
                }
            },
            Err(_) => Self::from_desktop(&desktop),
        }
    }

    /// Parse `text`, taking any of the four theme colours it does not set
    /// from the desktop's theme rather than the compiled dark defaults.
    /// Raven Settings writes all four here whenever it saves, so this only
    /// matters for a file written by hand, or before Settings has run.
    fn from_text(text: &str, desktop: &Desktop) -> Result<Self, toml::de::Error> {
        let mut table: toml::Table = toml::from_str(text)?;
        for (key, value) in desktop.palette() {
            table
                .entry(key)
                .or_insert_with(|| toml::Value::String(value));
        }
        table.try_into()
    }

    fn from_desktop(desktop: &Desktop) -> Self {
        Self::from_text("", desktop).unwrap_or_default()
    }
}

/// The slice of `~/.config/raven/desktop.toml` the bar falls back on for its
/// colours: theme mode, accent and transparency. Settings owns the file;
/// every key is optional and a parse error means the defaults.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct Desktop {
    appearance: Appearance,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Appearance {
    /// "light", "dark" or "auto"; anything but "light" is dark, as in
    /// Raven Settings.
    theme_mode: String,
    accent: String,
    transparency: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme_mode: "dark".into(),
            accent: DEFAULT_ACCENT.into(),
            transparency: true,
        }
    }
}

const DEFAULT_ACCENT: &str = "#7AA2F7";

impl Desktop {
    fn load() -> Self {
        std::fs::read_to_string(desktop_path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// accent/background/foreground/muted exactly as Raven Settings'
    /// `sync_roostbar` writes them for this theme.
    fn palette(&self) -> [(&'static str, String); 4] {
        let a = &self.appearance;
        let (bg, fg, muted) = if a.theme_mode == "light" {
            ("#D9F2F2F7", "#1C1C22", "#5E5E72")
        } else {
            ("#D816161F", "#E8E8F0", "#ABABC2")
        };
        let bg = if a.transparency {
            bg.to_string()
        } else {
            format!("#FF{}", &bg[3..])
        };
        let accent = if is_hex(&a.accent) { a.accent.as_str() } else { DEFAULT_ACCENT };
        [
            ("accent", accent.to_string()),
            ("background", bg),
            ("foreground", fg.to_string()),
            ("muted", muted.to_string()),
        ]
    }
}

fn is_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn desktop_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("raven")
        .join("desktop.toml")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop(text: &str) -> Desktop {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn missing_colours_follow_the_desktop() {
        let d = desktop("[appearance]\ntheme_mode = \"light\"\naccent = \"#F7768E\"\n");
        let cfg = Config::from_text("height = 30\n", &d).unwrap();
        assert_eq!(cfg.height, 30);
        assert_eq!(cfg.accent, "#F7768E");
        assert_eq!(cfg.background, "#D9F2F2F7");
        assert_eq!(cfg.foreground, "#1C1C22");
        assert_eq!(cfg.muted, "#5E5E72");
    }

    #[test]
    fn the_file_wins_over_the_desktop() {
        let d = desktop("[appearance]\ntheme_mode = \"light\"\n");
        let cfg = Config::from_text("accent = \"#22C5DD\"\nforeground = \"#FFFFFF\"\n", &d).unwrap();
        assert_eq!(cfg.accent, "#22C5DD");
        assert_eq!(cfg.foreground, "#FFFFFF");
        assert_eq!(cfg.background, "#D9F2F2F7");
    }

    #[test]
    fn auto_is_dark_and_opaque_without_transparency() {
        let d = desktop("[appearance]\ntheme_mode = \"auto\"\naccent = \"red\"\ntransparency = false\n");
        let cfg = Config::from_desktop(&d);
        assert_eq!(cfg.accent, DEFAULT_ACCENT);
        assert_eq!(cfg.background, "#FF16161F");
        assert_eq!(cfg.foreground, "#E8E8F0");
    }

    #[test]
    fn no_desktop_file_matches_the_compiled_defaults() {
        let cfg = Config::from_desktop(&Desktop::default());
        let def = Config::default();
        assert_eq!(
            (cfg.accent, cfg.background, cfg.foreground, cfg.muted),
            (def.accent, def.background, def.foreground, def.muted)
        );
    }
}

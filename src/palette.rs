use eframe::egui::{self, Color32};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Preset {
    #[default]
    Ferris,
    Monokai,
    Solarized,
    Claude,
    VsCode,
    Custom,
}
impl Preset {
    pub const ALL: [Self; 6] = [
        Self::Ferris,
        Self::Monokai,
        Self::Solarized,
        Self::Claude,
        Self::VsCode,
        Self::Custom,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Ferris => "Ferris",
            Self::Monokai => "Monokai",
            Self::Solarized => "Solarized",
            Self::Claude => "Claude",
            Self::VsCode => "VS Code",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Palette {
    pub string: [u8; 3],
    pub number: [u8; 3],
    pub boolean: [u8; 3],
    pub null: [u8; 3],
    pub object: [u8; 3],
    pub array: [u8; 3],
    pub punctuation: [u8; 3],
    pub background: [u8; 3],
    pub stripe: [u8; 3],
    pub selection: [u8; 3],
    pub search_match: [u8; 3],
}
pub fn color(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}
impl Palette {
    pub fn kind(self, kind: &str) -> Color32 {
        color(match kind {
            "string" => self.string,
            "number" => self.number,
            "boolean" => self.boolean,
            "null" => self.null,
            "object" => self.object,
            "array" => self.array,
            _ => self.punctuation,
        })
    }
    pub fn builtin(preset: Preset, dark: bool) -> Self {
        // Original type mappings inspired by these palettes, rather than copied editor themes.
        let colors: [u32; 11] = match (preset, dark) {
            (Preset::Monokai, true) => [
                0xe6db74, 0xae81ff, 0xf92672, 0xfd971f, 0x66d9ef, 0xa6e22e, 0xf8f8f2, 0x272822,
                0x2d2e27, 0x49483e, 0x665728,
            ],
            (Preset::Monokai, false) => [
                0x797018, 0x7951b0, 0xb31d59, 0xa25912, 0x167783, 0x507e16, 0x414339, 0xf6f5ed,
                0xeeede4, 0xe4dcc3, 0xffe4a1,
            ],
            (Preset::Solarized, true) => [
                0x859900, 0x2aa198, 0x268bd2, 0xcb4b16, 0x6c71c4, 0xb58900, 0x93a1a1, 0x002b36,
                0x073642, 0x174652, 0x514c20,
            ],
            (Preset::Solarized, false) => [
                0x647300, 0x16847d, 0x2176b4, 0xb34213, 0x655ba8, 0x906c00, 0x586e75, 0xfdf6e3,
                0xeee8d5, 0xd8e5d3, 0xf0dca5,
            ],
            (Preset::Claude, true) => [
                0x9fc89d, 0xd8b17c, 0xcaa7e8, 0xde9b87, 0x8bc7c2, 0xe2c48e, 0xe6dfd5, 0x292724,
                0x302d29, 0x514037, 0x5c4930,
            ],
            (Preset::Claude, false) => [
                0x4f704d, 0xa16a30, 0x8b5b9b, 0xb2674d, 0x397b78, 0x926f24, 0x514b43, 0xfaf7f0,
                0xf2ede3, 0xe9d9ca, 0xf5dfb1,
            ],
            (Preset::VsCode, true) => [
                0xce9178, 0xb5cea8, 0x569cd6, 0xc586c0, 0x9cdcfe, 0xdcdcaa, 0xd4d4d4, 0x1e1e1e,
                0x252526, 0x264f78, 0x614d20,
            ],
            (Preset::VsCode, false) => [
                0xa31515, 0x098658, 0x0000ff, 0x800080, 0x0451a5, 0x795e26, 0x333333, 0xffffff,
                0xf3f3f3, 0xd6eaff, 0xffe8a6,
            ],
            (_, true) => [
                0x73cd96, 0x6eb4ff, 0xcd96ff, 0xd7af7d, 0x6ecdd7, 0xebb964, 0xb4b9c3, 0x1b1b1b,
                0x232323, 0x1e415f, 0x4b4119,
            ],
            (_, false) => [
                0x1e7d41, 0x1964b9, 0x8241af, 0x915f23, 0x14737d, 0x966414, 0x555a64, 0xf8f8f8,
                0xeeeeee, 0xcde6fa, 0xffebaa,
            ],
        };
        let c = colors.map(|hex| [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8]);
        Self {
            string: c[0],
            number: c[1],
            boolean: c[2],
            null: c[3],
            object: c[4],
            array: c[5],
            punctuation: c[6],
            background: c[7],
            stripe: c[8],
            selection: c[9],
            search_match: c[10],
        }
    }
    pub fn apply(self, ctx: &egui::Context, dark: bool) {
        let mut visuals = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.panel_fill = color(self.background);
        visuals.window_fill = color(self.background);
        visuals.extreme_bg_color = color(self.background);
        visuals.faint_bg_color = color(self.stripe);
        visuals.override_text_color = Some(color(self.punctuation));
        visuals.selection.bg_fill = color(self.selection);
        ctx.set_visuals(visuals);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorSettings {
    pub preset: Preset,
    pub dark: bool,
    pub custom_dark: Palette,
    pub custom_light: Palette,
}
impl Default for ColorSettings {
    fn default() -> Self {
        Self {
            preset: Preset::Ferris,
            dark: true,
            custom_dark: Palette::builtin(Preset::Ferris, true),
            custom_light: Palette::builtin(Preset::Ferris, false),
        }
    }
}
impl ColorSettings {
    pub fn active(&self) -> Palette {
        if self.preset == Preset::Custom {
            if self.dark {
                self.custom_dark
            } else {
                self.custom_light
            }
        } else {
            Palette::builtin(self.preset, self.dark)
        }
    }
    pub fn customize(&mut self) {
        if self.preset != Preset::Custom {
            self.custom_dark = Palette::builtin(self.preset, true);
            self.custom_light = Palette::builtin(self.preset, false);
            self.preset = Preset::Custom;
        }
    }
    pub fn load() -> Result<Self, String> {
        let path = settings_path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| format!("Cannot read color settings: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
        }
    }
    pub fn save(&self) -> Result<(), String> {
        let path = settings_path()?;
        let parent = path.parent().ok_or("Invalid settings path")?;
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create settings directory: {e}"))?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        // Write a separate file first to avoid losing preferences if serialization or writing fails.
        let temp = parent.join(format!("colors.{}.json.tmp", std::process::id()));
        std::fs::write(&temp, bytes).map_err(|e| format!("Cannot save color settings: {e}"))?;
        std::fs::rename(&temp, &path).map_err(|e| format!("Cannot replace color settings: {e}"))
    }
}
pub fn settings_path() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let root = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let root =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    root.map(|p| p.join("FerrisJson/colors.json"))
        .ok_or_else(|| "Cannot locate user settings directory".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn customization_keeps_both_modes_and_round_trips() {
        let mut settings = ColorSettings {
            preset: Preset::Solarized,
            ..Default::default()
        };
        let dark = settings.active();
        settings.customize();
        assert_eq!(settings.active(), dark);
        settings.custom_dark.string = [1, 2, 3];
        settings.dark = false;
        assert_eq!(
            settings.active(),
            Palette::builtin(Preset::Solarized, false)
        );
        let copy: ColorSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(copy.custom_dark.string, [1, 2, 3]);
        assert_eq!(copy.preset, Preset::Custom);
        assert!(serde_json::from_str::<ColorSettings>(r#"{"preset":"Unknown"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<ColorSettings>("{}").unwrap().preset,
            Preset::Ferris
        );
    }
}

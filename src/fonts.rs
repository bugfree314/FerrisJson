use eframe::egui::{FontData, FontDefinitions, FontFamily};
use std::path::PathBuf;

/// Append installed fonts to both families so JSON and UI text use the same coverage.
/// Keep egui's Latin fonts first; load no font files from the JSON document itself.
pub fn definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    for (index, path) in candidates().into_iter().enumerate() {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let name = format!("system-fallback-{index}");
        fonts
            .font_data
            .insert(name.clone(), FontData::from_owned(bytes).into());
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
        }
    }
    fonts
}

fn candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/Windows"))
            .join("Fonts");
        // Chinese, Korean, Japanese, Arabic/Hebrew, Indic, Thai, broad Unicode.
        [
            "msyh.ttc",
            "malgun.ttf",
            "YuGothR.ttc",
            "msgothic.ttc",
            "segoeui.ttf",
            "tahoma.ttf",
            "Nirmala.ttc",
            "LeelawUI.ttf",
            "arial.ttf",
            "ARIALUNI.ttf",
        ]
        .into_iter()
        .map(|name| root.join(name))
        .collect()
    }
    #[cfg(target_os = "macos")]
    {
        [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansArabic-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansHebrew-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    #[test]
    fn installed_fallbacks_cover_main_languages_in_both_families() {
        let ctx = eframe::egui::Context::default();
        ctx.set_fonts(definitions());
        let _ = ctx.run(Default::default(), |ctx| {
            ctx.fonts_mut(|fonts| {
                for family in [FontFamily::Proportional, FontFamily::Monospace] {
                    let font = eframe::egui::FontId::new(16.0, family);
                    for text in [
                        "中文繁體",
                        "한국어특허",
                        "日本語あいうアイウ",
                        "English français español Deutsch",
                        "Русский Ελληνικά",
                        "العربية עברית",
                        "हिन्दी",
                        "ภาษาไทย",
                    ] {
                        assert!(fonts.has_glyphs(&font, text), "Missing glyphs for {text}");
                    }
                }
            });
        });
    }
}

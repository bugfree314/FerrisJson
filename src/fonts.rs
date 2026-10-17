use eframe::egui::{FontData, FontDefinitions, FontFamily, FontTweak};
use std::path::PathBuf;

/// Append installed fonts to both families so JSON and UI text use the same coverage.
/// Keep egui's Latin fonts first; load no font files from the JSON document itself.
pub fn definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    // Use an outline emoji font: epaint rasterizes outlines, not Apple's bitmap
    // color emoji. Replace the older bundled face without changing family order.
    fonts.font_data.insert(
        "NotoEmoji-Regular".into(),
        FontData::from_static(include_bytes!("../assets/fonts/NotoEmoji.ttf"))
            .tweak(FontTweak { scale: 0.81, ..Default::default() }).into(),
    );
    for (index, path) in candidates().into_iter().enumerate() {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let name = format!("system-fallback-{index}");
        #[cfg(target_os = "macos")]
        let face = pingfang_regular_face(&bytes).unwrap_or(0);
        #[cfg(not(target_os = "macos"))]
        let face = 0;
        let mut data = FontData::from_owned(bytes);
        data.index = face;
        fonts.font_data.insert(name.clone(), data.into());
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
        }
    }
    fonts
}

// Collection order differs across macOS releases; index 0 can be HK rather
// than SC. Select the regular simplified-Chinese face by its PostScript name.
#[cfg(target_os = "macos")]
fn pingfang_regular_face(bytes: &[u8]) -> Option<u32> {
    fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
        Some(u16::from_be_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
    }
    fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
        Some(u32::from_be_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
    }
    if bytes.get(..4)? != b"ttcf" { return None; }
    let faces = u32_at(bytes, 8)?;
    for face in 0..faces.min(256) {
        let offset = u32_at(bytes, 12 + face as usize * 4)? as usize;
        let tables = u16_at(bytes, offset.checked_add(4)?)? as usize;
        for table in 0..tables.min(256) {
            let record = offset.checked_add(12 + table * 16)?;
            if bytes.get(record..record.checked_add(4)?)? != b"name" { continue; }
            let start = u32_at(bytes, record.checked_add(8)?)? as usize;
            let length = u32_at(bytes, record.checked_add(12)?)? as usize;
            let names = bytes.get(start..start.checked_add(length)?)?;
            let count = u16_at(names, 2)? as usize;
            let storage = u16_at(names, 4)? as usize;
            for n in 0..count {
                let entry = 6 + n * 12;
                if u16_at(names, entry + 6)? != 6 { continue; }
                let platform = u16_at(names, entry)?;
                let length = u16_at(names, entry + 8)? as usize;
                let start = storage.checked_add(u16_at(names, entry + 10)? as usize)?;
                let name = names.get(start..start.checked_add(length)?)?;
                let matches = match platform {
                    0 | 3 => name.iter().copied().eq("PingFangSC-Regular".encode_utf16().flat_map(u16::to_be_bytes)),
                    1 => name == b"PingFangSC-Regular",
                    _ => false,
                };
                if matches { return Some(face); }
            }
        }
    }
    None
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
        let mut chinese = vec![PathBuf::from("/System/Library/Fonts/PingFang.ttc")];
        // Newer macOS versions install PingFang as a font asset. Inspect only
        // these known font containers, never recursively scan the filesystem.
        for root in [
            "/System/Library/AssetsV2/com_apple_MobileAsset_Font8",
            "/System/Library/AssetsV2/com_apple_MobileAsset_Font7",
        ] {
            if let Ok(entries) = std::fs::read_dir(root) {
                let mut paths: Vec<_> = entries.filter_map(Result::ok)
                    .map(|entry| entry.path().join("AssetData/PingFang.ttc"))
                    .filter(|path| path.is_file()).collect();
                paths.sort();
                chinese.extend(paths);
            }
        }
        chinese.extend([
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ].into_iter().map(PathBuf::from));
        // Load one Chinese family, before Korean/Japanese Han glyph fallbacks.
        // Loading every CJK collection would retain several large font files.
        let mut paths: Vec<_> = chinese.into_iter().find(|path| path.is_file()).into_iter().collect();
        paths.extend([
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ].into_iter().map(PathBuf::from));
        paths
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

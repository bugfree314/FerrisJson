//! Font loading for the gpui front end.
//!
//! gpui discovers installed families through the platform text system. The
//! bundled emoji face and the same installed fallback families used by the
//! previous front end are registered through the text system so JSON and UI
//! text keep their multi-script coverage. No font data is read from the viewed
//! document.
use gpui::App;
use std::{borrow::Cow, path::PathBuf};

pub fn install(cx: &mut App) {
    let mut fonts: Vec<Cow<'static, [u8]>> = vec![Cow::Borrowed(include_bytes!(
        "../assets/fonts/NotoEmoji.ttf"
    ))];
    for path in candidates() {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.push(Cow::Owned(bytes));
        }
    }
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        eprintln!("Cannot load fonts: {error:?}");
    }
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
        let mut files = vec![
            PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
            PathBuf::from("/System/Library/Fonts/Hiragino Sans GB.ttc"),
            PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"),
            PathBuf::from("/System/Library/Fonts/AppleSDGothicNeo.ttc"),
            PathBuf::from("/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc"),
            PathBuf::from("/System/Library/Fonts/Supplemental/Arial Unicode.ttf"),
        ];
        files.retain(|path| path.is_file());
        files.truncate(1);
        files
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

use eframe::egui::{self, Color32, RichText};

pub fn kind_color(kind: &str, dark: bool) -> Color32 {
    let rgb = match (kind, dark) {
        ("string", true) => (115, 205, 150),
        ("string", false) => (30, 125, 65),
        ("number", true) => (110, 180, 255),
        ("number", false) => (25, 100, 185),
        ("boolean", true) => (205, 150, 255),
        ("boolean", false) => (130, 65, 175),
        ("null", true) => (215, 175, 125),
        ("null", false) => (145, 95, 35),
        ("object", true) => (110, 205, 215),
        ("object", false) => (20, 115, 125),
        ("array", true) => (235, 185, 100),
        ("array", false) => (150, 100, 20),
        (_, true) => (180, 185, 195),
        (_, false) => (85, 90, 100),
    };
    Color32::from_rgb(rgb.0, rgb.1, rgb.2)
}
pub fn json_job(text: &str, dark: bool) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let kind = match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i = (i + 2).min(bytes.len()),
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let tail = text[i..].trim_start();
                if let Some(value) = tail.strip_prefix(':') {
                    value_kind(value.trim_start())
                } else {
                    "string"
                }
            }
            b'-' | b'0'..=b'9' => {
                i += 1;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    i += 1;
                }
                "number"
            }
            b't' if text[i..].starts_with("true") => {
                i += 4;
                "boolean"
            }
            b'f' if text[i..].starts_with("false") => {
                i += 5;
                "boolean"
            }
            b'n' if text[i..].starts_with("null") => {
                i += 4;
                "null"
            }
            _ => {
                i += text[i..].chars().next().unwrap().len_utf8();
                "punctuation"
            }
        };
        job.append(
            &text[start..i],
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(13.0),
                color: kind_color(kind, dark),
                ..Default::default()
            },
        );
    }
    job
}
fn value_kind(text: &str) -> &'static str {
    match text.as_bytes().first() {
        Some(b'"') => "string",
        Some(b't' | b'f') => "boolean",
        Some(b'n') => "null",
        Some(b'{') => "object",
        Some(b'[') => "array",
        _ => "number",
    }
}
pub fn json_preview(ui: &mut egui::Ui, mut text: &str, dark: bool, id: &str) {
    egui::ScrollArea::both()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, _: f32| {
                ui.fonts_mut(|fonts| fonts.layout_job(json_job(buffer.as_str(), dark)))
            };
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .layouter(&mut layouter),
            );
        });
}
pub fn cheatsheet(ctx: &egui::Context, open: &mut bool) {
    if !*open {
        return;
    }
    egui::Window::new("Keyboard cheatsheet").open(open).collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(720.0).max_width((ctx.content_rect().width() - 40.0).max(300.0))
        .max_height((ctx.content_rect().height() - 60.0).max(200.0)).show(ctx, |ui| {
            ui.label(RichText::new("Navigate JSON at the speed of thought").size(22.0).strong());
            ui.label("Vim keys work in the tree. Text fields keep normal typing. Esc closes this page.");
            ui.add_space(10.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                let groups: &[(&str, &[(&str, &str)])] = &[
                    ("NAVIGATION", &[("hjkl / arrows", "Parent / down / up / child"), ("12j · 3l", "Repeat a movement with a count"), ("gg · G · 25G", "First / last / visible row 25"), ("H · M · L", "Top / middle / bottom of viewport"), ("Ctrl+d / u", "Half page down / up"), ("PageDown / Up · Ctrl+e / b", "Full page down / up"), ("[ / ]", "Back / forward through visited positions")]),
                    ("SEARCH", &[("/ · ?", "Search forward / backward"), ("n · N · 3n", "Repeat search / reverse / count"), ("Enter · Esc", "Confirm search / return to tree"), ("F3 / Shift+F3", "Next / previous match"), ("Ctrl+F", "Focus search field")]),
                    ("TREE & COPY", &[("Space · za", "Toggle selected container"), ("zo · zc", "Open / close selected container"), ("zR · zM", "Expand / collapse whole document"), ("yy / Ctrl+C", "Copy full selected JSON"), ("yp / Ctrl+P", "Copy selected path")]),
                    ("DOCUMENT", &[("Ctrl+O · Ctrl+V", "Open file / load clipboard JSON"), ("F5", "Reload current file"), ("F1", "Toggle this cheatsheet"), ("Esc", "Cancel pending Vim keys / close dialog")]),
                ];
                for (title, rows) in groups {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label(RichText::new(*title).strong().color(Color32::from_rgb(80, 160, 220)));
                        egui::Grid::new(*title).num_columns(2).spacing([28.0, 7.0]).striped(true).show(ui, |ui| {
                            for (key, action) in *rows { ui.label(RichText::new(*key).monospace().strong()); ui.label(*action); ui.end_row(); }
                        });
                    }); ui.add_space(8.0);
                }
                ui.label(RichText::new("JSONPath").strong());
                ui.monospace("$.users[*].name   $..email   $.users[?(@.active == true)]");
                ui.label("Go / Enter runs JSONPath. The lower inspector keeps results; the upper always previews your selection. Drag the divider to resize.");
                ui.horizontal_wrapped(|ui| { for kind in ["string", "number", "boolean", "null", "object", "array"] {
                    ui.colored_label(kind_color(kind, ui.visuals().dark_mode), kind);
                }});
            });
        });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_colors_preserve_unicode_and_follow_value_types() {
        let text = r#"{"韩语":"한국어","flag":true,"n":2,"nil":null,"obj":{},"list":[]}"#;
        let job = json_job(text, true);
        assert_eq!(job.text, text);
        for (token, kind) in [
            ("\"flag\"", "boolean"),
            ("true", "boolean"),
            ("\"n\"", "number"),
            ("null", "null"),
            ("\"韩语\"", "string"),
        ] {
            let section = job
                .sections
                .iter()
                .find(|s| &job.text[s.byte_range.clone()] == token)
                .unwrap();
            assert_eq!(section.format.color, kind_color(kind, true));
        }
        assert_ne!(kind_color("string", false), kind_color("string", true));
    }
}

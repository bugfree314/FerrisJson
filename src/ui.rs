use crate::palette::{ColorSettings, Palette, Preset};
use eframe::egui::{self, Color32, RichText};

pub fn kind_color(kind: &str, palette: Palette) -> Color32 {
    palette.kind(kind)
}
pub fn json_job(text: &str, palette: Palette) -> egui::text::LayoutJob {
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
                color: kind_color(kind, palette),
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
fn preview_job(text: &str, palette: Palette, wrap: bool, width: f32) -> egui::text::LayoutJob {
    let mut job = json_job(text, palette);
    job.wrap.max_width = if wrap { width.max(1.0) } else { f32::INFINITY };
    job.wrap.break_anywhere = wrap;
    job
}
pub fn json_preview(ui: &mut egui::Ui, mut text: &str, palette: Palette, id: &str, wrap: bool) {
    let area = if wrap {
        egui::ScrollArea::vertical()
    } else {
        egui::ScrollArea::both()
    };
    area.id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
        let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
            let job = preview_job(buffer.as_str(), palette, wrap, wrap_width);
            ui.fonts_mut(|fonts| fonts.layout_job(job))
        };
        ui.add(
            egui::TextEdit::multiline(&mut text)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .layouter(&mut layouter),
        );
    });
}
pub fn cheatsheet(ctx: &egui::Context, open: &mut bool, palette: Palette) {
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
                    ui.colored_label(kind_color(kind, palette), kind);
                }});
            });
        });
}
pub fn color_editor(
    ctx: &egui::Context,
    open: &mut bool,
    settings: &mut ColorSettings,
    notice: &mut String,
    error: &mut Option<String>,
) -> bool {
    if !*open {
        return false;
    }
    let mut changed = false;
    egui::Window::new("JSON colors").open(open).collapsible(false).default_width(530.0)
        .max_height((ctx.content_rect().height() - 60.0).max(250.0)).show(ctx, |ui| {
        ui.heading("Make JSON feel like home");
        ui.label("Built-in palettes include light and dark variants. Edits apply immediately; Save remembers custom colors.");
        ui.horizontal(|ui| {
            let old = settings.preset;
            egui::ComboBox::from_id_salt("editor_palette").selected_text(settings.preset.name()).show_ui(ui, |ui| {
                for preset in Preset::ALL { ui.selectable_value(&mut settings.preset, preset, preset.name()); }
            });
            changed |= old != settings.preset;
            changed |= ui.checkbox(&mut settings.dark, "Dark mode").changed();
        });
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut palette = settings.active(); let mut edited = false;
            ui.add_space(6.0);
            egui::Grid::new("custom_colors").num_columns(3).spacing([18.0, 5.0]).show(ui, |ui| {
                for (label, rgb) in [("String", &mut palette.string), ("Number", &mut palette.number),
                    ("Boolean", &mut palette.boolean), ("Null", &mut palette.null), ("Object", &mut palette.object),
                    ("Array", &mut palette.array), ("Text / punctuation", &mut palette.punctuation),
                    ("Background", &mut palette.background), ("Alternating row", &mut palette.stripe),
                    ("Selected row", &mut palette.selection), ("Search match", &mut palette.search_match)] {
                    ui.label(label); edited |= ui.color_edit_button_srgb(rgb).changed();
                    ui.monospace(format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])); ui.end_row();
                }
            });
            if edited {
                settings.customize();
                if settings.dark { settings.custom_dark = palette; } else { settings.custom_light = palette; }
                changed = true;
            }
            ui.add_space(10.0);
            ui.label("Preview (keys follow their values' types)");
            egui::Frame::new().fill(crate::palette::color(settings.active().background)).inner_margin(10.0).show(ui, |ui| {
                ui.label(json_job(r#"{"name":"FerrisJson","count":42,"active":true,"optional":null,"object":{},"items":[]}"#, settings.active()));
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button("Use as custom").clicked() { settings.customize(); changed = true; }
                if ui.button("Restore Ferris defaults").clicked() {
                    let dark = settings.dark;
                    let wrap_values = settings.wrap_values;
                    let wrap_results = settings.wrap_results;
                    let low_memory_load = settings.low_memory_load;
                    let indexed_files = settings.indexed_files;
                    *settings = ColorSettings { dark, wrap_values, wrap_results, low_memory_load, indexed_files, ..Default::default() }; changed = true;
                }
                if ui.button("Save settings").clicked() {
                    match settings.save() { Ok(()) => { *notice = "Color settings saved".into(); *error = None; }, Err(e) => *error = Some(e) }
                }
            });
            if let Some(error) = error.as_ref() { ui.colored_label(Color32::LIGHT_RED, error); }
            if let Ok(path) = crate::palette::settings_path() {
                ui.add(egui::Label::new(format!("Saved to {}", path.display())).wrap());
            }
        });
    });
    changed
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_colors_preserve_unicode_and_follow_value_types() {
        let text = r#"{"韩语":"한국어","flag":true,"n":2,"nil":null,"obj":{},"list":[]}"#;
        let palette = Palette::builtin(Preset::Ferris, true);
        let job = json_job(text, palette);
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
            assert_eq!(section.format.color, kind_color(kind, palette));
        }
        assert_ne!(
            kind_color("string", Palette::builtin(Preset::Ferris, false)),
            kind_color("string", palette)
        );
    }
    #[test]
    fn wrapping_long_urls_fits_preview_width_without_changing_value() {
        let ctx = egui::Context::default();
        let text = format!("\"https://example.com/{}\"", "longpath".repeat(40));
        let palette = Palette::builtin(Preset::Ferris, true);
        let _ = ctx.run(Default::default(), |ctx| {
            ctx.fonts_mut(|fonts| {
                let wrapped = fonts.layout_job(preview_job(&text, palette, true, 120.0));
                let unwrapped = fonts.layout_job(preview_job(&text, palette, false, 120.0));
                assert!(wrapped.rows.len() > 1);
                assert!(wrapped.rect.width() <= 121.0);
                assert_eq!(unwrapped.rows.len(), 1);
                assert!(unwrapped.rect.width() > 120.0);
                assert_eq!(wrapped.job.text, text);
                assert_eq!(unwrapped.job.text, text);
            })
        });
    }
}

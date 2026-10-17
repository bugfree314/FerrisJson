use crate::palette::{ColorSettings, Palette, Preset};
use eframe::egui::{self, Color32, RichText};

/// egui maps COMMAND to Cmd on macOS and Ctrl on Windows/Linux. Consume the
/// event before text fields and Vim navigation can handle the same key press.
pub fn consume_open_file_shortcut(ctx: &egui::Context) -> bool {
    ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::O))
}

pub fn open_file_shortcut_label() -> &'static str {
    if cfg!(target_os = "macos") { "Open file (⌘O)" } else { "Open file (Ctrl+O)" }
}

#[derive(Clone)]
pub struct SearchHighlight {
    signature: (String, bool, bool, crate::document::SearchScope),
    matcher: regex::Regex,
}
impl SearchHighlight {
    pub fn new(term: &str, case: bool, regex: bool, scope: crate::document::SearchScope) -> Option<Self> {
        if term.is_empty() || term.len() > 64 * 1024 {
            return None;
        }
        let pattern = if regex { term.to_owned() } else { regex::escape(term) };
        let matcher = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!case)
            .size_limit(4 * 1024 * 1024)
            .dfa_size_limit(2 * 1024 * 1024)
            .build().ok()?;
        Some(Self { signature: (term.to_owned(), case, regex, scope), matcher })
    }
    pub fn same_query(&self, other: &Self) -> bool {
        self.signature == other.signature
    }
    fn includes(&self, key: bool) -> bool {
        use crate::document::SearchScope;
        match self.signature.3 {
            SearchScope::All => true,
            SearchScope::Keys => key,
            SearchScope::Values => !key,
        }
    }
    pub fn text_job(&self, text: &str, foreground: Color32, palette: Palette, key: bool) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        let format = egui::TextFormat {
            font_id: egui::FontId::monospace(13.0), color: foreground, ..Default::default()
        };
        self.append(&mut job, text, format, palette, key, false);
        job
    }
    fn append(&self, job: &mut egui::text::LayoutJob, text: &str, format: egui::TextFormat, palette: Palette, key: bool, quoted: bool) {
        if !self.includes(key) {
            job.append(text, 0.0, format);
            return;
        }
        // Search decoded JSON strings, then map matches back to their displayed escapes.
        let decoded = if quoted { serde_json::from_str::<String>(text).ok() } else { None };
        let search = decoded.as_deref().unwrap_or(text);
        let mut boundaries = Vec::new();
        if let Some(decoded) = &decoded {
            let mut raw = 1;
            for (offset, ch) in decoded.char_indices() {
                let start = raw;
                if text.as_bytes()[raw] == b'\\' {
                    raw += if text.as_bytes().get(raw + 1) == Some(&b'u') {
                        if ch.len_utf16() == 2 { 12 } else { 6 }
                    } else { 2 };
                } else {
                    raw += ch.len_utf8();
                }
                boundaries.push((offset, offset + ch.len_utf8(), start, raw));
            }
        }
        let mut end = 0;
        for found in self.matcher.find_iter(search).filter(|m| !m.is_empty()) {
            let (start, finish) = if decoded.is_some() {
                let start = boundaries.partition_point(|b| b.1 <= found.start());
                let finish = boundaries.partition_point(|b| b.0 < found.end());
                (boundaries[start].2, boundaries[finish - 1].3)
            } else { (found.start(), found.end()) };
            job.append(&text[end..start], 0.0, format.clone());
            let mut highlighted = format.clone();
            highlighted.background = crate::palette::color(palette.search_match);
            job.append(&text[start..finish], 0.0, highlighted);
            end = finish;
        }
        job.append(&text[end..], 0.0, format);
    }
}

pub fn kind_color(kind: &str, palette: Palette) -> Color32 {
    palette.kind(kind)
}
pub fn json_job(text: &str, palette: Palette) -> egui::text::LayoutJob {
    json_job_highlighted(text, palette, None)
}
pub fn json_job_highlighted(text: &str, palette: Palette, highlight: Option<&SearchHighlight>) -> egui::text::LayoutJob {
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
        let format = egui::TextFormat {
                font_id: egui::FontId::monospace(13.0),
                color: kind_color(kind, palette),
                ..Default::default()
            };
        let quoted = bytes[start] == b'"';
        let key = quoted && text[i..].trim_start().starts_with(':');
        if let Some(highlight) = highlight.filter(|_| quoted || matches!(kind, "number" | "boolean" | "null")) {
            highlight.append(&mut job, &text[start..i], format, palette, key, quoted);
        } else {
            job.append(&text[start..i], 0.0, format);
        }
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
pub fn json_preview_highlighted(ui: &mut egui::Ui, mut text: &str, palette: Palette, id: &str, wrap: bool, highlight: Option<&SearchHighlight>) {
    let area = if wrap {
        egui::ScrollArea::vertical()
    } else {
        egui::ScrollArea::both()
    };
    area.id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
        let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
            let mut job = if highlight.is_some() {
                json_job_highlighted(buffer.as_str(), palette, highlight)
            } else {
                preview_job(buffer.as_str(), palette, wrap, wrap_width)
            };
            job.wrap.max_width = if wrap { wrap_width.max(1.0) } else { f32::INFINITY };
            job.wrap.break_anywhere = wrap;
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
pub fn cheatsheet(ctx: &egui::Context, open: &mut bool, jsonpath_open: &mut bool, palette: Palette) {
    if !*open {
        return;
    }
    egui::Window::new("Keyboard cheatsheet").open(open).collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(720.0).max_width((ctx.content_rect().width() - 40.0).max(300.0))
        .max_height((ctx.content_rect().height() - 60.0).max(200.0)).show(ctx, |ui| {
            ui.label(RichText::new("Navigate JSON at the speed of thought").size(22.0).strong());
            ui.label("Vim keys work in the tree. Current row shows its absolute visible row number; other rows show distance from the cursor. Text fields keep normal typing. Esc closes this page.");
            ui.add_space(10.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                let groups: &[(&str, &[(&str, &str)])] = &[
                    ("NAVIGATION", &[("hjkl / arrows", "Parent / down / up / child"), ("12j · 3l", "Repeat a movement with a count"), ("gg · G · 25G", "First / last / visible row 25"), ("H · M · L", "Top / middle / bottom of viewport"), ("Ctrl+d / u", "Half page down / up"), ("PageDown / Up · Ctrl+e / b", "Full page down / up"), ("[ / ]", "Back / forward through visited positions")]),
                    ("SEARCH", &[("/ · ?", "Search forward / backward"), ("n · N · 3n", "Repeat search / reverse / count"), ("Enter · Esc", "Confirm search / return to tree"), ("F3 / Shift+F3", "Next / previous match"), ("Ctrl+F", "Focus search field")]),
                    ("TREE & COPY", &[("Space · za", "Toggle selected container"), ("zo · zc", "Open / close selected container"), ("zR · zM", "Expand / collapse whole document"), ("yy / Ctrl+C", "Copy full selected JSON"), ("yp / Ctrl+P", "Copy selected path")]),
                    ("DOCUMENT", &[("⌘O (macOS) / Ctrl+O (Windows)", "Open file"), ("Ctrl+V", "Load clipboard JSON"), ("F5", "Reload current file"), ("F1", "Toggle this cheatsheet"), ("Esc", "Cancel pending Vim keys / close dialog")]),
                ];
                for (title, rows) in groups {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label(RichText::new(*title).strong().color(Color32::from_rgb(80, 160, 220)));
                        egui::Grid::new(*title).num_columns(2).spacing([28.0, 7.0]).striped(true).show(ui, |ui| {
                            for (key, action) in *rows { ui.label(RichText::new(*key).monospace().strong()); ui.label(*action); ui.end_row(); }
                        });
                    }); ui.add_space(8.0);
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("JSONPath").strong());
                    if ui.small_button("Cheatsheet").on_hover_text("常用 JSONPath 语法与示例").clicked() {
                        *jsonpath_open = true;
                    }
                });
                ui.label("Press . outside text fields to focus JSONPath. The shortcut does not insert a dot.");
                ui.monospace("$.users[*].name   $..email   $.users[?(@.active == true)]");
                ui.label("Enter runs JSONPath in the default viewer; Run query / Enter in the paged tree. F4 / Shift+F4 jumps to the next / previous result and wraps around. The lower inspector keeps results; the upper always previews your selection. Drag the divider to resize.");
                ui.horizontal_wrapped(|ui| { for kind in ["string", "number", "boolean", "null", "object", "array"] {
                    ui.colored_label(kind_color(kind, palette), kind);
                }});
            });
        });
}
pub fn jsonpath_cheatsheet(ctx: &egui::Context, open: &mut bool) {
    if !*open { return; }
    egui::Window::new("JSONPath cheatsheet")
        .open(open).collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(700.0)
        .max_width((ctx.content_rect().width() - 40.0).max(240.0))
        .max_height((ctx.content_rect().height() - 60.0).max(180.0))
        .show(ctx, |ui| {
            ui.label("JSONPath 常用语法。点击 Copy 复制表达式，粘贴到 JSONPath 输入框后执行。Esc 关闭。示例假设根对象包含 users 数组。");
            egui::ScrollArea::vertical().id_salt("jsonpath_syntax_scroll").show(ui, |ui| {
                let groups: &[(&str, &[(&str, &str)])] = &[
                    ("基础与字段", &[
                        ("$", "根节点：选择整份 JSON。"),
                        ("$.users", "选择根对象的 users 字段；多层字段可写 $.data.users。"),
                        ("$['users']", "方括号字段访问，等价于 $.users。"),
                        ("$['user.name']", "访问键名本身含点号的字段；空格、中文或特殊字符键也可用引号。"),
                        ("$.*", "根对象的所有直接子节点。"),
                        ("$..name", "递归查找任意深度的 name 字段。"),
                    ]),
                    ("数组、切片与多选", &[
                        ("$.users[0]", "第一个用户；下标从 0 开始。"),
                        ("$.users[-1]", "最后一个用户；负数从数组末尾计数。"),
                        ("$.users[*]", "users 数组中的所有元素。"),
                        ("$.users[*].name", "选取每个用户的 name 字段。"),
                        ("$.users[0,2]", "按指定顺序选取下标 0、2；重复下标也会保留。"),
                        ("$.users[0:10]", "前 10 项；切片格式为 [起点:终点:步长]，不包含终点。"),
                        ("$.users[::2]", "从开头每隔一项选一项。"),
                        ("$.users[::-1]", "按相反顺序选择数组元素。"),
                        ("$.users[*]['name','age']", "选择每个用户的 name 和 age 节点；不会重组成对象或表格。"),
                    ]),
                    ("条件过滤：@ 表示当前候选元素", &[
                        ("$.users[?(@.age >= 18)]", "筛选 age 至少为 18 的用户。"),
                        ("$.users[?(@.name == '张三')]", "字符串相等；字符串必须用单引号或双引号包围。"),
                        ("$.users[?(@.active == true)]", "布尔值为真；true、false、null 不加引号。"),
                        ("$.users[?(@.email)]", "字段存在即可命中，即使值是 false 或 null。"),
                        ("$.users[?(!@.email)]", "字段不存在；! 表示逻辑非。"),
                        ("$.users[?(@.email == null)]", "字段值为 null；字段不存在与值为 null 有区别。"),
                        ("$.users[?(@.active == true && @.age >= 18)].name", "&& 表示同时满足；过滤后可继续选取 name。"),
                        ("$.users[?(@.age < 18 || @.age >= 60)]", "|| 表示任一满足；可加括号组合条件。"),
                        ("$.users[?(@.age >= $.minimumAge)]", "$ 在过滤中仍表示根节点；与根对象的 minimumAge 比较。"),
                        ("$.users[?(@.score > @.minimumScore)]", "比较同一元素中的两个字段。"),
                    ]),
                ];
                for (title, rows) in groups {
                    ui.add_space(8.0);
                    ui.heading(*title);
                    for (expression, explanation) in *rows {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(*expression).monospace());
                                if ui.small_button("Copy").clicked() { ctx.copy_text((*expression).into()); }
                            });
                            ui.label(*explanation);
                        });
                    }
                }
                ui.add_space(8.0);
                ui.heading("运算符与使用提示");
                ui.monospace("==  !=  <  <=  >  >=     &&  ||  !");
                ui.label("== 是比较，不能写成 =。字符串 '18' 与数字 18 不同；布尔 true 与字符串 'true' 也不同。JSONPath 区分字段名大小写。");
                ui.label("Enter 执行默认界面的查询；分页树可用 Run query / Enter。F4 下一项、Shift+F4 上一项，首尾循环并自动定位。扫描中只遍历已发布的结果。查询选择节点，不提供 SQL 式 GROUP BY、排序或聚合。");
                ui.heading("FerrisJson 当前支持范围");
                ui.label("文件后端（Indexed / Sparse）支持上述选择器和标量过滤；过滤中的 @ / $ 操作数只能使用字段、数组下标组成的单节点路径。");
                ui.label("文件后端暂不支持 length()、match() 等函数、数组或对象的深度比较，以及过滤操作数中的通配符 / 递归 / 多节点路径。内存模式的高级语法取决于原查询库，不保证所有模式通用。");
                ui.label("结果保留顺序和重复选择；递归和通配符可能产生大量结果。文件后端需完成源文件验证后执行查询。");
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
                    let live_search = settings.live_search;
                    *settings = ColorSettings { dark, wrap_values, wrap_results, low_memory_load, indexed_files, live_search, ..Default::default() }; changed = true;
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
